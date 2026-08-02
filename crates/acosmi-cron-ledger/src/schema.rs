//! Ledger schema DDL and the `user_version` stamp.
//!
//! Every statement in [`SCHEMA_DDL`] is `IF NOT EXISTS`, so applying it to an
//! already-initialized database is a no-op — [`crate::LedgerConnection::open`]
//! runs it unconditionally on every open. All tables are `STRICT` for
//! column-type enforcement.

/// `PRAGMA user_version` stamped after the schema is installed. Bump this (and
/// add a migration) whenever [`SCHEMA_DDL`] changes shape.
///
/// v1 → v2 (PR5-W2-1): `occurrences` gains `target` + `envelope_json` columns
/// and the `idx_occurrences_claim` index. A fresh database gets them from
/// [`SCHEMA_DDL`]; a pre-existing v1 database is upgraded in place by the
/// guarded `ALTER TABLE` in [`crate::LedgerConnection::open`] (the `IF NOT
/// EXISTS` `CREATE TABLE` never adds columns to an existing table).
pub const SCHEMA_USER_VERSION: i32 = 2;

/// Full ledger schema — tables `jobs`, `occurrences`, `attempts`, `results`,
/// `migration_state`.
///
/// W1a reads/writes only `jobs` + `occurrences` (via
/// [`crate::LedgerConnection::fire_occurrence`]). `attempts` / `results` /
/// `migration_state` are created here but exercised only by later slices
/// (W1b migration, W2 lease/claim, W3 results).
///
/// `occurrences.target` + `occurrences.envelope_json` (PR5-W2-1) carry the
/// immutable per-fire [`crate::ExecutionEnvelope`]: `envelope_json` is the full
/// serialized projection, `target` is its routing key denormalized for the
/// `idx_occurrences_claim` index a later claim/lease wave scans. Both are plain
/// nullable `TEXT` — a `STRICT` table cannot `ALTER`-add a `NOT NULL` / `CHECK`
/// column, so the envelope's domain is enforced in Rust, not by SQLite. A
/// migration-imported legacy occurrence leaves both `NULL` (it has no envelope).
///
/// The `idx_occurrences_claim` index on `(target, status, scheduled_at_ms)` is
/// deliberately **not** in this DDL: it references `target`, but a v1→v2 in-place
/// upgrade adds `target` via `ALTER` *after* this `IF NOT EXISTS` DDL no-ops the
/// existing table, so an index-create here would hit "no such column: target".
/// It is created (idempotently, after the columns are ensured) by
/// [`crate::LedgerConnection::open`] on every open — see
/// `LedgerConnection::upgrade_occurrences_to_v2`.
///
/// FK design (deliberate):
/// - `occurrences.job_id` has **no** foreign key to `jobs`. A one-shot job row
///   is deleted the instant it fires (`FireAdvance::Delete`), yet its
///   occurrence must remain for audit. With `foreign_keys = ON` an FK would
///   either block that delete (RESTRICT) or cascade the occurrence away — both
///   wrong. Identity is enforced instead by `UNIQUE(job_id, scheduled_at_ms)`.
/// - `attempts.occurrence_id` → `occurrences(id)` and `results.attempt_id` →
///   `attempts(id)` **do** carry FKs: those parent rows are never deleted.
pub const SCHEMA_DDL: &str = "\
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY, enabled INTEGER NOT NULL, owner TEXT,
    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
    job_json TEXT NOT NULL CHECK(json_valid(job_json))
) STRICT;
CREATE INDEX IF NOT EXISTS idx_jobs_enabled ON jobs(enabled);
CREATE INDEX IF NOT EXISTS idx_jobs_owner ON jobs(owner) WHERE owner IS NOT NULL;

CREATE TABLE IF NOT EXISTS occurrences (
    id INTEGER PRIMARY KEY, job_id TEXT NOT NULL, scheduled_at_ms INTEGER NOT NULL,
    occurrence_kind TEXT NOT NULL CHECK(occurrence_kind IN ('scheduled','manual','missed_catchup','missed_skipped')),
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','claimed','completed','failed','skipped','needs_adoption')),
    fence INTEGER NOT NULL DEFAULT 0, emitted_at_ms INTEGER NOT NULL,
    payload_message TEXT, job_name TEXT, legacy_event_id INTEGER,
    target TEXT, envelope_json TEXT,
    UNIQUE(job_id, scheduled_at_ms)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_occurrences_status ON occurrences(status);
CREATE INDEX IF NOT EXISTS idx_occurrences_job_id ON occurrences(job_id);

CREATE TABLE IF NOT EXISTS attempts (
    id INTEGER PRIMARY KEY,
    occurrence_id INTEGER NOT NULL REFERENCES occurrences(id),
    fence INTEGER NOT NULL,
    consumer_kind TEXT CHECK(consumer_kind = 'tui'),
    consumer_id TEXT, leased_at_ms INTEGER, lease_expires_at_ms INTEGER, lease_token TEXT,
    outcome TEXT CHECK(outcome IN ('accepted','abandoned','expired'))
) STRICT;
CREATE INDEX IF NOT EXISTS idx_attempts_occurrence_id ON attempts(occurrence_id);

CREATE TABLE IF NOT EXISTS results (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    fence INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('success','failure','error')),
    error_message TEXT, duration_ms INTEGER, recorded_at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_results_attempt_id ON results(attempt_id);

CREATE TABLE IF NOT EXISTS migration_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_epoch TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('imported','cutover_committed')),
    source_job_count INTEGER NOT NULL, source_hash TEXT NOT NULL,
    source_backup_path TEXT NOT NULL, imported_outbox_events INTEGER NOT NULL,
    needs_adoption_count INTEGER NOT NULL DEFAULT 0, migrated_at_ms INTEGER NOT NULL
) STRICT;
";
