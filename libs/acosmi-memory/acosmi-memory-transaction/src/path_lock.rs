// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Path-based locking for transactional file operations.
//!
//! Ported from `openviking/storage/transaction/path_lock.py`.
//!
//! Lock protocol: a lock file `{path}/.path.ovlock` whose content equals
//! the owning transaction ID indicates that `path` is locked.

use acosmi_memory_session::traits::{BoxError, FileSystem};

use crate::transaction_record::TransactionRecord;

/// Name of the lock sentinel file placed inside a locked directory.
pub const LOCK_FILE_NAME: &str = ".path.ovlock";

/// Path-level lock manager backed by an abstract [`FileSystem`].
///
/// All file-system calls are async and delegated to the injected `FS`.
pub struct PathLock<FS: FileSystem> {
    fs: FS,
}

impl<FS: FileSystem> PathLock<FS> {
    /// Create a new `PathLock` with the given file-system backend.
    pub fn new(fs: FS) -> Self {
        Self { fs }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn lock_path(path: &str) -> String {
        let path = path.trim_end_matches('/');
        format!("{path}/{LOCK_FILE_NAME}")
    }

    fn parent_path(path: &str) -> Option<String> {
        let path = path.trim_end_matches('/');
        let idx = path.rfind('/')?;
        if idx == 0 {
            return None;
        }
        Some(path[..idx].to_owned())
    }

    /// Check if `lock_path` is locked by a *different* transaction.
    async fn is_locked_by_other(&self, lock_path: &str, tx_id: &str) -> bool {
        match self.fs.read(lock_path).await {
            Ok(content) => {
                let owner = content.trim();
                !owner.is_empty() && owner != tx_id
            }
            Err(_) => false, // file doesn't exist → not locked
        }
    }

    async fn create_lock_file(&self, lock_path: &str, tx_id: &str) -> Result<(), String> {
        self.fs
            .write(lock_path, tx_id)
            .await
            .map_err(|e| format!("Failed to create lock file {lock_path}: {e}"))
    }

    async fn verify_ownership(&self, lock_path: &str, tx_id: &str) -> bool {
        match self.fs.read(lock_path).await {
            Ok(content) => content.trim() == tx_id,
            Err(_) => false,
        }
    }

    /// Best-effort lock file removal. Returns `Ok(())` if the file is gone
    /// after the call (whether we removed it or it never existed); returns
    /// `Err` only when removal *failed and the file is still there* — that
    /// is the deadlock-risk case the caller must surface.
    async fn remove_lock_file(&self, lock_path: &str) -> Result<(), BoxError> {
        match self.fs.rm(lock_path).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // If the lock file is already gone (e.g. cleaned up by a
                // sibling process), the post-condition is satisfied.
                match self.fs.exists(lock_path).await {
                    Ok(false) => Ok(()),
                    _ => Err(e),
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Acquire a lock for normal (read/write) operations.
    ///
    /// Steps:
    /// 1. Verify directory existence via `stat`.
    /// 2. Check target is not locked by another transaction.
    /// 3. Check parent is not locked by another transaction.
    /// 4. Write lock file.
    /// 5. Re-check parent (race-condition guard).
    /// 6. Verify lock ownership (content matches our tx ID).
    pub async fn acquire_normal(&self, path: &str, tx: &mut TransactionRecord) -> bool {
        let tx_id = tx.id.clone();
        let lp = Self::lock_path(path);
        let parent = Self::parent_path(path);

        // Step 1: directory must exist
        if self.fs.stat(path).await.is_err() {
            log::warn!("Directory does not exist: {path}");
            return false;
        }

        // Step 2: target not locked by another tx
        if self.is_locked_by_other(&lp, &tx_id).await {
            log::warn!("Path already locked by another transaction: {path}");
            return false;
        }

        // Step 3: parent not locked
        if let Some(ref pp) = parent {
            let parent_lp = Self::lock_path(pp);
            if self.is_locked_by_other(&parent_lp, &tx_id).await {
                log::warn!("Parent path locked by another transaction: {pp}");
                return false;
            }
        }

        // Step 4: create lock
        if let Err(e) = self.create_lock_file(&lp, &tx_id).await {
            log::error!("{e}");
            return false;
        }

        // Step 5: re-check parent (guard against race)
        if let Some(ref pp) = parent {
            let parent_lp = Self::lock_path(pp);
            if self.is_locked_by_other(&parent_lp, &tx_id).await {
                log::warn!("Parent path locked after lock creation: {pp}");
                if let Err(e) = self.remove_lock_file(&lp).await {
                    log::error!(
                        "Rollback after race: failed to remove our own \
                         lock file {lp}: {e} — manual cleanup required"
                    );
                }
                return false;
            }
        }

        // Step 6: verify ownership
        if !self.verify_ownership(&lp, &tx_id).await {
            log::error!("Lock ownership verification failed: {path}");
            return false;
        }

        tx.add_lock(&lp);
        log::debug!("Lock acquired: {lp}");
        true
    }

    /// Recursively collect all subdirectory paths under `path`.
    ///
    /// Uses [`FileSystem::ls`] to list entries and recursively descends
    /// into directories, mirroring Python `PathLock._collect_subdirectories`.
    pub async fn collect_subdirectories(&self, path: &str) -> Vec<String> {
        let mut subdirs = Vec::new();
        match self.fs.ls(path).await {
            Ok(entries) => {
                for entry in entries {
                    if entry.is_dir {
                        let entry_path =
                            if entry.name.starts_with('/') || entry.name.contains("://") {
                                entry.name.clone()
                            } else {
                                format!("{}/{}", path.trim_end_matches('/'), entry.name)
                            };
                        subdirs.push(entry_path.clone());
                        // Recurse into subdirectory
                        let mut children = Box::pin(self.collect_subdirectories(&entry_path)).await;
                        subdirs.append(&mut children);
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to list directory {path}: {e}");
            }
        }
        subdirs
    }

    /// Acquire locks for a recursive-delete (rm) operation.
    ///
    /// Locks all subdirectories bottom-up, then the target directory.
    /// On failure, all acquired locks are released in reverse order.
    pub async fn acquire_rm(
        &self,
        path: &str,
        tx: &mut TransactionRecord,
        subdirs: &[String],
    ) -> bool {
        let tx_id = tx.id.clone();
        let lp = Self::lock_path(path);
        let mut acquired: Vec<String> = Vec::new();

        // Lock subdirectories (assumed pre-sorted deepest-first by caller).
        // Closes §十三 HIGH-memvfs-3: previously the sub-locks called
        // create_lock_file (= fs.write) directly with no contention check,
        // silently overwriting a competing transaction's lock sentinel and
        // stealing its lock. acquire_normal does this check via Step 2 +
        // Step 6 ownership re-verify; acquire_rm now matches.
        for subdir in subdirs {
            let sub_lp = Self::lock_path(subdir);
            if self.is_locked_by_other(&sub_lp, &tx_id).await {
                log::warn!("RM sub-path already locked by another tx: {subdir}");
                self.rollback_orphaned(tx, &acquired).await;
                return false;
            }
            if let Err(e) = self.create_lock_file(&sub_lp, &tx_id).await {
                log::error!("Failed to acquire RM sub-lock: {e}");
                self.rollback_orphaned(tx, &acquired).await;
                return false;
            }
            // Re-verify ownership in case a racing tx clobbered our write.
            if !self.verify_ownership(&sub_lp, &tx_id).await {
                log::warn!(
                    "RM sub-lock ownership lost to a racing tx after \
                     creation: {subdir}"
                );
                self.rollback_orphaned(tx, &acquired).await;
                return false;
            }
            acquired.push(sub_lp);
        }

        // Lock target directory — same contention guard as sub-locks.
        if self.is_locked_by_other(&lp, &tx_id).await {
            log::warn!("RM target path already locked by another tx: {path}");
            self.rollback_orphaned(tx, &acquired).await;
            return false;
        }
        if let Err(e) = self.create_lock_file(&lp, &tx_id).await {
            log::error!("Failed to acquire RM target lock: {e}");
            self.rollback_orphaned(tx, &acquired).await;
            return false;
        }
        if !self.verify_ownership(&lp, &tx_id).await {
            log::warn!(
                "RM target lock ownership lost to a racing tx after \
                 creation: {path}"
            );
            self.rollback_orphaned(tx, &acquired).await;
            return false;
        }
        acquired.push(lp);

        // Register all locks with the transaction
        for lock in &acquired {
            tx.add_lock(lock);
        }

        log::debug!("RM locks acquired for {} paths", acquired.len());
        true
    }

    /// Acquire locks for a move (mv) operation.
    ///
    /// Source is locked with rm-style locking; destination with normal locking.
    pub async fn acquire_mv(
        &self,
        src_path: &str,
        dst_path: &str,
        tx: &mut TransactionRecord,
        src_subdirs: &[String],
    ) -> bool {
        // Lock source (rm-style)
        if !self.acquire_rm(src_path, tx, src_subdirs).await {
            log::warn!("Failed to lock source path: {src_path}");
            return false;
        }

        // Lock destination (normal)
        if !self.acquire_normal(dst_path, tx).await {
            log::warn!("Failed to lock destination path: {dst_path}");
            // Release all source locks; orphans (if any) are kept on
            // tx.locks for a later retry by cleanup_timed_out.
            if let Err(failed) = self.release(tx).await {
                log::error!(
                    "acquire_mv source-lock rollback left {} orphan(s) \
                     for tx {}",
                    failed.len(),
                    tx.id
                );
            }
            return false;
        }

        log::debug!("MV locks acquired: {src_path} -> {dst_path}");
        true
    }

    /// Release all locks held by the transaction (LIFO order).
    ///
    /// Returns `Ok(())` if every lock file was removed (or was already
    /// gone). Returns `Err(failed)` listing locks whose files could not
    /// be removed; those entries remain on `tx.locks` so a subsequent
    /// retry — `release` again, or the cleanup-timed-out task — can try
    /// to remove them. Successfully-removed locks are dropped from
    /// `tx.locks` regardless. Closes §九 §9.9 / §十三 HIGH-memvfs-4: the
    /// previous version unconditionally cleared `tx.locks` even when
    /// `rm` failed, leaving orphan lock files on disk that locked the
    /// path forever.
    pub async fn release(&self, tx: &mut TransactionRecord) -> Result<(), Vec<(String, BoxError)>> {
        let locks: Vec<String> = tx.locks.iter().rev().cloned().collect();
        let total = locks.len();
        let mut failed: Vec<(String, BoxError)> = Vec::new();
        for lock_path in &locks {
            if let Err(e) = self.remove_lock_file(lock_path).await {
                log::error!("Failed to remove lock {lock_path} for tx {}: {e}", tx.id);
                failed.push((lock_path.clone(), e));
            }
        }
        let failed_paths: std::collections::HashSet<String> =
            failed.iter().map(|(p, _)| p.clone()).collect();
        tx.locks.retain(|p| failed_paths.contains(p));

        if failed.is_empty() {
            log::debug!("Released locks for transaction {}", tx.id);
            Ok(())
        } else {
            log::error!(
                "{} of {total} locks failed to release for tx {} — orphan \
                 lock files remain on disk; will retry next release",
                failed.len(),
                tx.id
            );
            Err(failed)
        }
    }

    /// Rollback partially-acquired locks for a failed `acquire_rm` /
    /// `acquire_mv`. Any lock file that cannot be removed is registered
    /// on the transaction so the orphan can be retried by `release()` or
    /// the timeout-cleanup task — preventing permanent lock-file leaks.
    async fn rollback_orphaned(&self, tx: &mut TransactionRecord, acquired: &[String]) {
        for acq in acquired.iter().rev() {
            if let Err(e) = self.remove_lock_file(acq).await {
                log::error!(
                    "Rollback failed to remove lock {acq} for tx {}: {e} — \
                     registering for later cleanup",
                    tx.id
                );
                tx.add_lock(acq);
            }
        }
    }
}
