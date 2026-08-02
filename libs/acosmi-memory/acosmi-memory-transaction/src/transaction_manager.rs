// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Transaction lifecycle manager.
//!
//! Ported from `openviking/storage/transaction/transaction_manager.py`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use acosmi_memory_session::traits::FileSystem;

use crate::path_lock::PathLock;
use crate::transaction_record::{TransactionRecord, TransactionStatus};

/// Manages transaction lifecycles — creation, locking, commit, rollback,
/// and background timeout cleanup.
///
/// Unlike the Python original's global singleton, this struct is created
/// by the caller and its lifetime is explicitly managed.
pub struct TransactionManager<FS: FileSystem + Send + Sync + 'static> {
    path_lock: Arc<PathLock<FS>>,
    transactions: Arc<Mutex<HashMap<String, TransactionRecord>>>,
    timeout: Duration,
    max_parallel_locks: usize,
    cleanup_handle: Mutex<Option<JoinHandle<()>>>,
    running: Arc<tokio::sync::watch::Sender<bool>>,
}

impl<FS: FileSystem + Send + Sync + 'static> TransactionManager<FS> {
    /// Create a new `TransactionManager`.
    ///
    /// * `fs` — file-system backend for lock file operations.
    /// * `timeout` — maximum transaction age before forced rollback.
    /// * `max_parallel_locks` — max concurrent lock operations for RM/MV.
    pub fn new(fs: FS, timeout: Duration, max_parallel_locks: usize) -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        Self {
            path_lock: Arc::new(PathLock::new(fs)),
            transactions: Arc::new(Mutex::new(HashMap::new())),
            timeout,
            max_parallel_locks,
            cleanup_handle: Mutex::new(None),
            running: Arc::new(tx),
        }
    }

    /// Start the background cleanup task for timed-out transactions.
    pub async fn start(&self) {
        let mut handle = self.cleanup_handle.lock().await;
        if handle.is_some() {
            return;
        }

        // Step 2 Phase D.4 — closes Step 1 §六 R1 ② / §4.6:
        // Subscribe **before** sending the initial `running=true`
        // signal. The earlier order (`send` then `subscribe`) had no
        // receivers at the moment the signal was sent on a fresh
        // `tokio::sync::watch` channel — the send still mutates the
        // current value (so `rx.borrow()` later reads `true`), but
        // the *change notification* arrived before the spawned task
        // subscribed and was therefore consumed-by-no-one. Reordering
        // ensures the subscribed `rx` observes the initial transition
        // via `rx.changed()` if anyone ever waits on it. Even though
        // the spawned loop currently only uses `rx.changed()` for
        // shutdown, this makes the protocol correct under future
        // edits (e.g., a "wait for started" health probe).
        let txs = Arc::clone(&self.transactions);
        let path_lock = Arc::clone(&self.path_lock);
        let timeout = self.timeout;
        let mut rx = self.running.subscribe();
        let _ = self.running.send(true);

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        Self::cleanup_timed_out(&txs, &path_lock, timeout).await;
                    }
                    _ = rx.changed() => {
                        if !*rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        *handle = Some(task);
        log::info!("TransactionManager started");
    }

    /// Stop the background cleanup task.
    pub async fn stop(&self) {
        let _ = self.running.send(false);
        let mut handle = self.cleanup_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        self.transactions.lock().await.clear();
        log::info!("TransactionManager stopped");
    }

    async fn cleanup_timed_out(
        txs: &Mutex<HashMap<String, TransactionRecord>>,
        path_lock: &PathLock<FS>,
        timeout: Duration,
    ) {
        let now = chrono::Utc::now();
        let mut guard = txs.lock().await;
        let timed_out: Vec<String> = guard
            .iter()
            .filter(|(_, tx)| {
                let age = now.signed_duration_since(tx.updated_at);
                age.to_std().unwrap_or(Duration::ZERO) > timeout
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in timed_out {
            log::warn!("Transaction timed out: {id}");
            let Some(tx) = guard.get_mut(&id) else {
                continue;
            };
            tx.update_status(TransactionStatus::Fail);
            tx.update_status(TransactionStatus::Releasing);
            // Closes §九 §9.9 / §十三 HIGH-memvfs-4: previously this
            // path only did `tx.locks.clear()` (in-memory) without
            // touching the on-disk lock files, leaking PathLock state
            // forever and pinning paths to a permanently-deadlocked
            // owner. Now we actually release through PathLock.
            match path_lock.release(tx).await {
                Ok(()) => {
                    tx.update_status(TransactionStatus::Released);
                    guard.remove(&id);
                }
                Err(failed) => {
                    log::error!(
                        "cleanup_timed_out: tx {id} left {} orphan lock \
                         file(s) on disk; keeping tx record so the next \
                         cleanup tick can retry",
                        failed.len()
                    );
                    // Do NOT remove the tx record — keep it so the next
                    // tick can retry the failed lock removals.
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Transaction lifecycle
    // -----------------------------------------------------------------------

    /// Create a new transaction, returning its record.
    pub async fn create_transaction(
        &self,
        init_info: HashMap<String, serde_json::Value>,
    ) -> TransactionRecord {
        let tx = TransactionRecord::new(init_info);
        self.transactions
            .lock()
            .await
            .insert(tx.id.clone(), tx.clone());
        log::debug!("Transaction created: {}", tx.id);
        tx
    }

    /// Get a clone of a transaction record by ID.
    pub async fn get_transaction(&self, id: &str) -> Option<TransactionRecord> {
        self.transactions.lock().await.get(id).cloned()
    }

    /// Transition a transaction to the `Acquire` state.
    pub async fn begin(&self, id: &str) -> bool {
        let mut guard = self.transactions.lock().await;
        match guard.get_mut(id) {
            Some(tx) => {
                tx.update_status(TransactionStatus::Acquire);
                true
            }
            None => {
                log::error!("Transaction not found: {id}");
                false
            }
        }
    }

    /// Commit a transaction — release all locks and remove from active set.
    pub async fn commit(&self, id: &str) -> bool {
        let mut guard = self.transactions.lock().await;
        let tx = match guard.get_mut(id) {
            Some(t) => t,
            None => {
                log::error!("Transaction not found: {id}");
                return false;
            }
        };

        tx.update_status(TransactionStatus::Commit);
        tx.update_status(TransactionStatus::Releasing);
        match self.path_lock.release(tx).await {
            Ok(()) => {
                tx.update_status(TransactionStatus::Released);
                guard.remove(id);
                log::debug!("Transaction committed: {id}");
                true
            }
            Err(failed) => {
                // Commit succeeded logically; we just couldn't fully
                // unlock. Surface the leak so the orphans show up in
                // logs / metrics, and keep the tx around so cleanup can
                // retry — same shape as the rollback path.
                log::error!(
                    "Commit of tx {id} left {} orphan lock file(s) on \
                     disk; tx record retained for cleanup retry",
                    failed.len()
                );
                false
            }
        }
    }

    /// Rollback a transaction — release all locks and remove from active set.
    pub async fn rollback(&self, id: &str) -> bool {
        let mut guard = self.transactions.lock().await;
        let tx = match guard.get_mut(id) {
            Some(t) => t,
            None => {
                log::error!("Transaction not found: {id}");
                return false;
            }
        };

        tx.update_status(TransactionStatus::Fail);
        tx.update_status(TransactionStatus::Releasing);
        match self.path_lock.release(tx).await {
            Ok(()) => {
                tx.update_status(TransactionStatus::Released);
                guard.remove(id);
                log::debug!("Transaction rolled back: {id}");
                true
            }
            Err(failed) => {
                log::error!(
                    "Rollback of tx {id} left {} orphan lock file(s) on \
                     disk; tx record retained for cleanup retry",
                    failed.len()
                );
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lock acquisition
    // -----------------------------------------------------------------------

    /// Acquire a path lock for normal (non-rm/mv) operations.
    pub async fn acquire_lock_normal(&self, id: &str, path: &str) -> bool {
        let mut guard = self.transactions.lock().await;
        let tx = match guard.get_mut(id) {
            Some(t) => t,
            None => {
                log::error!("Transaction not found: {id}");
                return false;
            }
        };

        tx.update_status(TransactionStatus::Acquire);
        let success = self.path_lock.acquire_normal(path, tx).await;
        tx.update_status(if success {
            TransactionStatus::Exec
        } else {
            TransactionStatus::Fail
        });
        success
    }

    /// Acquire path locks for a recursive-delete (rm) operation.
    pub async fn acquire_lock_rm(&self, id: &str, path: &str, subdirs: &[String]) -> bool {
        let mut guard = self.transactions.lock().await;
        let tx = match guard.get_mut(id) {
            Some(t) => t,
            None => {
                log::error!("Transaction not found: {id}");
                return false;
            }
        };

        tx.update_status(TransactionStatus::Acquire);
        let success = self.path_lock.acquire_rm(path, tx, subdirs).await;
        tx.update_status(if success {
            TransactionStatus::Exec
        } else {
            TransactionStatus::Fail
        });
        success
    }

    /// Acquire path locks for a move (mv) operation.
    pub async fn acquire_lock_mv(
        &self,
        id: &str,
        src: &str,
        dst: &str,
        src_subdirs: &[String],
    ) -> bool {
        let mut guard = self.transactions.lock().await;
        let tx = match guard.get_mut(id) {
            Some(t) => t,
            None => {
                log::error!("Transaction not found: {id}");
                return false;
            }
        };

        tx.update_status(TransactionStatus::Acquire);
        let success = self.path_lock.acquire_mv(src, dst, tx, src_subdirs).await;
        tx.update_status(if success {
            TransactionStatus::Exec
        } else {
            TransactionStatus::Fail
        });
        success
    }

    // -----------------------------------------------------------------------
    // Introspection
    // -----------------------------------------------------------------------

    /// Get all active transactions (clone).
    pub async fn get_active_transactions(&self) -> HashMap<String, TransactionRecord> {
        self.transactions.lock().await.clone()
    }

    /// Get the number of active transactions.
    pub async fn transaction_count(&self) -> usize {
        self.transactions.lock().await.len()
    }

    /// Maximum parallel lock operations for RM/MV (exposed for callers).
    pub fn max_parallel_locks(&self) -> usize {
        self.max_parallel_locks
    }

    /// Test-only accessor for the `running` watch sender. Used by Phase D.4
    /// regression test to verify that subscribe-before-send leaves the
    /// channel observable to fresh subscribers via `.borrow()`.
    #[cfg(test)]
    #[must_use]
    pub fn running_for_test(&self) -> &tokio::sync::watch::Sender<bool> {
        &self.running
    }
}
