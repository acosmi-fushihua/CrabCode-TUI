//! `accept_occurrence` / `report_failure_occurrence`: the durable result-writeback
//! paths that close out a claimed occurrence, plus `occurrence_history`, a
//! read-only audit accessor over the joined occurrence → attempt → result rows.
//!
//! PR5-W3-1 ships these as isolated library methods on [`LedgerConnection`],
//! mirroring [`crate::LedgerConnection::claim_next_occurrence`]: the clock
//! (`now_ms`) is injected by the caller — there is **no** internal clock — so a
//! writeback is deterministically unit-testable. Each writeback is synchronous
//! and runs in exactly one `BEGIN IMMEDIATE` transaction. The actor + reverse-IPC
//! wiring that drives these is PR5-W3-3, not this wave.
//!
//! This is the **first** writer of the `results` table and the first path to set
//! `attempts.outcome` non-NULL (the claim path leaves it NULL).
//!
//! # The fence gate (zombie-write guard)
//! A claim bumps the per-occurrence `occurrences.fence` and hands the claimer the
//! post-bump value; a writeback echoes that `claimed_fence` back. The gate **is**
//! the terminal `occurrences` UPDATE's own `WHERE` clause plus its `changes()`
//! count — one atomic compare-and-advance, no separate SELECT-then-update:
//!
//! ```sql
//! UPDATE occurrences SET status = '<terminal>'
//!  WHERE id = ? AND fence = ? AND status = 'claimed';
//! ```
//!
//! - `changes() == 1` — the writer still holds the live claim: advance the
//!   occurrence to its terminal status, resolve the lease (`attempts.outcome`),
//!   and insert the `results` row, all in the same transaction.
//! - `changes() == 0` — the writer is superseded (a re-claim bumped the fence
//!   past `claimed_fence`) OR the occurrence is already terminal (an idempotent
//!   double-writeback). Read the current fence (absent ⇒ `None`), commit the
//!   otherwise-empty transaction, and return [`WritebackOutcome::Fenced`]. The
//!   lease and `results` table are left untouched — a superseded (zombie) writer
//!   records nothing.
//!
//! # `attempts.outcome` (lease) vs `results.status` (run) are orthogonal
//! `attempts.outcome` records the LEASE disposition; its CHECK domain is
//! `accepted | abandoned | expired` — there is **no** `failed` value, so a failed
//! run `abandon`s its lease. `results.status` records the RUN disposition
//! (`success | failure | error`). A successful writeback writes
//! `outcome = 'accepted'` + `results.status = 'success'`; a failure writeback
//! writes `outcome = 'abandoned'` + `results.status = 'failure' | 'error'`. Both
//! writes always happen together inside the gated transaction.
//!
//! # PR5-W3-2 (audit sweep / dead-letter / retry / cancel)
//! Four further methods extend the same isolated-library, injected-clock,
//! single-`BEGIN IMMEDIATE`, synchronous shape:
//! - [`LedgerConnection::expire_superseded_attempts`] — an audit-only sweep that
//!   marks every still-unresolved lease whose occurrence has already advanced to
//!   a higher fence (`attempts.fence < occurrences.fence`) as `expired`. It
//!   touches **no** occurrence status and no lifecycle: a superseded lease's
//!   occurrence was already re-claimed (W2) / retried / cancelled at a higher
//!   fence. It is idempotent (the `outcome IS NULL` guard skips already-resolved
//!   leases) and returns the count marked.
//! - [`LedgerConnection::consecutive_failure_count`] / [`DEAD_LETTER_THRESHOLD`]
//!   / [`LedgerConnection::is_dead_lettered`] — a read-only derivation over
//!   occurrence history (the leading run of `failed`, reset to zero by a
//!   `completed`). The ledger stores **no** counter — the JSON job state is the
//!   frozen daemon's (W1c-B boundary) — so the dead-letter signal is computed,
//!   not persisted, and is an audit input for PR-6, **not** enforcement (a
//!   dead-lettered job keeps firing new occurrences on schedule).
//! - [`LedgerConnection::retry_occurrence`] — reset a terminal `failed`
//!   occurrence back to `pending`, bumping the fence so any lingering writer from
//!   the failed attempt is fenced out; W2's claim then re-picks it.
//! - [`LedgerConnection::cancel_occurrence`] — terminate a `pending` / `claimed`
//!   occurrence (reusing the existing `skipped` status), bumping the fence and
//!   abandoning any in-flight lease so a cancelled claimer's later writeback is
//!   `Fenced`.
//!
//! The actor + reverse-IPC wiring that drives all of these is PR5-W3-3.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::connection::LedgerConnection;
use crate::error::LedgerError;

/// Outcome of a result-writeback ([`LedgerConnection::accept_occurrence`] /
/// [`LedgerConnection::report_failure_occurrence`]).
///
/// Small and `Copy` (all `i64` / `Option<i64>`): produced at most once per
/// writeback call and returned by value, never stored in bulk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackOutcome {
    /// The fence gate admitted the writer: the occurrence was advanced to its
    /// terminal status, the lease resolved, and the `results` row written.
    Recorded {
        /// The occurrence that was closed out.
        occurrence_id: i64,
        /// The fence the terminal transition + result were recorded at (the
        /// caller's `claimed_fence`).
        fence: i64,
    },
    /// The fence gate rejected the writer and **nothing** was written: either a
    /// re-claim superseded this lease (`claimed_fence` is now stale) or the
    /// occurrence is already terminal (idempotent double-writeback).
    Fenced {
        /// The occurrence the rejected writer targeted.
        occurrence_id: i64,
        /// The fence the writer presented (its `claimed_fence`).
        expected_fence: i64,
        /// The occurrence's current fence, or `None` if the occurrence row is
        /// absent. When the occurrence is already terminal at the same fence,
        /// this equals `expected_fence` — the `status = 'claimed'` guard, not a
        /// fence mismatch, is what rejected the write.
        actual_fence: Option<i64>,
    },
}

/// How a failed run is classified into `results.status`.
///
/// Orthogonal to the lease disposition: both variants `abandon` the lease
/// (`attempts.outcome = 'abandoned'`); they differ only in the `results.status`
/// they record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportedFailureKind {
    /// The run failed in an expected way → `results.status = 'failure'`.
    Failure,
    /// The run errored unexpectedly → `results.status = 'error'`.
    Error,
}

impl ReportedFailureKind {
    /// The `results.status` string this maps to.
    ///
    /// Byte-identical to the column's CHECK domain, so an insert of either
    /// variant always satisfies the constraint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// One flattened row of an occurrence's audit history: an occurrence joined to
/// (optionally) one of its attempts and that attempt's (optional) result.
///
/// Produced by [`LedgerConnection::occurrence_history`] via
/// `occurrences LEFT JOIN attempts LEFT JOIN results` — an occurrence with no
/// attempt yields a single row with every attempt/result field `None`; an
/// occurrence with attempts yields one row per attempt (each carrying its
/// result, if any). Read-only projection for PR-6's audit UI; no production
/// write path constructs it, and it has no TS consumer in this wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceHistoryRow {
    /// `occurrences.id`.
    pub occurrence_id: i64,
    /// `occurrences.scheduled_at_ms`.
    pub scheduled_at_ms: i64,
    /// `occurrences.status`.
    pub status: String,
    /// `occurrences.fence` (current).
    pub fence: i64,
    /// `occurrences.target`, or `None` for a migration-imported legacy row that
    /// carries no envelope.
    pub target: Option<String>,
    /// The joined `attempts.id`, or `None` when the occurrence has no attempt.
    pub attempt_id: Option<i64>,
    /// The joined `attempts.outcome` (`accepted` / `abandoned` / `expired`), or
    /// `None` when there is no attempt or its lease is still unresolved.
    pub outcome: Option<String>,
    /// The joined `results.status` (`success` / `failure` / `error`), or `None`
    /// when the attempt has no result row.
    pub result_status: Option<String>,
    /// The joined `results.error_message`, or `None`.
    pub error_message: Option<String>,
    /// The joined `results.duration_ms`, or `None`.
    pub duration_ms: Option<i64>,
    /// The joined `results.recorded_at_ms`, or `None`.
    pub recorded_at_ms: Option<i64>,
}

/// The consecutive-failure count at (or above) which a job is considered
/// *dead-lettered* by [`LedgerConnection::is_dead_lettered`].
///
/// **Derived, not enforced.** Crossing this threshold is an audit signal for
/// PR-6 — it neither disables the job nor stops new occurrences from firing on
/// schedule (the frozen daemon owns that). The ledger stores no failure counter;
/// the value is recomputed from occurrence history on every call.
pub const DEAD_LETTER_THRESHOLD: u32 = 5;

/// Outcome of [`LedgerConnection::retry_occurrence`].
///
/// `Retried` carries the post-bump fence; `NotRetryable` carries the occurrence's
/// current status (`None` if the row is absent) — the occurrence was not in the
/// terminal `failed` state the reset requires, so nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryOutcome {
    /// The occurrence was `failed` and has been reset to `pending` at a bumped
    /// fence, making it re-claimable by W2's `claim_next_occurrence`.
    Retried {
        /// The occurrence that was reset.
        occurrence_id: i64,
        /// The occurrence's fence **after** the retry bump.
        fence: i64,
    },
    /// The occurrence was not `failed` (or is absent): nothing was changed.
    NotRetryable {
        /// The occurrence the caller targeted.
        occurrence_id: i64,
        /// The occurrence's current status, or `None` if the row is absent.
        status: Option<String>,
    },
}

/// Outcome of [`LedgerConnection::cancel_occurrence`].
///
/// `Cancelled` carries the post-bump fence; `NotCancellable` carries the
/// occurrence's current status (`None` if the row is absent) — it was already
/// terminal, so there was nothing to cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The occurrence was `pending` / `claimed` and has been terminated
    /// (`skipped`) at a bumped fence; any in-flight lease was abandoned.
    Cancelled {
        /// The occurrence that was cancelled.
        occurrence_id: i64,
        /// The occurrence's fence **after** the cancel bump.
        fence: i64,
    },
    /// The occurrence was not `pending` / `claimed` (or is absent): nothing was
    /// changed.
    NotCancellable {
        /// The occurrence the caller targeted.
        occurrence_id: i64,
        /// The occurrence's current status, or `None` if the row is absent.
        status: Option<String>,
    },
}

impl LedgerConnection {
    /// Record a successful run of a claimed occurrence, in one `BEGIN IMMEDIATE`
    /// transaction gated by the occurrence's fence.
    ///
    /// The fence gate is the terminal UPDATE's own `WHERE` + `changes()`: only an
    /// occurrence still `claimed` at exactly `claimed_fence` is advanced (to
    /// `completed`). On a hit the lease is resolved (`attempts.outcome =
    /// 'accepted'`) and a `results` row (`status = 'success'`, `duration_ms`,
    /// `recorded_at_ms = now_ms`) is written; the call returns
    /// [`WritebackOutcome::Recorded`]. On a miss (`changes() == 0`) the writer is
    /// superseded or the occurrence is already terminal: nothing is written and
    /// [`WritebackOutcome::Fenced`] is returned. See the module docs for the full
    /// gate contract.
    ///
    /// The clock (`now_ms`) is injected — no internal clock — so callers control
    /// `recorded_at_ms` deterministically.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction, update, select, or
    /// insert fails.
    pub fn accept_occurrence(
        &mut self,
        occurrence_id: i64,
        attempt_id: i64,
        claimed_fence: i64,
        duration_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<WritebackOutcome, LedgerError> {
        self.writeback_terminal(
            occurrence_id,
            attempt_id,
            claimed_fence,
            "completed",
            "accepted",
            "success",
            None,
            duration_ms,
            now_ms,
        )
    }

    /// Record a failed run of a claimed occurrence, in one `BEGIN IMMEDIATE`
    /// transaction gated by the occurrence's fence.
    ///
    /// Identical in shape to [`Self::accept_occurrence`], except the occurrence
    /// advances to `failed`, the lease to `abandoned` (there is no `failed`
    /// lease-outcome value — see the module docs), and the `results` row records
    /// `kind.as_str()` (`failure` | `error`) with `error_message`. Returns
    /// [`WritebackOutcome::Recorded`] on a gate hit and
    /// [`WritebackOutcome::Fenced`] (writing nothing) on a miss.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction, update, select, or
    /// insert fails.
    #[allow(clippy::too_many_arguments)]
    pub fn report_failure_occurrence(
        &mut self,
        occurrence_id: i64,
        attempt_id: i64,
        claimed_fence: i64,
        kind: ReportedFailureKind,
        error_message: Option<&str>,
        duration_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<WritebackOutcome, LedgerError> {
        self.writeback_terminal(
            occurrence_id,
            attempt_id,
            claimed_fence,
            "failed",
            "abandoned",
            kind.as_str(),
            error_message,
            duration_ms,
            now_ms,
        )
    }

    /// The shared, fence-gated terminal writeback both public entry points run.
    ///
    /// One `BEGIN IMMEDIATE`. The `occurrences` UPDATE is the atomic
    /// compare-and-advance gate (`WHERE id = ? AND fence = ? AND status =
    /// 'claimed'`); its `changes()` count decides the branch. `accept` /
    /// `report_failure` differ only in the three bound state strings
    /// (`terminal_status` / `lease_outcome` / `result_status`) and
    /// `error_message`, so the gate logic lives here once and cannot drift
    /// between the success and failure paths.
    #[allow(clippy::too_many_arguments)]
    fn writeback_terminal(
        &mut self,
        occurrence_id: i64,
        attempt_id: i64,
        claimed_fence: i64,
        terminal_status: &str,
        lease_outcome: &str,
        result_status: &str,
        error_message: Option<&str>,
        duration_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<WritebackOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // The fence gate: one atomic compare-and-advance. Only an occurrence
        // still `claimed` at exactly `claimed_fence` is advanced to its terminal
        // status. `execute` returns the changed-row count (SQLite `changes()`);
        // because `id` is the PRIMARY KEY the result is 0 or 1.
        let advanced = tx.execute(
            "UPDATE occurrences SET status = ?1 \
             WHERE id = ?2 AND fence = ?3 AND status = 'claimed'",
            params![terminal_status, occurrence_id, claimed_fence],
        )?;

        if advanced == 0 {
            // Superseded (a re-claim bumped the fence past `claimed_fence`) or
            // already terminal (idempotent double-writeback). Read the current
            // fence for diagnostics (the row may be absent ⇒ None), commit the
            // otherwise-empty transaction so the IMMEDIATE write lock releases
            // promptly, and write nothing else.
            let actual_fence: Option<i64> = tx
                .query_row(
                    "SELECT fence FROM occurrences WHERE id = ?1",
                    params![occurrence_id],
                    |row| row.get(0),
                )
                .optional()?;
            tx.commit()?;
            return Ok(WritebackOutcome::Fenced {
                occurrence_id,
                expected_fence: claimed_fence,
                actual_fence,
            });
        }

        // Gate passed. Resolve the lease disposition. Scoped to this exact lease
        // (`id` + `occurrence_id` + `fence`) so it can only ever touch the row
        // the passing claim wrote.
        tx.execute(
            "UPDATE attempts SET outcome = ?1 \
             WHERE id = ?2 AND occurrence_id = ?3 AND fence = ?4",
            params![lease_outcome, attempt_id, occurrence_id, claimed_fence],
        )?;

        // Record the run disposition. `fence` is stamped with `claimed_fence`
        // (the fence the run was admitted at); `recorded_at_ms` is the injected
        // clock.
        tx.execute(
            "INSERT INTO results \
                 (attempt_id, fence, status, error_message, duration_ms, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt_id,
                claimed_fence,
                result_status,
                error_message,
                duration_ms,
                now_ms,
            ],
        )?;

        tx.commit()?;
        Ok(WritebackOutcome::Recorded {
            occurrence_id,
            fence: claimed_fence,
        })
    }

    /// Read the full audit history for `job_id`: every occurrence LEFT JOINed to
    /// its attempts and their results, ordered by `scheduled_at_ms` then
    /// `attempts.id`.
    ///
    /// One `SELECT`, never mutates. An occurrence with no attempt appears once
    /// with all attempt/result columns `None`; an occurrence with attempts
    /// appears once per attempt (each carrying that attempt's result, if
    /// written). Purpose-fit read accessor for PR-6's audit UI; not on any write
    /// path and with no TS consumer this wave.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if preparing or running the query fails.
    pub fn occurrence_history(
        &self,
        job_id: &str,
    ) -> Result<Vec<OccurrenceHistoryRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT o.id, o.scheduled_at_ms, o.status, o.fence, o.target, \
                    a.id, a.outcome, r.status, r.error_message, r.duration_ms, r.recorded_at_ms \
             FROM occurrences o \
             LEFT JOIN attempts a ON a.occurrence_id = o.id \
             LEFT JOIN results r ON r.attempt_id = a.id \
             WHERE o.job_id = ?1 \
             ORDER BY o.scheduled_at_ms, a.id",
        )?;
        let rows = stmt
            .query_map(params![job_id], |row| {
                Ok(OccurrenceHistoryRow {
                    occurrence_id: row.get(0)?,
                    scheduled_at_ms: row.get(1)?,
                    status: row.get(2)?,
                    fence: row.get(3)?,
                    target: row.get(4)?,
                    attempt_id: row.get(5)?,
                    outcome: row.get(6)?,
                    result_status: row.get(7)?,
                    error_message: row.get(8)?,
                    duration_ms: row.get(9)?,
                    recorded_at_ms: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark every still-unresolved lease its occurrence has already advanced past
    /// as `expired`, in one `BEGIN IMMEDIATE` transaction. Returns the number of
    /// leases newly marked (SQLite `changes()`).
    ///
    /// A lease whose `attempts.fence` is strictly below the occurrence's current
    /// `occurrences.fence` was provably superseded — the occurrence was re-claimed
    /// (W2) / retried / cancelled at a higher fence — so its lease can never again
    /// pass the W3-1 write gate and is `expired`. **Audit-only:** this touches no
    /// `occurrences.status` and no lifecycle; the occurrence's own state was
    /// already advanced by whatever superseded it. The `outcome IS NULL` guard
    /// skips any already-resolved lease (`accepted` / `abandoned` / `expired`), so
    /// the sweep is idempotent — a second run with no new supersessions marks
    /// nothing.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction or update fails.
    pub fn expire_superseded_attempts(&mut self) -> Result<usize, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Correlated predicate: an attempt is superseded exactly when its own
        // fence is below the fence its occurrence now advertises. `occurrence_id`
        // is a FK to a never-deleted `occurrences` row, so the subquery always
        // resolves to exactly one value.
        let expired = tx.execute(
            "UPDATE attempts SET outcome = 'expired' \
             WHERE outcome IS NULL \
               AND fence < (SELECT fence FROM occurrences WHERE id = attempts.occurrence_id)",
            [],
        )?;
        tx.commit()?;
        Ok(expired)
    }

    /// The number of most-recent consecutive `failed` occurrences for `job_id`,
    /// counting back from the newest terminal occurrence; a `completed` resets the
    /// run to zero. Read-only.
    ///
    /// Derived from occurrence history, never stored: the SELECT walks the
    /// `completed` / `failed` occurrences newest-`scheduled_at_ms`-first and Rust
    /// counts the leading run of `failed`, stopping at the first `completed`.
    /// Non-terminal statuses (`pending` / `claimed` / `skipped` / `needs_adoption`)
    /// are excluded from the SELECT, so they never interrupt or contribute to the
    /// run. The ledger holds no failure counter because the authoritative JSON job
    /// state belongs to the frozen daemon (W1c-B boundary).
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if preparing or running the query fails.
    pub fn consecutive_failure_count(&self, job_id: &str) -> Result<u32, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT status FROM occurrences \
             WHERE job_id = ?1 AND status IN ('completed','failed') \
             ORDER BY scheduled_at_ms DESC",
        )?;
        let mut rows = stmt.query(params![job_id])?;
        let mut run: u32 = 0;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            if status == "failed" {
                run += 1;
            } else {
                // The first `completed` (walking newest-first) ends the run.
                break;
            }
        }
        Ok(run)
    }

    /// Whether `job_id` is *dead-lettered*: its consecutive-failure run is at or
    /// above [`DEAD_LETTER_THRESHOLD`].
    ///
    /// A **derived audit signal only** (see [`Self::consecutive_failure_count`] /
    /// [`DEAD_LETTER_THRESHOLD`]): it is not a stored status and enforces nothing —
    /// a dead-lettered job keeps firing new occurrences on schedule; the daemon
    /// owns that. Consumed by PR-6's audit surface.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the underlying count query fails.
    pub fn is_dead_lettered(&self, job_id: &str) -> Result<bool, LedgerError> {
        Ok(self.consecutive_failure_count(job_id)? >= DEAD_LETTER_THRESHOLD)
    }

    /// Reset a terminal `failed` occurrence back to re-claimable `pending`,
    /// bumping its fence, in one `BEGIN IMMEDIATE` transaction.
    ///
    /// The single gated UPDATE (`WHERE id = ? AND status = 'failed'`) is the
    /// admission test. On a hit (`changes() == 1`) the occurrence flips to
    /// `pending` and `fence` advances by one — the bump fences out any lingering
    /// writer from the failed attempt (a stale-fence writeback hits W3-1's gate →
    /// `Fenced`) — and W2's `claim_next_occurrence` will re-pick it; the post-bump
    /// fence is returned in [`RetryOutcome::Retried`]. On a miss (`changes() == 0`
    /// — the occurrence is not `failed`, e.g. `pending` / `claimed` / `completed` /
    /// `skipped`, or absent) nothing changes and [`RetryOutcome::NotRetryable`]
    /// carries the current status (`None` if absent).
    ///
    /// `_now_ms` is accepted for signature symmetry with the other writeback paths
    /// (and possible future audit); the reset writes no timestamp, so it is unused
    /// this wave.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction, update, or status read
    /// fails.
    pub fn retry_occurrence(
        &mut self,
        occurrence_id: i64,
        _now_ms: i64,
    ) -> Result<RetryOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let advanced = tx.execute(
            "UPDATE occurrences SET status = 'pending', fence = fence + 1 \
             WHERE id = ?1 AND status = 'failed'",
            params![occurrence_id],
        )?;
        if advanced == 0 {
            // Not `failed` (or absent): read the current status for the caller and
            // commit the otherwise-empty transaction so the write lock releases.
            let status: Option<String> = tx
                .query_row(
                    "SELECT status FROM occurrences WHERE id = ?1",
                    params![occurrence_id],
                    |row| row.get(0),
                )
                .optional()?;
            tx.commit()?;
            return Ok(RetryOutcome::NotRetryable {
                occurrence_id,
                status,
            });
        }
        let fence: i64 = tx.query_row(
            "SELECT fence FROM occurrences WHERE id = ?1",
            params![occurrence_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(RetryOutcome::Retried {
            occurrence_id,
            fence,
        })
    }

    /// Terminate a `pending` / `claimed` occurrence as `skipped`, bumping its
    /// fence and abandoning any in-flight lease, in one `BEGIN IMMEDIATE`
    /// transaction.
    ///
    /// The first gated UPDATE (`WHERE id = ? AND status IN ('pending','claimed')`)
    /// is the admission test. On a hit (`changes() == 1`) the occurrence flips to
    /// `skipped` — reusing the existing status, since a cancel stays distinguishable
    /// in audit from a fire-time missed-skip (a cancel has `occurrence_kind IN
    /// ('scheduled','manual')` with an `abandoned` attempt, whereas a missed-skip
    /// has `occurrence_kind = 'missed_skipped'` with zero attempts) — `fence`
    /// advances by one, and a second UPDATE abandons any still-unresolved lease
    /// (`attempts.outcome = 'abandoned' WHERE outcome IS NULL`). The fence bump
    /// makes an in-flight consumer's later accept/report_failure hit W3-1's gate →
    /// `Fenced`. The post-bump fence is returned in [`CancelOutcome::Cancelled`].
    /// On a miss (`changes() == 0` — the occurrence is already terminal
    /// (`completed` / `failed` / `skipped` / `needs_adoption`) or absent) nothing
    /// changes and [`CancelOutcome::NotCancellable`] carries the current status.
    ///
    /// `_now_ms` is accepted for signature symmetry / future audit; the cancel
    /// writes no timestamp, so it is unused this wave.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction, either update, or the
    /// status read fails.
    pub fn cancel_occurrence(
        &mut self,
        occurrence_id: i64,
        _now_ms: i64,
    ) -> Result<CancelOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cancelled = tx.execute(
            "UPDATE occurrences SET status = 'skipped', fence = fence + 1 \
             WHERE id = ?1 AND status IN ('pending','claimed')",
            params![occurrence_id],
        )?;
        if cancelled == 0 {
            // Already terminal (or absent): read the current status and commit the
            // otherwise-empty transaction so the write lock releases.
            let status: Option<String> = tx
                .query_row(
                    "SELECT status FROM occurrences WHERE id = ?1",
                    params![occurrence_id],
                    |row| row.get(0),
                )
                .optional()?;
            tx.commit()?;
            return Ok(CancelOutcome::NotCancellable {
                occurrence_id,
                status,
            });
        }
        // Abandon any in-flight (unresolved) lease so a cancelled claimer's later
        // writeback finds its lease gone and its fence superseded.
        tx.execute(
            "UPDATE attempts SET outcome = 'abandoned' \
             WHERE occurrence_id = ?1 AND outcome IS NULL",
            params![occurrence_id],
        )?;
        let fence: i64 = tx.query_row(
            "SELECT fence FROM occurrences WHERE id = ?1",
            params![occurrence_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(CancelOutcome::Cancelled {
            occurrence_id,
            fence,
        })
    }
}
