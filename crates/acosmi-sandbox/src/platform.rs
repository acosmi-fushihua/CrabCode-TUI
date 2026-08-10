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

// ── Exec-backend 能力探测（W-SANDBOX-ENFORCED-DEADCODE PR-2）───────────────

/// `crabcode sandbox-probe` 的后端能力结论。
///
/// 只回答一个问题：**这台机器上，`sandbox-exec` 的自沙箱路径现在能用吗？**
/// 不可用是一个被报告的事实（`available:false` + `reason`），不是错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBackendProbe {
    pub available: bool,
    /// 不可用的原因 slug（单 token，TS 侧原样拼进 `backend-unavailable:<reason>`）。
    pub reason: Option<&'static str>,
}

/// 本平台没有自沙箱实现。
pub const PROBE_REASON_PLATFORM_UNSUPPORTED: &str = "platform-unsupported";
/// Windows：受限令牌 / 作业对象 / 子进程创建这一链在本机跑不起来。
pub const PROBE_REASON_WINDOWS_SPAWN_FAILED: &str = "windows-sandbox-spawn-failed";
/// Windows：链路能跑，但一次 spawn 的开销越过了性能门。
pub const PROBE_REASON_WINDOWS_TOO_SLOW: &str = "windows-sandbox-too-slow";

/// Windows 性能门：一次沙箱 spawn 的墙钟上限。
///
/// 不是审美取值 —— 每一条 Bash 命令都要付这笔钱，它直接叠在用户等待上。越过就
/// 不是"慢一点"，是把交互式工具变成不可用（本立项立项前实测 46 s/次，根因见
/// [`crate::windows::acl`]）。越门时探测返 `available:false` 走诚实降级：宽松档
/// 披露真因、严格档确定性拒绝，**绝不**假装可用后让用户逐条命令去发现。
#[cfg(target_os = "windows")]
pub const WINDOWS_SPAWN_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);
/// Landlock 不可用（内核太老 / LSM 未启用 / ruleset 创建被拒）。
pub const PROBE_REASON_LANDLOCK_UNAVAILABLE: &str = "landlock-unavailable";
/// seccomp-BPF 不可用。
pub const PROBE_REASON_SECCOMP_UNAVAILABLE: &str = "seccomp-unavailable";
/// Seatbelt 不可用。
pub const PROBE_REASON_SEATBELT_UNAVAILABLE: &str = "seatbelt-unavailable";

/// 探测 `sandbox-exec` 自沙箱后端的可用性。
///
/// # 铁律：探测**不得**有副作用
///
/// 探测跑在**宿主自己的进程**里（`crabcode sandbox-probe`）。施加沙箱是
/// **不可逆**的——`landlock_restrict_self` / `seccomp load` / `sandbox_init`
/// 一旦调用就没有退路。所以这里只做「能不能创建」级别的动作：Landlock 创建一个
/// ruleset fd 随即丢弃（不 `restrict_self`），seccomp 只读 `/proc` 探测，
/// Seatbelt 只看实现存在性。
///
/// 探测通过 ≠ 运行期一定成功：真失败由 125 协议在**那一条命令**上诚实上报，
/// 并把会话翻成降级。探测的职责是挡住「这台机器根本没有这个能力」，不是预演。
#[must_use]
#[allow(clippy::needless_return)]
pub fn probe_exec_backend() -> ExecBackendProbe {
    #[cfg(target_os = "linux")]
    {
        return probe_exec_backend_linux();
    }

    #[cfg(target_os = "macos")]
    {
        let caps = MacosCapabilities::detect();
        return if caps.select_backend().is_some() {
            ExecBackendProbe {
                available: true,
                reason: None,
            }
        } else {
            ExecBackendProbe {
                available: false,
                reason: Some(PROBE_REASON_SEATBELT_UNAVAILABLE),
            }
        };
    }

    #[cfg(target_os = "windows")]
    {
        return probe_exec_backend_windows();
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        return ExecBackendProbe {
            available: false,
            reason: Some(PROBE_REASON_PLATFORM_UNSUPPORTED),
        };
    }
}

/// Windows 探测 = **真跑一次**沙箱 spawn 并给它计时。
///
/// # 为什么不是静态能力查询
///
/// 这条链上历史事故全部出在"能不能"与"多快"两件事上，而两件事都查不出来：
/// `CreateRestrictedToken` 曾在本机直接失败（0x539/0x579）、受限令牌下的
/// `cmd.exe` 曾被 NT loader 拒绝（0xC0000022）、ACL 授权曾把每次 spawn 拖到
/// 46 s。这三件没有一件能靠"Job Object 可用吗"这种能力位问出来。所以探测跑的
/// 就是**发货那条路径**（[`crate::windows::exec::run_child`]，与 helper 逐字
/// 同一个函数），而不是一个形状相似的替身 —— 测替身等于没测。
///
/// 三重克制让"真跑"是安全的：workspace 是一个刚建的空临时目录（不碰用户的树，
/// 也就不会付 `O(对象数)`）、子进程 stdio 全指向 `NUL`（探测的 stdout 必须逐字
/// 只有一行 JSON）、并且带 5 s 硬超时（会挂死的探测比没有探测更糟）。
#[cfg(target_os = "windows")]
fn probe_exec_backend_windows() -> ExecBackendProbe {
    use crate::config::SecurityLevel;
    use crate::windows::exec::{ChildSpec, ChildStdio, run_child};

    let caps = WindowsCapabilities::detect();
    if caps.select_backend().is_none() {
        return ExecBackendProbe {
            available: false,
            reason: Some(PROBE_REASON_WINDOWS_SPAWN_FAILED),
        };
    }

    let Some(workspace) = probe_scratch_dir() else {
        return ExecBackendProbe {
            available: false,
            reason: Some(PROBE_REASON_WINDOWS_SPAWN_FAILED),
        };
    };

    // `cmd.exe` 刻意不是随便选的：受限令牌下的 cmd.exe 正是 2026-05 那次
    // ACCESS_DENIED 的受害者，拿它当探测负载 = 每次会话都在复验那个修复。
    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let started = std::time::Instant::now();
    let outcome = run_child(&ChildSpec {
        workspace: &workspace,
        security_level: SecurityLevel::L1Allowlist,
        program: &comspec,
        args: &["/C".to_string(), "exit 0".to_string()],
        stdio: ChildStdio::Null,
        timeout: Some(std::time::Duration::from_secs(5)),
    });
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&workspace);

    match outcome {
        Ok(0) => {}
        Ok(code) => {
            debug!(
                exit_code = code,
                "windows sandbox probe child exited non-zero"
            );
            return ExecBackendProbe {
                available: false,
                reason: Some(PROBE_REASON_WINDOWS_SPAWN_FAILED),
            };
        }
        Err(e) => {
            debug!(error = %e, "windows sandbox probe spawn failed");
            return ExecBackendProbe {
                available: false,
                reason: Some(PROBE_REASON_WINDOWS_SPAWN_FAILED),
            };
        }
    }

    if elapsed > WINDOWS_SPAWN_BUDGET {
        warn!(
            elapsed_ms = elapsed.as_millis(),
            budget_ms = WINDOWS_SPAWN_BUDGET.as_millis(),
            "windows sandbox spawn is over budget — reporting the backend as unavailable"
        );
        return ExecBackendProbe {
            available: false,
            reason: Some(PROBE_REASON_WINDOWS_TOO_SLOW),
        };
    }

    debug!(
        elapsed_ms = elapsed.as_millis(),
        "windows sandbox probe passed"
    );
    ExecBackendProbe {
        available: true,
        reason: None,
    }
}

/// A fresh, empty directory for the probe to use as a workspace.
///
/// Empty is the requirement, not a convenience: the probe must measure this
/// machine's fixed spawn cost, and a directory with a tree under it would fold
/// that tree's size into the number (see [`crate::windows::acl`]).
#[cfg(target_os = "windows")]
fn probe_scratch_dir() -> Option<std::path::PathBuf> {
    let unique = format!(
        "crabcode-sandbox-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[cfg(target_os = "linux")]
fn probe_exec_backend_linux() -> ExecBackendProbe {
    use landlock::{Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};

    let caps = LinuxCapabilities::detect();
    if !caps.has_seccomp {
        return ExecBackendProbe {
            available: false,
            reason: Some(PROBE_REASON_SECCOMP_UNAVAILABLE),
        };
    }

    // 真探测：向内核要一个 ruleset。`/sys/kernel/security/lsm` 里列着 landlock
    // **不等于**它能用（容器 / 老 ABI / seccomp 已经拦了这个 syscall 都会让
    // 创建失败）。按名字猜是 SoT §3 #26 点名禁止的做法。
    //
    // 只 create、**绝不** restrict_self —— 后者不可逆，会把探测进程自己关进
    // 沙箱，而这个进程接下来还要打印 JSON 并正常退出。
    let created = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(crate::linux::landlock::REQUIRED_FS_ABI))
        .and_then(Ruleset::create);
    match created {
        Ok(ruleset) => {
            drop(ruleset); // fd 随即关闭；进程状态零改动。
            ExecBackendProbe {
                available: true,
                reason: None,
            }
        }
        Err(e) => {
            debug!(error = %e, "landlock ruleset creation failed during probe");
            ExecBackendProbe {
                available: false,
                reason: Some(PROBE_REASON_LANDLOCK_UNAVAILABLE),
            }
        }
    }
}

// ── Plan-level self-sandbox（W-SANDBOX-ENFORCED-DEADCODE PR-2）─────────────

/// 一次计划施加的结果。
///
/// `notices` 是**结构性事实**：这条规则在这个平台上兑现不了、这条 allow 指向
/// 一个还不存在的路径、这条模式串翻译不出来。它们对同一个会话里的每条命令
/// 都一样，所以**默认不进命令的 stderr**（那会把同样几行注入模型每一次的工具
/// 结果里）——`CRABCODE_SANDBOX_EXEC_VERBOSE=1` 时才打印。
///
/// 「不打印」不等于「被吞掉」：它们在这里被逐条计算、命名、并原样返回给调用方。
/// 面向用户的常规披露走的是**保真度报告**那条链（TS 侧 `fidelity` → 降级层
/// 与 auto-allow 宽免），不是这里。
pub struct ExecPlanReport {
    /// 施加过程中的异常（罕见、值得当场喊）。
    pub warnings: Vec<String>,
    /// 结构性事实（恒定、按需查看）。
    pub notices: Vec<String>,
}

/// 打印 `notices` 的开关。见 [`ExecPlanReport`]。
pub const SANDBOX_EXEC_VERBOSE_ENV: &str = "CRABCODE_SANDBOX_EXEC_VERBOSE";

// ── 出口锁的运行期实证（W-SANDBOX-ENFORCED-DEADCODE PR-8）─────────────────
//
// cfg 用的是 `linux`/`macos` 的并集而不是 `unix`：`libc` 在本 crate 的
// `Cargo.toml` 里只对这两个 target 声明，写成 `cfg(unix)` 会让别的 Unix
// （FreeBSD 等，当前走 `PlatformNotSupported` 分支）编不过。两者对本立项要过的
// 三个 target 完全等价。

/// 非阻塞 connect 探针。**当且仅当** `connect()` 被内核沙箱**同步拒绝**
/// （errno `EPERM` 或 `EACCES`）时返回 `true`。
///
/// 其它每一种结果 —— `EINPROGRESS`、立刻成功、`ECONNREFUSED`、任何别的 errno、
/// 以及 `socket()` 本身失败 —— 一律返回 `false`。**构造上 fail-closed**：只有
/// 一次明确的权限拒绝才算「挡住了」。别的失败原因（fd 耗尽、路由不可达、
/// 端口没人听）看起来也像「连不上」，但它们证明不了沙箱在生效，把它们算作
/// 证据就等于允许一个没锁上的沙箱冒充锁上了。
///
/// 非阻塞是必须的而不是优化：探针打的是一个不可路由的地址，阻塞 connect 会在
/// 内核 TCP 重传超时里挂上一两分钟。
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn connect_syscall_denied(addr: [u8; 4], port: u16) -> bool {
    // SAFETY: socket(2) 参数全是常量，不涉及任何内存生命周期。
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        // socket() 失败**不是**「被挡住」的证据（EMFILE 也长这样）。
        return false;
    }

    // 转非阻塞。`SOCK_NONBLOCK` 是 Linux 扩展，Darwin 没有，所以走两侧都有的
    // fcntl。拿不到 flags 就不设：探针会退化成阻塞式，慢，但结论不会变错。
    // SAFETY: fcntl(2) 对本进程自己的 fd 操作，无内存要求。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = port.to_be();
    // `from_ne_bytes` 让 u32 的内存布局逐字节等于 `addr`，也就是网络字节序
    // ——`s_addr` 要的正是这个。
    sa.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(addr),
    };

    // SAFETY: `sa` 是本栈帧上一个已完整初始化的 sockaddr_in，长度如实传入；
    // connect(2) 只在调用期间读它。
    let rc = unsafe {
        libc::connect(
            fd,
            std::ptr::addr_of!(sa).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    let errno = if rc == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    };

    // 无条件关闭：探针跑在自沙箱之后、`exec` 之前，漏一个 fd 就是把一个半开
    // 套接字送进用户的命令。
    // SAFETY: `fd` 是上面刚拿到的、本函数独占的有效 fd。
    unsafe {
        libc::close(fd);
    }

    matches!(errno, Some(libc::EPERM) | Some(libc::EACCES))
}

/// 出口锁施加完之后**证明**它生效了——证不出来就 `Err`，helper 按 125 协议失败。
///
/// 一条命令绝不能在「以为自己被过滤了」的状态下跑起来：那比没有沙箱更糟，因为
/// 上层会据此放宽审批（`isSandboxAutoAllowActive`）。
///
///   (a) 外部 TCP（`192.0.2.1:80`，RFC 5737 TEST-NET-1，永不可路由）**必须**被拒；
///   (b) 代理端口（`127.0.0.1:<proxy_port>`）**必须不**被拒，否则锁得太死、
///       流量根本到不了代理，过滤同样不会发生。
///
/// # 两种已知的误报形态（都朝失败关闭那边倒，但文案会指错方向）
///
/// 1. **`policy == None` + 非零代理端口**：Linux 侧 `socket()` 直接 EPERM，
///    探针按契约返回 `false`，于是本函数报 (a) 失败。那是一份自相矛盾的配置
///    （既要求「完全无网络」又要求「走代理」），失败关闭是正确方向，但错误
///    文案会指向内核而不是配置。TS 侧不产生这种组合。
/// 2. **`proxy_port == 80`（仅 Linux）**：Landlock 的网络规则是端口模型、没有
///    地址维度，所以放行「代理端口」就等于放行**任意主机**的该端口 —— 探针 (a)
///    打的正好是 `:80`，于是它会被放行、判定「没锁上」、整条命令失败。macOS
///    不受影响（SBPL 锁的是 `localhost:<port>`，带地址）。实践中够不着：代理
///    监听的是内核分配的高位回环端口，且 Unix 上绑 80 要 root。真撞上时改的是
///    探针端口，不是放宽判据。
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_egress_locked_to_proxy(proxy_port: u16) -> Result<(), SandboxError> {
    if !connect_syscall_denied([192, 0, 2, 1], 80) {
        return Err(SandboxError::InvalidConfig {
            message: "network egress canary failed: external TCP was NOT blocked after applying \
                      the proxy egress lock — the kernel did not enforce it (kernel too old for \
                      Landlock network / SBPL host-token mismatch). Refusing to run unfiltered."
                .into(),
        });
    }
    if connect_syscall_denied([127, 0, 0, 1], proxy_port) {
        return Err(SandboxError::InvalidConfig {
            message: "network egress canary failed: the filtering proxy port is itself blocked — \
                      the egress lock is too strict and traffic cannot reach the proxy."
                .into(),
        });
    }
    Ok(())
}

/// 按执行计划自沙箱当前进程（`crabcode sandbox-exec` 的 Unix 路径）。
///
/// 与 [`apply_sandbox_to_self`] 的差别就是本立项存在的理由：那个只吃
/// [`SandboxConfig`]，而 `SandboxConfig` 表达不了 fs 四表 —— 事故里丢掉的正是
/// 那批规则（SoT §1.5「配置保真度缺口」）。
///
/// 施加**不可逆**，且被 `exec` 出来的进程继承——这正是 helper 需要的语义。
///
/// # Errors
///
/// 施加失败即错误：调用方（helper）必须按 125 协议失败，**绝不**降级裸跑。
#[allow(clippy::needless_return)]
pub fn apply_exec_plan_to_self(
    plan: &crate::exec_config::SandboxExecPlan,
) -> Result<ExecPlanReport, SandboxError> {
    let (resolved, notices) = resolve_plan_and_notices(plan)?;

    #[cfg(target_os = "macos")]
    {
        return apply_plan_to_self_macos(plan, &resolved, notices);
    }

    #[cfg(target_os = "linux")]
    {
        return apply_plan_to_self_linux(plan, &resolved, notices);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (&resolved, notices);
        Err(SandboxError::PlatformNotSupported {
            platform: std::env::consts::OS.into(),
            reason: "self-sandboxing is not how this platform runs a plan — \
                     Windows relays through `run_exec_plan_as_child`"
                .into(),
        })
    }
}

/// Everything both plan entry points must do before they diverge: validate the
/// workspace, resolve the fs tables against cwd/home, and name every rule that
/// will not be honoured.
///
/// Shared on purpose. These notices are the only per-command forensic trail
/// this link has — the config file is deleted the moment it is read, and on Unix
/// the process becomes the user's command a few lines later — so "the Windows
/// arm forgot to compute them" would be an invisible loss, not a visible bug.
fn resolve_plan_and_notices(
    plan: &crate::exec_config::SandboxExecPlan,
) -> Result<(crate::exec_rules::ResolvedFsRules, Vec<String>), SandboxError> {
    validate_workspace(&plan.base.workspace)?;

    let resolved = crate::exec_rules::resolve_fs_rules(
        &plan.filesystem,
        &plan.base.workspace,
        crate::exec_rules::home_dir_from_env().as_deref(),
    );

    let mut notices: Vec<String> = Vec::new();
    for rule in &resolved.unresolvable {
        // 翻译不出来的规则**不会**被近似成一个更宽（allow）或更窄（deny）的
        // 范围——近似正是本立项要消灭的「假隔离」。allow 翻译失败只会让
        // 沙箱更严，保留可取证 notice；deny 翻译失败会直接拆掉用户要求的防线，
        // 所有平台都必须在 exec 前 fail closed（CLI 映射为 125）。
        let message = format!(
            "{} `{}` cannot be applied: {}",
            rule.kind.wire_name(),
            rule.source,
            rule.why
        );
        if matches!(
            rule.kind,
            crate::exec_rules::FsRuleKind::DenyRead | crate::exec_rules::FsRuleKind::DenyWrite
        ) {
            return Err(SandboxError::InvalidConfig {
                message: format!("refusing to run with an unenforceable deny rule; {message}"),
            });
        }
        notices.push(format!("{message}; access remains denied by default"));
    }

    // TS 侧已经知道这几条兑现不了（保真度报告是它算的），但 helper 是这条链上
    // **唯一**能被事后单独取证的环节：配置文件读完就删了，进程也马上变成别的
    // 命令。把它原样念一遍，取证时才有东西可读。
    if !plan.fidelity.unenforced.is_empty() {
        notices.push(format!(
            "the config was already marked partial by the policy layer; \
             unenforceable by declaration: {}",
            plan.fidelity.unenforced.join(", ")
        ));
    }

    // `weaker.networkIsolation` 自 PR-8 起在 macOS 上**真的生效**了（Seatbelt
    // 放行 trustd，见 `seatbelt::generate_profile_for_plan`）。它在 schema 上就是
    // macOS-only，所以别的平台上它依旧是个 carried-but-unused 的开关——而
    // **carried-but-unused 必须出声**：一个被读进来、校验过、然后谁也没用的安全
    // 开关，正是 E1「可选安全字段链式失活」的形状。这次方向是「更弱」，不生效
    // 是安全的，但用户仍然有权知道他打开的开关在这台机器上没有任何效果。
    #[cfg(not(target_os = "macos"))]
    if plan.weaker.network_isolation {
        notices.push(
            "weaker.networkIsolation has no effect on this platform — it relaxes macOS \
             Seatbelt's Mach service filter (TLS trust evaluation) and has no counterpart \
             here; the sandbox is STRICTER than this flag asks for"
                .to_string(),
        );
    }

    Ok((resolved, notices))
}

/// macOS：SBPL profile（含 fs 四表）→ 施加 → **逐条实测 deny**。
#[cfg(target_os = "macos")]
fn apply_plan_to_self_macos(
    plan: &crate::exec_config::SandboxExecPlan,
    resolved: &crate::exec_rules::ResolvedFsRules,
    mut notices: Vec<String>,
) -> Result<ExecPlanReport, SandboxError> {
    use crate::macos::{ffi, seatbelt};

    let profile = seatbelt::generate_profile_for_plan(plan, resolved)?;
    let params_refs: Vec<(&str, &str)> = profile
        .params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let sandbox_args = ffi::SandboxArgs::new(&profile.sbpl, &params_refs)?;
    sandbox_args.apply().map_err(|e| SandboxError::Seatbelt {
        message: format!("self-sandbox failed: {e}"),
    })?;

    // ── deny 段的运行期实证 ────────────────────────────────────────────────
    //
    // SBPL 里 deny 压过 allow 靠的是「后写的规则赢」，而这条优先级假设本身没有
    // 编译期证据。所以施加完就当场问 kernel：这些路径现在真的写不了 / 读不了吗？
    // 证不出来 ⇒ 整条命令失败。放一个「以为挡住了」的沙箱出去，比没有沙箱更糟。
    //
    // 落在任何 allow 之外的 deny 由 `(deny default)` 天然兑现，同样会通过本检查
    // ——所以这里不需要先算重叠，全量实测既更简单也更严。
    for (rules, op, table) in [
        (
            &resolved.deny_write,
            ffi::OP_FILE_WRITE_DATA,
            "filesystem.denyWrite",
        ),
        (
            &resolved.deny_read,
            ffi::OP_FILE_READ_DATA,
            "filesystem.denyRead",
        ),
    ] {
        for rule in rules {
            let path = seatbelt::canonicalize_best_effort(&rule.path);
            let path_str = path.to_string_lossy();
            let denied = ffi::seatbelt_operation_denied(op, &path_str)?;
            if !denied {
                return Err(SandboxError::InvalidConfig {
                    message: format!(
                        "sandbox canary failed: {table} `{}` is still permitted after applying the \
                         profile — the deny rule did not take effect (resolved to `{path_str}`)",
                        rule.source
                    ),
                });
            }
        }
    }

    if !plan.network.allow_unix_sockets.is_empty() && !plan.network.allow_all_unix_sockets {
        notices.push(
            "network.allowUnixSockets is enforced as an all-or-nothing switch on macOS: \
             Seatbelt filters unix sockets by rule, not by the paths listed here"
                .to_string(),
        );
    }

    // ── 出口锁的运行期实证（PR-8）─────────────────────────────────────────
    //
    // 必须在**全部**限制都施加完之后（sandbox_init + 上面的 deny 实测）才问，
    // 否则问到的是一个还没锁上的进程。`?` 直通既有的 125 路径。
    if plan.network.http_proxy_port > 0 {
        verify_egress_locked_to_proxy(plan.network.http_proxy_port)?;
        notices.push(format!(
            "egress lock verified against proxy port {}: external TCP is denied by the kernel \
             and the proxy is reachable",
            plan.network.http_proxy_port
        ));
    }

    info!(
        security_level = ?plan.base.security_level,
        workspace = %plan.base.workspace.display(),
        deny_rules = resolved.deny_read.len() + resolved.deny_write.len(),
        proxy_port = plan.network.http_proxy_port,
        "seatbelt exec-plan self-sandbox applied (irreversible, deny rules verified)"
    );

    Ok(ExecPlanReport {
        warnings: Vec::new(),
        notices,
    })
}

/// Linux：NoNewPrivs → Landlock（含 fs allow 四表）→ Seccomp（含放宽）。
///
/// 顺序与 [`apply_to_self_linux`] 相同且不可换：NoNewPrivs 必须最先（否则
/// non-root 加载 seccomp filter 会 EPERM），Seccomp 必须最后（它一生效就改不动
/// Landlock 了）。
#[cfg(target_os = "linux")]
fn apply_plan_to_self_linux(
    plan: &crate::exec_config::SandboxExecPlan,
    resolved: &crate::exec_rules::ResolvedFsRules,
    mut notices: Vec<String>,
) -> Result<ExecPlanReport, SandboxError> {
    use crate::linux::{landlock, seccomp};

    // SAFETY: prctl 是 pure syscall，无内存生命周期要求。
    let prctl_rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_i32, 0_i32, 0_i32, 0_i32) };
    if prctl_rc != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(SandboxError::InvalidConfig {
            message: format!("prctl(PR_SET_NO_NEW_PRIVS): {errno}"),
        });
    }

    let extra: Vec<landlock::ExtraGrant> = resolved
        .allow_read
        .iter()
        .map(|r| landlock::ExtraGrant {
            path: r.path.clone(),
            write: false,
            source: r.source.clone(),
        })
        .chain(resolved.allow_write.iter().map(|r| landlock::ExtraGrant {
            path: r.path.clone(),
            write: true,
            source: r.source.clone(),
        }))
        .collect();

    // 出口锁（PR-8）：非零代理端口 ⇒ ruleset 额外管辖 connect(2)-TCP，并且只
    // 放行这一个端口。`0` ⇒ `None` ⇒ 完全不声明网络管辖，行为逐字不变。
    let connect_tcp_port = if plan.network.http_proxy_port > 0 {
        Some(plan.network.http_proxy_port)
    } else {
        None
    };

    let outcome = landlock::apply_landlock_rules_with_grants(&plan.base, &extra, connect_tcp_port)?;

    // ── UNENFORCEABLE(linux)：deny 表在已放行的子树里挖不了洞 ──────────────
    //
    // 证据与替代方案的否决理由见 `landlock::apply_landlock_rules_with_grants`
    // 的文档（Landlock 是纯 allow-grant 模型；空权限集会被内核拒绝）。
    // 与 allow **不重叠**的 deny 由 Landlock 的默认拒绝天然兑现，不报；重叠的
    // 兑现不了，逐条留下名字。
    for (rules, table) in [
        (&resolved.deny_write, "filesystem.denyWrite"),
        (&resolved.deny_read, "filesystem.denyRead"),
    ] {
        for rule in rules {
            if let Some(grant) =
                crate::exec_rules::shadowing_grant(&rule.path, &outcome.granted_roots)
            {
                notices.push(format!(
                    "UNENFORCEABLE(linux): {table} `{}` overlaps the granted subtree `{}` — \
                     Landlock is a pure allow-grant model and cannot carve a hole inside a \
                     granted hierarchy",
                    rule.source,
                    grant.display()
                ));
            }
        }
    }

    if !plan.network.allow_unix_sockets.is_empty() && !plan.network.allow_all_unix_sockets {
        // 方向是「比承诺更严」（全拦），不是安全缺口 —— 但仍要留下名字，
        // 否则用户只会看到一个没有解释的 EPERM。
        notices.push(
            "network.allowUnixSockets is enforced as an all-or-nothing switch on Linux: \
             seccomp filters `socket(AF_UNIX)` by domain and cannot read the socket path, \
             so every unix socket is blocked"
                .to_string(),
        );
    }

    seccomp::apply_seccomp_filter_with_relaxations(
        &plan.base,
        seccomp::SeccompRelaxations {
            nested_sandbox: plan.weaker.nested_sandbox,
            all_unix_sockets: plan.network.allow_all_unix_sockets,
        },
    )?;

    // ── 出口锁的运行期实证（PR-8）─────────────────────────────────────────
    //
    // 必须在 seccomp 之后：探针要走的 `socket()`/`connect()`/`close()` 全都得
    // 在最终那套 filter 下也能跑，早问一步问到的就不是最终状态。老内核
    // （ABI < V4）上 landlock 的 best-effort 会静默丢掉网络那一档 —— 这里就是
    // 那个「静默」被抓住的地方。`?` 直通既有的 125 路径。
    if plan.network.http_proxy_port > 0 {
        verify_egress_locked_to_proxy(plan.network.http_proxy_port)?;
        notices.push(format!(
            "egress lock verified against proxy port {}: external TCP is denied by the kernel \
             and the proxy is reachable",
            plan.network.http_proxy_port
        ));
    }

    info!(
        security_level = ?plan.base.security_level,
        workspace = %plan.base.workspace.display(),
        granted = outcome.granted_roots.len(),
        proxy_port = plan.network.http_proxy_port,
        "linux exec-plan self-sandbox applied (NoNewPrivs + Landlock + Seccomp, irreversible)"
    );

    Ok(ExecPlanReport {
        warnings: outcome.warnings,
        notices,
    })
}

// ── Plan-level child execution (Windows, W-SANDBOX-ENFORCED-DEADCODE PR-3)──

/// Result of relaying one command through the Windows sandbox.
#[cfg(target_os = "windows")]
pub struct ExecPlanChildOutcome {
    /// The child's exit code, verbatim.
    pub exit_code: i32,
    pub report: ExecPlanReport,
}

/// Run a plan's command as a sandboxed **child** and wait for it.
///
/// The Windows counterpart of [`apply_exec_plan_to_self`], and a different shape
/// on purpose: Windows cannot restrict a process after it has started, so there
/// is no "apply to self" to perform. The helper stays alive as a relay instead
/// (see [`crate::windows::exec`] for why that is invisible to the host).
///
/// # Single-threaded only
///
/// This mutates the process environment (`TMP`/`TEMP`) so the child inherits the
/// sandbox's temp directory, which is only sound while no other thread is
/// running. Its one caller is `crabcode sandbox-exec`, a helper process that has
/// started no threads by this point. **Do not call it from a server or a
/// runtime.**
///
/// # Errors
///
/// Returns [`SandboxError`] if the plan is invalid or any isolation step fails.
/// The caller must translate that into the 125 protocol — never into a silent
/// unsandboxed re-run.
#[cfg(target_os = "windows")]
pub fn run_exec_plan_as_child(
    plan: &crate::exec_config::SandboxExecPlan,
) -> Result<ExecPlanChildOutcome, SandboxError> {
    use crate::windows::exec::{ChildSpec, ChildStdio, run_child};

    let (resolved, mut notices) = resolve_plan_and_notices(plan)?;

    // ── UNENFORCEABLE(windows): the filesystem tables ────────────────────
    //
    // This backend isolates by token and job object. It has no path filter —
    // there is no Landlock/Seatbelt equivalent wired here — so every fs rule is
    // carried, validated, and then *not applied*. Saying so is the whole point:
    // an allow list that is silently ignored reads as "the sandbox confines the
    // command to these paths" while the command can still reach everything.
    //
    // Deny rules get named individually (they are the explicit "must not touch"
    // list, and forensics needs the names); the allow tables are summarised by
    // count because their failure mode is structural, not per-entry.
    let allow_rules = resolved.allow_read.len() + resolved.allow_write.len();
    if allow_rules > 0 {
        notices.push(format!(
            "UNENFORCEABLE(windows): filesystem.allowRead/allowWrite ({allow_rules} rules) \
             are not applied — this backend restricts the token and the job object, not \
             filesystem paths, so nothing narrows the command to these locations"
        ));
    }
    for (rules, table) in [
        (&resolved.deny_write, "filesystem.denyWrite"),
        (&resolved.deny_read, "filesystem.denyRead"),
    ] {
        for rule in rules {
            notices.push(format!(
                "UNENFORCEABLE(windows): {table} `{}` is not applied — this backend has no \
                 path-level filter",
                rule.source
            ));
        }
    }

    // Network: same story. `base.network_policy` reaches the Windows runner but
    // nothing there acts on it, and the domain-level enforcement everyone
    // actually wants needs the filtering proxy (PR-8).
    notices.push(format!(
        "UNENFORCEABLE(windows): network policy `{:?}` is not applied — the Windows backend \
         has no network filter; domain-level enforcement arrives with the filtering proxy",
        plan.network.policy
    ));

    // TMPDIR's Windows spelling. The Unix lane sets TMPDIR because the sandbox
    // rules read it; here nothing reads it, but the temp directory the policy
    // layer picked (and put in allowWrite) should still be the one the command
    // uses, or the two lanes quietly disagree about where scratch files live.
    // SAFETY: single-threaded — the helper has started no threads at this point.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("TMP", &plan.tmp_dir);
        std::env::set_var("TEMP", &plan.tmp_dir);
    }

    let exit_code = run_child(&ChildSpec {
        workspace: &plan.base.workspace,
        security_level: plan.base.security_level,
        program: &plan.base.command,
        args: &plan.base.args,
        stdio: ChildStdio::Inherit,
        // No ceiling here: the host owns the command's lifetime (see `ChildSpec`).
        timeout: None,
    })?;

    info!(
        security_level = ?plan.base.security_level,
        workspace = %plan.base.workspace.display(),
        exit_code,
        "windows exec-plan child completed (restricted token + job object)"
    );

    Ok(ExecPlanChildOutcome {
        exit_code,
        report: ExecPlanReport {
            warnings: Vec::new(),
            notices,
        },
    })
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

#[cfg(test)]
mod exec_plan_validation_tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{NetworkPolicy, ResourceLimits, SecurityLevel};
    use crate::exec_config::{
        FidelityLevel, FidelityReport, FsRules, NetworkRules, SandboxExecPlan, WeakerFlags,
    };

    fn plan(workspace: &std::path::Path, filesystem: FsRules) -> SandboxExecPlan {
        SandboxExecPlan {
            base: SandboxConfig {
                security_level: SecurityLevel::L0Deny,
                command: "true".into(),
                args: Vec::new(),
                workspace: workspace.to_path_buf(),
                mounts: Vec::new(),
                resource_limits: ResourceLimits::default(),
                network_policy: Some(NetworkPolicy::None),
                env_vars: HashMap::new(),
                format: Default::default(),
                backend: Default::default(),
            },
            filesystem,
            network: NetworkRules {
                policy: NetworkPolicy::None,
                allowed_domains: Vec::new(),
                denied_domains: Vec::new(),
                allow_unix_sockets: Vec::new(),
                allow_all_unix_sockets: false,
                allow_local_binding: false,
                http_proxy_port: 0,
                socks_proxy_port: 0,
            },
            weaker: WeakerFlags {
                nested_sandbox: false,
                network_isolation: false,
            },
            tmp_dir: workspace.to_path_buf(),
            fidelity: FidelityReport {
                level: FidelityLevel::Full,
                unenforced: Vec::new(),
            },
        }
    }

    fn empty_fs_rules() -> FsRules {
        FsRules {
            allow_read: Vec::new(),
            allow_write: Vec::new(),
            deny_read: Vec::new(),
            deny_write: Vec::new(),
        }
    }

    #[test]
    fn unresolvable_deny_rules_fail_closed_before_platform_dispatch() {
        let workspace = tempfile::tempdir().expect("temporary workspace");

        for deny_read in [true, false] {
            let mut filesystem = empty_fs_rules();
            if deny_read {
                filesystem.deny_read.push("/secret/*.pem".into());
            } else {
                filesystem.deny_write.push("/secret/*.pem".into());
            }

            let error = resolve_plan_and_notices(&plan(workspace.path(), filesystem))
                .expect_err("an unresolvable deny must refuse execution");
            let rendered = error.to_string();
            assert!(rendered.contains("refusing to run"));
            assert!(rendered.contains(if deny_read {
                "filesystem.denyRead"
            } else {
                "filesystem.denyWrite"
            }));
        }
    }

    #[test]
    fn unresolvable_allow_rules_remain_visible_and_security_stricter() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let mut filesystem = empty_fs_rules();
        filesystem.allow_read.push("/optional/*.pem".into());
        filesystem.allow_write.push("/optional/*.tmp".into());

        let (resolved, notices) = resolve_plan_and_notices(&plan(workspace.path(), filesystem))
            .expect("an unresolvable allow may continue with narrower access");
        assert!(resolved.allow_read.is_empty());
        assert!(resolved.allow_write.is_empty());
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("filesystem.allowRead"))
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("filesystem.allowWrite"))
        );
        assert!(
            notices
                .iter()
                .all(|notice| notice.contains("access remains denied by default"))
        );
    }
}
