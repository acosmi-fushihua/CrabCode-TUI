use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::daily_log::{append_daily_log, SessionEvent, TranscriptMeta as DailyLogTranscriptMeta};
use crate::extract_archive::{archive_runner_completed, now_ms, RunnerArchiveRecord};
use crate::extract_cursor::{complete_extract_success, load_extract_cursor, save_extract_cursor};
use crate::lock;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 2026-07-27 §25.1 / §27.3-G7 —— 每条 lane 的**往返**超时预算。
///
/// 关键：这与 `tier3_auto_dream::LLM_CALL_TIMEOUT_MS`（60s）**不是一回事**。
/// 那个是**单次 `call_llm`** 的预算；这里卡的是**整条 runner 往返**——做梦
/// 一条往返含 phase0..phase4 多次串行 LLM 调用加解析与落盘。把 60s 套到往返
/// 上会把正常工作的 runner 判死，并触发 `lock::rollback` 误回滚整理锁。
///
/// 抽取 = 一次 LLM 调用 + 落盘。
pub const EXTRACT_RUNNER_TTL_MS: u64 = 10 * 60 * 1_000;
/// 做梦 = 多相串行往返，预算按数量级放大。
pub const DREAM_RUNNER_TTL_MS: u64 = 45 * 60 * 1_000;
/// 未知 kind 的兜底预算（取两者较大者的语义：宁可晚判死也别误杀）。
pub const DEFAULT_RUNNER_TTL_MS: u64 = DREAM_RUNNER_TTL_MS;
/// 超时墓碑上限。墓碑的唯一用途是让**迟到回执**可被识别并告警，而不是
/// 落进 `known_trigger:false` 那个静音分支里 —— 有界即可。
const TIMED_OUT_TOMBSTONE_CAP: usize = 256;

#[must_use]
pub fn runner_ttl_ms(kind: &str) -> u64 {
    match kind {
        "extract" => EXTRACT_RUNNER_TTL_MS,
        "dream" => DREAM_RUNNER_TTL_MS,
        _ => DEFAULT_RUNNER_TTL_MS,
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PendingRunner {
    pub trigger_id: String,
    pub kind: String,
    pub session_id: String,
    pub memory_dir: PathBuf,
    pub project_state_dir: PathBuf,
    pub lock_token: Option<String>,
    pub prior_mtime_ms: Option<u64>,
    pub extract_last_assistant_uuid: Option<String>,
    pub extract_total_model_visible: Option<u64>,
    /// 登记时刻，用于 TTL 判定。0 = 调用方未提供（老测试构造）→ 永不超时。
    pub registered_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunnerCompleted {
    pub trigger_id: String,
    pub kind: String,
    pub written_paths: Vec<PathBuf>,
    pub usage: Option<Value>,
    pub error: Option<Value>,
    #[serde(default)]
    pub completed_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerCompletionReport {
    pub known_trigger: bool,
    pub lock_released: bool,
    pub rolled_back: bool,
    pub cursor_updated: bool,
    pub indexed_path_count: usize,
    /// W-MEMORY-ALIVE 4a (2026-07-01, 裁决②): `Some(memory_dir)` when a
    /// DREAM runner settled successfully — the caller (`memory.runner.completed`
    /// IPC arm) chains one imagination run for exactly that project. Taken
    /// from `pending.memory_dir` (not the wire payload), so a forged/mismatched
    /// completion cannot steer the chain at another directory.
    pub dream_settled_memory_dir: Option<PathBuf>,
    /// 该回执是在**本条 trigger 已被 TTL 判死之后**才到达的。此前这种情况
    /// 与"完全没见过的 trigger"一样落进 `known_trigger:false` 且一行日志都
    /// 不打 —— 正是 §25.4 那条家族缺陷（坏了看起来像没事）。
    pub late_after_timeout: bool,
}

#[derive(Default, Debug)]
pub struct ResultListener {
    pending: BTreeMap<String, PendingRunner>,
    /// 已被 TTL 判死的 trigger_id → 判死时刻。有界（[`TIMED_OUT_TOMBSTONE_CAP`]）。
    timed_out: BTreeMap<String, u64>,
}

impl ResultListener {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, pending: PendingRunner) {
        self.pending.insert(pending.trigger_id.clone(), pending);
    }

    pub fn discard(&mut self, trigger_id: &str) {
        self.pending.remove(trigger_id);
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 2026-07-27 §27.3-G8 —— 某项目是否已有同类 runner **在飞**。
    ///
    /// 这是在飞去重的**唯一真源**，且刻意只存在于内存里：进程重启 = 空表 =
    /// 天然放行。旧实现把 `in_progress` 落盘，再靠 `load_extract_cursor` 无
    /// 条件重置来处理"重启后 runner 已消失"，结果把**同进程内的并发去重**
    /// 也一并抹掉，使 `ExtractCursorSkip::InProgress` 成为生产不可达分支
    /// （实测 9 个项目里 7 个 `in_progress` 恒 true 且从未完成过一次抽取）。
    /// 用内存真源同时解决两件事，且不需要往状态文件里写任何进程身份 ——
    /// 后者会新造一份"判活"口径，正是本仓 cron 僵尸 PID 楔死的同族缺陷。
    #[must_use]
    pub fn has_in_flight(&self, kind: &str, project_state_dir: &Path) -> bool {
        self.pending
            .values()
            .any(|p| p.kind == kind && p.project_state_dir == project_state_dir)
    }

    /// 回收超过各自 lane TTL 的在飞 runner，并**按失败语义结算**（抽取回写
    /// 游标、做梦回滚整理锁），使它们不再无限占位。
    ///
    /// 不做这件事，`pending` 就只增不减：请求永不回执 ⇒ 条目在进程生命周期
    /// 内累积（实测某项目计数器 47 = 约 47 次 Run 决策 0 次回执）。而在
    /// `has_in_flight` 成为去重真源之后，它还会更严重 —— 一条卡死的 pending
    /// 会**真的**永久阻塞该项目的抽取。两者必须成对交付。
    pub async fn sweep_timeouts(&mut self, now_ms: u64) -> usize {
        let expired: Vec<String> = self
            .pending
            .values()
            .filter(|p| {
                p.registered_at_ms != 0
                    && now_ms.saturating_sub(p.registered_at_ms) > runner_ttl_ms(&p.kind)
            })
            .map(|p| p.trigger_id.clone())
            .collect();
        let mut swept = 0usize;
        for trigger_id in expired {
            let Some(pending) = self.pending.remove(&trigger_id) else {
                continue;
            };
            log::warn!(
                "runner {trigger_id} (kind={}) exceeded its {}ms round-trip budget with no \
                 completion — settling as failed and releasing the slot",
                pending.kind,
                runner_ttl_ms(&pending.kind),
            );
            if let Err(e) = self.settle_timed_out(&pending).await {
                log::warn!("runner {trigger_id} timeout settle failed (fail-soft): {e}");
            }
            self.remember_timed_out(trigger_id, now_ms);
            swept += 1;
        }
        swept
    }

    /// 超时结算：与 `handle_completed` 的失败分支保持**同一套语义**，
    /// 免得两条路径对同一状态机有两种理解。
    async fn settle_timed_out(&self, pending: &PendingRunner) -> Result<(), BoxError> {
        // 抽取无需结算：游标本就没被推进，而在飞标记随 `pending` 条目一起
        // 消失（这正是把在飞真源放进内存的好处）。做梦必须回滚整理锁，
        // 否则那把锁会一直挡住后续做梦。
        if pending.kind == "dream" {
            if let Some(prior) = pending.prior_mtime_ms {
                lock::rollback(&pending.memory_dir, prior).await?;
            }
        }
        Ok(())
    }

    fn remember_timed_out(&mut self, trigger_id: String, now_ms: u64) {
        self.timed_out.insert(trigger_id, now_ms);
        while self.timed_out.len() > TIMED_OUT_TOMBSTONE_CAP {
            // BTreeMap 按 trigger_id 排序；淘汰最旧的那条（按判死时刻）。
            let Some(oldest) = self
                .timed_out
                .iter()
                .min_by_key(|(_, ts)| **ts)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.timed_out.remove(&oldest);
        }
    }

    /// W-MEMORY-EVOLUTION PR-11 — the extract cursor is now persisted
    /// per-project (`<project_state_dir>/.memory-rust-derived/extract-cursor.json`)
    /// rather than held as a single shared in-memory `ExtractCursorState`. On
    /// completion we load the cursor for the completing trigger's own project
    /// (`pending.project_state_dir`), advance / roll it back via the existing
    /// state-machine helpers, then atomically save it back. This fixes both
    /// restart-duplicate-extraction (cursor survives process restart) and
    /// cross-project pollution (each project's cursor is independent).
    pub async fn handle_completed(
        &mut self,
        completed: RunnerCompleted,
    ) -> Result<RunnerCompletionReport, BoxError> {
        let Some(pending) = self.pending.remove(&completed.trigger_id) else {
            // §27.3-G7：这条分支此前**完全静音**（返回一个全 false 的报告，
            // 不打日志）。加了 TTL 之后它会真的被走到（超时判死后回执才到），
            // 静音就等于把"抽取真的成功了但结果被丢弃"变成不可观测。
            let late_after_timeout = self.timed_out.contains_key(&completed.trigger_id);
            if late_after_timeout {
                log::warn!(
                    "runner {} completed AFTER its timeout — result discarded (the slot was \
                     already released and the cursor settled as failed); the same window will \
                     be re-extracted",
                    completed.trigger_id,
                );
            } else {
                log::warn!(
                    "runner completion for unknown trigger {} — ignored (never registered, or \
                     the orchestrator restarted since)",
                    completed.trigger_id,
                );
            }
            return Ok(RunnerCompletionReport {
                known_trigger: false,
                lock_released: false,
                rolled_back: false,
                cursor_updated: false,
                indexed_path_count: 0,
                dream_settled_memory_dir: None,
                late_after_timeout,
            });
        };

        self.handle_known_completed(pending, completed).await
    }

    /// Settle a completion whose pending context was recovered from the
    /// durable runner journal rather than the process-local map.
    pub async fn handle_known_completed(
        &mut self,
        pending: PendingRunner,
        completed: RunnerCompleted,
    ) -> Result<RunnerCompletionReport, BoxError> {
        self.pending.remove(&completed.trigger_id);
        let failed = completed.error.is_some();
        let mut lock_released = false;
        let mut rolled_back = false;
        let mut dream_settled_memory_dir = None;

        if pending.kind == "dream" {
            // R4-2：TS-line runner 回执是三条产出 lane 之一，此前**不记账** ——
            // 与手动 lane 一起造成 `dreamed` 系统性漏计成功。它属**自驱** lane
            // （系统自己发起的），进适应度口径。
            crate::evolution::gate_stats::record_tick_outcome(
                &pending.project_state_dir,
                crate::evolution::gate_stats::LANE_TS_RUNNER,
                if failed { "errored" } else { "dreamed" },
                now_ms(),
            )
            .await;
            if failed {
                if let Some(prior) = pending.prior_mtime_ms {
                    lock::rollback(&pending.memory_dir, prior).await?;
                    rolled_back = true;
                }
            } else {
                match completed.completed_at_ms {
                    Some(completed_at_ms) => {
                        lock::record_consolidation_complete_at(
                            &pending.memory_dir,
                            completed_at_ms,
                        )
                        .await?;
                    }
                    None => lock::record_consolidation_complete(&pending.memory_dir).await?,
                }
                lock_released = true;
                // W-MEMORY-ALIVE 4a (裁决②): report the settled dir so the
                // IPC arm can chain imagination for the TS-line dream, same
                // as the Rust self-driven paths (periodic tick / RunNow).
                dream_settled_memory_dir = Some(pending.memory_dir.clone());
            }
        }

        let mut cursor_updated = false;
        if pending.kind == "extract" {
            // Load → mutate → save the per-project cursor for this trigger's
            // own project, so concurrent extractions across projects never
            // clobber each other's window.
            //
            // 2026-07-27：**失败路径不再回写**。在飞标记退役后（见
            // `ExtractCursorState`），失败时游标本就该原样不动 —— 旧实现
            // 调 `complete_extract_error` 只为把 `in_progress` 落回 false，
            // 那个字段已经没了，再存一次盘就是纯粹的无意义写入。
            if !failed {
                if let Some(uuid) = pending.extract_last_assistant_uuid.as_deref() {
                    let mut extract_cursor = load_extract_cursor(&pending.project_state_dir);
                    complete_extract_success(
                        &mut extract_cursor,
                        uuid,
                        pending.extract_total_model_visible,
                    );
                    save_extract_cursor(&pending.project_state_dir, &extract_cursor).await?;
                    cursor_updated = true;
                } else {
                    log::warn!(
                        "extract runner {} reported success without a cursor uuid — window not \
                         advanced (the same range will be re-extracted)",
                        pending.trigger_id,
                    );
                }
            }
        }

        let archive_record = RunnerArchiveRecord {
            trigger_id: completed.trigger_id,
            kind: completed.kind,
            completed_at_ms: completed.completed_at_ms.unwrap_or_else(now_ms),
            written_paths: completed.written_paths,
            usage: completed.usage,
            error: completed.error,
        };
        let archive_report =
            archive_runner_completed(&pending.project_state_dir, &archive_record).await?;
        append_runner_daily_log(&pending, &archive_record).await?;

        Ok(RunnerCompletionReport {
            known_trigger: true,
            lock_released,
            rolled_back,
            cursor_updated,
            indexed_path_count: archive_report.written_path_records.len(),
            dream_settled_memory_dir,
            late_after_timeout: false,
        })
    }
}

async fn append_runner_daily_log(
    pending: &PendingRunner,
    record: &RunnerArchiveRecord,
) -> Result<(), BoxError> {
    let occurred_at_ms = record.completed_at_ms;
    let transcript_meta = DailyLogTranscriptMeta {
        session_id: pending.session_id.clone(),
        path: pending
            .project_state_dir
            .join(format!("{}.jsonl", pending.session_id)),
        mtime_ms: occurred_at_ms,
        size_bytes: 0,
        sealed: true,
    };
    let event = SessionEvent {
        event_id: format!("{}:completed", record.trigger_id),
        kind: "memory.runner.completed".to_owned(),
        occurred_at_ms,
        payload: serde_json::json!({
            "trigger_id": record.trigger_id,
            "kind": record.kind,
            "written_paths": record
                .written_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            "usage": record.usage,
            "error": record.error,
        }),
    };
    append_daily_log(&pending.project_state_dir, &transcript_meta, &[event])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::extract_cursor::{load_extract_cursor, ExtractCursorState};
    use crate::lock::{last_consolidated_at, lock_path};

    use super::*;

    const T0: u64 = 1_700_000_000_000;

    fn pending(dir: &TempDir, kind: &str) -> PendingRunner {
        PendingRunner {
            trigger_id: format!("{kind}-1"),
            kind: kind.to_owned(),
            session_id: "session-a".to_owned(),
            memory_dir: dir.path().join("memory"),
            project_state_dir: dir.path().to_path_buf(),
            lock_token: Some("token".to_owned()),
            prior_mtime_ms: Some(1_700_000_000_000),
            extract_last_assistant_uuid: (kind == "extract").then(|| "assistant-2".to_owned()),
            extract_total_model_visible: (kind == "extract").then_some(8),
            registered_at_ms: T0,
        }
    }

    fn completed(dir: &TempDir, kind: &str) -> RunnerCompleted {
        RunnerCompleted {
            trigger_id: format!("{kind}-1"),
            kind: kind.to_owned(),
            written_paths: vec![dir.path().join("memory/topic.md")],
            usage: Some(json!({ "output_tokens": 5 })),
            error: None,
            completed_at_ms: None,
        }
    }

    #[tokio::test]
    async fn result_listener_releases_dream_lock_and_indexes_written_paths() {
        let dir = TempDir::new().unwrap();
        let memory_file = dir.path().join("memory/topic.md");
        fs::create_dir_all(memory_file.parent().unwrap()).unwrap();
        fs::write(&memory_file, "body").unwrap();
        fs::write(lock_path(&dir.path().join("memory")), "123").unwrap();
        let before = last_consolidated_at(&dir.path().join("memory"))
            .await
            .unwrap();
        let mut listener = ResultListener::new();
        listener.register(pending(&dir, "dream"));

        let report = listener
            .handle_completed(completed(&dir, "dream"))
            .await
            .unwrap();

        assert!(report.known_trigger);
        assert!(report.lock_released);
        assert!(!report.rolled_back);
        assert_eq!(report.indexed_path_count, 1);
        // W-MEMORY-ALIVE 4a: a settled dream reports its memory_dir so the
        // IPC arm chains imagination for the TS-line dream too (裁决②).
        assert_eq!(
            report.dream_settled_memory_dir,
            Some(dir.path().join("memory"))
        );
        assert!(
            last_consolidated_at(&dir.path().join("memory"))
                .await
                .unwrap()
                >= before
        );
        assert_eq!(
            fs::read_to_string(lock_path(&dir.path().join("memory"))).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn result_listener_rolls_back_failed_dream_lock_to_seconds_precision() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(lock_path(&dir.path().join("memory")), "123").unwrap();
        set_file_mtime(
            lock_path(&dir.path().join("memory")),
            FileTime::from_unix_time(1_800_000_000, 0),
        )
        .unwrap();
        let mut listener = ResultListener::new();
        listener.register(pending(&dir, "dream"));
        let mut failed = completed(&dir, "dream");
        failed.error = Some(json!({ "message": "aborted" }));

        let report = listener.handle_completed(failed).await.unwrap();

        assert!(report.rolled_back);
        // W-MEMORY-ALIVE 4a: a FAILED dream must not chain imagination.
        assert_eq!(report.dream_settled_memory_dir, None);
        assert_eq!(
            last_consolidated_at(&dir.path().join("memory"))
                .await
                .unwrap(),
            1_700_000_000_000
        );
    }

    #[tokio::test]
    async fn result_listener_advances_extract_cursor_on_success() {
        let dir = TempDir::new().unwrap();
        let mut listener = ResultListener::new();
        let pending = pending(&dir, "extract");
        let psd = pending.project_state_dir.clone();
        listener.register(pending);
        // 在飞标记已退役：Run 决策留在磁盘上的只是窗口位置。
        save_extract_cursor(&psd, &ExtractCursorState::default())
            .await
            .unwrap();

        let report = listener
            .handle_completed(completed(&dir, "extract"))
            .await
            .unwrap();

        assert!(report.cursor_updated);
        let cursor = load_extract_cursor(&psd);
        assert_eq!(cursor.last_assistant_uuid.as_deref(), Some("assistant-2"));
        assert_eq!(cursor.last_total_model_visible, 8);
        assert!(
            !listener.has_in_flight("extract", &psd),
            "回执落地后在飞条目必须已摘除"
        );
        // W-MEMORY-ALIVE 4a: only DREAM completions chain imagination.
        assert_eq!(report.dream_settled_memory_dir, None);
    }

    #[tokio::test]
    async fn result_listener_keeps_extract_cursor_on_error() {
        let dir = TempDir::new().unwrap();
        let mut listener = ResultListener::new();
        let pending = pending(&dir, "extract");
        let psd = pending.project_state_dir.clone();
        listener.register(pending);
        save_extract_cursor(
            &psd,
            &ExtractCursorState {
                last_assistant_uuid: Some("old".to_owned()),
                ..ExtractCursorState::default()
            },
        )
        .await
        .unwrap();
        let mut failed = completed(&dir, "extract");
        failed.error = Some(json!({ "message": "failed" }));

        let report = listener.handle_completed(failed).await.unwrap();

        assert!(!report.cursor_updated);
        let cursor = load_extract_cursor(&psd);
        assert_eq!(
            cursor.last_assistant_uuid.as_deref(),
            Some("old"),
            "失败不推进游标"
        );
        assert!(
            !listener.has_in_flight("extract", &psd),
            "失败回执同样要释放在飞占位"
        );
    }

    /// §27.3-G8 + §25.1 —— 在飞占位与超时回收的成对契约。
    #[tokio::test]
    async fn in_flight_blocks_until_completion_and_ttl_releases_a_stuck_runner() {
        let dir = TempDir::new().unwrap();
        let mut listener = ResultListener::new();
        let pending = pending(&dir, "extract");
        let psd = pending.project_state_dir.clone();
        listener.register(pending);

        assert!(listener.has_in_flight("extract", &psd), "登记即在飞");
        assert!(!listener.has_in_flight("dream", &psd), "kind 不串");

        // 未到 TTL：不回收。
        assert_eq!(listener.sweep_timeouts(T0 + EXTRACT_RUNNER_TTL_MS).await, 0);
        assert!(listener.has_in_flight("extract", &psd));

        // 过 TTL：回收，占位释放。
        assert_eq!(
            listener
                .sweep_timeouts(T0 + EXTRACT_RUNNER_TTL_MS + 1)
                .await,
            1
        );
        assert!(
            !listener.has_in_flight("extract", &psd),
            "超时后必须放行，否则一条卡死 pending 会永久阻塞该项目抽取"
        );

        // 迟到回执必须可识别 —— 不能落进那个静音分支。
        let report = listener
            .handle_completed(completed(&dir, "extract"))
            .await
            .unwrap();
        assert!(!report.known_trigger);
        assert!(report.late_after_timeout, "迟到回执要被标出来");
    }

    /// 做梦 lane 的往返预算必须**远大于**单次 `call_llm` 的 60s ——
    /// 一条做梦往返含 phase0..phase4 多次串行调用（§27.3-G7）。
    #[tokio::test]
    async fn dream_ttl_is_not_the_single_llm_call_budget() {
        const {
            assert!(
                DREAM_RUNNER_TTL_MS >= 30 * 60 * 1_000,
                "做梦往返预算不得退化到单次调用量级"
            )
        };
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        let mut listener = ResultListener::new();
        listener.register(pending(&dir, "dream"));

        // 60s（单次 call_llm 预算）时绝不能判死一个正常工作的做梦 runner。
        assert_eq!(listener.sweep_timeouts(T0 + 60_000).await, 0);
        assert!(listener.has_in_flight("dream", dir.path()));
    }

    #[tokio::test]
    async fn result_listener_ignores_unknown_trigger_completion() {
        let dir = TempDir::new().unwrap();
        let mut listener = ResultListener::new();

        let report = listener
            .handle_completed(completed(&dir, "extract"))
            .await
            .unwrap();

        assert!(!report.known_trigger);
        assert_eq!(listener.pending_len(), 0);
    }
}
