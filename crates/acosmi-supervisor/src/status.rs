//! Supervisor 运行时状态快照 —— 供 Go 通过 UDS `supervisor.status` RPC 查询。
//!
//! 设计原则：
//! - 不依赖 heartbeat.json 文件；launchd 是
//!   权威活性源，本结构只反映 supervisor 进程视角的内部状态快照。
//! - Snapshot 由主循环（`run_with_config`）每 watchdog tick 刷新一次；读端通过
//!   `Arc<RwLock<SupervisorStatus>>` 共享，`read()` 非阻塞快照。
//! - Serde 结构与 Go `backend/internal/infra/supervisor_client.go`（P1.2）的
//!   反序列化 struct `一一对齐；schema_version=1` 时字段只增不删。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// 共享状态别名。Arc 在 IPC 层与主循环间透传。
pub type SharedStatus = Arc<RwLock<SupervisorStatus>>;

/// Supervisor 当前状态的完整快照。
///
/// 2026-04-28 阶段 3-B：`scheduler` 字段已移除（立宪 3：cron 不再跟随 supervisor，
/// 改由独立 `crabcode-cron` 二进制承担；调度状态由 hub 暴露）。
/// `schema_version` 升至 2 标记契约破坏。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorStatus {
    pub schema_version: u32,
    /// 本次快照生成的墙钟时间（毫秒）。读端判断数据新鲜度用。
    pub snapshot_at_ms: u64,
    pub supervisor: SupervisorInfo,
    pub children: BTreeMap<String, ChildStatus>,
}

/// Supervisor 自身元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorInfo {
    pub pid: u32,
    pub started_at_ms: u64,
    pub uptime_s: u64,
    pub state_dir: String,
    pub version: String,
}

/// 单个子进程当前状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildStatus {
    /// "starting" | "alive" | "stopping" | "stopped" | "failed"
    pub state: String,
    pub pid: Option<u32>,
    pub uptime_s: Option<u64>,
    pub restart_count: u32,
}

impl SupervisorStatus {
    /// 构造初始快照。supervisor 启动时调用一次，后续由主循环增量更新。
    #[must_use]
    pub fn new_initial(state_dir: String) -> Self {
        let now_ms = current_unix_ms();
        Self {
            schema_version: 2,
            snapshot_at_ms: now_ms,
            supervisor: SupervisorInfo {
                pid: std::process::id(),
                started_at_ms: now_ms,
                uptime_s: 0,
                state_dir,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            children: BTreeMap::new(),
        }
    }

    /// 刷新 `snapshot_at_ms` 与 `supervisor.uptime_s。每次` watchdog tick 调用一次即可。
    pub fn refresh_timestamp(&mut self) {
        let now_ms = current_unix_ms();
        self.snapshot_at_ms = now_ms;
        self.supervisor.uptime_s = now_ms.saturating_sub(self.supervisor.started_at_ms) / 1000;
    }

    /// 全量覆写 children；由主循环扫描 `supervisor.processes` 后批量替换。
    pub fn replace_children(&mut self, children: BTreeMap<String, ChildStatus>) {
        self.children = children;
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_snapshot_has_valid_fields() {
        let s = SupervisorStatus::new_initial("/tmp/xyz".to_string());
        assert_eq!(s.schema_version, 2);
        assert!(s.snapshot_at_ms > 0);
        assert_eq!(s.supervisor.pid, std::process::id());
        assert_eq!(s.supervisor.state_dir, "/tmp/xyz");
        assert_eq!(s.supervisor.uptime_s, 0);
        assert!(s.children.is_empty());
    }

    #[test]
    fn refresh_updates_uptime_monotonic() {
        let mut s = SupervisorStatus::new_initial("/tmp".to_string());
        let first_ms = s.snapshot_at_ms;
        // 人工回退 started_at_ms 模拟已经运行 3 秒
        s.supervisor.started_at_ms = first_ms.saturating_sub(3_500);
        s.refresh_timestamp();
        assert!(s.supervisor.uptime_s >= 3);
    }

    #[test]
    fn schema_round_trip_json() {
        let s = SupervisorStatus::new_initial("/home/u/.crabcode".to_string());
        let j = serde_json::to_string(&s).expect("serialize");
        let back: SupervisorStatus = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(back.schema_version, s.schema_version);
        assert_eq!(back.supervisor.state_dir, s.supervisor.state_dir);
    }
}
