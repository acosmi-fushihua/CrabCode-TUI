//! Durable ledger connection: opens the SQLite-WAL database, applies the
//! required PRAGMAs, installs the schema, and performs a one-shot startup
//! checkpoint. See the crate-level docs for the durability contract.

use std::path::Path;

use rusqlite::Connection;
use tracing::warn;

use crate::error::LedgerError;
use crate::schema::{SCHEMA_DDL, SCHEMA_USER_VERSION};

/// An open handle to the cron occurrence ledger.
///
/// Wraps a single [`rusqlite::Connection`] configured for WAL journaling with
/// `synchronous = FULL`. All access in W1a is synchronous (`&mut self`); the
/// daemon-facing async wrapper (`spawn_blocking`) is added in W1c.
#[derive(Debug)]
pub struct LedgerConnection {
    /// Underlying SQLite connection. `pub(crate)` so the sibling `fire` module
    /// and in-crate tests can issue statements without a public escape hatch.
    pub(crate) conn: Connection,
}

impl LedgerConnection {
    /// Open (creating if absent) the ledger at `db_path`.
    ///
    /// Steps, in order:
    /// 1. `Connection::open` — creates the file if missing.
    /// 2. Apply durability PRAGMAs: `journal_mode = WAL`, `synchronous = FULL`,
    ///    `busy_timeout = 5000`, `foreign_keys = ON`.
    /// 3. Install the schema ([`SCHEMA_DDL`], all `IF NOT EXISTS` → idempotent).
    /// 4. Stamp `PRAGMA user_version` = [`SCHEMA_USER_VERSION`].
    /// 5. Run one `wal_checkpoint(TRUNCATE)` to fold any WAL left by a prior
    ///    unclean shutdown back into the main database and reset the sidecar.
    ///    A non-zero `busy` column (another connection held a lock) is logged
    ///    but is not an error — the next checkpoint catches up.
    ///
    /// The `-wal` / `-shm` sidecar files are never deleted by hand; SQLite owns
    /// their lifecycle.
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if opening, configuring, installing the
    /// schema, or the startup checkpoint fails.
    pub fn open(db_path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(db_path)?;

        // Durability PRAGMAs. `execute_batch` runs each statement and discards
        // the result rows some PRAGMAs return (e.g. `journal_mode` echoes the
        // resulting mode), which a plain `execute` would reject. Unquoted
        // keyword values avoid any string-quoting ambiguity.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = FULL;\n\
             PRAGMA busy_timeout = 5000;\n\
             PRAGMA foreign_keys = ON;",
        )?;

        conn.execute_batch(SCHEMA_DDL)?;

        // v1 → v2 in-place upgrade. Every statement in `SCHEMA_DDL` is
        // `IF NOT EXISTS`, so on a database first created under schema v1 the
        // `occurrences` `CREATE TABLE` is a no-op and does NOT add the v2
        // `target` / `envelope_json` columns. Add them here (idempotently), then
        // create the `idx_occurrences_claim` index that references them — the
        // index is NOT in `SCHEMA_DDL` precisely because it must be created only
        // after the columns exist (see `upgrade_occurrences_to_v2`). A fresh
        // database already has the columns from `SCHEMA_DDL`, so only the index
        // create does work there. Runs before the `user_version` stamp so an
        // interrupted upgrade is retried on the next open (the stamp is the
        // commit point).
        Self::upgrade_occurrences_to_v2(&conn)?;

        conn.pragma_update(None, "user_version", SCHEMA_USER_VERSION)?;

        // One startup checkpoint (TRUNCATE): fold + reset the WAL so a crash
        // right after open leaves the smallest possible recovery surface.
        // Returns (busy, log, checkpointed); a non-zero `busy` is a warning, not
        // a failure.
        let (busy, log, checkpointed): (i64, i64, i64) =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy != 0 {
            warn!(
                busy,
                log,
                checkpointed,
                "cron ledger: startup wal_checkpoint(TRUNCATE) was blocked by another connection; continuing"
            );
        }

        Ok(Self { conn })
    }

    /// Idempotently bring a pre-existing `occurrences` table up to schema v2:
    /// add the `target` / `envelope_json` columns and the `idx_occurrences_claim`
    /// index if missing.
    ///
    /// A fresh database already has the two columns from [`SCHEMA_DDL`], so the
    /// probe skips the `ALTER`s and only the index create does work; a database
    /// first created under schema v1 has neither, so it adds both columns and
    /// the index. Each `ADD COLUMN` is guarded by a `PRAGMA table_info(occurrences)`
    /// probe — `ALTER TABLE ... ADD COLUMN` errors if the column already exists,
    /// whereas the index create is `IF NOT EXISTS` (always safe). Columns are
    /// added before the index because the index references `target`; this is the
    /// sole creator of `idx_occurrences_claim` (it is intentionally absent from
    /// `SCHEMA_DDL`, which runs before the columns exist on a v1 upgrade).
    ///
    /// `STRICT` tables forbid `ALTER`-adding a `NOT NULL` / `CHECK` column, so
    /// both are plain nullable `TEXT`; their domain is enforced in Rust (see
    /// [`crate::ExecutionEnvelope`]).
    ///
    /// # Errors
    /// Returns [`LedgerError::Sqlite`] if the probe, an `ALTER`, or the index
    /// create fails.
    fn upgrade_occurrences_to_v2(conn: &Connection) -> Result<(), LedgerError> {
        let (mut has_target, mut has_envelope_json) = (false, false);
        {
            let mut stmt = conn.prepare("PRAGMA table_info(occurrences)")?;
            let mut rows = stmt.query([])?;
            // `table_info` columns: (cid, name, type, notnull, dflt_value, pk).
            while let Some(row) = rows.next()? {
                let name: String = row.get(1)?;
                match name.as_str() {
                    "target" => has_target = true,
                    "envelope_json" => has_envelope_json = true,
                    _ => {}
                }
            }
        }
        if !has_target {
            conn.execute_batch("ALTER TABLE occurrences ADD COLUMN target TEXT")?;
        }
        if !has_envelope_json {
            conn.execute_batch("ALTER TABLE occurrences ADD COLUMN envelope_json TEXT")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_occurrences_claim \
             ON occurrences(target, status, scheduled_at_ms)",
        )?;
        Ok(())
    }
}
