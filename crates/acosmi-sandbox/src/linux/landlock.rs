//! Landlock LSM filesystem isolation.
//!
//! Uses the `landlock` crate (official Rust bindings by Mickaël Salaün) to
//! restrict filesystem access for the sandboxed process. Landlock is unprivileged
//! and available since kernel 5.13 (ABI V1).
//!
//! # ABI compatibility
//!
//! Filesystem isolation requires ABI V3 (kernel 6.2, adds `TRUNCATE`) and TCP
//! network filtering requires ABI V4. Compatibility is a hard requirement:
//! silently dropping a requested access right would make an incomplete ruleset
//! look fully enforced to callers.
//!
//! # How it works
//!
//! 1. Declare handled access rights (anything not handled is unrestricted)
//! 2. Add allow-rules for specific paths/ports
//! 3. Call `restrict_self()` — irrevocable, inherited by exec'd processes

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use tracing::{debug, info};

use crate::config::{MountMode, SandboxConfig, SecurityLevel};
use crate::error::SandboxError;

/// Minimum ABI that contains every filesystem right used by this implementation.
/// V2 adds `REFER`; V3 adds `TRUNCATE`. Missing either must never be reported as
/// full filesystem isolation.
pub(crate) const REQUIRED_FS_ABI: ABI = ABI::V3;

/// Maximum ABI version we generate rules for (V4 adds TCP network filtering).
const TARGET_ABI: ABI = ABI::V4;

/// System paths allowed as read-only inside the sandbox.
const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr",   // libraries, binaries, data
    "/bin",   // essential binaries (may → /usr/bin symlink)
    "/lib",   // essential shared libraries (may → /usr/lib symlink)
    "/lib64", // 64-bit shared libraries
    "/sbin",  // system binaries
    "/etc",   // configuration files
    "/run",   // runtime data (systemd, dbus sockets)
];

/// S-04 audit fix: Scoped /proc paths (read-only) instead of full /proc.
/// Full /proc exposes other processes' info — limit to own process + essential files.
const PROC_READ_PATHS: &[&str] = &[
    "/proc/self",        // own process info — needed by most programs
    "/proc/thread-self", // own thread info
];

/// S-04 audit fix: Individual /proc files needed by many programs.
const PROC_READ_FILES: &[&str] = &[
    "/proc/meminfo",
    "/proc/cpuinfo",
    "/proc/stat",
    "/proc/filesystems",
    "/proc/version",
    "/proc/loadavg",
    "/proc/uptime",
];

/// S-04 audit fix: Scoped /sys paths (read-only) instead of full /sys.
/// Full /sys exposes all kernel/hardware info — limit to essential paths.
const SYS_READ_PATHS: &[&str] = &[
    "/sys/devices/system/cpu", // CPU topology — needed by runtime detection
];

/// S-04 audit fix: Specific device files instead of full /dev.
/// Full /dev access exposes all devices; sandbox only needs common ones.
const DEV_PATHS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/urandom",
    "/dev/random",
    "/dev/fd", // file descriptor directory
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/shm", // shared memory — needed by some runtimes
];

/// Temp directories allowed as read-write.
const TEMP_PATHS: &[&str] = &["/tmp", "/var/tmp"];

/// Apply Landlock filesystem isolation rules to the current process.
///
/// After this call, the process (and any exec'd children) can only access
/// paths explicitly allowed by the rules. This is irrevocable.
///
/// # Rule mapping
///
/// | Config | Landlock rule |
/// |--------|---------------|
/// | Workspace (L0) | Read-only |
/// | Workspace (L1/L2) | Read-write |
/// | System paths | Read-only |
/// | Temp dirs | Read-write |
/// | Additional mounts | Per `MountMode` |
pub fn apply_landlock_rules(config: &SandboxConfig) -> Result<(), SandboxError> {
    apply_landlock_rules_with_grants(config, &[], None).map(|_| ())
}

/// 一条来自 [`SandboxExecPlan`](crate::exec_config::SandboxExecPlan) 四表的额外授权。
#[derive(Debug, Clone)]
pub struct ExtraGrant {
    /// 授权目标子树的根（已由 `exec_rules::normalize_pattern` 归一化）。
    pub path: PathBuf,
    /// `true` = 读写（allowWrite），`false` = 只读（allowRead）。
    pub write: bool,
    /// 原始模式串，只用于诊断点名。
    pub source: String,
}

/// Landlock 施加结果。
pub struct LandlockOutcome {
    /// 本次实际授权的全部子树根。deny 的重叠判定以它为基准
    /// （见 `exec_rules::shadowing_grant`）——判据必须是**真正授出去的东西**，
    /// 不是配置里写了什么。
    pub granted_roots: Vec<PathBuf>,
    /// 施加过程中攒下的、必须让用户看见的事实。
    pub warnings: Vec<String>,
}

/// [`apply_landlock_rules`] + 来自 fs 四表的额外授权。
///
/// # UNENFORCEABLE(linux)：deny 表在已放行的子树里挖不了洞
///
/// Landlock 是**纯 allow-grant 模型**：ruleset 声明它管辖哪些访问权
/// （`handle_access`），然后逐条 `add_rule` 授权路径子树；没被授权的一律拒绝。
/// 它**没有** deny 规则这种东西，所以「放行 `<cwd>`，但其中的
/// `<cwd>/.crabcode/settings.json` 不许写」这类要求无法表达。
///
/// 试过并否掉的替代：给嵌套路径再加一条「权限更小」的规则来挖洞 —— 不成立。
/// 一条 `deny` 需要授出**空**权限集，而空权限集会被内核直接拒绝
/// （`landlock` crate 0.4.4 `src/fs.rs`：「An empty access-right would be an
/// error if passed to the kernel」，`errors.rs::AccessError::Empty`
/// 「would be rejected by the kernel」）。挂载命名空间覆盖是另一条路，但它要求
/// 特权或 unprivileged userns，且 self-apply 路径当前根本不进
/// `namespace.rs`（见 `platform::apply_to_self_linux`）——那是一次独立立项。
///
/// 因此本函数的契约是：**与 allow 不重叠的 deny 由默认拒绝天然兑现；重叠的
/// deny 兑现不了，必须原样报出去**（`platform::apply_exec_plan_to_self` 负责报）。
/// 绝不静默丢弃 —— 丢一条 deny 就是把用户的防线悄悄拆掉。
///
/// # `connect_tcp_port`：PR-8 的出口锁
///
/// `Some(port)` 时 ruleset 额外管辖 [`AccessNet::ConnectTcp`]，并且**只**授权
/// 这一个端口。于是 `connect()` 到其它任何 TCP 端口都被内核拒绝——过滤代理
/// 从「建议」变成「唯一出口」。`None` 时完全不声明网络管辖，行为与本参数出现
/// 之前逐字同构。
///
/// ## 它比 Seatbelt 弱在哪（必须知道，不许假装等价）
///
/// Landlock 的网络规则是**端口模型**：`NetPort` 只有端口，没有地址。所以
/// `Some(8080)` 的真实语义是「可以 connect 到**任意主机**的 8080 端口」，
/// 而不是 macOS 那条 `localhost:8080`。代价是：若外部某台主机恰好在代理端口
/// 上监听，它是可达的。收益是常见形态（`curl` → `:443`／`:80`）被真的挡住，
/// 只能走代理。内核当前没有提供地址级的 Landlock 规则，这是能力上界不是实现
/// 偷懒。
///
/// ## 老内核怎么办
///
/// 网络管辖是 ABI V4（内核 6.7）才有的。在更老的内核上，hard-requirement
/// compatibility 会在 ruleset 创建/施加阶段直接报错，整条命令以 125 失败；
/// 绝不静默丢掉网络或文件系统 access right。
///
/// ## UDP / DNS 不在管辖内
///
/// Landlock 网络只管 TCP 的 bind/connect，UDP 不受影响，所以 DNS 依旧走
/// seccomp 那一层的既有放行。这不是缺口：域名解析出来的 IP 一样连不上，
/// 而子进程连代理用的是 IP，本来就不需要 DNS。
pub fn apply_landlock_rules_with_grants(
    config: &SandboxConfig,
    extra: &[ExtraGrant],
    connect_tcp_port: Option<u16>,
) -> Result<LandlockOutcome, SandboxError> {
    let abi = TARGET_ABI;
    let mut granted_roots: Vec<PathBuf> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Create ruleset — handles all filesystem access rights for this ABI.
    // Anything not explicitly allowed will be denied.
    let mut builder = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| landlock_err("handle_access(fs)", e))?;

    // 网络管辖必须在 `create()` **之前**声明，且与 fs 共用同一个 ruleset ——
    // `restrict_self()` 一个进程只调一次，分成两个 ruleset 会让后一个覆盖前一个。
    if connect_tcp_port.is_some() {
        builder = builder
            .handle_access(AccessNet::ConnectTcp)
            .map_err(|e| landlock_err("handle_access(net connect-tcp)", e))?;
    }

    let mut ruleset = builder
        .create()
        .map_err(|e| landlock_err("create_ruleset", e))?;

    // ── Workspace access ──────────────────────────────────────────────────
    let workspace_access = match config.security_level {
        SecurityLevel::L0Deny => AccessFs::from_read(abi),
        SecurityLevel::L1Allowlist | SecurityLevel::L2Sandboxed => AccessFs::from_all(abi),
    };
    add_path_rule(
        &mut ruleset,
        &config.workspace,
        workspace_access,
        "workspace",
    )?;
    granted_roots.push(config.workspace.clone());

    // ── System paths (read-only) ──────────────────────────────────────────
    for path in SYSTEM_READ_PATHS {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_read(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }

    // S-04 audit fix: Scoped /proc access (not full /proc)
    for path in PROC_READ_PATHS {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_read(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }
    for path in PROC_READ_FILES {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_read(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }

    // S-04 audit fix: Scoped /sys access (not full /sys)
    for path in SYS_READ_PATHS {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_read(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }

    // S-04 audit fix: Scoped /dev access (specific device files only)
    for path in DEV_PATHS {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_all(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }

    // ── Temp directories (read-write) ─────────────────────────────────────
    for path in TEMP_PATHS {
        if Path::new(path).exists() {
            add_path_rule(&mut ruleset, path, AccessFs::from_all(abi), path)?;
            granted_roots.push(PathBuf::from(path));
        }
    }
    // Per-user TMPDIR (e.g., /run/user/<uid>)
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        if Path::new(&tmpdir).exists() {
            add_path_rule(&mut ruleset, &tmpdir, AccessFs::from_all(abi), "TMPDIR")?;
            granted_roots.push(PathBuf::from(tmpdir));
        }
    }

    // ── Additional mounts ─────────────────────────────────────────────────
    for mount in &config.mounts {
        let access = match mount.mode {
            MountMode::ReadOnly => AccessFs::from_read(abi),
            MountMode::ReadWrite => AccessFs::from_all(abi),
        };
        let label = mount.host_path.display().to_string();
        add_path_rule(&mut ruleset, &mount.host_path, access, &label)?;
        granted_roots.push(mount.host_path.clone());
    }

    // ── fs 四表的 allow 侧（W-SANDBOX-ENFORCED-DEADCODE PR-2）─────────────
    //
    // 事故前这里什么都没有：`handle_spawn_managed_sandboxed` 写死
    // `mounts: vec![]`，TS 派生的 allowRead/allowWrite 一条都到不了内核。
    for grant in extra {
        let access = if grant.write {
            AccessFs::from_all(abi)
        } else {
            AccessFs::from_read(abi)
        };
        if !grant.path.exists() {
            // `PathFd::new` 需要一个真实存在的路径。为一条指向不存在目录的
            // allow 规则把整条命令毙掉是本末倒置（典型：配置里写了一个本机
            // 没有的可选目录），但**必须留下名字**：命令若稍后自己创建它，
            // 那次访问会被拒，而用户有权知道原因出在这里。
            warnings.push(format!(
                "landlock: skipped {} `{}` — the path does not exist right now, \
                 so it cannot be granted (it will be denied if created later)",
                if grant.write {
                    "filesystem.allowWrite"
                } else {
                    "filesystem.allowRead"
                },
                grant.source
            ));
            continue;
        }
        let label = grant.path.display().to_string();
        add_path_rule(&mut ruleset, &grant.path, access, &label)?;
        granted_roots.push(grant.path.clone());
    }

    // ── 出口锁：唯一放行的 TCP 端口（W-SANDBOX-ENFORCED-DEADCODE PR-8）────
    //
    // 恰好一条规则。ruleset 已经声明管辖 `ConnectTcp`（上面），所以除了这条
    // 规则放行的端口之外，`connect()` 到任何 TCP 端口都会被内核拒绝：外部
    // 主机的 443、loopback 上代理之外的端口，一视同仁。
    if let Some(port) = connect_tcp_port {
        add_net_port_rule(&mut ruleset, port)?;
    }

    // ── Restrict self ─────────────────────────────────────────────────────
    let status = ruleset
        .restrict_self()
        .map_err(|e| landlock_err("restrict_self", e))?;

    require_fully_enforced(status.ruleset)?;
    info!("landlock fully enforced");

    Ok(LandlockOutcome {
        granted_roots,
        warnings,
    })
}

/// A sandbox run is allowed to proceed only when every requested Landlock
/// access right reached the kernel. `PartiallyEnforced` is not a successful
/// degradation here: the caller cannot reconstruct which filesystem operation
/// was silently left outside the policy.
fn require_fully_enforced(status: RulesetStatus) -> Result<(), SandboxError> {
    match status {
        RulesetStatus::FullyEnforced => Ok(()),
        RulesetStatus::PartiallyEnforced => Err(SandboxError::Landlock {
            operation: "restrict_self".into(),
            source: std::io::Error::other(
                "landlock ruleset only partially enforced; refusing incomplete isolation",
            ),
        }),
        RulesetStatus::NotEnforced => Err(SandboxError::Landlock {
            operation: "restrict_self".into(),
            source: std::io::Error::other("landlock ruleset not enforced by kernel"),
        }),
    }
}

/// Add a path-based allow rule to the Landlock ruleset.
fn add_path_rule<A: Into<landlock::BitFlags<AccessFs>>>(
    ruleset: &mut landlock::RulesetCreated,
    path: impl AsRef<Path>,
    access: A,
    label: &str,
) -> Result<(), SandboxError> {
    let path_ref = path.as_ref();
    let fd = PathFd::new(path_ref).map_err(|e| SandboxError::Landlock {
        operation: format!("open PathFd for {label}"),
        source: std::io::Error::other(e),
    })?;
    ruleset
        .add_rule(PathBeneath::new(fd, access))
        .map_err(|e| landlock_err(&format!("add_rule for {label}"), e))?;
    debug!(path = %path_ref.display(), "landlock: added path rule");
    Ok(())
}

/// Add the single `connect(2)`-TCP allow rule to the Landlock ruleset.
///
/// Shaped like [`add_path_rule`] and for the same reason: `add_rule` takes
/// `self` by value on an owned `RulesetCreated`, so calling it directly on the
/// local would move the ruleset out from under `restrict_self()`. The crate
/// also implements the trait for `&mut RulesetCreated`, which is what this
/// borrow selects.
fn add_net_port_rule(
    ruleset: &mut landlock::RulesetCreated,
    port: u16,
) -> Result<(), SandboxError> {
    ruleset
        .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
        .map_err(|e| landlock_err(&format!("add_rule for connect-tcp port {port}"), e))?;
    debug!(port, "landlock: added connect-tcp port rule (egress lock)");
    Ok(())
}

/// Convert a landlock error to our `SandboxError::Landlock` variant.
fn landlock_err(
    operation: &str,
    err: impl std::error::Error + Send + Sync + 'static,
) -> SandboxError {
    SandboxError::Landlock {
        operation: operation.into(),
        source: std::io::Error::other(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_landlock_statuses_fail_closed() {
        assert!(require_fully_enforced(RulesetStatus::FullyEnforced).is_ok());

        let partial = require_fully_enforced(RulesetStatus::PartiallyEnforced)
            .expect_err("partial enforcement must fail closed");
        assert!(partial.to_string().contains("partially enforced"));

        let absent = require_fully_enforced(RulesetStatus::NotEnforced)
            .expect_err("missing enforcement must fail closed");
        assert!(absent.to_string().contains("not enforced"));
    }
}
