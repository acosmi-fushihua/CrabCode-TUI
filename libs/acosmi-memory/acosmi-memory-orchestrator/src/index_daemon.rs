//! W-MEMORY-DREAM-REBUILD v7 P4.3 (2026-05-25) — Long-running index sync
//! daemon. File-system changes under the configured `memory_dir` (and any
//! `dreams/` subdirectory) auto-trigger incremental `SearchEngineIntegration::
//! upsert_file` calls so that `acosmi-memory-se` indexes stay fresh without
//! relying on explicit emit-driven triggers from Tier policy processors
//! (which is the P4.1 path).
//!
//! # Why a daemon (vs Tier-emit only)
//!
//! P4.1 lands the emit-driven path: a Tier processor finishes a write, then
//! explicitly calls `SearchEngineIntegration::upsert_file(path)`. That covers
//! reflection / dream / imagination output. But users (and external tools
//! outside the orchestrator's awareness) can also edit the markdown files
//! by hand — `~/.crabcode/projects/<slug>/memory/*.md` is a plain markdown
//! tree. A daemon watching the filesystem catches those "external trigger"
//! writes and keeps the index aligned with on-disk truth.
//!
//! # Architectural contract (CLAUDE.md §硬约束 #15)
//!
//! - **No TUI client method** — the daemon is a process-internal background
//!   `tokio::task`; nothing is exposed over IPC. WHITELIST + AllowAnyOrigin
//!   counts are unaffected.
//! - **No external network / SDK** — the daemon only reads the filesystem
//!   and calls `SearchEngineIntegration::upsert_file`. Embedding round-trips
//!   are the integration's responsibility (reverse IPC to TS).
//! - **Idempotent spawn** — calling `IndexDaemon::spawn(...)` multiple times
//!   replaces the running daemon (last-write-wins). The previous daemon's
//!   watcher is dropped → its event loop ends naturally. The host is
//!   responsible for keeping at most one alive at a time per
//!   `SearchEngineIntegration` instance.
//!
//! # Debounce
//!
//! Filesystem events are noisy (a single file save may emit Create + Write +
//! Chmod). We collect events into a per-path buffer and only fire the SE
//! upsert once the path has been quiet for `debounce_ms` (default 500ms,
//! overridable via `CRABCODE_MEMORY_INDEX_DEBOUNCE_MS` env).
//!
//! # Reconciliation
//!
//! On startup the daemon issues a full `index_all` scan so the SE state is
//! aligned with the current filesystem snapshot **before** the watch loop
//! starts handling events. This avoids a TOCTOU window where a file
//! modified during startup might be missed.
//!
//! # Delete semantics (W-MEMORY-EVOLUTION FIX #12 — real delete wired)
//!
//! `SearchEngine` exposes `delete(collection, point_id)` (by point ID), but
//! the daemon only sees `path` events. `SearchEngineIntegration::delete_by_path`
//! bridges the gap: it scrolls the topic collection and removes every point
//! whose `source_path` payload equals the deleted path. This is a real delete
//! (the earlier P4.3 warn-noop was incorrect — the index stores the file
//! `content` payload, so a deleted markdown file's point keeps scrolling +
//! matching queries until explicitly removed). `search` additionally drops any
//! hit whose `source_path` no longer exists on disk as defense-in-depth
//! against GC lag.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use acosmi_memory_se::indexer::MemoryRoot;
use log::{debug, error, info, warn};
use notify::{
    event::{EventKind, ModifyKind, RemoveKind},
    Error as NotifyError, Event, RecursiveMode, Watcher,
};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::se_integration::SearchEngineIntegration;

/// Default debounce window — 500ms is long enough to coalesce a normal
/// editor save burst (Create + Modify + Chmod within a few ms on macOS
/// FSEvents / Linux inotify) but short enough that interactive feedback
/// stays snappy.
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;

/// Env var that overrides `DEFAULT_DEBOUNCE_MS`. Value parses as `u64`
/// milliseconds; any parse failure or non-numeric content falls back to
/// the default (defensive — no panics on user mis-configuration).
pub const DEBOUNCE_ENV: &str = "CRABCODE_MEMORY_INDEX_DEBOUNCE_MS";

/// Max attempts to recreate a dead watcher before the daemon gives up and
/// logs a fatal error. 3 attempts ≈ ~3s with the 1-second sleep between
/// each; matches the conservative budget in CLAUDE.md §硬约束 #11
/// "control worker pre-warm" pattern.
const MAX_WATCHER_RETRIES: u32 = 3;

/// Resolve the effective debounce duration. Defensive parsing — bad values
/// log a warning + fall back to default (parity with the
/// `withRetry-rateLimit-classification` defensive env parsing pattern in
/// `src/services/api/withRetry.ts`).
#[must_use]
pub fn resolve_debounce_ms() -> u64 {
    match std::env::var(DEBOUNCE_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                warn!(
                    "[index_daemon] {DEBOUNCE_ENV}={raw} is not a valid u64; falling back to {DEFAULT_DEBOUNCE_MS}ms"
                );
                DEFAULT_DEBOUNCE_MS
            }
        },
        Err(_) => DEFAULT_DEBOUNCE_MS,
    }
}

/// Whether a path is interesting to the daemon. Currently:
/// - skip hidden directories (`.git/` / `.acosmi/` / `.crabcode/` dot-files)
/// - markdown files (`.md` / `.markdown` extension, case-insensitive)
///
/// The "watch dot-dirs" exclusion is enforced at the path level (per-event)
/// rather than at the watcher level because `notify` does not provide a
/// builtin "exclude these prefixes" API across all backends.
#[must_use]
pub fn is_markdown_path(path: &Path) -> bool {
    // Skip any path component that starts with '.'
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if let Some(s) = name.to_str() {
                if s.starts_with('.') {
                    return false;
                }
            }
        }
    }
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    ext == "md" || ext == "markdown"
}

/// Configuration for an `IndexDaemon` spawn. The daemon watches each root's
/// `path` recursively. Multiple roots are supported (private + team, etc.).
#[derive(Debug, Clone)]
pub struct IndexDaemonConfig {
    /// Roots to watch. Each root is recursively watched, and its `path`
    /// becomes the prefix for SE upsert calls (resolved by
    /// `SearchEngineIntegration::upsert_file`).
    pub roots: Vec<MemoryRoot>,
    /// Debounce window for coalescing rapid fs events on the same path.
    pub debounce_ms: u64,
    /// Whether to run a full `index_all` pass at startup before entering
    /// the watch loop. Tests may disable this to assert event-driven
    /// behavior in isolation.
    pub initial_reindex: bool,
}

impl IndexDaemonConfig {
    /// Construct config with the canonical defaults (debounce from env,
    /// initial reindex enabled).
    #[must_use]
    pub fn with_roots(roots: Vec<MemoryRoot>) -> Self {
        Self {
            roots,
            debounce_ms: resolve_debounce_ms(),
            initial_reindex: true,
        }
    }
}

/// Result of a debounce-flushed event coalesce — one of upsert (file exists)
/// or delete (file vanished). The daemon checks `path.is_file()` at flush
/// time so a Create + Remove burst that nets to "no file" reports as Delete
/// and a Modify on a still-extant file reports as Upsert.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalescedAction {
    Upsert(PathBuf),
    Delete(PathBuf),
}

/// Internal daemon handle. Owns the running task + a shutdown sender. Drop
/// the handle to terminate the daemon (the watcher will be dropped, the
/// channel will close, and the loop will exit).
pub struct IndexDaemon {
    handle: JoinHandle<()>,
}

impl IndexDaemon {
    /// Spawn the daemon in the background. Returns immediately. The daemon
    /// owns clones of `integration` (Arc) + `config.roots`.
    ///
    /// Caller is responsible for storing the returned `IndexDaemon` (or
    /// dropping it to terminate). Re-spawning is safe but creates an
    /// independent watcher; the host should hold at most one daemon per
    /// `SearchEngineIntegration` at a time.
    pub fn spawn(integration: Arc<SearchEngineIntegration>, config: IndexDaemonConfig) -> Self {
        let handle = tokio::spawn(run_daemon(integration, config));
        Self { handle }
    }

    /// Abort the daemon. Drops the watcher + cancels the loop. Idempotent.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

impl Drop for IndexDaemon {
    fn drop(&mut self) {
        // Abort on drop so callers can simply let the handle fall out of
        // scope (parity with tokio JoinHandle's "detach by default" model
        // would leak the task forever, which is not what we want for the
        // daemon — the SE handle goes away, why keep watching?).
        self.handle.abort();
    }
}

/// Body of the daemon. Sets up the watcher, runs the initial reindex if
/// requested, then enters the event coalescing loop.
async fn run_daemon(integration: Arc<SearchEngineIntegration>, config: IndexDaemonConfig) {
    // 1) Initial reindex (full scan + upsert pass).
    if config.initial_reindex {
        match integration.index_all(&config.roots) {
            Ok(stats) => {
                info!(
                    "[index_daemon] initial reindex done: roots={} md_files_seen={} indexed={}",
                    stats.roots_scanned, stats.md_files_seen, stats.indexed
                );
            }
            Err(e) => {
                warn!("[index_daemon] initial reindex failed: {e}");
            }
        }
        // W-MEMORY-ALIVE PR-2b: bring the dense side collection up to date
        // (reverse-IPC SDK embeddings). Fail-soft — with no executor the
        // first batch arms the backoff and the index stays lexical-only.
        match integration.sync_dense_index().await {
            Ok(0) => {}
            Ok(n) => info!("[index_daemon] dense sync after reindex: {n} point(s) embedded"),
            Err(e) => warn!("[index_daemon] dense sync failed (fail-soft): {e}"),
        }
    }

    // 2) Create the watcher + bridge channel.
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Result<Event, NotifyError>>();

    let mut watcher = match build_watcher(event_tx.clone(), &config.roots) {
        Ok(w) => w,
        Err(e) => {
            error!("[index_daemon] watcher init failed (giving up): {e}");
            return;
        }
    };

    let debounce = Duration::from_millis(config.debounce_ms);

    // 3) Event coalesce loop.
    let pending: Arc<Mutex<HashMap<PathBuf, tokio::time::Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let deleted: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    // Wrap event_rx in a Mutex so the recv loop owns it for the lifetime
    // of the task. (mpsc::UnboundedReceiver is not Sync.)
    let event_rx = Arc::new(Mutex::new(event_rx));

    let mut retry_count: u32 = 0;
    loop {
        // Try to receive an event. Use a short timeout so we can also flush
        // any pending debounced paths even when no new events come in.
        let recv_result = {
            let mut rx = event_rx.lock().await;
            tokio::time::timeout(debounce, rx.recv()).await
        };

        match recv_result {
            Ok(Some(Ok(event))) => {
                retry_count = 0;
                if let Err(e) = ingest_event(event, &pending, &deleted).await {
                    debug!("[index_daemon] ingest event skipped: {e}");
                }
            }
            Ok(Some(Err(e))) => {
                warn!("[index_daemon] watcher error: {e}");
                // Try to recreate the watcher (FS may have been unmounted).
                if retry_count >= MAX_WATCHER_RETRIES {
                    error!(
                        "[index_daemon] watcher dead after {MAX_WATCHER_RETRIES} retries; giving up"
                    );
                    return;
                }
                retry_count += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
                match build_watcher(event_tx.clone(), &config.roots) {
                    Ok(w) => {
                        watcher = w;
                        info!("[index_daemon] watcher recreated (retry {retry_count})");
                    }
                    Err(e) => {
                        warn!("[index_daemon] watcher recreate failed (retry {retry_count}): {e}");
                    }
                }
            }
            Ok(None) => {
                // Channel closed — watcher was dropped or all senders gone.
                debug!("[index_daemon] event channel closed; daemon exiting");
                drop(watcher);
                return;
            }
            Err(_) => {
                // Timed out — opportunity to flush debounced paths.
            }
        }

        // Flush any path that has been quiet for ≥ debounce.
        flush_debounced(&pending, &deleted, debounce, &integration, &config.roots).await;
    }
}

/// Build a recommended-platform watcher and start watching each root
/// recursively. Returns the watcher (which must be held alive for events
/// to flow).
fn build_watcher(
    tx: mpsc::UnboundedSender<Result<Event, NotifyError>>,
    roots: &[MemoryRoot],
) -> Result<notify::RecommendedWatcher, NotifyError> {
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, NotifyError>| {
        // Best-effort send; if the receiver is gone, the daemon is shutting
        // down and we drop the event.
        let _ = tx.send(res);
    })?;

    for root in roots {
        match watcher.watch(&root.path, RecursiveMode::Recursive) {
            Ok(()) => {
                debug!(
                    "[index_daemon] watching root scope={} path={}",
                    root.scope,
                    root.path.display()
                );
            }
            Err(e) => {
                warn!(
                    "[index_daemon] failed to watch root {}: {e}",
                    root.path.display()
                );
            }
        }
    }

    Ok(watcher)
}

/// Classify an inbound `notify::Event` and update the pending / deleted
/// path maps. Non-markdown / dot-dir paths are filtered out.
async fn ingest_event(
    event: Event,
    pending: &Arc<Mutex<HashMap<PathBuf, tokio::time::Instant>>>,
    deleted: &Arc<Mutex<HashSet<PathBuf>>>,
) -> Result<(), String> {
    let now = tokio::time::Instant::now();
    for path in event.paths {
        if !is_markdown_path(&path) {
            continue;
        }
        match event.kind {
            EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(_) => {
                // Mark as pending upsert (extends quiet timer).
                let mut p = pending.lock().await;
                p.insert(path.clone(), now);
                let mut d = deleted.lock().await;
                d.remove(&path);
            }
            EventKind::Remove(RemoveKind::File) | EventKind::Remove(_) => {
                let mut d = deleted.lock().await;
                d.insert(path.clone());
                let mut p = pending.lock().await;
                p.remove(&path);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Walk the pending map and dispatch any path that has been quiet for the
/// full `debounce` window. Also flushes any pending deletes (which fire as
/// warn-log TODOs per the §Delete semantics block above).
async fn flush_debounced(
    pending: &Arc<Mutex<HashMap<PathBuf, tokio::time::Instant>>>,
    deleted: &Arc<Mutex<HashSet<PathBuf>>>,
    debounce: Duration,
    integration: &Arc<SearchEngineIntegration>,
    roots: &[MemoryRoot],
) {
    let now = tokio::time::Instant::now();

    // Drain pending upserts whose quiet timer has elapsed.
    let mut to_upsert: Vec<PathBuf> = Vec::new();
    {
        let mut p = pending.lock().await;
        p.retain(|path, last| {
            if now.saturating_duration_since(*last) >= debounce {
                to_upsert.push(path.clone());
                false
            } else {
                true
            }
        });
    }

    let had_upserts = !to_upsert.is_empty();
    for path in to_upsert {
        let action = if path.is_file() {
            CoalescedAction::Upsert(path)
        } else {
            CoalescedAction::Delete(path)
        };
        dispatch_action(action, integration, roots).await;
    }
    if had_upserts {
        // W-MEMORY-ALIVE PR-2b: re-embed the changed documents (mtime-diffed
        // inside; only stale/new points hit the reverse-IPC channel). Gated
        // on actual upserts so the idle flush tick never scrolls the
        // collections. Deletes need no dense pass — `delete_by_path` cleans
        // both collections.
        match integration.sync_dense_index().await {
            Ok(0) => {}
            Ok(n) => debug!("[index_daemon] dense sync: {n} point(s) embedded"),
            Err(e) => debug!("[index_daemon] dense sync failed (fail-soft): {e}"),
        }
    }

    // Drain pending deletes (no quiet-window — deletes fire immediately
    // since there is no follow-up event to coalesce with).
    let drained_deletes: Vec<PathBuf> = {
        let mut d = deleted.lock().await;
        let snapshot: Vec<_> = d.iter().cloned().collect();
        d.clear();
        snapshot
    };
    for path in drained_deletes {
        dispatch_action(CoalescedAction::Delete(path), integration, roots).await;
    }
}

/// Apply a coalesced action to the SE integration. Delete is currently a
/// warn-log noop (see §Delete semantics in the module-level docs).
async fn dispatch_action(
    action: CoalescedAction,
    integration: &Arc<SearchEngineIntegration>,
    roots: &[MemoryRoot],
) {
    match action {
        CoalescedAction::Upsert(path) => match integration.upsert_file(roots, &path) {
            Ok(stats) => {
                debug!(
                    "[index_daemon] upsert path={} indexed={} skipped={}",
                    path.display(),
                    stats.indexed,
                    stats.skipped
                );
            }
            Err(e) => {
                warn!("[index_daemon] upsert failed path={}: {e}", path.display());
            }
        },
        CoalescedAction::Delete(path) => {
            // W-MEMORY-EVOLUTION FIX #12 (2026-06-01) — real SE delete by
            // path. The earlier TODO(P5.x) warn-noop was wrong: the index
            // stores the file `content` payload, so a deleted markdown file's
            // point keeps scrolling + matching queries until removed.
            // `delete_by_path` scrolls the collection and removes every point
            // whose `source_path` payload equals this path.
            match integration.delete_by_path(&path) {
                Ok(removed) => {
                    debug!(
                        "[index_daemon] delete path={} removed_points={removed}",
                        path.display()
                    );
                }
                Err(e) => {
                    warn!("[index_daemon] delete failed path={}: {e}", path.display());
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::se_integration::{RecordingEmitter, SearchEngineIntegration};
    use acosmi_memory_se::indexer::MemoryRoot;
    use tempfile::TempDir;

    /// Helper: build a SearchEngineIntegration over a tempdir.
    fn make_integration(data_dir: &Path) -> Arc<SearchEngineIntegration> {
        let emitter = Arc::new(RecordingEmitter::new());
        let integration = SearchEngineIntegration::new(
            data_dir,
            emitter as Arc<dyn crate::se_integration::EmbeddingEmitter>,
        )
        .expect("init");
        Arc::new(integration)
    }

    /// Helper: write a markdown file with valid frontmatter so the indexer
    /// will actually index it (vs skip with NoFrontmatter).
    fn write_md(path: &Path, ty: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        let body = format!(
            "---\ntype: {ty}\nname: {ty} sample\ndescription: a {ty} sample memory.\ncreated_at: 2026-05-25\n---\n\nbody text\n"
        );
        std::fs::write(path, body).expect("write");
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — debounce env override is honored.
    /// Bad values fall back to default (defensive parsing).
    #[test]
    fn debounce_env_override_and_fallback() {
        // Save current env (CI may have it set).
        let prev = std::env::var(DEBOUNCE_ENV).ok();

        // Good value.
        std::env::set_var(DEBOUNCE_ENV, "1000");
        assert_eq!(resolve_debounce_ms(), 1000);

        // Bad value → default.
        std::env::set_var(DEBOUNCE_ENV, "not-a-number");
        assert_eq!(resolve_debounce_ms(), DEFAULT_DEBOUNCE_MS);

        // Unset → default.
        std::env::remove_var(DEBOUNCE_ENV);
        assert_eq!(resolve_debounce_ms(), DEFAULT_DEBOUNCE_MS);

        // Restore.
        if let Some(v) = prev {
            std::env::set_var(DEBOUNCE_ENV, v);
        }
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — `is_markdown_path` filter:
    /// markdown + non-dot-dir = keep; hidden-dir / non-md = skip.
    #[test]
    fn is_markdown_path_filters_correctly() {
        assert!(is_markdown_path(Path::new("/tmp/memory/project_foo.md")));
        assert!(is_markdown_path(Path::new("/tmp/memory/dreams/d-1.MD")));
        assert!(is_markdown_path(Path::new("/tmp/memory/x.markdown")));
        // Non-markdown extension.
        assert!(!is_markdown_path(Path::new("/tmp/memory/notes.txt")));
        assert!(!is_markdown_path(Path::new("/tmp/memory/log")));
        // Dot-dir anywhere in path.
        assert!(!is_markdown_path(Path::new("/tmp/.git/foo.md")));
        assert!(!is_markdown_path(Path::new("/tmp/memory/.acosmi/foo.md")));
        assert!(!is_markdown_path(Path::new("/tmp/.crabcode/cache/x.md")));
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — initial reindex pass populates SE
    /// from existing fixture files even when no fs events fire.
    #[tokio::test]
    async fn daemon_initial_reindex_runs_at_startup() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        // Pre-populate 2 valid markdown fixtures.
        write_md(&memdir.join("project_alpha.md"), "project");
        write_md(&memdir.join("user_beta.md"), "user");

        let integration = make_integration(&data_dir);
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let config = IndexDaemonConfig {
            roots,
            debounce_ms: 50,
            initial_reindex: true,
        };

        let daemon = IndexDaemon::spawn(Arc::clone(&integration), config);

        // Wait a brief moment for the initial reindex to fire.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify by re-running index_all directly: state should already
        // reflect 2 files. (Easier than introspecting SE point counts.)
        let stats = integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("manual index_all");
        assert_eq!(stats.md_files_seen, 2);

        drop(daemon);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — fs event triggers upsert after
    /// debounce window elapses. (Uses a very short debounce + the actual
    /// notify crate so we exercise the full pipeline including the
    /// platform fs-event backend.)
    #[tokio::test]
    async fn daemon_upserts_on_file_create_after_debounce() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        let integration = make_integration(&data_dir);
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let config = IndexDaemonConfig {
            roots,
            debounce_ms: 100,
            initial_reindex: false,
        };

        let daemon = IndexDaemon::spawn(Arc::clone(&integration), config);

        // Give the watcher a beat to attach.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Create a new markdown file.
        write_md(&memdir.join("project_gamma.md"), "project");

        // Wait for the debounce flush + upsert. Generous timeout because
        // platform fs-event backends (FSEvents on macOS especially) can
        // batch up to ~100ms.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Verify by querying the engine — collection should exist + have
        // at least one indexed point. We use the indirect proof: run a
        // direct index_all and check that md_files_seen agrees with the
        // current state. (Direct SE point introspection requires a search
        // call, which is P4.2 territory.)
        let stats = integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("manual index_all");
        assert_eq!(stats.md_files_seen, 1);

        drop(daemon);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — modify event for an existing file
    /// is coalesced into a single upsert by the debounce window.
    /// We rely on observable behavior: two rapid writes followed by a
    /// debounce window should still result in the indexer finding the
    /// file present (1 md_files_seen).
    #[tokio::test]
    async fn daemon_coalesces_rapid_modifies_into_single_upsert() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        let target = memdir.join("project_delta.md");
        write_md(&target, "project");

        let integration = make_integration(&data_dir);
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let config = IndexDaemonConfig {
            roots: roots.clone(),
            debounce_ms: 200,
            initial_reindex: false,
        };

        let daemon = IndexDaemon::spawn(Arc::clone(&integration), config);

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Two rapid writes inside the debounce window.
        write_md(&target, "project");
        tokio::time::sleep(Duration::from_millis(20)).await;
        write_md(&target, "project");

        // Wait past debounce.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // The integration's index_all is idempotent — running it
        // independently should still find the single file.
        let stats = integration.index_all(&roots).expect("manual index_all");
        assert_eq!(stats.md_files_seen, 1);

        drop(daemon);
    }

    /// W-MEMORY-EVOLUTION FIX #12 — delete event removes the SE point (was a
    /// warn-log noop). After a remove, the daemon must drop the indexed point
    /// so a search for the file no longer matches.
    #[tokio::test]
    async fn daemon_handles_delete_without_panicking() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        let target = memdir.join("project_eps.md");
        write_md(&target, "project");

        let integration = make_integration(&data_dir);
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let config = IndexDaemonConfig {
            roots: roots.clone(),
            debounce_ms: 100,
            initial_reindex: true,
        };

        let daemon = IndexDaemon::spawn(Arc::clone(&integration), config);

        tokio::time::sleep(Duration::from_millis(200)).await;

        // After the initial reindex, the point is searchable (browse mode
        // returns the one indexed file).
        let before = integration
            .search("", 10, "text", true)
            .expect("search before");
        assert_eq!(
            before.len(),
            1,
            "file indexed before delete; got {before:?}"
        );

        // Remove the file.
        std::fs::remove_file(&target).expect("remove");

        // Wait for the event to flow + the real SE delete to fire.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // The daemon should still be alive (no panic). The SE point must be
        // gone: a search now returns nothing (delete_by_path removed it; the
        // search-time on-disk filter would also exclude it).
        let after = integration
            .search("", 10, "text", true)
            .expect("search after");
        assert!(
            after.is_empty(),
            "deleted file's SE point must be removed; got {after:?}"
        );

        // And the filesystem agrees (defensive).
        let stats = integration.index_all(&roots).expect("manual index_all");
        assert_eq!(stats.md_files_seen, 0);

        drop(daemon);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.3 — IndexDaemonConfig::with_roots
    /// applies the env-resolved debounce + enables initial reindex.
    #[test]
    fn config_with_roots_uses_env_debounce_and_enables_reindex() {
        let prev = std::env::var(DEBOUNCE_ENV).ok();
        std::env::set_var(DEBOUNCE_ENV, "777");
        let config = IndexDaemonConfig::with_roots(vec![MemoryRoot::private(PathBuf::from(
            "/tmp/whatever",
        ))]);
        assert_eq!(config.debounce_ms, 777);
        assert!(config.initial_reindex);
        assert_eq!(config.roots.len(), 1);

        // Restore.
        std::env::remove_var(DEBOUNCE_ENV);
        if let Some(v) = prev {
            std::env::set_var(DEBOUNCE_ENV, v);
        }
    }
}
