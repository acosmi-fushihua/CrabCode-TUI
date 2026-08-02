//! crabcode-cron — CrabCode 独立 cron 调度 daemon（立宪 3 阶段 3-A）。
//!
//! 启动入口：`crabcode-cron serve`（无 sub 参数也支持，单一职责二进制）。
//!
//! 流程：
//! 1. 初始化 tracing（写 `~/.crabcode/logs/cron.log`，环境变量 `RUST_LOG` 控制
//!    级别，缺省 info）
//! 2. resolve `cron_dir = acosmi_config::paths::resolve_state_dir()`
//! 3. 用 `acosmi_scheduler::lock::try_acquire_lock(cron_dir, identity)` 防双启
//! 4. 写 `~/.crabcode/run/cron.pid`，注册 atexit 清理
//! 5. 监听 `~/.crabcode/run/cron.sock` UDS（先 unlink 残留，确保独占绑定）
//! 6. 构造 `SchedulerDaemon`（cmd_rx + event_tx + status_sink），用 tokio
//!    multi-thread runtime 跑
//! 7. 主循环：转发 `JobFiredEvent` 到 `cron_events` 队列；监听 SIGTERM/SIGINT
//!    cancel 关停
//! 8. 关停：cancel daemon → 等 daemon 任务退出 → 删 pid 文件 →
//!    release_lock（daemon.run 内已释放，这里幂等兜底）
//!
//! 立宪硬规则：
//! - cron 调度核心 = Rust，Go 不参与决策
//! - 不打补丁、不 rollback、不留 TODO
//! - `~/.crabcode/cron/scheduled_tasks.json` 文件格式不变（走 acosmi-scheduler
//!   的 task_store 读写）
//!
//! 平台支持：双平台。
//!
//! - Unix：tokio `UnixListener / UnixStream`（UDS @ `~/.crabcode/run/cron.sock`）
//! - Windows：tokio `NamedPipeServer / NamedPipeClient`（@ `\\.\pipe\crabcode-<USER>-cron`）
//!
//! 业务层（`SchedulerDaemon` / IPC handler）完全跨平台；仅 listener bind /
//! signal handler 分平台。

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown
)]

mod delivery;
mod ipc;
mod layout_migration;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acosmi_cron_ledger::{FireAdvance, LedgerConnection, OccurrenceKind};
use acosmi_daemon_launcher::paths;
use acosmi_scheduler::{
    ChannelStatusSink, CronCommand, CronJob, FireReason, JobFiredEvent, MissedJobsEvent,
    OutboxEvent, OutboxEventType, SchedulerConfig, SchedulerDaemon, SchedulerStatusSink, outbox,
};
use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// 触发事件队列（`Mutex<Vec<Value>>`）—— ipc 模块 `poll_events` 会 drain。
/// 与 supervisor lib.rs:614-633 的 `cron_events_queue` 等价。
type EventQueue = Arc<Mutex<Vec<serde_json::Value>>>;

/// Lease length the cron actor stamps on a `claim_next_occurrence` lease, in
/// milliseconds. Mirrors `delivery::DELIVERY_LEASE_MS`: the actor injects both
/// the clock (`now_ms`) and this length, so a claimed occurrence's lease expires
/// at `now_ms + CLAIM_LEASE_MS`; once it lapses a crashed consumer's occurrence
/// becomes re-claimable at a bumped fence.
const CLAIM_LEASE_MS: i64 = 15_000;

/// daemon 内唯一的 durable cron ledger writer。
///
/// producer 只提交 job 语义，不在 producer task 中预分配 event_id。writer task
/// 串行执行「分配 id → append/rotation」以及 delivery begin/accept/abandon，
/// 所以 outbox 与 delivery journal 都只有这一个 mutation serialization point。
#[derive(Clone)]
pub(crate) struct CronLedgerWriter {
    tx: mpsc::Sender<CronLedgerCommand>,
}

enum CronLedgerCommand {
    AppendJob {
        job: Box<CronJob>,
        event_type: OutboxEventType,
        scheduled_at_ms: i64,
        /// Occurrence classification for the durable SQLite ledger row this
        /// fire also records (shares one identity with the outbox append).
        occurrence_kind: OccurrenceKind,
        /// How to advance the ledger `jobs` row once the occurrence is newly
        /// inserted (recurring reschedule / one-shot delete / manual no-op).
        advance: FireAdvance,
        reply: oneshot::Sender<Option<u64>>,
    },
    DeliveryBegin {
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event: Box<OutboxEvent>,
        consumer: delivery::DeliveryConsumer,
        reply: oneshot::Sender<
            std::result::Result<delivery::DeliveryBeginOutcome, delivery::DeliveryError>,
        >,
    },
    DeliveryAccept {
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event_id: u64,
        generation: u64,
        lease_token: String,
        reply: oneshot::Sender<
            std::result::Result<delivery::DeliveryAcceptOutcome, delivery::DeliveryError>,
        >,
    },
    DeliveryAbandon {
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event_id: u64,
        generation: u64,
        lease_token: String,
        reply: oneshot::Sender<
            std::result::Result<delivery::DeliveryAbandonOutcome, delivery::DeliveryError>,
        >,
    },
    OutboxRead {
        after_event_id: u64,
        limit: usize,
        reply: oneshot::Sender<std::io::Result<outbox::OutboxReadPage>>,
    },
    /// Claim the next runnable occurrence for `consumer.target`, minting a
    /// durable lease over it. The actor (not the caller) mints the clock
    /// (`now_ms`) and the `lease_token`, so a claim over the IPC boundary can
    /// never inject either — see the handler.
    ClaimNextOccurrence {
        consumer: acosmi_cron_ledger::ConsumerClaim,
        reply: oneshot::Sender<std::result::Result<acosmi_cron_ledger::ClaimOutcome, String>>,
    },
    /// Record a successful run of a claimed occurrence (W3-1 writeback). The
    /// actor mints the clock (`now_ms`); the caller echoes back the claim's
    /// identifiers and `fence` so the ledger's fence gate can reject a
    /// superseded (zombie) writer. Mirrors `ClaimNextOccurrence`.
    AcceptOccurrence {
        occurrence_id: i64,
        attempt_id: i64,
        fence: i64,
        duration_ms: Option<i64>,
        reply: oneshot::Sender<std::result::Result<acosmi_cron_ledger::WritebackOutcome, String>>,
    },
    /// Record a failed run of a claimed occurrence (W3-1 writeback). Same shape
    /// as `AcceptOccurrence` plus the failure classification and an optional
    /// error message; the actor mints the clock.
    ReportFailureOccurrence {
        occurrence_id: i64,
        attempt_id: i64,
        fence: i64,
        kind: acosmi_cron_ledger::ReportedFailureKind,
        error_message: Option<String>,
        duration_ms: Option<i64>,
        reply: oneshot::Sender<std::result::Result<acosmi_cron_ledger::WritebackOutcome, String>>,
    },
    /// Reset a terminal `failed` occurrence to `pending` (W3-2 lifecycle),
    /// bumping its fence so W2's claim re-picks it. The actor mints the clock.
    RetryOccurrence {
        occurrence_id: i64,
        reply: oneshot::Sender<std::result::Result<acosmi_cron_ledger::RetryOutcome, String>>,
    },
    /// Terminate a `pending` / `claimed` occurrence as `skipped` (W3-2
    /// lifecycle), bumping its fence and abandoning any in-flight lease. The
    /// actor mints the clock.
    CancelOccurrence {
        occurrence_id: i64,
        reply: oneshot::Sender<std::result::Result<acosmi_cron_ledger::CancelOutcome, String>>,
    },
    /// Read-only audit history for a job (occurrence ⋈ attempts ⋈ results). Still
    /// take()/replace across the blocking read so the serial actor keeps sole
    /// ownership of the single ledger connection.
    OccurrenceHistory {
        job_id: String,
        reply: oneshot::Sender<
            std::result::Result<Vec<acosmi_cron_ledger::OccurrenceHistoryRow>, String>,
        >,
    },
}

impl CronLedgerWriter {
    /// 提交一条事件并等待唯一 writer 完成 append。
    ///
    /// 返回分配到的 id；writer 关闭或 id 空间耗尽时返回 None。outbox 本身的
    /// I/O 失败仍沿用 `outbox::append` 的 fail-soft 契约：id 已消费，允许 gap，
    /// 但绝不回退或复用。
    async fn append_job(
        &self,
        job: &CronJob,
        event_type: OutboxEventType,
        scheduled_at_ms: i64,
        occurrence_kind: OccurrenceKind,
        advance: FireAdvance,
    ) -> Option<u64> {
        let job_id = job.id.clone();
        let (reply, reply_rx) = oneshot::channel();
        let request = CronLedgerCommand::AppendJob {
            job: Box::new(job.clone()),
            event_type,
            scheduled_at_ms,
            occurrence_kind,
            advance,
            reply,
        };
        if self.tx.send(request).await.is_err() {
            tracing::error!(job_id, "cron outbox writer 已关闭，事件未写入");
            return None;
        }
        match reply_rx.await {
            Ok(event_id) => event_id,
            Err(error) => {
                tracing::error!(job_id, error = %error, "cron outbox writer 未返回写入结果");
                None
            }
        }
    }

    pub(crate) async fn delivery_begin(
        &self,
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event: OutboxEvent,
        consumer: delivery::DeliveryConsumer,
    ) -> std::result::Result<delivery::DeliveryBeginOutcome, delivery::DeliveryError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(CronLedgerCommand::DeliveryBegin {
                expected_state_identity,
                expected_ledger_epoch,
                event: Box::new(event),
                consumer,
                reply,
            })
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?;
        reply_rx
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?
    }

    pub(crate) async fn delivery_accept(
        &self,
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event_id: u64,
        generation: u64,
        lease_token: String,
    ) -> std::result::Result<delivery::DeliveryAcceptOutcome, delivery::DeliveryError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(CronLedgerCommand::DeliveryAccept {
                expected_state_identity,
                expected_ledger_epoch,
                event_id,
                generation,
                lease_token,
                reply,
            })
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?;
        reply_rx
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?
    }

    pub(crate) async fn delivery_abandon(
        &self,
        expected_state_identity: String,
        expected_ledger_epoch: String,
        event_id: u64,
        generation: u64,
        lease_token: String,
    ) -> std::result::Result<delivery::DeliveryAbandonOutcome, delivery::DeliveryError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(CronLedgerCommand::DeliveryAbandon {
                expected_state_identity,
                expected_ledger_epoch,
                event_id,
                generation,
                lease_token,
                reply,
            })
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?;
        reply_rx
            .await
            .map_err(|_| delivery::DeliveryError::ActorUnavailable)?
    }

    pub(crate) async fn outbox_read(
        &self,
        after_event_id: u64,
        limit: usize,
    ) -> std::io::Result<outbox::OutboxReadPage> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(CronLedgerCommand::OutboxRead {
                after_event_id,
                limit,
                reply,
            })
            .await
            .map_err(|_| std::io::Error::other("cron ledger actor unavailable"))?;
        reply_rx
            .await
            .map_err(|_| std::io::Error::other("cron ledger actor unavailable"))?
    }

    /// Claim the next runnable occurrence for `consumer.target` over the sole
    /// ledger connection, minting a durable lease.
    ///
    /// The clock and lease token are minted inside the actor (see the
    /// `ClaimNextOccurrence` handler), never by the caller. On a closed actor
    /// channel or a dropped reply this returns `Err`, mirroring `append_job`'s
    /// fail-soft `None` (here the `Result` error arm) rather than panicking.
    pub(crate) async fn claim_next(
        &self,
        consumer: acosmi_cron_ledger::ConsumerClaim,
    ) -> std::result::Result<acosmi_cron_ledger::ClaimOutcome, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::ClaimNextOccurrence { consumer, reply })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，claim 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 claim 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }

    /// Record a successful run of a claimed occurrence over the sole ledger
    /// connection. The actor mints the clock; on a closed channel or a dropped
    /// reply this returns `Err`, mirroring [`Self::claim_next`].
    pub(crate) async fn accept_occurrence(
        &self,
        occurrence_id: i64,
        attempt_id: i64,
        fence: i64,
        duration_ms: Option<i64>,
    ) -> std::result::Result<acosmi_cron_ledger::WritebackOutcome, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::AcceptOccurrence {
                occurrence_id,
                attempt_id,
                fence,
                duration_ms,
                reply,
            })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，accept 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 accept 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }

    /// Record a failed run of a claimed occurrence over the sole ledger
    /// connection. Same fence-gated contract as [`Self::accept_occurrence`]; the
    /// actor mints the clock.
    pub(crate) async fn report_failure_occurrence(
        &self,
        occurrence_id: i64,
        attempt_id: i64,
        fence: i64,
        kind: acosmi_cron_ledger::ReportedFailureKind,
        error_message: Option<String>,
        duration_ms: Option<i64>,
    ) -> std::result::Result<acosmi_cron_ledger::WritebackOutcome, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::ReportFailureOccurrence {
                occurrence_id,
                attempt_id,
                fence,
                kind,
                error_message,
                duration_ms,
                reply,
            })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，report_failure 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 report_failure 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }

    /// Reset a terminal `failed` occurrence to `pending` over the sole ledger
    /// connection. The actor mints the clock; `Err` on a closed channel /
    /// dropped reply, mirroring [`Self::claim_next`].
    pub(crate) async fn retry_occurrence(
        &self,
        occurrence_id: i64,
    ) -> std::result::Result<acosmi_cron_ledger::RetryOutcome, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::RetryOccurrence {
                occurrence_id,
                reply,
            })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，retry 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 retry 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }

    /// Terminate a `pending` / `claimed` occurrence as `skipped` over the sole
    /// ledger connection. The actor mints the clock; `Err` on a closed channel /
    /// dropped reply, mirroring [`Self::claim_next`].
    pub(crate) async fn cancel_occurrence(
        &self,
        occurrence_id: i64,
    ) -> std::result::Result<acosmi_cron_ledger::CancelOutcome, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::CancelOccurrence {
                occurrence_id,
                reply,
            })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，cancel 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 cancel 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }

    /// Read the audit history for a job over the sole ledger connection. `Err` on
    /// a closed channel / dropped reply, mirroring [`Self::claim_next`].
    pub(crate) async fn occurrence_history(
        &self,
        job_id: String,
    ) -> std::result::Result<Vec<acosmi_cron_ledger::OccurrenceHistoryRow>, String> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(CronLedgerCommand::OccurrenceHistory { job_id, reply })
            .await
            .is_err()
        {
            tracing::error!("cron ledger writer 已关闭，occurrence_history 未执行");
            return Err("cron ledger actor unavailable".to_string());
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "cron ledger writer 未返回 occurrence_history 结果");
                Err("cron ledger actor unavailable".to_string())
            }
        }
    }
}

/// Absolute path of the cron SQLite ledger db, beside `scheduled_tasks.json`
/// (`<cron_dir>/cron/cron_ledger.db`).
///
/// Single source for both the startup migration open ([`migrate_cron_ledger`])
/// and the fire-path actor's reopen-on-panic, so the two can never drift onto
/// different files.
fn cron_ledger_db_path(cron_dir: &std::path::Path) -> PathBuf {
    acosmi_scheduler::task_store::cron_file_path(cron_dir)
        .with_file_name(acosmi_cron_ledger::LEDGER_DB_FILENAME)
}

/// W1b→W1c-B: run the one-shot `scheduled_tasks.json` → SQLite ledger migration
/// and RETURN the open connection for the fire path.
///
/// Opens the ledger at `<cron_dir>/cron/cron_ledger.db`, runs
/// import→verify→cutover (with a read-only backup COPY of the source — the live
/// file stays put for the frozen daemon), and hands the connection to the fire
/// writer actor (see [`spawn_cron_ledger_writer`]) so every cron fire records a
/// durable occurrence keyed by its theoretical `scheduled_at_ms`. Job STORAGE is
/// unchanged: `SchedulerDaemon` still reads/writes `scheduled_tasks.json`; only
/// occurrences go to SQLite. Any failure propagates as `Err`, so the caller
/// aborts startup through main's owned cleanup: the scheduler lock is released
/// and the PID is never published.
async fn migrate_cron_ledger(cron_dir: &std::path::Path) -> Result<LedgerConnection> {
    let ledger_db_path = cron_ledger_db_path(cron_dir);
    if let Some(parent) = ledger_db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 cron ledger 目录失败: {}", parent.display()))?;
    }
    let mut ledger = LedgerConnection::open(&ledger_db_path)
        .with_context(|| format!("打开 cron ledger 失败: {}", ledger_db_path.display()))?;
    let outcome = ledger
        .migrate_json_store(cron_dir)
        .await
        .context("cron ledger 首启迁移失败（scheduled_tasks.json → SQLite）")?;
    tracing::info!(?outcome, db = %ledger_db_path.display(), "cron ledger 迁移完成");
    Ok(ledger)
}

/// 启动唯一 writer task。`next_event_id` 只存在于该 task 内部；没有 producer
/// 能在等待 append 之前抢先拿到更低 id。
async fn spawn_cron_ledger_writer(
    cron_dir: PathBuf,
    initial_event_id: u64,
    state_identity: String,
    ledger_epoch: String,
    ledger: LedgerConnection,
) -> Result<(CronLedgerWriter, tokio::task::JoinHandle<()>)> {
    let mut delivery_store =
        delivery::DeliveryJournalStore::open(&cron_dir, state_identity, ledger_epoch).await?;
    let retained_page = outbox::read_after_with_bounds(&cron_dir, 0, usize::MAX).await?;
    if let Some(retained_floor) = retained_page.oldest_event_id {
        delivery_store
            .advance_retained_floor(retained_floor)
            .await?;
    }
    let retained_events = retained_page
        .events
        .into_iter()
        .map(|event| (event.event_id, event))
        .collect();
    Ok(spawn_cron_ledger_writer_with_store(
        cron_dir,
        initial_event_id,
        delivery_store,
        retained_events,
        ledger,
    ))
}

fn spawn_cron_ledger_writer_with_store(
    cron_dir: PathBuf,
    initial_event_id: u64,
    mut delivery_store: delivery::DeliveryJournalStore,
    mut retained_events: BTreeMap<u64, OutboxEvent>,
    ledger: LedgerConnection,
) -> (CronLedgerWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<CronLedgerCommand>(64);
    // Reopen target if a fire_occurrence task ever panics mid-write (fail-soft).
    // Derived from the SAME helper migrate_cron_ledger opened with, so the actor
    // never reopens a different file.
    let ledger_db_path = cron_ledger_db_path(&cron_dir);
    let task = tokio::spawn(async move {
        // Serial actor owns the sole ledger connection. `take()`/replace across
        // the spawn_blocking round-trip is safe: this task processes one command
        // at a time, so no other holder can observe the `None` gap.
        let mut ledger: Option<LedgerConnection> = Some(ledger);
        let mut next_event_id = Some(initial_event_id);
        let mut delivery_degraded_reason: Option<DeliveryDegradedReason> = None;
        while let Some(command) = rx.recv().await {
            match command {
                CronLedgerCommand::AppendJob {
                    job,
                    event_type,
                    scheduled_at_ms,
                    occurrence_kind,
                    advance,
                    reply,
                } => {
                    let Some(event_id) = next_event_id else {
                        tracing::error!(
                            job_id = %job.id,
                            max_safe_event_id = outbox::MAX_SAFE_EVENT_ID,
                            "cron outbox event_id 空间已耗尽；拒绝超过或复用 JavaScript MAX_SAFE_INTEGER"
                        );
                        let _ = reply.send(None);
                        continue;
                    };
                    // 先永久消费当前 id，再 append。append 失败允许留下 gap，但下一条
                    // 只能使用更高 id；JavaScript MAX_SAFE_INTEGER 使用一次后耗尽。
                    next_event_id = (event_id < outbox::MAX_SAFE_EVENT_ID).then_some(event_id + 1);
                    let event = OutboxEvent::from_job(&job, event_id, event_type, scheduled_at_ms);
                    match outbox::append_durable(&cron_dir, &event).await {
                        Ok(outcome) => {
                            if let Some(retained_after_rotation) =
                                outcome.retained_events_after_rotation
                            {
                                retained_events = retained_after_rotation
                                    .into_iter()
                                    .map(|event| (event.event_id, event))
                                    .collect();
                            } else {
                                retained_events.insert(event.event_id, event.clone());
                            }
                            if let Some(retained_floor) = outcome.retained_floor_after_rotation
                                && let Err(error) =
                                    delivery_store.advance_retained_floor(retained_floor).await
                            {
                                delivery_degraded_reason =
                                    Some(DeliveryDegradedReason::Store(error.to_string()));
                                tracing::error!(
                                    error = %error,
                                    retained_floor,
                                    "cron delivery retained floor advance failed"
                                );
                            }
                        }
                        Err(error) => {
                            delivery_degraded_reason =
                                Some(DeliveryDegradedReason::Outbox(error.to_string()));
                            tracing::error!(
                                error = %error,
                                job_id = %job.id,
                                event_id,
                                "cron_outbox: durable append failed; delivery mutations fail closed until restart"
                            );
                        }
                    }
                    // Second durable write of this fire: record a SQLite
                    // occurrence keyed by the theoretical `scheduled_at_ms`,
                    // reusing the just-built `event` so the occurrence and the
                    // outbox row share one derived identity (`legacy_event_id`
                    // = this `event_id`). Independent + fail-soft: it runs even
                    // if the outbox append above failed, and its own failure
                    // neither aborts the outbox path nor crashes the daemon.
                    // `fire_occurrence` is synchronous → wrap in spawn_blocking;
                    // the connection is moved out and back so the serial actor
                    // keeps sole ownership. UNIQUE(job_id, scheduled_at_ms)
                    // makes a crash-replay of the same firing an idempotent
                    // no-op (`AlreadyRecorded`).
                    if let Some(mut conn) = ledger.take() {
                        let (job_id, sched, kind, emitted) = (
                            event.job_id.clone(),
                            event.scheduled_at_ms,
                            occurrence_kind,
                            event.emitted_at_ms,
                        );
                        let (msg, name) = (event.payload_message.clone(), event.job_name.clone());
                        let legacy = i64::try_from(event_id).ok();
                        // Immutable per-fire execution envelope, projected from
                        // the job in hand. Captured before spawn_blocking and
                        // moved into the closure; fire_occurrence persists it on
                        // the occurrence row (ON CONFLICT DO NOTHING keeps the
                        // first envelope on an idempotent re-fire).
                        let envelope = acosmi_cron_ledger::ExecutionEnvelope::from_job(&job);
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.fire_occurrence(
                                &job_id,
                                sched,
                                kind,
                                emitted,
                                msg.as_deref(),
                                name.as_deref(),
                                legacy,
                                &envelope,
                                advance,
                            );
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(outcome))) => {
                                ledger = Some(c);
                                tracing::debug!(
                                    ?outcome,
                                    event_id,
                                    "cron ledger occurrence recorded"
                                );
                            }
                            Ok((c, Err(e))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %e,
                                    event_id,
                                    "cron ledger fire_occurrence failed (fail-soft)"
                                );
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger fire task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                            }
                        }
                    } else {
                        tracing::error!(
                            event_id,
                            "cron ledger connection unavailable; occurrence skipped (fail-soft)"
                        );
                    }
                    let _ = reply.send(Some(event_id));
                }
                CronLedgerCommand::DeliveryBegin {
                    expected_state_identity,
                    expected_ledger_epoch,
                    event,
                    consumer,
                    reply,
                } => {
                    // Membership/content is checked before availability. A
                    // rotation may evict a stale page and then fail to persist
                    // the floor; returning an allowlisted store-unavailable
                    // error first would let that evicted event fail open.
                    let membership_error = match retained_events.get(&event.event_id) {
                        None => Some(delivery::DeliveryError::EventNotRetained(event.event_id)),
                        Some(retained) if retained != event.as_ref() => Some(
                            delivery::DeliveryError::EventIdentityCollision(event.event_id),
                        ),
                        Some(_) => None,
                    };
                    let result = match membership_error {
                        Some(error) => Err(error),
                        None => match delivery_degraded_reason.as_ref() {
                            Some(reason) => Err(reason.to_error()),
                            None => {
                                delivery_store
                                    .begin(
                                        &expected_state_identity,
                                        &expected_ledger_epoch,
                                        *event,
                                        consumer,
                                    )
                                    .await
                            }
                        },
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::DeliveryAccept {
                    expected_state_identity,
                    expected_ledger_epoch,
                    event_id,
                    generation,
                    lease_token,
                    reply,
                } => {
                    let result = match delivery_degraded_reason.as_ref() {
                        Some(reason) => Err(reason.to_error()),
                        None => {
                            delivery_store
                                .accept(
                                    &expected_state_identity,
                                    &expected_ledger_epoch,
                                    event_id,
                                    generation,
                                    &lease_token,
                                )
                                .await
                        }
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::DeliveryAbandon {
                    expected_state_identity,
                    expected_ledger_epoch,
                    event_id,
                    generation,
                    lease_token,
                    reply,
                } => {
                    let result = match delivery_degraded_reason.as_ref() {
                        Some(reason) => Err(reason.to_error()),
                        None => {
                            delivery_store
                                .abandon(
                                    &expected_state_identity,
                                    &expected_ledger_epoch,
                                    event_id,
                                    generation,
                                    &lease_token,
                                )
                                .await
                        }
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::OutboxRead {
                    after_event_id,
                    limit,
                    reply,
                } => {
                    let result =
                        outbox::read_after_with_bounds(&cron_dir, after_event_id, limit).await;
                    if let Err(error) = &result {
                        delivery_degraded_reason =
                            Some(DeliveryDegradedReason::Outbox(error.to_string()));
                    }
                    let _ = reply.send(result);
                }
                CronLedgerCommand::ClaimNextOccurrence { consumer, reply } => {
                    // The actor mints BOTH the clock and the lease token here —
                    // the caller (and the IPC consumer beyond it) supplies
                    // neither, matching how delivery leases are minted daemon-
                    // side. `claim_next_occurrence` is synchronous → wrap it in
                    // spawn_blocking with the SAME take()/replace fold the
                    // fire_occurrence path uses, so the serial actor keeps sole
                    // ownership of the connection and a mid-write panic fails
                    // soft (reopen from `ledger_db_path`) instead of poisoning it.
                    let result = if let Some(mut conn) = ledger.take() {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let lease_token = uuid::Uuid::new_v4().to_string();
                        match tokio::task::spawn_blocking(move || {
                            // Opportunistic audit sweep (W3-2): before claiming,
                            // mark any still-unresolved lease whose occurrence has
                            // already advanced past it as `expired`. It is a
                            // SEPARATE `conn` op in its own `BEGIN IMMEDIATE`,
                            // OUTSIDE the claim's transaction (the claim opens its
                            // own tx), so it cannot interfere with the claim gate.
                            // Its Result is folded out and handled fail-soft below;
                            // `claim_next_occurrence` itself is unchanged.
                            let sweep = conn.expire_superseded_attempts();
                            let claim = conn.claim_next_occurrence(
                                &consumer,
                                now_ms,
                                CLAIM_LEASE_MS,
                                &lease_token,
                            );
                            (conn, sweep, claim)
                        })
                        .await
                        {
                            Ok((c, sweep, claim)) => {
                                ledger = Some(c);
                                // Fail-soft: a sweep error is logged and the claim
                                // still proceeds (the sweep touches no occurrence
                                // lifecycle, so a failed sweep never blocks work).
                                match sweep {
                                    Ok(expired) => tracing::debug!(
                                        expired,
                                        "cron ledger opportunistic expire sweep completed"
                                    ),
                                    Err(error) => tracing::error!(
                                        error = %error,
                                        "cron ledger expire_superseded_attempts failed (fail-soft; continuing to claim)"
                                    ),
                                }
                                match claim {
                                    Ok(outcome) => Ok(outcome),
                                    Err(error) => {
                                        tracing::error!(
                                            error = %error,
                                            "cron ledger claim_next_occurrence failed"
                                        );
                                        Err(error.to_string())
                                    }
                                }
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger claim task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!("cron ledger claim task panicked: {join}"))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; claim skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::AcceptOccurrence {
                    occurrence_id,
                    attempt_id,
                    fence,
                    duration_ms,
                    reply,
                } => {
                    // Mirror ClaimNextOccurrence: mint the clock actor-side, run
                    // the synchronous fence-gated writeback in spawn_blocking with
                    // the same take()/replace fold + reopen-on-panic.
                    let result = if let Some(mut conn) = ledger.take() {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.accept_occurrence(
                                occurrence_id,
                                attempt_id,
                                fence,
                                duration_ms,
                                now_ms,
                            );
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(outcome))) => {
                                ledger = Some(c);
                                Ok(outcome)
                            }
                            Ok((c, Err(error))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %error,
                                    "cron ledger accept_occurrence failed"
                                );
                                Err(error.to_string())
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger accept task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!("cron ledger accept task panicked: {join}"))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; accept skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::ReportFailureOccurrence {
                    occurrence_id,
                    attempt_id,
                    fence,
                    kind,
                    error_message,
                    duration_ms,
                    reply,
                } => {
                    let result = if let Some(mut conn) = ledger.take() {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.report_failure_occurrence(
                                occurrence_id,
                                attempt_id,
                                fence,
                                kind,
                                error_message.as_deref(),
                                duration_ms,
                                now_ms,
                            );
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(outcome))) => {
                                ledger = Some(c);
                                Ok(outcome)
                            }
                            Ok((c, Err(error))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %error,
                                    "cron ledger report_failure_occurrence failed"
                                );
                                Err(error.to_string())
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger report_failure task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!("cron ledger report_failure task panicked: {join}"))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; report_failure skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::RetryOccurrence {
                    occurrence_id,
                    reply,
                } => {
                    let result = if let Some(mut conn) = ledger.take() {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.retry_occurrence(occurrence_id, now_ms);
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(outcome))) => {
                                ledger = Some(c);
                                Ok(outcome)
                            }
                            Ok((c, Err(error))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %error,
                                    "cron ledger retry_occurrence failed"
                                );
                                Err(error.to_string())
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger retry task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!("cron ledger retry task panicked: {join}"))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; retry skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::CancelOccurrence {
                    occurrence_id,
                    reply,
                } => {
                    let result = if let Some(mut conn) = ledger.take() {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.cancel_occurrence(occurrence_id, now_ms);
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(outcome))) => {
                                ledger = Some(c);
                                Ok(outcome)
                            }
                            Ok((c, Err(error))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %error,
                                    "cron ledger cancel_occurrence failed"
                                );
                                Err(error.to_string())
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger cancel task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!("cron ledger cancel task panicked: {join}"))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; cancel skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
                CronLedgerCommand::OccurrenceHistory { job_id, reply } => {
                    // A read, but still take()/replace across the blocking query
                    // so the serial actor keeps sole ownership of the connection.
                    // `occurrence_history` borrows `&self`, so `conn` stays
                    // immutable — it is only moved out and back for ownership.
                    let result = if let Some(conn) = ledger.take() {
                        match tokio::task::spawn_blocking(move || {
                            let r = conn.occurrence_history(&job_id);
                            (conn, r)
                        })
                        .await
                        {
                            Ok((c, Ok(rows))) => {
                                ledger = Some(c);
                                Ok(rows)
                            }
                            Ok((c, Err(error))) => {
                                ledger = Some(c);
                                tracing::error!(
                                    error = %error,
                                    "cron ledger occurrence_history failed"
                                );
                                Err(error.to_string())
                            }
                            Err(join) => {
                                tracing::error!(
                                    error = %join,
                                    "cron ledger occurrence_history task panicked; reopening"
                                );
                                ledger = LedgerConnection::open(&ledger_db_path).ok();
                                Err(format!(
                                    "cron ledger occurrence_history task panicked: {join}"
                                ))
                            }
                        }
                    } else {
                        tracing::error!(
                            "cron ledger connection unavailable; occurrence_history skipped (fail-soft)"
                        );
                        Err("cron ledger connection unavailable".to_string())
                    };
                    let _ = reply.send(result);
                }
            }
        }
    });
    (CronLedgerWriter { tx }, task)
}

enum DeliveryDegradedReason {
    Outbox(String),
    Store(String),
}

impl DeliveryDegradedReason {
    fn to_error(&self) -> delivery::DeliveryError {
        match self {
            Self::Outbox(reason) => delivery::DeliveryError::OutboxStateUnavailable(format!(
                "cron outbox durability degraded; restart required: {reason}"
            )),
            Self::Store(reason) => delivery::DeliveryError::Store(std::io::Error::other(format!(
                "cron delivery store durability degraded; restart required: {reason}"
            ))),
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    init_tracing();
    parse_args_or_exit();

    let cron_dir = acosmi_config::paths::resolve_state_dir();
    let identity = format!("crabcode-cron-{}", std::process::id());

    acquire_scheduler_lock_or_exit(&cron_dir, &identity).await?;
    let pid_file = paths::pid_file("cron");
    let sock_path = unlink_stale_socket();
    // C3: bind one immutable state identity for this entire daemon lifecycle.
    // Never recompute from mutable process env while serving requests.
    let state_identity =
        acosmi_daemon_launcher::state_identity::cron_state_identity_for(&cron_dir, &sock_path)
            .context("resolve cron state identity")?;
    tracing::info!(state_identity, "cron daemon state identity bound");

    let result = run_locked_daemon(&cron_dir, &identity, &sock_path, &state_identity).await;
    cleanup_runtime_files(&pid_file, &sock_path, &cron_dir, &identity).await;

    match result {
        Ok(()) => tracing::info!("crabcode-cron 已停止"),
        Err(ref error) => tracing::error!(error = %error, "crabcode-cron 异常停止"),
    }
    result
}

/// Run one lifecycle after the state lock is owned.
///
/// The PID file is intentionally absent until three independent readiness
/// facts hold: the platform shutdown channel exists, SchedulerDaemon reports
/// ownership, and the first IPC listener/pipe instance is bound. Any critical
/// task exit is returned to `main`, which always performs owned cleanup.
async fn run_locked_daemon(
    cron_dir: &std::path::Path,
    identity: &str,
    sock_path: &std::path::Path,
    state_identity: &str,
) -> Result<()> {
    let cron_dir = cron_dir.to_path_buf();
    let identity = identity.to_owned();
    let sock_path = sock_path.to_path_buf();
    let state_identity = state_identity.to_owned();

    let (event_tx, event_rx) = mpsc::channel::<JobFiredEvent>(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<CronCommand>(32);
    let (owner_tx, mut owner_rx) = mpsc::channel::<bool>(16);
    // W-CRON-AUTOMATION-E2E P7：missed one-shot 任务事件经此通道从 scheduler
    // 报到本进程，由本进程写持久 outbox（scheduler 不持 dir + id counter）。
    let (missed_tx, missed_rx) = mpsc::channel::<MissedJobsEvent>(32);

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone())?;

    // W-CRON-DIR-UNIFY PR-1: unify the daemon's on-disk layout (`.crabcode/` →
    // `cron/`) BEFORE any nested-layer write. Both the primary (`cron/`) lock and,
    // when the legacy dir exists, the legacy (`.crabcode/`) lock are already held
    // here, so no concurrent old-generation daemon can be writing `.crabcode/`
    // during the move. W-CRON-MIGRATION-REENTRY-GUARD (2026-07-20): that "already
    // held" is no longer merely asserted — the migration re-verifies primary-lock
    // ownership (via this `identity`) before EACH destructive rename, so even if
    // the liveness layer's rare residual let a second daemon reclaim the lock, at
    // most one process performs the move; the loser aborts and yields at the gate.
    // Failure = cold-start failure: `?` bubbles to `main`, whose owned
    // `cleanup_runtime_files` releases BOTH locks and never publishes the PID
    // (mirrors the IPC-bind startup barrier) → launcher `ensure_running` retries →
    // the idempotent re-entry self-heals. Must run before `initialize_ledger_epoch`
    // (the first nested-layer write) and before any PID publication — both hold here.
    let layout_migration = layout_migration::migrate_legacy_layout_if_present(&cron_dir, &identity)
        .await
        .context("unify cron on-disk layout (.crabcode/ → cron/)")?;
    tracing::info!(?layout_migration, "cron on-disk layout checked");

    // R5-C4: create/load one durable ledger epoch before either the writer or
    // IPC reader becomes ready. Delivery leases, accepted tombstones, and
    // consumer cursors bind to this epoch so event-id reuse after a deliberate
    // ledger reset cannot collide with an old delivery record.
    let ledger_epoch = outbox::initialize_ledger_epoch(&cron_dir)
        .await
        .context("load cron outbox ledger epoch")?;
    tracing::info!(ledger_epoch, "cron outbox ledger epoch bound");

    // R5-C1：扫描完整 retained window 取 max+1，再交给唯一 writer task。
    // event_id 分配与 append/rotation 同处一个串行 task，不能只靠 Atomic。
    let next_event_id = outbox::next_event_id(&cron_dir)
        .await
        .context("scan cron outbox event-id cursor")?;
    tracing::info!(seed = next_event_id, "cron outbox event-id cursor seeded");

    // W1b: one-shot snapshot of scheduled_tasks.json (+ retained outbox events)
    // into the SQLite ledger (import→verify→cutover). Runs BEFORE any task
    // spawns and while this process holds the sole scheduler lock, so the
    // idempotency check is never raced. The backup is a COPY (not a rename):
    // `SchedulerDaemon` is frozen this wave and its `load_jobs` still reads the
    // live scheduled_tasks.json, so removing it would drop every existing job.
    // Migration failure = cold-start failure: return Err → main's owned cleanup
    // releases the scheduler lock and never publishes the PID (mirrors the
    // IPC-bind-failure startup barrier). W1c-B: the open connection is now handed
    // to the fire writer actor below so every fire records a durable occurrence;
    // job STORAGE stays in scheduled_tasks.json (only occurrences go to SQLite).
    let ledger = migrate_cron_ledger(&cron_dir).await?;

    let (outbox_writer, outbox_writer_task) = spawn_cron_ledger_writer(
        cron_dir.clone(),
        next_event_id,
        state_identity.clone(),
        ledger_epoch.clone(),
        ledger,
    )
    .await
    .context("open cron delivery journal store")?;

    let mut scheduler_handle = spawn_scheduler(
        cron_dir.clone(),
        identity.clone(),
        event_tx,
        cmd_rx,
        owner_tx,
        missed_tx,
        shutdown.clone(),
    );

    // W-CRON-AUTOMATION-E2E P7：missed one-shot 任务 → 持久 outbox。
    let missed_outbox_task =
        spawn_missed_outbox_writer(outbox_writer.clone(), missed_rx, shutdown.clone());

    let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
    // Push 模型 fire 事件广播。容量 64 与 event_rx 对齐；无 subscriber 时
    // broadcaster.send 返 SendError 是预期路径，event 仍写入 events 队列由
    // poll_events fallback drain。
    let (broadcaster, _) = tokio::sync::broadcast::channel::<serde_json::Value>(64);
    let (mut ipc_handle, ipc_ready) = spawn_ipc(
        sock_path.clone(),
        cmd_tx.clone(),
        Arc::clone(&events),
        broadcaster.clone(),
        cron_dir.clone(),
        outbox_writer.clone(),
        state_identity,
        ledger_epoch,
        shutdown.clone(),
    );
    if let Err(error) = wait_for_runtime_ready(
        &mut owner_rx,
        ipc_ready,
        &mut scheduler_handle,
        &mut ipc_handle,
        shutdown.clone(),
    )
    .await
    {
        // `wait_for_runtime_ready` may have consumed one critical JoinHandle
        // while reporting its exit. Re-polling a completed JoinHandle panics,
        // so abort/drop both here; outer owned cleanup releases the state lock
        // and socket deterministically.
        shutdown.cancel();
        scheduler_handle.abort();
        ipc_handle.abort();
        shutdown_all(
            shutdown,
            None,
            None,
            None,
            missed_outbox_task,
            (outbox_writer, outbox_writer_task),
            cmd_tx,
        )
        .await;
        return Err(error);
    }

    // Preserve subsequent owner transitions for diagnostics after consuming
    // the initial readiness value above.
    let owner_log_task = spawn_owner_logger(owner_rx);

    if let Err(error) = write_pid_file() {
        shutdown_all(
            shutdown,
            Some(scheduler_handle),
            Some(ipc_handle),
            Some(owner_log_task),
            missed_outbox_task,
            (outbox_writer, outbox_writer_task),
            cmd_tx,
        )
        .await;
        return Err(error);
    }
    tracing::info!(
        dir = %cron_dir.display(),
        sock = %sock_path.display(),
        "cron runtime ready; PID published"
    );

    let (runtime_result, scheduler_finished, ipc_finished) = run_event_loop(
        event_rx,
        Arc::clone(&events),
        broadcaster,
        outbox_writer.clone(),
        shutdown.clone(),
        &mut scheduler_handle,
        &mut ipc_handle,
    )
    .await;

    shutdown_all(
        shutdown,
        (!scheduler_finished).then_some(scheduler_handle),
        (!ipc_finished).then_some(ipc_handle),
        Some(owner_log_task),
        missed_outbox_task,
        (outbox_writer, outbox_writer_task),
        cmd_tx,
    )
    .await;
    runtime_result
}

fn parse_args_or_exit() {
    let args: Vec<String> = std::env::args().collect();
    // 兼容 `crabcode-cron`（无参）和 `crabcode-cron serve`（与 hub-launcher
    // spawn 时传的 argv 对齐）。其他参数 → 错误退出。
    match args.get(1).map(String::as_str) {
        None | Some("serve") => {}
        Some(other) => {
            eprintln!("crabcode-cron: 未知子命令 {other:?}（仅支持 'serve'）");
            std::process::exit(2);
        }
    }
}

/// 防双启：scheduler 业务级锁（per `state_dir`）。
///
/// 注意：`acosmi-daemon-launcher` 的 `cron.lock` 是 spawn 阶段 flock，防止两个
/// launcher 同时拉起；这里的 `try_acquire_lock` 是 owner 状态锁，跨进程跨
/// 启动周期。两层互补缺一不可。
async fn acquire_scheduler_lock_or_exit(cron_dir: &std::path::Path, identity: &str) -> Result<()> {
    // W-CRON-DIR-UNIFY PR-1 双锁过渡：仍在跑的**旧代**守护（PR-1 之前的二进制）只
    // 认旧锁 `.crabcode/scheduled_tasks.lock`、不认新主锁 `cron/…`。过渡期先竞争旧
    // 锁——且**仅当** legacy 目录已存在（纯新装绝不物化 `.crabcode/`，保持单层洁
    // 净）。撞见旧代守护即退避，未动任何 legacy 数据即退出（顺序 = legacy 先于
    // primary）。
    if acosmi_scheduler::HOLD_LEGACY_LOCK
        && cron_dir.join(layout_migration::LEGACY_DIR_NAME).exists()
    {
        let acquired_legacy = acosmi_scheduler::try_acquire_legacy_lock(cron_dir, identity)
            .await
            .context("legacy scheduler 锁获取失败")?;
        if !acquired_legacy {
            tracing::error!(
                dir = %cron_dir.display(),
                identity,
                "另一个（旧代）crabcode-cron 实例正持有 legacy scheduler 锁；拒绝双启动"
            );
            eprintln!("crabcode-cron: 已有旧代实例运行（持有 legacy scheduler 锁），退出");
            std::process::exit(11);
        }
    }

    let acquired = acosmi_scheduler::try_acquire_lock(cron_dir, identity)
        .await
        .context("scheduler 锁获取失败")?;
    if !acquired {
        tracing::error!(
            dir = %cron_dir.display(),
            identity,
            "另一个 crabcode-cron 实例正持有 scheduler 锁；拒绝双启动"
        );
        eprintln!("crabcode-cron: 已有另一实例运行（持有 scheduler 锁），退出");
        std::process::exit(11);
    }
    Ok(())
}

fn write_pid_file() -> Result<PathBuf> {
    paths::ensure_run_dir().context("创建 ~/.crabcode/run 失败")?;
    let pid_file = paths::pid_file("cron");
    std::fs::write(&pid_file, std::process::id().to_string())
        .with_context(|| format!("写 PID 文件失败: {}", pid_file.display()))?;
    // F1 代际握手 (2026-06-11)：紧邻 pid 文件自报 build-id（同生命周期，
    // cleanup_runtime_files 一并清理）。launcher `ensure_running` fast path
    // 据此判定旧代 daemon 并优雅换代。写失败 warn 不挡启动（缺文件 =
    // 下一轮 ensure 判 Mismatch 换代一次，语义自洽）。
    acosmi_daemon_launcher::build_id::write_build_id_file(
        "cron",
        acosmi_daemon_launcher::build_id::self_build_id(),
    );
    tracing::info!(
        pid = std::process::id(),
        build_id = acosmi_daemon_launcher::build_id::self_build_id(),
        file = %pid_file.display(),
        "PID 文件已写"
    );
    Ok(pid_file)
}

fn unlink_stale_socket() -> PathBuf {
    let sock_path = paths::socket_file("cron");
    if sock_path.exists() {
        // best-effort：失败也继续，UnixListener::bind 失败时再报错
        let _ = std::fs::remove_file(&sock_path);
    }
    sock_path
}

fn spawn_owner_logger(mut owner_rx: mpsc::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // 单进程独占 cron daemon：is_owner 状态主要用于诊断 + 测试观测。
        // 当前 crabcode-cron 没有 supervisor 共享 status，所以这里仅 log。
        while let Some(is_owner) = owner_rx.recv().await {
            tracing::info!(is_owner, "scheduler is_owner 变化");
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_scheduler(
    dir: PathBuf,
    identity: String,
    event_tx: mpsc::Sender<JobFiredEvent>,
    cmd_rx: mpsc::Receiver<CronCommand>,
    owner_tx: mpsc::Sender<bool>,
    missed_tx: mpsc::Sender<MissedJobsEvent>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        let mut daemon = SchedulerDaemon::new(dir, SchedulerConfig::default(), identity, event_tx)
            .with_status_sink(
                Arc::new(ChannelStatusSink::new(owner_tx)) as Arc<dyn SchedulerStatusSink>
            );
        daemon.set_cmd_rx(cmd_rx);
        // W-CRON-AUTOMATION-E2E P7：scheduler 检出 missed one-shot 任务后经此
        // 通道报本进程；本进程把它写持久 outbox（见 spawn_missed_outbox_writer）。
        daemon.set_missed_tx(missed_tx);
        // Step 2 Phase D.5 关联方调整：daemon.run() 已不再"永远返 Ok"——
        // 当 try_acquire_lock 返 io::Err（磁盘故障 / 权限异常 / NotADirectory）时，
        // run() 现在 propagate 该 Err 而不是默默以 is_owner=false 启动。如果
        // 这里仍 `let _ =` 吞错，D.5 的修复在 cron 这条调用链上没有任何效果。
        // C2: preserve the result at the task boundary. Main continuously
        // selects this JoinHandle; an unexpected Ok or any Err tears down the
        // whole daemon and produces a non-zero process exit.
        daemon.run(shutdown).await
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_ipc(
    sock_path: PathBuf,
    cmd_tx: mpsc::Sender<CronCommand>,
    events: EventQueue,
    broadcaster: ipc::EventBroadcaster,
    cron_dir: PathBuf,
    ledger_writer: CronLedgerWriter,
    state_identity: String,
    ledger_epoch: String,
    shutdown: CancellationToken,
) -> (tokio::task::JoinHandle<Result<()>>, oneshot::Receiver<()>) {
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        ipc::serve(
            sock_path,
            cmd_tx,
            events,
            broadcaster,
            cron_dir,
            ledger_writer,
            state_identity,
            ledger_epoch,
            shutdown,
            ready_tx,
        )
        .await
    });
    (task, ready_rx)
}

async fn wait_for_runtime_ready(
    owner_rx: &mut mpsc::Receiver<bool>,
    mut ipc_ready: oneshot::Receiver<()>,
    scheduler_handle: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    ipc_handle: &mut tokio::task::JoinHandle<Result<()>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut scheduler_ready = false;
    let mut listener_ready = false;

    while !scheduler_ready || !listener_ready {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                bail!("shutdown requested before cron runtime became ready");
            }
            outcome = &mut *scheduler_handle => {
                return Err(scheduler_exit_error(outcome, "startup"));
            }
            outcome = &mut *ipc_handle => {
                return Err(ipc_exit_error(outcome, "startup"));
            }
            status = owner_rx.recv(), if !scheduler_ready => {
                match status {
                    Some(true) => scheduler_ready = true,
                    Some(false) => bail!("SchedulerDaemon started without owning the state lock"),
                    None => bail!("SchedulerDaemon readiness channel closed before ready"),
                }
            }
            ready = &mut ipc_ready, if !listener_ready => {
                ready.context("IPC readiness channel closed before listener bind")?;
                listener_ready = true;
            }
        }
    }
    Ok(())
}

fn scheduler_exit_error(
    outcome: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
    phase: &str,
) -> anyhow::Error {
    match outcome {
        Ok(Ok(())) => anyhow!("SchedulerDaemon exited unexpectedly during {phase}"),
        Ok(Err(error)) => anyhow!("SchedulerDaemon failed during {phase}: {error}"),
        Err(error) => anyhow!("SchedulerDaemon task failed during {phase}: {error}"),
    }
}

fn ipc_exit_error(
    outcome: std::result::Result<Result<()>, tokio::task::JoinError>,
    phase: &str,
) -> anyhow::Error {
    match outcome {
        Ok(Ok(())) => anyhow!("cron IPC server exited unexpectedly during {phase}"),
        Ok(Err(error)) => anyhow!("cron IPC server failed during {phase}: {error:#}"),
        Err(error) => anyhow!("cron IPC task failed during {phase}: {error}"),
    }
}

/// W-CRON-AUTOMATION-E2E P7：consume scheduler 报来的 missed one-shot 任务，
/// 为每条分配单调 event_id 并写持久 outbox。
///
/// 窗口内（`within_catchup_window`）→ `Fired`（晚到补投，消费方仍执行）；
/// 窗口外 → `Skipped`（太陈旧仅记录）。missed 任务的实时投递（broadcast /
/// poll_events）不在此处 —— missed 的语义就是"REPL 当时不在"，唯一可靠的
/// 投递面是耐久 outbox + cursor 恢复。
fn spawn_missed_outbox_writer(
    outbox_writer: CronLedgerWriter,
    mut missed_rx: mpsc::Receiver<MissedJobsEvent>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                maybe = missed_rx.recv() => {
                    let Some(ev) = maybe else { break };
                    for info in ev.jobs {
                        // A missed one-shot is terminal: it never fires again, so
                        // the ledger `jobs` row is deleted (FireAdvance::Delete)
                        // while its occurrence is retained (no FK) — window-in →
                        // catch-up (still executed), window-out → skipped (audit).
                        let (event_type, occ_kind) = if info.within_catchup_window {
                            (OutboxEventType::Fired, OccurrenceKind::MissedCatchup)
                        } else {
                            (OutboxEventType::Skipped, OccurrenceKind::MissedSkipped)
                        };
                        if let Some(event_id) = outbox_writer
                            .append_job(
                                &info.job,
                                event_type,
                                info.scheduled_at_ms,
                                occ_kind,
                                FireAdvance::Delete,
                            )
                            .await
                        {
                            tracing::info!(
                                job_id = %info.job.id,
                                event_id,
                                within_window = info.within_catchup_window,
                                "missed one-shot 任务写入 outbox"
                            );
                        }
                    }
                }
            }
        }
    })
}

async fn run_event_loop(
    mut event_rx: mpsc::Receiver<JobFiredEvent>,
    events: EventQueue,
    broadcaster: ipc::EventBroadcaster,
    outbox_writer: CronLedgerWriter,
    shutdown: CancellationToken,
    scheduler_handle: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    ipc_handle: &mut tokio::task::JoinHandle<Result<()>>,
) -> (Result<()>, bool, bool) {
    // 注意：这里没有 `else =>` 兜底。当 event_rx 所有 sender drop 后，
    // recv() 会持续 Ready(None)，pattern `Some(fired)` 失配 → 该 select! 分支
    // 在本轮被 disable；但 shutdown.cancelled() 仍在 Pending，select! 不会进
    // `else`（disabled 与 pending 是两种状态）。daemon 退出本身就走
    // shutdown.cancel() 路径（见 main → shutdown_all），shutdown 分支必触发，
    // 不需要走 `else` 收尾，加 `else` 反而是死代码。
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                tracing::info!("crabcode-cron 收到 shutdown 信号");
                return (Ok(()), false, false);
            }
            outcome = &mut *scheduler_handle => {
                return (Err(scheduler_exit_error(outcome, "runtime")), true, false);
            }
            outcome = &mut *ipc_handle => {
                return (Err(ipc_exit_error(outcome, "runtime")), false, true);
            }
            Some(fired) = event_rx.recv() => {
                tracing::info!(
                    job_id = %fired.job.id,
                    job_name = %fired.job.name,
                    "Cron 任务触发，推送到事件队列"
                );
                // W-CRON-AUTOMATION-E2E P7：BEFORE broadcasting，先把 fire 事件
                // 耐久落进 outbox。outbox 是「daemon 重启 / REPL 离线也不丢」的
                // 恢复真源；broadcast + poll_events 仍保留作低延迟实时面。
                // scheduled_at_ms 取事件携带的理论到期时刻（`fired.scheduled_at_ms`,
                // 重启可复现），不是墙钟触发时刻 —— 它是 SQLite occurrence 的耐久键。
                // reason → (occurrence_kind, ledger advance)：recurring 用刚触发过、
                // 与 daemon persist_jobs 同源的 `fired.job` JSON 推进 ledger jobs 行；
                // final(one-shot/aged/at) 删行；manual 不推进。
                let (occ_kind, advance) = match fired.reason {
                    FireReason::ScheduledRecurring => (
                        OccurrenceKind::Scheduled,
                        match serde_json::to_string(&fired.job) {
                            Ok(j) => FireAdvance::Recurring { new_job_json: j },
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "serialize advanced job failed; occurrence without advance"
                                );
                                FireAdvance::NoAdvance
                            }
                        },
                    ),
                    FireReason::ScheduledFinal => (OccurrenceKind::Scheduled, FireAdvance::Delete),
                    FireReason::Manual => (OccurrenceKind::Manual, FireAdvance::NoAdvance),
                };
                let _ = outbox_writer
                    .append_job(
                        &fired.job,
                        OutboxEventType::Fired,
                        fired.scheduled_at_ms,
                        occ_kind,
                        advance,
                    )
                    .await;

                let event_json = serde_json::json!({
                    "type": "cron.fired",
                    "job": serde_json::to_value(&fired.job).unwrap_or_default(),
                });
                events.lock().await.push(event_json.clone());
                // Push 模型：broadcast 给所有 cron.subscribe 长连。无 subscriber
                // 时 send 返 SendError 是预期路径（仍走 events queue + poll_events）。
                let _ = broadcaster.send(event_json);
            }
        }
    }
}

async fn shutdown_all(
    shutdown: CancellationToken,
    scheduler_handle: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    ipc_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    owner_log_task: Option<tokio::task::JoinHandle<()>>,
    missed_outbox_task: tokio::task::JoinHandle<()>,
    outbox_runtime: (CronLedgerWriter, tokio::task::JoinHandle<()>),
    cmd_tx: mpsc::Sender<CronCommand>,
) {
    shutdown.cancel();
    if let Some(scheduler_handle) = scheduler_handle {
        let _ = tokio::time::timeout(Duration::from_secs(5), scheduler_handle).await;
    }
    drop(cmd_tx); // 关闭 cmd_rx 端，让 ipc serve 内部 sender clone 也最终终止
    if let Some(ipc_handle) = ipc_handle {
        let _ = tokio::time::timeout(Duration::from_secs(5), ipc_handle).await;
    }
    if let Some(owner_log_task) = owner_log_task {
        let _ = tokio::time::timeout(Duration::from_secs(2), owner_log_task).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), missed_outbox_task).await;
    // producer clones 已结束后 drop 最后一个 sender，让唯一 writer 把 channel
    // 中已接收的请求串行写完再退出。
    let (outbox_writer, outbox_writer_task) = outbox_runtime;
    drop(outbox_writer);
    let _ = tokio::time::timeout(Duration::from_secs(2), outbox_writer_task).await;
}

async fn cleanup_runtime_files(
    pid_file: &std::path::Path,
    sock_path: &std::path::Path,
    cron_dir: &std::path::Path,
    identity: &str,
) {
    // Startup may fail before this process publishes readiness. Never remove
    // another daemon's PID/build-id merely because the paths coincide.
    let owns_pid_file = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if owns_pid_file {
        if let Err(error) = std::fs::remove_file(pid_file)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %error, "删除本进程 PID 文件失败");
        }
        // F1：build-id 文件与 owned pid 文件同生命周期清理。
        let _ = std::fs::remove_file(acosmi_daemon_launcher::build_id::build_id_file("cron"));
    }
    // 兜底释放 scheduler 主锁（daemon.run 退出时已 release，这里幂等）
    if let Err(e) = acosmi_scheduler::release_lock(cron_dir, identity).await {
        tracing::warn!(error = %e, "释放 scheduler 锁失败（可能已释放）");
    }
    // W-CRON-DIR-UNIFY PR-1：过渡期一并释放旧业务锁（幂等；纯新装从未获取该锁 →
    // NotFound no-op，且绝不物化 `.crabcode/`）。失败只 warn 不阻断，同主锁风格。
    if acosmi_scheduler::HOLD_LEGACY_LOCK
        && let Err(e) = acosmi_scheduler::release_legacy_lock(cron_dir, identity).await
    {
        tracing::warn!(error = %e, "释放 legacy scheduler 锁失败（可能已释放）");
    }
    let _ = std::fs::remove_file(sock_path);
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // log file（best-effort）：daemonize 已把 stdout/stderr 重定向到 /dev/null，
    // 真实落地走 ~/.crabcode/logs/cron.log
    let log_file_path: PathBuf = paths::log_symlink("cron");
    let _ = std::fs::create_dir_all(paths::log_dir());
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .try_init();
    } else {
        // 兜底：log 目录写不了就上 stderr（grandchild 已 dup2 到 /dev/null，
        // 这里只对 fg 调试模式可见）
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }
}

fn spawn_signal_handler(cancel: CancellationToken) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Register synchronously before the ready PID is published. A caller
        // can therefore never observe ready and race a default-action SIGTERM
        // ahead of handler installation.
        let mut term = signal(SignalKind::terminate()).context("无法注册 SIGTERM 处理器")?;
        let mut int_sig = signal(SignalKind::interrupt()).context("无法注册 SIGINT 处理器")?;
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => tracing::info!("收到 SIGTERM"),
                _ = int_sig.recv() => tracing::info!("收到 SIGINT"),
            }
            cancel.cancel();
        });
    }
    #[cfg(windows)]
    {
        use acosmi_daemon_launcher::windows_event;

        // Create the event synchronously before the ready PID can be
        // published. launcher::stop("cron") can therefore always OpenEventW
        // for a current-generation ready daemon.
        let event = windows_event::create_event("cron")
            .context("创建 cron Windows soft-stop event 失败")?;
        let sendable_event = windows_event::SendableEventHandle(event);
        let event_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("cron-soft-stop".to_owned())
            .spawn(move || {
                let result = windows_event::wait_for_event_or_cancelled(
                    sendable_event,
                    Duration::from_millis(200),
                    || event_cancel.is_cancelled(),
                );
                match result {
                    Ok(true) => {
                        tracing::info!("收到 Windows soft-stop event");
                        event_cancel.cancel();
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(error = %error, "Windows soft-stop event wait 失败");
                        // A broken shutdown channel invalidates the ready
                        // lifecycle contract; fail the runtime closed.
                        event_cancel.cancel();
                    }
                }
            })
            .context("启动 cron Windows soft-stop waiter 失败")?;

        let ctrl_cancel = cancel.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    tracing::info!("收到 CTRL_C / CTRL_BREAK");
                    ctrl_cancel.cancel();
                }
                Err(error) => {
                    tracing::error!(error = %error, "无法注册 ctrl_c 处理器");
                }
            }
        });
    }
    Ok(())
}

/// Open the fire-path SQLite ledger under `dir` (creating the `cron`
/// parent), at the SAME path the actor derives via [`cron_ledger_db_path`] — so
/// a test-opened reader and the actor's reopen-on-panic hit one file. Shared by
/// the actor unit tests (`outbox_writer_tests`) and the IPC delivery tests
/// (`ipc::tests`), both of which now must hand the writer a ledger connection.
#[cfg(test)]
fn open_test_ledger(dir: &std::path::Path) -> LedgerConnection {
    let db_path = cron_ledger_db_path(dir);
    std::fs::create_dir_all(db_path.parent().expect("ledger db parent"))
        .expect("create ledger dir");
    LedgerConnection::open(&db_path).expect("open test ledger")
}

#[cfg(test)]
mod outbox_writer_tests {
    use super::*;
    use acosmi_scheduler::{
        CronJob, CronJobState, CronPayload, CronSchedule, PayloadKind, ScheduleKind, SessionTarget,
        WakeMode,
    };
    use tokio::sync::Barrier;

    fn test_job(id: &str) -> CronJob {
        CronJob {
            id: id.to_string(),
            agent_id: None,
            name: format!("job-{id}"),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".to_string()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some(format!("run-{id}")),
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

    #[tokio::test]
    async fn single_writer_serializes_controlled_normal_and_missed_interleaving() {
        const ROUNDS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:writer-test".to_string(),
            "00000000-0000-4000-8000-000000000001".to_string(),
            ledger,
        )
        .await
        .unwrap();
        let round_barrier = Arc::new(Barrier::new(2));

        let normal_writer = writer.clone();
        let normal_barrier = Arc::clone(&round_barrier);
        let normal = tokio::spawn(async move {
            let mut ids = Vec::with_capacity(ROUNDS);
            for round in 0..ROUNDS {
                normal_barrier.wait().await;
                let id = normal_writer
                    .append_job(
                        &test_job(&format!("normal-{round}")),
                        OutboxEventType::Fired,
                        10_000 + round as i64,
                        OccurrenceKind::Scheduled,
                        FireAdvance::NoAdvance,
                    )
                    .await
                    .expect("single writer must accept normal fire");
                ids.push(id);
            }
            ids
        });

        let missed_writer = writer.clone();
        let missed_barrier = Arc::clone(&round_barrier);
        let missed = tokio::spawn(async move {
            let mut ids = Vec::with_capacity(ROUNDS);
            for round in 0..ROUNDS {
                missed_barrier.wait().await;
                let id = missed_writer
                    .append_job(
                        &test_job(&format!("missed-{round}")),
                        OutboxEventType::Skipped,
                        20_000 + round as i64,
                        OccurrenceKind::MissedSkipped,
                        FireAdvance::Delete,
                    )
                    .await
                    .expect("single writer must accept missed event");
                ids.push(id);
            }
            ids
        });

        let mut allocated = normal.await.unwrap();
        allocated.extend(missed.await.unwrap());
        allocated.sort_unstable();
        assert_eq!(allocated, (1_u64..=16).collect::<Vec<_>>());

        drop(writer);
        writer_task.await.unwrap();

        let raw = tokio::fs::read_to_string(outbox::outbox_path(dir.path()))
            .await
            .unwrap();
        let persisted_ids: Vec<u64> = raw
            .lines()
            .map(|line| serde_json::from_str::<OutboxEvent>(line).unwrap().event_id)
            .collect();
        assert_eq!(
            persisted_ids,
            (1_u64..=16).collect::<Vec<_>>(),
            "allocation and physical append must share the same serial writer"
        );
    }

    #[tokio::test]
    async fn single_writer_uses_js_max_safe_once_then_refuses_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            outbox::MAX_SAFE_EVENT_ID,
            "cron-state-v1:writer-test".to_string(),
            "00000000-0000-4000-8000-000000000001".to_string(),
            ledger,
        )
        .await
        .unwrap();
        let first = writer
            .append_job(
                &test_job("last"),
                OutboxEventType::Fired,
                1000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await;
        let second = writer
            .append_job(
                &test_job("overflow"),
                OutboxEventType::Fired,
                1001,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await;
        assert_eq!(first, Some(outbox::MAX_SAFE_EVENT_ID));
        assert_eq!(
            second, None,
            "writer must not exceed or reuse JavaScript MAX_SAFE_INTEGER"
        );

        drop(writer);
        writer_task.await.unwrap();
        let events = outbox::read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, outbox::MAX_SAFE_EVENT_ID);
    }

    #[tokio::test]
    async fn single_actor_serializes_parallel_delivery_begin() {
        const STATE: &str = "cron-state-v1:parallel-delivery";
        const EPOCH: &str = "00000000-0000-4000-8000-000000000002";
        let dir = tempfile::tempdir().unwrap();
        let event = OutboxEvent::from_job(&test_job("parallel"), 7, OutboxEventType::Fired, 1000);
        outbox::append_durable(dir.path(), &event).await.unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            8,
            STATE.to_string(),
            EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();
        let first_writer = writer.clone();
        let first_event = event.clone();
        let first = tokio::spawn(async move {
            first_writer
                .delivery_begin(
                    STATE.to_string(),
                    EPOCH.to_string(),
                    first_event,
                    delivery::DeliveryConsumer {
                        kind: delivery::DeliveryConsumerKind::Tui,
                        id: "tui-a".to_string(),
                    },
                )
                .await
                .unwrap()
        });
        let second_writer = writer.clone();
        let second = tokio::spawn(async move {
            second_writer
                .delivery_begin(
                    STATE.to_string(),
                    EPOCH.to_string(),
                    event,
                    delivery::DeliveryConsumer {
                        kind: delivery::DeliveryConsumerKind::Tui,
                        id: "tui-b".to_string(),
                    },
                )
                .await
                .unwrap()
        });
        let outcomes = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, delivery::DeliveryBeginOutcome::Acquired(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, delivery::DeliveryBeginOutcome::Busy { .. }))
                .count(),
            1
        );

        drop(writer);
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn single_actor_rejects_missing_and_forged_events_before_journal_mutation() {
        const STATE: &str = "cron-state-v1:membership";
        const EPOCH: &str = "00000000-0000-4000-8000-000000000004";
        let dir = tempfile::tempdir().unwrap();
        let retained =
            OutboxEvent::from_job(&test_job("retained"), 7, OutboxEventType::Fired, 1000);
        outbox::append_durable(dir.path(), &retained).await.unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            8,
            STATE.to_string(),
            EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();
        let consumer = || delivery::DeliveryConsumer {
            kind: delivery::DeliveryConsumerKind::Tui,
            id: "client".to_string(),
        };

        let future = OutboxEvent::from_job(&test_job("future"), 8, OutboxEventType::Fired, 1001);
        assert!(matches!(
            writer
                .delivery_begin(STATE.to_string(), EPOCH.to_string(), future, consumer(),)
                .await,
            Err(delivery::DeliveryError::EventNotRetained(8))
        ));

        let mut forged = retained;
        forged.payload_message = Some("forged".to_string());
        assert!(matches!(
            writer
                .delivery_begin(STATE.to_string(), EPOCH.to_string(), forged, consumer(),)
                .await,
            Err(delivery::DeliveryError::EventIdentityCollision(7))
        ));

        let journal_dir = dir.path().join("cron/cron_delivery_v3").join(EPOCH);
        assert!(!journal_dir.join("evt-7.jsonl").exists());
        assert!(!journal_dir.join("evt-8.jsonl").exists());

        drop(writer);
        writer_task.await.unwrap();
    }

    #[tokio::test]
    async fn outbox_durability_failure_makes_delivery_actor_fail_closed() {
        const STATE: &str = "cron-state-v1:degraded-delivery";
        const EPOCH: &str = "00000000-0000-4000-8000-000000000003";
        let dir = tempfile::tempdir().unwrap();
        let retained = OutboxEvent::from_job(&test_job("retained"), 1, OutboxEventType::Fired, 999);
        outbox::append_durable(dir.path(), &retained).await.unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            2,
            STATE.to_string(),
            EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();
        tokio::fs::remove_file(outbox::outbox_path(dir.path()))
            .await
            .unwrap();
        tokio::fs::create_dir_all(outbox::outbox_path(dir.path()))
            .await
            .unwrap();
        assert_eq!(
            writer
                .append_job(
                    &test_job("broken"),
                    OutboxEventType::Fired,
                    1000,
                    OccurrenceKind::Scheduled,
                    FireAdvance::NoAdvance,
                )
                .await,
            Some(2),
            "allocator consumes the id even when durable append fails"
        );
        let consumer = || delivery::DeliveryConsumer {
            kind: delivery::DeliveryConsumerKind::Tui,
            id: "client".to_string(),
        };
        assert!(matches!(
            writer
                .delivery_begin(
                    STATE.to_string(),
                    EPOCH.to_string(),
                    retained.clone(),
                    consumer(),
                )
                .await,
            Err(delivery::DeliveryError::OutboxStateUnavailable(_))
        ));
        let future = OutboxEvent::from_job(&test_job("future"), 2, OutboxEventType::Fired, 1000);
        assert!(matches!(
            writer
                .delivery_begin(STATE.to_string(), EPOCH.to_string(), future, consumer(),)
                .await,
            Err(delivery::DeliveryError::EventNotRetained(2))
        ));
        let mut forged = retained;
        forged.payload_message = Some("forged".to_string());
        assert!(matches!(
            writer
                .delivery_begin(STATE.to_string(), EPOCH.to_string(), forged, consumer(),)
                .await,
            Err(delivery::DeliveryError::EventIdentityCollision(1))
        ));

        drop(writer);
        writer_task.await.unwrap();
    }

    /// W1c-B: the fire actor records a durable SQLite occurrence on every fire,
    /// keyed by the theoretical `scheduled_at_ms` and sharing ONE derived
    /// identity with the outbox append (`legacy_event_id` = the outbox
    /// `event_id`). A second fire of the SAME `(job_id, scheduled_at_ms)` appends
    /// another outbox row (fresh monotonic id) but is an idempotent no-op in the
    /// ledger — exactly one occurrence. Purely local: no daemon, no named pipe.
    #[tokio::test]
    async fn append_job_records_durable_occurrence_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:occurrence-test".to_string(),
            "00000000-0000-4000-8000-000000000009".to_string(),
            ledger,
        )
        .await
        .unwrap();

        let job = test_job("occ-rec");
        let sched = 1_800_000_000_000_i64;
        let advance = || FireAdvance::Recurring {
            new_job_json: serde_json::to_string(&job).unwrap(),
        };

        let first_id = writer
            .append_job(
                &job,
                OutboxEventType::Fired,
                sched,
                OccurrenceKind::Scheduled,
                advance(),
            )
            .await
            .expect("first fire must allocate an event id");
        // Same (job_id, scheduled_at_ms) → idempotent occurrence, fresh outbox id.
        let second_id = writer
            .append_job(
                &job,
                OutboxEventType::Fired,
                sched,
                OccurrenceKind::Scheduled,
                advance(),
            )
            .await
            .expect("second fire must allocate a new event id");
        assert_eq!(
            (first_id, second_id),
            (1_u64, 2_u64),
            "each fire consumes a fresh monotonic outbox event id"
        );

        drop(writer);
        writer_task.await.unwrap();

        // Store 1 — durable SQLite occurrences: exactly one row for the firing,
        // right key + kind, carrying the FIRST fire's outbox id as legacy identity.
        let reader = open_test_ledger(dir.path());
        let occurrences = reader
            .occurrences_for_job(&job.id)
            .expect("read occurrences");
        assert_eq!(
            occurrences.len(),
            1,
            "duplicate (job_id, scheduled_at_ms) must not create a second occurrence"
        );
        assert_eq!(occurrences[0].scheduled_at_ms, sched);
        assert_eq!(occurrences[0].occurrence_kind, "scheduled");
        assert_eq!(
            occurrences[0].legacy_event_id,
            Some(i64::try_from(first_id).unwrap()),
            "occurrence and outbox row share one derived identity (first fire's event id)"
        );

        // W2-1: every fire also captured the immutable execution envelope. The
        // actor projects it from the fired job and persists it on the occurrence.
        // `test_job` routes to SessionTarget::Main, so the denormalized `target`
        // column is 'main' and the serialized envelope round-trips to the same
        // target + resolved message.
        assert_eq!(
            occurrences[0].target.as_deref(),
            Some("main"),
            "occurrence.target must be the fired job's session target"
        );
        let envelope_json = occurrences[0]
            .envelope_json
            .as_deref()
            .expect("fire must persist a non-null envelope_json");
        let envelope: acosmi_cron_ledger::ExecutionEnvelope =
            serde_json::from_str(envelope_json).expect("persisted envelope_json parses");
        assert_eq!(envelope.target, acosmi_cron_ledger::EnvelopeTarget::Main);
        assert_eq!(
            envelope.message.as_deref(),
            Some("run-occ-rec"),
            "envelope message mirrors payload.message"
        );

        // Store 2 — append-only outbox JSONL: BOTH fires are recorded (the outbox
        // is the monotonic event ledger; it does not dedup like the occurrence).
        let events = outbox::read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.event_id).collect::<Vec<_>>(),
            vec![first_id, second_id],
            "every fire appends an outbox event even when the occurrence dedups"
        );
        assert!(
            events
                .iter()
                .all(|e| e.job_id == job.id && e.scheduled_at_ms == sched)
        );
    }

    /// PR5-W2-3: the actor exposes the ledger claim over its channel. Fire one
    /// Main-target occurrence (via the same AppendJob path production uses, which
    /// persists the projected `ExecutionEnvelope`), then `claim_next` hands it
    /// back with the actor-minted lease + fence bumped 0→1 + the immutable
    /// envelope. A second claim of the same target finds the single occurrence
    /// already leased and returns `NoWork`. Purely local: tempdir ledger,
    /// in-process actor, no daemon / named pipe.
    #[tokio::test]
    async fn claim_next_hands_fired_occurrence_then_reports_no_work() {
        use acosmi_cron_ledger::{ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget};

        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:claim-test".to_string(),
            "00000000-0000-4000-8000-00000000000c".to_string(),
            ledger,
        )
        .await
        .unwrap();

        let scheduled_at_ms = 1_800_000_042_000_i64;
        writer
            .append_job(
                &test_job("claimme"),
                OutboxEventType::Fired,
                scheduled_at_ms,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire must record a durable, claimable occurrence");

        let consumer = ConsumerClaim {
            kind: ConsumerKind::Tui,
            id: "tui-claimer".to_string(),
            target: EnvelopeTarget::Main,
        };
        let claimed = match writer
            .claim_next(consumer.clone())
            .await
            .expect("claim must succeed")
        {
            ClaimOutcome::Claimed(claimed) => claimed,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence, got NoWork"),
        };
        assert_eq!(claimed.job_id, "claimme");
        assert_eq!(claimed.scheduled_at_ms, scheduled_at_ms);
        assert_eq!(
            claimed.fence, 1,
            "first claim bumps the occurrence fence 0 -> 1"
        );
        assert!(
            !claimed.lease_token.is_empty(),
            "the actor must mint a non-empty lease token"
        );
        assert_eq!(claimed.envelope.target, EnvelopeTarget::Main);
        assert_eq!(
            claimed.envelope.message.as_deref(),
            Some("run-claimme"),
            "the claimed envelope carries the fired job's resolved message"
        );

        // The single occurrence is now leased at its current fence, so a second
        // claim of the same target has nothing runnable.
        let second = writer
            .claim_next(consumer)
            .await
            .expect("second claim must succeed");
        assert_eq!(second, ClaimOutcome::NoWork);

        drop(writer);
        writer_task.await.unwrap();
    }

    /// PR5-W3-3: the actor exposes the W3-1 accept writeback. Fire → claim →
    /// `accept` records the run and advances the occurrence to `completed`; the
    /// `occurrence_history` read reflects the joined occurrence ⋈ attempt ⋈
    /// result. A second accept at the same (now terminal) fence writes nothing
    /// and returns `Fenced`. Purely local: tempdir ledger, in-process actor.
    #[tokio::test]
    async fn accept_writeback_records_completed_then_second_is_fenced() {
        use acosmi_cron_ledger::{
            ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, WritebackOutcome,
        };
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:accept-test".to_string(),
            "00000000-0000-4000-8000-00000000000d".to_string(),
            ledger,
        )
        .await
        .unwrap();

        writer
            .append_job(
                &test_job("acc"),
                OutboxEventType::Fired,
                1_800_000_100_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire must record a claimable occurrence");
        let consumer = ConsumerClaim {
            kind: ConsumerKind::Tui,
            id: "tui".to_string(),
            target: EnvelopeTarget::Main,
        };
        let claimed = match writer.claim_next(consumer).await.expect("claim") {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let outcome = writer
            .accept_occurrence(
                claimed.occurrence_id,
                claimed.attempt_id,
                claimed.fence,
                Some(42),
            )
            .await
            .expect("accept");
        assert_eq!(
            outcome,
            WritebackOutcome::Recorded {
                occurrence_id: claimed.occurrence_id,
                fence: claimed.fence,
            }
        );

        // occurrence_history reflects the joined occurrence ⋈ attempt ⋈ result.
        let history = writer
            .occurrence_history("acc".to_string())
            .await
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].occurrence_id, claimed.occurrence_id);
        assert_eq!(history[0].status, "completed");
        assert_eq!(history[0].fence, claimed.fence);
        assert_eq!(history[0].target.as_deref(), Some("main"));
        assert_eq!(history[0].attempt_id, Some(claimed.attempt_id));
        assert_eq!(history[0].outcome.as_deref(), Some("accepted"));
        assert_eq!(history[0].result_status.as_deref(), Some("success"));
        assert_eq!(history[0].duration_ms, Some(42));

        // A second accept at the same fence: the occurrence is already terminal,
        // so the gate rejects it and nothing else is written.
        let second = writer
            .accept_occurrence(
                claimed.occurrence_id,
                claimed.attempt_id,
                claimed.fence,
                Some(7),
            )
            .await
            .expect("second accept");
        assert_eq!(
            second,
            WritebackOutcome::Fenced {
                occurrence_id: claimed.occurrence_id,
                expected_fence: claimed.fence,
                actual_fence: Some(claimed.fence),
            }
        );

        drop(writer);
        writer_task.await.unwrap();
    }

    /// PR5-W3-3: report_failure closes the occurrence as `failed` (lease
    /// `abandoned`, result `error`), then retry resets it to `pending` at a
    /// bumped fence — making it re-claimable by a subsequent claim.
    #[tokio::test]
    async fn report_failure_then_retry_makes_occurrence_reclaimable() {
        use acosmi_cron_ledger::{
            ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, ReportedFailureKind,
            RetryOutcome, WritebackOutcome,
        };
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:fail-retry".to_string(),
            "00000000-0000-4000-8000-00000000000f".to_string(),
            ledger,
        )
        .await
        .unwrap();

        writer
            .append_job(
                &test_job("fr"),
                OutboxEventType::Fired,
                1_800_000_200_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire");
        let consumer = ConsumerClaim {
            kind: ConsumerKind::Tui,
            id: "tui".to_string(),
            target: EnvelopeTarget::Main,
        };
        let claimed = match writer.claim_next(consumer.clone()).await.expect("claim") {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let outcome = writer
            .report_failure_occurrence(
                claimed.occurrence_id,
                claimed.attempt_id,
                claimed.fence,
                ReportedFailureKind::Error,
                Some("boom".to_string()),
                Some(5),
            )
            .await
            .expect("report_failure");
        assert_eq!(
            outcome,
            WritebackOutcome::Recorded {
                occurrence_id: claimed.occurrence_id,
                fence: claimed.fence,
            }
        );
        let history = writer
            .occurrence_history("fr".to_string())
            .await
            .expect("history");
        assert_eq!(history[0].status, "failed");
        assert_eq!(history[0].outcome.as_deref(), Some("abandoned"));
        assert_eq!(history[0].result_status.as_deref(), Some("error"));
        assert_eq!(history[0].error_message.as_deref(), Some("boom"));

        // retry resets the failed occurrence to pending at a bumped fence.
        let retry = writer
            .retry_occurrence(claimed.occurrence_id)
            .await
            .expect("retry");
        assert_eq!(
            retry,
            RetryOutcome::Retried {
                occurrence_id: claimed.occurrence_id,
                fence: claimed.fence + 1,
            }
        );

        // The occurrence is now re-claimable; the re-claim bumps the fence once
        // more (retry +1, then claim +1).
        let reclaimed = match writer.claim_next(consumer).await.expect("reclaim") {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("retry must make the occurrence re-claimable"),
        };
        assert_eq!(reclaimed.occurrence_id, claimed.occurrence_id);
        assert_eq!(reclaimed.fence, claimed.fence + 2);

        drop(writer);
        writer_task.await.unwrap();
    }

    /// PR5-W3-3: cancel terminates a `claimed` occurrence as `skipped`, abandons
    /// its in-flight lease, and bumps the fence — so the cancelled claimer's late
    /// accept is `Fenced`.
    #[tokio::test]
    async fn cancel_terminates_claimed_occurrence_and_fences_late_writeback() {
        use acosmi_cron_ledger::{
            CancelOutcome, ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget,
            WritebackOutcome,
        };
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:cancel-test".to_string(),
            "00000000-0000-4000-8000-000000000010".to_string(),
            ledger,
        )
        .await
        .unwrap();

        writer
            .append_job(
                &test_job("cx"),
                OutboxEventType::Fired,
                1_800_000_300_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire");
        let claimed = match writer
            .claim_next(ConsumerClaim {
                kind: ConsumerKind::Tui,
                id: "tui".to_string(),
                target: EnvelopeTarget::Main,
            })
            .await
            .expect("claim")
        {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let cancel = writer
            .cancel_occurrence(claimed.occurrence_id)
            .await
            .expect("cancel");
        assert_eq!(
            cancel,
            CancelOutcome::Cancelled {
                occurrence_id: claimed.occurrence_id,
                fence: claimed.fence + 1,
            }
        );
        let history = writer
            .occurrence_history("cx".to_string())
            .await
            .expect("history");
        assert_eq!(history[0].status, "skipped");
        assert_eq!(history[0].outcome.as_deref(), Some("abandoned"));

        // The cancelled claimer's late accept carries the stale fence → Fenced,
        // and the occurrence (already terminal at the bumped fence) is untouched.
        let late = writer
            .accept_occurrence(
                claimed.occurrence_id,
                claimed.attempt_id,
                claimed.fence,
                None,
            )
            .await
            .expect("late accept");
        assert_eq!(
            late,
            WritebackOutcome::Fenced {
                occurrence_id: claimed.occurrence_id,
                expected_fence: claimed.fence,
                actual_fence: Some(claimed.fence + 1),
            }
        );

        drop(writer);
        writer_task.await.unwrap();
    }

    /// PR5-W3-3: the claim handler runs an opportunistic expire sweep before each
    /// claim. Using injected clocks on a plain connection, fire → claim → re-claim
    /// after the first lease expires (leaving the first attempt unresolved at a
    /// superseded fence). A subsequent claim THROUGH THE ACTOR then finds that
    /// stale attempt and marks it `expired`.
    #[tokio::test]
    async fn claim_handler_opportunistic_sweep_expires_superseded_attempt() {
        use acosmi_cron_ledger::{ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget};

        let dir = tempfile::tempdir().unwrap();
        let job = test_job("sweep");
        // Far-future schedule so the actor's wall-clock claim below still sees the
        // second (re-claim) lease as live → it finds no runnable work, proving the
        // 'expired' marking came from the sweep and not from another re-claim.
        let sched = 4_000_000_000_000_i64;
        let consumer = ConsumerClaim {
            kind: ConsumerKind::Tui,
            id: "tui".to_string(),
            target: EnvelopeTarget::Main,
        };

        // Phase 1 — plain connection, injected clocks. Fire, claim #1, then
        // re-claim #2 after claim #1's lease has expired. The re-claim bumps the
        // occurrence fence 1 → 2 and leaves attempt a1 (fence 1) unresolved.
        {
            let mut setup = open_test_ledger(dir.path());
            let envelope = acosmi_cron_ledger::ExecutionEnvelope::from_job(&job);
            setup
                .fire_occurrence(
                    &job.id,
                    sched,
                    OccurrenceKind::Scheduled,
                    sched,
                    job.payload.message.as_deref(),
                    Some(job.name.as_str()),
                    None,
                    &envelope,
                    FireAdvance::NoAdvance,
                )
                .expect("fire on plain connection");
            match setup
                .claim_next_occurrence(&consumer, sched, CLAIM_LEASE_MS, "lease-1")
                .expect("claim #1")
            {
                ClaimOutcome::Claimed(c) => assert_eq!(c.fence, 1, "claim #1 bumps fence 0 -> 1"),
                ClaimOutcome::NoWork => panic!("claim #1 must claim the fired occurrence"),
            }
            match setup
                .claim_next_occurrence(
                    &consumer,
                    sched + CLAIM_LEASE_MS + 1,
                    CLAIM_LEASE_MS,
                    "lease-2",
                )
                .expect("re-claim #2")
            {
                ClaimOutcome::Claimed(c) => assert_eq!(
                    c.fence, 2,
                    "an expired-lease re-claim must bump the occurrence fence to 2"
                ),
                ClaimOutcome::NoWork => panic!("the expired lease must be re-claimable"),
            }
        }

        // Phase 2 — hand a fresh connection to the actor and run a claim. The
        // claim handler's opportunistic sweep runs first and expires the
        // superseded a1 (fence 1 < occurrence fence 2, still unresolved). The live
        // far-future lease on a2 leaves nothing runnable.
        let ledger = open_test_ledger(dir.path());
        let (writer, writer_task) = spawn_cron_ledger_writer(
            dir.path().to_path_buf(),
            1,
            "cron-state-v1:sweep".to_string(),
            "00000000-0000-4000-8000-000000000011".to_string(),
            ledger,
        )
        .await
        .unwrap();

        let outcome = writer.claim_next(consumer).await.expect("actor claim");
        assert_eq!(
            outcome,
            ClaimOutcome::NoWork,
            "the live re-claim lease leaves nothing claimable"
        );

        let history = writer
            .occurrence_history("sweep".to_string())
            .await
            .expect("history");
        let outcomes: Vec<Option<&str>> = history.iter().map(|r| r.outcome.as_deref()).collect();
        assert!(
            outcomes.contains(&Some("expired")),
            "the opportunistic sweep must expire the superseded attempt: {outcomes:?}"
        );
        assert!(
            outcomes.contains(&None),
            "the live re-claim lease must remain unresolved: {outcomes:?}"
        );

        drop(writer);
        writer_task.await.unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    // main 的功能性测试以集成测试 + 单元测试 ipc.rs 内部函数为主。
    // 这里仅放一些沙盒级单元测试，验证启动期路径解析不 panic。

    use super::*;

    #[test]
    fn cron_dir_resolves() {
        // 不依赖 env 也能跑出一个路径
        let dir = acosmi_config::paths::resolve_state_dir();
        assert!(dir.is_absolute() || dir.exists() || !dir.as_os_str().is_empty());
    }

    #[test]
    fn paths_consistent() {
        // pid / lock / socket 都在 run_dir 下
        let run = paths::run_dir();
        assert!(paths::pid_file("cron").starts_with(&run));
        assert!(paths::lock_file("cron").starts_with(&run));
        assert!(paths::socket_file("cron").starts_with(&run));
    }

    #[tokio::test]
    async fn startup_cleanup_never_removes_a_foreign_pid_file() {
        let runtime = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let pid_file = runtime.path().join("cron.pid");
        let sock_path = runtime.path().join("cron.sock");
        std::fs::write(&pid_file, "424242").unwrap();

        cleanup_runtime_files(&pid_file, &sock_path, state.path(), "identity-not-present").await;

        assert_eq!(std::fs::read_to_string(&pid_file).unwrap(), "424242");
    }
}

/// Cron storage layout tripwire.
///
/// 与 acosmi-scheduler 的 `layout_contract` 模块配对：本 crate 自有的
/// `DELIVERY_STORE_REL` 与 ledger db 派生路径同属一个布局整体，
/// 禁止单点改动，以免造成锁分裂或数据分叉。
///
/// 置于 crate 根级 `#[cfg(test)]`（**非** unix-gated `mod tests`）：布局常量
/// 在所有平台字节一致，契约 tripwire 必须在 Windows CI 同样生效。
#[cfg(test)]
mod layout_contract {
    use crate::{cron_ledger_db_path, delivery};

    #[test]
    fn delivery_store_rel_is_pinned_and_shares_the_scheduler_layout_dir() {
        assert_eq!(delivery::DELIVERY_STORE_REL, "cron/cron_delivery_v3");

        let dir = tempfile::tempdir().unwrap();
        let json = acosmi_scheduler::task_store::cron_file_path(dir.path());
        let delivery_root = dir.path().join(delivery::DELIVERY_STORE_REL);
        assert_eq!(
            delivery_root.parent(),
            json.parent(),
            "cron_delivery_v3 必须与 scheduled_tasks.json 同父目录 —— 布局分叉"
        );
    }

    /// 同卷约束的测试化：ledger db 与 `scheduled_tasks.json` 同目录 ⇒ 同卷，
    /// 迁移 `.bak` 与 ledger 永不跨挂载点（呼应
    /// `acosmi-cron-ledger/src/lib.rs` `LEDGER_DB_FILENAME` 注释）。
    #[test]
    fn ledger_db_path_lives_beside_scheduled_tasks_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = cron_ledger_db_path(dir.path());
        let json = acosmi_scheduler::task_store::cron_file_path(dir.path());
        assert_eq!(
            db.parent(),
            json.parent(),
            "cron_ledger.db 必须与 scheduled_tasks.json 同父目录（同卷约束）"
        );
        assert_eq!(
            db.file_name().and_then(|n| n.to_str()),
            Some(acosmi_cron_ledger::LEDGER_DB_FILENAME),
            "ledger db 文件名必须来自 LEDGER_DB_FILENAME 单一真源"
        );
    }
}
