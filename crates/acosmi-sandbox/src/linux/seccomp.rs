//! Seccomp-BPF syscall filtering.
//!
//! Uses the `libseccomp` crate (Rust bindings over the C libseccomp library)
//! to install a BPF filter restricting which syscalls the process can make.
//!
//! # Strategy
//!
//! We use a **default-allow** filter with explicit deny rules for:
//! 1. **Network syscalls** — controlled by `NetworkPolicy` (None/Restricted/Host)
//! 2. **Dangerous syscalls** — always blocked (ptrace, mount, reboot, etc.)
//!
//! A strict whitelist would be more secure but breaks compatibility with
//! arbitrary user programs (Python, Node, shell scripts need many syscalls).
//! The Landlock layer handles filesystem isolation.
//!
//! # Network policy mapping
//!
//! | Policy | socket | connect | Unix domain |
//! |--------|--------|---------|-------------|
//! | None | Block all | Block all | Block all |
//! | Restricted | Allow INET/INET6 | Allow INET/INET6 | Block |
//! | Host | Allow all | Allow all | Allow all |
//!
//! # System requirement
//!
//! Requires `libseccomp-dev` >= 2.5.0 installed on the build system.

use libseccomp::{ScmpAction, ScmpArgCompare, ScmpCompareOp, ScmpFilterContext, ScmpSyscall};
use tracing::{debug, info};

use crate::config::{NetworkPolicy, SandboxConfig};
use crate::error::SandboxError;

/// Dangerous syscalls that are always blocked regardless of security level.
///
/// These can escape sandboxes, modify system state, or elevate privileges.
const DANGEROUS_SYSCALLS: &[&str] = &[
    "ptrace",           // process tracing — sandbox escape vector
    "process_vm_readv", // cross-process memory access
    "process_vm_writev",
    "personality",     // change execution domain — bypass ASLR
    "mount",           // mount filesystems
    "umount2",         // unmount filesystems
    "pivot_root",      // change root filesystem
    "swapon",          // enable swap
    "swapoff",         // disable swap
    "reboot",          // reboot the system
    "sethostname",     // change hostname
    "setdomainname",   // change domain name
    "kexec_load",      // load new kernel
    "kexec_file_load", // load new kernel (file variant)
    "init_module",     // load kernel module
    "finit_module",    // load kernel module (file variant)
    "delete_module",   // unload kernel module
    "acct",            // process accounting
    "settimeofday",    // set system clock
    "clock_settime",   // set clock
    "adjtimex",        // adjust system clock
    "bpf",             // BPF operations — can load arbitrary BPF programs
    "userfaultfd",     // exploit primitive — used in many kernel exploits
    "perf_event_open", // performance monitoring — info leak
    "lookup_dcookie",  // kernel tracing
    "add_key",         // kernel keyring manipulation
    "request_key",     // kernel keyring
    "keyctl",          // kernel keyring control
    "io_setup",        // AIO — rarely needed, potential exploit surface
    "io_destroy",
    "io_submit",
    "io_cancel",
    "io_getevents",
    "move_pages",    // NUMA memory migration — privilege escalation vector
    "mbind",         // NUMA memory policy
    "set_mempolicy", // NUMA memory policy
    "migrate_pages", // NUMA page migration
    "unshare",       // create namespaces — prevent nested sandbox escape
    "setns",         // join namespaces — prevent sandbox escape
    // S-01 audit fix: mount-related syscalls (kernel 5.2+ mount API)
    "open_tree",  // create file handle for mount operations
    "move_mount", // move mount points — container escape vector
    "fsopen",     // open filesystem configuration context
    "fspick",     // pick filesystem for reconfiguration
    "fsconfig",   // configure a filesystem context
    "fsmount",    // create a mount from filesystem context
    // S-01 audit fix: clone3 can create new namespaces
    "clone3", // newer clone() — can create namespaces, prevent nested escape
];

/// Network-related syscalls blocked in `NetworkPolicy::None` mode.
const NETWORK_SYSCALLS: &[&str] = &[
    "socket",
    "socketpair",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "sendmmsg",
    "recvmmsg",
    "shutdown",
    "getsockopt",
    "setsockopt",
    "getsockname",
    "getpeername",
];

/// 嵌套沙箱（容器 / 再套一层沙箱）所需的命名空间 syscall。
///
/// 默认在 [`DANGEROUS_SYSCALLS`] 里被拒——它们正是「从沙箱里再造一个沙箱然后
/// 逃出去」的原语。用户显式打开 `sandbox.enableWeakerNestedSandbox` 时按名放行
/// 这三个，**且仅这三个**：`mount` / `pivot_root` / `umount2` 不在其中，因为
/// 挂载操作能直接改写沙箱看到的文件系统视图，那是另一个量级的放宽。
const NESTED_SANDBOX_SYSCALLS: &[&str] = &["unshare", "setns", "clone3"];

/// 用户显式要求的放宽。默认全 false ⇒ 与放宽前逐字同构。
#[derive(Debug, Clone, Copy, Default)]
pub struct SeccompRelaxations {
    /// `weaker.nestedSandbox`：放行 [`NESTED_SANDBOX_SYSCALLS`]。
    pub nested_sandbox: bool,
    /// `network.allowAllUnixSockets`：`Restricted` 档下不再拦 AF_UNIX。
    pub all_unix_sockets: bool,
}

/// Apply a seccomp-BPF filter based on the sandbox configuration.
///
/// Must be called after Landlock (seccomp is more restrictive and harder to debug).
/// The filter is irrevocable and inherited by exec'd processes.
pub fn apply_seccomp_filter(config: &SandboxConfig) -> Result<(), SandboxError> {
    apply_seccomp_filter_with_relaxations(config, SeccompRelaxations::default())
}

/// [`apply_seccomp_filter`] + 用户显式要求的放宽（W-SANDBOX-ENFORCED-DEADCODE PR-2）。
///
/// 放宽项**必须显式传**：默认值在这里不是中性的，漏传一个 `false` 与漏传一个
/// `true` 的后果完全不对称（前者只是功能受限，后者是隔离被悄悄拆掉）。
pub fn apply_seccomp_filter_with_relaxations(
    config: &SandboxConfig,
    relax: SeccompRelaxations,
) -> Result<(), SandboxError> {
    let network_policy = config.effective_network_policy();

    // Default action: Allow — we deny specific dangerous operations
    let mut filter =
        ScmpFilterContext::new(ScmpAction::Allow).map_err(|e| seccomp_err("new_filter", e))?;

    // ── Always-blocked dangerous syscalls ──────────────────────────────────
    for name in DANGEROUS_SYSCALLS {
        if relax.nested_sandbox && NESTED_SANDBOX_SYSCALLS.contains(name) {
            debug!(
                syscall = name,
                "seccomp: allowed by enableWeakerNestedSandbox"
            );
            continue;
        }
        if let Ok(syscall) = ScmpSyscall::from_name(name) {
            filter
                .add_rule(ScmpAction::Errno(libc::EPERM), syscall)
                .map_err(|e| seccomp_err(&format!("block {name}"), e))?;
        }
        // If syscall name not recognized (e.g., arch-specific), skip silently
    }

    // ── Network policy ────────────────────────────────────────────────────
    match network_policy {
        NetworkPolicy::None => {
            // Block ALL network syscalls
            for name in NETWORK_SYSCALLS {
                if let Ok(syscall) = ScmpSyscall::from_name(name) {
                    filter
                        .add_rule(ScmpAction::Errno(libc::EPERM), syscall)
                        .map_err(|e| seccomp_err(&format!("block network {name}"), e))?;
                }
            }
            debug!("seccomp: all network syscalls blocked (NetworkPolicy::None)");
        }

        NetworkPolicy::Restricted => {
            // Block Unix domain sockets to prevent proxy bypass.
            // Allow AF_INET (2) and AF_INET6 (10) for TCP/UDP.
            //
            // socket(domain, type, protocol):
            //   Block if domain == AF_UNIX (1)
            //   Block if domain == AF_LOCAL (1, alias for AF_UNIX)
            //
            // `network.allowAllUnixSockets` 打开时整块跳过：用户显式声明沙箱内
            // 需要 Unix 域套接字（docker.sock / 本地服务）。**按路径的
            // `allowUnixSockets` 白名单在这一层表达不了** —— seccomp 的 BPF
            // 读不到 `connect()` 第二参指向的 `sockaddr_un`，拿不到路径。
            // 这条差异由 `platform::apply_exec_plan_to_self` 如实报出，方向是
            // 「比承诺更严」（全拦），不是安全缺口。
            if !relax.all_unix_sockets {
                if let Ok(socket_sc) = ScmpSyscall::from_name("socket") {
                    filter
                        .add_rule_conditional(
                            ScmpAction::Errno(libc::EPERM),
                            socket_sc,
                            &[ScmpArgCompare::new(
                                0,
                                ScmpCompareOp::Equal,
                                libc::AF_UNIX as u64,
                            )],
                        )
                        .map_err(|e| seccomp_err("block socket(AF_UNIX)", e))?;
                }

                // socketpair is always AF_UNIX — block entirely
                if let Ok(sc) = ScmpSyscall::from_name("socketpair") {
                    filter
                        .add_rule(ScmpAction::Errno(libc::EPERM), sc)
                        .map_err(|e| seccomp_err("block socketpair", e))?;
                }
            }

            // S-02 audit fix: Block AF_NETLINK (can manipulate routing/iptables)
            if let Ok(socket_sc) = ScmpSyscall::from_name("socket") {
                filter
                    .add_rule_conditional(
                        ScmpAction::Errno(libc::EPERM),
                        socket_sc,
                        &[ScmpArgCompare::new(
                            0,
                            ScmpCompareOp::Equal,
                            libc::AF_NETLINK as u64,
                        )],
                    )
                    .map_err(|e| seccomp_err("block socket(AF_NETLINK)", e))?;
            }

            // S-02 audit fix: Block AF_PACKET (raw packet access — can sniff traffic)
            if let Ok(socket_sc) = ScmpSyscall::from_name("socket") {
                filter
                    .add_rule_conditional(
                        ScmpAction::Errno(libc::EPERM),
                        socket_sc,
                        &[ScmpArgCompare::new(
                            0,
                            ScmpCompareOp::Equal,
                            libc::AF_PACKET as u64,
                        )],
                    )
                    .map_err(|e| seccomp_err("block socket(AF_PACKET)", e))?;
            }

            // S-02 audit fix: Block AF_VSOCK (VM socket — potential escape in VM environments)
            // AF_VSOCK = 40 (not always defined in libc crate)
            if let Ok(socket_sc) = ScmpSyscall::from_name("socket") {
                const AF_VSOCK: u64 = 40;
                filter
                    .add_rule_conditional(
                        ScmpAction::Errno(libc::EPERM),
                        socket_sc,
                        &[ScmpArgCompare::new(0, ScmpCompareOp::Equal, AF_VSOCK)],
                    )
                    .map_err(|e| seccomp_err("block socket(AF_VSOCK)", e))?;
            }

            // Block connect to Unix domain sockets.
            // connect(fd, addr, addrlen): addr->sa_family == AF_UNIX
            // Note: libseccomp cannot inspect pointed-to memory, so we rely on
            // blocking socket(AF_UNIX) above to prevent Unix socket creation.
            // If somehow a Unix socket fd exists, connect would still work.
            // Full mitigation requires Landlock ABI 6+ scope or network namespace.

            debug!(
                "seccomp: Unix/NETLINK/PACKET/VSOCK sockets blocked (NetworkPolicy::Restricted)"
            );
        }

        NetworkPolicy::Host => {
            // No network restrictions
            debug!("seccomp: no network restrictions (NetworkPolicy::Host)");
        }
    }

    // ── Load filter ───────────────────────────────────────────────────────
    filter.load().map_err(|e| seccomp_err("load_filter", e))?;

    info!(
        network_policy = ?network_policy,
        dangerous_blocked = DANGEROUS_SYSCALLS.len(),
        "seccomp filter loaded"
    );

    Ok(())
}

/// Convert a libseccomp error to `SandboxError::Seccomp`.
fn seccomp_err(
    operation: &str,
    err: impl std::error::Error + Send + Sync + 'static,
) -> SandboxError {
    SandboxError::Seccomp {
        operation: operation.into(),
        source: std::io::Error::other(err),
    }
}
