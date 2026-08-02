//! `claim_next_occurrence`: the single durable claim/lease path that hands the
//! next runnable occurrence to a consumer and records a `attempts` lease over it.
//!
//! PR5-W2-2 ships this as an isolated library method on [`LedgerConnection`],
//! mirroring [`crate::LedgerConnection::fire_occurrence`]: the clock (`now_ms`),
//! lease length (`lease_ms`), and `lease_token` are all injected by the caller —
//! there is **no** internal clock or RNG, so the claim is deterministically
//! unit-testable. It is synchronous; the actor + reverse-IPC wiring is PR5-W2-3.
//!
//! # The claim, in one `BEGIN IMMEDIATE`
//! 1. Pick the oldest claimable occurrence for the consumer's
//!    [`EnvelopeTarget`] — envelope-bearing (so W1b legacy rows are excluded),
//!    and either still `pending` or `claimed` with **no live lease** at its
//!    current fence (the expired-lease re-claim). `needs_adoption` is neither
//!    `pending` nor `claimed`, so it drops out without a special case.
//! 2. No row ⇒ [`ClaimOutcome::NoWork`] (the empty transaction still commits so
//!    the `IMMEDIATE` write lock is released promptly).
//! 3. Bump the occurrence-scoped `fence` (`fence + 1`) and flip status to
//!    `claimed` in the same transaction, then read the post-bump fence back.
//! 4. Insert the durable `attempts` lease (`outcome` left NULL — PR5-W2-3/W3
//!    resolve it) at the new fence.
//! 5. Commit and return the claimed occurrence, its fresh lease, and the
//!    immutable [`ExecutionEnvelope`] re-parsed from the stored JSON.
//!
//! # Fence = per-occurrence, monotonic
//! The fence is `occurrences.fence`, bumped once per claim of that occurrence.
//! It is **not** a global counter and needs no new table: the zombie-write gate
//! W3 enforces is occurrence-scoped (a superseded lease carries a lower fence
//! than the occurrence now holds, so its late result write is rejected).

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::connection::LedgerConnection;
use crate::envelope::{EnvelopeTarget, ExecutionEnvelope};
use crate::error::LedgerError;

/// Which consumer surface is claiming an occurrence.
///
/// Maps 1:1 to the TUI-only `attempts.consumer_kind` CHECK domain
/// via [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerKind {
    /// The interactive terminal REPL.
    Tui,
}

impl ConsumerKind {
    /// The `attempts.consumer_kind` string this maps to.
    ///
    /// Byte-identical to the column's CHECK domain, so an insert of any variant
    /// always satisfies the constraint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
        }
    }
}

/// A consumer's identity plus the routing target it serves, supplied to
/// [`LedgerConnection::claim_next_occurrence`].
///
/// Only occurrences whose `occurrences.target` equals `target.as_str()` are
/// claimable by this consumer — a `Main` consumer never claims an `Isolated`
/// occurrence and vice versa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerClaim {
    /// Consumer surface (→ `attempts.consumer_kind`).
    pub kind: ConsumerKind,
    /// Stable consumer id (→ `attempts.consumer_id`).
    pub id: String,
    /// Routing target this consumer serves (matched against `occurrences.target`).
    pub target: EnvelopeTarget,
}

/// A durably-claimed occurrence together with the fresh lease minted over it.
///
/// Returned inside [`ClaimOutcome::Claimed`]. `fence` and `lease_expires_at_ms`
/// are the post-claim values a consumer echoes back when it later writes a
/// result (PR5-W2-3/W3): the fence is the occurrence's current fence and gates
/// stale writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedOccurrence {
    /// The claimed `occurrences.id`.
    pub occurrence_id: i64,
    /// The freshly inserted `attempts.id` (this lease's row).
    pub attempt_id: i64,
    /// The occurrence's `job_id`.
    pub job_id: String,
    /// The occurrence's `scheduled_at_ms`.
    pub scheduled_at_ms: i64,
    /// The occurrence's fence **after** this claim's bump (the value the lease
    /// was written at; the W3 result-write gate checks a writer against it).
    pub fence: i64,
    /// The lease token supplied by the caller and persisted on the lease.
    pub lease_token: String,
    /// When this lease expires (`now_ms + lease_ms`).
    pub lease_expires_at_ms: i64,
    /// The immutable per-fire execution projection, re-parsed from
    /// `occurrences.envelope_json`.
    pub envelope: ExecutionEnvelope,
}

/// Outcome of [`LedgerConnection::claim_next_occurrence`].
///
/// `Claimed` carries the (large) [`ClaimedOccurrence`] by value while `NoWork`
/// is a unit — the size skew is deliberate. This enum is a transient, by-value
/// return produced at most once per claim call and never stored in bulk, so the
/// flat shape is the ergonomic contract PR5-W2-3 consumes; boxing would add
/// heap indirection for no real memory saving.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// An occurrence was claimed; carries the lease + immutable envelope.
    Claimed(ClaimedOccurrence),
    /// No claimable occurrence exists for this consumer's target right now.
    NoWork,
}

impl LedgerConnection {
    /// Claim the next runnable occurrence for `claim.target`, minting a durable
    /// lease over it, all in one `BEGIN IMMEDIATE` transaction.
    ///
    /// Selection is oldest-`scheduled_at_ms`-first among rows that (a) target
    /// `claim.target`, (b) carry an envelope (`envelope_json IS NOT NULL`, which
    /// excludes W1b migration-imported legacy rows), and (c) are either `pending`
    /// or `claimed` with no live lease at the occurrence's current fence. On a
    /// match the occurrence's `fence` is bumped (`fence + 1`, occurrence-scoped
    /// and monotonic), its status flipped to `claimed`, and a `attempts` lease is
    /// inserted at the new fence with `outcome` NULL (PR5-W2-3/W3 resolve it).
    ///
    /// The clock (`now_ms`), lease length (`lease_ms`), and `lease_token` are
    /// injected — no internal clock — so callers control expiry deterministically.
    /// `lease_expires_at_ms` is `now_ms + lease_ms`.
    ///
    /// Returns [`ClaimOutcome::NoWork`] when nothing is claimable (the empty
    /// transaction is still committed to release the write lock).
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the transaction, select, update, or
    /// insert fails, or [`LedgerError::Json`] if the stored envelope JSON fails
    /// to deserialize (deserialization runs before commit, so a malformed
    /// envelope rolls the claim back rather than leaving a claimed-but-unusable
    /// occurrence).
    pub fn claim_next_occurrence(
        &mut self,
        claim: &ConsumerClaim,
        now_ms: i64,
        lease_ms: i64,
        lease_token: &str,
    ) -> Result<ClaimOutcome, LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Step 1 — pick the next claimable occurrence for this target, oldest
        // `scheduled_at_ms` first. Claimable = envelope-bearing (so W1b legacy
        // rows with a NULL envelope are excluded) AND either still `pending`, or
        // `claimed` with NO live lease at the occurrence's current fence (the
        // expired-lease re-claim). `needs_adoption` matches neither status arm,
        // so it is excluded with no special case. Column 3 (`o.fence`, pre-bump)
        // is selected for parity with the blueprint but not read here — step 3
        // bumps then re-reads the authoritative post-bump fence.
        let selected = tx
            .query_row(
                "SELECT o.id, o.job_id, o.scheduled_at_ms, o.fence, o.envelope_json
                 FROM occurrences o
                 WHERE o.target = ?1
                   AND o.envelope_json IS NOT NULL
                   AND (
                         o.status = 'pending'
                      OR (o.status = 'claimed'
                          AND NOT EXISTS (
                              SELECT 1 FROM attempts a
                              WHERE a.occurrence_id = o.id
                                AND a.fence = o.fence
                                AND a.lease_expires_at_ms > ?2))
                       )
                 ORDER BY o.scheduled_at_ms
                 LIMIT 1",
                params![claim.target.as_str(), now_ms],
                |row| {
                    let occurrence_id: i64 = row.get(0)?;
                    let job_id: String = row.get(1)?;
                    let scheduled_at_ms: i64 = row.get(2)?;
                    let envelope_json: String = row.get(4)?;
                    Ok((occurrence_id, job_id, scheduled_at_ms, envelope_json))
                },
            )
            .optional()?;

        let (occurrence_id, job_id, scheduled_at_ms, envelope_json) = match selected {
            Some(row) => row,
            None => {
                // Nothing claimable: commit the empty (read-only) transaction so
                // the `IMMEDIATE` write lock is released immediately.
                tx.commit()?;
                return Ok(ClaimOutcome::NoWork);
            }
        };

        // Step 3 — bump the per-occurrence fence and flip status to `claimed` in
        // the same transaction, then read the post-bump fence back. This is the
        // same-tx monotonic-fence invariant: the lease is written at exactly the
        // fence the occurrence now advertises.
        tx.execute(
            "UPDATE occurrences SET status = 'claimed', fence = fence + 1 WHERE id = ?1",
            params![occurrence_id],
        )?;
        let new_fence: i64 = tx.query_row(
            "SELECT fence FROM occurrences WHERE id = ?1",
            params![occurrence_id],
            |row| row.get(0),
        )?;

        // Step 4 — insert the durable lease at the new fence. `outcome` stays
        // NULL; PR5-W2-3/W3 fill it (accepted / abandoned / expired).
        let lease_expires_at_ms = now_ms + lease_ms;
        tx.execute(
            "INSERT INTO attempts (
                 occurrence_id, fence, consumer_kind, consumer_id,
                 leased_at_ms, lease_expires_at_ms, lease_token, outcome
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                occurrence_id,
                new_fence,
                claim.kind.as_str(),
                claim.id,
                now_ms,
                lease_expires_at_ms,
                lease_token,
            ],
        )?;
        let attempt_id = tx.last_insert_rowid();

        // Re-parse the immutable envelope for the return BEFORE committing, so a
        // (defensive) deserialize failure rolls the whole claim back rather than
        // leaving a claimed occurrence no consumer can execute.
        let envelope: ExecutionEnvelope = serde_json::from_str(&envelope_json)?;

        tx.commit()?;

        Ok(ClaimOutcome::Claimed(ClaimedOccurrence {
            occurrence_id,
            attempt_id,
            job_id,
            scheduled_at_ms,
            fence: new_fence,
            lease_token: lease_token.to_string(),
            lease_expires_at_ms,
            envelope,
        }))
    }
}
