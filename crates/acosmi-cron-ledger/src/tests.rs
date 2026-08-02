//! W1a unit tests for the cron SQLite-WAL ledger.
//!
//! In-crate (`#[cfg(test)] mod tests`) so they may touch the `pub(crate) conn`
//! handle for raw setup/inspection SQL. Every `Result` is unwrapped with
//! `.expect(...)` (never `.unwrap()`, which the workspace denies via
//! `clippy::unwrap_used`).

use std::path::{Path, PathBuf};

use acosmi_scheduler::{
    CronDelivery, CronJob, CronJobCreate, CronPayload, CronSchedule, DeliveryMode, PayloadKind,
    ScheduleKind, SessionTarget, WakeMode,
};
use rusqlite::params;
use tempfile::tempdir;

use crate::{
    CancelOutcome, ClaimOutcome, ClaimedOccurrence, ConsumerClaim, ConsumerKind,
    DEAD_LETTER_THRESHOLD, EnvelopeDeliveryMode, EnvelopePayloadKind, EnvelopeTarget,
    EnvelopeWakeMode, ExecutionEnvelope, FireAdvance, FireOutcome, LedgerConnection,
    OccurrenceKind, ReportedFailureKind, RetryOutcome, WritebackOutcome,
};

// ─────────────────────────── helpers ───────────────────────────

/// The SQLite `-wal` sidecar path for a db path (`<db>-wal`, byte-appended —
/// not an extension replace).
fn wal_sidecar(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

/// A minimal recurring `CronJob` with a caller-chosen id (so tests can address
/// the `jobs` row deterministically).
fn minimal_job(id: &str) -> CronJob {
    let mut job = CronJobCreate {
        agent_id: None,
        name: "ledger-test-job".to_string(),
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
        session_target: Default::default(),
        wake_mode: Default::default(),
        payload: CronPayload {
            kind: PayloadKind::AgentTurn,
            text: None,
            message: Some("tick".to_string()),
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

/// Insert a `jobs` row for `job` (serializing it to `job_json`).
fn insert_job(led: &LedgerConnection, job: &CronJob) {
    let job_json = serde_json::to_string(job).expect("serialize job to json");
    led.conn
        .execute(
            "INSERT INTO jobs (id, enabled, owner, created_at_ms, updated_at_ms, job_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job.id,
                job.enabled,
                job.owner,
                job.created_at_ms,
                job.updated_at_ms,
                job_json
            ],
        )
        .expect("insert job row");
}

fn count(led: &LedgerConnection, sql: &str) -> i64 {
    led.conn
        .query_row(sql, [], |r| r.get(0))
        .expect("count query")
}

/// The immutable per-fire envelope for `job` — the arg `fire_occurrence` now
/// requires. Thin wrapper over [`ExecutionEnvelope::from_job`] so the fire call
/// sites read cleanly.
fn envelope_for(job: &CronJob) -> ExecutionEnvelope {
    ExecutionEnvelope::from_job(job)
}

// ─────────────────────────── tests ───────────────────────────

#[test]
fn schema_ddl_is_idempotent_across_repeated_open() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let led1 = LedgerConnection::open(&db).expect("first open");
    drop(led1);
    // Re-opening runs SCHEMA_DDL again over the populated schema; every
    // statement is IF NOT EXISTS, so this must not error.
    let _led2 = LedgerConnection::open(&db).expect("second open must be idempotent");
}

#[test]
fn pragma_wal_mode_persists_across_reopen() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    {
        let _led = LedgerConnection::open(&db).expect("open");
    }
    let led = LedgerConnection::open(&db).expect("reopen");
    let mode: String = led
        .conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("query journal_mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal");
}

#[test]
fn json_valid_constraint_rejects_malformed_job_json() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let led = LedgerConnection::open(&db).expect("open");

    // Bundled SQLite ships JSON1, so CHECK(json_valid(job_json)) must reject
    // malformed JSON.
    let bad = led.conn.execute(
        "INSERT INTO jobs (id, enabled, owner, created_at_ms, updated_at_ms, job_json)
         VALUES ('bad', 1, NULL, 0, 0, ?1)",
        params!["{ this is not json"],
    );
    assert!(
        bad.is_err(),
        "malformed job_json must violate CHECK(json_valid(...))"
    );

    // A well-formed object is accepted — proves the constraint isn't rejecting
    // everything.
    led.conn
        .execute(
            "INSERT INTO jobs (id, enabled, owner, created_at_ms, updated_at_ms, job_json)
             VALUES ('good', 1, NULL, 0, 0, ?1)",
            params![r#"{"ok":true}"#],
        )
        .expect("valid json must be accepted");
}

#[test]
fn fire_occurrence_same_scheduled_at_ms_is_idempotent_skip() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");
    let job = minimal_job("job-idem");
    insert_job(&led, &job);

    // First fire: recurring advance to job_json v1.
    let mut v1 = job.clone();
    v1.state.last_run_at_ms = Some(1_700);
    let json_v1 = serde_json::to_string(&v1).expect("json v1");
    let first = led
        .fire_occurrence(
            "job-idem",
            1_700,
            OccurrenceKind::Scheduled,
            1_750,
            Some("first"),
            Some("n"),
            None,
            &envelope_for(&job),
            FireAdvance::Recurring {
                new_job_json: json_v1.clone(),
            },
        )
        .expect("first fire");
    assert_eq!(first, FireOutcome::Inserted);

    // Second fire of the SAME (job_id, scheduled_at_ms), with a DIFFERENT
    // advance payload: must be an idempotent skip that does NOT re-apply advance.
    let mut v2 = job.clone();
    v2.state.last_run_at_ms = Some(9_999);
    let json_v2 = serde_json::to_string(&v2).expect("json v2");
    let second = led
        .fire_occurrence(
            "job-idem",
            1_700,
            OccurrenceKind::Scheduled,
            1_760,
            Some("second"),
            Some("n"),
            None,
            &envelope_for(&job),
            FireAdvance::Recurring {
                new_job_json: json_v2,
            },
        )
        .expect("second fire");
    assert_eq!(second, FireOutcome::AlreadyRecorded);

    // Exactly one occurrence row for the key.
    assert_eq!(
        count(
            &led,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-idem' AND scheduled_at_ms=1700"
        ),
        1,
        "duplicate firing must not create a second occurrence"
    );

    // Advance ran only once: stored job_json is still v1, not v2.
    let stored_json: String = led
        .conn
        .query_row("SELECT job_json FROM jobs WHERE id='job-idem'", [], |r| {
            r.get(0)
        })
        .expect("read job_json");
    assert_eq!(
        stored_json, json_v1,
        "the duplicate fire must not re-apply the advance"
    );
    let stored: CronJob = serde_json::from_str(&stored_json).expect("deser stored job");
    assert_eq!(stored.state.last_run_at_ms, Some(1_700));
}

#[test]
fn fire_occurrence_recurring_updates_job_json_atomically() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");
    let job = minimal_job("job-rec");
    insert_job(&led, &job);

    let mut advanced = job.clone();
    advanced.state.last_run_at_ms = Some(4_242);
    let new_json = serde_json::to_string(&advanced).expect("serialize advanced job");

    let out = led
        .fire_occurrence(
            "job-rec",
            4_000,
            OccurrenceKind::Scheduled,
            4_050,
            Some("m"),
            Some("job-rec"),
            None,
            &envelope_for(&job),
            FireAdvance::Recurring {
                new_job_json: new_json,
            },
        )
        .expect("fire");
    assert_eq!(out, FireOutcome::Inserted);

    // The commit persisted the new job_json; round-tripping it recovers the
    // advanced last_run_at_ms.
    let stored_json: String = led
        .conn
        .query_row("SELECT job_json FROM jobs WHERE id='job-rec'", [], |r| {
            r.get(0)
        })
        .expect("read job_json");
    let stored: CronJob = serde_json::from_str(&stored_json).expect("deser stored job");
    assert_eq!(
        stored.state.last_run_at_ms,
        Some(4_242),
        "recurring advance must persist the new job_json in the same transaction"
    );
    assert_eq!(
        count(
            &led,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-rec' AND scheduled_at_ms=4000"
        ),
        1
    );
}

#[test]
fn fire_occurrence_one_shot_deletes_job_row_but_keeps_occurrence() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");
    let job = minimal_job("job-oneshot");
    insert_job(&led, &job);

    let out = led
        .fire_occurrence(
            "job-oneshot",
            5_000,
            OccurrenceKind::Scheduled,
            5_050,
            Some("m"),
            Some("n"),
            None,
            &envelope_for(&job),
            FireAdvance::Delete,
        )
        .expect("fire");
    assert_eq!(out, FireOutcome::Inserted);

    // Job row is gone...
    assert_eq!(
        count(&led, "SELECT COUNT(*) FROM jobs WHERE id='job-oneshot'"),
        0,
        "one-shot Delete advance must remove the job row"
    );
    // ...but the occurrence remains. `occurrences.job_id` has NO FK, so the
    // delete neither cascades it away nor is blocked. (A cascade FK would make
    // this 0; a RESTRICT FK would make `fire_occurrence` error above.)
    assert_eq!(
        count(
            &led,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-oneshot' AND scheduled_at_ms=5000"
        ),
        1,
        "occurrence must survive the job-row delete for audit"
    );
}

#[test]
fn fire_occurrence_manual_kind_does_not_advance_or_delete_job() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");
    let job = minimal_job("job-manual");
    let original_json = serde_json::to_string(&job).expect("serialize job");
    insert_job(&led, &job);

    let out = led
        .fire_occurrence(
            "job-manual",
            6_000,
            OccurrenceKind::Manual,
            6_050,
            Some("manual run"),
            Some("n"),
            None,
            &envelope_for(&job),
            FireAdvance::NoAdvance,
        )
        .expect("fire");
    assert_eq!(out, FireOutcome::Inserted);

    // NoAdvance: the job row is neither deleted nor rewritten.
    let stored_json: String = led
        .conn
        .query_row("SELECT job_json FROM jobs WHERE id='job-manual'", [], |r| {
            r.get(0)
        })
        .expect("read job_json");
    assert_eq!(
        stored_json, original_json,
        "NoAdvance must leave the job row byte-identical"
    );

    // Manual kind maps to occurrence_kind='manual', status='pending'.
    let (kind, status): (String, String) = led
        .conn
        .query_row(
            "SELECT occurrence_kind, status FROM occurrences WHERE job_id='job-manual' AND scheduled_at_ms=6000",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read occurrence");
    assert_eq!(kind, "manual");
    assert_eq!(status, "pending");
}

#[test]
fn open_survives_simulated_torn_wal_after_kill() {
    use std::fs;

    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let wal = wal_sidecar(&db);

    // Build a ledger with TWO committed occurrences, both kept in the WAL
    // (auto-checkpoint disabled) so the on-disk -wal holds occ1's commit frame
    // followed by occ2's.
    let mut led = LedgerConnection::open(&db).expect("open");
    led.conn
        .pragma_update(None, "wal_autocheckpoint", 0i64)
        .expect("disable autocheckpoint");
    let job = minimal_job("job-torn");
    insert_job(&led, &job);

    led.fire_occurrence(
        "job-torn",
        1_000,
        OccurrenceKind::Scheduled,
        1_050,
        Some("occ1"),
        Some("n"),
        None,
        &envelope_for(&job),
        FireAdvance::NoAdvance,
    )
    .expect("fire occ1");
    let size_after_occ1 = fs::metadata(&wal).expect("wal stat after occ1").len();

    led.fire_occurrence(
        "job-torn",
        2_000,
        OccurrenceKind::Scheduled,
        2_050,
        Some("occ2"),
        Some("n"),
        None,
        &envelope_for(&job),
        FireAdvance::NoAdvance,
    )
    .expect("fire occ2");
    let size_after_occ2 = fs::metadata(&wal).expect("wal stat after occ2").len();
    assert!(
        size_after_occ2 > size_after_occ1,
        "occ2 must append WAL frames (occ1={size_after_occ1}, occ2={size_after_occ2})"
    );

    // Snapshot the live db + wal (shared read) BEFORE dropping the connection:
    // a clean close would checkpoint occ1/occ2 into the main db and delete the
    // -wal, erasing the scenario.
    let dir2 = tempdir().expect("tempdir2");
    let db2 = dir2.path().join("ledger.db");
    let wal2 = wal_sidecar(&db2);
    fs::copy(&db, &db2).expect("copy db");
    fs::copy(&wal, &wal2).expect("copy wal");
    drop(led);

    // Simulate a kill mid-write on occ2: truncate the WAL copy partway through
    // occ2's frames (past occ1's complete commit). WAL recovery must apply
    // everything through occ1's commit and discard the torn tail.
    let torn_len = size_after_occ1 + (size_after_occ2 - size_after_occ1) / 2;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&wal2)
        .expect("open wal2 for truncate");
    f.set_len(torn_len).expect("truncate wal2");
    drop(f);

    // Reopen on the torn copy: must succeed (open runs wal_checkpoint(TRUNCATE))
    // and reflect ONLY the last complete commit (occ1), not the torn occ2.
    let led2 = LedgerConnection::open(&db2).expect("open must survive torn WAL");
    assert_eq!(
        count(
            &led2,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-torn' AND scheduled_at_ms=1000"
        ),
        1,
        "the last complete commit before the tear must survive"
    );
    assert_eq!(
        count(
            &led2,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-torn' AND scheduled_at_ms=2000"
        ),
        0,
        "the torn (incomplete) commit must be discarded"
    );
}

// ─────────────────── PR5-W2-1: ExecutionEnvelope capture ───────────────────

#[test]
fn fire_occurrence_persists_envelope_json_and_target_on_row() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let mut job = minimal_job("job-env");
    job.session_target = SessionTarget::Isolated;
    job.payload.model = Some("model-slug".to_string());
    insert_job(&led, &job);

    let envelope = ExecutionEnvelope::from_job(&job);
    let out = led
        .fire_occurrence(
            "job-env",
            7_000,
            OccurrenceKind::Scheduled,
            7_050,
            Some("m"),
            Some("n"),
            None,
            &envelope,
            FireAdvance::NoAdvance,
        )
        .expect("fire");
    assert_eq!(out, FireOutcome::Inserted);

    // Raw columns via the pub(crate) conn: `target` denormalized + full JSON.
    let (target, envelope_json): (Option<String>, Option<String>) = led
        .conn
        .query_row(
            "SELECT target, envelope_json FROM occurrences \
             WHERE job_id='job-env' AND scheduled_at_ms=7000",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read occurrence envelope columns");
    assert_eq!(
        target.as_deref(),
        Some("isolated"),
        "target column must be the envelope target string"
    );
    let envelope_json = envelope_json.expect("envelope_json must be non-null");
    let stored: ExecutionEnvelope =
        serde_json::from_str(&envelope_json).expect("stored envelope_json parses");
    assert_eq!(
        stored, envelope,
        "stored envelope_json must equal the envelope passed to fire"
    );
    assert_eq!(stored.target, EnvelopeTarget::Isolated);
    assert_eq!(stored.model.as_deref(), Some("model-slug"));
}

#[test]
fn execution_envelope_from_job_round_trips_and_maps_fields() {
    let mut job = minimal_job("job-rt");
    job.session_target = SessionTarget::Continuation;
    job.wake_mode = WakeMode::Now;
    job.payload.kind = PayloadKind::AgentTurn;
    job.payload.model = Some("model-slug".to_string());
    job.payload.thinking = Some("high".to_string());
    job.payload.timeout_seconds = Some(300);
    job.payload.allow_unsafe_external_content = Some(true);
    job.payload.message = Some("do the thing".to_string());
    job.delivery = Some(CronDelivery {
        mode: DeliveryMode::Announce,
        channel: Some("feishu".to_string()),
        to: Some("ops".to_string()),
        best_effort: Some(true),
    });
    job.session_key = Some("sess-1".to_string());
    job.channel_id = Some("chan-1".to_string());
    job.continuation_kind = Some("both".to_string());

    let env = ExecutionEnvelope::from_job(&job);

    // Field mapping.
    assert_eq!(env.target, EnvelopeTarget::Continuation);
    assert_eq!(env.target.as_str(), "continuation");
    assert_eq!(env.model.as_deref(), Some("model-slug"));
    assert_eq!(env.thinking.as_deref(), Some("high"));
    assert_eq!(env.timeout_seconds, Some(300));
    assert_eq!(env.permission_allow_unsafe_external_content, Some(true));
    assert_eq!(env.payload_kind, EnvelopePayloadKind::AgentTurn);
    assert_eq!(env.wake_mode, EnvelopeWakeMode::Now);
    assert_eq!(env.message.as_deref(), Some("do the thing"));
    assert_eq!(env.session_key.as_deref(), Some("sess-1"));
    assert_eq!(env.channel_id.as_deref(), Some("chan-1"));
    assert_eq!(env.continuation_kind.as_deref(), Some("both"));
    assert_eq!(env.delivery.mode, EnvelopeDeliveryMode::Announce);
    assert_eq!(env.delivery.channel.as_deref(), Some("feishu"));
    assert_eq!(env.delivery.to.as_deref(), Some("ops"));
    assert_eq!(env.delivery.best_effort, Some(true));

    // Round-trip: serialize → deserialize is identity.
    let json = serde_json::to_string(&env).expect("serialize envelope");
    let back: ExecutionEnvelope = serde_json::from_str(&json).expect("deserialize envelope");
    assert_eq!(env, back, "envelope must round-trip serialize->deserialize");
}

#[test]
fn fire_occurrence_duplicate_does_not_overwrite_first_envelope() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-imm"); // session_target = Main, message "tick"
    insert_job(&led, &job);
    let env_first = ExecutionEnvelope::from_job(&job);
    assert_eq!(env_first.target, EnvelopeTarget::Main);

    let first = led
        .fire_occurrence(
            "job-imm",
            8_000,
            OccurrenceKind::Scheduled,
            8_050,
            Some("first"),
            Some("n"),
            None,
            &env_first,
            FireAdvance::NoAdvance,
        )
        .expect("first fire");
    assert_eq!(first, FireOutcome::Inserted);

    // Second fire, SAME key, DIFFERENT envelope (isolated target + new message).
    let mut job2 = job.clone();
    job2.session_target = SessionTarget::Isolated;
    job2.payload.message = Some("SECOND".to_string());
    let env_second = ExecutionEnvelope::from_job(&job2);
    assert_eq!(env_second.target, EnvelopeTarget::Isolated);
    assert_ne!(env_first, env_second);

    let second = led
        .fire_occurrence(
            "job-imm",
            8_000,
            OccurrenceKind::Scheduled,
            8_060,
            Some("second"),
            Some("n"),
            None,
            &env_second,
            FireAdvance::NoAdvance,
        )
        .expect("second fire");
    assert_eq!(second, FireOutcome::AlreadyRecorded);

    // Immutable: the first envelope + target survive; the second is dropped.
    let (target, envelope_json): (Option<String>, Option<String>) = led
        .conn
        .query_row(
            "SELECT target, envelope_json FROM occurrences \
             WHERE job_id='job-imm' AND scheduled_at_ms=8000",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read occurrence");
    assert_eq!(
        target.as_deref(),
        Some("main"),
        "duplicate fire must not overwrite the first target"
    );
    let stored: ExecutionEnvelope =
        serde_json::from_str(&envelope_json.expect("envelope_json non-null"))
            .expect("stored envelope parses");
    assert_eq!(
        stored, env_first,
        "duplicate fire must not overwrite the first envelope"
    );
    assert_ne!(stored, env_second);
    assert_eq!(
        count(
            &led,
            "SELECT COUNT(*) FROM occurrences WHERE job_id='job-imm' AND scheduled_at_ms=8000"
        ),
        1,
        "duplicate firing must not create a second occurrence"
    );
}

/// PR5-W2-1 (Task 2): opening a pre-existing schema-v1 database upgrades its
/// `occurrences` table in place — the `IF NOT EXISTS` `SCHEMA_DDL` never adds
/// columns to the existing table, so the guarded `ALTER` in `open` must.
#[test]
fn open_upgrades_v1_occurrences_table_in_place() {
    use rusqlite::Connection;

    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");

    // Build a schema-v1 database by hand: an `occurrences` table WITHOUT the v2
    // `target` / `envelope_json` columns and without `idx_occurrences_claim`,
    // stamped user_version = 1, carrying one legacy row.
    {
        let conn = Connection::open(&db).expect("open raw v1 db");
        conn.execute_batch(
            "CREATE TABLE occurrences (
                 id INTEGER PRIMARY KEY, job_id TEXT NOT NULL, scheduled_at_ms INTEGER NOT NULL,
                 occurrence_kind TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
                 fence INTEGER NOT NULL DEFAULT 0, emitted_at_ms INTEGER NOT NULL,
                 payload_message TEXT, job_name TEXT, legacy_event_id INTEGER,
                 UNIQUE(job_id, scheduled_at_ms)
             ) STRICT;
             INSERT INTO occurrences
                 (job_id, scheduled_at_ms, occurrence_kind, status, fence, emitted_at_ms)
             VALUES ('legacy-job', 100, 'scheduled', 'pending', 0, 150);",
        )
        .expect("build v1 occurrences");
        conn.pragma_update(None, "user_version", 1i32)
            .expect("stamp v1");
    }

    // Opening runs the guarded ALTER migration.
    let mut led = LedgerConnection::open(&db).expect("open upgrades v1 -> v2");

    // user_version bumped to 2.
    let user_version: i32 = led
        .conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("read user_version");
    assert_eq!(user_version, crate::SCHEMA_USER_VERSION);
    assert_eq!(user_version, 2);

    // New columns exist and the legacy row survives with NULL target/envelope_json.
    let (target, envelope_json): (Option<String>, Option<String>) = led
        .conn
        .query_row(
            "SELECT target, envelope_json FROM occurrences WHERE job_id='legacy-job'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("legacy row readable with the new columns");
    assert_eq!(target, None, "migrated legacy row has NULL target");
    assert_eq!(
        envelope_json, None,
        "migrated legacy row has NULL envelope_json"
    );

    // The claim index now exists.
    assert_eq!(
        count(
            &led,
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='index' AND name='idx_occurrences_claim'"
        ),
        1,
        "idx_occurrences_claim must be created by the v1 -> v2 upgrade"
    );

    // The upgraded db is fully usable: a fresh fire populates the new columns.
    let job = minimal_job("job-after-upgrade");
    insert_job(&led, &job);
    let envelope = ExecutionEnvelope::from_job(&job);
    led.fire_occurrence(
        "job-after-upgrade",
        200,
        OccurrenceKind::Scheduled,
        250,
        Some("m"),
        Some("n"),
        None,
        &envelope,
        FireAdvance::NoAdvance,
    )
    .expect("fire on upgraded db");
    let new_target: Option<String> = led
        .conn
        .query_row(
            "SELECT target FROM occurrences WHERE job_id='job-after-upgrade'",
            [],
            |r| r.get(0),
        )
        .expect("read new occurrence target");
    assert_eq!(new_target.as_deref(), Some("main"));

    // Re-opening the now-v2 db is an idempotent no-op (probe finds the columns).
    drop(led);
    let _reopen = LedgerConnection::open(&db).expect("reopen upgraded db is idempotent");
}

// ─────────────────── PR5-W2-2: claim / lease helpers ───────────────────

/// Fire one `pending`, envelope-bearing occurrence of `job` at `scheduled_at_ms`
/// and return its `occurrences.id`. The claim path only considers rows whose
/// `envelope_json` is non-NULL, so every claim test seeds its work through here.
/// The `jobs` row is inserted lazily (once) so a single job may own several
/// occurrences across repeated calls.
fn fire_pending_occurrence(led: &mut LedgerConnection, job: &CronJob, scheduled_at_ms: i64) -> i64 {
    let job_exists: i64 = led
        .conn
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE id = ?1",
            params![job.id],
            |r| r.get(0),
        )
        .expect("count job row");
    if job_exists == 0 {
        insert_job(led, job);
    }
    let out = led
        .fire_occurrence(
            &job.id,
            scheduled_at_ms,
            OccurrenceKind::Scheduled,
            scheduled_at_ms + 10,
            Some("seed"),
            Some(&job.name),
            None,
            &envelope_for(job),
            FireAdvance::NoAdvance,
        )
        .expect("fire pending occurrence");
    assert_eq!(
        out,
        FireOutcome::Inserted,
        "seed occurrence must be newly inserted"
    );
    led.conn
        .query_row(
            "SELECT id FROM occurrences WHERE job_id = ?1 AND scheduled_at_ms = ?2",
            params![job.id, scheduled_at_ms],
            |r| r.get(0),
        )
        .expect("read seeded occurrence id")
}

/// A `ConsumerClaim` for `kind`/`id` serving `target`.
fn consumer(kind: ConsumerKind, id: &str, target: EnvelopeTarget) -> ConsumerClaim {
    ConsumerClaim {
        kind,
        id: id.to_string(),
        target,
    }
}

/// Count `attempts` rows for one occurrence.
fn attempts_count(led: &LedgerConnection, occurrence_id: i64) -> i64 {
    led.conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE occurrence_id = ?1",
            params![occurrence_id],
            |r| r.get(0),
        )
        .expect("count attempts")
}

/// Read one occurrence's current `(status, fence)`.
fn occurrence_status_fence(led: &LedgerConnection, occurrence_id: i64) -> (String, i64) {
    led.conn
        .query_row(
            "SELECT status, fence FROM occurrences WHERE id = ?1",
            params![occurrence_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read occurrence status+fence")
}

/// The W3 result-write fence gate, encoded for tests: a result write is admitted
/// only when the writer's `fence` still equals the occurrence's *current*
/// `occurrences.fence`. A lease superseded by a re-claim carries a stale (lower)
/// fence and is rejected — the occurrence-scoped zombie-write guard W3 relies on.
fn fence_gate_admits(led: &LedgerConnection, occurrence_id: i64, write_fence: i64) -> bool {
    let current: i64 = led
        .conn
        .query_row(
            "SELECT fence FROM occurrences WHERE id = ?1",
            params![occurrence_id],
            |r| r.get(0),
        )
        .expect("read current occurrence fence");
    write_fence == current
}

/// Unwrap a [`ClaimOutcome::Claimed`], failing the test on `NoWork`.
fn expect_claimed(outcome: ClaimOutcome) -> ClaimedOccurrence {
    match outcome {
        ClaimOutcome::Claimed(claimed) => claimed,
        ClaimOutcome::NoWork => panic!("expected a claimed occurrence, got NoWork"),
    }
}

// ─────────────────────── PR5-W2-2: claim / lease tests ───────────────────────

/// Test 1: a claim flips a `pending` occurrence to `claimed`, inserts exactly one
/// `attempts` row (right consumer / lease / token / fence), bumps `fence` 0→1,
/// and returns the envelope captured for the fired job.
#[test]
fn claim_flips_pending_and_inserts_single_attempt_bumping_fence() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let mut job = minimal_job("job-claim-basic"); // Main target (default)
    job.payload.model = Some("model-A".to_string());
    let occ_id = fire_pending_occurrence(&mut led, &job, 1_000);

    // Pre-state: pending, fence 0, no attempts.
    assert_eq!(
        occurrence_status_fence(&led, occ_id),
        ("pending".to_string(), 0)
    );
    assert_eq!(attempts_count(&led, occ_id), 0);

    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "token-abc")
            .expect("claim"),
    );

    // Returned claim shape.
    assert_eq!(claimed.occurrence_id, occ_id);
    assert_eq!(claimed.job_id, "job-claim-basic");
    assert_eq!(claimed.scheduled_at_ms, 1_000);
    assert_eq!(claimed.fence, 1, "fence bumped 0 -> 1");
    assert_eq!(claimed.lease_token, "token-abc");
    assert_eq!(
        claimed.lease_expires_at_ms, 35_000,
        "lease_expires_at_ms == now_ms + lease_ms"
    );
    assert_eq!(claimed.envelope.target, EnvelopeTarget::Main);
    assert_eq!(claimed.envelope.model.as_deref(), Some("model-A"));
    assert_eq!(
        claimed.envelope,
        envelope_for(&job),
        "returned envelope must equal the fired job's envelope"
    );

    // Occurrence flipped to claimed with fence persisted at 1.
    assert_eq!(
        occurrence_status_fence(&led, occ_id),
        ("claimed".to_string(), 1)
    );

    // Exactly one attempts row, carrying the right consumer / lease identity.
    assert_eq!(attempts_count(&led, occ_id), 1);
    let (a_id, a_fence, a_kind, a_cid): (i64, i64, String, String) = led
        .conn
        .query_row(
            "SELECT id, fence, consumer_kind, consumer_id FROM attempts WHERE occurrence_id = ?1",
            params![occ_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("read attempt identity");
    assert_eq!(
        a_id, claimed.attempt_id,
        "returned attempt_id == inserted row"
    );
    assert_eq!(a_fence, 1, "lease written at the post-bump fence");
    assert_eq!(a_kind, "tui");
    assert_eq!(a_cid, "tui-1");

    let (a_leased, a_expires, a_token, a_outcome): (i64, i64, String, Option<String>) = led
        .conn
        .query_row(
            "SELECT leased_at_ms, lease_expires_at_ms, lease_token, outcome \
             FROM attempts WHERE occurrence_id = ?1",
            params![occ_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("read attempt lease");
    assert_eq!(a_leased, 5_000);
    assert_eq!(a_expires, 35_000);
    assert_eq!(a_token, "token-abc");
    assert_eq!(a_outcome, None, "outcome stays NULL until PR5-W2-3/W3");
}

/// Test 2a: a `Main` consumer does not claim an `Isolated` occurrence (and vice
/// versa) — the target boundary is exact.
#[test]
fn claim_does_not_cross_target_boundary() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let mut iso_job = minimal_job("job-iso");
    iso_job.session_target = SessionTarget::Isolated;
    let iso_occ = fire_pending_occurrence(&mut led, &iso_job, 1_000);

    // A Main consumer finds no work — the only occurrence targets Isolated.
    let main_claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    assert_eq!(
        led.claim_next_occurrence(&main_claim, 5_000, 30_000, "t")
            .expect("claim main"),
        ClaimOutcome::NoWork,
        "Main must not claim an Isolated occurrence"
    );
    // The Isolated occurrence is untouched: still pending, fence 0, no attempts.
    assert_eq!(
        occurrence_status_fence(&led, iso_occ),
        ("pending".to_string(), 0)
    );
    assert_eq!(attempts_count(&led, iso_occ), 0);

    // An Isolated consumer does claim it.
    let iso_claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Isolated);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&iso_claim, 6_000, 30_000, "t2")
            .expect("claim isolated"),
    );
    assert_eq!(claimed.occurrence_id, iso_occ);
    assert_eq!(claimed.envelope.target, EnvelopeTarget::Isolated);
}

/// Test 2b: a `needs_adoption` occurrence is never claimed. It is seeded with a
/// matching target + envelope, so its status is the *only* disqualifier.
#[test]
fn claim_never_claims_needs_adoption_occurrence() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-adopt"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    led.conn
        .execute(
            "UPDATE occurrences SET status = 'needs_adoption' WHERE id = ?1",
            params![occ],
        )
        .expect("mark needs_adoption");

    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    assert_eq!(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "t")
            .expect("claim"),
        ClaimOutcome::NoWork,
        "a needs_adoption occurrence must never be claimed"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("needs_adoption".to_string(), 0),
        "the occurrence is left untouched"
    );
    assert_eq!(attempts_count(&led, occ), 0);
}

/// Test 2c: an occurrence with `envelope_json IS NULL` (a W1b-style migrated row)
/// is never claimed. The row's `target` is set so the NULL envelope is the sole
/// disqualifier.
#[test]
fn claim_never_claims_occurrence_with_null_envelope() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    // A W1b-style migrated row: target set (so it would match a Main consumer)
    // but envelope_json NULL — the `envelope_json IS NOT NULL` guard is the only
    // thing keeping it out of the claim set.
    led.conn
        .execute(
            "INSERT INTO occurrences
                 (job_id, scheduled_at_ms, occurrence_kind, status, fence, emitted_at_ms, target, envelope_json)
             VALUES ('w1b-job', 500, 'scheduled', 'pending', 0, 550, 'main', NULL)",
            [],
        )
        .expect("insert W1b-style row");
    let occ: i64 = led
        .conn
        .query_row(
            "SELECT id FROM occurrences WHERE job_id = 'w1b-job'",
            [],
            |r| r.get(0),
        )
        .expect("read W1b occurrence id");

    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    assert_eq!(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "t")
            .expect("claim"),
        ClaimOutcome::NoWork,
        "an occurrence with NULL envelope_json must never be claimed"
    );
    assert_eq!(attempts_count(&led, occ), 0);
}

/// Test 3: two sequential claims for the same target return DIFFERENT occurrences
/// ordered by `scheduled_at_ms`; once all are leased, a further claim returns
/// `NoWork`.
#[test]
fn claim_returns_distinct_occurrences_in_scheduled_order_then_no_work() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-order"); // Main target
    // Seed three occurrences out of scheduled order; the claim must still return
    // them oldest-first.
    let occ_late = fire_pending_occurrence(&mut led, &job, 3_000);
    let occ_early = fire_pending_occurrence(&mut led, &job, 1_000);
    let occ_mid = fire_pending_occurrence(&mut led, &job, 2_000);

    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // now grows slowly, lease=1_000 so each prior lease stays live: claims never
    // re-pick an already-leased occurrence.
    let first = expect_claimed(
        led.claim_next_occurrence(&claim, 10_000, 1_000, "t1")
            .expect("c1"),
    );
    let second = expect_claimed(
        led.claim_next_occurrence(&claim, 10_100, 1_000, "t2")
            .expect("c2"),
    );
    let third = expect_claimed(
        led.claim_next_occurrence(&claim, 10_200, 1_000, "t3")
            .expect("c3"),
    );

    assert_eq!(
        first.occurrence_id, occ_early,
        "oldest scheduled_at_ms first"
    );
    assert_eq!(first.scheduled_at_ms, 1_000);
    assert_eq!(second.occurrence_id, occ_mid);
    assert_eq!(second.scheduled_at_ms, 2_000);
    assert_eq!(third.occurrence_id, occ_late);
    assert_eq!(third.scheduled_at_ms, 3_000);

    // Three distinct occurrences.
    assert_ne!(first.occurrence_id, second.occurrence_id);
    assert_ne!(second.occurrence_id, third.occurrence_id);

    // All three now leased-live (expire 11_000 / 11_100 / 11_200) as of now=10_300
    // → nothing eligible → NoWork.
    assert_eq!(
        led.claim_next_occurrence(&claim, 10_300, 1_000, "t4")
            .expect("c4"),
        ClaimOutcome::NoWork,
        "no eligible occurrence remains while all leases are live"
    );
}

/// Test 4: lease expiry / re-claim. A second claim within the lease returns
/// `NoWork`; a second claim past `lease_expires_at_ms` re-claims the SAME
/// occurrence, bumps `fence` 1→2, and inserts a second `attempts` row. The W3
/// fence gate then rejects a write bearing the OLD fence (1) and admits the new
/// one (2).
#[test]
fn claim_re_claims_after_lease_expiry_and_fence_gate_rejects_stale_writer() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-lease"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // First claim: now=100, lease=50 → expires 150, fence 0→1.
    let first = expect_claimed(
        led.claim_next_occurrence(&claim, 100, 50, "lease-1")
            .expect("c1"),
    );
    assert_eq!(first.fence, 1);
    assert_eq!(first.lease_expires_at_ms, 150);

    // Within the lease (now=120 < 150): NoWork, occurrence untouched at fence 1,
    // still exactly one attempt.
    assert_eq!(
        led.claim_next_occurrence(&claim, 120, 50, "lease-x")
            .expect("c-within"),
        ClaimOutcome::NoWork,
        "a live lease blocks re-claim"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 1)
    );
    assert_eq!(
        attempts_count(&led, occ),
        1,
        "no second attempt while the lease is live"
    );

    // Past the lease (now=200 > 150): RE-claims the SAME occurrence, fence 1→2,
    // a second attempts row.
    let second = expect_claimed(
        led.claim_next_occurrence(&claim, 200, 50, "lease-2")
            .expect("c2"),
    );
    assert_eq!(
        second.occurrence_id, occ,
        "the expired lease re-claims the same occurrence"
    );
    assert_eq!(second.fence, 2, "fence bumped 1 -> 2 on re-claim");
    assert_eq!(
        attempts_count(&led, occ),
        2,
        "the re-claim inserts a second attempt"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 2)
    );

    // W3 fence gate: the FIRST (superseded) lease's write carries the stale fence
    // 1 and is rejected; the current fence 2 passes.
    assert!(
        !fence_gate_admits(&led, occ, first.fence),
        "a result write at the stale fence 1 must be rejected"
    );
    assert!(
        fence_gate_admits(&led, occ, second.fence),
        "a result write at the current fence 2 must be admitted"
    );
}

/// Test 5: the returned envelope is the one captured at fire time — mutating the
/// live `jobs` row afterwards does not change what the claim yields.
#[test]
fn claim_returns_envelope_captured_at_fire_time() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let mut job = minimal_job("job-env-A"); // Main target
    job.payload.model = Some("model-A".to_string());
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let fired_envelope = envelope_for(&job);

    // Mutate the live jobs row AFTER the fire (model B). The claim must still see
    // the frozen fire-time projection (model A).
    let mut mutated = job.clone();
    mutated.payload.model = Some("model-B".to_string());
    let mutated_json = serde_json::to_string(&mutated).expect("serialize mutated job");
    led.conn
        .execute(
            "UPDATE jobs SET job_json = ?1 WHERE id = ?2",
            params![mutated_json, job.id],
        )
        .expect("mutate job row");

    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "t")
            .expect("claim"),
    );

    assert_eq!(claimed.occurrence_id, occ);
    assert_eq!(
        claimed.envelope.model.as_deref(),
        Some("model-A"),
        "the envelope is frozen at fire time, not read from the mutated job"
    );
    assert_eq!(
        claimed.envelope, fired_envelope,
        "claim returns the fire-time envelope verbatim"
    );
}

/// Test 6: the fence is monotonic per occurrence across successive re-claims
/// (1 → 2 → 3), one attempt per claim.
#[test]
fn claim_fence_is_monotonic_per_occurrence_across_reclaims() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-mono"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // Three successive claims, each after the prior lease (lease=10) expires:
    // now 100 → fence 1, now 200 → fence 2, now 300 → fence 3.
    let c1 = expect_claimed(
        led.claim_next_occurrence(&claim, 100, 10, "t1")
            .expect("c1"),
    );
    let c2 = expect_claimed(
        led.claim_next_occurrence(&claim, 200, 10, "t2")
            .expect("c2"),
    );
    let c3 = expect_claimed(
        led.claim_next_occurrence(&claim, 300, 10, "t3")
            .expect("c3"),
    );

    assert_eq!((c1.occurrence_id, c1.fence), (occ, 1));
    assert_eq!((c2.occurrence_id, c2.fence), (occ, 2));
    assert_eq!((c3.occurrence_id, c3.fence), (occ, 3));
    assert!(
        c1.fence < c2.fence && c2.fence < c3.fence,
        "fence strictly increases per occurrence across re-claims"
    );
    assert_eq!(attempts_count(&led, occ), 3, "one attempt row per claim");
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 3)
    );
}

// ─────────────── PR5-W3-1: result writeback helpers ───────────────

/// Total number of `results` rows in the ledger (each test uses a fresh db, so a
/// whole-table count is an exact assertion of "how many results were written").
fn results_count(led: &LedgerConnection) -> i64 {
    count(led, "SELECT COUNT(*) FROM results")
}

/// Read one attempt's `outcome` column — NULL until a W3 writeback resolves the
/// lease (`accepted` / `abandoned`), or a later sweep marks it `expired`.
fn attempt_outcome(led: &LedgerConnection, attempt_id: i64) -> Option<String> {
    led.conn
        .query_row(
            "SELECT outcome FROM attempts WHERE id = ?1",
            params![attempt_id],
            |r| r.get(0),
        )
        .expect("read attempt outcome")
}

/// Read the single `results` row for `attempt_id` as
/// `(attempt_id, fence, status, error_message, duration_ms, recorded_at_ms)`.
fn result_row_for_attempt(
    led: &LedgerConnection,
    attempt_id: i64,
) -> (i64, i64, String, Option<String>, Option<i64>, i64) {
    led.conn
        .query_row(
            "SELECT attempt_id, fence, status, error_message, duration_ms, recorded_at_ms \
             FROM results WHERE attempt_id = ?1",
            params![attempt_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("read results row for attempt")
}

// ─────────────────── PR5-W3-1: result writeback tests ───────────────────

/// Test 1: a success writeback flips the occurrence to `completed`, resolves the
/// lease to `accepted`, and writes exactly one `results` row (`success`) carrying
/// the right `attempt_id` / `fence` / `duration_ms` / `recorded_at_ms`.
#[test]
fn accept_occurrence_records_success_and_resolves_lease() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-accept"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );
    assert_eq!(claimed.fence, 1, "claim bumped fence 0 -> 1");

    let outcome = led
        .accept_occurrence(occ, claimed.attempt_id, claimed.fence, Some(1_234), 6_000)
        .expect("accept");
    assert_eq!(
        outcome,
        WritebackOutcome::Recorded {
            occurrence_id: occ,
            fence: 1
        }
    );

    // Occurrence advanced to completed, fence unchanged at 1.
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("completed".to_string(), 1)
    );

    // The lease is resolved to 'accepted'.
    assert_eq!(
        attempt_outcome(&led, claimed.attempt_id),
        Some("accepted".to_string())
    );

    // Exactly one results row, success, with the right identity.
    assert_eq!(results_count(&led), 1);
    let (r_attempt, r_fence, r_status, r_err, r_dur, r_recorded) =
        result_row_for_attempt(&led, claimed.attempt_id);
    assert_eq!(r_attempt, claimed.attempt_id, "results.attempt_id");
    assert_eq!(r_fence, 1, "results.fence == claimed_fence");
    assert_eq!(r_status, "success");
    assert_eq!(r_err, None, "success writes no error_message");
    assert_eq!(r_dur, Some(1_234));
    assert_eq!(r_recorded, 6_000, "recorded_at_ms == injected now_ms");
}

/// Test 2: a failure writeback flips the occurrence to `failed`, `abandon`s the
/// lease, and writes one `results` row with the classified status + message. Also
/// pins the [`ReportedFailureKind`] → `results.status` mapping.
#[test]
fn report_failure_occurrence_records_error_and_abandons_lease() {
    // Mapping is contract: Failure -> 'failure', Error -> 'error'.
    assert_eq!(ReportedFailureKind::Failure.as_str(), "failure");
    assert_eq!(ReportedFailureKind::Error.as_str(), "error");

    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-fail"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );
    assert_eq!(claimed.fence, 1);

    let outcome = led
        .report_failure_occurrence(
            occ,
            claimed.attempt_id,
            claimed.fence,
            ReportedFailureKind::Error,
            Some("boom"),
            Some(99),
            6_000,
        )
        .expect("report failure");
    assert_eq!(
        outcome,
        WritebackOutcome::Recorded {
            occurrence_id: occ,
            fence: 1
        }
    );

    // Occurrence advanced to failed.
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("failed".to_string(), 1)
    );

    // The lease is 'abandoned' (there is no 'failed' lease-outcome value).
    assert_eq!(
        attempt_outcome(&led, claimed.attempt_id),
        Some("abandoned".to_string())
    );

    // One results row: status 'error', error_message 'boom'.
    assert_eq!(results_count(&led), 1);
    let (r_attempt, r_fence, r_status, r_err, r_dur, r_recorded) =
        result_row_for_attempt(&led, claimed.attempt_id);
    assert_eq!(r_attempt, claimed.attempt_id);
    assert_eq!(r_fence, 1);
    assert_eq!(r_status, "error", "Error kind -> results.status 'error'");
    assert_eq!(r_err.as_deref(), Some("boom"));
    assert_eq!(r_dur, Some(99));
    assert_eq!(r_recorded, 6_000);
}

/// Test 3: the fence gate rejects a superseded writer and writes NOTHING. A
/// first claim (fence 1) is superseded by a re-claim after lease expiry (fence
/// 2); the first claimer's `accept_occurrence` at the stale fence 1 must return
/// `Fenced { expected_fence:1, actual_fence:Some(2) }`, leave the occurrence
/// `claimed` (not `completed`), write no `results` row, and leave the first
/// lease's `outcome` untouched (it becomes `expired` only via a later sweep).
#[test]
fn accept_fence_gate_rejects_superseded_writer_and_writes_nothing() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-fence"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // First claim: now=100, lease=50 → expires 150, fence 0→1.
    let first = expect_claimed(
        led.claim_next_occurrence(&claim, 100, 50, "lease-1")
            .expect("c1"),
    );
    assert_eq!(first.fence, 1);

    // Re-claim past the lease: now=200 > 150 → same occurrence, fence 1→2, and a
    // second (live) attempts row.
    let second = expect_claimed(
        led.claim_next_occurrence(&claim, 200, 50, "lease-2")
            .expect("c2"),
    );
    assert_eq!(second.occurrence_id, occ);
    assert_eq!(second.fence, 2);
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 2)
    );

    // The FIRST (superseded) claimer tries to accept at its now-stale fence 1.
    let outcome = led
        .accept_occurrence(occ, first.attempt_id, first.fence, Some(7), 300)
        .expect("accept stale");
    assert_eq!(
        outcome,
        WritebackOutcome::Fenced {
            occurrence_id: occ,
            expected_fence: 1,
            actual_fence: Some(2),
        }
    );

    // The occurrence is STILL claimed at fence 2 — not advanced to completed.
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 2)
    );

    // The rejected accept wrote NO results row.
    assert_eq!(results_count(&led), 0, "a fenced writer records nothing");

    // Neither lease's outcome was touched by the rejected accept: the first
    // attempt is NOT marked (it becomes 'expired' only via the W3-2 sweep), and
    // the second (live) lease is likewise still unresolved.
    assert_eq!(attempt_outcome(&led, first.attempt_id), None);
    assert_eq!(attempt_outcome(&led, second.attempt_id), None);
}

/// Test 4: a double-accept is an idempotent reject. The first accept records
/// success (occurrence `completed`); the second, at the same fence, hits the
/// `status = 'claimed'` guard (the occurrence is no longer claimed) and returns
/// `Fenced` — even though `actual_fence == expected_fence` — writing no second
/// `results` row.
#[test]
fn double_accept_is_idempotent_reject_no_second_result() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-double"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );

    // First accept: Recorded, occurrence completed, one results row.
    let first = led
        .accept_occurrence(occ, claimed.attempt_id, claimed.fence, Some(10), 6_000)
        .expect("accept 1");
    assert_eq!(
        first,
        WritebackOutcome::Recorded {
            occurrence_id: occ,
            fence: 1
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("completed".to_string(), 1)
    );
    assert_eq!(results_count(&led), 1);

    // Second accept, SAME fence: the occurrence is no longer 'claimed', so the
    // gate rejects. actual_fence == expected_fence (1) — the status guard, not a
    // fence mismatch, is what fenced it.
    let second = led
        .accept_occurrence(occ, claimed.attempt_id, claimed.fence, Some(20), 7_000)
        .expect("accept 2");
    assert_eq!(
        second,
        WritebackOutcome::Fenced {
            occurrence_id: occ,
            expected_fence: 1,
            actual_fence: Some(1),
        }
    );

    // No second results row; occurrence unchanged; lease outcome unchanged.
    assert_eq!(
        results_count(&led),
        1,
        "double-accept writes no second result"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("completed".to_string(), 1)
    );
    assert_eq!(
        attempt_outcome(&led, claimed.attempt_id),
        Some("accepted".to_string()),
        "the first accept's lease outcome survives the rejected second accept"
    );
}

/// Test 5: `occurrence_history` joins occurrences to their attempts and results.
/// One occurrence is claimed+accepted (attempt + result present); the other is
/// left pending (a bare row with all attempt/result columns NULL). Rows come back
/// ordered by `scheduled_at_ms`.
#[test]
fn occurrence_history_joins_attempts_and_results() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-hist"); // Main target
    let occ_done = fire_pending_occurrence(&mut led, &job, 1_000);
    let occ_pending = fire_pending_occurrence(&mut led, &job, 2_000);

    // Claim the oldest (occ_done at 1_000) and accept it; leave occ_pending
    // untouched (never claimed → no attempt, no result).
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );
    assert_eq!(
        claimed.occurrence_id, occ_done,
        "claim picks the oldest scheduled_at_ms first"
    );
    led.accept_occurrence(occ_done, claimed.attempt_id, claimed.fence, Some(42), 6_000)
        .expect("accept");

    let rows = led.occurrence_history(&job.id).expect("history");
    assert_eq!(
        rows.len(),
        2,
        "one joined row per occurrence (each has ≤ 1 attempt here)"
    );

    // Ordering: scheduled_at_ms ascending → occ_done (1_000) before occ_pending.
    assert_eq!(rows[0].occurrence_id, occ_done);
    assert_eq!(rows[1].occurrence_id, occ_pending);

    // Row 0: occ_done fully joined (attempt + result).
    let done = &rows[0];
    assert_eq!(done.scheduled_at_ms, 1_000);
    assert_eq!(done.status, "completed");
    assert_eq!(done.fence, 1);
    assert_eq!(done.target.as_deref(), Some("main"));
    assert_eq!(done.attempt_id, Some(claimed.attempt_id));
    assert_eq!(done.outcome.as_deref(), Some("accepted"));
    assert_eq!(done.result_status.as_deref(), Some("success"));
    assert_eq!(done.error_message, None);
    assert_eq!(done.duration_ms, Some(42));
    assert_eq!(done.recorded_at_ms, Some(6_000));

    // Row 1: occ_pending bare — every attempt/result column NULL.
    let pending = &rows[1];
    assert_eq!(pending.scheduled_at_ms, 2_000);
    assert_eq!(pending.status, "pending");
    assert_eq!(pending.fence, 0);
    assert_eq!(pending.target.as_deref(), Some("main"));
    assert_eq!(pending.attempt_id, None);
    assert_eq!(pending.outcome, None);
    assert_eq!(pending.result_status, None);
    assert_eq!(pending.error_message, None);
    assert_eq!(pending.duration_ms, None);
    assert_eq!(pending.recorded_at_ms, None);
}

// ─────── PR5-W3-2: expiry sweep / dead-letter / retry / cancel helpers ───────

/// Fire a fresh occurrence for `job` at `scheduled_at_ms`, claim it, and drive it
/// to a terminal outcome — `completed` when `succeed`, else `failed`. Returns the
/// occurrence id. Each occurrence must be driven terminal before the next is
/// fired, so the intervening claim is unambiguous (only the just-fired occurrence
/// is claimable). Builds the occurrence-history runs the dead-letter derivation
/// reads.
fn fire_claim_terminal(
    led: &mut LedgerConnection,
    job: &CronJob,
    scheduled_at_ms: i64,
    succeed: bool,
) -> i64 {
    let occ = fire_pending_occurrence(led, job, scheduled_at_ms);
    let claim = consumer(ConsumerKind::Tui, "tui-term", EnvelopeTarget::Main);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, scheduled_at_ms + 100, 30_000, "tok")
            .expect("claim to drive terminal"),
    );
    assert_eq!(
        claimed.occurrence_id, occ,
        "the claim must pick the just-fired occurrence"
    );
    if succeed {
        led.accept_occurrence(
            occ,
            claimed.attempt_id,
            claimed.fence,
            Some(1),
            scheduled_at_ms + 200,
        )
        .expect("accept");
    } else {
        led.report_failure_occurrence(
            occ,
            claimed.attempt_id,
            claimed.fence,
            ReportedFailureKind::Failure,
            Some("boom"),
            Some(1),
            scheduled_at_ms + 200,
        )
        .expect("report failure");
    }
    occ
}

// ─────── PR5-W3-2: expiry sweep / dead-letter / retry / cancel tests ───────

/// Test 1: `expire_superseded_attempts` marks exactly the still-unresolved leases
/// whose occurrence has advanced past them. A claim (fence 1) superseded by a
/// re-claim (fence 2) leaves the first lease stale; the sweep expires it while the
/// current-fence lease stays NULL, returns 1, and is idempotent (a second sweep
/// returns 0). A lease already resolved (`accepted`) is skipped by the
/// `outcome IS NULL` guard even when its fence is stale.
#[test]
fn expire_superseded_attempts_marks_only_superseded_unresolved_leases() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    // Occurrence A (Main): claim (fence 0->1, A1) then re-claim after lease expiry
    // (fence 1->2, A2). A1 is superseded and unresolved; A2 is current, unresolved.
    let job_main = minimal_job("job-expire-main");
    let occ_a = fire_pending_occurrence(&mut led, &job_main, 1_000);
    let main_claim = consumer(ConsumerKind::Tui, "tui-main", EnvelopeTarget::Main);
    let a1 = expect_claimed(
        led.claim_next_occurrence(&main_claim, 100, 50, "l1")
            .expect("a1"),
    );
    let a2 = expect_claimed(
        led.claim_next_occurrence(&main_claim, 200, 50, "l2")
            .expect("a2"),
    );
    assert_eq!((a1.fence, a2.fence), (1, 2));
    assert_eq!(a2.occurrence_id, occ_a);

    // Occurrence B (Isolated, so its claims never race Occurrence A's, which is
    // Main): same claim/re-claim shape, but B1 (superseded) is resolved to
    // 'accepted' up front — the sweep must skip it because the predicate is
    // `outcome IS NULL`, not "fence is stale".
    let mut job_iso = minimal_job("job-expire-iso");
    job_iso.session_target = SessionTarget::Isolated;
    let occ_b = fire_pending_occurrence(&mut led, &job_iso, 1_000);
    let iso_claim = consumer(ConsumerKind::Tui, "tui-iso", EnvelopeTarget::Isolated);
    let b1 = expect_claimed(
        led.claim_next_occurrence(&iso_claim, 100, 50, "l3")
            .expect("b1"),
    );
    let b2 = expect_claimed(
        led.claim_next_occurrence(&iso_claim, 200, 50, "l4")
            .expect("b2"),
    );
    assert_eq!((b1.fence, b2.fence), (1, 2));
    assert_eq!(b2.occurrence_id, occ_b);
    led.conn
        .execute(
            "UPDATE attempts SET outcome = 'accepted' WHERE id = ?1",
            params![b1.attempt_id],
        )
        .expect("resolve b1 to accepted");

    // Pre-sweep: A1 / A2 / B2 unresolved, B1 accepted.
    assert_eq!(attempt_outcome(&led, a1.attempt_id), None);
    assert_eq!(attempt_outcome(&led, a2.attempt_id), None);
    assert_eq!(
        attempt_outcome(&led, b1.attempt_id),
        Some("accepted".to_string())
    );
    assert_eq!(attempt_outcome(&led, b2.attempt_id), None);

    // Sweep: only A1 (unresolved AND fence 1 < occ_a fence 2) is expired.
    let swept = led.expire_superseded_attempts().expect("sweep");
    assert_eq!(
        swept, 1,
        "exactly the one superseded, unresolved lease is expired"
    );
    assert_eq!(
        attempt_outcome(&led, a1.attempt_id),
        Some("expired".to_string()),
        "the superseded, unresolved lease is marked expired"
    );
    assert_eq!(
        attempt_outcome(&led, a2.attempt_id),
        None,
        "the current-fence lease stays unresolved"
    );
    assert_eq!(
        attempt_outcome(&led, b1.attempt_id),
        Some("accepted".to_string()),
        "a superseded but already-resolved lease is untouched (outcome IS NULL guard)"
    );
    assert_eq!(attempt_outcome(&led, b2.attempt_id), None);

    // Idempotent: a second sweep (no new supersessions) marks nothing.
    assert_eq!(
        led.expire_superseded_attempts().expect("sweep 2"),
        0,
        "a second sweep is a no-op"
    );

    // The sweep is audit-only: neither occurrence's status/fence changed.
    assert_eq!(
        occurrence_status_fence(&led, occ_a),
        ("claimed".to_string(), 2),
        "the sweep touches no occurrence lifecycle"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ_b),
        ("claimed".to_string(), 2)
    );
}

/// Test 2a: `consecutive_failure_count` counts the leading run of `failed`
/// terminal occurrences (newest-first) and a `completed` in the middle resets that
/// run — non-terminal statuses never contribute.
#[test]
fn consecutive_failure_count_counts_leading_failed_run_and_completed_resets() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-cfc"); // Main target

    // No terminal occurrences yet → 0.
    assert_eq!(
        led.consecutive_failure_count("job-cfc").expect("cfc empty"),
        0
    );

    // Three failures in a row (scheduled 1000/2000/3000) → 3.
    fire_claim_terminal(&mut led, &job, 1_000, false);
    fire_claim_terminal(&mut led, &job, 2_000, false);
    fire_claim_terminal(&mut led, &job, 3_000, false);
    assert_eq!(led.consecutive_failure_count("job-cfc").expect("cfc 3"), 3);

    // A newer `completed` (4000) resets the leading run to 0.
    fire_claim_terminal(&mut led, &job, 4_000, true);
    assert_eq!(
        led.consecutive_failure_count("job-cfc")
            .expect("cfc after completed"),
        0,
        "a newer completed resets the leading run"
    );

    // Two more failures (5000/6000) after the completed → 2, NOT 5: the completed
    // at 4000 stops the newest-first walk.
    fire_claim_terminal(&mut led, &job, 5_000, false);
    fire_claim_terminal(&mut led, &job, 6_000, false);
    assert_eq!(
        led.consecutive_failure_count("job-cfc")
            .expect("cfc after reset"),
        2,
        "only the failures newer than the last completed are counted"
    );

    // A still-pending occurrence (7000) is non-terminal → excluded from the count.
    fire_pending_occurrence(&mut led, &job, 7_000);
    assert_eq!(
        led.consecutive_failure_count("job-cfc")
            .expect("cfc with pending"),
        2,
        "a non-terminal (pending) occurrence neither contributes nor interrupts"
    );
}

/// Test 2b: `is_dead_lettered` flips from false to true exactly when the
/// consecutive-failure run reaches [`DEAD_LETTER_THRESHOLD`], and a later
/// `completed` clears it (the signal is derived, not sticky).
#[test]
fn is_dead_lettered_flips_at_threshold() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-dl"); // Main target
    assert_eq!(DEAD_LETTER_THRESHOLD, 5, "threshold invariant");
    assert!(
        !led.is_dead_lettered("job-dl")
            .expect("empty not dead-lettered"),
        "a job with no failures is not dead-lettered"
    );

    // Accumulate failures one at a time; below the threshold is never dead-lettered.
    for i in 1..DEAD_LETTER_THRESHOLD {
        fire_claim_terminal(&mut led, &job, i64::from(i) * 1_000, false);
        assert_eq!(led.consecutive_failure_count("job-dl").expect("cfc"), i);
        assert!(
            !led.is_dead_lettered("job-dl").expect("below threshold"),
            "below the threshold ({i} < {DEAD_LETTER_THRESHOLD}) is not dead-lettered"
        );
    }

    // The threshold-th consecutive failure flips it.
    fire_claim_terminal(
        &mut led,
        &job,
        i64::from(DEAD_LETTER_THRESHOLD) * 1_000,
        false,
    );
    assert_eq!(
        led.consecutive_failure_count("job-dl")
            .expect("cfc at threshold"),
        DEAD_LETTER_THRESHOLD
    );
    assert!(
        led.is_dead_lettered("job-dl").expect("dead-lettered"),
        "reaching the threshold is dead-lettered"
    );

    // A subsequent `completed` clears it again.
    fire_claim_terminal(&mut led, &job, 100_000, true);
    assert!(
        !led.is_dead_lettered("job-dl").expect("cleared"),
        "a completed run resets the derived signal"
    );
}

/// Test 3: `retry_occurrence` resets a terminal `failed` occurrence to
/// re-claimable `pending` at a bumped fence, W2's claim then re-picks it, and a
/// retry of a non-`failed` (or absent) occurrence is a `NotRetryable` no-op.
#[test]
fn retry_occurrence_resets_failed_to_reclaimable_and_rejects_non_failed() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-retry"); // Main target
    let occ = fire_pending_occurrence(&mut led, &job, 1_000);
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // Claim (fence 0->1) then fail → occurrence 'failed' at fence 1, lease abandoned.
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );
    assert_eq!(claimed.fence, 1);
    led.report_failure_occurrence(
        occ,
        claimed.attempt_id,
        claimed.fence,
        ReportedFailureKind::Failure,
        Some("boom"),
        Some(1),
        6_000,
    )
    .expect("report failure");
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("failed".to_string(), 1)
    );

    // Retry: 'failed' -> 'pending', fence 1 -> 2.
    let retry = led.retry_occurrence(occ, 7_000).expect("retry");
    assert_eq!(
        retry,
        RetryOutcome::Retried {
            occurrence_id: occ,
            fence: 2
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("pending".to_string(), 2)
    );

    // W2 re-picks the retried occurrence — a fresh claim bumps fence 2 -> 3.
    let reclaimed = expect_claimed(
        led.claim_next_occurrence(&claim, 8_000, 30_000, "tok2")
            .expect("reclaim"),
    );
    assert_eq!(
        reclaimed.occurrence_id, occ,
        "retry made the occurrence claimable again"
    );
    assert_eq!(
        reclaimed.fence, 3,
        "the re-claim bumps the retried fence again"
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 3)
    );

    // Retry of a non-`failed` occurrence (now 'claimed') is a no-op.
    let not_retryable = led.retry_occurrence(occ, 9_000).expect("retry non-failed");
    assert_eq!(
        not_retryable,
        RetryOutcome::NotRetryable {
            occurrence_id: occ,
            status: Some("claimed".to_string())
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ),
        ("claimed".to_string(), 3),
        "a NotRetryable retry changes nothing"
    );

    // Retry of an absent occurrence → NotRetryable with status None.
    let absent = led.retry_occurrence(9_999, 10_000).expect("retry absent");
    assert_eq!(
        absent,
        RetryOutcome::NotRetryable {
            occurrence_id: 9_999,
            status: None
        }
    );
}

/// Test 4: `cancel_occurrence` terminates a `pending` or `claimed` occurrence as
/// `skipped` at a bumped fence, abandons any in-flight lease so the stale
/// claimer's writeback is `Fenced`, and is a `NotCancellable` no-op on an
/// already-terminal occurrence.
#[test]
fn cancel_occurrence_terminates_pending_and_claimed_and_rejects_terminal() {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let mut led = LedgerConnection::open(&db).expect("open");

    let job = minimal_job("job-cancel"); // Main target
    let claim = consumer(ConsumerKind::Tui, "tui-1", EnvelopeTarget::Main);

    // (a) Cancel a PENDING occurrence: fence 0 -> 1, status 'skipped', no attempts.
    let occ_p = fire_pending_occurrence(&mut led, &job, 1_000);
    let cancelled_p = led.cancel_occurrence(occ_p, 5_000).expect("cancel pending");
    assert_eq!(
        cancelled_p,
        CancelOutcome::Cancelled {
            occurrence_id: occ_p,
            fence: 1
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ_p),
        ("skipped".to_string(), 1)
    );
    assert_eq!(
        attempts_count(&led, occ_p),
        0,
        "cancelling a pending occurrence abandons no lease (there is none)"
    );

    // (b) Cancel a CLAIMED occurrence: the in-flight lease is abandoned, fence
    // bumps 1 -> 2, and the (now stale-fence) claimer's accept is Fenced.
    let occ_c = fire_pending_occurrence(&mut led, &job, 2_000);
    let claimed = expect_claimed(
        led.claim_next_occurrence(&claim, 5_000, 30_000, "tok")
            .expect("claim"),
    );
    assert_eq!(claimed.occurrence_id, occ_c);
    assert_eq!(claimed.fence, 1);
    let cancelled_c = led.cancel_occurrence(occ_c, 6_000).expect("cancel claimed");
    assert_eq!(
        cancelled_c,
        CancelOutcome::Cancelled {
            occurrence_id: occ_c,
            fence: 2
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ_c),
        ("skipped".to_string(), 2)
    );
    assert_eq!(
        attempt_outcome(&led, claimed.attempt_id),
        Some("abandoned".to_string()),
        "the in-flight lease is abandoned by the cancel"
    );

    // The stale claimer tries to accept at its now-superseded fence 1 → Fenced.
    let fenced = led
        .accept_occurrence(occ_c, claimed.attempt_id, claimed.fence, Some(1), 7_000)
        .expect("stale accept");
    assert_eq!(
        fenced,
        WritebackOutcome::Fenced {
            occurrence_id: occ_c,
            expected_fence: 1,
            actual_fence: Some(2),
        }
    );
    // The cancel stands: still 'skipped' at fence 2, no results written.
    assert_eq!(
        occurrence_status_fence(&led, occ_c),
        ("skipped".to_string(), 2)
    );
    assert_eq!(
        results_count(&led),
        0,
        "the fenced stale accept records nothing"
    );

    // (c) Cancel an already-terminal (completed) occurrence → NotCancellable.
    let occ_done = fire_claim_terminal(&mut led, &job, 3_000, true);
    assert_eq!(occurrence_status_fence(&led, occ_done).0, "completed");
    let not_cancellable = led
        .cancel_occurrence(occ_done, 8_000)
        .expect("cancel completed");
    assert_eq!(
        not_cancellable,
        CancelOutcome::NotCancellable {
            occurrence_id: occ_done,
            status: Some("completed".to_string()),
        }
    );
    assert_eq!(
        occurrence_status_fence(&led, occ_done),
        ("completed".to_string(), 1),
        "a NotCancellable cancel changes nothing"
    );
}
