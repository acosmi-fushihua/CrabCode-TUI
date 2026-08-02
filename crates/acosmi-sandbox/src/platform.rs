//! Platform capability detection and sandbox backend selection.
//!
//! Detects available isolation mechanisms at runtime and selects the strongest
//! backend following the degradation chain:
//!
//! - **Linux**:   `Namespace+Seccomp+Landlock` → `Landlock+Seccomp` → Docker fallback
//! - **macOS**:   `Seatbelt FFI` → Docker fallback
//! - **Windows**: `RestrictedToken+JobObject` → `JobObject only` → Docker fallback

use tracing::{debug, info, warn};

use crate::SandboxRunner;
use crate::config::{BackendPreference, SandboxConfig};
use crate::error::SandboxError;

/// Detected sandbox backend variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    // ── Linux ──────────────────────────────────────────────────────
    /// Full Linux isolation: User/PID/Mount Namespaces + Seccomp-BPF + Landlock LSM.
    LinuxFull,
    /// Unprivileged-only: Landlock LSM + Seccomp-BPF (no namespace required).
    LinuxLandlockSeccomp,

    // ── macOS ──────────────────────────────────────────────────────
    /// macOS Seatbelt via `sandbox_init_with_parameters` FFI.
    MacosSeatbelt,

    // ── Windows ────────────────────────────────────────────────────
    /// Full Windows isolation: Restricted Token + Job Object.
    WindowsFull,
    /// Job Object only (resource limits + process tree reaping, no token restriction).
    WindowsJobOnly,

    // ── Fallback ───────────────────────────────────────────────────
    /// Docker CLI fallback when native backends are unavailable.
    DockerFallback,
}

impl SandboxBackend {
    /// Human-readable name for this backend (used in `SandboxOutput.sandbox_backend`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LinuxFull => "linux-namespace+seccomp+landlock",
            Self::LinuxLandlockSeccomp => "linux-landlock+seccomp",
            Self::MacosSeatbelt => "macos-seatbelt",
            Self::WindowsFull => "windows-restricted-token+job",
            Self::WindowsJobOnly => "windows-job-object",
            Self::DockerFallback => "docker-fallback",
        }
    }

    /// Whether this backend is a native OS sandbox (not Docker).
    #[must_use]
    pub const fn is_native(self) -> bool {
        !matches!(self, Self::DockerFallback)
    }
}

// ── Linux capability detection ─────────────────────────────────────────────

/// Linux-specific sandbox capabilities.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct LinuxCapabilities {
    /// Whether unprivileged user namespaces are available.
    /// Ubuntu 24.04+ may block these via AppArmor.
    pub has_user_namespace: bool,

    /// Landlock ABI version (0 = not available, 1-6 = supported version).
    /// ABI 4+ (kernel 6.7) required for TCP network filtering.
    pub landlock_abi_version: u8,

    /// Whether seccomp-BPF is available (kernel 3.5+).
    pub has_seccomp: bool,

    /// Whether cgroups v2 delegation is available (via systemd --user).
    pub has_cgroup_v2_delegation: bool,
}

#[cfg(target_os = "linux")]
impl LinuxCapabilities {
    /// Detect Linux sandbox capabilities from the current system.
    pub fn detect() -> Self {
        let has_user_namespace = detect_user_namespace();
        let landlock_abi_version = detect_landlock_abi();
        let has_seccomp = detect_seccomp();
        let has_cgroup_v2_delegation = detect_cgroup_v2_delegation();

        let caps = Self {
            has_user_namespace,
            landlock_abi_version,
            has_seccomp,
            has_cgroup_v2_delegation,
        };

        info!(
            user_ns = caps.has_user_namespace,
            landlock_abi = caps.landlock_abi_version,
            seccomp = caps.has_seccomp,
            cgroup_v2 = caps.has_cgroup_v2_delegation,
            "Linux sandbox capabilities detected"
        );

        caps
    }

    /// Select the strongest available backend.
    pub(crate) fn select_backend(&self) -> Option<SandboxBackend> {
        if !self.has_seccomp {
            warn!("seccomp not available — no native sandbox possible");
            return None;
        }

        if self.landlock_abi_version == 0 {
            warn!("landlock not available — no native sandbox possible");
            return None;
        }

        if self.has_user_namespace {
            debug!("full Linux sandbox available (namespace+seccomp+landlock)");
            Some(SandboxBackend::LinuxFull)
        } else {
            debug!("user namespace not available, using landlock+seccomp only");
            Some(SandboxBackend::LinuxLandlockSeccomp)
        }
    }
}

/// Check if unprivileged user namespaces are available.
#[cfg(target_os = "linux")]
fn detect_user_namespace() -> bool {
    // Method 1: Check sysctl (not all kernels expose this)
    if let Ok(content) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if content.trim() == "0" {
            debug!("unprivileged user namespaces disabled via sysctl");
            return false;
        }
    }

    // Method 2: Check AppArmor restriction (Ubuntu 24.04+)
    if let Ok(content) =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    {
        if content.trim() == "1" {
            debug!("unprivileged user namespaces restricted by AppArmor");
            return false;
        }
    }

    // Method 3: Try to create a user namespace (definitive test)
    // This is the most reliable check but has a small cost.
    // For Phase 1, we rely on the file-based checks above.
    // Phase 2 will add an actual unshare(CLONE_NEWUSER) test.

    debug!("user namespace appears available");
    true
}

/// Detect the Landlock ABI version by checking the LSM list.
#[cfg(target_os = "linux")]
fn detect_landlock_abi() -> u8 {
    // Check if Landlock is listed as an active LSM
    let lsm_list = match std::fs::read_to_string("/sys/kernel/security/lsm") {
        Ok(content) => content,
        Err(e) => {
            debug!("cannot read /sys/kernel/security/lsm: {e}");
            return 0;
        }
    };

    if !lsm_list.contains("landlock") {
        debug!("landlock not listed in active LSMs");
        return 0;
    }

    // Landlock is available; determine ABI version.
    // The actual ABI version is determined by creating a ruleset with
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION).
    // For Phase 1, we report ABI 1 as minimum if Landlock is active.
    // Phase 2 will use the `landlock` crate's `Compatible` trait for precise detection.
    debug!("landlock is active in LSM list");
    1
}

/// Check if seccomp-BPF is available.
#[cfg(target_os = "linux")]
fn detect_seccomp() -> bool {
    // Check /proc/sys/kernel/seccomp/actions_avail for BPF support
    if std::fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail").is_ok() {
        debug!("seccomp BPF actions available");
        return true;
    }

    // Fallback: check kernel config
    if let Ok(content) = std::fs::read_to_string("/proc/config.gz") {
        // This is compressed; skip for now
        let _ = content;
    }

    // Most modern kernels (3.17+) have seccomp
    debug!("assuming seccomp available (modern kernel)");
    true
}

/// Check if cgroups v2 delegation is available.
#[cfg(target_os = "linux")]
fn detect_cgroup_v2_delegation() -> bool {
    // Check if cgroups v2 unified hierarchy is mounted
    let controllers = match std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers") {
        Ok(content) => content,
        Err(_) => {
            debug!("cgroups v2 unified hierarchy not found");
            return false;
        }
    };

    // Check for user-level delegation (systemd)
    // A user-delegated cgroup should exist at /sys/fs/cgroup/user.slice/user-{UID}.slice/
    let uid = unsafe { libc::getuid() };
    let user_cgroup = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice");
    let delegated = std::path::Path::new(&user_cgroup).is_dir();

    debug!(
        controllers = controllers.trim(),
        delegated, "cgroups v2 detection"
    );

    delegated
}

// ── macOS capability detection ─────────────────────────────────────────────

/// macOS-specific sandbox capabilities.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct MacosCapabilities {
    /// Whether Seatbelt (`sandbox_init`) is available.
    pub has_seatbelt: bool,

    /// macOS version (major, minor). E.g., (15, 0) for macOS 15.0.
    pub os_version: (u32, u32),
}

#[cfg(target_os = "macos")]
impl MacosCapabilities {
    /// Detect macOS sandbox capabilities.
    pub fn detect() -> Self {
        let os_version = detect_macos_version();
        let has_seatbelt = detect_seatbelt();

        let caps = Self {
            has_seatbelt,
            os_version,
        };

        info!(
            seatbelt = caps.has_seatbelt,
            os_major = caps.os_version.0,
            os_minor = caps.os_version.1,
            "macOS sandbox capabilities detected"
        );

        caps
    }

    /// Select the available backend.
    fn select_backend(&self) -> Option<SandboxBackend> {
        if self.has_seatbelt {
            debug!("macOS Seatbelt sandbox available");
            Some(SandboxBackend::MacosSeatbelt)
        } else {
            warn!("Seatbelt not available on this macOS version");
            None
        }
    }
}

/// Detect macOS version from sysctl.
#[cfg(target_os = "macos")]
fn detect_macos_version() -> (u32, u32) {
    // Use sw_vers or sysctl to get version
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let version_str = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<&str> = version_str.trim().split('.').collect();
            let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            debug!(major, minor, "detected macOS version");
            (major, minor)
        }
        _ => {
            warn!("failed to detect macOS version, defaulting to (0, 0)");
            (0, 0)
        }
    }
}

/// Check if Seatbelt sandbox is available.
#[cfg(target_os = "macos")]
fn detect_seatbelt() -> bool {
    // sandbox-exec CLI exists (deprecated but still functional through macOS 15)
    let cli_exists = std::path::Path::new("/usr/bin/sandbox-exec").exists();

    // The real check: sandbox_init_with_parameters should be available via libSystem.
    // For Phase 1, we check the CLI as a proxy. Phase 3 will verify FFI availability
    // via dlsym at runtime.
    if cli_exists {
        debug!("sandbox-exec found at /usr/bin/sandbox-exec");
    } else {
        debug!("sandbox-exec not found");
    }

    // Seatbelt is available on macOS 10.5+ and still functional through macOS 15
    cli_exists
}

// ── Windows capability detection ───────────────────────────────────────────

/// Windows-specific sandbox capabilities.
///
/// P5.5 (2026-04-23): 移除 `has_appcontainer` 字段 + `detect_appcontainer_support`。
/// `AppContainer` 整块代码本来就没有调用入口（死代码，已删除），保留 capability
/// 探测只会让调用方误以为功能可用。
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct WindowsCapabilities {
    /// Whether Job Objects are available (always true on modern Windows).
    pub has_job_objects: bool,
}

#[cfg(target_os = "windows")]
impl WindowsCapabilities {
    /// Detect Windows sandbox capabilities.
    pub fn detect() -> Self {
        let caps = Self {
            // Job Objects are available on all supported Windows versions
            has_job_objects: true,
        };

        info!(
            job_objects = caps.has_job_objects,
            "Windows sandbox capabilities detected"
        );

        caps
    }

    /// Select the strongest available backend.
    fn select_backend(&self) -> Option<SandboxBackend> {
        if self.has_job_objects {
            // Restricted Token + Job Object is the default on Windows.
            debug!("Windows full sandbox available (restricted token + job object)");
            Some(SandboxBackend::WindowsFull)
        } else {
            warn!("Job Objects not available — unexpected on modern Windows");
            None
        }
    }
}

// ── Backend selection ──────────────────────────────────────────────────────

/// Check if Docker CLI is available.
fn docker_available() -> bool {
    which::which("docker").is_ok()
}

/// Select the best available sandbox runner for the given configuration.
///
/// This is the main entry point for the degradation chain.
pub fn select_runner(config: &SandboxConfig) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    // Validate config before selecting a runner
    config.validate()?;

    match config.backend {
        BackendPreference::Native => select_native_runner(config),
        BackendPreference::Docker => select_docker_runner(),
        BackendPreference::Auto => select_auto_runner(config),
    }
}

/// Select the best native runner, or error if none available.
fn select_native_runner(config: &SandboxConfig) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    #[cfg(not(target_os = "linux"))]
    let _ = config;

    #[cfg(target_os = "linux")]
    {
        crate::linux::select_native_runner(config)
            .map(|runner| Box::new(runner) as Box<dyn SandboxRunner>)
    }

    #[cfg(target_os = "macos")]
    {
        let caps = MacosCapabilities::detect();
        if let Some(_backend) = caps.select_backend() {
            return Ok(Box::new(crate::macos::MacosRunner::new(caps)));
        }
        Err(SandboxError::PlatformNotSupported {
            platform: "macos".into(),
            reason: "seatbelt not available".into(),
        })
    }

    #[cfg(target_os = "windows")]
    {
        let caps = WindowsCapabilities::detect();
        if let Some(backend) = caps.select_backend() {
            return Ok(Box::new(crate::windows::WindowsRunner::new(backend, caps)));
        }
        Err(SandboxError::PlatformNotSupported {
            platform: "windows".into(),
            reason: "job objects not available".into(),
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Err(SandboxError::PlatformNotSupported {
        platform: std::env::consts::OS.into(),
        reason: "unsupported operating system".into(),
    })
}

/// Select Docker fallback runner, or error if Docker is not available.
fn select_docker_runner() -> Result<Box<dyn SandboxRunner>, SandboxError> {
    if docker_available() {
        Ok(Box::new(crate::docker::DockerFallbackRunner::new()))
    } else {
        Err(SandboxError::PlatformNotSupported {
            platform: "docker".into(),
            reason: "docker CLI not found in PATH".into(),
        })
    }
}

// ── Self-sandbox API ───────────────────────────────────────────────────────

/// Validate workspace path for self-sandboxing (subset of `SandboxConfig::validate`).
fn validate_workspace(workspace: &std::path::Path) -> Result<(), SandboxError> {
    if !workspace.is_absolute() {
        return Err(SandboxError::PathError {
            path: workspace.to_path_buf(),
            reason: "workspace path must be absolute".into(),
        });
    }
    if workspace
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(SandboxError::PathError {
            path: workspace.to_path_buf(),
            reason: "workspace path contains '..' traversal".into(),
        });
    }
    Ok(())
}

/// Apply sandbox constraints to the **current** process (self-sandboxing).
///
/// This is irreversible — once applied, the sandbox cannot be removed.
/// Child processes inherit the restrictions.
///
/// Used by the persistent Worker process (`worker-start`) to sandbox itself
/// when started directly by the Go bridge (which bypasses the launcher's
/// `pre_exec` sandbox path).
///
/// # Errors
///
/// Returns [`SandboxError`] if the platform sandbox cannot be applied.
// `clippy::needless_return` fires on the cfg-gated tail expressions because on
// each platform only one arm is active and the explicit `return` becomes
// "the last statement". We keep `return` to make the cfg-mux symmetric across
// all four arms and avoid trailing-expression coupling between unrelated cfgs.
#[allow(clippy::needless_return)]
pub fn apply_sandbox_to_self(config: &SandboxConfig) -> Result<(), SandboxError> {
    // NOTE: We do NOT call config.validate() here because validate() requires
    // a non-empty command, which is irrelevant for self-sandboxing (we sandbox
    // the current process, not a new command). Only workspace validity matters.
    validate_workspace(&config.workspace)?;

    #[cfg(target_os = "macos")]
    {
        return apply_to_self_macos(config);
    }

    #[cfg(target_os = "linux")]
    {
        return apply_to_self_linux(config);
    }

    #[cfg(target_os = "windows")]
    {
        return apply_to_self_windows(config);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = config;
        Err(SandboxError::PlatformNotSupported {
            platform: std::env::consts::OS.into(),
            reason: "self-sandbox not supported on this platform".into(),
        })
    }
}

/// macOS self-sandbox: generate Seatbelt profile → apply to current process.
#[cfg(target_os = "macos")]
fn apply_to_self_macos(config: &SandboxConfig) -> Result<(), SandboxError> {
    use crate::macos::{ffi, seatbelt};

    // 1. Generate SBPL profile from config
    let profile = seatbelt::generate_profile(config)?;

    // 2. Build parameter references
    let params_refs: Vec<(&str, &str)> = profile
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // 3. Create SandboxArgs and apply to current process
    let sandbox_args = ffi::SandboxArgs::new(&profile.sbpl, &params_refs)?;
    sandbox_args.apply().map_err(|e| SandboxError::Seatbelt {
        message: format!("self-sandbox failed: {e}"),
    })?;

    info!(
        security_level = ?config.security_level,
        workspace = %config.workspace.display(),
        "seatbelt self-sandbox applied (irreversible)"
    );

    Ok(())
}

/// Linux self-sandbox: apply Landlock rules + Seccomp filter + NoNewPrivs to current process.
///
/// ROOT-CAUSE FIX: 旧实现只调 Landlock，没调 Seccomp、没设 NoNewPrivs。
/// 而 `verify_sandbox_active` canary 查的是 `/proc/self/status` 的
/// `Seccomp: >= 2` 和 `NoNewPrivs: 1` —— self-sandbox 路径的 canary 必然失败。
/// Go direct-spawn 路径下 Worker 就算"自沙箱"也只做了文件系统过滤，
/// 没有 syscall 层隔离。现补齐三件：
///   1. `prctl(PR_SET_NO_NEW_PRIVS, 1)` — 在 seccomp 之前必设，否则 non-root
///      加载 seccomp filter 会 EPERM；canary 检查同一位
///   2. Landlock 文件系统约束（原有）
///   3. Seccomp-BPF syscall 过滤（新增）
///
/// 顺序严格：NoNewPrivs → Landlock → Seccomp。Seccomp 过滤生效后就不能
/// 再改 Landlock，所以 Landlock 必须先于 Seccomp；NoNewPrivs 又必须最先
/// （否则 Seccomp load 失败）。
#[cfg(target_os = "linux")]
fn apply_to_self_linux(config: &SandboxConfig) -> Result<(), SandboxError> {
    use crate::linux::{landlock, seccomp};

    // 1. PR_SET_NO_NEW_PRIVS — 让后续 setuid binary 无法获得新特权，
    // 同时是 non-root 加载 seccomp filter 的前置条件。
    // SAFETY: prctl 是 pure syscall，无内存生命周期要求。
    let prctl_rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_i32, 0_i32, 0_i32, 0_i32) };
    if prctl_rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(SandboxError::InvalidConfig {
            message: format!("prctl(PR_SET_NO_NEW_PRIVS): {errno}"),
        });
    }

    // 2. Landlock 文件系统约束（已有实现）
    landlock::apply_landlock_rules(config)?;

    // 3. Seccomp syscall 过滤 — canary 检查 `/proc/self/status` 的
    // `Seccomp: 2` 行，只有走到这里 self-sandbox 才算客观成立。
    seccomp::apply_seccomp_filter(config)?;

    info!(
        security_level = ?config.security_level,
        workspace = %config.workspace.display(),
        "linux self-sandbox applied (NoNewPrivs + Landlock + Seccomp, irreversible)"
    );

    Ok(())
}

/// Windows self-sandbox: assign current process to a Job Object with resource limits.
///
/// This is a lighter form of isolation than the full sandbox runner — it applies
/// resource limits (memory, CPU, process count) via Job Objects but does not create
/// a restricted token or modify ACLs (since we can't restrict the current process
/// token after startup).
///
/// Irreversible: once assigned to a Job Object, the process cannot leave it.
#[cfg(target_os = "windows")]
fn apply_to_self_windows(config: &SandboxConfig) -> Result<(), SandboxError> {
    use crate::windows::job;

    // Create a Job Object with resource limits from config
    let job_guard = job::create_job_object(&config.resource_limits)?;

    // Assign the current process to the Job Object
    // SAFETY: GetCurrentProcess returns a pseudo-handle that is always valid.
    let current_process = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
    job_guard.assign_process(current_process)?;

    // Intentionally leak the job guard — we want the Job Object to persist
    // for the lifetime of the process. Drop would close the handle and
    // terminate all processes in the job (KILL_ON_JOB_CLOSE).
    std::mem::forget(job_guard);

    info!(
        security_level = ?config.security_level,
        workspace = %config.workspace.display(),
        "Windows Job Object self-sandbox applied (resource limits only)"
    );

    Ok(())
}

/// Canary 检测：验证当前进程是否真的受到沙箱/资源控制约束。
///
/// ROOT-CAUSE FIX 背景：之前 Worker 完全信 `OA_SANDBOX_APPLIED=1` 环境变量
/// 决定"已沙箱"，但 launcher `侧（apply_windows_sandbox` / `apply_linux_sandbox`）
/// 是空函数。Worker 收到 env=1 就跳过自沙箱 → 零隔离执行。
///
/// 本函数在 Worker 启动时 / `apply_sandbox_to_self` 之后调用，做客观探测：
///
/// - Windows: `IsProcessInJob` 检查当前进程是否在 Job Object 中
/// - Linux: /proc/self/status 里的 `Seccomp:` 行 != 0 且 `NoNewPrivs: 1`
/// - macOS: 暂时仅检查 `OA_SANDBOX_APPLIED` env（Seatbelt 状态读取需 FFI，
///   留 Phase 2 完善；但 macOS launcher 本来就有真实实现，风险较低）
///
/// `SecurityLevel::L0Deny` 表示"禁止执行"并非真沙箱（Worker 根本不该启动
/// 到这一步），这里仅在 L1/L2 严格校验。
// Same cfg-mux pattern as `apply_sandbox_to_self`; see comment there.
#[allow(clippy::needless_return)]
pub fn verify_sandbox_active(
    security_level: crate::config::SecurityLevel,
) -> Result<(), SandboxError> {
    use crate::config::SecurityLevel;

    // L0Deny 不需要沙箱（Worker 不会执行命令）
    if security_level == SecurityLevel::L0Deny {
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        return verify_windows_sandbox_active();
    }

    #[cfg(target_os = "linux")]
    {
        return verify_linux_sandbox_active();
    }

    #[cfg(target_os = "macos")]
    {
        return verify_macos_sandbox_active();
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = security_level;
        Err(SandboxError::PlatformNotSupported {
            platform: std::env::consts::OS.into(),
            reason: "verify_sandbox_active not supported on this platform".into(),
        })
    }
}

#[cfg(target_os = "windows")]
fn verify_windows_sandbox_active() -> Result<(), SandboxError> {
    use windows::Win32::System::JobObjects::IsProcessInJob;
    use windows::Win32::System::Threading::GetCurrentProcess;

    // ROOT-CAUSE FIX (verify canary 编译恢复 2026-04-23):
    //
    // 旧代码用 `windows::Win32::Foundation::BOOL`，但 `windows` crate 0.62 起
    // 已把 `BOOL` 从 `Foundation` 移到 `windows_core::BOOL`（= `windows::core::BOOL`），
    // Foundation 下不再 re-export；同时移除了 `BOOL::as_bool()` 方法，
    // 改用 tuple struct 直接访问 `.0 != 0`。
    //
    // 上一次 "P4 复审 11 项修复" 的影响矩阵声称 Windows verify canary 已实装为
    // IsProcessInJob（见架构文档 §4），但实际代码从来就没 compile 过 —
    // Windows 分支上 `cargo check` 会在此函数失败。这是文档与实装严重脱节
    // 的第 12 项根因。现在对齐 windows 0.62 的正确 API。
    let mut in_job = windows::core::BOOL(0);
    // SAFETY: GetCurrentProcess 返回伪句柄，恒定有效。IsProcessInJob 第二参数
    // 传 None 表示"任何 Job"，第三参数是 out-param，指向栈上 BOOL，生存期
    // 覆盖整个 unsafe 块。
    let result = unsafe { IsProcessInJob(GetCurrentProcess(), None, &raw mut in_job) };
    if result.is_err() {
        return Err(SandboxError::InvalidConfig {
            message: format!("IsProcessInJob failed: {result:?}"),
        });
    }
    if in_job.0 == 0 {
        return Err(SandboxError::InvalidConfig {
            message: "sandbox canary failed: process is NOT in a Job Object — sandbox was never applied despite claims".into(),
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_macos_sandbox_active() -> Result<(), SandboxError> {
    // ROOT-CAUSE FIX: 原实现是双路 no-op（env=1 直接返回 Ok；env 未设也
    // 返回 Ok）。现在调 Apple 私有 API `sandbox_check` 客观查 kernel：
    // 对 `/etc/master.passwd` 的 write 在 L1/L2 profile 的 `(deny default)`
    // 下必然被拒。若 kernel 说允许，沙箱要么未激活，要么 profile 有漏洞，
    // 两种情况都视为 canary 失败。
    //
    // 与 Windows IsProcessInJob / Linux /proc/self/status 同级的客观验证。
    const CANARY_PATH: &str = "/etc/master.passwd";
    match crate::macos::ffi::seatbelt_canary_write_denied(CANARY_PATH) {
        Ok(true) => Ok(()), // 被拒 = 沙箱生效
        Ok(false) => Err(SandboxError::InvalidConfig {
            message: format!(
                "sandbox canary failed: write to {CANARY_PATH} was NOT denied by Seatbelt — profile not active or too permissive"
            ),
        }),
        Err(e) => Err(SandboxError::InvalidConfig {
            message: format!("sandbox canary FFI error: {e}"),
        }),
    }
}

#[cfg(target_os = "linux")]
fn verify_linux_sandbox_active() -> Result<(), SandboxError> {
    // 读 /proc/self/status 检查 Seccomp 和 NoNewPrivs
    let status = std::fs::read_to_string("/proc/self/status").map_err(|e| SandboxError::Io {
        context: "verify_sandbox_active: read /proc/self/status".into(),
        source: e,
    })?;

    let mut seccomp_mode: Option<u32> = None;
    let mut no_new_privs: Option<u32> = None;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Seccomp:") {
            seccomp_mode = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("NoNewPrivs:") {
            no_new_privs = v.trim().parse().ok();
        }
    }

    // Seccomp=2 表示 filter 模式激活；Landlock 没有 /proc 条目，靠 Seccomp 作 proxy
    match seccomp_mode {
        Some(mode) if mode >= 2 => {}
        _ => {
            return Err(SandboxError::InvalidConfig {
                message: format!(
                    "sandbox canary failed: Seccomp mode={:?} (expected >= 2 = filter active)",
                    seccomp_mode
                ),
            });
        }
    }
    match no_new_privs {
        Some(1) => {}
        _ => {
            return Err(SandboxError::InvalidConfig {
                message: format!(
                    "sandbox canary failed: NoNewPrivs={:?} (expected 1)",
                    no_new_privs
                ),
            });
        }
    }
    Ok(())
}

/// Auto-select: try native first, then Docker fallback.
fn select_auto_runner(config: &SandboxConfig) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    match select_native_runner(config) {
        Ok(runner) => {
            info!(backend = runner.name(), "selected native sandbox backend");
            Ok(runner)
        }
        Err(native_err) => {
            info!(
                native_error = %native_err,
                "native sandbox unavailable, trying Docker fallback"
            );

            match select_docker_runner() {
                Ok(runner) => {
                    warn!("using Docker fallback — native sandbox unavailable");
                    Ok(runner)
                }
                Err(docker_err) => Err(SandboxError::NoBackendAvailable {
                    native_reason: native_err.to_string(),
                    docker_reason: docker_err.to_string(),
                }),
            }
        }
    }
}
