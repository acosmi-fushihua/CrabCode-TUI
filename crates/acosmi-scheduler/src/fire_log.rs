//! Cron 触发日志（持久化运行历史）
//!
//! daemon 触发 `SessionTarget::Continuation` 任务时，除了发出正常 `JobFiredEvent`，
//! 还往 `<dir>/cron/cron_fired.log` 追加一行 JSONL，作为 cron job 的运行历史。
//! `cron.runs` IPC 命令从此文件读末尾 N 行返回（见 `daemon.rs::handle_runs` /
//! `acosmi-cmd-cron/src/runs.rs`）。
//!
//! 设计要点：
//! - 原子追加：`O_APPEND` 单次 write，多进程/多任务并发不撕裂
//! - 滚动：单文件 >10MB 时 rename 为 `.1`，保留 3 份历史
//! - 非核心路径：写入失败仅记 error，不影响 daemon 主流程
//! - 仅 Continuation 任务写日志（Main-session fire 由活动 TS session 直接消费）
//!
//! 历史：早期版本（v3）此文件作为反向 bus 触发 Go gateway 的 fsnotify 监视，
//! 让 gateway 调用 `SessionContinuation.AppendAssistantMessage` 写回原会话；
//! Go gateway 在 M6 (2026-05-02) 整体下线后，本文件仅保留运行历史用途。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing;

use crate::task_store::{CronJob, SessionTarget};

/// 日志文件相对路径（与 `scheduled_tasks.json` 同目录）
// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const FIRE_LOG_REL: &str = "cron/cron_fired.log";

/// 单文件滚动阈值（10 MB）
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 保留历史文件数（`.1`、`.2`、`.3`）
const ROTATE_KEEP: usize = 3;

/// 一条触发日志
///
/// 字段与 Go `cronfire.FireLogEntry` 对齐，camelCase 序列化。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FireLogEntry {
    pub job_id: String,
    pub fire_ts_ms: i64,
    pub session_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// "chat" | "notify" | "both"
    pub continuation_kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl FireLogEntry {
    /// 从 `CronJob` 构造（仅当 `session_target` == Continuation 且 `session_key` 非空时有意义）
    #[must_use]
    pub fn from_job(job: &CronJob, fire_ts_ms: i64) -> Option<Self> {
        if job.session_target != SessionTarget::Continuation {
            return None;
        }
        let session_key = job.session_key.clone().unwrap_or_default();
        if session_key.is_empty() {
            return None;
        }
        // payload.message 优先；缺失/纯空白则回落到 text，与 outbox 投递
        // eligibility 及 cron add/update domain validator 保持一致。
        let message = job
            .payload
            .message
            .clone()
            .filter(|message| !message.trim().is_empty())
            .or_else(|| {
                job.payload
                    .text
                    .clone()
                    .filter(|text| !text.trim().is_empty())
            })?;
        let kind = job
            .continuation_kind
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "chat".to_string());
        Some(Self {
            job_id: job.id.clone(),
            fire_ts_ms,
            session_key,
            channel_id: job.channel_id.clone().filter(|s| !s.is_empty()),
            continuation_kind: kind,
            message,
            title: Some(job.name.clone()).filter(|s| !s.is_empty()),
        })
    }
}

/// 返回 fire log 绝对路径
#[must_use]
pub fn fire_log_path(dir: &Path) -> PathBuf {
    dir.join(FIRE_LOG_REL)
}

/// 追加一条日志到 fire log 文件（原子）
///
/// 失败仅 `tracing::error，不返回错误——调用方不应因此中断主流程`。
pub async fn append(dir: &Path, entry: &FireLogEntry) {
    if let Err(e) = append_inner(dir, entry).await {
        tracing::error!(
            error = %e,
            job_id = %entry.job_id,
            "cron_fire: 写触发日志失败"
        );
    }
}

async fn append_inner(dir: &Path, entry: &FireLogEntry) -> std::io::Result<()> {
    let path = fire_log_path(dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    // 滚动前的大小检查（best-effort；并发场景允许略微超过阈值）
    if let Ok(meta) = fs::metadata(&path).await
        && meta.len() >= ROTATE_BYTES
    {
        rotate(&path).await;
    }

    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');

    // 文件权限 0600：日志含 sessionKey + message 正文，限制为所有者可读写。
    // Windows 侧 mode 被忽略，由文件系统 ACL 控制（与本仓既有 cronstore 一致）。
    let mut open_opts = OpenOptions::new();
    open_opts.create(true).append(true);
    // tokio::fs::OpenOptions 在 unix 下提供同名 .mode() 方法，无需引入 std 的 trait。
    #[cfg(unix)]
    {
        open_opts.mode(0o600);
    }
    let mut f = open_opts.open(&path).await?;
    f.write_all(line.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

/// 简单滚动：a.log → a.log.1 → a.log.2 → a.log.3（丢弃更老的）
async fn rotate(path: &Path) {
    // 从高到低 rename，避免覆盖
    for i in (1..ROTATE_KEEP).rev() {
        let from = path_with_ext_num(path, i);
        let to = path_with_ext_num(path, i + 1);
        let _ = fs::rename(&from, &to).await;
    }
    let first = path_with_ext_num(path, 1);
    if let Err(e) = fs::rename(path, &first).await {
        tracing::warn!(error = %e, "cron_fire: rotate 失败");
    }
}

fn path_with_ext_num(path: &Path, n: usize) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(format!(".{n}"));
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::{
        CronJobState, CronPayload, CronSchedule, PayloadKind, ScheduleKind, WakeMode,
    };

    fn continuation_job(id: &str, session_key: &str, message: &str) -> CronJob {
        CronJob {
            id: id.into(),
            agent_id: None,
            name: "test-job".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Continuation,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some(message.into()),
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: Some(session_key.into()),
            channel_id: Some("chat-xyz".into()),
            continuation_kind: Some("chat".into()),
        }
    }

    #[test]
    fn from_job_returns_none_for_non_continuation() {
        let mut job = continuation_job("j1", "feishu:x", "hello");
        job.session_target = SessionTarget::Main;
        assert!(FireLogEntry::from_job(&job, 1).is_none());
    }

    #[test]
    fn from_job_returns_none_when_session_key_empty() {
        let mut job = continuation_job("j1", "feishu:x", "hello");
        job.session_key = Some("".into());
        assert!(FireLogEntry::from_job(&job, 1).is_none());
    }

    #[test]
    fn from_job_returns_none_when_message_empty() {
        let mut job = continuation_job("j1", "feishu:x", "hello");
        job.payload.message = None;
        job.payload.text = None;
        assert!(FireLogEntry::from_job(&job, 1).is_none());
    }

    #[test]
    fn from_job_blank_message_falls_back_to_non_blank_text() {
        let mut job = continuation_job("j1", "feishu:x", "   ");
        job.payload.text = Some("fallback text".into());

        let entry = FireLogEntry::from_job(&job, 1).expect("text fallback should be retained");

        assert_eq!(entry.message, "fallback text");
    }

    #[test]
    fn from_job_returns_none_when_message_and_text_are_blank() {
        let mut job = continuation_job("j1", "feishu:x", "   ");
        job.payload.text = Some("\t\n".into());

        assert!(FireLogEntry::from_job(&job, 1).is_none());
    }

    #[test]
    fn from_job_defaults_kind_to_chat() {
        let mut job = continuation_job("j1", "feishu:x", "hi");
        job.continuation_kind = None;
        let e = FireLogEntry::from_job(&job, 99).expect("should build");
        assert_eq!(e.continuation_kind, "chat");
        assert_eq!(e.fire_ts_ms, 99);
    }

    #[tokio::test]
    async fn append_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let entry = FireLogEntry {
            job_id: "j-1".into(),
            fire_ts_ms: 1700000000000,
            session_key: "feishu:abc".into(),
            channel_id: Some("c1".into()),
            continuation_kind: "chat".into(),
            message: "提醒开会".into(),
            title: Some("meeting".into()),
        };
        append(dir.path(), &entry).await;

        let content = fs::read_to_string(fire_log_path(dir.path())).await.unwrap();
        assert!(content.ends_with('\n'));
        let lines: Vec<&str> = content.trim().split('\n').collect();
        assert_eq!(lines.len(), 1);
        let back: FireLogEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back, entry);
    }

    #[tokio::test]
    async fn concurrent_writes_no_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();

        let mut handles = vec![];
        for i in 0..50 {
            let p = dir_path.clone();
            handles.push(tokio::spawn(async move {
                let entry = FireLogEntry {
                    job_id: format!("j-{i}"),
                    fire_ts_ms: 1000 + i,
                    session_key: "s".into(),
                    channel_id: None,
                    continuation_kind: "chat".into(),
                    message: format!("msg {i}"),
                    title: None,
                };
                append(&p, &entry).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let content = fs::read_to_string(fire_log_path(dir.path())).await.unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();
        assert_eq!(lines.len(), 50);
        for l in lines {
            let _: FireLogEntry =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("corrupted line {l:?}: {e}"));
        }
    }

    #[tokio::test]
    async fn rotate_moves_to_dot_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = fire_log_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        // 预写 > 阈值的文件
        let big = vec![b'x'; (ROTATE_BYTES + 16) as usize];
        fs::write(&path, &big).await.unwrap();

        // 追加一条，应触发滚动
        let entry = FireLogEntry {
            job_id: "after-rotate".into(),
            fire_ts_ms: 1,
            session_key: "s".into(),
            channel_id: None,
            continuation_kind: "chat".into(),
            message: "m".into(),
            title: None,
        };
        append(dir.path(), &entry).await;

        let rotated = path_with_ext_num(&path, 1);
        assert!(fs::metadata(&rotated).await.is_ok(), "应产生 .1 文件");
        let new_content = fs::read_to_string(&path).await.unwrap();
        assert!(new_content.contains("after-rotate"));
    }
}
