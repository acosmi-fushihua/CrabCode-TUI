//! `fire_occurrence`: the single durable write path that records one logical
//! firing of a cron job and (atomically) advances the owning job row.
//!
//! W1a ships this as an isolated library method on [`LedgerConnection`]; it is
//! **not** yet wired into the scheduler daemon (that is W1c). It is synchronous;
//! W1c wraps it in `spawn_blocking`.

use rusqlite::{TransactionBehavior, params};

use crate::connection::LedgerConnection;
use crate::envelope::ExecutionEnvelope;
use crate::error::LedgerError;

/// Why an occurrence fired. Maps 1:1 to the `occurrences.occurrence_kind` CHECK
/// domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccurrenceKind {
    /// A schedule matched at its planned time.
    Scheduled,
    /// An operator/manual trigger, out of band with the schedule.
    Manual,
    /// A missed firing replayed within the catch-up window.
    MissedCatchup,
    /// A missed firing too old to replay — recorded for audit only.
    MissedSkipped,
}

impl OccurrenceKind {
    /// The `occurrences.occurrence_kind` string this maps to.
    fn as_kind_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::MissedCatchup => "missed_catchup",
            Self::MissedSkipped => "missed_skipped",
        }
    }

    /// The initial `occurrences.status`. A skipped firing is terminal on
    /// arrival; every other kind starts `pending` for a consumer to claim.
    fn initial_status(self) -> &'static str {
        match self {
            Self::MissedSkipped => "skipped",
            Self::Scheduled | Self::Manual | Self::MissedCatchup => "pending",
        }
    }
}

/// How to advance the owning `jobs` row once an occurrence is durably inserted.
///
/// Exactly one arm runs, and only when the occurrence is *newly* inserted — an
/// idempotent re-fire advances nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireAdvance {
    /// Recurring job: replace its stored JSON (e.g. bumped
    /// `state.last_run_at_ms` / `next_run_at_ms`).
    Recurring {
        /// The full, serialized `CronJob` to persist.
        new_job_json: String,
    },
    /// One-shot job: delete the `jobs` row. Its occurrence is retained
    /// (`occurrences.job_id` intentionally has no FK).
    Delete,
    /// Leave the `jobs` row untouched. Manual-trigger semantics: a manual fire
    /// neither advances schedule state nor deletes a one-shot.
    NoAdvance,
}

/// Outcome of [`LedgerConnection::fire_occurrence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireOutcome {
    /// The occurrence was newly inserted and the advance (if any) applied.
    Inserted,
    /// An occurrence with the same `(job_id, scheduled_at_ms)` already existed:
    /// idempotent no-op, no advance applied.
    AlreadyRecorded,
}

impl LedgerConnection {
    /// Durably record one firing of `job_id` at `scheduled_at_ms` and advance
    /// the owning job row, all in one `BEGIN IMMEDIATE` transaction.
    ///
    /// Idempotency: `(job_id, scheduled_at_ms)` is `UNIQUE` — the same logical
    /// identity as `acosmi_scheduler::outbox::occurrence_id`. A second call with
    /// the same pair inserts nothing, applies no `advance`, and returns
    /// [`FireOutcome::AlreadyRecorded`]. This is the final anti-duplicate
    /// backstop.
    ///
    /// `fence` is initialized to `0` (aligned with the delivery `generation`
    /// baseline). On any error the transaction is rolled back — rusqlite rolls
    /// back a `Transaction` on drop.
    ///
    /// The immutable per-fire [`ExecutionEnvelope`] is captured on the row:
    /// `envelope.target.as_str()` into `occurrences.target` (denormalized for
    /// the claim index) and the serialized envelope into `occurrences.envelope_json`.
    /// Because the insert is `ON CONFLICT DO NOTHING`, the first fire's envelope
    /// is immutable — a duplicate re-fire never rewrites it.
    ///
    /// # Errors
    /// Returns [`LedgerError::Json`] if the envelope fails to serialize, or
    /// [`LedgerError::Sqlite`] if the transaction, insert, or advance statement
    /// fails.
    #[allow(clippy::too_many_arguments)]
    pub fn fire_occurrence(
        &mut self,
        job_id: &str,
        scheduled_at_ms: i64,
        kind: OccurrenceKind,
        emitted_at_ms: i64,
        payload_message: Option<&str>,
        job_name: Option<&str>,
        legacy_event_id: Option<i64>,
        envelope: &ExecutionEnvelope,
        advance: FireAdvance,
    ) -> Result<FireOutcome, LedgerError> {
        // Serialize before opening the write transaction so a serde failure
        // aborts cheaply. A STRICT table can't enforce the envelope's shape, so
        // it is validated in Rust and stored as opaque JSON text.
        let envelope_json = serde_json::to_string(envelope)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // `ON CONFLICT DO NOTHING` makes the UNIQUE(job_id, scheduled_at_ms)
        // collision a no-op; `execute` then reports 0 rows changed. On a
        // duplicate the first fire's `target` / `envelope_json` are left intact.
        let inserted = tx.execute(
            "INSERT INTO occurrences (
                 job_id, scheduled_at_ms, occurrence_kind, status, fence,
                 emitted_at_ms, payload_message, job_name, legacy_event_id,
                 target, envelope_json
             ) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(job_id, scheduled_at_ms) DO NOTHING",
            params![
                job_id,
                scheduled_at_ms,
                kind.as_kind_str(),
                kind.initial_status(),
                emitted_at_ms,
                payload_message,
                job_name,
                legacy_event_id,
                envelope.target.as_str(),
                envelope_json,
            ],
        )?;

        if inserted == 0 {
            // Duplicate firing: commit the (empty) transaction and skip the
            // advance — re-applying it would be a spurious second schedule bump
            // or delete.
            tx.commit()?;
            return Ok(FireOutcome::AlreadyRecorded);
        }

        // Newly inserted: apply exactly one advance in the same transaction.
        match advance {
            FireAdvance::Recurring { new_job_json } => {
                tx.execute(
                    "UPDATE jobs SET job_json = ?1, updated_at_ms = ?2 WHERE id = ?3",
                    params![new_job_json, emitted_at_ms, job_id],
                )?;
            }
            FireAdvance::Delete => {
                tx.execute("DELETE FROM jobs WHERE id = ?1", params![job_id])?;
            }
            FireAdvance::NoAdvance => {}
        }

        tx.commit()?;
        Ok(FireOutcome::Inserted)
    }
}

/// A minimal read-only projection of one `occurrences` row.
///
/// Exposed so out-of-crate consumers — the `crabcode-cron` fire-path actor unit
/// test and the lifecycle smoke — can assert an occurrence was durably recorded
/// without reaching into the `pub(crate)` [`LedgerConnection`] handle. Read-only:
/// no production write path constructs or consumes it. In-crate tests read the
/// `pub(crate) conn` directly and do not need this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceRow {
    /// The firing's `job_id` (half of the `UNIQUE(job_id, scheduled_at_ms)` key).
    pub job_id: String,
    /// Theoretical due instant this occurrence is keyed by (epoch ms), the other
    /// half of the unique identity.
    pub scheduled_at_ms: i64,
    /// `occurrence_kind` string (`scheduled` / `manual` / `missed_catchup` /
    /// `missed_skipped`).
    pub occurrence_kind: String,
    /// The outbox `event_id` this occurrence shares its derived identity with,
    /// or `None` for a row imported without a legacy event.
    pub legacy_event_id: Option<i64>,
    /// The `occurrences.target` column: the [`crate::EnvelopeTarget::as_str`]
    /// value (`main` / `isolated` / `continuation`) captured at fire time, or
    /// `None` for a migration-imported legacy row that carries no envelope.
    pub target: Option<String>,
    /// The serialized [`ExecutionEnvelope`] JSON captured at fire time, or
    /// `None` for a migration-imported legacy row.
    pub envelope_json: Option<String>,
}

impl LedgerConnection {
    /// Read every `occurrences` row for `job_id`, ascending by `scheduled_at_ms`.
    ///
    /// A purpose-fit read accessor for out-of-crate tests (see [`OccurrenceRow`]);
    /// it issues one `SELECT` and never mutates. Not on any production path.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if preparing or running the query fails.
    pub fn occurrences_for_job(&self, job_id: &str) -> Result<Vec<OccurrenceRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT job_id, scheduled_at_ms, occurrence_kind, legacy_event_id, target, envelope_json
             FROM occurrences WHERE job_id = ?1 ORDER BY scheduled_at_ms",
        )?;
        let rows = stmt
            .query_map(params![job_id], |row| {
                Ok(OccurrenceRow {
                    job_id: row.get(0)?,
                    scheduled_at_ms: row.get(1)?,
                    occurrence_kind: row.get(2)?,
                    legacy_event_id: row.get(3)?,
                    target: row.get(4)?,
                    envelope_json: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
