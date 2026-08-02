//! 定时任务持久化存储
//!
//! 支持两种文件格式：
//! - v2（新）: `{ "version": 2, "jobs": [CronJob...] }` — 完整 `CronJob` 格式
//! - v1（旧）: `{ "tasks": [CronTask...] }` — 兼容 TS cronScheduler 7 字段格式
//!
//! 读取时自动检测格式并迁移；写入始终使用 v2 格式。
//! 路径: `<dir>/cron/scheduled_tasks.json`（`<dir>` 由调用方传入；生产唯一调用方
//! 是 crabcode-cron 守护，`<dir>` = `resolve_state_dir()` → `<state>/cron/…`，
//! W-CRON-DIR-UNIFY 归一后单层）。per-project 会话 lane 是 TS 侧
//! `<project>/.crabcode/`（`cronTasks.ts`），与本 Rust 常量无关、不受归一影响。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing;

// ============================================================================
// 旧格式类型（保留供反序列化兼容 + daemon 过渡期使用）
// ============================================================================

/// 旧版单个定时任务（与 TS `CronTask` 类型对齐，7 字段）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronTask {
    pub id: String,
    /// 5-field cron 表达式（本地时间）
    pub cron: String,
    /// 触发时入队的 prompt
    pub prompt: String,
    /// 创建时间（epoch ms）
    pub created_at: i64,
    /// 上次触发时间（epoch ms）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
    /// true = 周期任务（触发后重新调度而非删除）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurring: Option<bool>,
    /// true = 永久任务（豁免 auto-expiry）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
}

/// 旧版 JSON 文件顶层结构
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronFile {
    pub tasks: Vec<CronTask>,
}

// ============================================================================
// CronJob — 完整定时任务类型（对齐 Go CronJob 42 可达字段）
// ============================================================================

/// 存储文件版本号
pub const STORE_VERSION: i32 = 2;

/// 调度类型（对齐 Go `CronScheduleKind`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleKind {
    /// ISO-8601 一次性绝对时间
    At,
    /// 固定间隔 + 锚点
    Every,
    /// 5 字段 cron 表达式
    Cron,
}

/// 调度配置（对齐 Go `CronSchedule`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronSchedule {
    pub kind: ScheduleKind,
    /// kind=at: ISO-8601 时间字符串
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// kind=every: 间隔毫秒数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<i64>,
    /// kind=every: 锚点时间戳（毫秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_ms: Option<i64>,
    /// kind=cron: cron 表达式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// kind=cron: IANA 时区
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

/// 会话目标（对齐 Go `CronSessionTarget`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SessionTarget {
    #[default]
    Main,
    Isolated,
    /// Phase D: 写回原聊天会话（需配合 `session_key` / `channel_id` / `continuation_kind`）。
    /// 现路径：daemon 触发后写 `cron_fired.log` JSONL（`fire_log.rs` 仅对
    /// `Continuation` 目标 append），并发出 `JobFiredEvent`；TS 侧 REPL
    /// `useScheduledTasks` 通过 IPC `cron.poll_events` 收到事件，把 payload.message
    /// enqueue 到 lead 队列回写原会话。
    ///
    /// 历史：早期版本由 Go gateway 通过 HTTP 反向回调
    /// `SessionContinuation.AppendAssistantMessage` 把 payload 写回 session
    /// transcript + 广播 webchat + 可选远程频道投递；M6 (2026-05-02) Go 下线
    /// 后路径 1:1 收敛到 TS REPL 直接消费 IPC 事件，远程频道投递保留为后续工作。
    Continuation,
}

/// 唤醒模式（对齐 Go `CronWakeMode`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WakeMode {
    #[default]
    NextHeartbeat,
    Now,
}

/// 投递模式（对齐 Go `CronDeliveryMode`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
}

/// 投递配置（对齐 Go `CronDelivery`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronDelivery {
    pub mode: DeliveryMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_effort: Option<bool>,
}

/// 负载类型（对齐 Go `CronPayloadKind`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PayloadKind {
    #[default]
    SystemEvent,
    AgentTurn,
}

/// 任务负载（对齐 Go `CronPayload`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronPayload {
    pub kind: PayloadKind,
    /// kind=systemEvent: 系统事件文本
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// kind=agentTurn: Agent 消息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unsafe_external_content: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deliver: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_effort_deliver: Option<bool>,
}

/// 执行状态（对齐 Go `CronJobStatus`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JobStatus {
    Ok,
    Error,
    Skipped,
}

/// 运行时状态（对齐 Go `CronJobState`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CronJobState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<JobStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<i64>,
    /// 连续执行错误次数（成功时重置），用于退避
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consecutive_errors: Option<i32>,
}

/// 完整定时任务（对齐 Go `CronJob` + permanent 扩展）
///
/// 14 直接字段 + CronSchedule(6) + CronPayload(11) + CronDelivery(4) + CronJobState(7) = 42 可达字段
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// L1 根因修复：业主命名空间标识，区分 cron job 的"谁管这条"。
    /// 当前已知值："agent-fleet.skill-trigger"（gateway.skillTriggerRegistrar 独占）。
    /// 老 store 无此字段 → 读出来为 None，与"未声明业主"语义一致；registrar 做双路
    /// 匹配（owner==agent-fleet.skill-trigger || name 前缀 fallback）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub owner: Option<String>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_run: Option<bool>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub schedule: CronSchedule,
    #[serde(default)]
    pub session_target: SessionTarget,
    #[serde(default)]
    pub wake_mode: WakeMode,
    pub payload: CronPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    #[serde(default)]
    pub state: CronJobState,
    /// 永久任务（豁免 age-based auto-expiry）— Rust 调度器扩展字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
    /// Phase D: 会话续写字段（仅在 `session_target` == Continuation 时有意义）。
    /// daemon 触发 Continuation 任务时，把 payload.message 通过 HTTP 回调到 Go gateway，
    /// gateway 用这些字段调用 SessionContinuation.AppendAssistantMessage 写回原会话。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub continuation_kind: Option<String>, // "chat" | "notify" | "both"
}

impl CronJob {
    /// 一次性任务判定的唯一真源。`ScheduleKind::At` 在语义上天然 one-shot
    /// （绝对时间触发一次），无论入口是否传 `delete_after_run`；显式
    /// `delete_after_run == Some(true)` 也是 one-shot（如 one-shot cron）。
    #[must_use]
    pub fn is_one_shot(&self) -> bool {
        self.delete_after_run == Some(true) || self.schedule.kind == ScheduleKind::At
    }
}

/// v2 持久化文件格式（版本化）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronStoreFile {
    pub version: i32,
    pub jobs: Vec<CronJob>,
}

impl Default for CronStoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            jobs: Vec::new(),
        }
    }
}

/// 创建任务入参（对齐 Go CronJobCreate，无 id/createdAtMs/updatedAtMs/state）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronJobCreate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_run: Option<bool>,
    pub schedule: CronSchedule,
    #[serde(default)]
    pub session_target: SessionTarget,
    #[serde(default)]
    pub wake_mode: WakeMode,
    pub payload: CronPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
    // Phase D: see CronJob 同名字段说明
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub continuation_kind: Option<String>,
}

impl CronJobCreate {
    /// 转换为 CronJob（自动生成 id/createdAtMs/updatedAtMs）
    #[must_use]
    pub fn into_job(self) -> CronJob {
        let now_ms = chrono::Utc::now().timestamp_millis();
        CronJob {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: self.agent_id,
            name: self.name,
            description: self.description,
            owner: self.owner,
            enabled: self.enabled.unwrap_or(true),
            delete_after_run: if self.schedule.kind == ScheduleKind::At {
                Some(true)
            } else {
                self.delete_after_run
            },
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            schedule: self.schedule,
            session_target: self.session_target,
            wake_mode: self.wake_mode,
            payload: self.payload,
            delivery: self.delivery,
            state: CronJobState::default(),
            permanent: self.permanent,
            session_key: self.session_key,
            channel_id: self.channel_id,
            continuation_kind: self.continuation_kind,
        }
    }
}

/// 更新任务入参（对齐 Go CronJobPatch，所有字段可选）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CronJobPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_target: Option<SessionTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_mode: Option<WakeMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
}

impl CronJobPatch {
    /// 将补丁应用到 CronJob（自动更新 updatedAtMs）
    pub fn apply_to(self, job: &mut CronJob) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Some(v) = self.agent_id {
            job.agent_id = Some(v);
        }
        if let Some(v) = self.name {
            job.name = v;
        }
        if let Some(v) = self.description {
            job.description = Some(v);
        }
        if let Some(v) = self.owner {
            job.owner = Some(v);
        }
        if let Some(v) = self.enabled {
            job.enabled = v;
        }
        if let Some(v) = self.delete_after_run {
            job.delete_after_run = Some(v);
        }
        if let Some(v) = self.schedule {
            job.schedule = v;
        }
        if let Some(v) = self.session_target {
            job.session_target = v;
        }
        if let Some(v) = self.wake_mode {
            job.wake_mode = v;
        }
        if let Some(v) = self.payload {
            job.payload = v;
        }
        if let Some(v) = self.delivery {
            job.delivery = Some(v);
        }
        if let Some(v) = self.permanent {
            job.permanent = Some(v);
        }
        job.updated_at_ms = now_ms;
    }
}

// ============================================================================
// CronTask → CronJob 迁移
// ============================================================================

impl From<CronTask> for CronJob {
    fn from(task: CronTask) -> Self {
        let now_ms = chrono::Utc::now().timestamp_millis();
        // name: 截取 prompt 前 50 字符
        let name: String = task.prompt.chars().take(50).collect();
        let name = if task.prompt.chars().count() > 50 {
            format!("{name}...")
        } else {
            name
        };
        Self {
            id: task.id,
            agent_id: None,
            name,
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: if task.recurring == Some(true) {
                Some(false)
            } else {
                Some(true)
            },
            created_at_ms: task.created_at,
            updated_at_ms: now_ms,
            schedule: CronSchedule {
                kind: ScheduleKind::Cron,
                at: None,
                every_ms: None,
                anchor_ms: None,
                expr: Some(task.cron),
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some(task.prompt),
                message: None,
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
            state: CronJobState {
                next_run_at_ms: None,
                running_at_ms: None,
                last_run_at_ms: task.last_fired_at,
                last_status: None,
                last_error: None,
                last_duration_ms: None,
                consecutive_errors: None,
            },
            permanent: task.permanent,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }
    }
}

// ============================================================================
// 文件路径
// ============================================================================

// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const CRON_FILE_REL: &str = "cron/scheduled_tasks.json";

/// 获取 cron 文件完整路径
#[must_use]
pub fn cron_file_path(dir: &Path) -> PathBuf {
    dir.join(CRON_FILE_REL)
}

// ============================================================================
// 旧版读写函数（保持向后兼容，daemon 过渡期使用）
// ============================================================================

/// 读取 `scheduled_tasks.json（旧接口，仅读取` v1 格式）
///
/// 文件不存在或解析失败时返回空任务列表。
pub async fn read_tasks(dir: &Path) -> CronFile {
    let path = cron_file_path(dir);
    match fs::read_to_string(&path).await {
        Ok(content) => match serde_json::from_str::<CronFile>(&content) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("scheduled_tasks.json 解析失败: {e}");
                CronFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CronFile::default(),
        Err(e) => {
            tracing::warn!("读取 scheduled_tasks.json 失败: {e}");
            CronFile::default()
        }
    }
}

/// 写入 `scheduled_tasks.json（旧接口，v1` 格式，原子写入）
pub async fn write_tasks(dir: &Path, file: &CronFile) -> std::io::Result<()> {
    let path = cron_file_path(dir);
    let tmp_path = path.with_extension("json.tmp");

    // 确保目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let content = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;

    fs::write(&tmp_path, &content).await?;
    fs::rename(&tmp_path, &path).await?;

    tracing::debug!("写入 {} 个任务到 {}", file.tasks.len(), path.display());
    Ok(())
}

/// 标记任务已触发：更新 `last_fired_at，删除` one-shot 任务（旧接口）
pub fn mark_tasks_fired(file: &mut CronFile, fired_ids: &[String], now_ms: i64) {
    file.tasks.retain_mut(|task| {
        if !fired_ids.contains(&task.id) {
            return true;
        }
        if task.recurring == Some(true) {
            task.last_fired_at = Some(now_ms);
            true
        } else {
            // one-shot: 删除
            tracing::info!(id = %task.id, "one-shot 任务已触发，移除");
            false
        }
    });
}

// ============================================================================
// v2 存储读写（CronJob 格式）
// ============================================================================

/// 读取定时任务存储（自动检测 v1/v2 格式，v1 自动迁移为 v2）
pub async fn read_store(dir: &Path) -> CronStoreFile {
    let path = cron_file_path(dir);
    let content = match fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CronStoreFile::default(),
        Err(e) => {
            tracing::warn!("读取定时任务文件失败: {e}");
            return CronStoreFile::default();
        }
    };

    // 使用 Value 探测文件格式
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("定时任务文件 JSON 解析失败: {e}");
            return CronStoreFile::default();
        }
    };

    // v2 格式: { "version": N, "jobs": [...] }
    if value.get("jobs").is_some() {
        match serde_json::from_value::<CronStoreFile>(value) {
            Ok(store) => return store,
            Err(e) => tracing::warn!("v2 格式解析失败，尝试 v1: {e}"),
        }
    // v1 格式: { "tasks": [...] }
    } else if value.get("tasks").is_some() {
        match serde_json::from_value::<CronFile>(value) {
            Ok(legacy) => {
                let jobs = legacy.tasks.into_iter().map(CronJob::from).collect();
                tracing::info!("从 v1 格式自动迁移定时任务");
                return CronStoreFile {
                    version: STORE_VERSION,
                    jobs,
                };
            }
            Err(e) => tracing::warn!("v1 格式解析失败: {e}"),
        }
    }

    tracing::warn!("定时任务文件格式无法识别");
    CronStoreFile::default()
}

/// 写入定时任务存储（v2 格式，原子写入）
pub async fn write_store(dir: &Path, store: &CronStoreFile) -> std::io::Result<()> {
    let path = cron_file_path(dir);
    let tmp_path = path.with_extension("json.tmp");

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let content = serde_json::to_string_pretty(store).map_err(std::io::Error::other)?;

    // 文件权限 0600：scheduled_tasks.json 含 prompt 正文 + session/channel key，
    // 限制为所有者可读写（与 outbox / fire_log 一致；旧实现走 fs::write 落 0644
    // 由 umask 决定，会把这些敏感字段暴露给同机其他用户）。先以 0600 写 tmp 再
    // rename，保证最终文件从创建起就是 0600（rename 不改权限）。
    // Windows 侧 mode 被忽略，由文件系统 ACL 控制。
    {
        use tokio::io::AsyncWriteExt;
        let mut open_opts = fs::OpenOptions::new();
        open_opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            open_opts.mode(0o600);
        }
        let mut f = open_opts.open(&tmp_path).await?;
        f.write_all(content.as_bytes()).await?;
        f.flush().await?;
    }
    fs::rename(&tmp_path, &path).await?;

    tracing::debug!("写入 {} 个任务到 {}", store.jobs.len(), path.display());
    Ok(())
}

/// 标记任务已触发（CronJob 版本）：更新 state，删除 `delete_after_run` 任务
pub fn mark_jobs_fired(store: &mut CronStoreFile, fired_ids: &[String], now_ms: i64) {
    store.jobs.retain_mut(|job| {
        if !fired_ids.contains(&job.id) {
            return true;
        }
        if job.is_one_shot() {
            // 一次性任务：删除
            tracing::info!(id = %job.id, "一次性任务已触发，移除");
            false
        } else {
            // 周期任务：更新 last_run_at_ms
            job.state.last_run_at_ms = Some(now_ms);
            true
        }
    });
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- 旧格式兼容测试 ---

    #[test]
    fn serde_roundtrip() {
        let task = CronTask {
            id: "test-1".into(),
            cron: "*/5 * * * *".into(),
            prompt: "hello".into(),
            created_at: 1712966400000,
            last_fired_at: None,
            recurring: Some(true),
            permanent: None,
        };
        let file = CronFile {
            tasks: vec![task.clone()],
        };

        let json = serde_json::to_string(&file).expect("serialize");
        let parsed: CronFile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.tasks.len(), 1);
        assert_eq!(parsed.tasks[0].id, "test-1");
        assert_eq!(parsed.tasks[0].recurring, Some(true));
    }

    #[test]
    fn camel_case_compat() {
        // Verify the JSON uses camelCase (matching TS format)
        let task = CronTask {
            id: "t".into(),
            cron: "* * * * *".into(),
            prompt: "p".into(),
            created_at: 1000,
            last_fired_at: Some(2000),
            recurring: None,
            permanent: None,
        };
        let json = serde_json::to_string(&task).expect("serialize");
        assert!(json.contains("\"createdAt\""));
        assert!(json.contains("\"lastFiredAt\""));
        assert!(!json.contains("\"created_at\""));
    }

    #[test]
    fn mark_fired_removes_oneshot() {
        let mut file = CronFile {
            tasks: vec![
                CronTask {
                    id: "oneshot".into(),
                    cron: "0 9 * * *".into(),
                    prompt: "once".into(),
                    created_at: 1000,
                    last_fired_at: None,
                    recurring: None,
                    permanent: None,
                },
                CronTask {
                    id: "recurring".into(),
                    cron: "0 9 * * *".into(),
                    prompt: "repeat".into(),
                    created_at: 1000,
                    last_fired_at: None,
                    recurring: Some(true),
                    permanent: None,
                },
            ],
        };

        mark_tasks_fired(&mut file, &["oneshot".into(), "recurring".into()], 5000);

        assert_eq!(file.tasks.len(), 1);
        assert_eq!(file.tasks[0].id, "recurring");
        assert_eq!(file.tasks[0].last_fired_at, Some(5000));
    }

    #[tokio::test]
    async fn read_missing_file_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let file = read_tasks(dir.path()).await;
        assert!(file.tasks.is_empty());
    }

    #[tokio::test]
    async fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let file = CronFile {
            tasks: vec![CronTask {
                id: "rt-1".into(),
                cron: "0 * * * *".into(),
                prompt: "test roundtrip".into(),
                created_at: 1000,
                last_fired_at: None,
                recurring: None,
                permanent: None,
            }],
        };

        write_tasks(dir.path(), &file).await.expect("write");
        let read_back = read_tasks(dir.path()).await;
        assert_eq!(read_back.tasks.len(), 1);
        assert_eq!(read_back.tasks[0].id, "rt-1");
    }

    // --- CronJob 类型测试 ---

    #[test]
    fn cron_job_serde_roundtrip() {
        let job = CronJob {
            id: "job-1".into(),
            agent_id: Some("agent-x".into()),
            name: "Test Job".into(),
            description: Some("A test job".into()),
            owner: None,
            enabled: true,
            delete_after_run: Some(false),
            created_at_ms: 1712966400000,
            updated_at_ms: 1712966400000,
            schedule: CronSchedule {
                kind: ScheduleKind::Cron,
                at: None,
                every_ms: None,
                anchor_ms: None,
                expr: Some("*/5 * * * *".into()),
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("hello".into()),
                message: None,
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };

        let json = serde_json::to_string_pretty(&job).expect("serialize");
        let parsed: CronJob = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id, "job-1");
        assert_eq!(parsed.agent_id, Some("agent-x".into()));
        assert_eq!(parsed.schedule.kind, ScheduleKind::Cron);
        assert_eq!(parsed.payload.kind, PayloadKind::SystemEvent);
    }

    #[test]
    fn cron_job_json_field_names() {
        let job = CronJob {
            id: "j".into(),
            agent_id: None,
            name: "n".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None,
            created_at_ms: 1000,
            updated_at_ms: 2000,
            schedule: CronSchedule {
                kind: ScheduleKind::Every,
                at: None,
                every_ms: Some(60000),
                anchor_ms: Some(1000),
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Isolated,
            wake_mode: WakeMode::Now,
            payload: CronPayload {
                kind: PayloadKind::AgentTurn,
                text: None,
                message: Some("do something".into()),
                model: None,
                thinking: None,
                timeout_seconds: Some(300),
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: Some(CronDelivery {
                mode: DeliveryMode::Announce,
                channel: Some("feishu".into()),
                to: None,
                best_effort: Some(true),
            }),
            state: CronJobState {
                next_run_at_ms: Some(5000),
                running_at_ms: None,
                last_run_at_ms: Some(3000),
                last_status: Some(JobStatus::Ok),
                last_error: None,
                last_duration_ms: Some(1200),
                consecutive_errors: None,
            },
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };

        let json = serde_json::to_string(&job).expect("serialize");
        // Go 对齐: camelCase 字段名
        assert!(json.contains("\"createdAtMs\""));
        assert!(json.contains("\"updatedAtMs\""));
        assert!(json.contains("\"everyMs\""));
        assert!(json.contains("\"anchorMs\""));
        assert!(json.contains("\"sessionTarget\":\"isolated\""));
        assert!(json.contains("\"wakeMode\":\"now\""));
        assert!(json.contains("\"nextRunAtMs\""));
        assert!(json.contains("\"lastRunAtMs\""));
        assert!(json.contains("\"lastDurationMs\""));
        assert!(json.contains("\"timeoutSeconds\""));
        assert!(json.contains("\"bestEffort\""));
    }

    #[test]
    fn schedule_kind_serde() {
        assert_eq!(serde_json::to_string(&ScheduleKind::At).unwrap(), "\"at\"");
        assert_eq!(
            serde_json::to_string(&ScheduleKind::Every).unwrap(),
            "\"every\""
        );
        assert_eq!(
            serde_json::to_string(&ScheduleKind::Cron).unwrap(),
            "\"cron\""
        );
    }

    #[test]
    fn delivery_mode_serde() {
        assert_eq!(
            serde_json::to_string(&DeliveryMode::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&DeliveryMode::Announce).unwrap(),
            "\"announce\""
        );
    }

    #[test]
    fn wake_mode_serde() {
        assert_eq!(
            serde_json::to_string(&WakeMode::NextHeartbeat).unwrap(),
            "\"next-heartbeat\""
        );
        assert_eq!(serde_json::to_string(&WakeMode::Now).unwrap(), "\"now\"");
    }

    #[test]
    fn payload_kind_serde() {
        assert_eq!(
            serde_json::to_string(&PayloadKind::SystemEvent).unwrap(),
            "\"systemEvent\""
        );
        assert_eq!(
            serde_json::to_string(&PayloadKind::AgentTurn).unwrap(),
            "\"agentTurn\""
        );
    }

    #[test]
    fn job_status_serde() {
        assert_eq!(serde_json::to_string(&JobStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&JobStatus::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(
            serde_json::to_string(&JobStatus::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    // --- CronTask → CronJob 迁移测试 ---

    #[test]
    fn legacy_task_to_job_migration() {
        let task = CronTask {
            id: "legacy-1".into(),
            cron: "0 9 * * 1".into(),
            prompt: "weekly report".into(),
            created_at: 1712966400000,
            last_fired_at: Some(1713052800000),
            recurring: Some(true),
            permanent: Some(true),
        };

        let job: CronJob = task.into();
        assert_eq!(job.id, "legacy-1");
        assert_eq!(job.name, "weekly report");
        assert!(job.enabled);
        assert_eq!(job.delete_after_run, Some(false)); // recurring → not delete
        assert_eq!(job.created_at_ms, 1712966400000);
        assert_eq!(job.schedule.kind, ScheduleKind::Cron);
        assert_eq!(job.schedule.expr, Some("0 9 * * 1".into()));
        assert_eq!(job.payload.kind, PayloadKind::SystemEvent);
        assert_eq!(job.payload.text, Some("weekly report".into()));
        assert_eq!(job.state.last_run_at_ms, Some(1713052800000));
        assert_eq!(job.permanent, Some(true));
    }

    #[test]
    fn oneshot_task_to_job_migration() {
        let task = CronTask {
            id: "os-1".into(),
            cron: "30 14 * * *".into(),
            prompt: "one time task".into(),
            created_at: 1712966400000,
            last_fired_at: None,
            recurring: None,
            permanent: None,
        };

        let job: CronJob = task.into();
        assert_eq!(job.delete_after_run, Some(true)); // non-recurring → delete
        assert_eq!(job.permanent, None);
    }

    // --- CronStoreFile 读写测试 ---

    #[tokio::test]
    async fn read_store_missing_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let store = read_store(dir.path()).await;
        assert!(store.jobs.is_empty());
        assert_eq!(store.version, STORE_VERSION);
    }

    #[tokio::test]
    async fn write_store_and_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![CronJob {
                id: "store-1".into(),
                agent_id: None,
                name: "Store Test".into(),
                description: None,
                owner: None,
                enabled: true,
                delete_after_run: None,
                created_at_ms: 1000,
                updated_at_ms: 1000,
                schedule: CronSchedule {
                    kind: ScheduleKind::Cron,
                    at: None,
                    every_ms: None,
                    anchor_ms: None,
                    expr: Some("0 * * * *".into()),
                    tz: None,
                },
                session_target: SessionTarget::Main,
                wake_mode: WakeMode::NextHeartbeat,
                payload: CronPayload {
                    kind: PayloadKind::SystemEvent,
                    text: Some("test".into()),
                    message: None,
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
                session_key: None,
                channel_id: None,
                continuation_kind: None,
            }],
        };

        write_store(dir.path(), &store).await.expect("write");
        let read_back = read_store(dir.path()).await;
        assert_eq!(read_back.jobs.len(), 1);
        assert_eq!(read_back.jobs[0].id, "store-1");
        assert_eq!(read_back.version, STORE_VERSION);

        // A19 回归：scheduled_tasks.json 含 prompt 正文 + session/channel key，
        // 落盘权限必须是 0600（不再依赖 umask 落 0644）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(cron_file_path(dir.path())).expect("stat");
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "scheduled_tasks.json 必须 0600"
            );
        }
    }

    #[tokio::test]
    async fn read_store_auto_migrates_v1() {
        let dir = tempfile::tempdir().expect("tmpdir");
        // 写入 v1 格式
        let legacy = CronFile {
            tasks: vec![CronTask {
                id: "v1-task".into(),
                cron: "0 9 * * *".into(),
                prompt: "migrate me".into(),
                created_at: 1000,
                last_fired_at: Some(2000),
                recurring: Some(true),
                permanent: None,
            }],
        };
        write_tasks(dir.path(), &legacy).await.expect("write v1");

        // read_store 应自动迁移
        let store = read_store(dir.path()).await;
        assert_eq!(store.version, STORE_VERSION);
        assert_eq!(store.jobs.len(), 1);
        assert_eq!(store.jobs[0].id, "v1-task");
        assert_eq!(store.jobs[0].schedule.kind, ScheduleKind::Cron);
        assert_eq!(store.jobs[0].schedule.expr, Some("0 9 * * *".into()));
        assert_eq!(store.jobs[0].payload.text, Some("migrate me".into()));
        assert_eq!(store.jobs[0].state.last_run_at_ms, Some(2000));
        assert_eq!(store.jobs[0].delete_after_run, Some(false)); // recurring
    }

    // --- mark_jobs_fired 测试 ---

    #[test]
    fn mark_jobs_fired_removes_delete_after_run() {
        let mut store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![
                CronJob {
                    id: "oneshot".into(),
                    agent_id: None,
                    name: "Once".into(),
                    description: None,
                    owner: None,
                    enabled: true,
                    delete_after_run: Some(true),
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                    schedule: CronSchedule {
                        kind: ScheduleKind::Cron,
                        at: None,
                        every_ms: None,
                        anchor_ms: None,
                        expr: Some("0 9 * * *".into()),
                        tz: None,
                    },
                    session_target: SessionTarget::Main,
                    wake_mode: WakeMode::NextHeartbeat,
                    payload: CronPayload {
                        kind: PayloadKind::SystemEvent,
                        text: Some("once".into()),
                        message: None,
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
                    session_key: None,
                    channel_id: None,
                    continuation_kind: None,
                },
                CronJob {
                    id: "recurring".into(),
                    agent_id: None,
                    name: "Repeat".into(),
                    description: None,
                    owner: None,
                    enabled: true,
                    delete_after_run: Some(false),
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                    schedule: CronSchedule {
                        kind: ScheduleKind::Cron,
                        at: None,
                        every_ms: None,
                        anchor_ms: None,
                        expr: Some("0 9 * * *".into()),
                        tz: None,
                    },
                    session_target: SessionTarget::Main,
                    wake_mode: WakeMode::NextHeartbeat,
                    payload: CronPayload {
                        kind: PayloadKind::SystemEvent,
                        text: Some("repeat".into()),
                        message: None,
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
                    session_key: None,
                    channel_id: None,
                    continuation_kind: None,
                },
            ],
        };

        mark_jobs_fired(&mut store, &["oneshot".into(), "recurring".into()], 5000);

        assert_eq!(store.jobs.len(), 1);
        assert_eq!(store.jobs[0].id, "recurring");
        assert_eq!(store.jobs[0].state.last_run_at_ms, Some(5000));
    }

    // --- CronJobCreate / CronJobPatch 测试 ---

    #[test]
    fn create_into_job() {
        let create = CronJobCreate {
            agent_id: None,
            name: "New Job".into(),
            description: None,
            owner: None,
            enabled: None,
            delete_after_run: None,
            schedule: CronSchedule {
                kind: ScheduleKind::Every,
                at: None,
                every_ms: Some(3600000),
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::Now,
            payload: CronPayload {
                kind: PayloadKind::AgentTurn,
                text: None,
                message: Some("check status".into()),
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
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };

        let job = create.into_job();
        assert!(!job.id.is_empty());
        assert_eq!(job.name, "New Job");
        assert!(job.enabled); // default true
        assert_eq!(job.schedule.kind, ScheduleKind::Every);
        assert_eq!(job.wake_mode, WakeMode::Now);
        assert!(job.created_at_ms > 0);
        assert_eq!(job.created_at_ms, job.updated_at_ms);
    }

    // --- is_one_shot / At one-shot 归一测试 ---

    /// `into_job()` on a `CronJobCreate` with `kind == At` and `delete_after_run: None`
    /// 必须持久化 `delete_after_run == Some(true)`，且 `is_one_shot()` 为 true。
    #[test]
    fn create_into_job_at_kind_normalizes_delete_after_run_to_true() {
        let create = CronJobCreate {
            agent_id: None,
            name: "At Job".into(),
            description: None,
            owner: None,
            enabled: None,
            delete_after_run: None, // 入口未传，At 仍应归一
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00Z".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::Now,
            payload: CronPayload {
                kind: PayloadKind::AgentTurn,
                text: None,
                message: Some("absolute time task".into()),
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
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };

        let job = create.into_job();
        assert_eq!(job.delete_after_run, Some(true));
        assert!(job.is_one_shot());
    }

    /// `CronJob` with `kind == At`, `delete_after_run: None` → `is_one_shot() == true`。
    #[test]
    fn is_one_shot_at_kind_with_none_delete_after_run() {
        let job = CronJob {
            id: "at-1".into(),
            agent_id: None,
            name: "At".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00Z".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("at".into()),
                message: None,
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        assert!(job.is_one_shot());
    }

    /// `CronJob` with `kind == Cron`, `delete_after_run: Some(false)` → `is_one_shot() == false`。
    #[test]
    fn is_one_shot_cron_kind_with_delete_after_run_false_is_recurring() {
        let job = CronJob {
            id: "cron-1".into(),
            agent_id: None,
            name: "Cron".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(false),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::Cron,
                at: None,
                every_ms: None,
                anchor_ms: None,
                expr: Some("0 9 * * *".into()),
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("cron".into()),
                message: None,
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        assert!(!job.is_one_shot());
    }

    /// `CronJob` with `kind == Cron`, `delete_after_run: Some(true)` → `is_one_shot() == true`
    /// （one-shot cron）。
    #[test]
    fn is_one_shot_cron_kind_with_delete_after_run_true_is_one_shot() {
        let job = CronJob {
            id: "cron-os".into(),
            agent_id: None,
            name: "Cron OneShot".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::Cron,
                at: None,
                every_ms: None,
                anchor_ms: None,
                expr: Some("0 9 * * *".into()),
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("cron-os".into()),
                message: None,
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        assert!(job.is_one_shot());
    }

    #[test]
    fn patch_apply_to_job() {
        let mut job = CronJob {
            id: "p-1".into(),
            agent_id: None,
            name: "Original".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::Cron,
                at: None,
                every_ms: None,
                anchor_ms: None,
                expr: Some("0 * * * *".into()),
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("old".into()),
                message: None,
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };

        let patch = CronJobPatch {
            name: Some("Updated".into()),
            enabled: Some(false),
            ..Default::default()
        };

        patch.apply_to(&mut job);
        assert_eq!(job.name, "Updated");
        assert!(!job.enabled);
        assert!(job.updated_at_ms > 1000);
        // 未修改字段保持不变
        assert_eq!(job.id, "p-1");
        assert_eq!(job.schedule.kind, ScheduleKind::Cron);
    }
}
