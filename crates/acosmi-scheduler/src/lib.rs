//! Acosmi 定时任务守护调度器
//!
//! 统一调度引擎，支持三种 schedule kind（at/every/cron）。
//! 读取 `cron/scheduled_tasks.json`（v1/v2 双格式自动检测），
//! 实现 cron 表达式解析、jitter 防雷暴、文件监视、跨进程锁和守护循环。
//!
//! 子模块：
//! - `cron_parse`: 5-field cron 表达式解析 + 下次触发时间计算
//! - jitter: 防雷暴群 jitter 系统 + ISO-8601 解析 + missed 检测
//! - `task_store`: 定时任务持久化存储（v1 `CronTask` + v2 `CronJob` 双格式）
//! - daemon: `SchedulerDaemon` 守护主循环 + 文件监视 + 触发
//! - lock: 跨进程调度器锁（兼容 TS cronTasksLock.ts）

pub mod cron_parse;
pub mod daemon;
pub mod fire_log;
pub mod jitter;
pub mod lock;
pub mod outbox;
pub mod task_store;

pub use cron_parse::{CronFields, parse_cron};
pub use daemon::{
    ChannelStatusSink, CronAddResult, CronCommand, CronError, CronStoreStatus, FireReason,
    JobFiredEvent, MissedJobInfo, MissedJobsEvent, SchedulerConfig, SchedulerDaemon,
    SchedulerStatusSink,
};
pub use fire_log::{FireLogEntry, fire_log_path};
pub use jitter::{
    CronJitterConfig, find_missed_jobs, find_missed_jobs_detailed, find_missed_tasks,
    is_recurring_task_aged, jittered_next_cron_run_ms, jittered_next_cron_run_ms_tz,
    next_cron_run_ms, next_cron_run_ms_tz, next_every_run_ms, one_shot_jittered_next_cron_run_ms,
    one_shot_jittered_next_cron_run_ms_tz, parse_iso8601_to_ms,
};
pub use lock::{
    HOLD_LEGACY_LOCK, SchedulerLock, owns_primary_lock, release_legacy_lock, release_lock,
    try_acquire_legacy_lock, try_acquire_lock,
};
pub use outbox::{
    MISSED_CATCHUP_WINDOW_MS, OutboxEvent, OutboxEventType, next_event_id as outbox_next_event_id,
    outbox_path, read_after as outbox_read_after,
};
// 旧格式类型（daemon 过渡期使用）
pub use task_store::{CronFile, CronTask};
// 新 CronJob 类型（完整 Go CronJob 对齐）
pub use task_store::{
    CronDelivery, CronJob, CronJobCreate, CronJobPatch, CronJobState, CronPayload, CronSchedule,
    CronStoreFile, DeliveryMode, JobStatus, PayloadKind, STORE_VERSION, ScheduleKind,
    SessionTarget, WakeMode,
};

/// Cron storage layout tripwire.
///
/// cron 守护存储布局的 6 个相对路径常量分散在 4 个模块里，但它们是**一个**
/// 整体契约：全部落在 `<state_dir>` 下同一子目录（归一后 = `cron/` 单层，即
/// `~/.crabcode/cron/…`；归一前是「双脑」嵌套 `~/.crabcode/.crabcode/…`）。守护
/// 主业务锁（`LOCK_FILE_REL`）本身位于该布局内 —— 任何人单点改动其中一个常量而
/// 不同步其余（尤其是锁），新旧守护会互不见锁 → 并行双写 → outbox/ledger 在两个
/// 目录活跃分叉。过渡期旧锁 `LEGACY_LOCK_FILE_REL` 是**有意保留**的 `.crabcode/`
/// 例外（双锁过渡），不并入下方 `cron/` 前缀集。
///
/// 改动任一常量必须整体迁移（含数据搬迁和双锁过渡），并同步更新本测试。
#[cfg(test)]
mod layout_contract {
    use crate::{fire_log, lock, outbox, task_store};

    /// 归一后唯一合法前缀（`cron/` 单层）。
    const EXPECTED_PREFIX: &str = "cron/";

    fn all_layout_constants() -> [(&'static str, &'static str); 6] {
        [
            ("task_store::CRON_FILE_REL", task_store::CRON_FILE_REL),
            ("lock::LOCK_FILE_REL", lock::LOCK_FILE_REL),
            ("fire_log::FIRE_LOG_REL", fire_log::FIRE_LOG_REL),
            ("outbox::OUTBOX_REL", outbox::OUTBOX_REL),
            (
                "outbox::OUTBOX_LEDGER_EPOCH_REL",
                outbox::OUTBOX_LEDGER_EPOCH_REL,
            ),
            (
                "outbox::OUTBOX_LEDGER_STARTED_REL",
                outbox::OUTBOX_LEDGER_STARTED_REL,
            ),
        ]
    }

    #[test]
    fn layout_constants_are_pinned_verbatim() {
        assert_eq!(task_store::CRON_FILE_REL, "cron/scheduled_tasks.json");
        assert_eq!(lock::LOCK_FILE_REL, "cron/scheduled_tasks.lock");
        assert_eq!(fire_log::FIRE_LOG_REL, "cron/cron_fired.log");
        assert_eq!(outbox::OUTBOX_REL, "cron/cron_outbox.jsonl");
        assert_eq!(
            outbox::OUTBOX_LEDGER_EPOCH_REL,
            "cron/cron_outbox.ledger_epoch"
        );
        assert_eq!(
            outbox::OUTBOX_LEDGER_STARTED_REL,
            "cron/cron_outbox.ledger_started"
        );
        // 过渡期旧业务锁是有意保留的 `.crabcode/` 例外（双锁过渡），单独钉死其值，
        // 防止未来「顺手归一」把它也翻到 cron/（=旧代守护互斥失效）。
        assert_eq!(lock::LEGACY_LOCK_FILE_REL, ".crabcode/scheduled_tasks.lock");
    }

    #[test]
    fn all_layout_constants_share_one_prefix() {
        for (name, value) in all_layout_constants() {
            assert!(
                value.starts_with(EXPECTED_PREFIX),
                "{name} = {value:?} 脱离了布局前缀 {EXPECTED_PREFIX:?} —— \
                 布局常量禁止单点改动，见 W-CRON-DIR-UNIFY 方案 doc"
            );
        }
    }

    #[test]
    fn path_helpers_resolve_into_one_shared_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        let json = task_store::cron_file_path(base);
        let json_parent = json.parent().expect("cron file parent").to_path_buf();
        let others = [
            ("lock", lock::lock_path(base)),
            ("fire_log", fire_log::fire_log_path(base)),
            ("outbox", outbox::outbox_path(base)),
            ("ledger_epoch", outbox::ledger_epoch_path(base)),
            (
                "ledger_started",
                base.join(outbox::OUTBOX_LEDGER_STARTED_REL),
            ),
        ];
        for (name, path) in others {
            assert_eq!(
                path.parent(),
                Some(json_parent.as_path()),
                "{name} 路径 {path:?} 与 scheduled_tasks.json 不同父目录 —— 布局分叉"
            );
        }
    }
}
