//! `CrabCode` 进程执行层
//!
//! 子模块：
//! - `process`: 跨平台进程启动与管理
//! - `tree_kill`: 跨平台进程树终止
//! - `output`: stdout/stderr 输出捕获与管理
//!
//! 注：原 `sandbox` 子模块（Sprint 7 阶段 7 deprecated stub）于 R-Sandbox Phase 1B
//! （2026-05-06）整体下线，真后端在 `acosmi-sandbox` crate 的 `AsyncSandboxRunner`。

pub mod output;
pub mod process;
pub mod tree_kill;

// 重导出核心类型
pub use output::OutputCollector;
pub use process::{ManagedProcess, OutputConfig, ProcessBuilder, StdinConfig};
pub use tree_kill::{TreeKillOptions, TreeKillResult, kill_process_tree, kill_process_tree_force};
