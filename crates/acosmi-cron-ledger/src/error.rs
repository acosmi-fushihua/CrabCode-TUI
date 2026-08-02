//! Ledger error type.
//!
//! Marked `#[non_exhaustive]`: later slices add failure modes (migration
//! import/cutover, lease/claim conflicts, …), and downstream `match` sites
//! should keep a catch-all rather than break when a variant is added.

use thiserror::Error;

/// Errors surfaced by the cron ledger.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LedgerError {
    /// An underlying SQLite failure (open, PRAGMA, statement, or transaction).
    #[error("cron ledger sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A filesystem failure while preparing the ledger's directory / paths.
    #[error("cron ledger I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization failure. Two producers: migration serializing a
    /// `CronJob` into `jobs.job_json` on import (and reading it back for the
    /// verify round-trip, W1b), and `fire_occurrence` serializing the per-fire
    /// [`crate::ExecutionEnvelope`] into `occurrences.envelope_json` (PR5-W2-1).
    #[error("cron ledger JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The migration verify phase found the imported `jobs` did not match the
    /// source store (row-count or structural round-trip mismatch). Fail loud:
    /// the import is **not** promoted to `cutover_committed`, the source file
    /// is left untouched, and the next start clears and re-imports. (W1b.)
    #[error("cron ledger migration verification failed: {0}")]
    MigrationVerify(String),
}
