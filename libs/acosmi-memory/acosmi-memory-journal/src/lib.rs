//! Durable, fenced handoff journal for Memory runner triggers and reverse RPC.
//!
//! The journal deliberately promises **at-least-once delivery plus fenced,
//! idempotent state transitions**. It does not claim that an arbitrary external
//! side effect is exactly-once: a consumer can crash after performing a file or
//! network write and before [`Journal::mark_settled`] commits. Settlement
//! consumers must therefore make their sink idempotent by [`WorkItem::key`] (or
//! make the sink update part of the same SQLite transaction).
//!
//! This is a journal-API property, not an end-to-end worker replay guarantee.
//! A runtime that claims only caller-supplied keys must separately enumerate
//! recoverable rows and reconstruct every execution context required to replay
//! them; this crate does not manufacture that context.
//!
//! # State machine
//!
//! ```text
//! enqueue
//!    │
//!    ▼
//! Pending ── claim_delivery ──► Leased ── ack_delivery ──► Acked
//!    ▲                              │                         │
//!    └──── release / lease expiry ──┴─────────────────────────┘
//!                                   │ record_result (fenced)
//!                                   ▼
//!                              ResultReady
//!                                   │ claim_settlement
//!                                   ▼
//!                                Settling
//!                                   │ mark_settled
//!                                   ▼
//!                                Settled
//! ```
//!
//! A delivery or settlement claim increments `delivery_epoch`. Every ack,
//! result, release, and settle must present the exact `(owner, epoch)` fence.
//! Once an expired lease is reclaimed, messages from the former owner are
//! rejected as stale. `ResultReady` and expired `Settling` rows are the durable
//! crash-recovery surface.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Launcher-to-orchestrator contract. The launcher must resolve this path from
/// the canonical state root; the journal never guesses from a project path or
/// an implicit home/config directory.
pub const JOURNAL_PATH_ENV: &str = "CRABCODE_MEMORY_JOURNAL_PATH";

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS work_items (
    key                    TEXT PRIMARY KEY NOT NULL,
    kind                   TEXT NOT NULL
                           CHECK (kind IN ('runner_trigger', 'reverse_request')),
    payload_json           TEXT NOT NULL,
    payload_sha256         TEXT NOT NULL,
    state                  TEXT NOT NULL
                           CHECK (state IN (
                               'pending', 'leased', 'acked', 'result_ready',
                               'settling', 'settled', 'dead_letter'
                           )),
    delivery_epoch         INTEGER NOT NULL DEFAULT 0
                           CHECK (delivery_epoch >= 0),
    attempts               INTEGER NOT NULL DEFAULT 0
                           CHECK (attempts >= 0),
    next_attempt_at_ms     INTEGER NOT NULL DEFAULT 0
                           CHECK (next_attempt_at_ms >= 0),
    lease_owner            TEXT,
    lease_expires_at_ms    INTEGER,
    result_key             TEXT,
    result_json            TEXT,
    result_sha256          TEXT,
    result_recorded_at_ms  INTEGER,
    created_at_ms          INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms          INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    settled_at_ms          INTEGER,
    last_error             TEXT,
    CHECK (
        (state IN ('leased', 'acked', 'settling')
            AND lease_owner IS NOT NULL
            AND lease_expires_at_ms IS NOT NULL)
        OR
        (state NOT IN ('leased', 'acked', 'settling')
            AND lease_owner IS NULL
            AND lease_expires_at_ms IS NULL)
    ),
    CHECK (
        (result_key IS NULL
            AND result_json IS NULL
            AND result_sha256 IS NULL
            AND result_recorded_at_ms IS NULL)
        OR
        (result_key IS NOT NULL
            AND result_json IS NOT NULL
            AND result_sha256 IS NOT NULL
            AND result_recorded_at_ms IS NOT NULL)
    ),
    CHECK (
        state NOT IN ('result_ready', 'settling', 'settled')
        OR result_key IS NOT NULL
    ),
    CHECK (
        (state = 'settled' AND settled_at_ms IS NOT NULL)
        OR
        (state != 'settled' AND settled_at_ms IS NULL)
    )
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_work_result_key
    ON work_items(result_key)
    WHERE result_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_memory_work_delivery
    ON work_items(kind, state, next_attempt_at_ms, lease_expires_at_ms, created_at_ms);

CREATE INDEX IF NOT EXISTS idx_memory_work_settlement
    ON work_items(state, lease_expires_at_ms, result_recorded_at_ms);
"#;

/// Stable class of durable work. Reverse-IPC subtypes live inside the payload
/// so adding a model operation does not require a database migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkKind {
    RunnerTrigger,
    ReverseRequest,
}

impl WorkKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RunnerTrigger => "runner_trigger",
            Self::ReverseRequest => "reverse_request",
        }
    }

    fn parse(raw: &str) -> Result<Self, JournalError> {
        match raw {
            "runner_trigger" => Ok(Self::RunnerTrigger),
            "reverse_request" => Ok(Self::ReverseRequest),
            other => Err(JournalError::CorruptState(format!(
                "unknown work kind {other:?}"
            ))),
        }
    }
}

/// Durable lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkState {
    Pending,
    Leased,
    Acked,
    ResultReady,
    Settling,
    Settled,
    DeadLetter,
}

impl WorkState {
    fn parse(raw: &str) -> Result<Self, JournalError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "leased" => Ok(Self::Leased),
            "acked" => Ok(Self::Acked),
            "result_ready" => Ok(Self::ResultReady),
            "settling" => Ok(Self::Settling),
            "settled" => Ok(Self::Settled),
            "dead_letter" => Ok(Self::DeadLetter),
            other => Err(JournalError::CorruptState(format!(
                "unknown work state {other:?}"
            ))),
        }
    }
}

/// One durable work row.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkItem {
    pub key: String,
    pub kind: WorkKind,
    pub payload: Value,
    pub state: WorkState,
    pub delivery_epoch: u64,
    pub attempts: u64,
    pub next_attempt_at_ms: u64,
    pub lease_owner: Option<String>,
    pub lease_expires_at_ms: Option<u64>,
    pub result_key: Option<String>,
    pub result: Option<Value>,
    pub result_recorded_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub settled_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// Fence returned by a claim. It is intentionally small enough to put on the
/// runner/result wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryFence {
    pub owner: String,
    pub epoch: u64,
}

impl DeliveryFence {
    #[must_use]
    pub fn new(owner: impl Into<String>, epoch: u64) -> Self {
        Self {
            owner: owner.into(),
            epoch,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Inserted,
    Existing { state: WorkState },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AckOutcome {
    Acked,
    AlreadyAcked,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenewOutcome {
    Renewed,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordResultOutcome {
    Recorded,
    Duplicate,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    Released,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleOutcome {
    Settled,
    AlreadySettled,
    Missing,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeadLetterOutcome {
    DeadLettered,
    AlreadyDeadLettered,
    Missing,
    Stale,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("memory journal path environment variable {JOURNAL_PATH_ENV} is missing or empty")]
    MissingJournalPath,
    #[error("memory journal path must be absolute: {0}")]
    NonAbsolutePath(PathBuf),
    #[error("refusing to open memory journal through a symlink: {0}")]
    SymlinkPath(PathBuf),
    #[error("memory journal path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("memory journal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("memory journal SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("memory journal JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "memory journal schema version {found} is unsupported; this build supports version {supported}"
    )]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error(
        "refusing to initialize an unversioned non-empty database; found user schema object {object:?}"
    )]
    UnversionedNonEmptyDatabase { object: String },
    #[error("memory journal schema does not match version {version}: {reason}")]
    SchemaMismatch { version: i64, reason: String },
    #[error("memory journal timestamp/epoch does not fit SQLite INTEGER: {0}")]
    IntegerOverflow(u64),
    #[error("memory journal idempotency conflict for key {key:?}: {reason}")]
    IdempotencyConflict { key: String, reason: String },
    #[error("memory journal contains an invalid row: {0}")]
    CorruptState(String),
}

/// Path-backed journal handle. Connections are intentionally short-lived:
/// each operation opens its own WAL connection, making the handle cheap to
/// clone and safe to share across async tasks/processes without a global mutex.
#[derive(Clone, Debug)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Open and initialize the journal at an explicit absolute path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(JournalError::NonAbsolutePath(path.to_path_buf()));
        }
        reject_symlink(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| JournalError::MissingParent(path.to_path_buf()))?;
        fs::create_dir_all(parent)?;
        let canonical_parent = canonicalize_for_sqlite(parent)?;
        let file_name = path
            .file_name()
            .ok_or_else(|| JournalError::MissingParent(path.to_path_buf()))?;
        let canonical_path = canonical_parent.join(file_name);

        // Version/schema validation deliberately happens before switching an
        // existing database into WAL mode. Unsupported or forged databases
        // are rejected without a schema migration or journal-mode rewrite.
        let mut conn = open_base_connection(&canonical_path)?;
        let schema_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match schema_version {
            SCHEMA_VERSION => {
                // A version number is not a schema witness. Do not use
                // CREATE ... IF NOT EXISTS to silently "repair" a forged or
                // partially copied v1 database: validate the complete table,
                // constraints, STRICT marker, indexes, and object set first.
                validate_schema(&conn)?;
            }
            0 => {
                if let Some(object) = first_user_schema_object(&conn)? {
                    return Err(JournalError::UnversionedNonEmptyDatabase { object });
                }
                // Publish schema + version atomically. A failed DDL statement
                // must leave an empty v0 database, never a partial v1.
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
                validate_schema(&conn)?;
            }
            found => {
                return Err(JournalError::UnsupportedSchemaVersion {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
        }
        // Close the preflight connection before changing journal mode. This
        // guarantees every schema-inspection statement/read transaction is
        // finalized before the durable WAL connection is established.
        drop(conn);
        let conn = open_connection(&canonical_path)?;
        let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        secure_file(&canonical_path)?;
        sync_parent(&canonical_parent)?;
        Ok(Self {
            path: canonical_path,
        })
    }

    /// Open from the launcher-injected canonical state-root path.
    pub fn open_from_env() -> Result<Self, JournalError> {
        let raw = std::env::var_os(JOURNAL_PATH_ENV)
            .filter(|value| !value.is_empty())
            .ok_or(JournalError::MissingJournalPath)?;
        Self::open(PathBuf::from(raw))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Idempotently enqueue work. Reusing a key with byte-equivalent canonical
    /// JSON and the same kind is a no-op; reusing it for different work fails.
    pub fn enqueue(
        &self,
        key: &str,
        kind: WorkKind,
        payload: &Value,
        created_at_ms: u64,
    ) -> Result<EnqueueOutcome, JournalError> {
        require_non_empty("work key", key)?;
        let created_at_ms = sqlite_u64(created_at_ms)?;
        let (payload_json, payload_hash) = canonical_json(payload)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT kind, payload_sha256, state FROM work_items WHERE key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((existing_kind, existing_hash, state)) = existing {
            if existing_kind != kind.as_str() || existing_hash != payload_hash {
                return Err(JournalError::IdempotencyConflict {
                    key: key.to_owned(),
                    reason: "same key was already committed for different work".to_owned(),
                });
            }
            tx.commit()?;
            return Ok(EnqueueOutcome::Existing {
                state: WorkState::parse(&state)?,
            });
        }

        tx.execute(
            "INSERT INTO work_items (
                key, kind, payload_json, payload_sha256, state,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
            params![
                key,
                kind.as_str(),
                payload_json,
                payload_hash,
                created_at_ms
            ],
        )?;
        tx.commit()?;
        Ok(EnqueueOutcome::Inserted)
    }

    /// Claim pending work, including delivery leases abandoned by a crashed
    /// worker. Reclaims increment the epoch, fencing the previous owner.
    pub fn claim_delivery(
        &self,
        kind: WorkKind,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
        limit: usize,
    ) -> Result<Vec<WorkItem>, JournalError> {
        require_non_empty("delivery owner", owner)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(lease_ms.max(1)))?;
        let limit = i64::try_from(limit).map_err(|_| JournalError::IntegerOverflow(u64::MAX))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let keys = {
            let mut stmt = tx.prepare(
                "SELECT key FROM work_items
                 WHERE kind = ?1
                   AND (
                       (state = 'pending' AND next_attempt_at_ms <= ?2)
                       OR
                       (state IN ('leased', 'acked') AND lease_expires_at_ms <= ?2)
                   )
                 ORDER BY next_attempt_at_ms, created_at_ms, key
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![kind.as_str(), now, limit], |row| row.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };

        let mut claimed = Vec::with_capacity(keys.len());
        for key in keys {
            tx.execute(
                "UPDATE work_items
                 SET state = 'leased',
                     delivery_epoch = delivery_epoch + 1,
                     attempts = attempts + 1,
                     lease_owner = ?2,
                     lease_expires_at_ms = ?3,
                     updated_at_ms = ?4,
                     last_error = NULL
                 WHERE key = ?1",
                params![key, owner, expires, now],
            )?;
            claimed.push(load_item(&tx, &key)?.ok_or_else(|| {
                JournalError::CorruptState(format!("claimed row disappeared: {key}"))
            })?);
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// Return a bounded, read-only snapshot of delivery keys claimable at
    /// `now_ms`. No owner, epoch, or lease is created by this method.
    ///
    /// Consumers enumerate this snapshot and then claim each selected key
    /// under a fresh fence. Keeping enumeration non-mutating prevents a poison
    /// payload at the head of the queue from monopolizing repeated batch
    /// claims and starving later valid work.
    pub fn delivery_candidate_keys(
        &self,
        kind: WorkKind,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<String>, JournalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = sqlite_u64(now_ms)?;
        let limit = i64::try_from(limit).map_err(|_| JournalError::IntegerOverflow(u64::MAX))?;
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT key FROM work_items
             WHERE kind = ?1
               AND (
                   (state = 'pending' AND next_attempt_at_ms <= ?2)
                   OR
                   (state IN ('leased', 'acked') AND lease_expires_at_ms <= ?2)
               )
             ORDER BY next_attempt_at_ms, created_at_ms, key
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![kind.as_str(), now, limit], |row| row.get(0))?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(JournalError::from)
    }

    /// Claim one known work item. A retry by the same owner while its lease is
    /// still live returns the existing fence; reclaiming an expired lease
    /// increments the epoch and fences the former owner.
    pub fn claim_delivery_by_key(
        &self,
        key: &str,
        kind: WorkKind,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<WorkItem>, JournalError> {
        require_non_empty("work key", key)?;
        require_non_empty("delivery owner", owner)?;
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(lease_ms.max(1)))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            tx.commit()?;
            return Ok(None);
        };
        if item.kind != kind {
            tx.commit()?;
            return Ok(None);
        }

        let same_live_claim = matches!(item.state, WorkState::Leased | WorkState::Acked)
            && item.lease_owner.as_deref() == Some(owner)
            && item
                .lease_expires_at_ms
                .is_some_and(|expiry| expiry >= now_ms);
        if same_live_claim {
            tx.commit()?;
            return Ok(Some(item));
        }

        let claimable = (item.state == WorkState::Pending && item.next_attempt_at_ms <= now_ms)
            || (matches!(item.state, WorkState::Leased | WorkState::Acked)
                && item
                    .lease_expires_at_ms
                    .is_some_and(|expiry| expiry <= now_ms));
        if !claimable {
            tx.commit()?;
            return Ok(None);
        }

        tx.execute(
            "UPDATE work_items
             SET state = 'leased',
                 delivery_epoch = delivery_epoch + 1,
                 attempts = attempts + 1,
                 lease_owner = ?2,
                 lease_expires_at_ms = ?3,
                 updated_at_ms = ?4,
                 last_error = NULL
             WHERE key = ?1",
            params![key, owner, expires, now],
        )?;
        let claimed = load_item(&tx, key)?
            .ok_or_else(|| JournalError::CorruptState(format!("claimed row disappeared: {key}")))?;
        tx.commit()?;
        Ok(Some(claimed))
    }

    /// Ack that a worker durably accepted a leased handoff.
    pub fn ack_delivery(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        extend_lease_ms: u64,
    ) -> Result<AckOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(extend_lease_ms.max(1)))?;
        let epoch = sqlite_u64(fence.epoch)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            return Ok(AckOutcome::Missing);
        };

        let outcome = if item.state == WorkState::Acked
            && item.delivery_epoch == fence.epoch
            && item.lease_owner.as_deref() == Some(fence.owner.as_str())
            && item
                .lease_expires_at_ms
                .is_some_and(|expiry| expiry >= now_ms)
        {
            AckOutcome::AlreadyAcked
        } else if item.state != WorkState::Leased
            || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            AckOutcome::Stale
        } else {
            tx.execute(
                "UPDATE work_items
                 SET state = 'acked', lease_expires_at_ms = ?2, updated_at_ms = ?3
                 WHERE key = ?1 AND delivery_epoch = ?4 AND lease_owner = ?5",
                params![key, expires, now, epoch, fence.owner],
            )?;
            AckOutcome::Acked
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Extend a live delivery lease without changing its epoch or lifecycle
    /// state. Both `Leased` and `Acked` are accepted so an ack response loss
    /// cannot prevent the consumer from renewing the same valid fence.
    pub fn renew_delivery(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        extend_lease_ms: u64,
    ) -> Result<RenewOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(extend_lease_ms.max(1)))?;
        let Some(item) = self.get(key)? else {
            return Ok(RenewOutcome::Missing);
        };
        if !matches!(item.state, WorkState::Leased | WorkState::Acked)
            || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            return Ok(RenewOutcome::Stale);
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE work_items
             SET lease_expires_at_ms = ?2, updated_at_ms = ?3
             WHERE key = ?1
               AND state IN ('leased', 'acked')
               AND delivery_epoch = ?4
               AND lease_owner = ?5
               AND lease_expires_at_ms >= ?3",
            params![key, expires, now, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        Ok(if changed == 1 {
            RenewOutcome::Renewed
        } else {
            RenewOutcome::Stale
        })
    }

    /// Return an uncompleted delivery to `Pending` after a retryable failure.
    pub fn release_delivery(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        next_attempt_at_ms: u64,
        error: &str,
    ) -> Result<ReleaseOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let next = sqlite_u64(next_attempt_at_ms)?;
        let Some(item) = self.get(key)? else {
            return Ok(ReleaseOutcome::Missing);
        };
        if !matches!(item.state, WorkState::Leased | WorkState::Acked)
            || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            return Ok(ReleaseOutcome::Stale);
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE work_items
             SET state = 'pending',
                 next_attempt_at_ms = ?2,
                 lease_owner = NULL,
                 lease_expires_at_ms = NULL,
                 updated_at_ms = ?3,
                 last_error = ?4
             WHERE key = ?1
               AND state IN ('leased', 'acked')
               AND delivery_epoch = ?5
               AND lease_owner = ?6
               AND lease_expires_at_ms >= ?3",
            params![key, next, now, error, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        Ok(if changed == 1 {
            ReleaseOutcome::Released
        } else {
            ReleaseOutcome::Stale
        })
    }

    /// Record a result under a valid delivery fence. An identical retry after
    /// the commit returns [`RecordResultOutcome::Duplicate`].
    pub fn record_result(
        &self,
        key: &str,
        result_key: &str,
        result: &Value,
        fence: &DeliveryFence,
        now_ms: u64,
    ) -> Result<RecordResultOutcome, JournalError> {
        self.record_result_inner(key, result_key, result, Some(fence), now_ms)
    }

    /// Compatibility bridge for a result source that has a stable idempotency
    /// key but no delivery fence yet. It can consume **only Pending work**;
    /// once a worker lease exists, unfenced results are rejected. New protocol
    /// wiring should use [`Journal::record_result`].
    pub fn record_unfenced_pending_result(
        &self,
        key: &str,
        result_key: &str,
        result: &Value,
        now_ms: u64,
    ) -> Result<RecordResultOutcome, JournalError> {
        self.record_result_inner(key, result_key, result, None, now_ms)
    }

    fn record_result_inner(
        &self,
        key: &str,
        result_key: &str,
        result: &Value,
        fence: Option<&DeliveryFence>,
        now_ms: u64,
    ) -> Result<RecordResultOutcome, JournalError> {
        require_non_empty("result key", result_key)?;
        let now = sqlite_u64(now_ms)?;
        let (result_json, result_hash) = canonical_json(result)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            return Ok(RecordResultOutcome::Missing);
        };

        if item.result_key.is_some() {
            let existing_hash: String = tx.query_row(
                "SELECT result_sha256 FROM work_items WHERE key = ?1",
                [key],
                |row| row.get(0),
            )?;
            if item.result_key.as_deref() == Some(result_key) && existing_hash == result_hash {
                tx.commit()?;
                return Ok(RecordResultOutcome::Duplicate);
            }
            return Err(JournalError::IdempotencyConflict {
                key: key.to_owned(),
                reason: "a different result was already committed".to_owned(),
            });
        }

        let duplicate_owner: Option<String> = tx
            .query_row(
                "SELECT key FROM work_items WHERE result_key = ?1",
                [result_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(other) = duplicate_owner {
            return Err(JournalError::IdempotencyConflict {
                key: result_key.to_owned(),
                reason: format!("result key is already bound to work {other:?}"),
            });
        }

        let valid = match fence {
            Some(fence) => {
                matches!(item.state, WorkState::Leased | WorkState::Acked)
                    && item.delivery_epoch == fence.epoch
                    && item.lease_owner.as_deref() == Some(fence.owner.as_str())
                    && item
                        .lease_expires_at_ms
                        .is_some_and(|expiry| expiry >= now_ms)
            }
            None => item.state == WorkState::Pending,
        };
        if !valid {
            tx.commit()?;
            return Ok(RecordResultOutcome::Stale);
        }

        tx.execute(
            "UPDATE work_items
             SET state = 'result_ready',
                 result_key = ?2,
                 result_json = ?3,
                 result_sha256 = ?4,
                 result_recorded_at_ms = ?5,
                 lease_owner = NULL,
                 lease_expires_at_ms = NULL,
                 updated_at_ms = ?5,
                 last_error = NULL
             WHERE key = ?1",
            params![key, result_key, result_json, result_hash, now],
        )?;
        tx.commit()?;
        Ok(RecordResultOutcome::Recorded)
    }

    /// Claim durable results for idempotent side-effect settlement. Expired
    /// settlement claims are crash-recovered with a new fencing epoch.
    ///
    /// Startup recovery that must isolate one malformed row from all later
    /// rows should first snapshot [`Journal::settlement_candidate_keys`] and
    /// then call [`Journal::claim_settlement_by_key`] once per key.
    pub fn claim_settlement(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
        limit: usize,
    ) -> Result<Vec<WorkItem>, JournalError> {
        require_non_empty("settlement owner", owner)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(lease_ms.max(1)))?;
        let limit = i64::try_from(limit).map_err(|_| JournalError::IntegerOverflow(u64::MAX))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let keys = {
            let mut stmt = tx.prepare(
                "SELECT key FROM work_items
                 WHERE state = 'result_ready'
                    OR (state = 'settling' AND lease_expires_at_ms <= ?1)
                 ORDER BY result_recorded_at_ms, key
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![now, limit], |row| row.get(0))?;
            rows.collect::<Result<Vec<String>, _>>()?
        };

        let mut claimed = Vec::with_capacity(keys.len());
        for key in keys {
            tx.execute(
                "UPDATE work_items
                 SET state = 'settling',
                     delivery_epoch = delivery_epoch + 1,
                     lease_owner = ?2,
                     lease_expires_at_ms = ?3,
                     updated_at_ms = ?4
                 WHERE key = ?1",
                params![key, owner, expires, now],
            )?;
            claimed.push(load_item(&tx, &key)?.ok_or_else(|| {
                JournalError::CorruptState(format!("settlement row disappeared: {key}"))
            })?);
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// Snapshot every result that is claimable for settlement at `now_ms`.
    ///
    /// This is intentionally read-only. A caller still has to claim each key
    /// under a fresh fence before applying effects. Snapshotting keys lets a
    /// restart drain attempt every row exactly once in that drain pass: one
    /// poison result can be released for a later retry without being
    /// immediately reclaimed and starving subsequent rows.
    pub fn settlement_candidate_keys(
        &self,
        kind: WorkKind,
        now_ms: u64,
    ) -> Result<Vec<String>, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT key FROM work_items
             WHERE kind = ?1
               AND (
                   state = 'result_ready'
                   OR (state = 'settling' AND lease_expires_at_ms <= ?2)
               )
             ORDER BY result_recorded_at_ms, key",
        )?;
        let rows = stmt.query_map(params![kind.as_str(), now], |row| row.get(0))?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(JournalError::from)
    }

    /// Claim settlement for one known result. Retrying with the same owner
    /// while its settlement lease remains live returns the existing fence.
    pub fn claim_settlement_by_key(
        &self,
        key: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<WorkItem>, JournalError> {
        require_non_empty("work key", key)?;
        require_non_empty("settlement owner", owner)?;
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(lease_ms.max(1)))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            tx.commit()?;
            return Ok(None);
        };

        let same_live_claim = item.state == WorkState::Settling
            && item.lease_owner.as_deref() == Some(owner)
            && item
                .lease_expires_at_ms
                .is_some_and(|expiry| expiry >= now_ms);
        if same_live_claim {
            tx.commit()?;
            return Ok(Some(item));
        }

        let claimable = item.state == WorkState::ResultReady
            || (item.state == WorkState::Settling
                && item
                    .lease_expires_at_ms
                    .is_some_and(|expiry| expiry <= now_ms));
        if !claimable {
            tx.commit()?;
            return Ok(None);
        }

        tx.execute(
            "UPDATE work_items
             SET state = 'settling',
                 delivery_epoch = delivery_epoch + 1,
                 lease_owner = ?2,
                 lease_expires_at_ms = ?3,
                 updated_at_ms = ?4
             WHERE key = ?1",
            params![key, owner, expires, now],
        )?;
        let claimed = load_item(&tx, key)?.ok_or_else(|| {
            JournalError::CorruptState(format!("settlement row disappeared: {key}"))
        })?;
        tx.commit()?;
        Ok(Some(claimed))
    }

    /// Extend a live settlement lease without changing its epoch. Settlement
    /// may include durable filesystem work whose duration has no fixed upper
    /// bound, so callers must renew rather than treating the initial lease as
    /// an execution timeout.
    pub fn renew_settlement(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        extend_lease_ms: u64,
    ) -> Result<RenewOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let expires = sqlite_u64(now_ms.saturating_add(extend_lease_ms.max(1)))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            tx.commit()?;
            return Ok(RenewOutcome::Missing);
        };
        if item.state != WorkState::Settling
            || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            tx.commit()?;
            return Ok(RenewOutcome::Stale);
        }

        let changed = tx.execute(
            "UPDATE work_items
             SET lease_expires_at_ms = ?2, updated_at_ms = ?3
             WHERE key = ?1
               AND state = 'settling'
               AND delivery_epoch = ?4
               AND lease_owner = ?5
               AND lease_expires_at_ms >= ?3",
            params![key, expires, now, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        Ok(if changed == 1 {
            RenewOutcome::Renewed
        } else {
            RenewOutcome::Stale
        })
    }

    /// Commit successful settlement under the claim fence.
    pub fn mark_settled(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
    ) -> Result<SettleOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            return Ok(SettleOutcome::Missing);
        };
        if item.state == WorkState::Settled {
            tx.commit()?;
            return Ok(SettleOutcome::AlreadySettled);
        }
        if item.state != WorkState::Settling
            || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            tx.commit()?;
            return Ok(SettleOutcome::Stale);
        }

        tx.execute(
            "UPDATE work_items
             SET state = 'settled',
                 lease_owner = NULL,
                 lease_expires_at_ms = NULL,
                 settled_at_ms = ?2,
                 updated_at_ms = ?2,
                 last_error = NULL
             WHERE key = ?1 AND state = 'settling'
               AND delivery_epoch = ?3 AND lease_owner = ?4
               AND lease_expires_at_ms >= ?2",
            params![key, now, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        Ok(SettleOutcome::Settled)
    }

    /// Release a failed settlement for deterministic retry.
    pub fn release_settlement(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        error: &str,
    ) -> Result<ReleaseOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE work_items
             SET state = 'result_ready',
                 lease_owner = NULL,
                 lease_expires_at_ms = NULL,
                 updated_at_ms = ?2,
                 last_error = ?3
             WHERE key = ?1 AND state = 'settling'
               AND delivery_epoch = ?4 AND lease_owner = ?5
               AND lease_expires_at_ms >= ?2",
            params![key, now, error, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        if changed == 1 {
            Ok(ReleaseOutcome::Released)
        } else if self.get(key)?.is_none() {
            Ok(ReleaseOutcome::Missing)
        } else {
            Ok(ReleaseOutcome::Stale)
        }
    }

    /// Permanently stop retrying a currently fenced delivery or settlement.
    /// The payload/result remain in SQLite for diagnosis and manual replay.
    pub fn mark_dead_letter(
        &self,
        key: &str,
        fence: &DeliveryFence,
        now_ms: u64,
        error: &str,
    ) -> Result<DeadLetterOutcome, JournalError> {
        let now = sqlite_u64(now_ms)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(item) = load_item(&tx, key)? else {
            return Ok(DeadLetterOutcome::Missing);
        };
        if item.state == WorkState::DeadLetter {
            tx.commit()?;
            return Ok(DeadLetterOutcome::AlreadyDeadLettered);
        }
        if !matches!(
            item.state,
            WorkState::Leased | WorkState::Acked | WorkState::Settling
        ) || item.delivery_epoch != fence.epoch
            || item.lease_owner.as_deref() != Some(fence.owner.as_str())
            || item
                .lease_expires_at_ms
                .is_none_or(|expiry| expiry < now_ms)
        {
            tx.commit()?;
            return Ok(DeadLetterOutcome::Stale);
        }

        let changed = tx.execute(
            "UPDATE work_items
             SET state = 'dead_letter',
                 lease_owner = NULL,
                 lease_expires_at_ms = NULL,
                 updated_at_ms = ?2,
                 last_error = ?3
             WHERE key = ?1
               AND state IN ('leased', 'acked', 'settling')
               AND delivery_epoch = ?4
               AND lease_owner = ?5
               AND lease_expires_at_ms >= ?2",
            params![key, now, error, sqlite_u64(fence.epoch)?, fence.owner],
        )?;
        tx.commit()?;
        Ok(if changed == 1 {
            DeadLetterOutcome::DeadLettered
        } else {
            DeadLetterOutcome::Stale
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<WorkItem>, JournalError> {
        let conn = self.connection()?;
        load_item(&conn, key)
    }

    fn connection(&self) -> Result<Connection, JournalError> {
        reject_symlink(&self.path)?;
        open_connection(&self.path)
    }
}

fn open_connection(path: &Path) -> Result<Connection, JournalError> {
    let conn = open_base_connection(path)?;
    configure_durable_connection(&conn)?;
    Ok(conn)
}

fn open_base_connection(path: &Path) -> Result<Connection, JournalError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

fn configure_durable_connection(conn: &Connection) -> Result<(), JournalError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA wal_autocheckpoint = 1000;",
    )?;
    Ok(())
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != ';')
        .collect::<String>()
        .to_ascii_lowercase()
        .replace("ifnotexists", "")
}

fn validate_schema(conn: &Connection) -> Result<(), JournalError> {
    let quick_check: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if quick_check != "ok" {
        return Err(JournalError::SchemaMismatch {
            version: SCHEMA_VERSION,
            reason: format!("PRAGMA quick_check failed: {quick_check}"),
        });
    }

    let statements = SCHEMA
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect::<Vec<_>>();
    if statements.len() != 4 {
        return Err(JournalError::SchemaMismatch {
            version: SCHEMA_VERSION,
            reason: format!(
                "internal schema contract contains {} statements instead of 4",
                statements.len()
            ),
        });
    }

    let mut expected = vec![
        (
            "table".to_owned(),
            "work_items".to_owned(),
            normalize_schema_sql(statements[0]),
        ),
        (
            "index".to_owned(),
            "idx_memory_work_result_key".to_owned(),
            normalize_schema_sql(statements[1]),
        ),
        (
            "index".to_owned(),
            "idx_memory_work_delivery".to_owned(),
            normalize_schema_sql(statements[2]),
        ),
        (
            "index".to_owned(),
            "idx_memory_work_settlement".to_owned(),
            normalize_schema_sql(statements[3]),
        ),
    ];
    expected.sort();

    let mut query = conn.prepare(
        "SELECT type, name, sql
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'
           AND type IN ('table', 'index', 'view', 'trigger')
         ORDER BY type, name",
    )?;
    let mut actual = query
        .query_map([], |row| {
            let object_type: String = row.get(0)?;
            let name: String = row.get(1)?;
            let sql: Option<String> = row.get(2)?;
            Ok((object_type, name, sql))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(object_type, name, sql)| {
            (
                object_type,
                name,
                sql.as_deref().map(normalize_schema_sql).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    actual.sort();

    if actual != expected {
        let expected_identity = expected
            .iter()
            .map(|(object_type, name, _)| format!("{object_type}:{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let actual_identity = actual
            .iter()
            .map(|(object_type, name, _)| format!("{object_type}:{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(JournalError::SchemaMismatch {
            version: SCHEMA_VERSION,
            reason: format!(
                "expected exact objects [{expected_identity}], found [{actual_identity}]; \
                 one or more definitions, constraints, STRICT markers, or indexes differ"
            ),
        });
    }
    Ok(())
}

fn first_user_schema_object(conn: &Connection) -> Result<Option<String>, JournalError> {
    conn.query_row(
        "SELECT name
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'
           AND type IN ('table', 'index', 'view', 'trigger')
         ORDER BY name
         LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(JournalError::from)
}

fn load_item(conn: &Connection, key: &str) -> Result<Option<WorkItem>, JournalError> {
    let raw = conn
        .query_row(
            "SELECT key, kind, payload_json, state, delivery_epoch, attempts,
                    next_attempt_at_ms, lease_owner, lease_expires_at_ms,
                    result_key, result_json, result_recorded_at_ms,
                    created_at_ms, updated_at_ms, settled_at_ms, last_error
             FROM work_items WHERE key = ?1",
            [key],
            |row| {
                Ok(RawWorkItem {
                    key: row.get(0)?,
                    kind: row.get(1)?,
                    payload_json: row.get(2)?,
                    state: row.get(3)?,
                    delivery_epoch: row.get(4)?,
                    attempts: row.get(5)?,
                    next_attempt_at_ms: row.get(6)?,
                    lease_owner: row.get(7)?,
                    lease_expires_at_ms: row.get(8)?,
                    result_key: row.get(9)?,
                    result_json: row.get(10)?,
                    result_recorded_at_ms: row.get(11)?,
                    created_at_ms: row.get(12)?,
                    updated_at_ms: row.get(13)?,
                    settled_at_ms: row.get(14)?,
                    last_error: row.get(15)?,
                })
            },
        )
        .optional()?;
    raw.map(RawWorkItem::try_into).transpose()
}

struct RawWorkItem {
    key: String,
    kind: String,
    payload_json: String,
    state: String,
    delivery_epoch: i64,
    attempts: i64,
    next_attempt_at_ms: i64,
    lease_owner: Option<String>,
    lease_expires_at_ms: Option<i64>,
    result_key: Option<String>,
    result_json: Option<String>,
    result_recorded_at_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
    settled_at_ms: Option<i64>,
    last_error: Option<String>,
}

impl TryFrom<RawWorkItem> for WorkItem {
    type Error = JournalError;

    fn try_from(raw: RawWorkItem) -> Result<Self, Self::Error> {
        Ok(Self {
            key: raw.key,
            kind: WorkKind::parse(&raw.kind)?,
            payload: serde_json::from_str(&raw.payload_json)?,
            state: WorkState::parse(&raw.state)?,
            delivery_epoch: non_negative(raw.delivery_epoch, "delivery_epoch")?,
            attempts: non_negative(raw.attempts, "attempts")?,
            next_attempt_at_ms: non_negative(raw.next_attempt_at_ms, "next_attempt_at_ms")?,
            lease_owner: raw.lease_owner,
            lease_expires_at_ms: optional_non_negative(
                raw.lease_expires_at_ms,
                "lease_expires_at_ms",
            )?,
            result_key: raw.result_key,
            result: raw
                .result_json
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            result_recorded_at_ms: optional_non_negative(
                raw.result_recorded_at_ms,
                "result_recorded_at_ms",
            )?,
            created_at_ms: non_negative(raw.created_at_ms, "created_at_ms")?,
            updated_at_ms: non_negative(raw.updated_at_ms, "updated_at_ms")?,
            settled_at_ms: optional_non_negative(raw.settled_at_ms, "settled_at_ms")?,
            last_error: raw.last_error,
        })
    }
}

fn canonical_json(value: &Value) -> Result<(String, String), JournalError> {
    let canonical = canonicalize(value);
    let bytes = serde_json::to_vec(&canonical)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let json = String::from_utf8(bytes)
        .map_err(|error| JournalError::CorruptState(format!("JSON was not UTF-8: {error}")))?;
    Ok((json, hash))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonicalize(&values[key]));
            }
            Value::Object(canonical)
        }
        other => other.clone(),
    }
}

fn sqlite_u64(value: u64) -> Result<i64, JournalError> {
    i64::try_from(value).map_err(|_| JournalError::IntegerOverflow(value))
}

fn non_negative(value: i64, field: &str) -> Result<u64, JournalError> {
    u64::try_from(value).map_err(|_| {
        JournalError::CorruptState(format!(
            "{field} unexpectedly contains negative value {value}"
        ))
    })
}

fn optional_non_negative(value: Option<i64>, field: &str) -> Result<Option<u64>, JournalError> {
    value.map(|value| non_negative(value, field)).transpose()
}

fn require_non_empty(label: &str, value: &str) -> Result<(), JournalError> {
    if value.trim().is_empty() {
        return Err(JournalError::CorruptState(format!(
            "{label} must not be empty"
        )));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(JournalError::SymlinkPath(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// SQLite accepts Windows verbatim paths, so the repository-wide reason for
/// preferring `dunce` (passing paths to legacy Win32 programs) does not apply
/// at this private storage boundary. Keeping the standard canonical form also
/// avoids adding a dependency solely to strip a prefix SQLite understands.
#[allow(clippy::disallowed_methods)]
fn canonicalize_for_sqlite(path: &Path) -> Result<PathBuf, std::io::Error> {
    fs::canonicalize(path)
}

fn secure_file(path: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn sync_parent(parent: &Path) -> Result<(), JournalError> {
    #[cfg(unix)]
    {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
