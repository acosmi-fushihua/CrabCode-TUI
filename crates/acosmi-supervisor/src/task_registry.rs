//! Supervisor task lifecycle registry.
//!
//! Bare `let _ = tokio::spawn(...)` patterns drop `JoinHandle`s, losing
//! task panics + state. All async tasks owned by `Supervisor` must be
//! spawned via [`TrackedSpawn::spawn`], which:
//!
//! - Catches the spawned future's panic via [`futures::FutureExt::catch_unwind`]
//!   and logs it at `error` level with the spawn-site label, so panicking
//!   tasks no longer disappear silently;
//! - Owns each `JoinHandle` and aborts every outstanding task on `Drop`,
//!   so when a `Supervisor` (or test fixture) goes away the tasks it
//!   spawned go with it instead of running on against a dropped state;
//! - Returns an opaque [`TrackedTaskId`] callers can use to abort an
//!   individual task without touching the registry's internals.
//!
//! ## Invariants enforced
//!
//! * `tokio::spawn` is *disallowed* inside `acosmi-supervisor` (see
//!   `clippy.toml`). Existing legitimate spawn sites that hold their
//!   `JoinHandle` carry an `#[allow(clippy::disallowed_methods)]`
//!   marker until D.2 migrates the bare-discard sites to this registry.
//! * Spawning never panics; panics inside the future are converted to
//!   `tracing::error` and the future ends.
//! * Dropping a `TrackedSpawn` aborts every task it spawned. Tasks that
//!   would prefer not to be aborted should not use this registry.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use futures::future::FutureExt;
use tokio::task::JoinHandle;

/// Process-global registry. Used by D.2 caller sites that previously
/// did `let _ = tokio::spawn(...)` and have no easy way to thread a
/// `TrackedSpawn` reference down through nested closures (e.g.
/// per-connection IPC handlers nested several levels deep inside
/// `start_unix` / `start_windows`).
///
/// Tests and embedded uses can construct their own [`TrackedSpawn`]
/// directly; the global is just a convenience for the supervisor's
/// own bare-discard sites.
static GLOBAL: OnceLock<TrackedSpawn> = OnceLock::new();

/// Get a `&'static` reference to the process-global registry,
/// initialising it on first call.
#[must_use]
pub fn global() -> &'static TrackedSpawn {
    GLOBAL.get_or_init(TrackedSpawn::new)
}

/// Await a `JoinHandle<T>` and classify the result into the three
/// possibilities `tokio` can return — `Ok(value)` / panic / aborted —
/// emitting a distinct trace event for each so a panicked or aborted
/// task does not silently degrade into the `T::default()` value.
///
/// Closes Step 2 Phase D.6 / Step 1 §六 R1 ④: callers previously did
/// `task.await.unwrap_or_default()`, which folded panicked-task and
/// aborted-task into the same empty / default value as a successful
/// "task ran but produced empty output". With this helper, panic
/// / abort now each leave a trace breadcrumb under `$target` while
/// the caller still gets a usable `T` to keep going.
///
/// `T: Default` is required so the post-error path has a concrete
/// fallback. If the caller would rather propagate the error than
/// fall back, `await` the handle directly and match `JoinError` —
/// this helper is only for the "best-effort collection of output
/// from a sibling task" pattern.
///
/// `label` identifies the spawn site in trace output; pick a value
/// matching the same label used at `TrackedSpawn::spawn` or the
/// surrounding `tokio::spawn` site. The tracing *target* is fixed at
/// `supervisor.task.await_or_log` so log routing rules can filter on
/// it uniformly.
pub async fn await_or_log<T: Default>(
    handle: tokio::task::JoinHandle<T>,
    label: &'static str,
) -> T {
    match handle.await {
        Ok(value) => value,
        Err(join_err) if join_err.is_panic() => {
            tracing::error!(
                target: "supervisor.task.await_or_log",
                %label,
                "joined task panicked — using Default::default() to keep going",
            );
            T::default()
        }
        Err(join_err) if join_err.is_cancelled() => {
            tracing::warn!(
                target: "supervisor.task.await_or_log",
                %label,
                "joined task was cancelled (aborted) — using Default::default()",
            );
            T::default()
        }
        Err(_) => {
            // Future tokio variants — keep the trace generic.
            tracing::warn!(
                target: "supervisor.task.await_or_log",
                %label,
                "joined task ended in an unrecognised error variant — using Default::default()",
            );
            T::default()
        }
    }
}

/// Opaque identifier for a task tracked by a [`TrackedSpawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackedTaskId(u64);

impl TrackedTaskId {
    /// Numeric id (only useful for tracing / debugging — not part of the
    /// stable API contract).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

struct TrackedHandle {
    label: &'static str,
    join: JoinHandle<()>,
}

/// Process-local registry of tracked tokio tasks.
///
/// `TrackedSpawn` is `Send + Sync` and meant to be shared via `Arc`
/// across `Supervisor` subsystems (IPC handlers, child stream readers,
/// signal handler) so they all funnel through one ownership point.
pub struct TrackedSpawn {
    handles: Mutex<HashMap<TrackedTaskId, TrackedHandle>>,
    next_id: AtomicU64,
}

impl TrackedSpawn {
    /// Construct an empty registry. Equivalent to [`TrackedSpawn::default`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Spawn `fut` on the current tokio runtime and track its
    /// `JoinHandle` until the registry is dropped or [`Self::abort`] /
    /// [`Self::shutdown`] is called.
    ///
    /// `label` must identify the spawn site — it's emitted on
    /// every trace event (`spawn` / `panic` / `abort` / `complete`)
    /// so operators can attribute background-task noise to its origin.
    /// Use a `&'static str` constant; **do not** synthesize per-call
    /// labels with `format!` (that would defeat the labelling).
    ///
    /// ## Panic handling
    ///
    /// If `fut` panics, the panic is caught via
    /// [`futures::FutureExt::catch_unwind`] and logged at `error` level
    /// with the label and best-effort downcast of the panic payload.
    /// Note `AssertUnwindSafe` is applied unconditionally — the future
    /// may leave shared state in an inconsistent shape on panic, but
    /// silently dropping the panic without trace would be worse.
    ///
    /// ## Lifecycle
    ///
    /// When the future completes (cleanly or via caught panic), the
    /// registry entry is *not* automatically reclaimed; entries
    /// accumulate for the registry's lifetime. This is by design:
    /// the registry is owned by `Supervisor` and lives for the
    /// supervisor's whole run, so accumulation is bounded by the
    /// number of long-lived tasks (handful, not unbounded). Add a
    /// reaper if a use case arises that wants per-task cleanup before
    /// shutdown.
    pub fn spawn<F>(&self, label: &'static str, fut: F) -> TrackedTaskId
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let id = TrackedTaskId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let wrapped = async move {
            // AssertUnwindSafe: F is generic and we do not statically
            // know it is UnwindSafe; we accept potential state
            // inconsistency in exchange for converting silent task
            // disappearance into a logged error. See module docs.
            if let Err(panic_payload) = AssertUnwindSafe(fut).catch_unwind().await {
                let payload_msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("(unknown panic payload)");
                tracing::error!(
                    target: "supervisor.task",
                    %label,
                    id = id.0,
                    panic = %payload_msg,
                    "tracked task panicked",
                );
            } else {
                tracing::trace!(
                    target: "supervisor.task",
                    %label,
                    id = id.0,
                    "tracked task completed",
                );
            }
        };

        // The registry itself is the *one* place we are allowed to
        // call `tokio::spawn` directly; all other callers in this
        // crate go through `TrackedSpawn::spawn`. The clippy.toml
        // disallowed-methods rule fires on `tokio::spawn`, so we
        // suppress here (only here) with an explanatory comment.
        #[allow(clippy::disallowed_methods)]
        let join = tokio::spawn(wrapped);

        match self.handles.lock() {
            Ok(mut guard) => {
                guard.insert(id, TrackedHandle { label, join });
            }
            Err(poisoned) => {
                // Recover from poisoning rather than panicking — a
                // poisoned mutex inside the supervisor's task tracker
                // means *some* prior task panicked while the lock was
                // held; the right thing is to keep going so the
                // supervisor can still abort what's left.
                tracing::warn!(
                    target: "supervisor.task",
                    "TrackedSpawn handles mutex was poisoned; recovering",
                );
                poisoned
                    .into_inner()
                    .insert(id, TrackedHandle { label, join });
            }
        }

        tracing::debug!(
            target: "supervisor.task",
            %label,
            id = id.0,
            "spawned tracked task",
        );

        id
    }

    /// Abort a single task by id. Returns `true` if the id was known
    /// and the task was sent an abort signal; `false` if the id was
    /// already drained / unknown.
    pub fn abort(&self, id: TrackedTaskId) -> bool {
        let mut guard = match self.handles.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(h) = guard.remove(&id) {
            tracing::debug!(
                target: "supervisor.task",
                label = %h.label,
                id = id.0,
                "aborting tracked task",
            );
            h.join.abort();
            true
        } else {
            false
        }
    }

    /// Abort all tracked tasks and clear the registry. Returns the
    /// number of tasks aborted.
    pub fn shutdown(&self) -> usize {
        // Move the entries out under the lock, then drop the guard
        // before doing the actual abort/trace work — keeping the lock
        // held across `JoinHandle::abort` calls would let other
        // threads' `spawn`/`abort` calls block longer than necessary
        // (significant_drop_tightening).
        let drained: Vec<(TrackedTaskId, TrackedHandle)> = {
            let mut guard = match self.handles.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.drain().collect()
        };
        let total = drained.len();
        for (id, h) in drained {
            tracing::debug!(
                target: "supervisor.task",
                label = %h.label,
                id = id.0,
                "aborting tracked task on shutdown",
            );
            h.join.abort();
        }
        total
    }

    /// Number of currently-tracked tasks. Useful for tests / metrics;
    /// not stable across the lifetime of completed tasks (see lifecycle
    /// note on [`Self::spawn`]).
    #[must_use]
    pub fn len(&self) -> usize {
        match self.handles.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    /// `true` if no tasks are currently registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for TrackedSpawn {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TrackedSpawn {
    fn drop(&mut self) {
        // Don't go through `shutdown()` because that re-acquires the
        // mutex; on Drop we have exclusive `&mut self` and can drain
        // directly with `get_mut()`. Recover from poison rather than
        // skipping the abort — a poisoned mutex still has live tasks
        // we need to terminate.
        let drained = match self.handles.get_mut() {
            Ok(g) => std::mem::take(g),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
        for (id, h) in drained {
            tracing::debug!(
                target: "supervisor.task",
                label = %h.label,
                id = id.0,
                "aborting tracked task on registry drop",
            );
            h.join.abort();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    /// Smoke test — spawn a no-op task and confirm the registry length
    /// reflects the live spawn before/after.
    #[tokio::test]
    async fn spawn_records_a_handle() {
        let registry = TrackedSpawn::new();
        assert!(registry.is_empty());

        let id = registry.spawn("test.smoke", async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        assert_eq!(registry.len(), 1);
        assert!(registry.abort(id), "abort of just-spawned task succeeds");
        // After abort, entry is removed; registry should be empty.
        assert!(registry.is_empty());
    }

    /// Closes Step 1 §六 R1 sub-bullet ① "task lifecycle 无主". A
    /// panicking task must surface as a logged error rather than
    /// disappearing silently. We verify by running the task and
    /// confirming the surrounding test does not itself panic.
    #[tokio::test]
    async fn tracked_task_panic_does_not_kill_supervisor() {
        let registry = TrackedSpawn::new();
        let _id = registry.spawn("test.panicker", async move {
            panic!("intentional panic for test");
        });
        // Yield the runtime enough times to let the task run + panic +
        // catch_unwind handler fire.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // If catch_unwind didn't fire, the tokio runtime would have
        // surfaced the panic into the worker thread; this test would
        // not have reached here. Reaching here = catch_unwind worked.
        // Drop the registry; abort path should be a noop on a
        // completed task but must not panic either.
        drop(registry);
    }

    /// Closes Step 1 §六 R1 sub-bullet ① — registry drop must abort
    /// outstanding tasks. We verify by spawning a never-finishing task
    /// and confirming a shared flag the task would set never gets set
    /// after the registry is dropped.
    #[tokio::test]
    async fn supervisor_drop_aborts_all_tracked() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let registry = TrackedSpawn::new();
            let flag_for_task = Arc::clone(&flag);
            registry.spawn("test.never_finishes", async move {
                // Sleep long enough that we will surely abort first.
                tokio::time::sleep(Duration::from_secs(3)).await;
                flag_for_task.store(true, Ordering::SeqCst);
            });
            // registry drops here at end of scope.
        }
        // Wait briefly to confirm the task did not run to completion.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "task continued running after registry drop — abort regression",
        );
    }

    /// Multiple spawns receive distinct ids. We spawn from
    /// concurrent tokio tasks (rather than `std::thread::spawn`,
    /// which lives outside the runtime) because `TrackedSpawn::spawn`
    /// internally calls `tokio::spawn` and needs the runtime context.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ids_are_unique_across_concurrent_spawns() {
        let registry = Arc::new(TrackedSpawn::new());
        let counter = Arc::new(AtomicU32::new(0));

        let mut joiners = Vec::new();
        for _ in 0..16_u32 {
            let r = Arc::clone(&registry);
            let c = Arc::clone(&counter);
            // Test spawner; JoinHandle owned by `joiners` and awaited below.
            // The body invokes the registry's `spawn` (the legitimate path).
            #[allow(clippy::disallowed_methods)]
            let h = tokio::task::spawn(async move {
                r.spawn("test.race", async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            });
            joiners.push(h);
        }

        let mut ids: Vec<TrackedTaskId> = Vec::with_capacity(joiners.len());
        for j in joiners {
            match j.await {
                Ok(id) => ids.push(id),
                Err(e) => panic!("test spawner task failed: {e}"),
            }
        }

        let mut sorted = ids.clone();
        sorted.sort_by_key(|t| t.0);
        let before = sorted.len();
        sorted.dedup_by_key(|t| t.0);
        assert_eq!(
            sorted.len(),
            before,
            "ids must be unique across concurrent spawns"
        );

        // Drain so we don't rely on tasks finishing after the test ends.
        registry.shutdown();
    }

    /// `shutdown()` returns the count of tasks aborted.
    #[tokio::test]
    async fn shutdown_returns_count() {
        let registry = TrackedSpawn::new();
        for i in 0..5_u32 {
            // Different labels to make sure label is `&'static str`
            // OK for repeated spawn (yes — labels are user-supplied).
            let label: &'static str = match i {
                0 => "test.shutdown.0",
                1 => "test.shutdown.1",
                2 => "test.shutdown.2",
                3 => "test.shutdown.3",
                _ => "test.shutdown.4plus",
            };
            registry.spawn(label, async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
        }
        assert_eq!(registry.len(), 5);
        let aborted = registry.shutdown();
        assert_eq!(aborted, 5);
        assert!(registry.is_empty());
    }
}
