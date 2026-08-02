//! `acosmi-cron-ledger` — the durable SQLite-WAL occurrence ledger for Acosmi
//! cron.
//!
//! This crate owns the on-disk store recording every cron *occurrence* (one
//! logical firing of a job) plus the `jobs` / `attempts` / `results` /
//! `migration_state` tables the delivery and migration paths build on. It is a
//! pure data-management layer: schema, connection/PRAGMA setup, and the
//! transactional [`LedgerConnection::fire_occurrence`] write path.
//!
//! # Scope (W1a)
//! This slice is the foundation only:
//! - the schema ([`SCHEMA_DDL`]),
//! - [`LedgerConnection::open`] (WAL + `synchronous = FULL` + `busy_timeout` +
//!   `foreign_keys`, one startup `wal_checkpoint(TRUNCATE)`),
//! - the isolated [`LedgerConnection::fire_occurrence`] function.
//!
//! It is **not** wired into the scheduler daemon and introduces no async — the
//! daemon integration and `spawn_blocking` wrapper arrive in W1c; lease/claim
//! (`attempts`) and results in W2/W3.
//!
//! # W1b (migration)
//! [`LedgerConnection::migrate_json_store`] adds the one-shot import of the
//! legacy `scheduled_tasks.json` job store (+ retained outbox events) into
//! `jobs` / `occurrences` / `migration_state`, with import→verify→cutover and a
//! read-only backup **copy** of the source (the source stays live — the frozen
//! `SchedulerDaemon` still reads it this wave). `crabcode-cron` runs it once at
//! startup before the daemon spawns. `attempts` / `results` remain unread until
//! W2/W3. This is `async` only to reuse the existing async readers; the SQLite
//! work is synchronous.
//!
//! # W2 (claim / lease)
//! [`LedgerConnection::claim_next_occurrence`] is the pure ledger claim path —
//! the first writer of the `attempts` table. In one `BEGIN IMMEDIATE` it picks
//! the oldest claimable occurrence for a consumer's [`EnvelopeTarget`], bumps the
//! occurrence-scoped `fence`, flips it to `claimed`, and writes a durable lease
//! (clock + `lease_token` injected by the caller, like `fire_occurrence` — no
//! internal clock, so it is deterministically unit-testable). A `claimed`
//! occurrence whose lease has expired at its current fence is re-claimable
//! (bumping the fence again); `needs_adoption` rows and envelope-less W1b legacy
//! rows are never claimed. The lease `outcome` and the `results` row stay unread
//! until PR5-W2-3 / W3; the actor + reverse-IPC wiring is PR5-W2-3.
//!
//! # W3 (result writeback)
//! [`LedgerConnection::accept_occurrence`] /
//! [`LedgerConnection::report_failure_occurrence`] are the pure ledger
//! result-writeback paths — the first writers of the `results` table and the
//! first to set `attempts.outcome` non-NULL. In one `BEGIN IMMEDIATE` each closes
//! out a claimed occurrence, gated by the occurrence's fence: the terminal
//! `occurrences` UPDATE's own `WHERE fence = claimed_fence AND status = 'claimed'`
//! plus its `changes()` count is an atomic compare-and-advance (no
//! SELECT-then-update), so a superseded (zombie) writer whose `claimed_fence`
//! fell behind a re-claim — or a double-writeback of an already-terminal
//! occurrence — records nothing and returns [`WritebackOutcome::Fenced`]. On a
//! gate hit the occurrence advances to `completed` / `failed`, the lease resolves
//! to `accepted` / `abandoned`, and a `results` row is written, returning
//! [`WritebackOutcome::Recorded`]. The clock (`now_ms`) is injected — no internal
//! clock — as in the fire/claim paths. [`LedgerConnection::occurrence_history`]
//! is a read-only audit accessor (occurrence ⋈ attempts ⋈ results). The actor +
//! reverse-IPC wiring is PR5-W3-3.
//!
//! # W3-2 (audit sweep / dead-letter / retry / cancel)
//! [`LedgerConnection::expire_superseded_attempts`] is an audit-only sweep that
//! marks each still-unresolved lease whose occurrence has advanced past it
//! (`attempts.fence < occurrences.fence`) as `expired`; it is idempotent and
//! changes no occurrence lifecycle. [`LedgerConnection::consecutive_failure_count`]
//! / [`DEAD_LETTER_THRESHOLD`] / [`LedgerConnection::is_dead_lettered`] derive a
//! dead-letter signal from occurrence history (the leading run of `failed`, reset
//! by a `completed`) without storing any counter — an audit input for PR-6, not
//! enforcement. [`LedgerConnection::retry_occurrence`] resets a terminal `failed`
//! occurrence to `pending` and [`LedgerConnection::cancel_occurrence`] terminates
//! a `pending` / `claimed` occurrence as `skipped`; both bump the fence (fencing
//! out any lingering writer) and, like the W3-1 paths, run in one
//! `BEGIN IMMEDIATE` with an injected clock. The actor + reverse-IPC wiring is
//! PR5-W3-3.
//!
//! # Durability contract
//! - WAL journaling with `synchronous = FULL`: a committed occurrence survives
//!   process kill / power loss.
//! - `UNIQUE(job_id, scheduled_at_ms)` is the final idempotency backstop: a
//!   crash-retry of the same firing is a no-op.
//! - The `-wal` / `-shm` sidecars are never hand-deleted; open only checkpoints.

pub mod claim;
pub mod connection;
pub mod envelope;
pub mod error;
pub mod fire;
pub mod migration;
pub mod result;
pub mod schema;

pub use claim::{ClaimOutcome, ClaimedOccurrence, ConsumerClaim, ConsumerKind};
pub use connection::LedgerConnection;
pub use envelope::{
    DeliveryEnvelope, EnvelopeDeliveryMode, EnvelopePayloadKind, EnvelopeTarget, EnvelopeWakeMode,
    ExecutionEnvelope,
};
pub use error::LedgerError;
pub use fire::{FireAdvance, FireOutcome, OccurrenceKind, OccurrenceRow};
pub use migration::MigrationOutcome;
pub use result::{
    CancelOutcome, DEAD_LETTER_THRESHOLD, OccurrenceHistoryRow, ReportedFailureKind, RetryOutcome,
    WritebackOutcome,
};
pub use schema::{SCHEMA_DDL, SCHEMA_USER_VERSION};

/// Canonical ledger db file name. Lives in `<cron_dir>/cron/` beside
/// `scheduled_tasks.json` (same directory ⇒ same volume, so the migration's
/// backup and the ledger never straddle a mount boundary).
// Layout contract: update together with every `layout_contract` tripwire.
pub const LEDGER_DB_FILENAME: &str = "cron_ledger.db";

#[cfg(test)]
mod tests;
