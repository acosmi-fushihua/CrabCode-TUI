//! W1b one-shot migration: snapshot the legacy `scheduled_tasks.json` job store
//! (plus retained `cron_outbox.jsonl` events) into the SQLite ledger.
//!
//! Three phases, per the reviewed blueprint (§B.4):
//! 1. **import** — one `BEGIN IMMEDIATE` transaction: insert every job into
//!    `jobs`, every retained outbox event into `occurrences`, and one
//!    `migration_state` row with `status='imported'`.
//! 2. **verify** — outside the transaction: the `jobs` row count *and* a
//!    structural round-trip (deserialize `job_json`, compare to the in-memory
//!    source) must match. Any mismatch fails loud (returns `Err`) **before**
//!    the source is touched, so the next start re-imports idempotently.
//! 3. **cutover** — a second small transaction flips `status` to
//!    `cutover_committed` (closing the rollback window), then writes a
//!    read-only backup of the source file.
//!
//! # Non-destructive backup (COPY, not rename) — wave-boundary decision
//! The blueprint's end-state retires `scheduled_tasks.json` and makes SQLite the
//! sole authority. That retirement (`rename` → `.bak`) belongs to **W1c**, when
//! the daemon actually switches to reading SQLite. **This wave (W1b) freezes
//! `acosmi-scheduler/src/daemon.rs`**: `SchedulerDaemon::run` still calls
//! `load_jobs → task_store::read_store`, so `scheduled_tasks.json` remains the
//! live runtime authority. Renaming it away here would make the very next
//! `load_jobs` read an absent file and load **zero jobs**, silently dropping
//! every existing user's cron jobs until W1c ships. Therefore the backup is a
//! **copy**: the `.bak` snapshot is created for audit/`留痕` while the original
//! stays in place for the unchanged daemon. This satisfies both stated W1b hard
//! constraints — "SchedulerDaemon 仍读写 scheduled_tasks.json" and "不让 SQLite
//! 成为任何运行路径权威源" — and every migration_state / cutover state transition
//! is identical to the rename design. See the delivery report for the full
//! rationale.
//!
//! # Idempotency & re-entry
//! - `status == 'cutover_committed'` → [`MigrationOutcome::AlreadyMigrated`]
//!   (never re-imports; the copy is non-destructive so the source persists and
//!   is re-detected, but the committed status short-circuits before any write).
//! - `status == 'imported'` (a prior run crashed after import, or verify failed
//!   and left the partial) → clear `jobs` / `occurrences` / `migration_state`
//!   inside the import transaction and re-import from scratch.
//! - no `migration_state` row → first migration.
//! - no `scheduled_tasks.json` at all → [`MigrationOutcome::NoSource`] (a fresh
//!   install with no jobs; nothing to migrate, no state written).
//!
//! Cross-process safety rests on the caller: `crabcode-cron` holds the single
//! scheduler state lock for the whole daemon lifecycle and runs this migration
//! once at startup before any other task, so the check-then-import is never
//! raced by a second migrator.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use acosmi_scheduler::outbox::{self, OutboxEvent, OutboxEventType};
use acosmi_scheduler::task_store;
use acosmi_scheduler::{CronJob, SessionTarget};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use tracing::{error, info};

use crate::connection::LedgerConnection;
use crate::error::LedgerError;

// `migration_state.status` domain (mirrors schema.rs CHECK).
const STATUS_IMPORTED: &str = "imported";
const STATUS_CUTOVER_COMMITTED: &str = "cutover_committed";

// `occurrences.occurrence_kind` / `.status` literals used by the import
// mapping (must stay within the schema.rs CHECK domains).
const OCC_KIND_SCHEDULED: &str = "scheduled";
const OCC_KIND_MISSED_SKIPPED: &str = "missed_skipped";
const OCC_STATUS_PENDING: &str = "pending";
const OCC_STATUS_SKIPPED: &str = "skipped";
const OCC_STATUS_NEEDS_ADOPTION: &str = "needs_adoption";

/// What one call to [`LedgerConnection::migrate_json_store`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// No `scheduled_tasks.json` present — a fresh install with nothing to
    /// migrate. No `migration_state` row is written.
    NoSource,
    /// `migration_state.status` was already `cutover_committed`; this call was a
    /// short-circuit no-op (no re-import, no backup rewrite).
    AlreadyMigrated,
    /// A fresh import → verify → cutover completed on this call.
    Migrated {
        /// Jobs read from the source and inserted into `jobs`.
        source_job_count: i64,
        /// Occurrence rows actually inserted (retained outbox events, minus any
        /// `(job_id, scheduled_at_ms)` duplicates collapsed by `ON CONFLICT`).
        imported_occurrences: i64,
        /// Of those, how many landed in `needs_adoption` (Continuation-target
        /// firings that no active thread may blindly claim).
        needs_adoption_count: i64,
    },
}

/// The read-only backup path for a source store: `<source>.bak` (the file name
/// gets `.bak` appended, e.g. `scheduled_tasks.json.bak`).
pub(crate) fn backup_path_for(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

/// Classify one retained outbox event into `(occurrence_kind, status)` for the
/// `occurrences` import, per the blueprint mapping table:
///
/// | input                                             | kind             | status          |
/// |---------------------------------------------------|------------------|-----------------|
/// | `event_type == Skipped`                           | `missed_skipped` | `skipped`       |
/// | non-Skipped, job `session_target != Continuation` | `scheduled`      | `pending`       |
/// | non-Skipped, job `session_target == Continuation` | `scheduled`      | `needs_adoption`|
///
/// An orphan event (its `job_id` is absent from the imported job set, so it is
/// not in `continuation_ids`) is treated as non-Continuation → `pending`.
fn classify_occurrence(
    event: &OutboxEvent,
    continuation_ids: &HashSet<&str>,
) -> (&'static str, &'static str) {
    if event.event_type == OutboxEventType::Skipped {
        (OCC_KIND_MISSED_SKIPPED, OCC_STATUS_SKIPPED)
    } else if continuation_ids.contains(event.job_id.as_str()) {
        (OCC_KIND_SCHEDULED, OCC_STATUS_NEEDS_ADOPTION)
    } else {
        (OCC_KIND_SCHEDULED, OCC_STATUS_PENDING)
    }
}

/// Copy `source` to `backup` and mark the copy read-only.
///
/// The `.bak` mirrors the sensitive `scheduled_tasks.json` (prompt bodies +
/// session/channel keys), so it must not be group/other readable. On Unix the
/// copy is set to `0o400` (owner read-only — "0600 只读留存" minus the write
/// bit nothing needs); on other platforms the read-only attribute is set.
fn copy_readonly_backup(source: &Path, backup: &Path) -> std::io::Result<()> {
    std::fs::copy(source, backup)?;
    let mut perms = std::fs::metadata(backup)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o400);
    }
    #[cfg(not(unix))]
    {
        perms.set_readonly(true);
    }
    std::fs::set_permissions(backup, perms)?;
    Ok(())
}

impl LedgerConnection {
    /// Run the one-shot W1b migration of `cron_dir`'s legacy JSON store into
    /// this ledger. See the module docs for the phase/idempotency contract and
    /// the copy-not-rename wave-boundary decision.
    ///
    /// `cron_dir` is the cron state directory (the parent of `cron/`);
    /// `scheduled_tasks.json` and `cron_outbox.jsonl` are resolved beneath it
    /// via the existing `task_store` / `outbox` path helpers.
    ///
    /// This method is `async` solely to reuse the existing async readers
    /// ([`task_store::read_store`] and [`outbox::read_after_with_bounds`]); the
    /// SQLite transaction work itself is synchronous and never straddles an
    /// `await`. It is a one-time startup call — W1c wraps the *fire* path in
    /// `spawn_blocking`, not this.
    ///
    /// # Errors
    /// - [`LedgerError::Sqlite`] on any statement/transaction failure.
    /// - [`LedgerError::Io`] reading the source bytes or the retained outbox.
    /// - [`LedgerError::Json`] serializing a job to `job_json` or reading it
    ///   back during verify.
    /// - [`LedgerError::MigrationVerify`] if the verify phase mismatches; the
    ///   import is not promoted and the source is left untouched.
    pub async fn migrate_json_store(
        &mut self,
        cron_dir: &Path,
    ) -> Result<MigrationOutcome, LedgerError> {
        // ── Idempotency short-circuit ──────────────────────────────────────
        // Safe as a plain read before the write txn: the caller holds the
        // single scheduler lock, so no second migrator races this.
        let existing_status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM migration_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if existing_status.as_deref() == Some(STATUS_CUTOVER_COMMITTED) {
            return Ok(MigrationOutcome::AlreadyMigrated);
        }

        // ── Source presence + raw-byte hash ────────────────────────────────
        let source_path = task_store::cron_file_path(cron_dir);
        let raw_source_bytes = match tokio::fs::read(&source_path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Fresh install: nothing to migrate. Leave migration_state
                // absent so a later start (once jobs exist) does the real import.
                return Ok(MigrationOutcome::NoSource);
            }
            Err(error) => return Err(LedgerError::Io(error)),
        };
        // Hash the *original* file bytes, not a re-serialization (per §B.4).
        let mut hasher = Sha256::new();
        hasher.update(&raw_source_bytes);
        let source_hash = format!("{:x}", hasher.finalize());

        // ── Reuse existing readers for the payload ─────────────────────────
        // `read_store` handles v1→v2 auto-migration + schedule validation; call
        // it exactly once and reuse the result for both import and verify (it is
        // not idempotent for v1 sources — `CronTask → CronJob` stamps a fresh
        // `updated_at_ms`, so a second read would diverge).
        let jobs: Vec<CronJob> = task_store::read_store(cron_dir).await.jobs;
        let retained: Vec<OutboxEvent> = outbox::read_after_with_bounds(cron_dir, 0, usize::MAX)
            .await
            .map_err(LedgerError::Io)?
            .events;

        let source_job_count = i64::try_from(jobs.len()).unwrap_or(i64::MAX);
        let backup_path = backup_path_for(&source_path);
        let backup_path_str = backup_path.to_string_lossy().into_owned();
        let continuation_ids: HashSet<&str> = jobs
            .iter()
            .filter(|job| job.session_target == SessionTarget::Continuation)
            .map(|job| job.id.as_str())
            .collect();
        let schema_epoch = uuid::Uuid::new_v4().to_string();
        let imported_at_ms = chrono::Utc::now().timestamp_millis();

        // ── Phase ① import (single BEGIN IMMEDIATE txn) ────────────────────
        // No `.await` inside: the whole SQLite mutation is synchronous.
        let (imported_occurrences, needs_adoption_count) = {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;

            // Clear any prior partial import (retry after a crash/verify
            // failure that left status='imported'). No-ops on a first run.
            tx.execute("DELETE FROM occurrences", [])?;
            tx.execute("DELETE FROM jobs", [])?;
            tx.execute("DELETE FROM migration_state", [])?;

            for job in &jobs {
                let job_json = serde_json::to_string(job)?;
                tx.execute(
                    "INSERT INTO jobs (id, enabled, owner, created_at_ms, updated_at_ms, job_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        job.id,
                        job.enabled,
                        job.owner,
                        job.created_at_ms,
                        job.updated_at_ms,
                        job_json,
                    ],
                )?;
            }

            let mut imported: i64 = 0;
            let mut needs_adoption: i64 = 0;
            for event in &retained {
                let (kind, status) = classify_occurrence(event, &continuation_ids);
                let changed = tx.execute(
                    "INSERT INTO occurrences (
                         job_id, scheduled_at_ms, occurrence_kind, status, fence,
                         emitted_at_ms, payload_message, job_name, legacy_event_id
                     ) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8)
                     ON CONFLICT(job_id, scheduled_at_ms) DO NOTHING",
                    params![
                        event.job_id,
                        event.scheduled_at_ms,
                        kind,
                        status,
                        event.emitted_at_ms,
                        event.payload_message,
                        event.job_name,
                        i64::try_from(event.event_id).ok(),
                    ],
                )?;
                if changed == 1 {
                    imported += 1;
                    if status == OCC_STATUS_NEEDS_ADOPTION {
                        needs_adoption += 1;
                    }
                }
            }

            tx.execute(
                "INSERT INTO migration_state (
                     id, schema_epoch, status, source_job_count, source_hash,
                     source_backup_path, imported_outbox_events, needs_adoption_count,
                     migrated_at_ms
                 ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    schema_epoch,
                    STATUS_IMPORTED,
                    source_job_count,
                    source_hash,
                    backup_path_str,
                    imported,
                    needs_adoption,
                    imported_at_ms,
                ],
            )?;

            tx.commit()?;
            (imported, needs_adoption)
        };

        // ── Phase ② verify (reads only; fail loud before touching source) ──
        let db_job_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))?;
        if db_job_count != source_job_count {
            return Err(LedgerError::MigrationVerify(format!(
                "job row count mismatch after import: db={db_job_count}, source={source_job_count}"
            )));
        }
        let mut db_jobs: Vec<CronJob> = {
            let mut stmt = self.conn.prepare("SELECT job_json FROM jobs")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut collected: Vec<CronJob> = Vec::with_capacity(jobs.len());
            for row in rows {
                let json = row?;
                collected.push(serde_json::from_str::<CronJob>(&json)?);
            }
            collected
        };
        let mut source_jobs_sorted = jobs.clone();
        db_jobs.sort_by(|a, b| a.id.cmp(&b.id));
        source_jobs_sorted.sort_by(|a, b| a.id.cmp(&b.id));
        if db_jobs != source_jobs_sorted {
            return Err(LedgerError::MigrationVerify(
                "job round-trip mismatch after import (serialization drift)".to_string(),
            ));
        }

        // ── Phase ③ cutover (second small txn) + read-only backup copy ─────
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute(
                "UPDATE migration_state SET status = ?1, migrated_at_ms = ?2
                 WHERE id = 1 AND status = ?3",
                params![
                    STATUS_CUTOVER_COMMITTED,
                    chrono::Utc::now().timestamp_millis(),
                    STATUS_IMPORTED,
                ],
            )?;
            if changed != 1 {
                // Dropping `tx` here rolls the UPDATE back; status stays
                // 'imported' and the next start re-imports. Unreachable under
                // the single-instance lock (we just wrote 'imported').
                return Err(LedgerError::MigrationVerify(format!(
                    "cutover UPDATE affected {changed} rows (expected exactly 1)"
                )));
            }
            tx.commit()?;
        }

        // The rollback window is closed: SQLite is now the authoritative record
        // of *this snapshot*. The backup is a COPY (see module docs) — best
        // effort, so a copy failure is logged, never rolled back.
        if let Err(copy_error) = copy_readonly_backup(&source_path, &backup_path) {
            error!(
                error = %copy_error,
                source = %source_path.display(),
                backup = %backup_path.display(),
                "cron ledger migration: read-only backup copy failed; snapshot already committed to SQLite, continuing"
            );
        }

        info!(
            source_job_count,
            imported_occurrences,
            needs_adoption_count,
            schema_epoch,
            "cron ledger migration: scheduled_tasks.json snapshot imported to SQLite"
        );
        Ok(MigrationOutcome::Migrated {
            source_job_count,
            imported_occurrences,
            needs_adoption_count,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Migration unit tests. Sources are built with the real `task_store` /
    //! `outbox` writers so the migration exercises the same read path as
    //! production. `.expect(...)` throughout (the workspace denies `.unwrap()`).

    use std::path::Path;

    use acosmi_scheduler::outbox::{self, OutboxEvent, OutboxEventType};
    use acosmi_scheduler::{
        CronJob, CronJobCreate, CronPayload, CronSchedule, CronStoreFile, PayloadKind,
        STORE_VERSION, ScheduleKind, SessionTarget, WakeMode, task_store,
    };
    use rusqlite::params;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{MigrationOutcome, backup_path_for};
    use crate::LedgerConnection;

    /// A minimal recurring job with a caller-chosen id and session target.
    fn make_job(id: &str, target: SessionTarget) -> CronJob {
        let mut job = CronJobCreate {
            agent_id: None,
            name: format!("job-{id}"),
            description: None,
            owner: None,
            enabled: Some(true),
            delete_after_run: None,
            schedule: CronSchedule {
                kind: ScheduleKind::Every,
                at: None,
                every_ms: Some(60_000),
                anchor_ms: Some(0),
                expr: None,
                tz: None,
            },
            session_target: target,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::AgentTurn,
                text: None,
                message: Some(format!("msg-{id}")),
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
        }
        .into_job();
        job.id = id.to_string();
        job
    }

    /// Write `jobs` to `<dir>/cron/scheduled_tasks.json` (v2), creating
    /// `cron/` as a side effect (so the ledger db can open beside it).
    async fn write_source(dir: &Path, jobs: Vec<CronJob>) {
        let store = CronStoreFile {
            version: STORE_VERSION,
            jobs,
        };
        task_store::write_store(dir, &store)
            .await
            .expect("write scheduled_tasks.json");
    }

    async fn append_event(
        dir: &Path,
        job: &CronJob,
        event_id: u64,
        event_type: OutboxEventType,
        scheduled_at_ms: i64,
    ) {
        let event = OutboxEvent::from_job(job, event_id, event_type, scheduled_at_ms);
        outbox::append_durable(dir, &event)
            .await
            .expect("append durable outbox event");
    }

    fn open_ledger(dir: &Path) -> LedgerConnection {
        let db = task_store::cron_file_path(dir).with_file_name(crate::LEDGER_DB_FILENAME);
        LedgerConnection::open(&db).expect("open ledger")
    }

    fn occurrence_kind_status(led: &LedgerConnection, job_id: &str) -> (String, String) {
        led.conn
            .query_row(
                "SELECT occurrence_kind, status FROM occurrences WHERE job_id = ?1",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read occurrence")
    }

    fn count(led: &LedgerConnection, sql: &str) -> i64 {
        led.conn.query_row(sql, [], |r| r.get(0)).expect("count")
    }

    #[tokio::test]
    async fn migrate_preserves_all_jobs_count_and_hash_verified() {
        let dir = tempdir().expect("tempdir");
        let jobs = vec![
            make_job("alpha", SessionTarget::Main),
            make_job("beta", SessionTarget::Isolated),
            make_job("gamma", SessionTarget::Main),
        ];
        write_source(dir.path(), jobs).await;

        // Expected hash = sha256 of the raw on-disk bytes.
        let source_path = task_store::cron_file_path(dir.path());
        let raw = std::fs::read(&source_path).expect("read raw source");
        let mut hasher = Sha256::new();
        hasher.update(&raw);
        let expected_hash = format!("{:x}", hasher.finalize());

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("migration succeeds");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 3,
                imported_occurrences: 0,
                needs_adoption_count: 0,
            }
        );

        // migration_state committed + hash verified.
        let (status, sjc, hash): (String, i64, String) = led
            .conn
            .query_row(
                "SELECT status, source_job_count, source_hash FROM migration_state WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("read migration_state");
        assert_eq!(status, "cutover_committed");
        assert_eq!(sjc, 3);
        assert_eq!(
            hash, expected_hash,
            "source_hash must be sha256 of raw bytes"
        );
        assert_eq!(count(&led, "SELECT COUNT(*) FROM jobs"), 3);

        // COPY invariant: the source stays live (daemon still reads it this
        // wave) AND the read-only backup is a byte-identical copy.
        assert!(
            source_path.exists(),
            "W1b must NOT remove the live scheduled_tasks.json (daemon still reads it)"
        );
        let backup = backup_path_for(&source_path);
        assert!(backup.exists(), "read-only .bak backup must be written");
        assert_eq!(
            std::fs::read(&backup).expect("read backup"),
            raw,
            ".bak must be a byte-identical copy of the source"
        );
    }

    #[tokio::test]
    async fn migrate_continuation_target_events_become_needs_adoption() {
        let dir = tempdir().expect("tempdir");
        let job = make_job("cont", SessionTarget::Continuation);
        write_source(dir.path(), vec![job.clone()]).await;
        // A non-empty Fired event so normalization keeps it Fired (not Skipped).
        append_event(dir.path(), &job, 1, OutboxEventType::Fired, 10_000).await;

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("migration succeeds");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 1,
                imported_occurrences: 1,
                needs_adoption_count: 1,
            }
        );

        let (kind, status) = occurrence_kind_status(&led, "cont");
        assert_eq!(kind, "scheduled");
        assert_eq!(
            status, "needs_adoption",
            "Continuation-target firing must not be blindly claimable"
        );
    }

    #[tokio::test]
    async fn migrate_skipped_outbox_events_become_skipped_occurrences() {
        let dir = tempdir().expect("tempdir");
        let job = make_job("aged", SessionTarget::Main);
        write_source(dir.path(), vec![job.clone()]).await;
        append_event(dir.path(), &job, 1, OutboxEventType::Skipped, 20_000).await;

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("migration succeeds");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 1,
                imported_occurrences: 1,
                needs_adoption_count: 0,
            }
        );

        let (kind, status) = occurrence_kind_status(&led, "aged");
        assert_eq!(kind, "missed_skipped");
        assert_eq!(status, "skipped");
    }

    #[tokio::test]
    async fn migrate_is_idempotent_retry_after_verification_failure() {
        // The state a verify failure leaves behind is: import committed,
        // status='imported', cutover never reached. Reproduce that exact state
        // (a stale partial import) and confirm the retry clears it and re-imports
        // cleanly to cutover_committed.
        let dir = tempdir().expect("tempdir");
        let jobs = vec![
            make_job("a", SessionTarget::Main),
            make_job("b", SessionTarget::Main),
        ];
        write_source(dir.path(), jobs).await;

        let mut led = open_ledger(dir.path());
        // Seed a botched prior import: status='imported' + a stale job row that
        // is NOT in the source.
        led.conn
            .execute(
                "INSERT INTO migration_state (
                     id, schema_epoch, status, source_job_count, source_hash,
                     source_backup_path, imported_outbox_events, needs_adoption_count, migrated_at_ms
                 ) VALUES (1, 'stale-epoch', 'imported', 99, 'stale-hash', 'stale.bak', 0, 0, 0)",
                [],
            )
            .expect("seed stale imported state");
        led.conn
            .execute(
                "INSERT INTO jobs (id, enabled, owner, created_at_ms, updated_at_ms, job_json)
                 VALUES ('stale-x', 1, NULL, 0, 0, '{}')",
                [],
            )
            .expect("seed stale job");

        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("retry migration succeeds");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 2,
                imported_occurrences: 0,
                needs_adoption_count: 0,
            }
        );

        // The stale partial is gone; only the source's 2 jobs remain.
        assert_eq!(count(&led, "SELECT COUNT(*) FROM jobs"), 2);
        assert_eq!(
            count(&led, "SELECT COUNT(*) FROM jobs WHERE id = 'stale-x'"),
            0,
            "stale partial-import row must be cleared on retry"
        );
        let (status, sjc): (String, i64) = led
            .conn
            .query_row(
                "SELECT status, source_job_count FROM migration_state WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read migration_state");
        assert_eq!(status, "cutover_committed");
        assert_eq!(
            sjc, 2,
            "source_job_count must reflect the re-import, not 99"
        );
    }

    #[tokio::test]
    async fn migrate_refuses_second_run_after_cutover_committed() {
        let dir = tempdir().expect("tempdir");
        write_source(dir.path(), vec![make_job("only", SessionTarget::Main)]).await;

        let mut led = open_ledger(dir.path());
        let first = led
            .migrate_json_store(dir.path())
            .await
            .expect("first migration succeeds");
        assert!(matches!(first, MigrationOutcome::Migrated { .. }));
        assert_eq!(count(&led, "SELECT COUNT(*) FROM jobs"), 1);
        let epoch_after_first: String = led
            .conn
            .query_row(
                "SELECT schema_epoch FROM migration_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .expect("read schema_epoch");

        // Second call must short-circuit: no re-import, no doubling, epoch stable.
        let second = led
            .migrate_json_store(dir.path())
            .await
            .expect("second migration is a no-op");
        assert_eq!(second, MigrationOutcome::AlreadyMigrated);
        assert_eq!(
            count(&led, "SELECT COUNT(*) FROM jobs"),
            1,
            "already-migrated ledger must not re-import (would double the jobs)"
        );
        let epoch_after_second: String = led
            .conn
            .query_row(
                "SELECT schema_epoch FROM migration_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .expect("read schema_epoch again");
        assert_eq!(
            epoch_after_first, epoch_after_second,
            "no-op must not rewrite migration_state"
        );
    }

    #[tokio::test]
    async fn migrate_without_source_is_noop() {
        // Fresh install: no scheduled_tasks.json. Must skip cleanly and write no
        // migration_state (the smoke-test cold-start path).
        let dir = tempdir().expect("tempdir");
        // Create `cron/` so the ledger db can open, but write no source.
        std::fs::create_dir_all(dir.path().join("cron")).expect("mk cron");
        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("no-source migration is Ok");
        assert_eq!(outcome, MigrationOutcome::NoSource);
        assert_eq!(
            count(&led, "SELECT COUNT(*) FROM migration_state"),
            0,
            "no source → no migration_state row"
        );
    }

    // ── W-CRON-RELEASE-REOPEN（2026-07-16）脏用户状态验证 ──────────────────
    //
    // 2026-07-16 现场：NSIS PREINSTALL `taskkill /F` 斩首旧守护，零 cleanup —
    // `cron/scheduled_tasks.lock`（业务锁）、`*.json.tmp`（原子写中间态）、
    // 陈旧 `.bak` 都可能残留。发布门重开后首个新守护会在**这种**目录上跑
    // 首启迁移；以下用例把迁移对脏态的行为钉成契约。

    #[tokio::test]
    async fn migrate_ignores_stale_lock_and_tmp_leftovers() {
        // 硬杀残留指纹：陈旧业务锁 + 原子写 .tmp 与源文件并存。迁移必须照常
        // 完成（这些文件不是它的输入），且不得动它们（锁归 daemon 生命周期、
        // .tmp 归 task_store 写路径覆盖，各有其主）。
        let dir = tempdir().expect("tempdir");
        write_source(dir.path(), vec![make_job("a", SessionTarget::Main)]).await;

        let stale_lock = dir.path().join("cron/scheduled_tasks.lock");
        std::fs::write(
            &stale_lock,
            r#"{"sessionId":"dead-daemon","pid":99999999,"acquiredAt":0}"#,
        )
        .expect("write stale lock");
        let stale_tmp = dir.path().join("cron/scheduled_tasks.json.tmp");
        std::fs::write(&stale_tmp, b"{ torn atomic write leftover").expect("write stale tmp");

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("migration must succeed on a hard-kill-dirty dir");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 1,
                imported_occurrences: 0,
                needs_adoption_count: 0,
            }
        );
        assert!(stale_lock.exists(), "迁移不得清理/持有业务锁文件");
        assert!(stale_tmp.exists(), "迁移不得吞掉原子写 .tmp 残留");
        assert!(
            backup_path_for(&task_store::cron_file_path(dir.path())).exists(),
            "正常源在脏目录上仍应产出 .bak"
        );
    }

    #[tokio::test]
    async fn migrate_corrupt_source_commits_empty_snapshot_and_preserves_bytes() {
        // 源 JSON 损坏（斩首/磁盘意外）：`read_store` fail-soft 返回空店 →
        // 迁移以「0 jobs」快照提交 + .bak 保全**原始损坏字节**供取证。
        // 钉死此语义的意义：W1b 期 JSON 仍是运行权威（daemon 同样读到空店，
        // 迁移不放大损失）；若未来 W1c 让 SQLite 接管权威，本用例是「修复后
        // 的 JSON 不会被 AlreadyMigrated 短路吞掉」问题的显式提醒锚。
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("cron")).expect("mk cron");
        let source = task_store::cron_file_path(dir.path());
        let corrupt_bytes: &[u8] = b"{\"version\":2,\"jobs\":[{\"id\":\"tor"; // 截断
        std::fs::write(&source, corrupt_bytes).expect("write corrupt source");

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("corrupt source is fail-soft, not Err");
        assert_eq!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 0,
                imported_occurrences: 0,
                needs_adoption_count: 0,
            }
        );
        let backup = std::fs::read(backup_path_for(&source)).expect("read backup");
        assert_eq!(
            backup, corrupt_bytes,
            ".bak 必须保全原始损坏字节（原样取证，不是重序列化）"
        );
        // 已提交 → 第二次短路（与正常源同一幂等契约）。
        let second = led
            .migrate_json_store(dir.path())
            .await
            .expect("second run after corrupt-commit");
        assert_eq!(second, MigrationOutcome::AlreadyMigrated);
    }

    #[tokio::test]
    async fn migrate_overwrites_preexisting_stale_backup() {
        // 上一代安装曾留下过期 .bak：本次迁移的备份必须覆盖为**当前**源字节
        // （.bak 的契约 = 本次快照的取证副本，不是历史堆积）。
        let dir = tempdir().expect("tempdir");
        write_source(dir.path(), vec![make_job("fresh", SessionTarget::Main)]).await;
        let source = task_store::cron_file_path(dir.path());
        let backup = backup_path_for(&source);
        std::fs::write(&backup, b"ancient stale backup").expect("write stale backup");

        let mut led = open_ledger(dir.path());
        let outcome = led
            .migrate_json_store(dir.path())
            .await
            .expect("migration over stale backup");
        assert!(matches!(
            outcome,
            MigrationOutcome::Migrated {
                source_job_count: 1,
                ..
            }
        ));
        let backup_bytes = std::fs::read(&backup).expect("read refreshed backup");
        let source_bytes = std::fs::read(&source).expect("read source");
        assert_eq!(
            backup_bytes, source_bytes,
            "陈旧 .bak 必须被本次快照覆盖为当前源字节"
        );
    }
}
