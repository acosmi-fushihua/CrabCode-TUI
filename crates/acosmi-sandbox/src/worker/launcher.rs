//! Worker process launcher.
//!
//! Spawns a persistent sandbox Worker process and returns a [`WorkerHandle`]
//! for sending commands over stdin/stdout IPC.
//!
//! The launcher applies platform-specific sandbox constraints (Seatbelt/Landlock/
//! Seccomp/Job Object) before exec, so the Worker runs already-sandboxed.
//! All subsequent fork+exec'd commands inherit the sandbox.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing::{debug, info};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::config::SandboxConfig;
use crate::config::{NetworkPolicy, SecurityLevel};
use crate::error::SandboxError;

use super::handle::WorkerHandle;

/// Configuration for launching a Worker process.
#[derive(Debug, Clone)]
pub struct WorkerLaunchConfig {
    /// Security level for the sandbox.
    pub security_level: SecurityLevel,

    /// Workspace directory (mounted read-write in the sandbox).
    pub workspace: PathBuf,

    /// Network policy override (defaults to security level's default).
    pub network_policy: Option<NetworkPolicy>,

    /// Additional bind mounts.
    pub mounts: Vec<crate::config::MountSpec>,

    /// Default timeout in seconds for commands executed by the Worker.
    pub default_timeout_secs: u64,

    /// Idle timeout in seconds. Worker exits if no request arrives within this duration.
    /// 0 = no idle timeout.
    pub idle_timeout_secs: u64,
}

impl Default for WorkerLaunchConfig {
    fn default() -> Self {
        Self {
            security_level: SecurityLevel::L1Allowlist,
            workspace: std::env::temp_dir(),
            network_policy: None,
            mounts: vec![],
            default_timeout_secs: 120,
            idle_timeout_secs: 0,
        }
    }
}

/// Launch a persistent sandbox Worker process.
///
/// The Worker process runs inside the platform's native sandbox with constraints
/// matching the specified security level. Commands sent via the returned
/// [`WorkerHandle`] are executed inside the sandbox.
///
/// # Platform behavior
///
/// - **macOS**: Applies Seatbelt (SBPL) profile via `pre_exec` before exec.
/// - **Linux**: Applies Landlock + Seccomp via `pre_exec` (Phase 2).
/// - **Windows**: Binds to Job Object + Restricted Token (Phase 2).
///
/// # Errors
///
/// Returns `SandboxError` if:
/// - The CLI binary cannot be found
/// - Sandbox setup fails (invalid config, platform not supported)
/// - Process spawn fails
pub fn launch_worker(config: &WorkerLaunchConfig) -> Result<WorkerHandle, SandboxError> {
    let cli_binary = find_cli_binary()?;

    info!(
        binary = %cli_binary.display(),
        workspace = %config.workspace.display(),
        security = ?config.security_level,
        "launching worker process"
    );

    let mut cmd = Command::new(&cli_binary);
    cmd.arg("sandbox")
        .arg("worker-start")
        .arg("--workspace")
        .arg(&config.workspace)
        .arg("--timeout")
        .arg(config.default_timeout_secs.to_string())
        .arg("--security-level")
        .arg(security_level_to_str(config.security_level))
        .arg("--idle-timeout")
        .arg(config.idle_timeout_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()); // Worker logs flow to parent's stderr

    // Platform-specific sandbox application.
    //
    // ROOT-CAUSE FIX: 旧实现在 Windows / Linux 上总是设 OA_SANDBOX_APPLIED=1，
    // 但对应的 apply_windows_sandbox / apply_linux_sandbox 是空函数
    // （Phase 2 未实现）。Worker 读到 env 后跳过自沙箱 → 零隔离执行。
    //
    // 现在两步原子化：
    //   1. 先调 apply_<os>_sandbox，失败直接返回（Windows/Linux 当前硬失败）
    //   2. 成功后才设 OA_SANDBOX_APPLIED=1
    // launcher-path 在 Windows / Linux 直接不可用 —— 调用方应改用
    // Go native_bridge 的直 spawn 路径（Worker 自沙箱到 Job Object 级别），
    // 或等待 P3 的 WindowsRunner::run 整合实装。
    #[cfg(target_os = "macos")]
    {
        apply_macos_sandbox(&mut cmd, config)?;
        cmd.env("OA_SANDBOX_APPLIED", "1");
    }

    #[cfg(target_os = "linux")]
    {
        apply_linux_sandbox(&mut cmd, config)?;
        cmd.env("OA_SANDBOX_APPLIED", "1");
    }

    #[cfg(target_os = "windows")]
    {
        apply_windows_sandbox(&mut cmd, config)?;
        cmd.env("OA_SANDBOX_APPLIED", "1");
    }

    let child = cmd.spawn().map_err(|e| SandboxError::Io {
        context: format!("spawning worker process '{}'", cli_binary.display()),
        source: e,
    })?;

    let pid = child.id();
    debug!(pid, "worker process spawned");

    WorkerHandle::new(child).map_err(|e| SandboxError::Io {
        context: "creating worker handle from spawned process".into(),
        source: e,
    })
}

/// Launch a Worker process **without** sandbox constraints.
///
/// Useful for testing the Worker event loop and IPC without platform-specific
/// sandbox overhead. Should not be used in production.
pub fn launch_worker_unsandboxed(
    config: &WorkerLaunchConfig,
) -> Result<WorkerHandle, SandboxError> {
    let cli_binary = find_cli_binary()?;

    info!(
        binary = %cli_binary.display(),
        workspace = %config.workspace.display(),
        "launching unsandboxed worker process (testing only)"
    );

    let mut cmd = Command::new(&cli_binary);
    cmd.arg("sandbox")
        .arg("worker-start")
        .arg("--workspace")
        .arg(&config.workspace)
        .arg("--timeout")
        .arg(config.default_timeout_secs.to_string())
        .arg("--security-level")
        .arg(security_level_to_str(config.security_level))
        .arg("--idle-timeout")
        .arg(config.idle_timeout_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let child = cmd.spawn().map_err(|e| SandboxError::Io {
        context: format!(
            "spawning unsandboxed worker process '{}'",
            cli_binary.display()
        ),
        source: e,
    })?;

    let pid = child.id();
    debug!(pid, "unsandboxed worker process spawned");

    WorkerHandle::new(child).map_err(|e| SandboxError::Io {
        context: "creating worker handle from spawned process".into(),
        source: e,
    })
}

/// Convert a [`SecurityLevel`] to its CLI argument string.
const fn security_level_to_str(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::L0Deny => "deny",
        SecurityLevel::L1Allowlist => "allowlist",
        SecurityLevel::L2Sandboxed => "sandboxed",
    }
}

// ── Platform-specific sandbox setup ─────────────────────────────────────

/// Apply macOS Seatbelt sandbox to the Worker command (pre_exec).
#[cfg(target_os = "macos")]
fn apply_macos_sandbox(cmd: &mut Command, config: &WorkerLaunchConfig) -> Result<(), SandboxError> {
    use crate::macos::seatbelt;
    use std::os::unix::process::CommandExt;

    // Build a SandboxConfig for profile generation
    let sandbox_config = SandboxConfig {
        security_level: config.security_level,
        command: String::new(), // Not used for profile generation
        args: vec![],
        workspace: config.workspace.clone(),
        mounts: config.mounts.clone(),
        resource_limits: crate::config::ResourceLimits::default(),
        network_policy: config.network_policy,
        env_vars: std::collections::HashMap::new(),
        format: crate::config::OutputFormat::Json,
        backend: crate::config::BackendPreference::Native,
    };

    let profile = seatbelt::generate_profile(&sandbox_config)?;
    let params_refs: Vec<(&str, &str)> = profile
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let sandbox_args = crate::macos::ffi::SandboxArgs::new(&profile.sbpl, &params_refs)?;

    debug!(
        profile_len = profile.sbpl.len(),
        param_count = profile.params.len(),
        "applying seatbelt sandbox to worker"
    );

    // SAFETY:
    // - `sandbox_args` is pre-built with all allocations complete before fork.
    // - `SandboxArgs::apply()` calls sandbox_init_with_parameters, safe post-fork.
    // - The sandbox is irreversible and inherited by the exec'd worker process.
    // - Verified by Skill 5: fork+exec'd children inherit Seatbelt constraints.
    unsafe {
        cmd.pre_exec(move || sandbox_args.apply());
    }

    Ok(())
}

/// Apply Linux Landlock + Seccomp sandbox to the Worker command (pre-fork hook).
///
/// ROOT-CAUSE FIX: 旧实现是空函数只打 warn，调用方仍设 OA_SANDBOX_APPLIED=1，
/// 导致 Worker 零隔离运行。现在真正实装 `pre_exec` 钩子：fork 后、exec 前，
/// child 进程依序执行：
///   1. `prctl(PR_SET_NO_NEW_PRIVS, 1)` — Seccomp 加载前置
///   2. Landlock 文件系统约束
///   3. Seccomp syscall 过滤
/// 与 `apply_to_self_linux` 对齐（两条路径有相同内核级约束）。namespace 和
/// cgroup 不在 launcher 路径（它们需要 pre-fork 的父进程设施，由
/// LinuxRunner::run 的 spawn-child 模式承担）。
///
/// # CONTRACT: Single-threaded caller required
///
/// **调用方必须在单线程上下文调用本函数**。pre_exec 闭包会在 fork 后的
/// child 进程里执行 `apply_landlock_rules` 和 `apply_seccomp_filter`，两者
/// 内部都会做堆分配（Vec/String 构造、libseccomp C 库的 seccomp_init
/// 内部 malloc）。按 POSIX 规范，fork() 只复制调用线程，parent 其他线程
/// 持有的 allocator mutex 会在 child 里变成死锁的 locked 状态——任何
/// malloc 都会挂起。
///
/// 典型安全场景：
///   - Rust CLI `acosmi sandbox run` 单线程启动
///   - acosmi-cmd-mcp / acosmi-cmd-sandbox 的测试 binary（单线程）
///
/// 典型**不安全**场景（不要在这类 caller 里用本函数）：
///   - Tokio runtime 里的 spawn_blocking
///   - Go runtime 通过 CGO 调用本 crate（Go 本身多线程）
///
/// Go 侧当前走的是 `Command::new(crabcode-sandbox) ... spawn()` 直接启动
/// 新 binary 再让它调 `apply_sandbox_to_self`（见 platform.rs），不经由
/// 本 launcher 函数，因此不受此 contract 约束。
///
/// 复审 H-1（2026-04 Rust 审计）明确记录：在多线程 parent 里使用本函数
/// 会遭 malloc 锁死锁。若必要，需重构为 "parent 预构建 ruleset FD +
/// BPF bytecode，pre_exec 只做 syscall"，工作量约 200 行 unsafe syscall。
#[cfg(target_os = "linux")]
fn apply_linux_sandbox(cmd: &mut Command, config: &WorkerLaunchConfig) -> Result<(), SandboxError> {
    use std::os::unix::process::CommandExt;

    // 在 pre-fork 的父进程上下文里构建 SandboxConfig（避免 fork 后 child
    // 里分配堆内存 —— pre_exec 的经典限制）。
    let sandbox_config = SandboxConfig {
        security_level: config.security_level,
        command: String::new(),
        args: vec![],
        workspace: config.workspace.clone(),
        mounts: config.mounts.clone(),
        resource_limits: crate::config::ResourceLimits::default(),
        network_policy: config.network_policy,
        env_vars: std::collections::HashMap::new(),
        format: crate::config::OutputFormat::Json,
        backend: crate::config::BackendPreference::Native,
    };

    debug!(
        workspace = %sandbox_config.workspace.display(),
        security = ?sandbox_config.security_level,
        "applying linux launcher sandbox (pre-fork hook)"
    );

    // SAFETY:
    // - pre_exec 闭包在 fork 后 child 里同步运行，由 std::process 暴露的
    //   POSIX fork-safe 钩子。
    // - sandbox_config 被 move 进闭包，拥有所有堆内存；闭包不分配新内存。
    // - prctl/landlock/seccomp 这三步都是 syscall 级操作，fork-safe。
    // - 任意一步返回 Err，钩子把它转成 spawn() 的 io::Error，parent 拿到
    //   失败，child 不会继续进入 exec'd 态。
    // - 约束不可逆且被 exec'd Worker 继承（由 kernel 保证）。
    unsafe {
        cmd.pre_exec(move || -> std::io::Result<()> {
            // 1. NoNewPrivs
            let rc = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_i32, 0_i32, 0_i32, 0_i32);
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }

            // 2. Landlock
            if let Err(e) = crate::linux::landlock::apply_landlock_rules(&sandbox_config) {
                return Err(std::io::Error::other(format!("landlock: {e}")));
            }

            // 3. Seccomp
            if let Err(e) = crate::linux::seccomp::apply_seccomp_filter(&sandbox_config) {
                return Err(std::io::Error::other(format!("seccomp: {e}")));
            }

            Ok(())
        });
    }

    Ok(())
}

/// Apply Windows Job Object + Restricted Token sandbox to the Worker command.
///
/// ROOT-CAUSE FIX: 同 `apply_linux_sandbox，空函数会让` Worker 零隔离运行。
/// Windows Runner 的完整实现在 `windows::WindowsRunner，但它是` spawn-child 模式，
/// 不是 pre_exec-style（Windows API 不支持修改已启动进程的 token）。
/// 本 launcher-path 在 Phase 3.1 整合 `WindowsRunner::run` 之前保持硬失败。
#[cfg(target_os = "windows")]
fn apply_windows_sandbox(
    _cmd: &mut Command,
    _config: &WorkerLaunchConfig,
) -> Result<(), SandboxError> {
    Err(SandboxError::PlatformNotSupported {
        platform: "windows".into(),
        reason: "launcher-path sandbox not implemented for Windows (Windows token cannot be modified post-spawn). Use Go native_bridge direct-spawn path or WindowsRunner::run spawn-child mode.".into(),
    })
}

// ── Binary discovery ────────────────────────────────────────────────────

/// CLI 可执行文件的已知名称（不含扩展名）。
/// `current_exe()` 只有在文件名匹配这些名称时才被采用，
/// 防止在 cargo test 等场景下误把测试二进制当作 CLI。
const KNOWN_CLI_NAMES: &[&str] = &["crabcode", "acosmi", "oa"];

/// Find the CLI binary path for launching the Worker.
///
/// Discovery order:
/// 1. `CRABCODE_CLI_BINARY` 环境变量（优先）/ `OA_CLI_BINARY`（向后兼容）
/// 2. `current_exe()`，仅当文件名为已知 CLI 名时采用
/// 3. `which crabcode` / `which oa` fallback
fn find_cli_binary() -> Result<PathBuf, SandboxError> {
    // 1. 环境变量显式指定（优先新名，兼容旧名）
    for env_key in ["CRABCODE_CLI_BINARY", "OA_CLI_BINARY"] {
        if let Ok(path) = std::env::var(env_key) {
            let p = PathBuf::from(&path);
            if p.exists() {
                return Ok(p);
            }
            return Err(SandboxError::InvalidConfig {
                message: format!("{env_key}={path} does not exist"),
            });
        }
    }

    // 2. current_exe()：仅当文件名为已知 CLI 名时采用。
    //    在 cargo test 场景下 current_exe() 返回测试二进制（如 macos_worker_integration-xxx），
    //    不匹配已知名称，正确地跳过此分支。
    if let Ok(exe) = std::env::current_exe() {
        let exe_str = exe.to_string_lossy();
        if exe.exists() && !exe_str.contains("(deleted)") {
            let file_stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if KNOWN_CLI_NAMES.contains(&file_stem) {
                return Ok(exe);
            }
        }
    }

    // 3. which fallback（优先新名，兼容旧名）
    for bin_name in KNOWN_CLI_NAMES {
        if let Ok(path) = which::which(bin_name) {
            return Ok(path);
        }
    }

    Err(SandboxError::InvalidConfig {
        message: "cannot find CLI binary for Worker launcher: \
                  set CRABCODE_CLI_BINARY or ensure 'crabcode' is in PATH"
            .into(),
    })
}

/// Check if a path is a valid executable.
#[allow(dead_code)]
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}
