//! OS-native sandbox runtime for Acosmi.
//!
//! Provides platform-specific process isolation with automatic degradation:
//!
//! | Platform | Primary Backend | Fallback |
//! |----------|----------------|----------|
//! | Linux    | Landlock + Seccomp (+ Namespaces) | Docker |
//! | macOS    | Seatbelt FFI (`sandbox_init_with_parameters`) | Docker |
//! | Windows  | Restricted Token + Job Object | Docker |
//!
//! # Usage
//!
//! ```rust,no_run
//! use acosmi_sandbox::config::{SandboxConfig, SecurityLevel};
//! use acosmi_sandbox::select_runner;
//!
//! # fn example(config: SandboxConfig) -> anyhow::Result<()> {
//! let runner = select_runner(&config)?;
//! let output = runner.run(&config)?;
//! println!("{}", serde_json::to_string(&output)?);
//! # Ok(())
//! # }
//! ```
//!
//! # Safety
//!
//! This crate overrides the workspace `unsafe_code = "forbid"` lint because it
//! requires `unsafe` for platform FFI calls:
//! - Linux: `libseccomp` BPF filter installation
//! - macOS: `sandbox_init_with_parameters` via `extern "C"`
//! - Windows: Win32 API calls via the `windows` crate
//!
//! All `unsafe` blocks include mandatory `// SAFETY:` comments and are subject
//! to Skill 4 line-level audit.
#![allow(unsafe_code)]

pub mod config;
pub mod error;
// W-SANDBOX-ENFORCED-DEADCODE PR-1：`crabcode sandbox-exec` 配置文件（v1）的
// 解析与映射。三平台同构编译——解析/校验必须在每个平台都跑得起来，执行侧才按
// 平台分（Unix = PR-2，Windows = PR-3）。
pub mod exec_config;
// W-SANDBOX-ENFORCED-DEADCODE PR-2：fs 模式表 → 内核路径规则的归一化与重叠
// 判据。三平台同构编译且**刻意平台无关**——同一份语义写三遍只会有一份被测到。
pub mod exec_rules;
pub mod output;
pub mod platform;
pub mod worker;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

pub mod docker;

// Sprint 7 阶段 3（2026-04-23）：async spawn API 公共模块
pub mod async_runner;

use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::output::SandboxOutput;

pub use async_runner::{AsyncSandboxRunner, SandboxedProcess, select_async_runner};

/// Core sandbox execution trait.
///
/// Each platform backend implements this trait. The [`platform`] module
/// selects the best available backend based on runtime capability detection.
pub trait SandboxRunner: Send + Sync {
    /// Human-readable name of this backend (e.g., `"linux-landlock+seccomp"`).
    fn name(&self) -> &'static str;

    /// Returns `true` if this backend's prerequisites are met on the current system.
    fn available(&self) -> bool;

    /// Execute a command within the sandbox.
    ///
    /// Returns a [`SandboxOutput`] with captured stdout/stderr, exit code, and timing.
    /// The output is serialized to JSON for the Go orchestration layer.
    fn run(&self, config: &SandboxConfig) -> Result<SandboxOutput, SandboxError>;
}

/// Select the best available sandbox runner for the current platform and config.
///
/// Follows the degradation chain per platform (see module-level docs).
/// With [`BackendPreference::Auto`](config::BackendPreference::Auto) (default),
/// tries native first, then Docker fallback.
///
/// # Errors
///
/// Returns [`SandboxError::NoBackendAvailable`] if no backend (native or Docker) is usable.
pub fn select_runner(config: &SandboxConfig) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    platform::select_runner(config)
}

/// Apply sandbox constraints to the current process (self-sandboxing).
///
/// Irreversible. Used by the persistent Worker to sandbox itself when started
/// directly by the Go bridge (bypassing the launcher's `pre_exec` path).
///
/// See [`platform::apply_sandbox_to_self`] for details.
pub fn apply_sandbox_to_self(config: &SandboxConfig) -> Result<(), SandboxError> {
    platform::apply_sandbox_to_self(config)
}

/// 按 [`SandboxExecPlan`](crate::exec_config::SandboxExecPlan) 自沙箱当前进程。
///
/// 与 [`apply_sandbox_to_self`] 的区别就是本立项的全部理由：后者只吃
/// [`SandboxConfig`]，而 `SandboxConfig` **表达不了 fs 四表**——事故里丢掉的
/// 正是那批规则（SoT §1.5）。
///
/// 见 [`platform::apply_exec_plan_to_self`]。
pub fn apply_exec_plan_to_self(
    plan: &crate::exec_config::SandboxExecPlan,
) -> Result<platform::ExecPlanReport, SandboxError> {
    platform::apply_exec_plan_to_self(plan)
}

/// Windows：按 [`SandboxExecPlan`](crate::exec_config::SandboxExecPlan) 起一个
/// 受沙箱约束的**子进程**并等它退出。
///
/// Windows 没有 `exec`，也无法在进程启动后收紧自己的令牌，所以这条链不是
/// [`apply_exec_plan_to_self`] 的同形态，而是它的对偶：helper 活着当中继。
///
/// 见 [`platform::run_exec_plan_as_child`]。
///
/// # Errors
///
/// 隔离链任一环失败即错误；调用方必须走 125 协议，**绝不**静默裸跑。
#[cfg(target_os = "windows")]
pub fn run_exec_plan_as_child(
    plan: &crate::exec_config::SandboxExecPlan,
) -> Result<platform::ExecPlanChildOutcome, SandboxError> {
    platform::run_exec_plan_as_child(plan)
}

/// Canary: verify sandbox constraints are actually active on the current process.
///
/// ROOT-CAUSE FIX: prevent silent zero-isolation execution when launcher path
/// sets `OA_SANDBOX_APPLIED=1` but its platform-specific apply function is a
/// no-op stub. Worker must call this before running its event loop; on failure
/// it should abort rather than serve commands un-isolated.
///
/// See [`platform::verify_sandbox_active`] for per-platform detection.
pub fn verify_sandbox_active(
    security_level: crate::config::SecurityLevel,
) -> Result<(), SandboxError> {
    platform::verify_sandbox_active(security_level)
}
