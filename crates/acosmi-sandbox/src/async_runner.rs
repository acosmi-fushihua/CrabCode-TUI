//! Async sandbox runner — Sprint 7 阶段 3 新增（2026-04-23）
//!
//! # 背景
//!
//! 既有 [`SandboxRunner`](crate::SandboxRunner) trait 的 `run()` 方法是
//! **同步 + 一次性**：调用方传 `SandboxConfig`，拿到聚合后的 `SandboxOutput`
//! （含完整 stdout/stderr + `exit_code）。这个契约服务于` `acosmi sandbox run`
//! CLI 一次性命令、`acosmi-coder/tools/bash.rs` 的 bash 工具、既有 bench/test。
//!
//! 但 supervisor 的 `handle_spawn_managed`（`crates/acosmi-supervisor/src/ipc.rs`）
//! 需要**异步 + 流式**接口：立即返 `process_id`，stdout/stderr 逐行以 notification
//! 回传，进程结束时发 exec.completed。直接调 sync `run()` 会在 tokio runtime
//! worker thread 上阻塞若干秒（Windows 下 2-6s／macOS 50-70ms），严重破坏
//! async 并发语义。
//!
//! # 设计
//!
//! 新增 `AsyncSandboxRunner` trait 和 `SandboxedProcess` 结构，**跟 sync 路径
//! 并存**：
//!
//! - 既有 sync `SandboxRunner::run()` 保留不动；所有旧 caller（CLI / bash tool /
//!   既有 bench）不受影响
//! - 新 `AsyncSandboxRunner::spawn()` 返回 `SandboxedProcess`，提供异步 stdout/
//!   stderr `AsyncRead` + 延迟 exit code `Future` + abort closure
//! - `SandboxedProcess._guards` 字段持有 Token/ACL/Job Object 等 Windows 专属
//!   RAII 句柄，保证在进程 wait 完成前不被释放
//!
//! # 平台实装计划
//!
//! | 平台 | 实装方式 | 状态（Sprint 7 阶段 3 / 4）|
//! |---|---|---|
//! | Linux | `tokio::process::Command::pre_exec` 直接 async，复用 `apply_landlock_rules` + `apply_seccomp_filter` | 阶段 3 完整实装 |
//! | macOS | `tokio::process::Command::pre_exec` + seatbelt FFI.apply | 阶段 3 完整实装 |
//! | Windows | 因 `STATUS_DLL_INIT_FAILED` 根因未修（见 Round 3 独立立项），暂返 `SandboxError::PlatformNotSupported`；阶段 4 opt-in 默认 None 规避 | 阶段 3 stub + Round 3 修 DLL 后完整实装 |
//! | Docker | 阶段 3 stub 返 Unsupported；阶段 6 按需补齐 | 延后 |
//!
//! # 使用示例
//!
//! ```rust,ignore
//! let runner = acosmi_sandbox::select_async_runner(&config)?;
//! let proc = runner.spawn(&config).await?;
//! let pid = proc.pid;
//! // 启动后台任务读 stdout ...
//! let code = proc.wait.await?;
//! ```

use std::pin::Pin;

use crate::config::SandboxConfig;
use crate::error::SandboxError;

/// 用于 async 返回的 boxed Future，避免 GAT / `async_trait` 依赖
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 异步沙箱 spawn 接口
///
/// 跟 [`crate::SandboxRunner`] sync `run()` trait 并存；调用方按需选择：
/// - 一次性命令（CLI、acosmi-coder/bash）：用 sync `run()`
/// - 长运行 + 流式 progress（supervisor `handle_spawn_managed）：用` async `spawn()`
pub trait AsyncSandboxRunner: Send + Sync {
    /// Human-readable name of this backend（与 [`crate::SandboxRunner::name`] 保持一致）
    fn name(&self) -> &'static str;

    /// Returns `true` if this backend's prerequisites are met on the current
    /// system **and** the async path is functional (Windows 下 `false`，因
    /// `STATUS_DLL_INIT_FAILED` 根因未修完；需 Round 3 独立立项后放开)。
    fn available(&self) -> bool;

    /// Spawn a sandboxed process and return async handles.
    ///
    /// 生命周期约束：调用方持有 [`SandboxedProcess`] 期间沙箱 RAII 守护依旧有效。
    /// 进程 exit 之后必须 drop `SandboxedProcess` 让 guards 清理（Token、ACL、Job
    /// Object handles）。
    fn spawn<'a>(
        &'a self,
        config: &'a SandboxConfig,
    ) -> BoxFuture<'a, Result<SandboxedProcess, SandboxError>>;
}

/// Spawn 沙箱进程的 async handle 集合
///
/// # 字段说明
///
/// - `pid`：子进程 PID，用于外部 diagnostic / `abort_token` 关联
/// - `stdout` / `stderr`：`AsyncRead` 管道（`tokio::process::ChildStdout` / `ChildStderr`
///   / Windows 下 `HandleAsyncRead` 包装），调用方自己决定如何消费（逐行 / 缓冲）
/// - `wait`：延迟 exit code；resolve 时进程已退出
/// - `abort`：提前终止进程（SIGKILL / TerminateProcess）；调用方持有 [`SandboxedProcess`]
///   才能调用；abort 之后 `wait` 会 resolve
/// - `sandbox_backend`：实际使用的 backend 名（如 `"linux-landlock+seccomp"`），
///   便于 supervisor 回传给 UI/模型让用户看到真实沙箱状态
/// - `_guards`：平台专属 RAII 句柄（Windows Token/ACL/Job，Linux cgroup guard）
///
/// # 内存语义
///
/// `SandboxedProcess` 不可 `Clone`（持有独占资源）。Drop 会自动 kill 进程
/// （Windows Job Object `KILL_ON_JOB_CLOSE，Unix` `kill_on_drop`），
/// 然后释放 `_guards`。
pub struct SandboxedProcess {
    /// 子进程 PID
    pub pid: u32,
    /// 异步 stdout 句柄。调用方从中逐行读取。
    pub stdout: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
    /// 异步 stderr 句柄。调用方从中逐行读取。
    pub stderr: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
    /// 实际使用的沙箱后端名（debug / 回报用）
    pub sandbox_backend: String,
    /// 进程退出 future；resolve 时 `Ok(exit_code)` 或 `Err(SandboxError)`
    pub wait: BoxFuture<'static, Result<i32, SandboxError>>,
    /// 终止子进程的 closure
    pub abort: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>,
    /// 平台专属 RAII guards — 保持到 `SandboxedProcess` drop。
    /// Linux/macOS 上通常是 `()`；Windows 上持有 `RestrictedToken` / `AclGuard` / `JobGuard`。
    pub _guards: Box<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for SandboxedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxedProcess")
            .field("pid", &self.pid)
            .field("sandbox_backend", &self.sandbox_backend)
            .field("has_stdout", &self.stdout.is_some())
            .field("has_stderr", &self.stderr.is_some())
            .finish_non_exhaustive()
    }
}

/// Select the best available async sandbox runner for the current platform.
///
/// Sprint 7 阶段 3：目前只有 Linux / macOS native 路径真实实装；Windows 返
/// `PlatformNotSupported` 让 supervisor 的 opt-in 默认 None 分支规避；Docker
/// stub 延后。
///
/// 调用方通常走 opt-in 流程，所以 Windows 上即便此函数返 Err，
/// `handle_spawn_managed` 回退 direct `ProcessBuilder` spawn 也能继续工作。
pub fn select_async_runner(
    config: &SandboxConfig,
) -> Result<Box<dyn AsyncSandboxRunner>, SandboxError> {
    // P1-3 (2026-06-05): Validate config BEFORE selecting a runner, mirroring
    // the sync `select_runner` (platform.rs). The supervisor's enforced spawn
    // path (`handle_spawn_managed` → select_async_runner) previously entered
    // UNCHECKED, letting relative/`..` cwd and LD_PRELOAD/DYLD_INSERT_LIBRARIES
    // env reach the enforced path. This is the single gate for the async/
    // enforced path; the self-sandbox path (`apply_sandbox_to_self`) intentionally
    // skips full validate (empty command) and is NOT touched.
    config.validate()?;

    #[cfg(target_os = "linux")]
    {
        if let Some(runner) = crate::linux::async_runner::try_select(config)? {
            return Ok(runner);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(runner) = crate::macos::async_runner::try_select(config)? {
            return Ok(runner);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(runner) = crate::windows::async_runner::try_select(config)? {
            return Ok(runner);
        }
    }
    let _ = config;
    Err(SandboxError::PlatformNotSupported {
        platform: std::env::consts::OS.into(),
        reason: "async sandbox runner not yet implemented on this platform; see Sprint 7 阶段 3 跟踪文档".into(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{
        BackendPreference, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn valid_config() -> SandboxConfig {
        SandboxConfig {
            security_level: SecurityLevel::L1Allowlist,
            command: "echo".into(),
            args: vec!["hi".into()],
            workspace: std::env::temp_dir(),
            mounts: vec![],
            resource_limits: ResourceLimits::default(),
            network_policy: None,
            env_vars: HashMap::new(),
            format: OutputFormat::Json,
            backend: BackendPreference::Auto,
        }
    }

    /// P1-3: the enforced/async path must reject a relative cwd BEFORE any
    /// runner selection (validate gate at select_async_runner entry).
    /// Helper: assert select_async_runner rejected the config with a matching
    /// error variant, without requiring `Box<dyn AsyncSandboxRunner>: Debug`.
    fn assert_path_error(c: &SandboxConfig, what: &str) {
        match select_async_runner(c) {
            Err(SandboxError::PathError { .. }) => {}
            Err(other) => panic!("{what}: expected PathError, got {other:?}"),
            Ok(_) => panic!("{what}: expected PathError, but a runner was selected"),
        }
    }

    fn assert_invalid_config(c: &SandboxConfig, what: &str) {
        match select_async_runner(c) {
            Err(SandboxError::InvalidConfig { .. }) => {}
            Err(other) => panic!("{what}: expected InvalidConfig, got {other:?}"),
            Ok(_) => panic!("{what}: expected InvalidConfig, but a runner was selected"),
        }
    }

    #[test]
    fn async_relative_workspace_rejected() {
        let mut c = valid_config();
        c.workspace = PathBuf::from("relative/path");
        assert_path_error(&c, "relative cwd");
    }

    /// P1-3: workspace containing `..` traversal rejected on the async path.
    #[test]
    fn async_traversal_workspace_rejected() {
        let mut c = valid_config();
        c.workspace = PathBuf::from("/tmp/../etc/passwd");
        assert_path_error(&c, "traversal cwd");
    }

    /// P1-3: LD_PRELOAD in env rejected on the async path.
    #[test]
    fn async_ld_preload_rejected() {
        let mut c = valid_config();
        c.env_vars
            .insert("LD_PRELOAD".into(), "/tmp/evil.so".into());
        assert_invalid_config(&c, "LD_PRELOAD");
    }

    /// P1-3: DYLD_INSERT_LIBRARIES in env rejected on the async path.
    #[test]
    fn async_dyld_insert_rejected() {
        let mut c = valid_config();
        c.env_vars
            .insert("DYLD_INSERT_LIBRARIES".into(), "/tmp/evil.dylib".into());
        assert_invalid_config(&c, "DYLD_INSERT_LIBRARIES");
    }

    /// P1-3 anti-over-rejection: a valid config must still pass the validate
    /// gate. On a platform without a real async runner it returns
    /// `PlatformNotSupported` (NOT a validate error) — proving the gate let it
    /// through to selection. On Linux/macOS it may select a runner or report a
    /// platform-capability error, but NEVER an InvalidConfig/PathError.
    #[test]
    fn async_valid_config_passes_validate_gate() {
        let c = valid_config();
        match select_async_runner(&c) {
            Ok(_) => {}
            Err(SandboxError::InvalidConfig { .. } | SandboxError::PathError { .. }) => {
                panic!("valid config must not be rejected by the validate gate")
            }
            // PlatformNotSupported / capability errors are fine — they prove the
            // config passed validate and reached runner selection.
            Err(_) => {}
        }
    }
}
