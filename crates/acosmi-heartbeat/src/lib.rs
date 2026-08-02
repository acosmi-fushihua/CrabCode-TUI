//! `CrabCode` 心跳/Watchdog 模块（心监）
//!
//! 子模块：
//! - liveness: 统一存活判定状态机
//! - `task_watchdog`: 后台任务与 worker 健康检查
//!
//! `ProcessKind` 是心跳协议上 supervisor 用来区分子进程种类的标签
//! （TS Session 等）。真实的进程注册/查询单位在 supervisor 层是 `ProcessId`，
//! 但 IPC 线上协议只需要粗粒度的 TS/其它区分，所以此枚举留在协议层。
//! M6.1 后 Go 类型保留在协议层但已无活跃 producer。

pub mod liveness;
pub mod task_watchdog;

// 重导出核心类型
pub use liveness::{LivenessConfig, LivenessState, LivenessTracker};
pub use task_watchdog::{TaskHealth, TaskWatchdog, TaskWatchdogConfig, TaskWatchdogEvent};

use serde::{Deserialize, Serialize};

/// IPC 层心跳标识的进程种类。
///
/// 用于 supervisor IPC 信号 `IpcSignal::Heartbeat(kind)`，区分心跳来源种类。
/// supervisor 主循环再把它映射回 `ProcessId`。M6.1 后仅 `TypeScript` 是
/// 活跃 producer，`Go` 留作历史协议兼容标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessKind {
    /// TypeScript 子进程
    TypeScript,
    /// Go 子进程
    Go,
}

impl std::fmt::Display for ProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeScript => write!(f, "TypeScript"),
            Self::Go => write!(f, "Go"),
        }
    }
}
