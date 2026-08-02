//! Cron 事件 outbox（持久化、可恢复的触发事件投递）
//!
//! W-CRON-AUTOMATION-E2E P7 (RC6)。
//!
//! ## 背景与根因
//!
//! 改造前 cron fire 事件**只**活在 `crabcode-cron` 进程内存的 `EventQueue`
//! （`Arc<Mutex<Vec<Value>>>`，由 `cron.poll_events` drain）+ 一个 broadcast
//! channel 里。两条都是非持久的：
//! - daemon 重启 → 内存队列清空，未被 drain 的 fire 事件永久丢失
//! - REPL 离线时任务触发 → 没人订阅、没人 poll，事件丢失
//! - missed 的 one-shot 任务由 `detect_missed_jobs()` 检出后**直接删除** ——
//!   连"它本该触发过"这件事都从此无据可查
//!
//! ## 设计
//!
//! 本模块在 `<dir>/cron/cron_outbox.jsonl` 维护一份 append-only 的
//! JSONL outbox。每个 fire / missed-catch-up / skipped 事件落一行。每条带一个
//! **单调递增**的 `event_id`，消费方用 cursor（"已消费到的最大 event_id"）做
//! 幂等读取（`read_after`）。daemon 启动时 `next_event_id` 扫描完整 retained
//! window 取 max+1；进程内由唯一 writer task 串行执行「分配 id → append/rotate」，
//! 避免 allocator 顺序与物理落盘顺序脱钩。
//!
//! - 与 `fire_log.rs` 并存：`fire_log` 服务 Continuation HTTP 回调的运行历史
//!   （`cron.runs`），只覆盖 Continuation-session 任务；本 outbox 服务**所有**
//!   任务的可恢复事件投递（`cron.outbox_read`）。两者用途不同，不互相替代。
//! - 文件追加 / 滚动复用 `fire_log` 的成熟模式（`O_APPEND` 原子追加 + 10MB
//!   滚动保留 3 份）。
//!
//! ## 恢复窗口边界（明确声明）
//!
//! `read_after` 跨**当前** outbox 文件 + 滚动保留的 `.1` / `.2` / `.3` 历史
//! 文件一并扫描。reader 不信任历史物理顺序：扫描后按 `event_id` 选择最小的
//! `limit` 条并升序返回，避免旧文件乱序时高 id 先推动 cursor。恢复窗口
//! = 当前文件 + 3 个 rotated 文件（合计约 40MB 事件），覆盖消费方长时间离线、
//! 期间 outbox 已滚动的场景。更老的 rotation（`.4` 及以上）由 `rotate` 直接
//! 丢弃 —— 这是有意的**有界**历史，不引数据库做无界回放。
//!
//! ## 错误语义
//!
//! 追加失败仅 `tracing::error`，**绝不 panic、绝不阻塞 fire 路径** —— outbox
//! 是 fire 路径旁路的耐久层，它的故障不能拖垮调度本身。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing;
use uuid::Uuid;

use crate::task_store::{CronJob, SessionTarget, WakeMode};

/// Exact consumer identity captured at durable-job creation time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronExecutionTarget {
    pub version: u8,
    pub owner_thread_id: String,
    pub project: String,
    pub cwd: String,
    pub connection_id: String,
    pub surface: String,
}

/// outbox 文件相对路径（与 `scheduled_tasks.json` / `cron_fired.log` 同目录）
// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const OUTBOX_REL: &str = "cron/cron_outbox.jsonl";

/// Durable identity of one logical outbox ledger. It is created beside the
/// retained JSONL files and does not rotate with them. A deliberate/full
/// retained-ledger reset starts a fresh epoch before ids can restart at 1.
// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const OUTBOX_LEDGER_EPOCH_REL: &str = "cron/cron_outbox.ledger_epoch";

/// Durable evidence that the current epoch has begun writing events. The
/// marker is created (and fsync'd) before the first append attempt. At daemon
/// startup, marker + empty retained window means the prior ledger was fully
/// reset and its epoch must be replaced before event ids are reused.
// Layout contract: update together with every `layout_contract` tripwire.
pub(crate) const OUTBOX_LEDGER_STARTED_REL: &str = "cron/cron_outbox.ledger_started";

/// 单文件滚动阈值（10 MB）—— 与 `fire_log` 对齐
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 保留历史文件数（`.1`、`.2`、`.3`）—— 与 `fire_log` 对齐
const ROTATE_KEEP: usize = 3;

/// Largest event id that can cross the Rust -> JSON -> JavaScript boundary
/// without precision loss.  The allocator may issue this id once, then must
/// stop; it must never emit a value that TypeScript would round.
pub const MAX_SAFE_EVENT_ID: u64 = 9_007_199_254_740_991;

/// Missed one-shot 任务的"补投窗口"。
///
/// 一个 missed 的 one-shot 任务，若其计划触发时刻在 `now` 往前
/// `MISSED_CATCHUP_WINDOW_MS` 之内，则视为"虽迟但仍应执行" —— outbox 写一条
/// [`OutboxEventType::Fired`] 事件（晚到的真触发），消费方按"现在就跑"处理。
/// 超出此窗口的（计划触发时刻太久远）则写 [`OutboxEventType::Skipped`]，消费方
/// 仅作记录、不执行。
///
/// 取值 24h：跨夜 / 周末关机再开机仍能补投，但几天前的陈旧 one-shot 不会被
/// 突然唤醒打扰用户。
pub const MISSED_CATCHUP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// outbox 事件类型。
///
/// camelCase 序列化 → `"fired"` / `"missed"` / `"skipped"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutboxEventType {
    /// 正常触发（schedule 命中）或 missed-but-within-catchup-window 的晚到触发。
    /// 消费方一律按"现在就执行这个任务"处理。
    Fired,
    /// missed 任务的中性记录（保留给未来需要"区分晚到 vs 准时"的消费方；
    /// 当前 daemon 的 catch-up 路径把窗口内 missed 写成 `Fired`，本变体不产生）。
    Missed,
    /// missed 且超出补投窗口 —— 任务太陈旧不再执行，仅作审计记录。
    Skipped,
}

/// 一条 outbox 事件。
///
/// camelCase 序列化，与 TS 端 `OutboxEvent` 类型对齐。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxEvent {
    /// 单调递增的事件 id（daemon 唯一 writer task 分配，跨重启由
    /// `next_event_id` 扫描 retained window 续号）。消费方 cursor 即
    /// "已消费的最大 id"。
    pub event_id: u64,
    /// 触发的 cron job id。
    pub job_id: String,
    /// 事件类型。
    pub event_type: OutboxEventType,
    /// 任务的计划触发时刻（epoch ms）。schedule 触发 = 理论到期时刻
    /// （`JobFiredEvent::scheduled_at_ms`，重启可复现，**不是**墙钟触发时刻）；
    /// missed 路径 = 任务原本应触发的时刻。亦为 SQLite occurrence 的耐久键。
    pub scheduled_at_ms: i64,
    /// 本事件写入 outbox 的时刻（epoch ms）。
    pub emitted_at_ms: i64,
    /// 任务 payload 的 `message`（优先）/ `text`，供消费方做 prompt。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_message: Option<String>,
    /// 任务名（诊断 / UI 展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,
    /// Immutable routing semantics copied from the job at fire time.
    #[serde(default)]
    pub session_target: SessionTarget,
    #[serde(default)]
    pub wake_mode: WakeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_target: Option<CronExecutionTarget>,
}

/// One bounded cursor page plus the retained-ledger bounds observed during
/// the same scan. Consumers use `oldest_event_id` as the only safe tombstone
/// GC watermark: ids below it can no longer be replayed from the outbox.
#[derive(Debug)]
pub struct OutboxReadPage {
    pub events: Vec<OutboxEvent>,
    pub oldest_event_id: Option<u64>,
    pub newest_event_id: Option<u64>,
}

/// Result of one durable append.  The retained floor can only change after a
/// completed rotation, so ordinary appends avoid rescanning the 40MB window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxAppendOutcome {
    pub retained_floor_after_rotation: Option<u64>,
    /// Complete, strictly validated retained window after rotation. The
    /// daemon actor swaps its exact-membership index from this snapshot so a
    /// delivery begin never needs an O(40MB) rescan.
    pub retained_events_after_rotation: Option<Vec<OutboxEvent>>,
}

/// Deterministic occurrence identity for one logical `(job, scheduled-time)`
/// firing.
///
/// Unlike the monotonic `event_id` — which is freshly allocated on every
/// append, so a crash-retry of the *same* firing gets a *different* id — this
/// id is a pure function of `job_id` + `scheduled_at_ms`: the same firing
/// always maps to the same string. That lets a duplicate append / claim of one
/// firing be recognized and de-duplicated by key.
///
/// Format: `occ_<jobId>_<scheduledAtMs>`. Pure — no clock, no state, no I/O —
/// so callers on both the append (produce) and claim (consume) sides derive an
/// identical key without any coordination.
///
/// NOTE (PR-0C scope): this is the deterministic *key* only. Actively enforcing
/// dedup (skipping a duplicate append, or collapsing duplicate claims) changes
/// persist / delivery timing and is deliberately left to the delivery-timing
/// work (PR-5); this function does not itself alter when or whether anything is
/// persisted or sent.
#[must_use]
pub fn occurrence_id(job_id: &str, scheduled_at_ms: i64) -> String {
    format!("occ_{job_id}_{scheduled_at_ms}")
}

impl OutboxEvent {
    /// Deterministic occurrence dedup key for this event's logical firing.
    ///
    /// Derived purely from `job_id` + `scheduled_at_ms` (see [`occurrence_id`]),
    /// so it stays stable across a crash-retry that would otherwise mint a new
    /// `event_id`. Computed on demand — legacy retained events (persisted before
    /// this key existed) still yield the correct value on read-back.
    #[must_use]
    pub fn occurrence_id(&self) -> String {
        occurrence_id(&self.job_id, self.scheduled_at_ms)
    }

    /// 从一个 `CronJob` + 已分配的 `event_id` + 事件类型构造 outbox 事件。
    ///
    /// `scheduled_at_ms` 由调用方给出（schedule 触发传实际触发时刻，missed
    /// 路径传任务原本应触发的时刻）。`emitted_at_ms` 取当前时刻。
    #[must_use]
    pub fn from_job(
        job: &CronJob,
        event_id: u64,
        event_type: OutboxEventType,
        scheduled_at_ms: i64,
    ) -> Self {
        let payload_message = job
            .payload
            .message
            .clone()
            .filter(|message| !message.trim().is_empty())
            .or_else(|| {
                job.payload
                    .text
                    .clone()
                    .filter(|text| !text.trim().is_empty())
            });
        normalize_delivery_eligibility(Self {
            event_id,
            job_id: job.id.clone(),
            event_type,
            scheduled_at_ms,
            emitted_at_ms: chrono::Utc::now().timestamp_millis(),
            payload_message,
            job_name: Some(job.name.clone()).filter(|s| !s.is_empty()),
            session_target: job.session_target.clone(),
            wake_mode: job.wake_mode.clone(),
            execution_target: job
                .session_key
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok()),
        })
    }
}

/// Empty/whitespace prompts cannot produce a downstream turn. Normalize both
/// newly constructed and legacy retained events to the existing audit-only
/// `skipped` semantic so every surface coordinates/accepts the row and can
/// advance its cursor instead of abandoning it forever.
fn normalize_delivery_eligibility(mut event: OutboxEvent) -> OutboxEvent {
    if matches!(
        event.event_type,
        OutboxEventType::Fired | OutboxEventType::Missed
    ) && event
        .payload_message
        .as_deref()
        .is_none_or(|message| message.trim().is_empty())
    {
        event.payload_message = None;
        event.event_type = OutboxEventType::Skipped;
    }
    event
}

/// 返回 outbox 文件绝对路径。
#[must_use]
pub fn outbox_path(dir: &Path) -> PathBuf {
    dir.join(OUTBOX_REL)
}

/// Return the ledger-epoch file path.
#[must_use]
pub fn ledger_epoch_path(dir: &Path) -> PathBuf {
    dir.join(OUTBOX_LEDGER_EPOCH_REL)
}

fn ledger_started_path(dir: &Path) -> PathBuf {
    dir.join(OUTBOX_LEDGER_STARTED_REL)
}

/// Load the durable ledger epoch, creating it exactly once when the ledger is
/// first opened. `create_new` is the cross-process arbitration point; a racer
/// that loses retries the read. Corrupt/empty epochs fail loud instead of
/// silently minting a replacement that could split one live ledger into two
/// delivery namespaces.
pub async fn load_or_create_ledger_epoch(dir: &Path) -> std::io::Result<String> {
    let path = ledger_epoch_path(dir);
    if let Some(parent) = path.parent() {
        create_dir_all_durable(parent).await?;
    }

    loop {
        match read_regular_file_no_follow(&path).await {
            Ok(bytes) => {
                let raw = std::str::from_utf8(&bytes).map_err(|error| {
                    invalid_data(format!("cron outbox ledger epoch is not UTF-8: {error}"))
                })?;
                let epoch = raw.trim();
                Uuid::parse_str(epoch).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid cron outbox ledger epoch: {error}"),
                    )
                })?;
                return Ok(epoch.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let epoch = Uuid::new_v4().to_string();
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path).await {
            Ok(mut file) => {
                file.write_all(epoch.as_bytes()).await?;
                file.write_all(b"\n").await?;
                file.flush().await?;
                file.sync_all().await?;
                drop(file);
                sync_parent_directory(&path)?;
                return Ok(epoch);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process won `create_new`; loop and validate its
                // durable value instead of inventing a second epoch.
            }
            Err(error) => return Err(error),
        }
    }
}

async fn replace_ledger_epoch(path: &Path, epoch: &str) -> std::io::Result<()> {
    let temp_path = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        Uuid::new_v4()
    ));
    let result = async {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temp_path).await?;
        file.write_all(epoch.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        atomic_replace(&temp_path, path).await?;
        sync_parent_directory(path)
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

/// Bind the immutable epoch used by one daemon process. Normal rotation keeps
/// the epoch. If the prior epoch had started writing but the entire retained
/// window is now empty, ids would restart at 1; replace the epoch first so an
/// old accepted tombstone/cursor can never alias the new ledger.
pub async fn initialize_ledger_epoch(dir: &Path) -> std::io::Result<String> {
    let current = load_or_create_ledger_epoch(dir).await?;
    let started_path = ledger_started_path(dir);
    let was_started = match read_regular_file_no_follow(&started_path).await {
        Ok(bytes) if bytes == b"started\n" => true,
        Ok(_) => {
            return Err(invalid_data(format!(
                "invalid cron outbox ledger started marker: {}",
                started_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !was_started || next_event_id(dir).await? != 1 {
        return Ok(current);
    }

    let replacement = Uuid::new_v4().to_string();
    replace_ledger_epoch(&ledger_epoch_path(dir), &replacement).await?;
    match fs::remove_file(&started_path).await {
        Ok(()) => sync_parent_directory(&started_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(replacement)
}

async fn mark_ledger_started(dir: &Path) -> std::io::Result<()> {
    let path = ledger_started_path(dir);
    if let Some(parent) = path.parent() {
        create_dir_all_durable(parent).await?;
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(&path).await {
        Ok(mut file) => {
            file.write_all(b"started\n").await?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            sync_parent_directory(&path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = read_regular_file_no_follow(&path).await?;
            if bytes == b"started\n" {
                Ok(())
            } else {
                Err(invalid_data(format!(
                    "invalid cron outbox ledger started marker: {}",
                    path.display()
                )))
            }
        }
        Err(error) => Err(error),
    }
}

/// 原子追加一条事件到 outbox。
///
/// 失败仅 `tracing::error`，不返回错误 —— fire 路径不应因 outbox 故障中断。
pub async fn append(dir: &Path, event: &OutboxEvent) {
    if let Err(e) = append_durable(dir, event).await {
        tracing::error!(
            error = %e,
            job_id = %event.job_id,
            event_id = event.event_id,
            occurrence_id = %event.occurrence_id(),
            "cron_outbox: 写 outbox 失败（fire 路径不受影响）"
        );
    }
}

/// Append one event and return a strictly scanned retained floor only when a
/// rotation actually discarded history.  The caller is the daemon's single
/// writer actor; it durably fences that floor before deleting delivery state.
pub async fn append_durable(
    dir: &Path,
    event: &OutboxEvent,
) -> std::io::Result<OutboxAppendOutcome> {
    let event = normalize_delivery_eligibility(event.clone());
    validate_outbox_event(&event)?;
    let path = outbox_path(dir);
    if let Some(parent) = path.parent() {
        create_dir_all_durable(parent).await?;
    }

    // Persist epoch activity before the event. A crash/failure may cause a
    // harmless epoch change on a later empty startup, but can never leave a
    // written event whose eventual id reuse is invisible to epoch recovery.
    mark_ledger_started(dir).await?;

    // A crash can leave a non-newline-terminated tail.  It is not committed;
    // truncate and sync it before either rotating or appending, otherwise the
    // next JSON record would turn it into a corrupt complete line.
    repair_torn_tail(&path).await?;

    let rotated = match fs::symlink_metadata(&path).await {
        Ok(meta) if metadata_is_link_or_reparse(&meta) || !meta.is_file() => {
            return Err(invalid_data(format!(
                "cron outbox target is not a regular non-symlink file: {}",
                path.display()
            )));
        }
        Ok(meta) if meta.len() >= ROTATE_BYTES => {
            rotate(&path).await?;
            true
        }
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };

    let mut line = serde_json::to_string(&event).map_err(std::io::Error::other)?;
    line.push('\n');

    // 文件权限 0600：outbox 含 payload 正文，限制为所有者可读写。
    // Windows 侧 mode 被忽略，由文件系统 ACL 控制（与 fire_log / cronstore 一致）。
    let mut open_opts = OpenOptions::new();
    open_opts.create(true).append(true);
    #[cfg(unix)]
    {
        open_opts.mode(0o600);
        open_opts.custom_flags(libc::O_NOFOLLOW);
    }
    let existed = match fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            return Err(invalid_data(format!(
                "cron outbox target is not a regular non-symlink file: {}",
                path.display()
            )));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let mut f = open_opts.open(&path).await?;
    if !f.metadata().await?.is_file() {
        return Err(invalid_data(format!(
            "cron outbox handle is not a regular file: {}",
            path.display()
        )));
    }
    f.write_all(line.as_bytes()).await?;
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    if !existed {
        sync_parent_directory(&path)?;
    }

    let retained_after_rotation = if rotated {
        Some(read_after_with_bounds(dir, 0, usize::MAX).await?)
    } else {
        None
    };
    let retained_floor_after_rotation = retained_after_rotation
        .as_ref()
        .map(|page| {
            page.oldest_event_id
                .ok_or_else(|| invalid_data("rotated outbox became empty after durable append"))
        })
        .transpose()?;
    let retained_events_after_rotation = retained_after_rotation.map(|page| page.events);
    Ok(OutboxAppendOutcome {
        retained_floor_after_rotation,
        retained_events_after_rotation,
    })
}

/// 简单滚动：outbox → outbox.1 → outbox.2 → outbox.3（丢弃更老的）。
async fn rotate(path: &Path) -> std::io::Result<()> {
    for i in (1..ROTATE_KEEP).rev() {
        let from = path_with_ext_num(path, i);
        let to = path_with_ext_num(path, i + 1);
        replace_if_exists(&from, &to).await?;
    }
    let first = path_with_ext_num(path, 1);
    atomic_replace(path, &first).await?;
    sync_parent_directory(path)
}

fn path_with_ext_num(path: &Path, n: usize) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(format!(".{n}"));
    PathBuf::from(os)
}

/// retained 文件扫描顺序：current → `.1` → `.2` → `.3`（新到旧）。
///
/// 结果最终会按 event_id 排序，所以物理扫描顺序不承载 cursor 语义。daemon
/// 生产读取与 rotation 由同一 actor 串行；crash 留下的跨文件精确副本由 identity
/// map 去重，同 id 不同内容则 fail-loud。
fn retained_paths_newest_to_oldest(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(ROTATE_KEEP + 1);
    paths.push(path.to_path_buf());
    for n in 1..=ROTATE_KEEP {
        paths.push(path_with_ext_num(path, n));
    }
    paths
}

/// 读取 outbox 中 `event_id > after_event_id` 的事件，最多 `limit` 条。
///
/// 跨 current + retained rotated 文件扫描。物理文件/行顺序不可信：完整窗口
/// 先按 `event_id` 建立有序 identity map，再取最小页。rotation crash 造成的
/// 完全相同副本会去重；同 id 不同内容 fail-loud。这样 cursor 不会越过尾部低 id。
/// Missing files are an empty retained slot.  Any other read failure or any
/// corrupt *complete* line fails loud; silently skipping one could advance a
/// cursor or destructive retained floor past an event.  A final incomplete
/// line is a crash-torn, uncommitted tail and is ignored.
pub async fn read_after(
    dir: &Path,
    after_event_id: u64,
    limit: usize,
) -> std::io::Result<Vec<OutboxEvent>> {
    Ok(read_after_with_bounds(dir, after_event_id, limit)
        .await?
        .events)
}

/// [`read_after`] plus retained min/max ids from the same complete scan.
/// This read API never advances destructive GC; only the daemon's startup and
/// post-rotation actor path may commit a retained floor.
pub async fn read_after_with_bounds(
    dir: &Path,
    after_event_id: u64,
    limit: usize,
) -> std::io::Result<OutboxReadPage> {
    scan_retained(dir, after_event_id, limit).await
}

/// Strict full retained scan used by the single writer at startup and after
/// rotation.  Only this completeness-proven bound may advance destructive
/// delivery GC.
pub async fn retained_bounds(dir: &Path) -> std::io::Result<OutboxReadPage> {
    scan_retained(dir, u64::MAX, 0).await
}

async fn scan_retained(
    dir: &Path,
    after_event_id: u64,
    limit: usize,
) -> std::io::Result<OutboxReadPage> {
    let path = outbox_path(dir);
    let mut raw_retained = BTreeMap::<u64, OutboxEvent>::new();
    for retained_path in retained_paths_newest_to_oldest(&path) {
        collect_from_file(&retained_path, &mut raw_retained).await?;
    }
    // Identity/collision checks above use the exact persisted event. Delivery
    // projection happens only afterwards so two different legacy whitespace
    // payloads cannot collapse into one apparently identical skipped row.
    let retained: BTreeMap<u64, OutboxEvent> = raw_retained
        .into_iter()
        .map(|(event_id, event)| (event_id, normalize_delivery_eligibility(event)))
        .collect();

    let oldest_event_id = retained.first_key_value().map(|(event_id, _)| *event_id);
    let newest_event_id = retained.last_key_value().map(|(event_id, _)| *event_id);
    let events = if limit == 0 {
        Vec::new()
    } else {
        retained
            .range((
                std::ops::Bound::Excluded(after_event_id),
                std::ops::Bound::Unbounded,
            ))
            .take(limit)
            .map(|(_, event)| event.clone())
            .collect()
    };
    Ok(OutboxReadPage {
        events,
        oldest_event_id,
        newest_event_id,
    })
}

/// Read one retained file into the identity map. An exact duplicate may be a
/// crash snapshot of the same file during rotation and is deduplicated. The
/// same id with different content is a fail-loud ledger collision.
/// A missing file is expected.  Read failures and corrupt committed lines are
/// fatal because callers use the observed min/max for cursor and GC safety.
async fn collect_from_file(
    path: &Path,
    retained: &mut BTreeMap<u64, OutboxEvent>,
) -> std::io::Result<()> {
    let bytes = match read_regular_file_no_follow(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let committed_len = committed_prefix_len(&bytes);
    for (line_index, line) in bytes[..committed_len]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event = serde_json::from_slice::<OutboxEvent>(line).map_err(|error| {
            invalid_data(format!(
                "corrupt cron outbox {} line {}: {error}",
                path.display(),
                line_index + 1
            ))
        })?;
        validate_outbox_event(&event).map_err(|error| {
            invalid_data(format!(
                "invalid cron outbox {} line {}: {error}",
                path.display(),
                line_index + 1
            ))
        })?;
        match retained.entry(event.event_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(event);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get() != &event {
                    return Err(invalid_data(format!(
                        "cron outbox event identity collision for event {} in {} line {}",
                        event.event_id,
                        path.display(),
                        line_index + 1
                    )));
                }
            }
        }
    }
    Ok(())
}

/// 扫描完整 retained window，返回 `max(event_id) + 1`（全空 / 不存在 → 1）。
///
/// Every existing retained slot must be readable and every complete nonempty
/// line valid.  Returning a seed from a partial scan could reuse an id.
pub async fn next_event_id(dir: &Path) -> std::io::Result<u64> {
    let newest = retained_bounds(dir).await?.newest_event_id.unwrap_or(0);
    if newest >= MAX_SAFE_EVENT_ID {
        return Err(invalid_data(format!(
            "cron outbox event_id space exhausted at JavaScript MAX_SAFE_INTEGER ({MAX_SAFE_EVENT_ID}); refusing id reuse"
        )));
    }
    Ok(newest + 1)
}

fn validate_event_id(event_id: u64) -> std::io::Result<()> {
    if event_id == 0 || event_id > MAX_SAFE_EVENT_ID {
        return Err(invalid_data(format!(
            "event_id must be within 1..={MAX_SAFE_EVENT_ID}, got {event_id}"
        )));
    }
    Ok(())
}

fn validate_outbox_event(event: &OutboxEvent) -> std::io::Result<()> {
    validate_event_id(event.event_id)?;
    if event.job_id.trim().is_empty() {
        return Err(invalid_data(format!(
            "event {} has an empty job_id",
            event.event_id
        )));
    }
    Ok(())
}

fn committed_prefix_len(bytes: &[u8]) -> usize {
    if bytes.ends_with(b"\n") {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    }
}

async fn repair_torn_tail(path: &Path) -> std::io::Result<()> {
    let bytes = match read_regular_file_no_follow(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let committed_len = committed_prefix_len(&bytes);
    if committed_len == bytes.len() {
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(invalid_data(format!(
            "cron outbox repair handle is not a regular file: {}",
            path.display()
        )));
    }
    file.set_len(
        u64::try_from(committed_len)
            .map_err(|_| invalid_data("cron outbox committed prefix length does not fit u64"))?,
    )
    .await?;
    file.sync_all().await
}

async fn read_regular_file_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).await?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(invalid_data(format!(
            "not a regular non-symlink file: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(invalid_data(format!(
            "opened handle is not a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn replace_if_exists(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(from).await {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            Err(invalid_data(format!(
                "cron outbox rotation source is not a regular non-symlink file: {}",
                from.display()
            )))
        }
        Ok(_) => atomic_replace(from, to).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
async fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to).await?;
    sync_parent_directory(to)
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW is the Win32 atomic replace + write-through primitive.
async fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both buffers are NUL-terminated and remain alive for the call.
    let result = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Create a directory chain and durably publish every new component on
/// POSIX. Windows has no reliable directory-handle flush contract; file
/// contents are still `sync_all`'d and replacements use `MoveFileExW` with
/// `MOVEFILE_WRITE_THROUGH`.
fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    false
}

async fn create_dir_all_durable(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match fs::symlink_metadata(candidate).await {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        format!("{} is not a non-symlink directory", candidate.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(candidate.to_path_buf());
                cursor = candidate.parent();
            }
            Err(error) => return Err(error),
        }
    }
    fs::create_dir_all(path).await?;
    #[cfg(unix)]
    for created in missing.iter().rev() {
        if let Some(parent) = created.parent() {
            sync_directory(parent)?;
        }
        sync_directory(created)?;
    }
    Ok(())
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::{
        CronJobState, CronPayload, CronSchedule, PayloadKind, ScheduleKind, SessionTarget, WakeMode,
    };

    fn test_job(id: &str, message: &str) -> CronJob {
        CronJob {
            id: id.into(),
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
                at: Some("2030-01-01T00:00:00+00:00".into()),
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
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }
    }

    #[test]
    fn event_type_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&OutboxEventType::Fired).unwrap(),
            "\"fired\""
        );
        assert_eq!(
            serde_json::to_string(&OutboxEventType::Missed).unwrap(),
            "\"missed\""
        );
        assert_eq!(
            serde_json::to_string(&OutboxEventType::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    #[test]
    fn occurrence_id_is_deterministic_for_same_job_and_time() {
        // 同 jobId + 同 scheduledAtMs → 同 id（幂等去重键的核心不变量）。
        assert_eq!(
            occurrence_id("job-abc", 1_700_000_000_000),
            occurrence_id("job-abc", 1_700_000_000_000)
        );
        assert_eq!(
            occurrence_id("job-abc", 1_700_000_000_000),
            "occ_job-abc_1700000000000"
        );
        // 负的 scheduled_at_ms（pre-epoch / 时钟回拨）仍确定性成形。
        assert_eq!(occurrence_id("j", -5), "occ_j_-5");
    }

    #[test]
    fn occurrence_id_differs_for_different_job_or_time() {
        // 不同 jobId → 不同 id。
        assert_ne!(occurrence_id("job-a", 1000), occurrence_id("job-b", 1000));
        // 不同 scheduledAtMs → 不同 id。
        assert_ne!(occurrence_id("job-a", 1000), occurrence_id("job-a", 1001));
        // 两者都不同 → 不同 id。
        assert_ne!(occurrence_id("job-a", 1000), occurrence_id("job-b", 2000));
    }

    #[test]
    fn outbox_event_occurrence_id_matches_free_function() {
        // 事件方法与自由函数对同一 (job_id, scheduled_at_ms) 必须一致，
        // append(produce) 侧与 claim(consume) 侧因此得到同一去重键。
        let event = OutboxEvent::from_job(&test_job("j7", "m"), 7, OutboxEventType::Fired, 4242);
        assert_eq!(event.occurrence_id(), occurrence_id("j7", 4242));
        assert_eq!(event.occurrence_id(), "occ_j7_4242");

        // event_id 不参与 key —— 同一 firing 的 crash-retry（新 event_id）
        // 仍产生同一 occurrence key。
        let retry = OutboxEvent::from_job(&test_job("j7", "m"), 99, OutboxEventType::Fired, 4242);
        assert_eq!(event.occurrence_id(), retry.occurrence_id());
    }

    #[test]
    fn from_job_prefers_message_then_text() {
        let mut job = test_job("j1", "hello");
        let e1 = OutboxEvent::from_job(&job, 1, OutboxEventType::Fired, 5000);
        assert_eq!(e1.payload_message.as_deref(), Some("hello"));
        assert_eq!(e1.job_name.as_deref(), Some("job-j1"));
        assert_eq!(e1.scheduled_at_ms, 5000);
        assert_eq!(e1.event_id, 1);

        // 无 message → 回落 text
        job.payload.message = None;
        job.payload.text = Some("from-text".into());
        let e2 = OutboxEvent::from_job(&job, 2, OutboxEventType::Skipped, 6000);
        assert_eq!(e2.payload_message.as_deref(), Some("from-text"));

        // message / text 都无 → None
        job.payload.text = None;
        let e3 = OutboxEvent::from_job(&job, 3, OutboxEventType::Fired, 7000);
        assert!(e3.payload_message.is_none());
        assert_eq!(e3.event_type, OutboxEventType::Skipped);

        job.payload.message = Some("   \t".into());
        job.payload.text = Some("fallback-text".into());
        let whitespace = OutboxEvent::from_job(&job, 4, OutboxEventType::Missed, 8000);
        assert_eq!(whitespace.payload_message.as_deref(), Some("fallback-text"));
        assert_eq!(whitespace.event_type, OutboxEventType::Missed);

        job.payload.text = Some("  ".into());
        let all_blank = OutboxEvent::from_job(&job, 5, OutboxEventType::Missed, 9000);
        assert!(all_blank.payload_message.is_none());
        assert_eq!(all_blank.event_type, OutboxEventType::Skipped);
    }

    #[tokio::test]
    async fn append_then_read_after_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=3 {
            let job = test_job(&format!("j{i}"), &format!("msg{i}"));
            let ev = OutboxEvent::from_job(&job, i, OutboxEventType::Fired, 1000 + i as i64);
            append(dir.path(), &ev).await;
        }

        // cursor 0 → 全部 3 条
        let all = read_after(dir.path(), 0, 100).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event_id, 1);
        assert_eq!(all[2].event_id, 3);
        assert_eq!(all[0].job_id, "j1");
        assert_eq!(all[2].payload_message.as_deref(), Some("msg3"));
        assert_eq!(all[1].event_type, OutboxEventType::Fired);
    }

    #[tokio::test]
    async fn read_after_cursor_filters() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=5 {
            let job = test_job(&format!("j{i}"), "m");
            let ev = OutboxEvent::from_job(&job, i, OutboxEventType::Fired, 1000);
            append(dir.path(), &ev).await;
        }
        // cursor 2 → 只剩 id 3,4,5
        let after2 = read_after(dir.path(), 2, 100).await.unwrap();
        assert_eq!(after2.len(), 3);
        assert_eq!(after2[0].event_id, 3);

        // cursor 5 → 空
        let after5 = read_after(dir.path(), 5, 100).await.unwrap();
        assert!(after5.is_empty());
    }

    #[tokio::test]
    async fn read_after_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=10 {
            let job = test_job(&format!("j{i}"), "m");
            let ev = OutboxEvent::from_job(&job, i, OutboxEventType::Fired, 1000);
            append(dir.path(), &ev).await;
        }
        let limited = read_after(dir.path(), 0, 4).await.unwrap();
        assert_eq!(limited.len(), 4);
        // limit 截断的是最早 4 条（升序）
        assert_eq!(limited[0].event_id, 1);
        assert_eq!(limited[3].event_id, 4);
    }

    #[tokio::test]
    async fn read_after_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let r = read_after(dir.path(), 0, 100).await.unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn next_event_id_from_empty() {
        let dir = tempfile::tempdir().unwrap();
        // 文件不存在
        assert_eq!(next_event_id(dir.path()).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn next_event_id_from_populated() {
        let dir = tempfile::tempdir().unwrap();
        // 乱序写入 id 3,1,2 → max=3 → next=4
        for id in [3_u64, 1, 2] {
            let job = test_job("j", "m");
            let ev = OutboxEvent::from_job(&job, id, OutboxEventType::Fired, 1000);
            append(dir.path(), &ev).await;
        }
        assert_eq!(next_event_id(dir.path()).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn next_event_id_seeds_monotonic_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        // 第一段：写 id 1,2
        for id in 1..=2_u64 {
            let job = test_job("j", "m");
            let ev = OutboxEvent::from_job(&job, id, OutboxEventType::Fired, 1000);
            append(dir.path(), &ev).await;
        }
        // 模拟 daemon 重启：扫描得 next=3
        let next = next_event_id(dir.path()).await.unwrap();
        assert_eq!(next, 3);
        // 续写 id 3 → read_after(2) 拿到它
        let job = test_job("j", "m");
        let ev = OutboxEvent::from_job(&job, next, OutboxEventType::Fired, 1000);
        append(dir.path(), &ev).await;
        let after = read_after(dir.path(), 2, 100).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].event_id, 3);
    }

    #[tokio::test]
    async fn next_event_id_scans_rotated_when_current_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("rotated-high", "m");
        let high = OutboxEvent::from_job(&job, 42, OutboxEventType::Fired, 1000);
        fs::write(
            path_with_ext_num(&path, 1),
            format!("{}\n", serde_json::to_string(&high).unwrap()),
        )
        .await
        .unwrap();

        assert!(!path.exists(), "current 特意缺失，模拟 rotate 后 crash");
        assert_eq!(
            next_event_id(dir.path()).await.unwrap(),
            43,
            "seed 必须覆盖 retained .1，不能在 current 缺失时回退到 1"
        );
    }

    #[tokio::test]
    async fn next_event_id_uses_retained_max_over_lower_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("retained-max", "m");
        let high = OutboxEvent::from_job(&job, 42, OutboxEventType::Fired, 1000);
        let low = OutboxEvent::from_job(&job, 2, OutboxEventType::Fired, 1001);
        fs::write(
            path_with_ext_num(&path, 1),
            format!("{}\n", serde_json::to_string(&high).unwrap()),
        )
        .await
        .unwrap();
        fs::write(&path, format!("{}\n", serde_json::to_string(&low).unwrap()))
            .await
            .unwrap();

        assert_eq!(
            next_event_id(dir.path()).await.unwrap(),
            43,
            "current 的低 id 不能让 seed 低于 retained max+1"
        );
    }

    #[tokio::test]
    async fn next_event_id_fails_loud_on_corrupt_complete_rotated_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("rotated-corrupt", "m");
        let high = OutboxEvent::from_job(&job, 77, OutboxEventType::Fired, 1000);
        let content = format!(
            "{{ not json\n{}\n{{ also broken\n",
            serde_json::to_string(&high).unwrap()
        );
        fs::write(path_with_ext_num(&path, 2), content)
            .await
            .unwrap();

        let error = next_event_id(dir.path())
            .await
            .expect_err("complete corrupt line must prevent seed");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn next_event_id_refuses_reuse_at_js_max_safe_integer() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("max-id", "m");
        let max = OutboxEvent::from_job(&job, MAX_SAFE_EVENT_ID, OutboxEventType::Fired, 1000);
        fs::write(&path, format!("{}\n", serde_json::to_string(&max).unwrap()))
            .await
            .unwrap();

        let error = next_event_id(dir.path())
            .await
            .expect_err("MAX_SAFE_INTEGER may not be reused after restart");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("MAX_SAFE_INTEGER"));
    }

    #[tokio::test]
    async fn read_after_orders_before_limit_so_out_of_order_tail_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("out-of-order", "m");
        let mut content = String::new();
        // 物理顺序故意是 2,3,1。若先按物理顺序截 limit=2，再把 max=3
        // 当 cursor，尾部 id=1 会被永久越过。
        for id in [2_u64, 3, 1] {
            let event = OutboxEvent::from_job(&job, id, OutboxEventType::Fired, 1000);
            content.push_str(&serde_json::to_string(&event).unwrap());
            content.push('\n');
        }
        fs::write(&path, content).await.unwrap();

        let first = read_after(dir.path(), 0, 2).await.unwrap();
        let first_ids: Vec<u64> = first.iter().map(|event| event.event_id).collect();
        assert_eq!(first_ids, vec![1, 2], "limit 前必须先按 event_id 排序");

        let second = read_after(dir.path(), 2, 2).await.unwrap();
        let second_ids: Vec<u64> = second.iter().map(|event| event.event_id).collect();
        assert_eq!(second_ids, vec![3], "下一页必须还能读到剩余高 id");
    }

    #[tokio::test]
    async fn rotate_moves_to_dot_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("after-rotate", "m");
        let old = OutboxEvent::from_job(&job, 1, OutboxEventType::Fired, 1000);
        let mut big = format!("{}\n", serde_json::to_string(&old).unwrap());
        let padding = format!("{}\n", " ".repeat(4096));
        while big.len() < (ROTATE_BYTES + 16) as usize {
            big.push_str(&padding);
        }
        fs::write(&path, big).await.unwrap();

        let ev = OutboxEvent::from_job(&job, 2, OutboxEventType::Fired, 2000);
        append(dir.path(), &ev).await;

        let rotated = path_with_ext_num(&path, 1);
        assert!(fs::metadata(&rotated).await.is_ok(), "应产生 .1 文件");
        let after = read_after(dir.path(), 0, 100).await.unwrap();
        assert_eq!(
            after.iter().map(|event| event.event_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn read_after_spans_rotated_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();

        // 手工构造 rotated 历史：.3 含 id 1-2，.1 含 id 3-4，当前文件含 id 5-6。
        // （.2 故意缺失 —— 验证不存在的 rotated 文件被 best-effort 跳过）
        let lines_for = |ids: &[u64]| -> String {
            let job = test_job("j", "m");
            let mut s = String::new();
            for &id in ids {
                let ev = OutboxEvent::from_job(&job, id, OutboxEventType::Fired, 1000);
                s.push_str(&serde_json::to_string(&ev).unwrap());
                s.push('\n');
            }
            s
        };
        fs::write(&path_with_ext_num(&path, 3), lines_for(&[1, 2]))
            .await
            .unwrap();
        fs::write(&path_with_ext_num(&path, 1), lines_for(&[3, 4]))
            .await
            .unwrap();
        fs::write(&path, lines_for(&[5, 6])).await.unwrap();

        // cursor 0 → 跨 3 个文件按 event_id 升序汇总（.2 缺失被跳过）
        let all = read_after(dir.path(), 0, 100).await.unwrap();
        let ids: Vec<u64> = all.iter().map(|e| e.event_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6], "跨 rotated 文件升序汇总");

        // cursor 落在 rotated 文件内 → 只剩 .1 的尾部 + 当前文件
        let after3 = read_after(dir.path(), 3, 100).await.unwrap();
        let ids2: Vec<u64> = after3.iter().map(|e| e.event_id).collect();
        assert_eq!(ids2, vec![4, 5, 6], "cursor 跨 rotation 边界正确过滤");

        // limit 跨文件累计 —— 截断的是最早的 limit 条
        let limited = read_after(dir.path(), 0, 3).await.unwrap();
        let ids3: Vec<u64> = limited.iter().map(|e| e.event_id).collect();
        assert_eq!(ids3, vec![1, 2, 3], "limit 跨文件截断最早 N 条");
    }

    #[tokio::test]
    async fn retained_scan_deduplicates_exact_rotation_copy_and_rejects_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("duplicate", "same");
        let event = OutboxEvent::from_job(&job, 7, OutboxEventType::Fired, 1000);
        let line = format!("{}\n", serde_json::to_string(&event).unwrap());
        fs::write(&path, &line).await.unwrap();
        fs::write(path_with_ext_num(&path, 1), &line).await.unwrap();

        let exact = read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(exact, vec![event.clone()]);
        assert_eq!(next_event_id(dir.path()).await.unwrap(), 8);

        let mut conflicting = event;
        conflicting.payload_message = Some("different".to_string());
        fs::write(
            path_with_ext_num(&path, 1),
            format!("{}\n", serde_json::to_string(&conflicting).unwrap()),
        )
        .await
        .unwrap();
        let error = retained_bounds(dir.path())
            .await
            .expect_err("same id with different content must fail loud");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("identity collision"));
    }

    #[tokio::test]
    async fn read_after_recovers_events_from_real_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();

        // 写一条 valid 事件 + 非 JSON padding 把文件撑过 ROTATE_BYTES 阈值。
        let job = test_job("old-event", "m");
        let ev1 = OutboxEvent::from_job(&job, 1, OutboxEventType::Fired, 1000);
        let mut content = serde_json::to_string(&ev1).unwrap();
        content.push('\n');
        let pad_line = format!("{}\n", " ".repeat(4096));
        while content.len() < (ROTATE_BYTES as usize) + 16 {
            content.push_str(&pad_line);
        }
        fs::write(&path, content).await.unwrap();

        // append → 触发 rotate（含 ev1 的文件 → .1），新事件落 fresh 当前文件
        let ev2 = OutboxEvent::from_job(&job, 2, OutboxEventType::Fired, 2000);
        append(dir.path(), &ev2).await;
        assert!(
            fs::metadata(&path_with_ext_num(&path, 1)).await.is_ok(),
            "append 应触发 rotate 产生 .1"
        );

        // read_after 跨 rotation 边界，恢复 .1 中的 ev1 + 当前文件的 ev2
        let all = read_after(dir.path(), 0, 100).await.unwrap();
        let ids: Vec<u64> = all.iter().map(|e| e.event_id).collect();
        assert_eq!(ids, vec![1, 2], "rotated .1 中的 ev1 应被 read_after 恢复");
    }

    #[tokio::test]
    async fn read_after_and_seed_fail_loud_on_corrupt_complete_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("j1", "m");
        let good = OutboxEvent::from_job(&job, 1, OutboxEventType::Fired, 1000);
        let mut content = serde_json::to_string(&good).unwrap();
        content.push('\n');
        content.push_str("{ not json\n");
        let good2 = OutboxEvent::from_job(&job, 2, OutboxEventType::Fired, 1000);
        content.push_str(&serde_json::to_string(&good2).unwrap());
        content.push('\n');
        fs::write(&path, content).await.unwrap();

        assert_eq!(
            read_after(dir.path(), 0, 100)
                .await
                .expect_err("reader must not skip complete corruption")
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            next_event_id(dir.path())
                .await
                .expect_err("seed must not use a partial scan")
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn legacy_empty_prompt_is_read_as_a_skipped_audit_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let mut legacy = OutboxEvent::from_job(
            &test_job("legacy-empty", "placeholder"),
            1,
            OutboxEventType::Fired,
            1000,
        );
        legacy.payload_message = Some("   ".to_string());
        legacy.event_type = OutboxEventType::Fired;
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .await
        .unwrap();

        let observed = read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].event_type, OutboxEventType::Skipped);
        assert!(observed[0].payload_message.is_none());
        assert_eq!(next_event_id(dir.path()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn raw_identity_collision_is_checked_before_empty_prompt_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let mut first = OutboxEvent::from_job(
            &test_job("raw-collision", "placeholder"),
            7,
            OutboxEventType::Fired,
            1000,
        );
        first.payload_message = Some(" ".to_string());
        first.event_type = OutboxEventType::Fired;
        let mut second = first.clone();
        second.payload_message = Some("\t".to_string());
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&first).unwrap()),
        )
        .await
        .unwrap();
        fs::write(
            path_with_ext_num(&path, 1),
            format!("{}\n", serde_json::to_string(&second).unwrap()),
        )
        .await
        .unwrap();

        let error = retained_bounds(dir.path())
            .await
            .expect_err("raw events differ even though both project to skipped");
        assert!(error.to_string().contains("identity collision"));
    }

    #[tokio::test]
    async fn empty_job_id_fails_loud_in_append_and_retained_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut event =
            OutboxEvent::from_job(&test_job("job", "payload"), 1, OutboxEventType::Fired, 1000);
        event.job_id = "  ".to_string();
        assert_eq!(
            append_durable(dir.path(), &event).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        let path = outbox_path(dir.path());
        assert_eq!(
            fs::symlink_metadata(&path).await.unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "invalid events must not create or append the outbox"
        );
        assert_eq!(
            fs::symlink_metadata(ledger_started_path(dir.path()))
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound,
            "invalid events must fail before recording ledger activity"
        );
        let bounds = retained_bounds(dir.path()).await.unwrap();
        assert_eq!(bounds.oldest_event_id, None);
        assert_eq!(bounds.newest_event_id, None);
        assert_eq!(next_event_id(dir.path()).await.unwrap(), 1);

        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            retained_bounds(dir.path()).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn torn_final_tail_is_ignored_for_scan_and_repaired_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = outbox_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        let job = test_job("torn", "m");
        let first = OutboxEvent::from_job(&job, 1, OutboxEventType::Fired, 1000);
        let mut bytes = serde_json::to_vec(&first).unwrap();
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"eventId":2,"jobId":"torn""#);
        fs::write(&path, bytes).await.unwrap();

        assert_eq!(next_event_id(dir.path()).await.unwrap(), 2);
        let observed = read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(observed, vec![first]);

        let second = OutboxEvent::from_job(&job, 2, OutboxEventType::Fired, 2000);
        append_durable(dir.path(), &second).await.unwrap();
        let repaired = read_after(dir.path(), 0, 10).await.unwrap();
        assert_eq!(
            repaired
                .iter()
                .map(|event| event.event_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn event_id_above_js_safe_integer_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let event = OutboxEvent::from_job(
            &test_job("unsafe", "m"),
            MAX_SAFE_EVENT_ID + 1,
            OutboxEventType::Fired,
            1000,
        );
        let error = append_durable(dir.path(), &event)
            .await
            .expect_err("unsafe JSON integer must not be persisted");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!outbox_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn ledger_epoch_is_stable_across_rotation_and_changes_on_full_reset() {
        let dir = tempfile::tempdir().unwrap();
        let first = initialize_ledger_epoch(dir.path()).await.unwrap();
        Uuid::parse_str(&first).expect("epoch must be a UUID");

        append(
            dir.path(),
            &OutboxEvent::from_job(&test_job("epoch", "payload"), 1, OutboxEventType::Fired, 1),
        )
        .await;
        let outbox = outbox_path(dir.path());
        let rotated = path_with_ext_num(&outbox, 1);
        fs::rename(&outbox, &rotated).await.unwrap();

        let after_rotation = initialize_ledger_epoch(dir.path()).await.unwrap();
        assert_eq!(
            first, after_rotation,
            "normal retention must keep the epoch"
        );

        fs::remove_file(&rotated).await.unwrap();

        let after_reset = initialize_ledger_epoch(dir.path()).await.unwrap();
        assert_ne!(first, after_reset, "full reset must start a new epoch");
        assert_eq!(
            initialize_ledger_epoch(dir.path()).await.unwrap(),
            after_reset,
            "an unused empty epoch must remain stable across restarts"
        );
    }

    #[tokio::test]
    async fn corrupt_ledger_epoch_fails_loud_instead_of_splitting_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = ledger_epoch_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, "not-an-epoch\n").await.unwrap();

        let error = load_or_create_ledger_epoch(dir.path())
            .await
            .expect_err("corrupt epoch must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(path).await.unwrap(), "not-an-epoch\n");
    }

    #[tokio::test]
    async fn corrupt_retained_line_prevents_automatic_reepoch() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = initialize_ledger_epoch(dir.path()).await.unwrap();
        append(
            dir.path(),
            &OutboxEvent::from_job(&test_job("started", "m"), 1, OutboxEventType::Fired, 1),
        )
        .await;
        fs::write(outbox_path(dir.path()), b"{complete-corruption}\n")
            .await
            .unwrap();

        let error = initialize_ledger_epoch(dir.path())
            .await
            .expect_err("incomplete retained scan must block re-epoch");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            load_or_create_ledger_epoch(dir.path()).await.unwrap(),
            epoch
        );
    }
}
