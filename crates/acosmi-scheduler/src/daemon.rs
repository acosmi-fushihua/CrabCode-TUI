//! `SchedulerDaemon` 守护主循环
//!
//! 统一调度引擎，支持三种 schedule kind（at/every/cron）。
//! 基于 TS `cronScheduler.ts` 移植，扩展为 Go `CronJob` 完整 schema。
//! 包含：jitter 防雷暴、in-flight 防双触发、lock probe 接管、
//! missed task 检测、enabled/disabled 过滤、错误退避（7 级）、
//! 执行状态跟踪、IANA 时区支持。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing;

use crate::cron_parse::parse_cron;
use crate::fire_log;
use crate::jitter::{
    CronJitterConfig, is_recurring_task_aged, jittered_next_cron_run_ms_tz, next_every_run_ms,
    one_shot_jittered_next_cron_run_ms_tz, parse_iso8601_to_ms,
};
use crate::lock;
use crate::task_store::{
    self, CronJob, CronJobCreate, CronJobPatch, CronStoreFile, STORE_VERSION, ScheduleKind,
};

const LOCK_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// 错误退避阶梯（7 级：0 → 30s → 1min → 5min → 15min → 30min → 1h）
const BACKOFF_STEPS_MS: &[i64] = &[
    0,         // level 0: 无退避
    30_000,    // level 1: 30s
    60_000,    // level 2: 1min
    300_000,   // level 3: 5min
    900_000,   // level 4: 15min
    1_800_000, // level 5: 30min
    3_600_000, // level 6: 1h（上限）
];

/// 根据连续错误次数计算退避时间
fn backoff_ms(consecutive_errors: i32) -> i64 {
    // 负值（理论上不出现，但 schema 没强约束 i32 ≥ 0）按 0 处理；
    // try_from 失败 → 0；上限取 steps 末尾。
    let idx = usize::try_from(consecutive_errors)
        .unwrap_or(0)
        .min(BACKOFF_STEPS_MS.len() - 1);
    BACKOFF_STEPS_MS[idx]
}

/// 调度器配置
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// 主循环检查间隔（TS: `CHECK_INTERVAL_MS` = 1000）
    pub check_interval: Duration,
    /// 文件变化后的稳定等待时间（TS: `FILE_STABILITY_MS` = 300）
    pub file_stability: Duration,
    /// Jitter 配置（可从 `GrowthBook` 动态注入）
    pub jitter: CronJitterConfig,
    /// 任务过滤器
    pub filter_mode: Option<FilterMode>,
}

/// 任务过滤模式
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterMode {
    /// 仅处理 permanent 任务（daemon cron worker 用）
    PermanentOnly,
    /// 处理所有任务（默认）
    All,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(1),
            file_stability: Duration::from_millis(300),
            jitter: CronJitterConfig::default(),
            filter_mode: None,
        }
    }
}

/// Why a JobFiredEvent fired. Scheduler-native (the ledger crate depends on
/// this crate, so ledger types cannot be named here); crabcode-cron maps this
/// to the ledger's OccurrenceKind + FireAdvance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireReason {
    /// Schedule matched; recurring and not aged-out → advance schedule state.
    ScheduledRecurring,
    /// Schedule matched; one-shot / aged-out / `at` → the job is deleted.
    ScheduledFinal,
    /// Manual trigger (`Run` command) → no advance, no delete.
    Manual,
}

/// 任务触发事件（携带完整 `CronJob` 信息供上游网关路由）
#[derive(Debug, Clone)]
pub struct JobFiredEvent {
    /// 触发的完整任务（含 `payload/delivery/session_target`）
    pub job: CronJob,
    /// Theoretical due instant this fire corresponds to (epoch ms). For a
    /// scheduled fire this is `next` (restart-stable); for a manual fire it is
    /// the wall-clock trigger time. This is the durable occurrence key.
    pub scheduled_at_ms: i64,
    /// Why this fired.
    pub reason: FireReason,
}

/// 单个 missed 任务的明细 —— 携带原计划触发时刻 + 补投窗口分类。
///
/// W-CRON-AUTOMATION-E2E P7：scheduler 侧负责检出 missed 任务、算出它原本
/// 应触发的时刻、判定是否落在 `MISSED_CATCHUP_WINDOW_MS` 补投窗口内；
/// daemon 侧（`crabcode-cron`）拿到这份明细后把它写进持久 outbox。
#[derive(Debug, Clone)]
pub struct MissedJobInfo {
    /// missed 的完整任务。
    pub job: CronJob,
    /// 任务原本应触发的时刻（epoch ms）。
    pub scheduled_at_ms: i64,
    /// 计划触发时刻是否落在 `MISSED_CATCHUP_WINDOW_MS` 之内。
    /// true → daemon 写 `Fired`（晚到补投，消费方仍执行）；
    /// false → daemon 写 `Skipped`（太陈旧，仅记录不执行）。
    pub within_catchup_window: bool,
}

/// Missed task 事件
#[derive(Debug, Clone)]
pub struct MissedJobsEvent {
    pub jobs: Vec<MissedJobInfo>,
}

// 保留旧类型别名供过渡期编译兼容
pub type TaskFiredEvent = JobFiredEvent;
pub type MissedTasksEvent = MissedJobsEvent;

/// Cron 调度器命令 —— IPC handler 通过 channel 向 daemon 主循环投递 CRUD 请求。
///
/// 2026-04-21 根因修复：原实现中 IPC handler 直接读写 `scheduled_tasks.json`，
/// 不更新 daemon 的 self.jobs，导致 daemon 每次 tick 末尾 `write_store`
/// （用 `self.jobs.clone()` 快照回写）会覆盖 IPC handler 刚写入的新 job。
/// Go gateway 也同步走文件直写，造成双写竞态，user job 在下次 tick 被抹。
///
/// 改造后：所有 CRUD 都经 `CronCommand` 投递给 daemon 主循环，
/// daemon 在两个 tick 之间串行处理命令，统一通过 `persist_jobs()` 出口
/// 写盘，保证内存与文件的原子一致。
pub enum CronCommand {
    Add {
        input: CronJobCreate,
        reply: oneshot::Sender<Result<CronAddResult, CronError>>,
    },
    Remove {
        id: String,
        reply: oneshot::Sender<Result<(), CronError>>,
    },
    Update {
        id: String,
        patch: CronJobPatch,
        reply: oneshot::Sender<Result<(), CronError>>,
    },
    List {
        include_disabled: bool,
        reply: oneshot::Sender<Vec<CronJob>>,
    },
    Status {
        reply: oneshot::Sender<CronStoreStatus>,
    },
    /// 立宪 3 阶段 3-D 补全：手动触发某 cron job —— 直接 push `JobFiredEvent`
    /// 到 `event_tx`，与 schedule 触发同路径（含 `fire_log` 落盘 / `running_at_ms`
    /// / `last_run_at_ms` 时间戳），但不更新磁盘 state、不删 one-shot —— 让
    /// 正常 schedule 继续 tick。
    Run {
        id: String,
        reply: oneshot::Sender<Result<(), CronError>>,
    },
    /// 立宪 3 阶段 3-D 补全：读取某 cron job 的运行历史 —— 从 `fire_log` JSONL
    /// 过滤 `job_id` 后取末尾 N 条。daemon 与 IPC handler 都不持久化排序顺序，
    /// 直接相信文件追加顺序（与 `fire_log::append` 单调一致）。
    Runs {
        id: String,
        limit: usize,
        reply: oneshot::Sender<Vec<fire_log::FireLogEntry>>,
    },
}

#[derive(Debug, Clone)]
pub struct CronAddResult {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct CronStoreStatus {
    pub version: i32,
    pub job_count: usize,
    pub enabled_count: usize,
}

#[derive(Debug, Clone)]
pub enum CronError {
    NotFound(String),
    Persist(String),
}

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "cron job not found: {id}"),
            Self::Persist(msg) => write!(f, "cron persist failed: {msg}"),
        }
    }
}

impl std::error::Error for CronError {}

// ─────────────────── Scheduler 锁所有权回写 sink（PR-B / Finding A 修复）───────────────────
//
// 2026-04-21 v3 补审发现 "Readiness gate 观测剧场" 根因:
//   - supervisor 启动时 *乐观* 写 `supervisor_status.scheduler.is_owner = true`
//   - daemon.run() 永远返 Ok(()),调用侧 Err 分支是死代码 → 从不回写 false
//   - daemon 真实的 `self.is_owner` (初始抢锁结果 / lock_probe 恢复 / 关停释放)
//     是 private 字段,**没有**同步到 supervisor 共享状态的通道
// 后果: `SchedulerReady()` 永远 true,gate 只挡 UDS 完全不通;挡不住锁被占、
//       watcher 挂、daemon 监视错目录、daemon 静默 idle。
//
// 修复: 引入 `SchedulerStatusSink` 接口,daemon 在四个真实时机 publish;
// supervisor 侧用 `ChannelStatusSink(mpsc::Sender)` 串行写 RwLock。
// 方向单一 (daemon → supervisor) 避免双写冲突,buffer 16 对瞬时抖动足够。

/// Daemon 对外发布 `is_owner` 变化的接口。Send + Sync,可在 Arc 里跨 task 持。
pub trait SchedulerStatusSink: Send + Sync {
    /// 记录当前是否持有调度器锁。必须幂等:可重复调用同值不产生副作用。
    fn set_is_owner(&self, is_owner: bool);
}

/// 基于 `tokio::mpsc::Sender<bool>` 的 sink 实现。
///
/// 语义:
///   - `try_send` 非阻塞,buffer full 时丢弃并 `tracing::warn`
///   - 丢弃容忍来自: daemon 高频切换锁状态概率极低 (初始/恢复/关停三处 + 偶发 probe)
///   - 若 forwarder 停止消费 → `rx.recv()` drop, `try_send` 返回 Closed,记 warn
pub struct ChannelStatusSink {
    tx: mpsc::Sender<bool>,
}

impl ChannelStatusSink {
    #[must_use]
    pub const fn new(tx: mpsc::Sender<bool>) -> Self {
        Self { tx }
    }
}

impl SchedulerStatusSink for ChannelStatusSink {
    fn set_is_owner(&self, is_owner: bool) {
        if let Err(e) = self.tx.try_send(is_owner) {
            tracing::warn!(
                err = %e,
                is_owner,
                "SchedulerStatusSink: 发布 is_owner 失败 (forwarder 过载或已停),readiness gate 可能短暂滞后",
            );
        }
    }
}

/// 守护调度器（CronJob 统一引擎）
pub struct SchedulerDaemon {
    dir: PathBuf,
    config: SchedulerConfig,
    identity: String,

    /// `CronJob` 列表
    jobs: Vec<CronJob>,
    /// 每任务的下次触发时间 epoch ms
    next_fire_at: HashMap<String, i64>,
    /// 已入队但尚未从文件删除的任务
    in_flight: HashSet<String>,
    /// 已通知 missed 的任务
    missed_asked: HashSet<String>,
    /// 是否为锁主
    is_owner: bool,

    /// 正常触发事件通道
    event_tx: mpsc::Sender<JobFiredEvent>,
    /// Missed task 事件通道（可选）
    missed_tx: Option<mpsc::Sender<MissedJobsEvent>>,
    /// Cron CRUD 命令通道（可选）。
    /// 2026-04-21 新增：IPC handler 通过它向主循环投递 add/remove/update/list/status。
    /// None 时 daemon 仅处理 tick 触发和锁探测（兼容旧 `new()` 构造路径）。
    cmd_rx: Option<mpsc::Receiver<CronCommand>>,
    /// `is_owner` 变化发布通道（可选）。PR-B / Finding A 修复。
    /// None 时 daemon 不回写共享状态（单元测试默认、旧 `new()` 构造路径兼容）。
    status_sink: Option<Arc<dyn SchedulerStatusSink>>,
}

impl SchedulerDaemon {
    /// 创建新的调度器守护实例
    #[must_use]
    pub fn new(
        dir: PathBuf,
        config: SchedulerConfig,
        identity: String,
        event_tx: mpsc::Sender<JobFiredEvent>,
    ) -> Self {
        Self {
            dir,
            config,
            identity,
            jobs: Vec::new(),
            next_fire_at: HashMap::new(),
            in_flight: HashSet::new(),
            missed_asked: HashSet::new(),
            is_owner: false,
            event_tx,
            missed_tx: None,
            cmd_rx: None,
            status_sink: None,
        }
    }

    /// 设置 missed task 事件接收通道
    pub fn set_missed_tx(&mut self, tx: mpsc::Sender<MissedJobsEvent>) {
        self.missed_tx = Some(tx);
    }

    /// 设置 Cron CRUD 命令接收通道。
    /// 2026-04-21：IPC handler 通过 `mpsc::Sender<CronCommand>` 投递请求到主循环。
    /// 只调用一次，否则旧通道被丢弃（panic 不会发生，但之前的 send 方将永远等不到回复）。
    pub fn set_cmd_rx(&mut self, rx: mpsc::Receiver<CronCommand>) {
        self.cmd_rx = Some(rx);
    }

    /// 注入 `is_owner` 回写 sink。PR-B / Finding A 修复。
    /// 链式构造设计,方便 supervisor 侧与 `ChannelStatusSink` 组合:
    ///   `SchedulerDaemon::new(...).with_status_sink(Arc::new(ChannelStatusSink::new(tx)))`
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn SchedulerStatusSink>) -> Self {
        self.status_sink = Some(sink);
        self
    }

    /// 发布当前 `is_owner。无` sink 时 no-op。
    fn publish_is_owner(&self, is_owner: bool) {
        if let Some(sink) = &self.status_sink {
            sink.set_is_owner(is_owner);
        }
    }

    /// 获取所有任务中最早的下次触发时间（epoch ms）
    #[must_use]
    pub fn get_next_fire_time(&self) -> Option<i64> {
        self.next_fire_at
            .values()
            .copied()
            .filter(|&t| t < i64::MAX)
            .min()
    }

    /// 运行守护主循环
    pub async fn run(&mut self, shutdown: CancellationToken) -> std::io::Result<()> {
        // Step 2 Phase D.5 — closes Step 1 §六 R1 ④ + §一 P1-1:
        // try_acquire_lock 早就返 std::io::Result<bool>，但 caller 把它折叠
        // 成 unwrap_or(false)，把"锁被另一实例占"（Ok(false)）和"磁盘故障 /
        // 权限异常 / OOM 的真 IO 错"（Err(e)）压到同一路径——用户看到的现象
        // 是 daemon 默默以非 owner 模式启动，不调度任何任务，无告警。现在
        // 三态分流：Ok(true)=become owner / Ok(false)=normal lose-race /
        // Err=立即 propagate，让 supervisor 看见真错。
        self.is_owner = match lock::try_acquire_lock(&self.dir, &self.identity).await {
            Ok(acquired) => acquired,
            Err(e) => {
                tracing::error!(
                    dir = %self.dir.display(),
                    error = %e,
                    "SchedulerDaemon: try_acquire_lock IO 失败 — 拒绝静默以非 owner 启动",
                );
                return Err(e);
            }
        };
        // PR-B 回写点 ①: 初始抢锁结果
        self.publish_is_owner(self.is_owner);

        if shutdown.is_cancelled() {
            if self.is_owner {
                self.is_owner = false;
                let _ = lock::release_lock(&self.dir, &self.identity).await;
            }
            // PR-B 回写点 ②: shutdown 早退时确保共享状态与本地一致
            // (即使从未抢到锁,发一次 false 也是幂等的)
            self.publish_is_owner(false);
            return Ok(());
        }

        tracing::info!(
            dir = %self.dir.display(),
            is_owner = self.is_owner,
            "SchedulerDaemon 启动"
        );

        // 初始加载（读文件启动时的存量 job；之后 Rust 独占写入）
        self.load_jobs(true).await;

        let mut check_interval = tokio::time::interval(self.config.check_interval);
        let _ = check_interval.tick().await;

        let mut lock_probe_interval = tokio::time::interval(LOCK_PROBE_INTERVAL);
        let _ = lock_probe_interval.tick().await;

        // Take command receiver out of self so we can borrow it mutably
        // without borrowing the rest of self concurrently in select!.
        let mut cmd_rx = self.cmd_rx.take();

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    tracing::info!("SchedulerDaemon 收到关闭信号");
                    break;
                }
                _ = check_interval.tick() => {
                    self.check().await;
                }
                maybe_cmd = async {
                    match cmd_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // 无 cmd_rx 时永远 pending，让 select! 只等其他分支。
                        None => std::future::pending::<Option<CronCommand>>().await,
                    }
                } => {
                    if let Some(cmd) = maybe_cmd { self.handle_command(cmd).await } else {
                        tracing::info!("cron 命令通道已关闭，daemon 继续运行但不再处理 CRUD");
                        cmd_rx = None;
                    }
                }
                _ = lock_probe_interval.tick(), if !self.is_owner => {
                    match lock::try_acquire_lock(&self.dir, &self.identity).await {
                        Ok(true) => {
                            self.is_owner = true;
                            tracing::info!("锁探测成功，接管调度器");
                            // PR-B 回写点 ③: lock_probe 恢复成功
                            self.publish_is_owner(true);
                            self.load_jobs(true).await;
                        }
                        Ok(false) => {}
                        Err(e) => tracing::debug!("锁探测失败: {e}"),
                    }
                }
            }
        }

        if self.is_owner {
            self.is_owner = false;
            let _ = lock::release_lock(&self.dir, &self.identity).await;
        }
        // PR-B 回写点 ④: 主循环结束最终释放 (含 "从未抢到锁就关停" 的 false→false 幂等)
        self.publish_is_owner(false);
        tracing::info!("SchedulerDaemon 已停止");
        Ok(())
    }

    /// 任务是否通过 filter
    fn passes_filter(&self, job: &CronJob) -> bool {
        match &self.config.filter_mode {
            None | Some(FilterMode::All) => true,
            Some(FilterMode::PermanentOnly) => job.permanent == Some(true),
        }
    }

    /// 计算 `CronJob` 的下次触发时间（epoch ms）
    fn next_fire_ms_for_job(&self, job: &CronJob, from_ms: i64) -> Option<i64> {
        let cfg = &self.config.jitter;
        let tz = job.schedule.tz.as_deref();
        let is_recurring = !job.is_one_shot();

        let base = match job.schedule.kind {
            ScheduleKind::At => {
                let at_str = job.schedule.at.as_deref()?;
                let ms = parse_iso8601_to_ms(at_str)?;
                if ms > from_ms { Some(ms) } else { None }
            }
            ScheduleKind::Every => {
                let every_ms = job.schedule.every_ms?;
                let anchor = job.schedule.anchor_ms.unwrap_or(job.created_at_ms);
                next_every_run_ms(every_ms, anchor, from_ms)
            }
            ScheduleKind::Cron => {
                let expr = job.schedule.expr.as_deref()?;
                if is_recurring {
                    jittered_next_cron_run_ms_tz(expr, from_ms, &job.id, cfg, tz)
                } else {
                    one_shot_jittered_next_cron_run_ms_tz(expr, from_ms, &job.id, cfg, tz)
                }
            }
        };

        // 应用错误退避
        let backoff = backoff_ms(job.state.consecutive_errors.unwrap_or(0));
        base.map(|t| t + backoff)
    }

    /// 验证 `CronJob` 的 schedule 配置是否有效
    fn is_valid_schedule(job: &CronJob) -> bool {
        match job.schedule.kind {
            ScheduleKind::Cron => job
                .schedule
                .expr
                .as_deref()
                .is_some_and(|e| parse_cron(e).is_some()),
            ScheduleKind::At => job
                .schedule
                .at
                .as_deref()
                .is_some_and(|a| parse_iso8601_to_ms(a).is_some()),
            ScheduleKind::Every => job.schedule.every_ms.is_some_and(|ms| ms > 0),
        }
    }

    /// 加载任务
    async fn load_jobs(&mut self, initial: bool) {
        let store = task_store::read_store(&self.dir).await;

        // 验证 schedule 配置
        self.jobs = store
            .jobs
            .into_iter()
            .filter(|j| {
                if Self::is_valid_schedule(j) {
                    true
                } else {
                    tracing::warn!(id = %j.id, kind = ?j.schedule.kind, "跳过无效调度配置");
                    false
                }
            })
            .collect();

        tracing::debug!(count = self.jobs.len(), initial, "加载任务");

        // 历史归一：早期入口创建的 At 任务可能 delete_after_run == None/Some(false)。
        // At 天然 one-shot，归一为 Some(true) 并在变更时写回，避免僵尸任务。
        let mut normalized = false;
        for job in &mut self.jobs {
            if job.schedule.kind == ScheduleKind::At && job.delete_after_run != Some(true) {
                job.delete_after_run = Some(true);
                normalized = true;
            }
        }
        if normalized && let Err(e) = self.persist_jobs().await {
            tracing::warn!("At 任务归一持久化失败（内存已归一，下次 persist 重试）: {e}");
        }

        if initial {
            self.detect_missed_jobs().await;
        }
    }

    /// 检测并通知 missed tasks
    ///
    /// W-CRON-AUTOMATION-E2E P7：对每个 missed one-shot 任务，算出它原本应
    /// 触发的时刻并判定是否落在 `MISSED_CATCHUP_WINDOW_MS` 补投窗口内，把
    /// 这份明细（`MissedJobInfo`）经 `missed_tx` 送给 daemon 进程 ——
    /// daemon 据此把事件写进持久 outbox（窗口内 → `Fired` 晚到补投；
    /// 窗口外 → `Skipped`）。任务本身仍如旧逻辑从内存 + 文件删除，但"它本该
    /// 触发过"这件事现在耐久落盘，不再随删除静默丢失。
    async fn detect_missed_jobs(&mut self) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        // detailed 形态：(id, scheduled_at_ms)。检出逻辑与 find_missed_jobs 一致。
        let detailed = crate::jitter::find_missed_jobs_detailed(&self.jobs, now_ms);
        let scheduled_by_id: HashMap<String, i64> = detailed.into_iter().collect();

        let missed: Vec<MissedJobInfo> = self
            .jobs
            .iter()
            .filter_map(|j| {
                let scheduled_at_ms = *scheduled_by_id.get(&j.id)?;
                if self.missed_asked.contains(&j.id) || !self.passes_filter(j) {
                    return None;
                }
                Some(MissedJobInfo {
                    job: j.clone(),
                    scheduled_at_ms,
                    within_catchup_window: now_ms.saturating_sub(scheduled_at_ms)
                        <= crate::outbox::MISSED_CATCHUP_WINDOW_MS,
                })
            })
            .collect();

        if missed.is_empty() {
            return;
        }

        for m in &missed {
            let _ = self.missed_asked.insert(m.job.id.clone());
            let _ = self.next_fire_at.insert(m.job.id.clone(), i64::MAX);
        }

        tracing::info!(count = missed.len(), "检测到 missed one-shot 任务");

        if let Some(ref tx) = self.missed_tx {
            let _ = tx
                .send(MissedJobsEvent {
                    jobs: missed.clone(),
                })
                .await;
        }

        // 从内存 + 文件删除 missed one-shot 任务（走统一 persist 出口）
        let missed_ids_set: HashSet<String> = missed.iter().map(|m| m.job.id.clone()).collect();
        self.jobs.retain(|j| !missed_ids_set.contains(&j.id));
        if let Err(e) = self.persist_jobs().await {
            tracing::warn!("删除 missed 任务持久化失败: {e}");
        }
    }

    /// `persist_jobs` 是 daemon 向 `scheduled_tasks.json` 写盘的唯一出口。
    ///
    /// 2026-04-21 根因修复：所有写盘（tick 末尾 recurring 状态更新 / one-shot
    /// 删除 / missed 删除 / CRUD 命令）统一走这里，保证序列化自 self.jobs
    /// 当下快照，且与主循环串行执行（因为命令分支和 tick 分支互斥）。
    ///
    /// 不再 clone self.jobs 再写 —— 直接构造 `CronStoreFile` 引用 self.jobs
    /// 的所有权借用，减少一次大 Vec 拷贝。
    async fn persist_jobs(&self) -> std::io::Result<()> {
        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: self.jobs.clone(),
        };
        task_store::write_store(&self.dir, &store).await
    }

    /// 处理一条 CronCommand（add/remove/update/list/status）。
    /// 主循环在两个 tick 之间串行调用，保证 self.jobs 和磁盘一致。
    async fn handle_command(&mut self, cmd: CronCommand) {
        match cmd {
            CronCommand::Add { input, reply } => {
                let job = input.into_job();
                let id = job.id.clone();
                self.jobs.push(job);
                // 新 job 需要让 check() 下次 tick 重算 next_fire_at —— 不预插。
                match self.persist_jobs().await {
                    Ok(()) => {
                        let _ = reply.send(Ok(CronAddResult { id }));
                    }
                    Err(e) => {
                        // 回滚内存变更，保持磁盘/内存一致
                        self.jobs.pop();
                        let _ = reply.send(Err(CronError::Persist(e.to_string())));
                    }
                }
            }
            CronCommand::Remove { id, reply } => {
                let before = self.jobs.len();
                self.jobs.retain(|j| j.id != id);
                if self.jobs.len() == before {
                    let _ = reply.send(Err(CronError::NotFound(id)));
                    return;
                }
                let _ = self.next_fire_at.remove(&id);
                let _ = self.in_flight.remove(&id);
                let _ = self.missed_asked.remove(&id);
                match self.persist_jobs().await {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(CronError::Persist(e.to_string())));
                    }
                }
            }
            CronCommand::Update { id, patch, reply } => {
                match self.jobs.iter_mut().find(|j| j.id == id) {
                    Some(job) => {
                        patch.apply_to(job);
                        // patch 可能改了 schedule —— 清掉 next_fire_at 让下 tick 重算
                        let _ = self.next_fire_at.remove(&id);
                        match self.persist_jobs().await {
                            Ok(()) => {
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(CronError::Persist(e.to_string())));
                            }
                        }
                    }
                    None => {
                        let _ = reply.send(Err(CronError::NotFound(id)));
                    }
                }
            }
            CronCommand::List {
                include_disabled,
                reply,
            } => {
                let jobs = if include_disabled {
                    self.jobs.clone()
                } else {
                    self.jobs.iter().filter(|j| j.enabled).cloned().collect()
                };
                let _ = reply.send(jobs);
            }
            CronCommand::Status { reply } => {
                let enabled_count = self.jobs.iter().filter(|j| j.enabled).count();
                let _ = reply.send(CronStoreStatus {
                    version: STORE_VERSION,
                    job_count: self.jobs.len(),
                    enabled_count,
                });
            }
            CronCommand::Run { id, reply } => self.handle_run(id, reply).await,
            CronCommand::Runs { id, limit, reply } => self.handle_runs(id, limit, reply).await,
        }
    }

    /// `Run` 命令处理：手动触发某 cron job —— 直接 push `JobFiredEvent` 走与
    /// schedule 触发同路径（含 `fire_log` 落盘 + state 时间戳），但不更新磁盘
    /// state、不删 one-shot —— 让正常 schedule 继续 tick。
    async fn handle_run(&self, id: String, reply: oneshot::Sender<Result<(), CronError>>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let Some(job) = self.jobs.iter().find(|j| j.id == id).cloned() else {
            let _ = reply.send(Err(CronError::NotFound(id)));
            return;
        };
        let mut fired_job = job;
        fired_job.state.running_at_ms = Some(now_ms);
        fired_job.state.last_run_at_ms = Some(now_ms);
        if let Some(entry) = fire_log::FireLogEntry::from_job(&fired_job, now_ms) {
            fire_log::append(&self.dir, &entry).await;
        }
        // Manual fire has no theoretical schedule instant → carry the wall-clock
        // trigger time (now_ms); reason = Manual (no advance, no delete downstream).
        if let Err(e) = self
            .event_tx
            .send(JobFiredEvent {
                job: fired_job,
                scheduled_at_ms: now_ms,
                reason: FireReason::Manual,
            })
            .await
        {
            let _ = reply.send(Err(CronError::Persist(format!("emit fired event: {e}"))));
            return;
        }
        let _ = reply.send(Ok(()));
    }

    /// `Runs` 命令处理：从 `fire_log` JSONL 过滤 `job_id` 后取末尾 N 条。
    /// 文件不存在视作零条历史，不报错（首次启动 / 从未触发都正常）。
    async fn handle_runs(
        &self,
        id: String,
        limit: usize,
        reply: oneshot::Sender<Vec<fire_log::FireLogEntry>>,
    ) {
        let path = fire_log::fire_log_path(&self.dir);
        let mut entries: Vec<fire_log::FireLogEntry> = Vec::new();
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<fire_log::FireLogEntry>(trimmed)
                    && entry.job_id == id
                {
                    entries.push(entry);
                }
            }
        }
        if entries.len() > limit {
            let skip = entries.len() - limit;
            entries.drain(..skip);
        }
        let _ = reply.send(entries);
    }

    /// `check()` — 核心调度循环
    ///
    /// 支持三种 schedule kind (at/every/cron) + enabled/disabled + 错误退避 + 状态跟踪。
    /// 拆分调度→触发→持久化三段会丢失对 `self.jobs` / `next_fire_at` /
    /// `in_flight` 的共享借用语义，反而增加 `BorrowMut` 冲突；保持单函数 + allow。
    #[allow(clippy::too_many_lines)]
    async fn check(&mut self) {
        if !self.is_owner {
            return;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let cfg = &self.config.jitter;
        let mut seen = HashSet::new();

        let mut fired_recurring: Vec<String> = Vec::new();
        let mut delete_ids: Vec<String> = Vec::new();
        let mut events: Vec<JobFiredEvent> = Vec::new();

        for job in &self.jobs {
            // enabled 过滤
            if !job.enabled {
                continue;
            }
            if !self.passes_filter(job) {
                continue;
            }
            let _ = seen.insert(job.id.clone());

            // In-flight guard
            if self.in_flight.contains(&job.id) {
                continue;
            }

            let is_recurring = !job.is_one_shot();

            // First-sight: 计算下次触发时间（避免 entry() 借用冲突）
            if !self.next_fire_at.contains_key(&job.id) {
                let anchor = job.state.last_run_at_ms.unwrap_or(job.created_at_ms);
                let fire_ms = self.next_fire_ms_for_job(job, anchor).unwrap_or(i64::MAX);
                tracing::debug!(
                    id = %job.id,
                    kind = ?job.schedule.kind,
                    fire_at = if fire_ms == i64::MAX {
                        "never".to_string()
                    } else {
                        chrono::DateTime::from_timestamp_millis(fire_ms)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default()
                    },
                    "调度任务"
                );
                let _ = self.next_fire_at.insert(job.id.clone(), fire_ms);
            }
            let next = self.next_fire_at.get(&job.id).copied().unwrap_or(i64::MAX);

            if now_ms < next {
                continue;
            }

            // 触发！
            let aged = is_recurring_task_aged(
                is_recurring,
                job.permanent == Some(true),
                job.created_at_ms,
                now_ms,
                cfg.recurring_max_age_ms,
            );

            if aged {
                tracing::info!(
                    id = %job.id,
                    age_hours = (now_ms - job.created_at_ms) / 1000 / 60 / 60,
                    "周期任务过期，最后一次触发后删除"
                );
            }

            tracing::info!(
                id = %job.id,
                kind = ?job.schedule.kind,
                recurring = is_recurring,
                "触发任务"
            );

            // 构造触发事件（携带完整 CronJob 供路由）
            let mut fired_job = job.clone();
            fired_job.state.running_at_ms = Some(now_ms);
            fired_job.state.last_run_at_ms = Some(now_ms);
            let reason = if is_recurring && !aged {
                FireReason::ScheduledRecurring
            } else {
                FireReason::ScheduledFinal
            };
            // `next` (daemon.rs:799) is the theoretical due instant — restart-stable,
            // so a crash-replay produces the same occurrence key. NOT `now_ms`.
            events.push(JobFiredEvent {
                job: fired_job,
                scheduled_at_ms: next,
                reason,
            });

            if is_recurring && !aged {
                // Recurring: reschedule from now
                let new_next = self.next_fire_ms_for_job(job, now_ms).unwrap_or(i64::MAX);
                let _ = self.next_fire_at.insert(job.id.clone(), new_next);
                fired_recurring.push(job.id.clone());
            } else {
                // One-shot / aged-out / deleteAfterRun / at schedule
                let _ = self.in_flight.insert(job.id.clone());
                delete_ids.push(job.id.clone());
                let _ = self.next_fire_at.remove(&job.id);
            }
        }

        // 发送触发事件
        for event in events {
            // v3: 若为会话续写任务，先落盘日志（文件 bus → Go fsnotify）。
            // 失败仅记 error，不影响内存通道。
            if let Some(entry) = fire_log::FireLogEntry::from_job(&event.job, now_ms) {
                fire_log::append(&self.dir, &entry).await;
            }
            if let Err(e) = self.event_tx.send(event).await {
                tracing::warn!("发送触发事件失败: {e}");
            }
        }

        // 批量更新 recurring 任务状态（修改 self.jobs 内的 state.last_run_at_ms，然后 persist）
        if !fired_recurring.is_empty() {
            for id in &fired_recurring {
                let _ = self.in_flight.insert(id.clone());
            }
            // 直接在 self.jobs 上原地更新 state
            let fired_set: HashSet<&String> = fired_recurring.iter().collect();
            for job in &mut self.jobs {
                if fired_set.contains(&job.id) {
                    job.state.last_run_at_ms = Some(now_ms);
                    job.state.running_at_ms = Some(now_ms);
                }
            }
            if let Err(e) = self.persist_jobs().await {
                tracing::warn!(
                    "持久化 recurring 状态失败（内存已更新，磁盘滞后，重启可能重复触发）: {e}"
                );
            }
            for id in &fired_recurring {
                let _ = self.in_flight.remove(id);
            }
        }

        // 删除 one-shot / aged-out 任务
        if !delete_ids.is_empty() {
            let delete_set: HashSet<&String> = delete_ids.iter().collect();
            self.jobs.retain(|j| !delete_set.contains(&j.id));
            if let Err(e) = self.persist_jobs().await {
                tracing::warn!(
                    "删除任务持久化失败（内存已更新，磁盘滞后，重启可能重现已删任务）: {e}"
                );
            }
            for id in &delete_ids {
                let _ = self.in_flight.remove(id);
            }
        }

        // 清理不再存在的任务的 nextFireAt
        if seen.is_empty() {
            self.next_fire_at.clear();
        } else {
            self.next_fire_at.retain(|id, _| seen.contains(id));
        }
    }
}

// 2026-04-21 文件监视已删除：
// 原实现通过 notify crate 监视 scheduled_tasks.json 变更，配合 reload_flag +
// clear_flag 让 daemon 在下次 tick 重读文件。这是为了兼容 Go gateway
// 同时写文件的旧架构。根因重构后，Rust 独占写入所有 CRUD（经 CronCommand
// 主循环串行处理 + persist_jobs 统一出口），不再需要反向同步。外部写入
// 一律视为错误（Go 侧已切 UDS RPC，任何非 daemon 的直接写都是 bug）。

// ============================================================================
// 测试辅助
// ============================================================================

#[cfg(test)]
use crate::task_store::{
    CronJobState, CronPayload, CronSchedule, PayloadKind, SessionTarget, WakeMode,
};

#[cfg(test)]
fn test_cron_job(
    id: &str,
    cron_expr: &str,
    prompt: &str,
    created_at: i64,
    recurring: bool,
) -> CronJob {
    CronJob {
        id: id.into(),
        agent_id: None,
        name: prompt.into(),
        description: None,
        owner: None,
        enabled: true,
        delete_after_run: Some(!recurring),
        created_at_ms: created_at,
        updated_at_ms: created_at,
        schedule: CronSchedule {
            kind: ScheduleKind::Cron,
            at: None,
            every_ms: None,
            anchor_ms: None,
            expr: Some(cron_expr.into()),
            tz: None,
        },
        session_target: SessionTarget::Main,
        wake_mode: WakeMode::NextHeartbeat,
        payload: CronPayload {
            kind: PayloadKind::SystemEvent,
            text: Some(prompt.into()),
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
    }
}

#[cfg(test)]
fn test_every_job(id: &str, every_ms: i64, anchor_ms: i64, created_at: i64) -> CronJob {
    CronJob {
        id: id.into(),
        agent_id: None,
        name: "every job".into(),
        description: None,
        owner: None,
        enabled: true,
        delete_after_run: Some(false),
        created_at_ms: created_at,
        updated_at_ms: created_at,
        schedule: CronSchedule {
            kind: ScheduleKind::Every,
            at: None,
            every_ms: Some(every_ms),
            anchor_ms: Some(anchor_ms),
            expr: None,
            tz: None,
        },
        session_target: SessionTarget::Main,
        wake_mode: WakeMode::NextHeartbeat,
        payload: CronPayload {
            kind: PayloadKind::SystemEvent,
            text: Some("every".into()),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store;

    #[test]
    fn default_config_is_sane() {
        let config = SchedulerConfig::default();
        assert_eq!(config.check_interval, Duration::from_secs(1));
        assert_eq!(config.file_stability, Duration::from_millis(300));
    }

    #[test]
    fn filter_mode_permanent_only() {
        let config = SchedulerConfig {
            filter_mode: Some(FilterMode::PermanentOnly),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(PathBuf::from("/tmp"), config, "test".into(), tx);

        let mut permanent = test_cron_job("p", "* * * * *", "", 0, true);
        permanent.permanent = Some(true);
        let normal = test_cron_job("n", "* * * * *", "", 0, true);

        assert!(daemon.passes_filter(&permanent));
        assert!(!daemon.passes_filter(&normal));
    }

    #[test]
    fn get_next_fire_time_empty() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );
        assert_eq!(daemon.get_next_fire_time(), None);
    }

    #[test]
    fn get_next_fire_time_ignores_infinity() {
        let (tx, _rx) = mpsc::channel(1);
        let mut daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );
        let _ = daemon.next_fire_at.insert("a".into(), i64::MAX);
        let _ = daemon.next_fire_at.insert("b".into(), 5000);
        assert_eq!(daemon.get_next_fire_time(), Some(5000));
    }

    #[test]
    fn backoff_calculation() {
        assert_eq!(backoff_ms(0), 0);
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(3), 300_000);
        assert_eq!(backoff_ms(6), 3_600_000);
        // 超过最大级别 → 上限
        assert_eq!(backoff_ms(100), 3_600_000);
    }

    #[test]
    fn valid_schedule_checks() {
        let cron_job = test_cron_job("c", "*/5 * * * *", "test", 1000, true);
        assert!(SchedulerDaemon::is_valid_schedule(&cron_job));

        let mut invalid_cron = test_cron_job("c", "invalid", "test", 1000, true);
        invalid_cron.schedule.expr = Some("not valid".into());
        assert!(!SchedulerDaemon::is_valid_schedule(&invalid_cron));

        let every_job = test_every_job("e", 60000, 1000, 1000);
        assert!(SchedulerDaemon::is_valid_schedule(&every_job));

        let mut invalid_every = test_every_job("e", 0, 1000, 1000);
        invalid_every.schedule.every_ms = Some(0);
        assert!(!SchedulerDaemon::is_valid_schedule(&invalid_every));
    }

    #[test]
    fn disabled_jobs_skipped_in_filter() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );
        let mut job = test_cron_job("d", "* * * * *", "test", 0, true);
        // enabled=true → passes
        assert!(daemon.passes_filter(&job));
        // Note: enabled check is in check(), not passes_filter
        job.enabled = false;
        // passes_filter doesn't check enabled (that's in check())
        assert!(daemon.passes_filter(&job));
    }

    /// PR-B / Finding A 修复测试辅助: 收集 sink 调用序列的 MockSink。
    #[derive(Default)]
    struct MockSink {
        calls: std::sync::Mutex<Vec<bool>>,
    }

    impl MockSink {
        fn calls(&self) -> Vec<bool> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SchedulerStatusSink for MockSink {
        fn set_is_owner(&self, is_owner: bool) {
            self.calls.lock().unwrap().push(is_owner);
        }
    }

    /// PR-B 回写点 ① + ④: 初始抢锁成功 + 关停释放 → sink 至少收到 true 和 false。
    /// tempdir 下无竞争,初始抢锁必成。
    #[tokio::test]
    async fn status_sink_records_initial_and_shutdown() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel(16);
        let sink = Arc::new(MockSink::default());

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-sink-initial".into(),
            tx,
        )
        .with_status_sink(Arc::clone(&sink) as Arc<dyn SchedulerStatusSink>);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move { daemon.run(shutdown_clone).await });

        // 等主循环进入,再 cancel
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("daemon 应在 5s 内退出");

        let calls = sink.calls();
        assert!(!calls.is_empty(), "sink 至少应收到初始抢锁结果");
        assert!(
            *calls.first().unwrap(),
            "回写点 ①: 初始抢锁 tempdir 应成功 → true"
        );
        assert!(!*calls.last().unwrap(), "回写点 ④: 关停后必发 false");
    }

    /// Step 2 Phase D.5 / Step 1 §六 R1 ④ + §一 P1-1 regression:
    /// when `try_acquire_lock` returns `Err(IoError)` (disk full / permission
    /// denied / "not a directory" / OOM), `run()` must propagate that Err
    /// rather than collapsing it into `is_owner = false` via `unwrap_or(false)`.
    ///
    /// Setup: pre-create a regular *file* at the path the daemon will treat
    /// as its scheduler directory. `try_acquire_lock` -> `fs::create_dir_all`
    /// fails with `NotADirectory`-class error, exercising the IO-error
    /// branch. The earlier `unwrap_or(false)` would have silently degraded
    /// to non-owner mode; the fix returns the io error.
    #[tokio::test]
    async fn lock_io_error_distinct_from_occupied() {
        let parent = tempfile::tempdir().expect("tmpdir");
        // The "scheduler dir" path is actually a regular file. Any attempt
        // to create_dir_all the lock-file's parent under it will fail.
        let scheduler_dir = parent.path().join("not-a-dir");
        std::fs::write(&scheduler_dir, b"i am a regular file").expect("create blocker file");

        let (tx, _rx) = mpsc::channel(16);
        let mut daemon = SchedulerDaemon::new(
            scheduler_dir,
            SchedulerConfig::default(),
            "test-io-err".into(),
            tx,
        );

        let result = daemon.run(CancellationToken::new()).await;
        let err = result.expect_err("IO error must propagate, not collapse to is_owner=false");
        // Regression invariant: any io::Error variant is acceptable here —
        // the contract is "do not silently swallow", not a specific kind.
        let kind = err.kind();
        assert!(
            matches!(
                kind,
                std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Other
                    | std::io::ErrorKind::InvalidInput
            ),
            "expected an io error from create_dir_all on a non-directory path, got {kind:?}"
        );
    }

    /// PR-B 回写点 ②: shutdown 已 cancel 即开启 run() → 早退分支发 false。
    #[tokio::test]
    async fn status_sink_records_shutdown_early_exit() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel(16);
        let sink = Arc::new(MockSink::default());

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-sink-early".into(),
            tx,
        )
        .with_status_sink(Arc::clone(&sink) as Arc<dyn SchedulerStatusSink>);

        let shutdown = CancellationToken::new();
        shutdown.cancel(); // 启动前就取消

        daemon.run(shutdown).await.expect("run 应返 Ok");

        let calls = sink.calls();
        // 回写点 ① (初始抢锁, tempdir 成功 true) + 回写点 ② (早退发 false)
        assert_eq!(
            calls.len(),
            2,
            "早退路径应发 2 次: init + shutdown (got {:?})",
            calls
        );
        assert!(calls[0], "初始抢锁 tempdir 成功");
        assert!(!calls[1], "早退必发 false");
    }

    /// PR-B 回写点 ③: 第二 daemon 抢锁失败 (竞争) 时 sink 收到 false,
    /// 后续关停 publish false 再来一次,不 publish true。
    #[tokio::test]
    async fn status_sink_reflects_lock_contention() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx1, _rx1) = mpsc::channel(16);
        let (tx2, _rx2) = mpsc::channel(16);

        // 先让 daemon1 拿到锁并保持运行
        let sink1 = Arc::new(MockSink::default());
        let mut daemon1 = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-sink-owner".into(),
            tx1,
        )
        .with_status_sink(Arc::clone(&sink1) as Arc<dyn SchedulerStatusSink>);
        let shutdown1 = CancellationToken::new();
        let shutdown1_clone = shutdown1.clone();
        let handle1 = tokio::spawn(async move { daemon1.run(shutdown1_clone).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // daemon2 同目录抢锁应失败
        let sink2 = Arc::new(MockSink::default());
        let mut daemon2 = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-sink-contender".into(),
            tx2,
        )
        .with_status_sink(Arc::clone(&sink2) as Arc<dyn SchedulerStatusSink>);
        let shutdown2 = CancellationToken::new();
        let shutdown2_clone = shutdown2.clone();
        let handle2 = tokio::spawn(async move { daemon2.run(shutdown2_clone).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown2.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle2)
            .await
            .expect("daemon2 应及时退出");

        // daemon2 从未抢到锁 —— sink 收到的所有值都应是 false
        let calls2 = sink2.calls();
        assert!(!calls2.is_empty(), "抢锁失败 daemon 也应回写 ≥1 次");
        assert!(
            calls2.iter().all(|&v| !v),
            "抢锁失败期间 sink 不应出现 true (got {:?})",
            calls2
        );

        // 清理 daemon1
        shutdown1.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle1).await;
        let calls1 = sink1.calls();
        assert!(*calls1.first().unwrap(), "daemon1 初始抢锁应成功");
    }

    /// PR-B: ChannelStatusSink buffer full 时丢值但不 panic。
    #[test]
    fn channel_sink_drops_silently_when_full() {
        let (tx, _rx) = mpsc::channel::<bool>(1);
        let sink = ChannelStatusSink::new(tx);
        // 第一次 try_send 成功;第二次 buffer 满,丢弃但不 panic
        sink.set_is_owner(true);
        sink.set_is_owner(false); // 不 panic
    }

    #[tokio::test]
    async fn daemon_starts_and_stops() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel(16);
        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-daemon".into(),
            tx,
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(async move { daemon.run(shutdown_clone).await });
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "daemon 应及时退出");
    }

    #[tokio::test]
    async fn check_fires_overdue_recurring() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(16);

        let two_min_ago = chrono::Utc::now().timestamp_millis() - 2 * 60 * 1000;
        let mut job = test_cron_job("r1", "* * * * *", "hello", two_min_ago, true);
        job.state.last_run_at_ms = Some(two_min_ago);

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job.clone()],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-check".into(),
            tx,
        );
        daemon.is_owner = true;
        daemon.jobs = vec![job];

        daemon.check().await;

        let event = rx.try_recv().expect("应收到触发事件");
        assert_eq!(event.job.id, "r1");
        assert_eq!(event.job.delete_after_run, Some(false));
        assert!(event.job.state.last_run_at_ms.is_some());
        // W1c-A: recurring 未过期 → ScheduledRecurring。
        assert_eq!(event.reason, FireReason::ScheduledRecurring);
        // 理论应触发时刻 = anchor(last_run_at_ms = two_min_ago) 之后第一个
        // "* * * * *" 网格点。jitter_frac("r1")=0（非 hex id）+ consecutive_errors=None
        // → 无 jitter、无 backoff，故等于 raw 网格点，且重启后可从 last_run_at_ms 复现。
        let expected_scheduled =
            crate::jitter::next_cron_run_ms_tz("* * * * *", two_min_ago, None).expect("grid point");
        assert_eq!(event.scheduled_at_ms, expected_scheduled);
        // 理论时刻严格早于本次墙钟触发时刻（state.last_run_at_ms = check 的 now_ms），
        // 证明 scheduled_at_ms 携带的是理论 next 而非 now_ms。
        assert!(event.scheduled_at_ms < event.job.state.last_run_at_ms.unwrap());

        // 第二次 check() 不应重复触发
        daemon.check().await;
        assert!(rx.try_recv().is_err(), "重调度后不应重复触发");

        // lastRunAtMs 已持久化
        let after = task_store::read_store(dir.path()).await;
        assert_eq!(after.jobs.len(), 1);
        assert!(after.jobs[0].state.last_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn check_fires_and_deletes_oneshot() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(16);

        let two_min_ago = chrono::Utc::now().timestamp_millis() - 2 * 60 * 1000;
        let job = test_cron_job("os1", "* * * * *", "once", two_min_ago, false);

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job.clone()],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-oneshot".into(),
            tx,
        );
        daemon.is_owner = true;
        daemon.jobs = vec![job];

        daemon.check().await;

        let event = rx.try_recv().expect("应收到触发事件");
        assert_eq!(event.job.id, "os1");
        assert_eq!(event.job.delete_after_run, Some(true));
        // W1c-A: 一次性任务 → ScheduledFinal（走删除路径）。
        assert_eq!(event.reason, FireReason::ScheduledFinal);
        // 无 last_run_at_ms → anchor = created_at = two_min_ago。one-shot 路径
        // jitter_frac("os1")=0 → lead=0 → 返回 raw 网格点，等于 next_cron_run_ms_tz。
        let expected_scheduled =
            crate::jitter::next_cron_run_ms_tz("* * * * *", two_min_ago, None).expect("grid point");
        assert_eq!(event.scheduled_at_ms, expected_scheduled);
        assert!(event.scheduled_at_ms < event.job.state.last_run_at_ms.unwrap());

        let after = task_store::read_store(dir.path()).await;
        assert!(after.jobs.is_empty(), "one-shot 应已删除");

        daemon.check().await;
        assert!(rx.try_recv().is_err());
    }

    /// 旧式服务入口创建的 `At` 任务可能 `delete_after_run == None`。
    /// `At` 天然 one-shot：overdue 时必须触发并被删除（删除路径），而非重调度。
    #[tokio::test]
    async fn check_fires_and_deletes_at_job_with_none_delete_after_run() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(16);

        let now = chrono::Utc::now().timestamp_millis();
        // 任务创建于 1 小时前；at 触发时刻在 created_at 之后、now 之前 →
        // first-sight（anchor=created_at）算出的 next 落在 now 之前 → check() 应触发。
        let one_hour_ago = now - 60 * 60 * 1000;
        // 显式 delete_after_run: None —— 模拟省略 deleteAfterRun 的入口
        let mut job = test_cron_job("at-none", "* * * * *", "at task", one_hour_ago, true);
        job.delete_after_run = None;
        job.schedule.kind = ScheduleKind::At;
        job.schedule.expr = None;
        let at_str = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        job.schedule.at = Some(at_str.clone());
        // 入口未传 deleteAfterRun 时 stored 即为 None，仍应判定 one-shot
        assert!(
            job.is_one_shot(),
            "At 任务无论 delete_after_run 都应 one-shot"
        );

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job.clone()],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-at-none".into(),
            tx,
        );
        daemon.is_owner = true;
        daemon.jobs = vec![job];

        daemon.check().await;

        let event = rx.try_recv().expect("应收到触发事件");
        assert_eq!(event.job.id, "at-none");
        // W1c-A: At 天然 one-shot → ScheduledFinal。
        assert_eq!(event.reason, FireReason::ScheduledFinal);
        // At 的理论应触发时刻 = at 字符串解析出的绝对时刻（backoff=0），
        // 与墙钟 now 无关，重启后从同一 at 字符串复现同一 occurrence key。
        let expected_at = parse_iso8601_to_ms(&at_str).expect("at 可解析");
        assert_eq!(event.scheduled_at_ms, expected_at);
        assert!(event.scheduled_at_ms < event.job.state.last_run_at_ms.unwrap());

        let after = task_store::read_store(dir.path()).await;
        assert!(
            after.jobs.is_empty(),
            "At 任务触发后应进入删除路径，不应重调度为僵尸任务"
        );

        daemon.check().await;
        assert!(rx.try_recv().is_err(), "已删除的 At 任务不应重复触发");
    }

    #[tokio::test]
    async fn check_skips_disabled_jobs() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(16);

        let two_min_ago = chrono::Utc::now().timestamp_millis() - 2 * 60 * 1000;
        let mut job = test_cron_job("d1", "* * * * *", "disabled", two_min_ago, true);
        job.enabled = false;
        job.state.last_run_at_ms = Some(two_min_ago);

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job.clone()],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-disabled".into(),
            tx,
        );
        daemon.is_owner = true;
        daemon.jobs = vec![job];

        daemon.check().await;
        assert!(rx.try_recv().is_err(), "disabled 任务不应触发");
    }

    #[tokio::test]
    async fn in_flight_prevents_double_fire() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(64);

        let two_min_ago = chrono::Utc::now().timestamp_millis() - 2 * 60 * 1000;
        let mut job = test_cron_job("r2", "* * * * *", "p", two_min_ago, true);
        job.state.last_run_at_ms = Some(two_min_ago);

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job.clone()],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-inflight".into(),
            tx,
        );
        daemon.is_owner = true;
        daemon.jobs = vec![job];

        daemon.check().await;
        assert!(rx.try_recv().is_ok());

        for _ in 0..5 {
            daemon.check().await;
        }
        let mut extra = 0;
        while rx.try_recv().is_ok() {
            extra += 1;
        }
        assert_eq!(extra, 0, "recurring 重调度后不应再触发");
    }

    #[tokio::test]
    async fn daemon_full_integration() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel(16);

        let two_min_ago = chrono::Utc::now().timestamp_millis() - 2 * 60 * 1000;
        let mut job = test_cron_job("integ-1", "* * * * *", "integration", two_min_ago, true);
        job.state.last_run_at_ms = Some(two_min_ago);

        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs: vec![job],
        };
        task_store::write_store(dir.path(), &store)
            .await
            .expect("write");

        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig {
                check_interval: Duration::from_millis(50),
                ..Default::default()
            },
            "test-integ".into(),
            tx,
        );

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move { daemon.run(shutdown_clone).await });

        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("应在 10s 内收到事件")
            .expect("channel not closed");

        assert_eq!(event.job.id, "integ-1");
        assert_eq!(event.job.delete_after_run, Some(false));
        assert!(event.job.state.running_at_ms.is_some());

        shutdown.cancel();
        let _ = handle.await;
    }

    #[test]
    fn next_fire_ms_for_every_job() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );

        let now = chrono::Utc::now().timestamp_millis();
        let job = test_every_job("ev1", 60_000, now - 90_000, now - 90_000);
        // anchor 90s ago, interval 60s → should fire at anchor + 2*60s = anchor + 120s
        let next = daemon.next_fire_ms_for_job(&job, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_fire_ms_for_at_job() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );

        let now = chrono::Utc::now().timestamp_millis();
        // Future time
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let at_str = future.to_rfc3339();

        let mut job = test_cron_job("at1", "* * * * *", "test", now, false);
        job.schedule.kind = ScheduleKind::At;
        job.schedule.expr = None;
        job.schedule.at = Some(at_str);

        let next = daemon.next_fire_ms_for_job(&job, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_fire_ms_for_at_past_returns_none() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );

        let now = chrono::Utc::now().timestamp_millis();
        let mut job = test_cron_job("at2", "* * * * *", "test", now, false);
        job.schedule.kind = ScheduleKind::At;
        job.schedule.expr = None;
        job.schedule.at = Some("2020-01-01T00:00:00+00:00".into());

        let next = daemon.next_fire_ms_for_job(&job, now);
        assert!(next.is_none());
    }

    #[test]
    fn backoff_delays_next_fire() {
        let (tx, _rx) = mpsc::channel(1);
        let daemon = SchedulerDaemon::new(
            PathBuf::from("/tmp"),
            SchedulerConfig::default(),
            "test".into(),
            tx,
        );

        let now = chrono::Utc::now().timestamp_millis();
        let mut job = test_cron_job("b1", "* * * * *", "test", now - 120_000, true);

        // No errors → no backoff
        let next_no_err = daemon.next_fire_ms_for_job(&job, now);

        // 3 consecutive errors → 5min backoff
        job.state.consecutive_errors = Some(3);
        let next_with_err = daemon.next_fire_ms_for_job(&job, now);

        assert!(next_no_err.is_some());
        assert!(next_with_err.is_some());
        assert!(
            next_with_err.unwrap() > next_no_err.unwrap(),
            "backoff should delay next fire"
        );
        assert!(
            next_with_err.unwrap() - next_no_err.unwrap() >= 300_000,
            "level 3 backoff should be >= 5min"
        );
    }

    // ════════════════════════════════════════════════════════════════════
    // 阶段 3-D 补全：handle_run / handle_runs 直接 unit 测试
    // ════════════════════════════════════════════════════════════════════
    //
    // 防的根因（F-A1 forward fix）：handle_run / handle_runs 由 IPC 层
    // sub_run / sub_runs 通过 cmd channel 间接调用，IPC 测试覆盖了路由 +
    // cmd 透传，但**不能**断 daemon 内部行为：
    //   - JobFiredEvent 是否真 push 到 event_tx
    //   - fire_log 是否真 append 到磁盘
    //   - state.last_run_at_ms / running_at_ms 是否更新到事件副本
    //   - NotFound 路径 reply 是否携带正确 id
    //   - runs 是否真按 job_id 过滤
    //   - runs 末尾 N 条切片是否正确（drain..skip）
    // 这些是 IPC 测试盲区，本组测试直接打 daemon 私有方法补齐。

    /// 构造一个 Continuation-session job（fire_log 落盘的前置条件）。
    /// `FireLogEntry::from_job` 仅对 `SessionTarget::Continuation` + 非空
    /// `session_key` + 非空 message/text 返回 Some；其它情况 daemon 静默跳过
    /// fire_log（Main-session 的 fire 由活动 TS session 直接消费，不需要恢复
    /// 日志）。本 helper 让 handle_run / handle_runs 测试能命中真实落盘路径。
    fn continuation_job(id: &str, session_key: &str) -> CronJob {
        let now = chrono::Utc::now().timestamp_millis();
        let mut job = test_cron_job(id, "* * * * *", "p", now - 60_000, true);
        job.session_target = SessionTarget::Continuation;
        job.session_key = Some(session_key.into());
        job.continuation_kind = Some("chat".into());
        job.payload.message = Some("hello".into());
        job
    }

    /// `handle_run`：命中已有 Continuation job → reply Ok + event_tx 收到事件 +
    /// fire_log 写盘 + 不污染磁盘 state（仅 clone）。
    #[tokio::test]
    async fn handle_run_emits_event_for_existing_job() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel::<JobFiredEvent>(8);
        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-handle-run".into(),
            tx,
        );
        let job = continuation_job("job-1", "feishu:x");
        daemon.jobs.push(job);

        let (reply_tx, reply_rx) = oneshot::channel();
        daemon.handle_run("job-1".into(), reply_tx).await;

        // reply Ok
        let reply = reply_rx.await.expect("reply 必收到");
        assert!(reply.is_ok(), "应 Ok，实际 {reply:?}");

        // event_tx 收到 JobFiredEvent，job.id 一致
        let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("等 event 不应超时")
            .expect("event_tx 应收到事件");
        assert_eq!(event.job.id, "job-1");
        // 事件副本携带本次 fire 时间戳
        assert!(event.job.state.running_at_ms.is_some());
        assert!(event.job.state.last_run_at_ms.is_some());
        // W1c-A: 手动触发 → Manual；无理论调度时刻，scheduled_at_ms = 墙钟触发时刻
        // （now_ms），与 fired_job.state.last_run_at_ms 同源，二者相等且 > 0。
        assert_eq!(event.reason, FireReason::Manual);
        assert_eq!(
            event.scheduled_at_ms,
            event.job.state.last_run_at_ms.expect("last_run_at_ms")
        );
        assert!(event.scheduled_at_ms > 0);

        // fire_log 文件已落盘
        let log_path = fire_log::fire_log_path(dir.path());
        assert!(log_path.exists(), "fire_log 应存在: {}", log_path.display());
        let raw = tokio::fs::read_to_string(&log_path)
            .await
            .expect("read log");
        let line = raw.lines().next().expect("至少一行");
        let entry: fire_log::FireLogEntry = serde_json::from_str(line).expect("entry 可反序列化");
        assert_eq!(entry.job_id, "job-1");

        // 不影响磁盘 state — daemon.jobs[0] 的 state.last_run_at_ms 未变（修改的是 clone）
        assert!(daemon.jobs[0].state.last_run_at_ms.is_none());
    }

    /// `handle_run`：命中 Main-session job → reply Ok + event_tx 收事件，但
    /// fire_log **不**落盘（与 schedule-trigger 同语义；Main 由活 TS 直接消费）。
    /// 防止「我以为 cron.run 必写 fire_log」的误解，固化此行为契约。
    #[tokio::test]
    async fn handle_run_skips_fire_log_for_main_session() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel::<JobFiredEvent>(8);
        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-main-skip".into(),
            tx,
        );
        let now = chrono::Utc::now().timestamp_millis();
        // test_cron_job 默认 SessionTarget::Main
        daemon.jobs.push(test_cron_job(
            "main-1",
            "* * * * *",
            "p",
            now - 60_000,
            true,
        ));

        let (reply_tx, reply_rx) = oneshot::channel();
        daemon.handle_run("main-1".into(), reply_tx).await;
        assert!(reply_rx.await.expect("reply 必收").is_ok());

        // 事件仍发（TS 直接消费）
        let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("不应超时")
            .expect("应有事件");
        assert_eq!(event.job.id, "main-1");

        // fire_log 文件不应存在（Main-session 路径）
        assert!(
            !fire_log::fire_log_path(dir.path()).exists(),
            "Main-session run 不应写 fire_log"
        );
    }

    /// `handle_run`：未找到 job → reply Err(NotFound) + event_tx 不收事件 +
    /// fire_log 不写盘
    #[tokio::test]
    async fn handle_run_returns_not_found_for_missing_id() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, mut rx) = mpsc::channel::<JobFiredEvent>(8);
        let daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-not-found".into(),
            tx,
        );

        let (reply_tx, reply_rx) = oneshot::channel();
        daemon.handle_run("ghost".into(), reply_tx).await;

        let reply = reply_rx.await.expect("reply 必收到");
        match reply {
            Err(CronError::NotFound(id)) => assert_eq!(id, "ghost"),
            other => panic!("期望 NotFound(ghost)，实际 {other:?}"),
        }

        // 没事件 / 没文件
        assert!(rx.try_recv().is_err(), "event_tx 不应有事件");
        assert!(
            !fire_log::fire_log_path(dir.path()).exists(),
            "fire_log 不应被写"
        );
    }

    /// `handle_runs`：从 fire_log 过滤 `job_id` 并 take 末尾 N 条
    #[tokio::test]
    async fn handle_runs_filters_by_id_and_takes_last_n() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<JobFiredEvent>(8);
        let daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-runs".into(),
            tx,
        );

        // 手工写 5 条 entries 到 fire_log：3×A + 2×B
        let mk_entry = |job_id: &str, fire_ts_ms: i64| fire_log::FireLogEntry {
            job_id: job_id.into(),
            fire_ts_ms,
            session_key: "sk".into(),
            channel_id: None,
            continuation_kind: "chat".into(),
            message: format!("{job_id}@{fire_ts_ms}"),
            title: None,
        };
        for entry in &[
            mk_entry("A", 1000),
            mk_entry("B", 1100),
            mk_entry("A", 1200),
            mk_entry("B", 1300),
            mk_entry("A", 1400),
        ] {
            fire_log::append(dir.path(), entry).await;
        }

        // limit=2 → 取 A 的最后两条 (1200, 1400)
        let (reply_tx, reply_rx) = oneshot::channel();
        daemon.handle_runs("A".into(), 2, reply_tx).await;
        let entries = reply_rx.await.expect("reply 必收到");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.job_id == "A"));
        assert_eq!(entries[0].fire_ts_ms, 1200);
        assert_eq!(entries[1].fire_ts_ms, 1400);

        // limit=10（超过 A 总数）→ 全部 3 条 A
        let (reply_tx2, reply_rx2) = oneshot::channel();
        daemon.handle_runs("A".into(), 10, reply_tx2).await;
        let entries2 = reply_rx2.await.expect("reply 必收到");
        assert_eq!(entries2.len(), 3);
    }

    /// `handle_runs`：fire_log 文件不存在 → 返回空列表（首次启动 / 从未触发都
    /// 是这条路径，不应报错）
    #[tokio::test]
    async fn handle_runs_returns_empty_when_log_missing() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<JobFiredEvent>(8);
        let daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-runs-empty".into(),
            tx,
        );

        // 不写任何 fire_log；直接查
        assert!(!fire_log::fire_log_path(dir.path()).exists());

        let (reply_tx, reply_rx) = oneshot::channel();
        daemon.handle_runs("any".into(), 50, reply_tx).await;
        let entries = reply_rx.await.expect("reply 必收到");
        assert!(entries.is_empty());
    }

    // ════════════════════════════════════════════════════════════════════
    // W-CRON-AUTOMATION-E2E P7：detect_missed_jobs → MissedJobsEvent 分类
    // ════════════════════════════════════════════════════════════════════
    //
    // 防的根因：missed one-shot 任务被 detect_missed_jobs 检出后**直接删除**，
    // 改造前连"它本该触发过"都无据可查。P7 要求 detect_missed_jobs 把每个
    // missed 任务的计划触发时刻 + 补投窗口分类经 missed_tx 报给 daemon 进程
    // （由 daemon 写持久 outbox）。本组测试直接打 detect_missed_jobs 私有方法，
    // 断 MissedJobsEvent 的 scheduled_at_ms / within_catchup_window 分类。

    /// 构造一个 `At` one-shot job，at 时刻为相对 now 的偏移量（负=过去）。
    fn at_job_offset(id: &str, offset_ms: i64) -> CronJob {
        let now = chrono::Utc::now().timestamp_millis();
        let at_iso = chrono::DateTime::from_timestamp_millis(now + offset_ms)
            .expect("ts→DateTime")
            .to_rfc3339();
        let mut job = test_cron_job(id, "* * * * *", "missed-task", 1000, false);
        job.schedule.kind = ScheduleKind::At;
        job.schedule.expr = None;
        job.schedule.at = Some(at_iso);
        job.delete_after_run = Some(true);
        job
    }

    /// missed one-shot 的 at 时刻在补投窗口内 → MissedJobInfo
    /// within_catchup_window == true，scheduled_at_ms 与 at 时刻一致。
    #[tokio::test]
    async fn detect_missed_within_window_classified_as_catchup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<JobFiredEvent>(8);
        let (mtx, mut mrx) = mpsc::channel::<MissedJobsEvent>(8);
        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-missed-window".into(),
            tx,
        );
        daemon.set_missed_tx(mtx);
        // at 时刻在 1 小时前 → 远小于 24h 补投窗口
        let job = at_job_offset("missed-recent", -60 * 60 * 1000);
        daemon.jobs = vec![job];

        daemon.detect_missed_jobs().await;

        let ev = mrx.try_recv().expect("应收到 MissedJobsEvent");
        assert_eq!(ev.jobs.len(), 1);
        let info = &ev.jobs[0];
        assert_eq!(info.job.id, "missed-recent");
        assert!(
            info.within_catchup_window,
            "1h-ago missed 应落在 24h 补投窗口内"
        );
        // job 已从内存删除
        assert!(daemon.jobs.is_empty(), "missed 任务应已删除");
    }

    /// missed one-shot 的 at 时刻超出补投窗口 → within_catchup_window == false。
    #[tokio::test]
    async fn detect_missed_beyond_window_classified_as_skipped() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let (tx, _rx) = mpsc::channel::<JobFiredEvent>(8);
        let (mtx, mut mrx) = mpsc::channel::<MissedJobsEvent>(8);
        let mut daemon = SchedulerDaemon::new(
            dir.path().to_path_buf(),
            SchedulerConfig::default(),
            "test-missed-old".into(),
            tx,
        );
        daemon.set_missed_tx(mtx);
        // at 时刻在 48 小时前 → 超出 24h 补投窗口
        let job = at_job_offset("missed-stale", -48 * 60 * 60 * 1000);
        daemon.jobs = vec![job];

        daemon.detect_missed_jobs().await;

        let ev = mrx.try_recv().expect("应收到 MissedJobsEvent");
        assert_eq!(ev.jobs.len(), 1);
        assert!(
            !ev.jobs[0].within_catchup_window,
            "48h-ago missed 应超出 24h 补投窗口"
        );
    }
}
