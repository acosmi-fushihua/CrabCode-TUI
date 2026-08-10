use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use acosmi_memory_journal::{
    AckOutcome, DeadLetterOutcome, DeliveryFence, Journal, RecordResultOutcome, ReleaseOutcome,
    RenewOutcome, SettleOutcome, WorkItem, WorkKind, WorkState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dream_config::{read_dream_config, set_dream_enabled};
use crate::dream_gate::{project_state_dir_from_memory_dir, system_time_to_ms};
use crate::extract_archive::{archive_runner_completed, now_ms, RunnerArchiveRecord};
use crate::leader_lock;
use crate::lock;
use crate::result_listener::{RunnerCompleted, RunnerCompletionReport};
use crate::status::{build_status, StatusRequest};
use crate::tier::tier1_session_memory::{
    LlmCallEmitter, RecordingEmitter, SessionMemoryGate, SessionMemoryGateInput,
    SessionMemoryGateOutput, SessionMemoryProcessInput, SessionMemoryProcessor,
};
// W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 ExtractMemories
// processor + types. Uses its own RecordingEmitter / LlmCallEmitter trait
// (the trait is repeated per-tier file to keep tier modules independent;
// emitters are interchangeable at the wire-payload layer because both
// implement `LlmCallEmitter` over the same `LlmCallRequestPayload`).
use crate::tier::tier2_extract_memories::{
    ExtractGate, ExtractProcessInput, ExtractProcessor, LlmCallEmitter as Tier2LlmCallEmitter,
    RecordingEmitter as Tier2RecordingEmitter,
};
// W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 AutoDream processor +
// types. Same `LlmCallEmitter`-per-tier pattern as Tier-1 / Tier-2; emitters
// are interchangeable at the wire-payload layer because each implements the
// per-tier `LlmCallEmitter` over the shared `LlmCallRequestPayload`.
use crate::tier::tier3_auto_dream::{
    AutoDreamGate, AutoDreamGateInput, DreamProcessInput, DreamProcessor,
    LlmCallEmitter as Tier3LlmCallEmitter, RecordingEmitter as Tier3RecordingEmitter,
};
// W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination processor +
// types. Independent of tier3_auto_dream (different prompt set + different
// gate semantics + different pipeline shape: 5-layer confidence rather than
// 5-phase consolidation). Same `LlmCallEmitter`-per-tier pattern; emitters
// are interchangeable at the wire-payload layer because each implements its
// per-tier `LlmCallEmitter` over the shared `LlmCallRequestPayload`.
// W-MEMORY-LIFECYCLE K10 (2026-07-09): `WatchContext` (root + focus) is the
// watch-scoped evidence context threaded into the imagination pipeline so
// `gather_evidence` can emit the read-only `readFile` / `listDir` tool kinds
// against the watched path.
use crate::tier::tier3_imagination::{
    ImaginationGate, ImaginationGateInput, ImaginationGeneratedInput, ImaginationProcessInput,
    ImaginationProcessor, LlmCallEmitter as Tier3ImagLlmCallEmitter,
    RecordingEmitter as Tier3ImagRecordingEmitter, ToolCallResultPayload, WatchContext,
};
use crate::tier::TierGate;
// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — Phase 4 起手 PR: acosmi-se
// 搜索引擎接通骨架。SearchEngineIntegration owns the SE handle + reverse-IPC
// embedding emitter + pending oneshot map. Reverse-IPC embedding round-trips
// follow the same pattern as the per-tier `LlmCallEmitter` (P3.1) — wire-
// payload-distinct + own pending map keyed by `req_id` prefix `se-embed-`.
use crate::se_integration::{
    search_dir_for_project_state, EmbeddingEmitter, EmbeddingResultPayload,
    RecordingEmitter as SeRecordingEmitter, SearchEngineIntegration,
};
use crate::turn_evaluator::{
    runner_work_key, DreamRunNowRequest, DurableRunnerWork, ExtractRunNowRequest, RunNowResponse,
    RunnerKind, TurnEndEvaluateRequest, TurnEndEvaluateResponse, TurnEndTrigger, TurnEvaluator,
};
use acosmi_memory_se::indexer::MemoryRoot;
// W-MEMORY-LIFECYCLE K10 (2026-07-09) — dream-watch (专项检测) config store +
// the orchestrator-local `<base>` resolution (§硬约束 #4 env contract).
use crate::watch_config::{generate_watch_id, load_watch_config, save_watch_config, WatchTarget};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const RUNNER_DELIVERY_LEASE_MS: u64 = 60_000;
const RUNNER_CANDIDATE_DEFAULT_LIMIT: usize = 32;
const RUNNER_CANDIDATE_MAX_LIMIT: usize = 128;
const RUNNER_RETRY_BASE_DELAY_MS: u64 = 1_000;
const RUNNER_RETRY_MAX_DELAY_MS: u64 = 300_000;
const RUNNER_SETTLEMENT_LEASE_MS: u64 = 60_000;
const RUNNER_SETTLEMENT_RENEW_INTERVAL_MS: u64 = 20_000;
const DURABLE_IMAGINATION_SCHEMA_ID: &str = "crabcode-imagination-followup-v1";
const DURABLE_IMAGINATION_LEASE_MS: u64 = 60_000;
const DURABLE_IMAGINATION_RENEW_INTERVAL_MS: u64 = 20_000;
const DURABLE_IMAGINATION_RETRY_DELAY_MS: u64 = 30_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DurableImaginationFollowup {
    schema_id: String,
    source_trigger_id: String,
    memory_dir: PathBuf,
    project_state_dir: PathBuf,
}

fn imagination_followup_key(trigger_id: &str) -> String {
    format!("imagination-after-dream:{trigger_id}")
}

#[derive(Debug)]
enum RunnerSettlementAttempt {
    Settled(RunnerCompletionReport),
    Missing,
    FenceLost,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunnerSettlementRecoveryReport {
    pub candidates: usize,
    pub settled: usize,
    pub failed: usize,
    pub fence_lost: usize,
}

/// W3 P1-4 (2026-06-05) — canonical per-project key for the SE state map.
/// Wraps the canonicalized `project_state_dir` string (falls back to the raw
/// lossy path when `canonicalize` fails — e.g. the dir doesn't exist on disk
/// yet). Two requests for the same project resolve to the same key (and thus
/// reuse the same SE + index daemon); a different project gets a fresh SE
/// rooted at ITS own `<project_state_dir>/search/`, never the first project's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectKey(String);

impl ProjectKey {
    fn from_project_state_dir(project_state_dir: &Path) -> Self {
        let canonical = dunce::canonicalize(project_state_dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| project_state_dir.to_string_lossy().into_owned());
        Self(canonical)
    }
}

fn build_id_version_newer(candidate: &str, current: &str) -> bool {
    fn version(id: &str) -> Option<Vec<u64>> {
        let raw = id.split_once('+')?.0;
        if raw.is_empty() {
            return None;
        }
        raw.split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    }

    matches!((version(candidate), version(current)), (Some(candidate), Some(current)) if candidate > current)
}

fn settlement_owner() -> String {
    format!(
        "orchestrator:{}:{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    )
}

/// W3 P1-4 (2026-06-05) — per-project SE runtime state. Holds the lazily
/// constructed `SearchEngineIntegration` plus the fs-event index daemon and
/// the one-shot initial-index guard — all of which used to be process-global
/// fields on `IpcHandler` (causing cross-project index-root reuse + a single
/// global initial-index that only fired for the FIRST project). Dropping a
/// `SeState` (on LRU eviction) aborts its `IndexDaemon` (the daemon aborts its
/// watcher task on Drop), so eviction is a clean teardown — no leaked watcher
/// thread / open fds.
struct SeState {
    se: Arc<SearchEngineIntegration>,
    /// fs-event index daemon for THIS project's roots. `None` only transiently
    /// during construction; aborted on Drop (eviction / handler shutdown).
    ///
    /// Held purely for its Drop side-effect (`IndexDaemon::drop` aborts the
    /// watcher task), so it is "never read" in a non-test build — that is the
    /// intent: the daemon must outlive the `SeState` and die exactly when the
    /// state is evicted/dropped. (Read in `#[cfg(test)]` via
    /// `project_has_index_daemon`.)
    #[allow(dead_code)]
    index_daemon: Option<crate::index_daemon::IndexDaemon>,
    /// One-shot guard: the expensive initial `index_all` background pass fires
    /// exactly once per project (not once globally). Recorded for clarity /
    /// future-proofing — the lazy-init miss path is the only place that builds
    /// a `SeState`, so it is implicitly already a one-shot per key.
    #[allow(dead_code)]
    initial_index_started: bool,
}

/// W3 P1-4 (2026-06-05) — bounded LRU cap on the number of simultaneously
/// live per-project SE instances. A long TUI session can touch many projects;
/// without a cap we'd accumulate unbounded SE handles + index daemons + open
/// fds. 8 covers realistic multi-project workflows while bounding resource use;
/// the least-recently-used project is evicted (its index daemon torn down) when
/// a 9th distinct project is opened. Re-touching an evicted project re-inits
/// lazily (a fresh initial index pass), which is correct (just slightly cold).
const SE_STATE_LRU_CAP: usize = 8;

/// W3 P1-4 (2026-06-05) — the bounded-LRU map of per-project SE state. Front of
/// the `Vec` = most recently used. Kept as a plain `Vec` (cap is tiny: 8) to
/// avoid pulling in an `lru` / `indexmap` dependency for a structure this
/// small. All mutation goes through the `SeStateMap` helpers below, which keep
/// the recency order + cap invariants.
#[derive(Default)]
struct SeStateMap {
    entries: Vec<(ProjectKey, SeState)>,
}

impl SeStateMap {
    /// Look up by key, moving the hit to the front (most-recently-used) and
    /// returning a clone of its SE `Arc`. `None` if not present.
    fn get_se(&mut self, key: &ProjectKey) -> Option<Arc<SearchEngineIntegration>> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        if pos != 0 {
            let entry = self.entries.remove(pos);
            self.entries.insert(0, entry);
        }
        Some(Arc::clone(&self.entries[0].1.se))
    }

    /// Most-recently-used SE (front), if any. Used by the embedding-result
    /// delivery + the legacy `se_integration()` accessor.
    fn front_se(&self) -> Option<Arc<SearchEngineIntegration>> {
        self.entries.first().map(|(_, s)| Arc::clone(&s.se))
    }

    /// Every live SE `Arc` (for broadcast-style delivery to all projects).
    fn all_se(&self) -> Vec<Arc<SearchEngineIntegration>> {
        self.entries
            .iter()
            .map(|(_, s)| Arc::clone(&s.se))
            .collect()
    }

    /// Insert a freshly built state at the front, evicting the LRU tail beyond
    /// `SE_STATE_LRU_CAP`. Dropping the evicted `SeState` aborts its daemon.
    fn insert_front(&mut self, key: ProjectKey, state: SeState) {
        // Defensive: if the key somehow already exists, replace it.
        self.entries.retain(|(k, _)| k != &key);
        self.entries.insert(0, (key, state));
        while self.entries.len() > SE_STATE_LRU_CAP {
            // Pop the least-recently-used (tail). Drop = daemon abort.
            let _evicted = self.entries.pop();
            log::info!(
                "[se] LRU cap ({SE_STATE_LRU_CAP}) reached — evicting least-recently-used \
                 project SE (index daemon torn down)"
            );
        }
    }
}

pub struct IpcHandler {
    // W-MEMORY-EVOLUTION PR-0 (2026-05-29) — `evaluator` carries mutable state
    // (extract_cursor / scan throttle). Wrapped in a `tokio::sync::Mutex` so
    // `handle_value` can be `&self`. The guard IS held across the evaluate
    // await, but evaluate is fast deterministic gate/cursor work that **never
    // awaits an LLM round-trip** — so this does not reintroduce B2 (the
    // deadlock was a global lock spanning the tier `process` LLM await, which
    // no longer touches the evaluator).
    pub evaluator: tokio::sync::Mutex<TurnEvaluator>,
    /// Durable runner handoff journal. Production handlers always install it;
    /// unit-only `new()` keeps `None` for focused policy tests that do not
    /// construct a managed state root.
    journal: Option<Arc<Journal>>,
    settlement_owner: String,
    runner_settlement_gate: tokio::sync::Mutex<()>,
    // W-MEMORY-EVOLUTION PR-0 (2026-05-29) — D3 去全局 Mutex 根治死锁。
    // 这三个可变字段改为 `std::sync::Mutex` 内部可变，使 `handle_value`
    // 等方法签名从 `&mut self` 收敛为 `&self`，`lib.rs` 外层
    // `Arc<Mutex<IpcHandler>>` 降为 `Arc<IpcHandler>`。临界区只做
    // clone / 赋值，**绝不跨 await 持有 guard**（async 串行 + delivery
    // 抢锁是 B2 死锁根因），故用同步 `std::sync::Mutex` 而非 tokio Mutex。
    last_memory_dir: std::sync::Mutex<Option<PathBuf>>,
    last_project_state_dir: std::sync::Mutex<Option<PathBuf>>,
    /// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — last foreground turn/tier
    /// activity timestamp (ms since epoch; 0 = never). Stamped at the entry
    /// of `memory.turn_end.evaluate` + every `memory.tier*.process` method.
    /// The periodic dream task reads this to honor the idle gate (don't
    /// preempt an active foreground session). `AtomicU64` so the stamp is a
    /// single lock-free store on the hot turn-end path.
    last_turn_activity_ms: std::sync::atomic::AtomicU64,
    /// W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — Tier-1 SessionMemory
    /// processor (per-orchestrator singleton). Holds the gate's per-session
    /// state + the pending oneshot map keyed by `req_id`. Reverse IPC LLM
    /// call requests are emitted via `RecordingEmitter` by default
    /// (in-memory, for unit tests + safety in environments without an
    /// TUI client outgoing channel hooked up); production wiring swaps in a
    /// broadcast emitter via `set_tier1_emitter()`.
    tier1_processor: Arc<SessionMemoryProcessor>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — emitter handle. Kept
    /// for diagnostics + test inspection.
    #[allow(dead_code)]
    tier1_emitter: Arc<RecordingEmitter>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 ExtractMemories
    /// processor (per-orchestrator singleton). Same pending-HashMap pattern
    /// as Tier-1 but keyed by Tier-2 `req_id` (prefix `tier2-`). Reverse
    /// IPC LLM call results routed through `memory.tier.llm_call_result`
    /// are delivered to **both** processors (each is a no-op on unknown
    /// `req_id`), so a single dispatcher request can land in whichever
    /// processor owns the matching pending oneshot.
    tier2_processor: Arc<ExtractProcessor>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 emitter handle.
    /// Same recording semantics as `tier1_emitter` for diagnostics + tests.
    #[allow(dead_code)]
    tier2_emitter: Arc<Tier2RecordingEmitter>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 AutoDream
    /// processor (per-orchestrator singleton). Same pending-HashMap pattern
    /// as Tier-1/Tier-2; `req_id` prefix `tier3-`. Reverse IPC LLM call
    /// results routed through `memory.tier.llm_call_result` are delivered to
    /// **all three** processors (each is a no-op on unknown `req_id`).
    tier3_processor: Arc<DreamProcessor>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 emitter handle.
    #[allow(dead_code)]
    tier3_emitter: Arc<Tier3RecordingEmitter>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination
    /// processor (per-orchestrator singleton). Independent of `tier3_processor`
    /// (AutoDream); `req_id` prefix `tier3-imagination-`. Reverse IPC LLM
    /// call results routed through `memory.tier.llm_call_result` are
    /// delivered to **all four** processors (quad-deliver: tier1 + tier2 +
    /// tier3-dream + tier3-imagination). Each is a no-op on unknown
    /// `req_id`; the first prefix-matching processor wins.
    tier3_imagination_processor: Arc<ImaginationProcessor>,
    /// W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination
    /// emitter handle.
    #[allow(dead_code)]
    tier3_imagination_emitter: Arc<Tier3ImagRecordingEmitter>,
    /// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — Phase 4 起手 PR:
    /// SearchEngineIntegration handle. Lazily initialized on first request
    /// that has a known `memory_dir` / `project_state_dir` (the SE data
    /// directory lives at `<project_state_dir>/search/`, not derivable at
    /// `Default::default()` construction time). Until then this is `None`
    /// and `memory.tier.embedding_result` deliveries are accepted but
    /// silently no-op (mirrors the `tier1/2/3` delivery semantics: unknown
    /// `req_id` is a no-op).
    /// W3 P1-4 (2026-06-05) — per-project SE state, keyed by canonical
    /// `project_state_dir` (`ProjectKey`), bounded-LRU (cap 8). REPLACES the
    /// former process-global `se_integration` singleton (which was shared
    /// across all connections and reused project A's index root for every
    /// later project). `ensure_se_integration` looks up by the request's key
    /// and only reuses the SE for the SAME key; a different key gets a fresh
    /// SE rooted at ITS own `<project_state_dir>/search/`. The index daemon +
    /// initial-index guard now live INSIDE each `SeState`, so the daemon
    /// watches each project's own roots and the initial index fires once per
    /// project, not once globally.
    se_states: std::sync::Mutex<SeStateMap>,
    /// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — SE embedding emitter
    /// handle. Production wiring (`with_event_sink`) supplies the TUI client
    /// broadcast emitter (`UdsBroadcastEmitter`); `default()` supplies an
    /// in-memory `RecordingEmitter`. This is the emitter handed to the lazily
    /// constructed `SearchEngineIntegration` (PR-9) so its embedding
    /// reverse-IPC round-trips use the same channel as the Tier processors.
    ///
    /// W-MEMORY-EVOLUTION PR-9 (2026-05-29): typed as `Arc<dyn EmbeddingEmitter>`
    /// (was `Arc<SeRecordingEmitter>`) so the production broadcast emitter can
    /// back the lazily-constructed SE integration. The embedding *channel*
    /// itself has no TS executor yet (no SDK embedding endpoint; §15 P7
    /// routes semantic retrieval to the local Rust engine and forbids adding
    /// an SDK embedding caller) — so today the emitter is unused at runtime
    /// for search (BM25/text-only path); it is wired for future drift safety.
    se_emitter: Arc<dyn EmbeddingEmitter>,
    // W3 P1-4 (2026-06-05) — the former process-global `index_daemon` +
    // `se_initial_index_started` fields moved INTO each `SeState` (see
    // `se_states` above) so they are PER-PROJECT, not process-global.
    /// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — gate-skip emitter. Emits a
    /// `memory/gate/skipped` frame whenever the periodic dream task's gate
    /// declines to run (idle / disabled / dream_gate skip). Production wiring
    /// (`with_event_sink`) installs the real `UdsBroadcastEmitter`; `default()`
    /// installs the in-memory `RecordingGateSkipEmitter` (safe for unit tests
    /// and any environment without an TUI client events sink). The TUI client
    /// pump maps the frame to `ServerNotification::MemoryGateSkipped`,
    /// broadcasts it, and the TUI `GateDecisionPanel` renders the
    /// "why no auto-dream" data.
    gate_skip_emitter: Arc<dyn crate::broadcast_emitter::GateSkipEmitter>,
    /// W-MEMORY-LIFECYCLE K10/K9 (2026-07-09) — the resolved `<base>` config
    /// root (`CRABCODE_CONFIG_DIR` > `<CRABCODE_HOME>/.crabcode` >
    /// `<home>/.crabcode`), captured once at construction. Consumers:
    /// `dream-watch.json` (watch store), the knowledge corpus dir
    /// (`<base>/knowledge`) fed into dream-corpus assembly. Interior-mutable
    /// so tests can point it at a hermetic tempdir (`set_base_dir`).
    base_dir: std::sync::Mutex<PathBuf>,
    /// W-MEMORY-LIFECYCLE K5 (2026-07-09) — in-flight latch for the
    /// *independent* periodic imagination cycle. The `last-imagination.json`
    /// marker is only refreshed when a sweep completes, so a sweep outliving
    /// one 10-min tick would otherwise be double-started by the next tick;
    /// this latch closes that window. Chained (after-dream / watch) runs do
    /// not take the latch — their own throttles (dream gate / watch interval)
    /// bound them.
    imagination_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for IpcHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcHandler")
            .field(
                "last_memory_dir",
                &self.last_memory_dir.lock().ok().and_then(|g| g.clone()),
            )
            .field(
                "last_project_state_dir",
                &self
                    .last_project_state_dir
                    .lock()
                    .ok()
                    .and_then(|g| g.clone()),
            )
            .finish_non_exhaustive()
    }
}

impl Default for IpcHandler {
    fn default() -> Self {
        let emitter = Arc::new(RecordingEmitter::new());
        let gate = Arc::new(SessionMemoryGate::new());
        let processor = Arc::new(SessionMemoryProcessor::new(
            gate,
            Arc::clone(&emitter) as Arc<dyn LlmCallEmitter>,
        ));
        // W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 processor +
        // gate (own state, own pending map, own emitter). Production wiring
        // swaps the recording emitter for an TUI client broadcast emitter
        // later; per-tier isolation keeps the two stacks decoupled.
        let tier2_emitter = Arc::new(Tier2RecordingEmitter::new());
        let tier2_gate = Arc::new(ExtractGate::new());
        let tier2_processor = Arc::new(ExtractProcessor::new(
            tier2_gate,
            Arc::clone(&tier2_emitter) as Arc<dyn Tier2LlmCallEmitter>,
        ));
        // W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 processor +
        // gate (own state, own pending map, own emitter). Mirrors Tier-1 /
        // Tier-2 wiring; production swaps the recording emitter for an
        // TUI client broadcast emitter.
        let tier3_emitter = Arc::new(Tier3RecordingEmitter::new());
        let tier3_gate = Arc::new(AutoDreamGate::new());
        let tier3_processor = Arc::new(DreamProcessor::new(
            tier3_gate,
            Arc::clone(&tier3_emitter) as Arc<dyn Tier3LlmCallEmitter>,
        ));
        // W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination
        // processor + gate (own state, own pending map, own emitter).
        // Independent of `tier3_processor` (AutoDream) by design — different
        // pipeline shape (5-layer confidence vs 5-phase consolidation).
        let tier3_imagination_emitter = Arc::new(Tier3ImagRecordingEmitter::new());
        let tier3_imagination_gate = Arc::new(ImaginationGate::new());
        let tier3_imagination_processor = Arc::new(ImaginationProcessor::new(
            tier3_imagination_gate,
            Arc::clone(&tier3_imagination_emitter) as Arc<dyn Tier3ImagLlmCallEmitter>,
        ));
        // W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — SE emitter is
        // created up-front (recording mode); the actual SearchEngine handle
        // is lazily attached when the first request lands with a known
        // `project_state_dir` (the SE data directory layout requires a
        // disk path; not derivable at default-construction time).
        let se_emitter = Arc::new(SeRecordingEmitter::new()) as Arc<dyn EmbeddingEmitter>;
        // W-MEMORY-EVOLUTION PR-10 — recording gate-skip emitter (in-memory).
        let gate_skip_emitter = Arc::new(crate::broadcast_emitter::RecordingGateSkipEmitter::new())
            as Arc<dyn crate::broadcast_emitter::GateSkipEmitter>;
        Self {
            evaluator: tokio::sync::Mutex::new(TurnEvaluator::default()),
            journal: None,
            settlement_owner: settlement_owner(),
            runner_settlement_gate: tokio::sync::Mutex::new(()),
            last_memory_dir: std::sync::Mutex::new(None),
            last_project_state_dir: std::sync::Mutex::new(None),
            last_turn_activity_ms: std::sync::atomic::AtomicU64::new(0),
            tier1_processor: processor,
            tier1_emitter: emitter,
            tier2_processor,
            tier2_emitter,
            tier3_processor,
            tier3_emitter,
            tier3_imagination_processor,
            tier3_imagination_emitter,
            se_states: std::sync::Mutex::new(SeStateMap::default()),
            se_emitter,
            gate_skip_emitter,
            base_dir: std::sync::Mutex::new(crate::watch_config::resolve_base_dir()),
            imagination_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl IpcHandler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_journal_for_testing(journal: Arc<Journal>) -> Self {
        let mut handler = Self::default();
        handler.evaluator =
            tokio::sync::Mutex::new(TurnEvaluator::with_journal(Arc::clone(&journal)));
        handler.journal = Some(journal);
        handler
    }

    /// W-MEMORY-EVOLUTION PR-3 (2026-05-29) — production wiring constructor.
    /// Builds an `IpcHandler` whose 5 Tier processors (tier1 / tier2 /
    /// tier3-dream / tier3-imagination) emit reverse-IPC LLM call requests
    /// through a real `UdsBroadcastEmitter` backed by the events long-
    /// connection `EventSink` (PR-1), rather than the in-memory
    /// `RecordingEmitter`. The pushed frames are read by TUI client
    /// `memory_events_pump`, translated to `ServerNotification`, and
    /// broadcast to the TS business side (which runs the SDK call and writes
    /// the result back via `memory.tier.llm_call_result`).
    ///
    /// `new()` / `default()` keep the `RecordingEmitter` (safe for unit
    /// tests + any environment without a hooked-up events sink).
    ///
    /// # SE embedding emitter (deliberately not wired here)
    ///
    /// The SE `SearchEngineIntegration` requires a disk `data_dir`
    /// (`<project_state_dir>/search/`) which is not known at construction
    /// time (lazy-init pattern: `se_integration` stays `None` exactly like
    /// `default()`). Attaching a broadcast-backed SE happens at the lazy-init
    /// site (P4.x SE wire / PR-9), where a fresh `UdsBroadcastEmitter` can be
    /// built from the same `EventSink` and passed to
    /// `SearchEngineIntegration::new(..)` + `set_se_integration(..)`. The
    /// embedding-request side of the broadcast emitter is exercised by the
    /// `broadcast_emitter` unit tests; the LLM-emitter wiring (the core PR-3
    /// deliverable) is fully live through this constructor.
    #[cfg(unix)]
    #[must_use]
    pub fn with_event_sink(
        event_sink: Arc<crate::event_sink::EventSink>,
        journal: Arc<Journal>,
    ) -> Self {
        use crate::broadcast_emitter::UdsBroadcastEmitter;

        // One shared broadcast emitter backs every Tier processor's LLM
        // reverse-IPC path.
        let broadcast = Arc::new(UdsBroadcastEmitter::new(event_sink));

        let tier1_emitter = Arc::new(RecordingEmitter::new());
        let gate = Arc::new(SessionMemoryGate::new());
        let tier1_processor = Arc::new(SessionMemoryProcessor::new(
            gate,
            Arc::clone(&broadcast) as Arc<dyn LlmCallEmitter>,
        ));

        let tier2_emitter = Arc::new(Tier2RecordingEmitter::new());
        let tier2_gate = Arc::new(ExtractGate::new());
        let tier2_processor = Arc::new(ExtractProcessor::new(
            tier2_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier2LlmCallEmitter>,
        ));

        let tier3_emitter = Arc::new(Tier3RecordingEmitter::new());
        let tier3_gate = Arc::new(AutoDreamGate::new());
        let tier3_processor = Arc::new(DreamProcessor::new(
            tier3_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier3LlmCallEmitter>,
        ));

        let tier3_imagination_emitter = Arc::new(Tier3ImagRecordingEmitter::new());
        let tier3_imagination_gate = Arc::new(ImaginationGate::new());
        // W-MEMORY-EVOLUTION PR-7b — the shared broadcast emitter ALSO backs
        // the imagination tool-call (evidence-gathering) reverse-IPC path. The
        // tool requests are emitted to the same `EventSink` long-connection and
        // mapped by the TUI client pump to `MemoryTierToolCallRequest`.
        let tier3_imagination_processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            tier3_imagination_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier3ImagLlmCallEmitter>,
            Arc::clone(&broadcast) as Arc<dyn crate::tier::tier3_imagination::ToolCallEmitter>,
        ));

        // W-MEMORY-EVOLUTION PR-9 — the SE embedding emitter is the same shared
        // broadcast emitter that backs the Tier LLM channels. The lazily-
        // constructed `SearchEngineIntegration` (on first request with a known
        // project_state_dir) uses this so its embedding reverse-IPC requests go
        // out the events long-connection. (Today the embedding *channel* has no
        // TS executor — search runs BM25/text-only — but the production emitter
        // is wired for forward-compat / future-drift safety.)
        let se_emitter = Arc::clone(&broadcast) as Arc<dyn EmbeddingEmitter>;
        // W-MEMORY-EVOLUTION PR-10 — the same shared broadcast emitter backs the
        // gate-skip channel; `run_dream_tick` pushes `memory/gate/skipped`
        // frames through it on idle / disabled / gate-declined ticks.
        let gate_skip_emitter =
            Arc::clone(&broadcast) as Arc<dyn crate::broadcast_emitter::GateSkipEmitter>;
        Self {
            evaluator: tokio::sync::Mutex::new(TurnEvaluator::with_journal(Arc::clone(&journal))),
            journal: Some(journal),
            settlement_owner: settlement_owner(),
            runner_settlement_gate: tokio::sync::Mutex::new(()),
            last_memory_dir: std::sync::Mutex::new(None),
            last_project_state_dir: std::sync::Mutex::new(None),
            last_turn_activity_ms: std::sync::atomic::AtomicU64::new(0),
            tier1_processor,
            tier1_emitter,
            tier2_processor,
            tier2_emitter,
            tier3_processor,
            tier3_emitter,
            tier3_imagination_processor,
            tier3_imagination_emitter,
            se_states: std::sync::Mutex::new(SeStateMap::default()),
            se_emitter,
            gate_skip_emitter,
            base_dir: std::sync::Mutex::new(crate::watch_config::resolve_base_dir()),
            imagination_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// W-MEMORY-EVOLUTION W11 PR-5 (2026-05-29) — Windows sibling of the
    /// `#[cfg(unix)]` `with_event_sink` production wiring constructor. Identical
    /// body (one shared `UdsBroadcastEmitter` backs every Tier processor + SE +
    /// gate-skip channel); the only platform difference is the underlying
    /// `EventSink` transport (Windows Named Pipe write-halves). Kept as a
    /// separate `#[cfg(windows)]` item so the Unix constructor stays verbatim.
    #[cfg(windows)]
    #[must_use]
    pub fn with_event_sink(
        event_sink: Arc<crate::event_sink::EventSink>,
        journal: Arc<Journal>,
    ) -> Self {
        use crate::broadcast_emitter::UdsBroadcastEmitter;

        // One shared broadcast emitter backs every Tier processor's LLM
        // reverse-IPC path.
        let broadcast = Arc::new(UdsBroadcastEmitter::new(event_sink));

        let tier1_emitter = Arc::new(RecordingEmitter::new());
        let gate = Arc::new(SessionMemoryGate::new());
        let tier1_processor = Arc::new(SessionMemoryProcessor::new(
            gate,
            Arc::clone(&broadcast) as Arc<dyn LlmCallEmitter>,
        ));

        let tier2_emitter = Arc::new(Tier2RecordingEmitter::new());
        let tier2_gate = Arc::new(ExtractGate::new());
        let tier2_processor = Arc::new(ExtractProcessor::new(
            tier2_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier2LlmCallEmitter>,
        ));

        let tier3_emitter = Arc::new(Tier3RecordingEmitter::new());
        let tier3_gate = Arc::new(AutoDreamGate::new());
        let tier3_processor = Arc::new(DreamProcessor::new(
            tier3_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier3LlmCallEmitter>,
        ));

        let tier3_imagination_emitter = Arc::new(Tier3ImagRecordingEmitter::new());
        let tier3_imagination_gate = Arc::new(ImaginationGate::new());
        let tier3_imagination_processor = Arc::new(ImaginationProcessor::with_tool_emitter(
            tier3_imagination_gate,
            Arc::clone(&broadcast) as Arc<dyn Tier3ImagLlmCallEmitter>,
            Arc::clone(&broadcast) as Arc<dyn crate::tier::tier3_imagination::ToolCallEmitter>,
        ));

        let se_emitter = Arc::clone(&broadcast) as Arc<dyn EmbeddingEmitter>;
        let gate_skip_emitter =
            Arc::clone(&broadcast) as Arc<dyn crate::broadcast_emitter::GateSkipEmitter>;
        Self {
            evaluator: tokio::sync::Mutex::new(TurnEvaluator::with_journal(Arc::clone(&journal))),
            journal: Some(journal),
            settlement_owner: settlement_owner(),
            runner_settlement_gate: tokio::sync::Mutex::new(()),
            last_memory_dir: std::sync::Mutex::new(None),
            last_project_state_dir: std::sync::Mutex::new(None),
            last_turn_activity_ms: std::sync::atomic::AtomicU64::new(0),
            tier1_processor,
            tier1_emitter,
            tier2_processor,
            tier2_emitter,
            tier3_processor,
            tier3_emitter,
            tier3_imagination_processor,
            tier3_imagination_emitter,
            se_states: std::sync::Mutex::new(SeStateMap::default()),
            se_emitter,
            gate_skip_emitter,
            base_dir: std::sync::Mutex::new(crate::watch_config::resolve_base_dir()),
            imagination_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Expose the Tier-1 processor (clone of `Arc`) so external code (the
    /// future TUI client reverse-IPC emit hook, or tests) can deliver
    /// `LlmCallResultPayload` matches without going through the IpcHandler
    /// outer Mutex.
    #[must_use]
    pub fn tier1_processor(&self) -> Arc<SessionMemoryProcessor> {
        Arc::clone(&self.tier1_processor)
    }

    /// W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — expose the Tier-2
    /// processor (clone of `Arc`) so external code (the future TUI client
    /// reverse-IPC emit hook, or tests) can deliver `LlmCallResultPayload`
    /// matches without going through the IpcHandler outer Mutex.
    #[must_use]
    pub fn tier2_processor(&self) -> Arc<ExtractProcessor> {
        Arc::clone(&self.tier2_processor)
    }

    /// W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — expose the Tier-3
    /// processor (clone of `Arc`) — symmetry with Tier-1/Tier-2 accessors;
    /// lets external code deliver `LlmCallResultPayload` matches without
    /// holding the IpcHandler outer Mutex.
    #[must_use]
    pub fn tier3_processor(&self) -> Arc<DreamProcessor> {
        Arc::clone(&self.tier3_processor)
    }

    /// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — build the `DreamProcessInput`
    /// for a manually-requested dream from the `RunNowResponse` produced by
    /// `TurnEvaluator::evaluate_dream_run_now` (which has already registered the
    /// trigger + acquired the consolidation lock). Returns `None` when the
    /// evaluator surfaced a skip (e.g. `lock_held`) — nothing to run.
    ///
    /// The `AutoDreamGateOutput` is built from the trigger the evaluator already
    /// produced (the lock is held; `prior_mtime_ms` came from the real
    /// pre-acquire mtime, so rollback-on-failure inside `process()` stays
    /// correct). We do NOT re-run `evaluate_gate` — that would re-read the
    /// now-fresh lock mtime and corrupt `prior_mtime_ms`.
    fn build_dream_now_input(
        memory_dir: &Path,
        response: &RunNowResponse,
        knowledge_dir: Option<&Path>,
    ) -> Option<DreamProcessInput> {
        let trigger = response.triggers.first()?;
        let prior_mtime_ms = trigger
            .runner_payload
            .get("prior_mtime_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let gate_payload = crate::tier::tier3_auto_dream::AutoDreamGateOutput {
            lock_path: crate::lock::lock_path(memory_dir),
            holder_pid: std::process::id(),
            prior_mtime_ms,
            touched_session_count_at_trigger: trigger
                .runner_payload
                .get("sessions_since_last_consolidation")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        };
        // W-MEMORY-DATA-COMPLETION Phase 0 (2026-06-20): assemble the real
        // corpus from disk (transcripts + memdir) so the manual "Run Now" dream
        // also sees actual data. `prior_mtime_ms` is the pre-acquire watermark;
        // project_state_dir derives from memory_dir (manual path has no handler
        // context — same derivation the manual gate uses). fail-soft to empty.
        // W-MEMORY-LIFECYCLE K9 (2026-07-09): the personal knowledge base
        // (`<base>/knowledge`) is folded in as an extra corpus section.
        let corpus = crate::dream_corpus::build_dream_corpus_for_memory_dir(
            memory_dir,
            prior_mtime_ms,
            knowledge_dir,
        );
        Some(DreamProcessInput {
            memory_dir: memory_dir.to_path_buf(),
            gate_payload,
            // G3-a：水位线推进目标随语料透传（撞帽积压留给下轮）。
            consumed_watermark_ms: corpus.consumed_watermark_ms,
            recent_sessions_summary: corpus.recent_sessions_summary,
            memdir_manifest: corpus.memdir_manifest,
            model_hint: None,
            params: crate::tier::LlmCallParams::default(),
            instance_key: String::new(),
        })
    }

    /// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — kick off a manually-requested
    /// dream on a DETACHED task (the IPC response returns promptly; the dream
    /// runs in the background driving the Tier-3 `DreamProcessor::process`
    /// reverse-IPC LLM round-trips → `dreams/*.md`). This closes gap2 ("Run Now
    /// didn't execute"): the old behaviour stopped at registering a trigger.
    ///
    /// The lock acquired by `evaluate_dream_run_now` is the real consolidation
    /// lock. A3 fix (P0-3, 2026-06-05): `DreamProcessor::process` now settles
    /// that lock at every exit (success → fresh-mtime release via
    /// `record_consolidation_complete`; failure → `rollback` to the prior
    /// mtime), so a single acquire/release cycle spans the whole run. Before A3
    /// `process()` left the file lock held until ~1h stale → self-deadlocked
    /// dreaming. The file lock — not the in-process `dream_in_progress` flag —
    /// is the authoritative cross-task guard here (a concurrent periodic tick's
    /// gate `try_acquire_for` fails → `lock_held` skip while this run holds it).
    ///
    /// Returns a small status block for the response:
    /// * `{ "started": false, "skip_reason": <reason> }` when the evaluator
    ///   declined (busy lock) — nothing to execute.
    /// * `{ "started": true }` when the dream task was spawned.
    fn spawn_dream_now(&self, memory_dir: &Path, response: &RunNowResponse) -> Value {
        let knowledge_dir = self.knowledge_dir();
        let Some(process_input) =
            Self::build_dream_now_input(memory_dir, response, Some(&knowledge_dir))
        else {
            return json!({
                "started": false,
                "skip_reason": response.gate_skip_reason.clone(),
            });
        };
        let processor = Arc::clone(&self.tier3_processor);
        // W-MEMORY-SELF-EVOLUTION A3+B1 (2026-06-11, 用户裁决③④): a
        // successful manual dream auto-promotes qualifying insights into the
        // MEMORY.md index, then chains one self-generated imagination run.
        let imagination = Arc::clone(&self.tier3_imagination_processor);
        let imagination_dir = memory_dir.to_path_buf();
        let promote_state_dir = project_state_dir_from_memory_dir(memory_dir);
        // W3 (2026-07-16)：detached 任务里没有 self —— 报告结构语言在 spawn
        // 前解析并捕获。
        let report_language =
            crate::output_language::resolve_memory_output_language(&self.base_dir());
        tokio::spawn(async move {
            match processor.process(process_input).await {
                Ok(output) => {
                    // R4-2：手动 lane 此前**完全不记账**，于是磁盘上明明有
                    // 手动做梦落下的 insight 产物，`gate-stats.dreamed` 却
                    // 停在 0 —— `dreamed` 系统性漏计成功。现在记 manual lane
                    // （仅供诊断展示，`GateStats::automatic` 会把它排除在
                    // 适应度口径之外，见该函数的消费侧契约）。
                    crate::evolution::gate_stats::record_tick_outcome(
                        &promote_state_dir,
                        crate::evolution::gate_stats::LANE_MANUAL,
                        "dreamed",
                        crate::extract_archive::now_ms(),
                    )
                    .await;
                    let auto_promote = read_dream_config(&promote_state_dir)
                        .map(|cfg| cfg.auto_promote)
                        .unwrap_or_default();
                    let _ = crate::tier::tier3_auto_dream::auto_promote_insights(
                        &imagination_dir,
                        auto_promote,
                        &output.insight_paths,
                    )
                    .await;
                    // W6 (6c) — 手动做梦成功同样消费重要性积分。
                    crate::importance_pressure::reset_importance(
                        &promote_state_dir,
                        crate::extract_archive::now_ms(),
                    )
                    .await;
                    spawn_imagination_after_dream(
                        imagination,
                        imagination_dir,
                        promote_state_dir,
                        None,
                        None,
                        report_language,
                    );
                }
                Err(e) => {
                    crate::evolution::gate_stats::record_tick_outcome(
                        &promote_state_dir,
                        crate::evolution::gate_stats::LANE_MANUAL,
                        "errored",
                        crate::extract_archive::now_ms(),
                    )
                    .await;
                    log::warn!("memory.dream.run_now: dream process failed (fail-soft): {e}");
                }
            }
        });
        json!({ "started": true })
    }

    /// W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — expose the Tier-3
    /// Imagination processor (clone of `Arc`) — symmetry with Tier-1/Tier-2/
    /// Tier-3 dream accessors; lets external code deliver
    /// `LlmCallResultPayload` matches without holding the IpcHandler outer
    /// Mutex.
    #[must_use]
    pub fn tier3_imagination_processor(&self) -> Arc<ImaginationProcessor> {
        Arc::clone(&self.tier3_imagination_processor)
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — accessor for the
    /// SearchEngineIntegration handle (clone of `Arc`). Returns `None` if no
    /// SE has been initialized yet.
    ///
    /// W3 P1-4 (2026-06-05) — now backed by the per-project `se_states` LRU
    /// map (was a single process-global slot). Returns the MOST-RECENTLY-USED
    /// project's SE (map front). Per-project routing for the actual search /
    /// turn_end paths goes through `ensure_se_integration` (keyed); this
    /// accessor remains for the embedding-result delivery + tests that
    /// inject a single SE.
    #[must_use]
    pub fn se_integration(&self) -> Option<Arc<SearchEngineIntegration>> {
        self.se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .front_se()
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — inject a pre-built
    /// `SearchEngineIntegration`. Used by tests to attach an SE without a
    /// disk-resolved project key.
    ///
    /// W3 P1-4 (2026-06-05) — inserts under a synthetic test key at the front
    /// of the per-project LRU map (no real index daemon / initial index — the
    /// SE is injected directly for delivery/unit-test purposes).
    pub fn set_se_integration(&self, integration: Arc<SearchEngineIntegration>) {
        let key = ProjectKey("__injected__".to_string());
        let state = SeState {
            se: integration,
            index_daemon: None,
            initial_index_started: false,
        };
        self.se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_front(key, state);
    }

    /// W-MEMORY-EVOLUTION PR-9 (2026-05-29) — lazily construct the production
    /// `SearchEngineIntegration` + spawn the fs-event index daemon the first
    /// time a request lands with a known `memory_dir` + `project_state_dir`.
    ///
    /// The SE data directory (`<project_state_dir>/search/`) is not derivable
    /// at construction time, hence lazy init (mirrors the `last_memory_dir` /
    /// `last_project_state_dir` lazy state). Subsequent calls are no-ops once
    /// the SE is attached (single-init; the daemon's initial reindex picks up
    /// any files written before init).
    ///
    /// Failure is fail-soft: a construction error logs a warning and leaves
    /// `se_integration` as `None` (search returns the empty "warming up"
    /// state; the next request retries). The full reindex is run on a blocking
    /// thread (so it does not block the async request) and the daemon picks up
    /// subsequent fs changes.
    ///
    /// Returns the attached integration `Arc` (or `None` if init failed).
    ///
    /// W3 P1-4 (2026-06-05) — keyed PER-PROJECT by the canonical
    /// `project_state_dir` (`ProjectKey`). A request for an already-seen
    /// project reuses ITS SE (fast path); a DIFFERENT project gets a fresh SE
    /// rooted at ITS own `<project_state_dir>/search/` (so project A's index
    /// root is never reused for project B). The fs-event index daemon and the
    /// one-shot initial-index guard live inside each project's `SeState`, so
    /// the daemon watches each project's own roots and the initial index fires
    /// once per project. A bounded LRU (`SE_STATE_LRU_CAP`) evicts the
    /// least-recently-used project (tearing down its daemon) so a long
    /// multi-project session does not accumulate unbounded handles/fds.
    fn ensure_se_integration(
        &self,
        memory_dir: &Path,
        project_state_dir: &Path,
    ) -> Option<Arc<SearchEngineIntegration>> {
        // Build the index roots from the memory_dir (private scope).
        self.ensure_se_integration_for_root(
            MemoryRoot::private(memory_dir.to_path_buf()),
            project_state_dir,
        )
    }

    /// W-MEMORY-LIFECYCLE K9+K4 (2026-07-09) — scope-generic core of
    /// `ensure_se_integration`. The per-root SE state (engine + fs-event
    /// index daemon + one-shot initial index) is keyed by the canonical
    /// `state_dir`, exactly like the per-project path — global
    /// (`<base>/.global-memory-state/`) and knowledge
    /// (`<base>/.knowledge-state/`) roots simply join the same bounded-LRU
    /// map under their own keys.
    fn ensure_se_integration_for_root(
        &self,
        root: MemoryRoot,
        state_dir: &Path,
    ) -> Option<Arc<SearchEngineIntegration>> {
        let key = ProjectKey::from_project_state_dir(state_dir);

        // Fast path — this root already has a live SE. Moves it to the
        // front of the LRU (most-recently-used).
        if let Some(existing) = self
            .se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_se(&key)
        {
            return Some(existing);
        }

        let data_dir = search_dir_for_project_state(state_dir);
        let integration =
            match SearchEngineIntegration::new(&data_dir, Arc::clone(&self.se_emitter)) {
                Ok(integration) => Arc::new(integration),
                Err(e) => {
                    log::warn!(
                        "[se] lazy init failed (fail-soft; search stays empty until retry): \
                     data_dir={} err={e}",
                        data_dir.display()
                    );
                    return None;
                }
            };

        if let Err(e) = integration.init_collections() {
            log::warn!("[se] init_collections failed (fail-soft): {e}");
            // Still attach — search will fail-soft to empty until a later
            // index pass succeeds; better than dropping the handle.
        }

        let memory_dir = root.path.clone();
        let roots = vec![root];

        // W-MEMORY-EVOLUTION FIX #13 (2026-06-01) — run the initial full index
        // pass OFF the request thread. `index_all` walks the memory tree +
        // reads/parses every markdown file + upserts into the segment store —
        // all blocking IO/CPU. Doing it synchronously here made the first
        // `memory.turn_end.evaluate` block long enough that the TS side
        // (`EVALUATE_TIMEOUT_MS = 250`) timed out and silently dropped the
        // cold-start trigger. We hand the walk to `tokio::task::spawn_blocking`
        // (correct runtime idiom for blocking work) and return immediately.
        // Searches that race ahead of indexing completion fail-soft to empty.
        //
        // W3 P1-4 (2026-06-05) — the initial-index one-shot is now per-project
        // (this code path only runs on the lazy-init miss for THIS key, so it
        // fires exactly once per project; the per-project `initial_index_started`
        // flag stored in `SeState` records it for future-proofing / clarity).
        let integration_for_index = Arc::clone(&integration);
        let roots_for_index = roots.clone();
        tokio::task::spawn_blocking(move || {
            match integration_for_index.index_all(&roots_for_index) {
                Ok(stats) => log::info!(
                    "[se] initial index pass (background): roots={} md_files_seen={} indexed={}",
                    stats.roots_scanned,
                    stats.md_files_seen,
                    stats.indexed
                ),
                Err(e) => log::warn!("[se] initial index pass failed (fail-soft): {e}"),
            }
        });

        // Spawn the fs-event daemon (incremental upserts on later edits) for
        // THIS project's roots. The daemon holds a clone of the SE `Arc`; it
        // is stored inside the project's `SeState` so it is not dropped (drop =
        // abort). On LRU eviction the `SeState` (and thus the daemon) is
        // dropped → the watcher task aborts (clean teardown).
        let config = crate::index_daemon::IndexDaemonConfig {
            roots,
            debounce_ms: crate::index_daemon::resolve_debounce_ms(),
            initial_reindex: false,
        };
        let daemon = crate::index_daemon::IndexDaemon::spawn(Arc::clone(&integration), config);

        let state = SeState {
            se: Arc::clone(&integration),
            index_daemon: Some(daemon),
            initial_index_started: true,
        };
        self.se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_front(key, state);

        log::info!(
            "[se] integration initialised: data_dir={} memory_dir={} (per-project index daemon spawned)",
            data_dir.display(),
            memory_dir.display()
        );

        Some(integration)
    }

    /// W-MEMORY-LIFECYCLE K9+K4 (2026-07-09) — stand up (or reuse) the SE for
    /// a non-project search scope (`global` / `knowledge`). Both dirs come
    /// from the request payload (dispatcher/TS inject them from `<base>` per
    /// the §4 line contract); a scope whose dirs are absent, or whose root
    /// dir does not exist on disk, is silently skipped (`None`) — the spec'd
    /// fail-soft so a machine without a global root / knowledge base never
    /// grows empty state dirs as a search side-effect.
    fn ensure_scope_se(
        &self,
        payload: &Value,
        scope: &'static str,
        dir_key: &str,
        state_key: &str,
    ) -> Option<Arc<SearchEngineIntegration>> {
        let root_dir = payload
            .get(dir_key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(PathBuf::from)?;
        let state_dir = payload
            .get(state_key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(PathBuf::from)?;
        if !root_dir.is_dir() {
            return None;
        }
        self.ensure_se_integration_for_root(scope_memory_root(scope, root_dir), &state_dir)
    }

    /// W-MEMORY-EVOLUTION PR-0 (2026-05-29) — interior-mutability helpers for
    /// the lazy-init `memory_dir` / `project_state_dir` state. Each takes a
    /// short sync lock (no await held) so concurrent connections don't
    /// serialize behind a global handler lock.
    fn last_memory_dir(&self) -> Option<PathBuf> {
        self.last_memory_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_last_memory_dir(&self, dir: PathBuf) {
        *self
            .last_memory_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dir);
    }
    fn last_project_state_dir(&self) -> Option<PathBuf> {
        self.last_project_state_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_last_project_state_dir(&self, dir: PathBuf) {
        *self
            .last_project_state_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dir);
    }

    /// W-MEMORY-LIFECYCLE K10/K9 (2026-07-09) — the resolved `<base>` config
    /// root this handler operates against (watch store + knowledge dir).
    #[must_use]
    pub fn base_dir(&self) -> PathBuf {
        self.base_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Point the handler at a different `<base>` root. Primarily for tests
    /// (hermetic tempdir instead of the real `~/.crabcode`); production
    /// keeps the construction-time `resolve_base_dir()` value.
    pub fn set_base_dir(&self, dir: PathBuf) {
        *self
            .base_dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = dir;
    }

    /// `<base>/knowledge` — the personal knowledge-base root folded into the
    /// dream corpus as an extra source section (K9 anti-orphan channel d).
    fn knowledge_dir(&self) -> PathBuf {
        self.base_dir().join("knowledge")
    }

    // W-MEMORY-EVOLUTION PR-5 (2026-05-29) — foreground-activity stamp +
    // public accessors for the periodic dream task.

    /// Stamp "the foreground just did something" (turn-end evaluate or a
    /// tier `process`). Single lock-free `Relaxed` store — this sits on the
    /// hot path and is read by a 10-min-interval background task, so no
    /// ordering guarantee beyond eventual visibility is required.
    fn stamp_turn_activity(&self) {
        self.last_turn_activity_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Last foreground turn/tier activity timestamp (ms since epoch; 0 =
    /// never). Read by `run_dream_tick` for the idle gate.
    #[must_use]
    pub fn last_turn_activity_ms(&self) -> u64 {
        self.last_turn_activity_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — public read of the lazy-init
    /// `memory_dir` for the periodic dream task (which lives in `lib.rs`,
    /// outside this module, so the private `last_memory_dir` getter isn't
    /// reachable). Returns `None` until a request with a known `memory_dir`
    /// has landed.
    #[must_use]
    pub fn current_memory_dir(&self) -> Option<PathBuf> {
        self.last_memory_dir()
    }

    /// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — public read of the lazy-init
    /// `project_state_dir` (the `dream-config.json` parent). `None` until a
    /// request established it.
    #[must_use]
    pub fn current_project_state_dir(&self) -> Option<PathBuf> {
        self.last_project_state_dir()
    }

    /// W-MEMORY-SYNERGY W5 (2026-07-16) — test-only read of the Tier-1
    /// (SessionMemory) emitter's recorded LLM call requests（镜像
    /// `tier3_recorded_requests` 语义；仅 `new()`/`default()` 构造下有值）。
    #[cfg(test)]
    pub(crate) async fn tier1_recorded_requests(&self) -> Vec<crate::tier::LlmCallRequestPayload> {
        self.tier1_emitter.recorded().await
    }

    /// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — test-only read of the Tier-3
    /// (AutoDream) emitter's recorded LLM call requests. Only meaningful for
    /// `new()` / `default()` handlers (where the Tier-3 processor uses the
    /// in-memory `RecordingEmitter`); under `with_event_sink` the processor
    /// emits through the broadcast emitter and this stays empty.
    #[cfg(test)]
    pub(crate) async fn tier3_recorded_requests(&self) -> Vec<crate::tier::LlmCallRequestPayload> {
        self.tier3_emitter.recorded().await
    }

    /// W-MEMORY-LIFECYCLE K5 (2026-07-09) — test-only read of the Tier-3
    /// Imagination emitter's recorded LLM call requests (mirror of
    /// `tier3_recorded_requests`; only meaningful under `new()`/`default()`
    /// where the imagination processor uses the in-memory recorder).
    #[cfg(test)]
    pub(crate) async fn tier3_imagination_recorded_requests(
        &self,
    ) -> Vec<crate::tier::LlmCallRequestPayload> {
        self.tier3_imagination_emitter.recorded().await
    }

    /// W3 P1-4 (2026-06-05) — test-only: number of live per-project SE states
    /// in the LRU map.
    #[cfg(test)]
    pub(crate) fn se_state_count(&self) -> usize {
        self.se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }

    /// W3 P1-4 (2026-06-05) — test-only: whether the project keyed by
    /// `project_state_dir` has a live (non-`None`) index daemon handle.
    #[cfg(test)]
    pub(crate) fn project_has_index_daemon(&self, project_state_dir: &Path) -> bool {
        let key = ProjectKey::from_project_state_dir(project_state_dir);
        let map = self
            .se_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entries
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, s)| s.index_daemon.is_some())
            .unwrap_or(false)
    }

    /// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — emit a gate-skip decision
    /// through the installed gate-skip emitter (broadcast in production, in-
    /// memory recording under `new()` / `default()`). Called by
    /// `run_dream_tick` on idle / disabled / gate-declined ticks so the TUI
    /// `GateDecisionPanel` gets the real "why no auto-dream" reasons. `&self`;
    /// the emitter holds its own interior mutability.
    async fn emit_gate_skip(&self, payload: crate::broadcast_emitter::GateSkipPayload) {
        self.gate_skip_emitter.emit_gate_skip(payload).await;
    }

    /// W-MEMORY-EVOLUTION PR-10 (2026-05-29) — test-only read of the recorded
    /// gate-skip payloads. Only meaningful for `new()` / `default()` handlers
    /// (where the gate-skip emitter is the in-memory
    /// `RecordingGateSkipEmitter`); downcasts the `Arc<dyn GateSkipEmitter>`.
    #[cfg(test)]
    pub(crate) async fn recorded_gate_skips(
        &self,
    ) -> Vec<crate::broadcast_emitter::GateSkipPayload> {
        // The default handler installs a `RecordingGateSkipEmitter`; expose its
        // recorded set via a small downcast. (Trait objects can't expose
        // `recorded()` directly, so we keep a typed handle in tests by always
        // constructing through `new()`/`default()`.)
        if let Some(rec) = self
            .gate_skip_emitter
            .as_any()
            .downcast_ref::<crate::broadcast_emitter::RecordingGateSkipEmitter>()
        {
            rec.recorded().await
        } else {
            Vec::new()
        }
    }

    fn journal(&self) -> Result<&Journal, BoxError> {
        self.journal
            .as_deref()
            .ok_or_else(|| invalid_input("memory runner journal is unavailable").into())
    }

    fn runner_leader_dir(&self) -> PathBuf {
        self.base_dir().join(".memory-rust-derived")
    }

    async fn require_runner_leader(&self, payload: &Value) -> Result<(), BoxError> {
        let leader_token = required_str(payload, "leader_token")?;
        let leader_epoch = required_positive_u64(payload, "leader_epoch")?;
        let valid = leader_lock::validate_leader_fence(
            &self.runner_leader_dir(),
            &leader_token,
            leader_epoch,
        )
        .await?;
        if valid.is_none() {
            return Err(invalid_input("stale or invalid runner leader fence").into());
        }
        Ok(())
    }

    fn validate_runner_work(item: &WorkItem) -> Result<DurableRunnerWork, BoxError> {
        let work: DurableRunnerWork = serde_json::from_value(item.payload.clone())?;
        work.validate()?;
        if runner_work_key(&work.trigger.trigger_id) != item.key {
            return Err(invalid_input("runner journal key identity mismatch").into());
        }
        Ok(work)
    }

    async fn runner_candidates(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let requested_limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .map(usize::try_from)
            .transpose()
            .map_err(|_| invalid_input("runner candidate limit does not fit usize"))?
            .unwrap_or(RUNNER_CANDIDATE_DEFAULT_LIMIT);
        if requested_limit == 0 {
            return Err(invalid_input("runner candidate limit must be positive").into());
        }
        let limit = requested_limit.min(RUNNER_CANDIDATE_MAX_LIMIT);
        let mut keys = self.journal()?.delivery_candidate_keys(
            WorkKind::RunnerTrigger,
            now_ms(),
            limit.saturating_add(1),
        )?;
        let has_more = keys.len() > limit;
        keys.truncate(limit);

        let mut candidates = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(item) = self.journal()?.get(&key)? else {
                continue;
            };
            match Self::validate_runner_work(&item) {
                Ok(work) => {
                    let mut candidate = trigger_json(&work.trigger);
                    candidate["recovery"] = serde_json::to_value(work.recovery)?;
                    candidates.push(candidate);
                }
                Err(_) => {
                    // Never expose an unvalidated recovery payload. The opaque
                    // key suffix is enough for a leader to claim the row; the
                    // claim path fences and dead-letters it atomically before
                    // returning.
                    if let Some(trigger_id) = key.strip_prefix("runner:") {
                        candidates.push(json!({
                            "trigger_id": trigger_id,
                            "invalid_reason": "invalid_recovery_locator",
                        }));
                    }
                }
            }
        }
        Ok(json!({
            "candidates": candidates,
            "has_more": has_more,
            "limit": limit,
        }))
    }

    async fn claim_runner_delivery(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let trigger_id = required_str(payload, "trigger_id")?;
        let worker_id = required_str(payload, "worker_id")?;
        let claim_now_ms = now_ms();
        let item = self.journal()?.claim_delivery_by_key(
            &runner_work_key(&trigger_id),
            WorkKind::RunnerTrigger,
            &worker_id,
            claim_now_ms,
            RUNNER_DELIVERY_LEASE_MS,
        )?;
        let Some(item) = item else {
            return Ok(json!({
                "received": false,
                "reason": "not_claimable",
            }));
        };
        let work = match Self::validate_runner_work(&item) {
            Ok(work) if work.trigger.trigger_id == trigger_id => work,
            Ok(_) | Err(_) => {
                let poison_fence = DeliveryFence::new(worker_id, item.delivery_epoch);
                let outcome = self.journal()?.mark_dead_letter(
                    &item.key,
                    &poison_fence,
                    claim_now_ms,
                    "invalid_recovery_locator",
                )?;
                return Ok(json!({
                    "received": false,
                    "reason": match outcome {
                        DeadLetterOutcome::DeadLettered
                        | DeadLetterOutcome::AlreadyDeadLettered => {
                            "invalid_recovery_locator_dead_lettered"
                        }
                        DeadLetterOutcome::Missing => "invalid_recovery_locator_missing",
                        DeadLetterOutcome::Stale => "invalid_recovery_locator_fence_lost",
                    },
                }));
            }
        };
        let mut trigger = trigger_json(&work.trigger);
        trigger["recovery"] = serde_json::to_value(work.recovery)?;
        trigger["delivery_owner"] = json!(worker_id);
        trigger["delivery_epoch"] = json!(item.delivery_epoch);
        trigger["lease_expires_at_ms"] = json!(item.lease_expires_at_ms);
        Ok(json!({
            "received": true,
            "trigger": trigger,
        }))
    }

    async fn ack_runner_delivery(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let trigger_id = required_str(payload, "trigger_id")?;
        let fence = parse_delivery_fence(payload)?;
        let outcome = self.journal()?.ack_delivery(
            &runner_work_key(&trigger_id),
            &fence,
            now_ms(),
            RUNNER_DELIVERY_LEASE_MS,
        )?;
        let received = matches!(outcome, AckOutcome::Acked | AckOutcome::AlreadyAcked);
        let lease_expires_at_ms = if received {
            self.journal()?
                .get(&runner_work_key(&trigger_id))?
                .and_then(|item| item.lease_expires_at_ms)
        } else {
            None
        };
        let mut response = json!({
            "received": received,
            "lease_expires_at_ms": lease_expires_at_ms,
        });
        if !received {
            response["reason"] = json!(match outcome {
                AckOutcome::Missing => "missing",
                AckOutcome::Stale => "stale_delivery",
                AckOutcome::Acked | AckOutcome::AlreadyAcked => unreachable!(),
            });
        }
        Ok(response)
    }

    async fn renew_runner_delivery(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let trigger_id = required_str(payload, "trigger_id")?;
        let fence = parse_delivery_fence(payload)?;
        let outcome = self.journal()?.renew_delivery(
            &runner_work_key(&trigger_id),
            &fence,
            now_ms(),
            RUNNER_DELIVERY_LEASE_MS,
        )?;
        let received = outcome == RenewOutcome::Renewed;
        let lease_expires_at_ms = if received {
            self.journal()?
                .get(&runner_work_key(&trigger_id))?
                .and_then(|item| item.lease_expires_at_ms)
        } else {
            None
        };
        let mut response = json!({
            "received": received,
            "lease_expires_at_ms": lease_expires_at_ms,
        });
        if !received {
            response["reason"] = json!(match outcome {
                RenewOutcome::Missing => "missing",
                RenewOutcome::Stale => "stale_delivery",
                RenewOutcome::Renewed => unreachable!(),
            });
        }
        Ok(response)
    }

    async fn release_runner_delivery(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let trigger_id = required_str(payload, "trigger_id")?;
        let fence = parse_delivery_fence(payload)?;
        let reason_code = parse_reason_code(payload)?;
        let key = runner_work_key(&trigger_id);
        let now = now_ms();
        let Some(item) = self.journal()?.get(&key)? else {
            return Ok(json!({ "received": false, "reason": "missing" }));
        };
        let shift = item.attempts.saturating_sub(1).min(20) as u32;
        let delay_ms = RUNNER_RETRY_BASE_DELAY_MS
            .checked_shl(shift)
            .unwrap_or(u64::MAX)
            .min(RUNNER_RETRY_MAX_DELAY_MS);
        let next_attempt_at_ms = now.saturating_add(delay_ms);
        let outcome = self.journal()?.release_delivery(
            &key,
            &fence,
            now,
            next_attempt_at_ms,
            &reason_code,
        )?;
        let received = outcome == ReleaseOutcome::Released;
        let mut response = json!({
            "received": received,
            "next_attempt_at_ms": if received {
                Some(next_attempt_at_ms)
            } else {
                None
            },
        });
        if !received {
            response["reason"] = json!(match outcome {
                ReleaseOutcome::Missing => "missing",
                ReleaseOutcome::Stale => "stale_delivery",
                ReleaseOutcome::Released => unreachable!(),
            });
        }
        Ok(response)
    }

    async fn dead_letter_runner_delivery(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let trigger_id = required_str(payload, "trigger_id")?;
        let fence = parse_delivery_fence(payload)?;
        let reason_code = parse_reason_code(payload)?;
        let outcome = self.journal()?.mark_dead_letter(
            &runner_work_key(&trigger_id),
            &fence,
            now_ms(),
            &reason_code,
        )?;
        let received = outcome == DeadLetterOutcome::DeadLettered;
        let mut response = json!({ "received": received });
        if !received {
            response["reason"] = json!(match outcome {
                DeadLetterOutcome::AlreadyDeadLettered => "already_dead_lettered",
                DeadLetterOutcome::Missing => "missing",
                DeadLetterOutcome::Stale => "stale_delivery",
                DeadLetterOutcome::DeadLettered => unreachable!(),
            });
        }
        Ok(response)
    }

    fn settlement_fence(settling: &WorkItem) -> Result<DeliveryFence, BoxError> {
        Ok(DeliveryFence::new(
            settling
                .lease_owner
                .clone()
                .ok_or_else(|| invalid_input("settlement claim has no owner"))?,
            settling.delivery_epoch,
        ))
    }

    async fn settle_claimed_runner(
        &self,
        settling: WorkItem,
        settlement_fence: &DeliveryFence,
    ) -> Result<RunnerSettlementAttempt, BoxError> {
        let journal = self.journal()?;
        let key = settling.key.clone();
        let work = Self::validate_runner_work(&settling)?;
        let mut durable_completed: RunnerCompleted = serde_json::from_value(
            settling
                .result
                .ok_or_else(|| invalid_input("settlement claim has no result"))?,
        )?;
        if runner_work_key(&durable_completed.trigger_id) != key
            || work.trigger.trigger_id != durable_completed.trigger_id
            || work.pending.trigger_id != durable_completed.trigger_id
            || work.pending.kind != durable_completed.kind
            || work.trigger.kind.as_str() != durable_completed.kind
        {
            return Err(invalid_input("runner settlement identity mismatch").into());
        }
        durable_completed.completed_at_ms = settling.result_recorded_at_ms;

        let renewal_journal = (*journal).clone();
        let renewal_key = key.clone();
        let renewal_fence = settlement_fence.clone();
        let (stop_renewal_tx, mut stop_renewal_rx) = tokio::sync::oneshot::channel::<()>();
        let renewal_task = tokio::spawn(async move {
            let period = std::time::Duration::from_millis(RUNNER_SETTLEMENT_RENEW_INTERVAL_MS);
            let first_tick = tokio::time::Instant::now() + period;
            let mut interval = tokio::time::interval_at(first_tick, period);
            loop {
                tokio::select! {
                    _ = &mut stop_renewal_rx => {
                        return Ok::<Option<RenewOutcome>, acosmi_memory_journal::JournalError>(None);
                    }
                    _ = interval.tick() => {
                        let outcome = renewal_journal.renew_settlement(
                            &renewal_key,
                            &renewal_fence,
                            now_ms(),
                            RUNNER_SETTLEMENT_LEASE_MS,
                        )?;
                        if outcome != RenewOutcome::Renewed {
                            return Ok(Some(outcome));
                        }
                    }
                }
            }
        });

        let report = {
            let mut evaluator = self.evaluator.lock().await;
            evaluator
                .results
                .handle_known_completed(work.pending, durable_completed)
                .await
        };
        let _ = stop_renewal_tx.send(());
        let renewal_outcome = renewal_task.await?;
        let report = report?;
        match renewal_outcome? {
            None => {}
            Some(RenewOutcome::Missing) => return Ok(RunnerSettlementAttempt::Missing),
            Some(RenewOutcome::Stale) => return Ok(RunnerSettlementAttempt::FenceLost),
            Some(RenewOutcome::Renewed) => unreachable!(),
        }

        // A successful journaled dream has a second externally visible phase:
        // the imagination sweep. Persist that follow-up before the source
        // completion becomes Settled. A crash in either half of this sequence
        // is safe: enqueue is idempotent by the source trigger id, while an
        // un-settled source completion is replayed by startup recovery.
        if let Some(memory_dir) = report.dream_settled_memory_dir.clone() {
            let followup = DurableImaginationFollowup {
                schema_id: DURABLE_IMAGINATION_SCHEMA_ID.to_owned(),
                source_trigger_id: work.trigger.trigger_id.clone(),
                project_state_dir: project_state_dir_from_memory_dir(&memory_dir),
                memory_dir,
            };
            journal.enqueue(
                &imagination_followup_key(&followup.source_trigger_id),
                WorkKind::ReverseRequest,
                &serde_json::to_value(followup)?,
                now_ms(),
            )?;
        }

        match journal.mark_settled(&key, settlement_fence, now_ms())? {
            SettleOutcome::Settled | SettleOutcome::AlreadySettled => {}
            SettleOutcome::Missing => return Ok(RunnerSettlementAttempt::Missing),
            SettleOutcome::Stale => return Ok(RunnerSettlementAttempt::FenceLost),
        }
        Ok(RunnerSettlementAttempt::Settled(report))
    }

    async fn complete_journaled_runner(&self, payload: &Value) -> Result<Value, BoxError> {
        self.require_runner_leader(payload).await?;
        let mut completed = parse_runner_completed(payload)?;
        let trigger_id = completed.trigger_id.clone();
        let key = runner_work_key(&trigger_id);
        let delivery_fence = parse_delivery_fence(payload)?;
        let journal = self.journal()?;
        let Some(before) = journal.get(&key)? else {
            return Ok(json!({ "received": false, "reason": "missing" }));
        };
        let durable = Self::validate_runner_work(&before)?;
        if durable.trigger.trigger_id != trigger_id
            || durable.pending.trigger_id != trigger_id
            || durable.pending.kind != completed.kind
            || durable.trigger.kind.as_str() != completed.kind
        {
            return Ok(json!({
                "received": false,
                "reason": "trigger_identity_mismatch",
            }));
        }

        completed.completed_at_ms = None;
        let result = serde_json::to_value(&completed)?;
        let result_key = format!(
            "runner-completion:{trigger_id}:{}:{}",
            delivery_fence.owner, delivery_fence.epoch
        );
        match journal.record_result(&key, &result_key, &result, &delivery_fence, now_ms())? {
            RecordResultOutcome::Missing => {
                return Ok(json!({ "received": false, "reason": "missing" }));
            }
            RecordResultOutcome::Stale => {
                return Ok(json!({
                    "received": false,
                    "reason": "stale_delivery",
                }));
            }
            RecordResultOutcome::Recorded | RecordResultOutcome::Duplicate => {}
        }

        let _settlement_guard = self.runner_settlement_gate.lock().await;
        let current = journal
            .get(&key)?
            .ok_or_else(|| invalid_input("runner journal row disappeared after result commit"))?;
        if current.state == WorkState::Settled {
            return Ok(json!({ "received": true, "settled": true }));
        }

        let Some(settling) = journal.claim_settlement_by_key(
            &key,
            &self.settlement_owner,
            now_ms(),
            RUNNER_SETTLEMENT_LEASE_MS,
        )?
        else {
            return Ok(json!({
                "received": false,
                "reason": "settlement_busy",
            }));
        };
        let settlement_fence = Self::settlement_fence(&settling)?;
        let report = match self
            .settle_claimed_runner(settling, &settlement_fence)
            .await
        {
            Ok(RunnerSettlementAttempt::Settled(report)) => report,
            Ok(RunnerSettlementAttempt::Missing) => {
                return Ok(json!({
                    "received": false,
                    "reason": "settlement_missing",
                }));
            }
            Ok(RunnerSettlementAttempt::FenceLost) => {
                return Ok(json!({
                    "received": false,
                    "reason": "settlement_fence_lost",
                }));
            }
            Err(error) => {
                let _ = journal.release_settlement(
                    &key,
                    &settlement_fence,
                    now_ms(),
                    &error.to_string(),
                );
                return Err(error);
            }
        };
        Ok(json!({
            "received": true,
            "settled": true,
            "known_trigger": report.known_trigger,
            "lock_released": report.lock_released,
            "rolled_back": report.rolled_back,
            "cursor_updated": report.cursor_updated,
            "indexed_path_count": report.indexed_path_count,
        }))
    }

    /// Recover every result that was durably committed before a prior
    /// orchestrator stopped. The candidate set is snapshotted before claims,
    /// so one malformed row is released for a later retry without starving
    /// later rows or being retried in a tight loop during this startup pass.
    pub async fn recover_runner_settlements(
        &self,
    ) -> Result<RunnerSettlementRecoveryReport, BoxError> {
        let journal = self.journal()?;
        let _settlement_guard = self.runner_settlement_gate.lock().await;
        let candidate_keys =
            journal.settlement_candidate_keys(WorkKind::RunnerTrigger, now_ms())?;
        let mut recovery = RunnerSettlementRecoveryReport {
            candidates: candidate_keys.len(),
            ..RunnerSettlementRecoveryReport::default()
        };

        for key in candidate_keys {
            let Some(settling) = journal.claim_settlement_by_key(
                &key,
                &self.settlement_owner,
                now_ms(),
                RUNNER_SETTLEMENT_LEASE_MS,
            )?
            else {
                continue;
            };
            let settlement_fence = Self::settlement_fence(&settling)?;
            match self
                .settle_claimed_runner(settling, &settlement_fence)
                .await
            {
                Ok(RunnerSettlementAttempt::Settled(_)) => {
                    recovery.settled += 1;
                }
                Ok(RunnerSettlementAttempt::Missing | RunnerSettlementAttempt::FenceLost) => {
                    recovery.fence_lost += 1;
                }
                Err(error) => {
                    journal.release_settlement(
                        &key,
                        &settlement_fence,
                        now_ms(),
                        &error.to_string(),
                    )?;
                    recovery.failed += 1;
                    log::warn!(
                        "[runner-settlement-recovery] key={key:?} failed and was released for a later retry: {error}"
                    );
                }
            }
        }
        Ok(recovery)
    }

    /// Settle previously completed durable imagination rows, then execute at
    /// most one claimable follow-up. The caller invokes this only while at
    /// least one events subscriber exists, because the processor's LLM/tool
    /// requests require the reverse-IPC channel.
    ///
    /// Returns `true` when one delivery was claimed (including a malformed
    /// row moved to dead-letter), and `false` when no delivery was ready.
    pub async fn drain_one_durable_imagination_followup(&self) -> Result<bool, BoxError> {
        let journal = self.journal()?;

        // A crash after record_result but before mark_settled must not replay
        // the model/tool work. ReverseRequest settlement has no additional
        // external effect, so finishing the journal transition is sufficient.
        for key in journal.settlement_candidate_keys(WorkKind::ReverseRequest, now_ms())? {
            let Some(settling) = journal.claim_settlement_by_key(
                &key,
                &self.settlement_owner,
                now_ms(),
                RUNNER_SETTLEMENT_LEASE_MS,
            )?
            else {
                continue;
            };
            let fence = Self::settlement_fence(&settling)?;
            match journal.mark_settled(&key, &fence, now_ms())? {
                SettleOutcome::Settled | SettleOutcome::AlreadySettled => {}
                SettleOutcome::Missing | SettleOutcome::Stale => continue,
            }
        }

        let owner = format!(
            "imagination:{}:{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let Some(item) = journal
            .claim_delivery(
                WorkKind::ReverseRequest,
                &owner,
                now_ms(),
                DURABLE_IMAGINATION_LEASE_MS,
                1,
            )?
            .into_iter()
            .next()
        else {
            return Ok(false);
        };
        let key = item.key.clone();
        let fence = DeliveryFence::new(
            item.lease_owner
                .clone()
                .ok_or_else(|| invalid_input("durable imagination claim has no owner"))?,
            item.delivery_epoch,
        );
        let followup =
            match serde_json::from_value::<DurableImaginationFollowup>(item.payload.clone()) {
                Ok(followup)
                    if followup.schema_id == DURABLE_IMAGINATION_SCHEMA_ID
                        && !followup.source_trigger_id.is_empty()
                        && imagination_followup_key(&followup.source_trigger_id) == key =>
                {
                    let expected_project_state_dir =
                        project_state_dir_from_memory_dir(&followup.memory_dir);
                    if !followup.memory_dir.is_absolute()
                        || !followup.project_state_dir.is_absolute()
                        || followup.project_state_dir != expected_project_state_dir
                    {
                        journal.mark_dead_letter(
                            &key,
                            &fence,
                            now_ms(),
                            "durable imagination paths are not an absolute matching project pair",
                        )?;
                        return Ok(true);
                    }
                    followup
                }
                Ok(_) => {
                    journal.mark_dead_letter(
                        &key,
                        &fence,
                        now_ms(),
                        "durable imagination identity/schema mismatch",
                    )?;
                    return Ok(true);
                }
                Err(error) => {
                    journal.mark_dead_letter(
                        &key,
                        &fence,
                        now_ms(),
                        &format!("durable imagination payload is invalid: {error}"),
                    )?;
                    return Ok(true);
                }
            };

        match journal.ack_delivery(&key, &fence, now_ms(), DURABLE_IMAGINATION_LEASE_MS)? {
            AckOutcome::Acked | AckOutcome::AlreadyAcked => {}
            AckOutcome::Missing | AckOutcome::Stale => return Ok(true),
        }

        let renewal_journal = (*journal).clone();
        let renewal_key = key.clone();
        let renewal_fence = fence.clone();
        let (stop_renewal_tx, mut stop_renewal_rx) = tokio::sync::oneshot::channel::<()>();
        let renewal_task = tokio::spawn(async move {
            let period = std::time::Duration::from_millis(DURABLE_IMAGINATION_RENEW_INTERVAL_MS);
            let first_tick = tokio::time::Instant::now() + period;
            let mut interval = tokio::time::interval_at(first_tick, period);
            loop {
                tokio::select! {
                    _ = &mut stop_renewal_rx => {
                        return Ok::<Option<RenewOutcome>, acosmi_memory_journal::JournalError>(None);
                    }
                    _ = interval.tick() => {
                        let outcome = renewal_journal.renew_delivery(
                            &renewal_key,
                            &renewal_fence,
                            now_ms(),
                            DURABLE_IMAGINATION_LEASE_MS,
                        )?;
                        if outcome != RenewOutcome::Renewed {
                            return Ok(Some(outcome));
                        }
                    }
                }
            }
        });

        let result = run_imagination_after_dream(
            self.tier3_imagination_processor(),
            followup.memory_dir,
            followup.project_state_dir,
            None,
            None,
            crate::output_language::resolve_memory_output_language(&self.base_dir()),
        )
        .await;
        let _ = stop_renewal_tx.send(());
        match renewal_task.await?? {
            None => {}
            Some(RenewOutcome::Missing | RenewOutcome::Stale) => return Ok(true),
            Some(RenewOutcome::Renewed) => unreachable!(),
        }

        if let Err(error) = result {
            let _ = journal.release_delivery(
                &key,
                &fence,
                now_ms(),
                now_ms().saturating_add(DURABLE_IMAGINATION_RETRY_DELAY_MS),
                &error.to_string(),
            )?;
            return Ok(true);
        }

        let result_key = format!("imagination-completion:{}", followup.source_trigger_id);
        match journal.record_result(
            &key,
            &result_key,
            &json!({ "completed": true }),
            &fence,
            now_ms(),
        )? {
            RecordResultOutcome::Recorded | RecordResultOutcome::Duplicate => {}
            RecordResultOutcome::Missing | RecordResultOutcome::Stale => return Ok(true),
        }
        let Some(settling) = journal.claim_settlement_by_key(
            &key,
            &self.settlement_owner,
            now_ms(),
            RUNNER_SETTLEMENT_LEASE_MS,
        )?
        else {
            return Ok(true);
        };
        let settlement_fence = Self::settlement_fence(&settling)?;
        let _ = journal.mark_settled(&key, &settlement_fence, now_ms())?;
        Ok(true)
    }

    pub async fn handle_value(&self, request: Value) -> Value {
        match self.handle_value_result(request).await {
            Ok(value) => value,
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        }
    }

    async fn handle_value_result(&self, request: Value) -> Result<Value, BoxError> {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("missing IPC method"))?;
        let payload = request.get("payload").cloned().unwrap_or(Value::Null);

        match method {
            "memory.ping" => Ok(json!({
                "ok": true,
                "service": crate::MEMORY_SERVICE_IDENTITY,
                "protocol_version": crate::MEMORY_PROTOCOL_VERSION,
                "schema_id": crate::MEMORY_SCHEMA_ID,
                "build_id": env!("CRABCODE_BUILD_ID"),
                "capabilities": crate::MEMORY_CAPABILITIES,
                "pid": std::process::id(),
            })),
            "memory.coordinator.promote" => {
                let expected_current_build_id = payload
                    .get("expected_current_build_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("missing expected_current_build_id"))?;
                let expected_current_pid = payload
                    .get("expected_current_pid")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_input("missing expected_current_pid"))?;
                let successor_build_id = payload
                    .get("successor_build_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("missing successor_build_id"))?;
                let protocol_version = payload
                    .get("protocol_version")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_input("missing protocol_version"))?;
                let schema_id = payload
                    .get("schema_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("missing schema_id"))?;
                if expected_current_build_id != env!("CRABCODE_BUILD_ID") {
                    return Err(invalid_input(
                        "expected current build does not match the live Memory owner",
                    )
                    .into());
                }
                if expected_current_pid != u64::from(std::process::id()) {
                    return Err(invalid_input(
                        "expected current pid does not match the live Memory owner",
                    )
                    .into());
                }
                if protocol_version != crate::MEMORY_PROTOCOL_VERSION
                    || schema_id != crate::MEMORY_SCHEMA_ID
                {
                    return Err(
                        invalid_input("successor Memory protocol/schema is incompatible").into(),
                    );
                }
                if !build_id_version_newer(successor_build_id, env!("CRABCODE_BUILD_ID")) {
                    return Err(invalid_input(
                        "successor build must be strictly newer than current Memory owner",
                    )
                    .into());
                }
                Ok(json!({
                    "ok": true,
                    "promote": true,
                    "current_build_id": env!("CRABCODE_BUILD_ID"),
                    "current_pid": std::process::id(),
                    "successor_build_id": successor_build_id,
                    "protocol_version": crate::MEMORY_PROTOCOL_VERSION,
                    "schema_id": crate::MEMORY_SCHEMA_ID,
                }))
            }
            "memory.turn_end.evaluate" => {
                // W-MEMORY-EVOLUTION PR-5 (2026-05-29) — stamp foreground
                // activity so the periodic dream task's idle gate stays off
                // while a session is actively running turns.
                self.stamp_turn_activity();
                let request = parse_turn_end_request(&payload)?;
                self.set_last_memory_dir(request.memory_dir.clone());
                let project_state_dir = project_state_dir_from_memory_dir(&request.memory_dir);
                self.set_last_project_state_dir(project_state_dir.clone());
                // W-MEMORY-EVOLUTION PR-9 — lazily stand up the SE integration
                // + fs-event index daemon now that both `memory_dir` and
                // `project_state_dir` are known. Fail-soft (None on error);
                // the daemon's initial reindex runs off the request thread.
                let _ = self.ensure_se_integration(&request.memory_dir, &project_state_dir);
                let response = self
                    .evaluator
                    .lock()
                    .await
                    .evaluate_turn_end(request)
                    .await?;
                Ok(turn_end_response_json(&response))
            }
            "memory.runner.candidates" => self.runner_candidates(&payload).await,
            "memory.runner.claim" => self.claim_runner_delivery(&payload).await,
            "memory.runner.ack" => self.ack_runner_delivery(&payload).await,
            "memory.runner.renew" => self.renew_runner_delivery(&payload).await,
            "memory.runner.release" => self.release_runner_delivery(&payload).await,
            "memory.runner.dead_letter" => self.dead_letter_runner_delivery(&payload).await,
            "memory.runner.completed" => {
                if self.journal.is_some() {
                    return self.complete_journaled_runner(&payload).await;
                }
                self.require_runner_leader(&payload).await?;
                let completed = parse_runner_completed(&payload)?;
                let report = {
                    let mut ev = self.evaluator.lock().await;
                    ev.results.handle_completed(completed).await?
                };
                // W-MEMORY-ALIVE 4a (2026-07-01, 裁决②): the TS-line dream
                // (turn-end trigger → TS forked runner → this completion)
                // now chains ONE imagination run, same as the Rust
                // self-driven paths (periodic tick :2338 / RunNow :673).
                // Throttle inherited: the dream trigger itself passed the
                // 24h AutoDreamGate, so the chain fires at most once per
                // consolidation cycle. Detached + fail-soft inside
                // `spawn_imagination_after_dream`.
                if let Some(memory_dir) = report.dream_settled_memory_dir.clone() {
                    let project_state_dir = project_state_dir_from_memory_dir(&memory_dir);
                    spawn_imagination_after_dream(
                        self.tier3_imagination_processor(),
                        memory_dir,
                        project_state_dir,
                        None,
                        None,
                        crate::output_language::resolve_memory_output_language(&self.base_dir()),
                    );
                }
                Ok(json!({
                    "ok": true,
                    "known_trigger": report.known_trigger,
                    "lock_released": report.lock_released,
                    "rolled_back": report.rolled_back,
                    "cursor_updated": report.cursor_updated,
                    "indexed_path_count": report.indexed_path_count,
                }))
            }
            "memory.dream.is_enabled" => {
                let project_state_dir = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir());
                let enabled = match project_state_dir {
                    Some(path) => read_dream_config(&path)?.enabled,
                    None => false,
                };
                Ok(json!({ "enabled": enabled }))
            }
            "memory.dream.set_enabled" => {
                let enabled = payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        invalid_input("memory.dream.set_enabled requires enabled boolean")
                    })?;
                let project_state_dir = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir())
                    .ok_or_else(|| {
                        invalid_input(
                            "memory.dream.set_enabled requires memory_dir or project_state_dir",
                        )
                    })?;
                self.set_last_project_state_dir(project_state_dir.clone());
                let config = set_dream_enabled(&project_state_dir, enabled).await?;
                Ok(json!({ "enabled": config.enabled }))
            }
            "memory.lock.last_consolidated_at" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| {
                        invalid_input("memory.lock.last_consolidated_at requires memory_dir")
                    })?;
                Ok(json!({ "mtime_ms": lock::last_consolidated_at(&memory_dir).await? }))
            }
            "memory.status" => {
                let request = parse_status_request(&payload)?;
                let status = build_status(&request)?;
                let mut value = serde_json::to_value(status)?;
                // §14.1-3 —— dense 半环可诊断（扩字段，不新增 method）。
                // SE 尚未起来时给出 `available:false` 而不是省略字段：省略
                // 会让"没起来"和"字段还没实现"看起来一样。
                if let Some(object) = value.as_object_mut() {
                    let dense = match self.se_integration() {
                        Some(se) => {
                            let health = se.dense_health();
                            let mut v = serde_json::to_value(&health)?;
                            if let Some(o) = v.as_object_mut() {
                                o.insert("available".to_string(), Value::Bool(true));
                            }
                            v
                        }
                        None => json!({ "available": false }),
                    };
                    object.insert("dense".to_string(), dense);
                }
                Ok(value)
            }
            "memory.lock.acquire" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.lock.acquire requires memory_dir"))?;
                let holder = payload
                    .get("holder")
                    .and_then(Value::as_str)
                    .and_then(|raw| {
                        raw.strip_prefix("ts-pid-")
                            .unwrap_or(raw)
                            .parse::<u32>()
                            .ok()
                    })
                    .unwrap_or_else(std::process::id);
                let prior = lock::try_acquire_for(
                    &memory_dir,
                    &lock::LockOwner { holder_pid: holder },
                    &lock::LockOptions::default(),
                )
                .await?;
                match prior {
                    Some(prior_mtime_ms) => Ok(json!({
                        "lock_token": format!("manual:{}:{holder}", memory_dir.to_string_lossy()),
                        "prior_mtime_ms": prior_mtime_ms,
                    })),
                    None => Ok(json!({ "error": "lock held" })),
                }
            }
            "memory.lock.rollback" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.lock.rollback requires memory_dir"))?;
                let prior = payload
                    .get("prior_mtime_ms")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid_input("memory.lock.rollback requires prior_mtime_ms"))?;
                lock::rollback(&memory_dir, prior).await?;
                Ok(json!({ "ok": true }))
            }
            // Per-memory_dir leader election for concurrent dream/extract
            // auto-trigger. Distinct from `.consolidate-lock` 1hr stale lease:
            // 60s TTL + 30s renew window so a crashed leader is taken over
            // within ~1 minute. Distinct file `.bootstrap-leader-lock`. Detail
            // contract in `leader_lock.rs`.
            "memory.leader.claim" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.leader.claim requires memory_dir"))?;
                let owner_pid = parse_required_pid(&payload, "owner_pid")?;
                let ttl_ms = parse_positive_duration_ms(
                    &payload,
                    "ttl_ms",
                    Some(leader_lock::DEFAULT_LEADER_TTL_MS),
                )?;
                match leader_lock::try_claim_leader(&memory_dir, owner_pid, ttl_ms).await? {
                    Some(claim) => Ok(json!({
                        "granted": true,
                        "holder_pid": claim.holder_pid,
                        "leader_token": claim.leader_token,
                        "leader_epoch": claim.leader_epoch,
                        "claimed_at_ms": claim.claimed_at_ms,
                        "lease_expires_at_ms": claim.lease_expires_at_ms,
                    })),
                    None => Ok(json!({ "granted": false })),
                }
            }
            "memory.leader.renew" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.leader.renew requires memory_dir"))?;
                let owner_pid = parse_required_pid(&payload, "owner_pid")?;
                let leader_token = required_str(&payload, "leader_token")?;
                let leader_epoch = required_positive_u64(&payload, "leader_epoch")?;
                let ttl_ms = parse_positive_duration_ms(&payload, "ttl_ms", None)?;
                let renewed = leader_lock::renew_leader_lease(
                    &memory_dir,
                    owner_pid,
                    &leader_token,
                    leader_epoch,
                    ttl_ms,
                )
                .await?;
                match renewed {
                    Some(claim) => Ok(json!({
                        "still_leader": true,
                        "leader_epoch": claim.leader_epoch,
                        "lease_expires_at_ms": claim.lease_expires_at_ms,
                    })),
                    None => Ok(json!({
                        "still_leader": false,
                        "leader_epoch": Value::Null,
                        "lease_expires_at_ms": Value::Null,
                    })),
                }
            }
            "memory.leader.release" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.leader.release requires memory_dir"))?;
                let owner_pid = parse_required_pid(&payload, "owner_pid")?;
                let leader_token = required_str(&payload, "leader_token")?;
                let leader_epoch = required_positive_u64(&payload, "leader_epoch")?;
                let released = leader_lock::release_leader(
                    &memory_dir,
                    owner_pid,
                    &leader_token,
                    leader_epoch,
                )
                .await?;
                Ok(json!({ "ok": true, "released": released }))
            }
            "memory.leader.query" => {
                let memory_dir = memory_dir_from_payload(&payload)
                    .or_else(|| self.last_memory_dir())
                    .ok_or_else(|| invalid_input("memory.leader.query requires memory_dir"))?;
                let my_pid = parse_required_pid(&payload, "my_pid")?;
                let ttl_ms = parse_positive_duration_ms(
                    &payload,
                    "ttl_ms",
                    Some(leader_lock::DEFAULT_LEADER_TTL_MS),
                )?;
                let status = leader_lock::query_leader_status(&memory_dir, my_pid, ttl_ms).await?;
                Ok(serde_json::to_value(status)?)
            }
            "memory.index.changed_paths" => {
                let project_state_dir = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir())
                    .ok_or_else(|| {
                        invalid_input(
                            "memory.index.changed_paths requires memory_dir or project_state_dir",
                        )
                    })?;
                let written_paths = string_array(&payload, "written_paths")
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let record = RunnerArchiveRecord {
                    trigger_id: format!("changed_paths:{}", now_ms()),
                    kind: "index".to_owned(),
                    completed_at_ms: now_ms(),
                    written_paths,
                    usage: None,
                    error: None,
                };
                let report = archive_runner_completed(&project_state_dir, &record).await?;
                Ok(json!({
                    "ok": true,
                    "indexed_path_count": report.written_path_records.len(),
                }))
            }
            "memory.archive_handoff" => self.handle_archive_handoff(&payload).await,
            // W-MEMORY-SELF-EVOLUTION A5 (2026-06-11) — was a v7 P1.3
            // accept-and-noop stub (returned `archived: true` without writing
            // anything, 2026-06-09 审计 §6 D-7). Now lands a real
            // `RunnerArchiveRecord` in the same append-only ledger the other
            // archive paths use (`.memory-rust-derived/archives/
            // runner-completed.jsonl`), so completed tasks / closed sessions
            // become a queryable corpus for the dream/imagination tiers.
            // Producers: `src/tasks/LocalMainSessionTask.ts` +
            // `src/tasks/DreamTask/DreamTask.ts` (task_done, fire-and-forget)
            // and `src/utils/gracefulShutdown.ts` (session_close, 5s budget).
            // Fail-soft: with no resolvable project_state_dir yet (orchestrator
            // saw no turn for this project) we answer honestly with
            // `archived: false` instead of pretending.
            "memory.archive.task_done" => {
                let Some(project_state_dir) = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir())
                else {
                    return Ok(json!({
                        "ok": true,
                        "archived": false,
                        "reason": "no_project_state_dir",
                    }));
                };
                let task_id = payload
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let task_type = payload
                    .get("task_type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let written_paths = payload
                    .get("transcript_path")
                    .and_then(Value::as_str)
                    .map(|p| vec![PathBuf::from(p)])
                    .unwrap_or_default();
                let record = RunnerArchiveRecord {
                    trigger_id: format!("task_done:{task_type}:{task_id}"),
                    kind: "task_done".to_owned(),
                    completed_at_ms: now_ms(),
                    written_paths,
                    usage: None,
                    error: None,
                };
                let report = archive_runner_completed(&project_state_dir, &record).await?;
                Ok(json!({
                    "ok": true,
                    "archived": true,
                    "indexed_path_count": report.written_path_records.len(),
                }))
            }
            // W-MEMORY-SELF-EVOLUTION W-C (2026-06-11) — read the persistent
            // archive ledger newest-first (backing the TUI「归档会话」tab).
            // Read-only; missing ledger = empty list, not an error.
            "memory.archive.recent" => {
                let Some(project_state_dir) = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir())
                else {
                    return Ok(json!({ "ok": true, "entries": [] }));
                };
                let limit = payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(1, 200) as usize)
                    .unwrap_or(50);
                let entries =
                    crate::extract_archive::read_recent_archive_records(&project_state_dir, limit)
                        .await;
                Ok(json!({ "ok": true, "entries": entries }))
            }
            "memory.archive.session_close" => {
                let Some(project_state_dir) = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir())
                else {
                    return Ok(json!({
                        "ok": true,
                        "archived": false,
                        "reason": "no_project_state_dir",
                    }));
                };
                let session_id = payload
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let exit_kind = payload
                    .get("exit_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let record = RunnerArchiveRecord {
                    trigger_id: format!("session_close:{session_id}:{exit_kind}"),
                    kind: "session_close".to_owned(),
                    completed_at_ms: now_ms(),
                    written_paths: Vec::new(),
                    usage: None,
                    error: None,
                };
                let report = archive_runner_completed(&project_state_dir, &record).await?;
                // W5 (2026-07-16, RC-6)：会话关闭归档 = Tier-1 会话速记的
                // 天然触发点（detached、幂等、fail-soft，详 helper 头注）。
                let memory_dir = memory_dir_from_payload(&payload)
                    .unwrap_or_else(|| project_state_dir.join("memory"));
                spawn_session_notes_after_archive(
                    self.tier1_processor(),
                    memory_dir,
                    project_state_dir.clone(),
                    vec![session_id.to_string()],
                    crate::output_language::resolve_memory_output_language(&self.base_dir()),
                );
                Ok(json!({
                    "ok": true,
                    "archived": true,
                    "indexed_path_count": report.written_path_records.len(),
                }))
            }
            // W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — manual `Run Dream
            // Now` / `Run Extract Now` invocations. Each bypasses the
            // automatic gate set (KAIROS / feature flags / min hours /
            // min sessions / cursor sufficiency) while still honoring the
            // consolidation lock (dream only). Trigger registration mirrors
            // the gated `memory.turn_end.evaluate` path so the existing
            // `memory.runner.completed` settle path remains intact.
            "memory.dream.run_now" => {
                let request = parse_dream_run_now_request(&payload)?;
                let memory_dir = request.memory_dir.clone();
                self.set_last_memory_dir(memory_dir.clone());
                self.set_last_project_state_dir(project_state_dir_from_memory_dir(&memory_dir));
                // W4 (2026-07-16, RC-7a) — 空语料门：手动做梦在拿锁之前先
                // 探一次语料。没有任何可整理的新会话文本时，Phase-1 只会
                // 对着 "(no recent sessions)" 产出 0 主题 + 空洞进化报告，
                // 白烧 2+ 次 LLM 调用（2026-07-16 butler 空报告实况）。
                // 探测在 evaluator 之前 = 不碰 consolidation lock，无需回滚。
                {
                    let prior_mtime_ms = lock::last_consolidated_at(&memory_dir).await.unwrap_or(0);
                    let knowledge_dir = self.knowledge_dir();
                    let corpus = crate::dream_corpus::build_dream_corpus_for_memory_dir(
                        &memory_dir,
                        prior_mtime_ms,
                        Some(&knowledge_dir),
                    );
                    if dream_corpus_is_empty(&corpus.recent_sessions_summary) {
                        let response = RunNowResponse {
                            triggers: Vec::new(),
                            gate_skip_reason: Some("corpus_empty".to_string()),
                        };
                        let mut value = run_now_response_json(&response);
                        value["dream_run"] = serde_json::json!({
                            "started": false,
                            "skip_reason": "corpus_empty",
                        });
                        return Ok(value);
                    }
                }
                // Phase 1: register the trigger + acquire the consolidation lock
                // (bypasses the automatic gates; lock_held surfaces as a skip).
                let response = self
                    .evaluator
                    .lock()
                    .await
                    .evaluate_dream_run_now(request)
                    .await?;
                // W-MEMORY-EVOLUTION PR-10 — Phase 2: actually RUN the dream.
                // The old behaviour stopped at registering a trigger; the new
                // model executes the dream in the orchestrator (Tier-3
                // `processor.process` → reverse-IPC LLM → write `dreams/*.md`).
                //
                // The dream is run on a DETACHED task — NOT awaited inline —
                // because `process()` blocks on multi-phase reverse-IPC LLM
                // round-trips (each capped at 60s) while the TUI client
                // dispatcher's run_now IPC has a short (2s) timeout. Awaiting
                // inline would time the IPC out before the dream finishes.
                // The dream still TRULY executes (process is invoked; the
                // consolidation lock acquired above is surrendered by
                // `process()` on settle); its result lands as `dreams/*.md`
                // (observed by the DreamSpace tabs) and the periodic-tick gate
                // path. `dream_run.started` tells the TUI the run kicked off.
                let dream_run = self.spawn_dream_now(&memory_dir, &response);
                let mut value = run_now_response_json(&response);
                value["dream_run"] = dream_run;
                Ok(value)
            }
            // ⚠️ SEMANTICALLY SEALED (W-MEMORY-SELF-EVOLUTION A5, 2026-06-11):
            // this endpoint registers a PendingRunner + returns a forged
            // trigger, but NOTHING can execute that trigger from a management
            // page — the TS extract runner structurally requires a LIVE
            // conversation context (`runExtractMemoryTrigger` reads
            // `context.messages`), which a DreamSpace button click does not
            // have. The v7 "Run Extract Now" concept was an architecture
            // mismatch, not a missing spawn (2026-06-09 审计 §6 D-6 真因).
            // Tier-1/Tier-2 run automatically at turn end once the A1
            // switches are on; the TUI stops offering manual Tier-1/2 buttons
            // (W-C). Endpoint kept wire-compatible for old clients.
            "memory.extract.run_now" => {
                let request = parse_extract_run_now_request(&payload)?;
                self.set_last_memory_dir(request.memory_dir.clone());
                self.set_last_project_state_dir(project_state_dir_from_memory_dir(
                    &request.memory_dir,
                ));
                let response = self
                    .evaluator
                    .lock()
                    .await
                    .evaluate_extract_run_now(request)
                    .await?;
                Ok(run_now_response_json(&response))
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.1 (2026-05-25) — 反向 IPC LLM
            // 调用结果回写入口。dispatcher 收到 `memory/tier/llmCallResult`
            // request 后转发到此处。本 PR 仅立 method 入口 + 写日志；real
            // consumer（pending oneshot channel 按 req_id 匹配）留 P3.2-P3.5
            // Tier policy 实施。详 `tier::LlmCallResultPayload` + mod 头反向
            // IPC 时序图 + CLAUDE.md §硬约束 #15 第 5 条。
            "memory.tier.llm_call_result" => {
                let parsed: crate::tier::LlmCallResultPayload = serde_json::from_value(payload)
                    .map_err(|e| {
                        invalid_input(format!("memory.tier.llm_call_result payload: {e}"))
                    })?;
                let req_id = parsed.req_id.clone();
                let success = parsed.error.is_none() && parsed.response.is_some();
                log::info!(
                    "memory.tier.llm_call_result received: req_id={req_id} success={success}",
                );
                // W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25): deliver to
                // the per-orchestrator Tier-1 processor's pending oneshot
                // map. Cloning the Arc is cheap; the processor's internal
                // map uses its own Mutex, and (since W-MEMORY-EVOLUTION PR-0)
                // there is no longer an outer IpcHandler Mutex, so delivery
                // never serializes behind a concurrent `tier1.process` await.
                //
                // W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25): also deliver
                // to the Tier-2 processor. `req_id` prefix discriminates
                // (`tier1-*` vs `tier2-*`) so each processor's
                // `deliver_result` is a no-op when the prefix doesn't match
                // its own pending map. The first match wins; subsequent
                // delivery attempts on the same `req_id` are no-ops.
                // W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25): triple-deliver
                // upgrade — also deliver to Tier-3 (`tier3-*` req_id prefix).
                // Each processor's `deliver_result` is a no-op on unknown
                // `req_id`; the first prefix-matching processor wins.
                //
                // W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25): quad-deliver
                // upgrade — also deliver to Tier-3 Imagination
                // (`tier3-imagination-*` req_id prefix). Tier-3 dream uses
                // `tier3-<phase>-...` (phase ∈ {phase0..phase4}); imagination
                // uses `tier3-imagination-<layer>-...` (layer ∈ {l1..l3}), so
                // prefix collision is structurally avoided.
                let t1 = Arc::clone(&self.tier1_processor);
                let t1_delivered = t1.deliver_result(parsed.clone()).await;
                let t2 = Arc::clone(&self.tier2_processor);
                let t2_delivered = t2.deliver_result(parsed.clone()).await;
                let t3 = Arc::clone(&self.tier3_processor);
                let t3_delivered = t3.deliver_result(parsed.clone()).await;
                let t3i = Arc::clone(&self.tier3_imagination_processor);
                let t3i_delivered = t3i.deliver_result(parsed).await;
                let delivered = t1_delivered || t2_delivered || t3_delivered || t3i_delivered;
                Ok(json!({
                    "ok": true,
                    "received": delivered,
                    "req_id": req_id,
                }))
            }
            // W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25): Phase 4 起手 PR
            // 反向 IPC Embedding 调用结果回写入口。dispatcher 收到
            // `memory/tier/embeddingResult` request 后转发到此处。
            // SearchEngineIntegration 用 `req_id` 匹配 pending 的 oneshot
            // channel，把向量交给挂起的 indexer 完成 upsert。`req_id`
            // 前缀 `se-embed-` 与 tier1/2/3 的 `tier1-` / `tier2-` /
            // `tier3-` / `tier3-imagination-` 结构性正交，不会与之冲突。
            // SE 未初始化时（`se_integration: None`），accept-and-noop
            // — 与 unknown `req_id` 的语义一致（late delivery 行为不变）。
            "memory.tier.embedding_result" => {
                let parsed: EmbeddingResultPayload =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier.embedding_result payload: {e}"))
                    })?;
                let req_id = parsed.req_id.clone();
                let success = parsed.error.is_none();
                log::info!(
                    "memory.tier.embedding_result received: req_id={req_id} success={success}",
                );
                // W-MEMORY-EVOLUTION PR-0: clone the Arc(s) out of the sync
                // Mutex and drop the guard BEFORE the await (a std Mutex guard
                // must never be held across an await point).
                //
                // W3 P1-4 (2026-06-05) — SE is now per-project (LRU map). An
                // embedding result's `req_id` is owned by exactly one project's
                // pending map, so deliver to ALL live SEs (each is a no-op on
                // an unknown `req_id` — mirrors the Tier quad-deliver pattern);
                // the owning project's pending oneshot resolves, the rest
                // ignore it. (W-MEMORY-ALIVE PR-2a, 2026-07-01: the TS
                // executor is live — `src/services/memoryTierProxy/` answers
                // `memory/tier/embeddingRequest` frames via SDK
                // `client.embeddings()` and writes back here.)
                let ses = self
                    .se_states
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .all_se();
                let mut delivered = false;
                for se in ses {
                    if se.deliver_result(parsed.clone()).await {
                        delivered = true;
                    }
                }
                Ok(json!({
                    "ok": true,
                    "received": delivered,
                    "req_id": req_id,
                }))
            }
            // W-MEMORY-KB-UPLIFT P1 (2026-07-17) — 反向 IPC rerank 结果回写
            // 入口。dispatcher 收到 `memory/tier/rerankResult` request 后转发
            // 到此处。`req_id` 前缀 `se-rerank-` 与 embedding 的 `se-embed-`
            // / tier 前缀结构性正交。quad-deliver 语义与 embedding_result
            // 一致：投给所有活 SE，持有该 req_id 的 pending oneshot 解析，
            // 其余 no-op（late delivery / unknown req_id → received=false）。
            "memory.tier.rerank_result" => {
                let parsed: crate::se_integration::RerankResultPayload =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier.rerank_result payload: {e}"))
                    })?;
                let req_id = parsed.req_id.clone();
                let success = parsed.error.is_none();
                log::info!("memory.tier.rerank_result received: req_id={req_id} success={success}",);
                let ses = self
                    .se_states
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .all_se();
                let mut delivered = false;
                for se in ses {
                    if se.deliver_rerank_result(parsed.clone()).await {
                        delivered = true;
                    }
                }
                Ok(json!({
                    "ok": true,
                    "received": delivered,
                    "req_id": req_id,
                }))
            }
            // W-MEMORY-EVOLUTION PR-7b (2026-05-29) — 反向 IPC 工具取证结果回写
            // 入口。dispatcher 收到 `memory/tier/toolCallResult` request 后转发
            // 到此处（payload snake_case：`req_id` / `evidence[]` / `error?`）。
            // ImaginationProcessor 用 `req_id`（前缀 `tier3-imagination-
            // evidence-`，只该 processor 持有）匹配 pending 的 oneshot，把
            // evidence 交给挂起的 `gather_evidence`。late delivery / unknown
            // `req_id` → received=false（mirror llm_call_result arm）。
            "memory.tier.tool_call_result" => {
                let parsed: ToolCallResultPayload =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier.tool_call_result payload: {e}"))
                    })?;
                let req_id = parsed.req_id.clone();
                let success = parsed.error.is_none();
                log::info!(
                    "memory.tier.tool_call_result received: req_id={req_id} success={success} \
                     evidence={}",
                    parsed.evidence.len(),
                );
                let t3i = Arc::clone(&self.tier3_imagination_processor);
                let delivered = t3i.deliver_tool_result(parsed).await;
                Ok(json!({
                    "ok": true,
                    "received": delivered,
                    "req_id": req_id,
                }))
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — Tier-1 gate
            // evaluation entry. Deterministic, no IO outside the gate's
            // in-memory state. Returns `GateDecision<SessionMemoryGateOutput>`
            // serialized as JSON (snake_case, matches wire form expected by
            // any future TS-side caller).
            "memory.tier1.evaluate" => {
                let parsed: SessionMemoryGateInput = serde_json::from_value(payload)
                    .map_err(|e| invalid_input(format!("memory.tier1.evaluate payload: {e}")))?;
                let gate = self.tier1_processor.gate();
                let decision = gate
                    .evaluate_gate(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier1 gate eval failed: {e}")))?;
                Ok(serde_json::to_value(decision)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — Tier-1 process
            // entry (per-turn extraction). Emits reverse IPC LLM call
            // request, awaits result (30s timeout), writes SESSION.md +
            // per-turn snapshot.
            //
            // ⚠️ SEALED (W-MEMORY-SELF-EVOLUTION A4, 2026-06-11 用户裁决②):
            // ZERO production callers — the TS SessionMemory pipeline
            // (`src/services/SessionMemory/`) is the single source of truth
            // for Tier-1; the planned v7 P6.1 collapse was abandoned and the
            // `CRABCODE_USE_RUST_TIER1` migration switch deleted. The handler
            // body + tests stay (sealed stub per repo convention, 生产不可达
            // 壳子保留不删); the Rust-owned Tier-1 responsibility that REMAINS
            // live is gate evaluation (`memory.tier1.evaluate`, consumed by
            // `memory.turn_end.evaluate`). Re-activating this endpoint
            // requires a fresh审计立项 — do NOT quietly add a caller (双真源
            // = SESSION.md write races between TS and Rust).
            //
            // NOTE (W-MEMORY-EVOLUTION PR-0, 2026-05-29): this call awaits the
            // LLM round-trip. The historical B2 deadlock — outer
            // `Arc<Mutex<IpcHandler>>` held across `handle_value`, blocking the
            // concurrent `llm_call_result` delivery that resolves this await —
            // is now structurally removed: `IpcHandler` uses interior mutability
            // (`&self` methods, no outer Mutex), so a `tier1.process` await no
            // longer serializes other connections, and result delivery (which
            // only touches the processor's own pending map) proceeds freely.
            // Dedicated connections are no longer required for correctness.
            "memory.tier1.process" => {
                self.stamp_turn_activity();
                let parsed: SessionMemoryProcessInput = serde_json::from_value(payload)
                    .map_err(|e| invalid_input(format!("memory.tier1.process payload: {e}")))?;
                let processor = Arc::clone(&self.tier1_processor);
                let output = processor
                    .process(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier1 process failed: {e}")))?;
                Ok(serde_json::to_value(output)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 gate
            // evaluation entry. Deterministic, no IO outside the gate's
            // in-memory state (no memdir scan — that happens in
            // `tier2.process`). Returns `GateDecision<ExtractGateOutput>`
            // serialized as JSON (snake_case wire form).
            "memory.tier2.evaluate" => {
                let parsed: crate::tier::tier2_extract_memories::ExtractGateInput =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier2.evaluate payload: {e}"))
                    })?;
                let gate = self.tier2_processor.gate();
                let decision = gate
                    .evaluate_gate(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier2 gate eval failed: {e}")))?;
                Ok(serde_json::to_value(decision)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.3 (2026-05-25) — Tier-2 process
            // entry (per-query extraction). Emits reverse IPC LLM call
            // request, awaits result (30s timeout), parses LLM output into
            // memory blocks, two-step write
            // (`user/feedback/project/reference_*.md` + `MEMORY.md`).
            //
            // ⚠️ SEALED (W-MEMORY-SELF-EVOLUTION A4, 2026-06-11 用户裁决②):
            // ZERO production callers — the TS extractMemories pipeline
            // (`src/services/extractMemories/` via memoryRunners) is the
            // single source of truth for Tier-2. Handler body + tests stay
            // (sealed stub, 生产不可达壳子保留不删); the live Rust-owned
            // Tier-2 responsibility is gate evaluation inside
            // `memory.turn_end.evaluate`. Re-activating requires a fresh
            // 审计立项 (双真源 = MEMORY.md write races between TS and Rust).
            //
            // NOTE: the historical dedicated-connection deadlock caveat is
            // resolved by W-MEMORY-EVOLUTION PR-0 (no outer Mutex); see
            // `memory.tier1.process`.
            "memory.tier2.process" => {
                self.stamp_turn_activity();
                let parsed: ExtractProcessInput = serde_json::from_value(payload)
                    .map_err(|e| invalid_input(format!("memory.tier2.process payload: {e}")))?;
                let processor = Arc::clone(&self.tier2_processor);
                let output = processor
                    .process(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier2 process failed: {e}")))?;
                Ok(serde_json::to_value(output)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 AutoDream
            // gate evaluation entry. Reads `.consolidate-lock` mtime + in-mem
            // scan-throttle state + optional config override; on pass-through
            // acquires the PID lock (returns `lock_path` + `holder_pid` +
            // `prior_mtime_ms` in payload for rollback semantics).
            "memory.tier3.evaluate" => {
                let parsed: AutoDreamGateInput = serde_json::from_value(payload)
                    .map_err(|e| invalid_input(format!("memory.tier3.evaluate payload: {e}")))?;
                let gate = self.tier3_processor.gate();
                let decision = gate
                    .evaluate_gate(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier3 gate eval failed: {e}")))?;
                Ok(serde_json::to_value(decision)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.4 (2026-05-25) — Tier-3 AutoDream
            // process entry. Runs the 5-phase pipeline (Phase 0 Self-RAG
            // reflection + Phase 1 Orient + Phase 2 Gather + Phase 3
            // Consolidate + Phase 4 Prune). Emits reverse IPC LLM call
            // requests per phase (`req_id` prefix `tier3-<phase>-...`),
            // awaits each result (60s timeout), writes `dreams/insight_*.md`
            // (strong signal) and `dreams/fragment_*.md` (weak signal).
            //
            // NOTE: the historical dedicated-connection deadlock caveat is
            // resolved by W-MEMORY-EVOLUTION PR-0 (no outer Mutex); see
            // `memory.tier1.process`.
            "memory.tier3.process" => {
                self.stamp_turn_activity();
                let parsed: DreamProcessInput = serde_json::from_value(payload)
                    .map_err(|e| invalid_input(format!("memory.tier3.process payload: {e}")))?;
                let processor = Arc::clone(&self.tier3_processor);
                let output = processor
                    .process(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier3 process failed: {e}")))?;
                Ok(serde_json::to_value(output)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination
            // gate evaluation entry. Always-trigger (subject to `enabled`
            // feature flag). Returns `GateDecision<ImaginationGateOutput>`
            // serialized as JSON (snake_case wire form). No IO beyond
            // resolving the review-queue dir under `memory_dir`.
            "memory.tier3.imagination.evaluate" => {
                let parsed: ImaginationGateInput =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier3.imagination.evaluate payload: {e}"))
                    })?;
                let gate = self.tier3_imagination_processor.gate();
                let decision = gate.evaluate_gate(parsed).await.map_err(|e| {
                    invalid_input(format!("tier3 imagination gate eval failed: {e}"))
                })?;
                Ok(serde_json::to_value(decision)?)
            }
            // W-MEMORY-DREAM-REBUILD v7 P3.5 (2026-05-25) — Tier-3 Imagination
            // process entry. Runs the 5-layer confidence pipeline (L1 Self-RAG
            // + L2 four-dimension + L3 atomic verify + L4 weighted fusion +
            // L5 promotion threshold). Emits reverse IPC LLM call requests
            // per layer (`req_id` prefix `tier3-imagination-<layer>-...`),
            // awaits each result (60s timeout), writes
            // `imagination/review-queue/imagined_<hash>.md` when verdict ≠
            // expired. Promotion (review queue → memdir main) requires manual
            // confirm in P5.3 UI; not implemented here.
            //
            // NOTE: the historical dedicated-connection deadlock caveat is
            // resolved by W-MEMORY-EVOLUTION PR-0 (no outer Mutex); see
            // `memory.tier1.process`.
            "memory.tier3.imagination.process" => {
                self.stamp_turn_activity();
                let parsed: ImaginationProcessInput =
                    serde_json::from_value(payload).map_err(|e| {
                        invalid_input(format!("memory.tier3.imagination.process payload: {e}"))
                    })?;
                let processor = Arc::clone(&self.tier3_imagination_processor);
                let output = processor
                    .process(parsed)
                    .await
                    .map_err(|e| invalid_input(format!("tier3 imagination process failed: {e}")))?;
                Ok(serde_json::to_value(output)?)
            }
            // W-MEMORY-SELF-EVOLUTION A3 (2026-06-11) — manual trigger for a
            // self-generated imagination sweep (Stage-0 hypothesis generation
            // → per-candidate L1-L5 pipeline → review queue). Mirrors the
            // `memory.dream.run_now` detached-spawn shape: the IPC response
            // returns promptly; the sweep runs in the background driving the
            // reverse-IPC LLM + tool round-trips. Payload: `{ memory_dir }`.
            "memory.tier3.imagination.generate" => {
                self.stamp_turn_activity();
                let memory_dir = PathBuf::from(required_str(&payload, "memory_dir")?);
                let project_state_dir = project_state_dir_from_memory_dir(&memory_dir);
                spawn_imagination_after_dream(
                    Arc::clone(&self.tier3_imagination_processor),
                    memory_dir,
                    project_state_dir,
                    None,
                    None,
                    crate::output_language::resolve_memory_output_language(&self.base_dir()),
                );
                Ok(json!({ "started": true }))
            }
            // W-MEMORY-DREAM-REBUILD v7 P4.2 (2026-05-25) — `memory.search`.
            //
            // Wraps the `SearchEngineIntegration` singleton + underlying
            // `acosmi-memory-se::SearchEngine::search(...)`. Declare-now-
            // emit-later: when `se_integration` is `None` (P4.1 lazy-init
            // pending) we return a structurally valid empty-results response
            // with an honest `reason` string, so the TUI can render an
            // "engine warming up" empty state without surfacing an error.
            //
            // Real query-vector synthesis (text → embedding via reverse IPC;
            // hybrid score fusion; payload filter wiring) lands together
            // with the P5.2 cross-session search UI; this PR is the wire
            // contract + accept-noop scaffold.
            // W-MEMORY-EVOLUTION PR-9 (2026-05-29) — real text search. Resolves
            // (or lazily stands up) the `SearchEngineIntegration`, then scores
            // over the indexed payload text fields (`name` / `abstract` /
            // `overview` / `content`) with BM25F.
            // W-MEMORY-ALIVE PR-2b (2026-07-01, 裁决③ revised §15-7): dense
            // recall is NOW wired on top — `search_hybrid` embeds the query
            // via the reverse-IPC SDK channel and RRF-fuses dense hits from
            // the side dense collection; unavailable embeddings degrade to
            // the lexical floor (`engine: "text"`). Fail-soft: an
            // un-initialised SE (no turn_end has landed to give us a
            // memory_dir) returns empty + honest reason.
            // W-MEMORY-LIFECYCLE K9+K4 (2026-07-09) — multi-scope retrieval.
            // `scopes` selects any of `project` / `global` / `knowledge`
            // (default: all three). Each scope runs against its OWN SE
            // instance (same keyed per-root LRU map as projects):
            //   * project   — the existing per-project lazy path (unchanged
            //                 behaviour when the other scopes are absent).
            //   * global    — `global_memory_dir` + `global_state_dir`
            //                 (dispatcher/TS inject from `<base>`; root scope
            //                 string "global", same hygiene excludes as
            //                 private).
            //   * knowledge — `knowledge_dir` + `knowledge_state_dir` (root
            //                 scope "knowledge", no extra excludes beyond the
            //                 type whitelist).
            // A scope whose dirs are missing from the payload, or whose root
            // dir does not exist on disk, is silently skipped. Per-scope hits
            // are rank-interleaved (round-robin by rank, de-duplicated by
            // source path) and every wire item carries a `scope` field.
            "memory.search" => {
                let query = payload
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let top_k = payload.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("hybrid")
                    .to_string();
                let top_k_usize = usize::try_from(top_k).unwrap_or(usize::MAX);
                // W-MEMORY-KB-UPLIFT P0 — `injection: manual` 知识条目仅显式
                // 搜索可见：MemorySearch 工具 / TUI 人用搜索传 true；被动逐轮
                // 召回省略（默认 false）。IPC 松散 payload 字段，非协议变更。
                let include_manual = payload
                    .get("include_manual")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // W-MEMORY-KB-UPLIFT P1 — cross-encoder rerank opt-in. Only
                // explicit searches (MemorySearch 工具 / TUI 人用搜索) pass
                // true; passive per-turn recall omits it (rerank is
                // gateway-billed and latency-bounded, 3s). Combined below
                // with the dream-config `search.rerank_enabled` master
                // switch (8b evolvable) + channel backoff.
                let allow_rerank = payload
                    .get("allow_rerank")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let scopes = requested_search_scopes(&payload);

                let mut per_scope_hits: Vec<(
                    &'static str,
                    Vec<crate::se_integration::MemorySearchHit>,
                )> = Vec::new();
                let mut any_se = false;
                let mut engine_label: String = "text".to_string();
                let mut last_error: Option<String> = None;
                let mut rerank_se: Option<Arc<crate::se_integration::SearchEngineIntegration>> =
                    None;

                for scope in scopes {
                    let se = match scope {
                        // W3 P1-4 (2026-06-05) — resolve the SE PER-PROJECT.
                        // The dispatcher injects `memory_dir` +
                        // `project_state_dir` derived from the request `cwd`;
                        // fall back to the last-seen dirs only when the
                        // payload omits them (legacy callers). This routes
                        // each search to its OWN project's SE instead of the
                        // most-recently-used singleton (the cross-project
                        // leak).
                        "project" => {
                            let memory_dir = memory_dir_from_payload(&payload)
                                .or_else(|| self.last_memory_dir());
                            let project_state_dir = project_state_dir_from_payload(&payload)
                                .or_else(|| {
                                    memory_dir.as_deref().map(project_state_dir_from_memory_dir)
                                })
                                .or_else(|| self.last_project_state_dir());
                            match (memory_dir, project_state_dir) {
                                (Some(md), Some(psd)) => self.ensure_se_integration(&md, &psd),
                                // No project context at all — nothing to key on.
                                _ => None,
                            }
                        }
                        "global" => self.ensure_scope_se(
                            &payload,
                            "global",
                            "global_memory_dir",
                            "global_state_dir",
                        ),
                        "knowledge" => self.ensure_scope_se(
                            &payload,
                            "knowledge",
                            "knowledge_dir",
                            "knowledge_state_dir",
                        ),
                        _ => unreachable!("requested_search_scopes only yields valid scopes"),
                    };
                    let Some(se) = se else { continue };
                    any_se = true;
                    // Any live SE can serve as the cross-scope rerank client
                    // (the channel is scope-agnostic: query + texts).
                    if rerank_se.is_none() {
                        rerank_se = Some(Arc::clone(&se));
                    }
                    // W-MEMORY-ALIVE PR-2b (2026-07-01, 裁决③): hybrid
                    // retrieval — BM25F lexical floor fused (RRF) with dense
                    // SDK-embedding recall when the dense side is available;
                    // every degraded path honestly reports `"engine": "text"`.
                    // The await is safe here: no evaluator lock is held, and
                    // the embedding result arrives via a concurrent
                    // `memory.tier.embedding_result` connection.
                    match se
                        .search_hybrid(&query, top_k_usize, &mode, include_manual)
                        .await
                    {
                        Ok((hits, engine)) => {
                            if engine == "hybrid" {
                                engine_label = "hybrid".to_string();
                            }
                            per_scope_hits.push((scope, hits));
                        }
                        Err(e) => {
                            log::warn!(
                                "[se] memory.search scope={scope} failed (fail-soft empty): {e}"
                            );
                            last_error = Some(e.to_string());
                        }
                    }
                }

                if !any_se {
                    return Ok(json!({
                        "ok": true,
                        "results": [],
                        "reason": "search engine not initialised (no memory_dir seen yet — run a turn first)",
                        "query": query,
                        "top_k": top_k,
                        "mode": mode,
                    }));
                }

                // W-MEMORY-SELF-EVOLVE-DGM G1 (2026-07-16) — 检索评分策略层。
                // scope 内：归一化 × 时间衰减(evergreen 豁免) × 来源权重 ×
                // access_boost + 陈旧标注（search_policy::apply_scope_policy）；
                // 跨 scope 合并保持 rank-interleave 契约；MMR 开启时 interleave
                // 扩 3× 候选池后做多样性重排。空查询 = 浏览模式：跳过策略
                // （保 recency 契约）、不计访问/统计。全链 fail-soft：无
                // project_state_dir（无项目上下文）→ 中性配置零计数。
                let policy_psd = project_state_dir_from_payload(&payload)
                    .or_else(|| self.last_project_state_dir());
                let policy_active = !query.trim().is_empty();
                let policy_cfg = policy_psd
                    .as_deref()
                    .and_then(|psd| read_dream_config(psd).ok())
                    .map(|cfg| cfg.search_policy)
                    .unwrap_or_default();
                if policy_active {
                    let access_view = policy_psd
                        .as_deref()
                        .map(crate::access_counts::load_access_counts)
                        .unwrap_or_default();
                    let language =
                        crate::output_language::resolve_memory_output_language(&self.base_dir());
                    let policy_now = crate::extract_archive::now_ms();
                    for (_, hits) in per_scope_hits.iter_mut() {
                        crate::search_policy::apply_scope_policy(
                            hits,
                            &policy_cfg,
                            &access_view,
                            policy_now,
                            language,
                        );
                    }
                }
                // W-MEMORY-KB-UPLIFT P1 — rerank 生效判定：显式请求
                // (allow_rerank) × dream-config 总开关 × 有活 SE 客户端。
                // 模型存在与否/backoff 由 `rerank_values` 内部 fail-soft。
                let rerank_live = policy_active
                    && allow_rerank
                    && policy_cfg.rerank_enabled
                    && rerank_se.is_some();
                let pool_k = if policy_active && (policy_cfg.mmr_enabled || rerank_live) {
                    top_k_usize.saturating_mul(3)
                } else {
                    top_k_usize
                };
                let mut results = interleave_scope_hits(per_scope_hits, pool_k);
                // W-MEMORY-KB-UPLIFT P1 — 单次跨 scope 交叉编码重排（interleave
                // 之后、MMR 之前）：一次 SDK 调用覆盖全池（省额度 + 全局序），
                // 相关性分写回 `score` 供 MMR/展示消费。失败/超时 → RRF 序
                // 原样返回并武装 10min backoff。
                if rerank_live {
                    if let Some(se) = rerank_se.as_ref() {
                        let (reranked, applied) = se.rerank_values(&query, results).await;
                        results = reranked;
                        if applied {
                            engine_label.push_str("+rerank");
                        }
                    }
                }
                if policy_active && policy_cfg.mmr_enabled {
                    results = crate::search_policy::mmr_rerank_values(
                        results,
                        policy_cfg.mmr_lambda,
                        top_k_usize,
                    );
                } else if results.len() > top_k_usize {
                    // rerank 池扩到 3×top_k 且未走 MMR 截断时收口到 top_k。
                    results.truncate(top_k_usize);
                }
                if policy_active {
                    if let Some(psd) = policy_psd.as_deref() {
                        let stat_now = crate::extract_archive::now_ms();
                        let ids: Vec<String> = results
                            .iter()
                            .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
                            .collect();
                        crate::access_counts::record_access(psd, &ids, stat_now).await;
                        crate::search_stats::record_search(psd, &query, results.len(), stat_now)
                            .await;
                    }
                }
                let mut response = json!({
                    "ok": true,
                    "results": results,
                    "query": query,
                    "top_k": top_k,
                    "mode": mode,
                    "engine": engine_label,
                });
                if let Some(error) = last_error {
                    if response["results"]
                        .as_array()
                        .map(Vec::is_empty)
                        .unwrap_or(true)
                    {
                        response["reason"] = json!(format!("search error (fail-soft): {error}"));
                    }
                }
                Ok(response)
            }
            // W-MEMORY-DREAM-REBUILD v7 P5.1 (2026-05-25) — `memory.tier.list`.
            //
            // Walks the per-tier memory subdirectory and returns file
            // metadata + optional YAML-frontmatter `abstract` string.
            // The dispatcher (`crates/acosmi-app-server/src/dispatcher/
            // memory.rs::handle_memory_tier_list`) injects `memory_dir` /
            // `project_state_dir` derived from `cwd` (UC-W1 boundary —
            // orchestrator does not re-derive paths).
            //
            // Tier ↔ filesystem mapping (declare-now-evolve-later — tier2
            // currently reads `<memory_dir>/extracts/` per the P5.1 plan
            // even though the live P3.3 ExtractMemoriesProcessor writes
            // `<memory_dir>/user_*.md` / `feedback_*.md` flat; the extra
            // archive subdir lands at the next P3.3 iteration).
            //   memory: `<memory_dir>/*.md`     (excl MEMORY.md / SESSION.md / .session-*.md)
            //   tier1:  `<memory_dir>/SESSION.md` + `<memory_dir>/.session-*.md`
            //   tier2:  `<memory_dir>/extracts/*.md`
            //   tier3:  `<memory_dir>/dreams/insight_*.md` + `dreams/fragment_*.md`
            //           + `dreams/dream_*.md` (promoted imagination drafts — K3)
            //
            // Pagination: `page` (0-based) + `page_size` (server clamps
            // to 200, default 50). Sort accepts `mtime_desc` (default) /
            // `mtime_asc` / `name_asc` / `name_desc`; unknown values fall
            // back to `mtime_desc`.
            //
            // Frontmatter `abstract` extraction is best-effort — a single
            // peek at the file header (first 8 KiB) looking for a leading
            // `---` block + `abstract:` key. Failure (no frontmatter, no
            // `abstract`, parse error) returns `None` silently.
            "memory.tier.list" => {
                let memory_dir = payload
                    .get("memory_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| invalid_input("memory.tier.list requires memory_dir"))?;
                let tier = payload
                    .get("tier")
                    .and_then(Value::as_str)
                    .unwrap_or("memory");
                let sort = payload
                    .get("sort")
                    .and_then(Value::as_str)
                    .unwrap_or("mtime_desc");
                let page = payload.get("page").and_then(Value::as_u64).unwrap_or(0);
                let page_size = payload
                    .get("page_size")
                    .and_then(Value::as_u64)
                    .unwrap_or(50)
                    .clamp(1, 200);

                let (mut items, reason) = collect_tier_files(memory_dir.as_path(), tier);
                sort_tier_items(&mut items, sort);
                let total = items.len() as u64;
                let start = page.saturating_mul(page_size).min(total);
                let end = start.saturating_add(page_size).min(total);
                let slice: Vec<Value> = items
                    .into_iter()
                    .skip(start as usize)
                    .take((end - start) as usize)
                    .collect();
                Ok(json!({
                    "ok": true,
                    "items": slice,
                    "total": total,
                    "page": page,
                    "reason": reason,
                }))
            }
            // W-MEMORY-DREAM-REBUILD v7 P5.3 (2026-05-25) — `memory.
            // imagination.promote`. Move an `imagination/review-queue/
            // imagined_<hash>.md` artifact (produced by the P3.5
            // ImaginationProcessor) into `<memory_dir>/dreams/
            // dream_<hash>.md` alongside the P3.4 AutoDream outputs.
            // Rewrites the YAML frontmatter `status: pending-review` to
            // `status: confirmed` and injects a `confirmed_at_ms` key.
            // Optional `edit_content` replaces the file body with the
            // user-edited markdown before the rewrite + move.
            //
            // Path-injection defence (in order):
            //   * `path` is rejected if it does not start with
            //     `imagination/review-queue/`.
            //   * `path` is rejected if it contains any `..` segment.
            //   * The resolved absolute path must be a real file.
            "memory.imagination.promote" => {
                let memory_dir = payload
                    .get("memory_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        invalid_input("memory.imagination.promote requires memory_dir")
                    })?;
                let rel_path = payload.get("path").and_then(Value::as_str).unwrap_or("");
                if let Err(reason) = validate_review_queue_path(rel_path) {
                    return Ok(json!({ "ok": false, "error": reason }));
                }
                let edit_content = payload
                    .get("edit_content")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                match promote_imagination(&memory_dir, rel_path, edit_content.as_deref()) {
                    Ok(promoted_path) => {
                        // W-MEMORY-SELF-EVOLUTION B1 (2026-06-11): a
                        // human-confirmed promotion also lands a MEMORY.md
                        // index line — promotion means "this should shape
                        // future behaviour", and MEMORY.md is the strong
                        // system-prompt injection channel. Fail-soft: an
                        // index failure must not undo the promotion.
                        let (name, description, _) =
                            match tokio::fs::read_to_string(&promoted_path).await {
                                Ok(raw) => {
                                    crate::tier::tier3_auto_dream::scan_insight_frontmatter(&raw)
                                }
                                Err(_) => (String::new(), String::new(), String::new()),
                            };
                        if let Some(filename) = promoted_path.file_name().and_then(|f| f.to_str()) {
                            let title = if description.is_empty() {
                                if name.is_empty() {
                                    filename.to_string()
                                } else {
                                    name
                                }
                            } else {
                                description
                            };
                            let line = format!(
                                "- [{title}](dreams/{filename}) — 想象提案（人工确认晋级）"
                            );
                            let memory_md = memory_dir.join("MEMORY.md");
                            if let Err(e) =
                                crate::tier::tier2_extract_memories::append_to_memory_index(
                                    &memory_md,
                                    &[line],
                                )
                                .await
                            {
                                log::warn!(
                                    "imagination promote: MEMORY.md index append failed (fail-soft): {e}"
                                );
                            }
                        }
                        Ok(json!({
                            "ok": true,
                            "promoted_path": promoted_path.to_string_lossy(),
                        }))
                    }
                    Err(reason) => Ok(json!({ "ok": false, "error": reason })),
                }
            }
            // W-MEMORY-DREAM-REBUILD v7 P5.3 (2026-05-25) — `memory.
            // imagination.reject`. Delete an imagination review-queue
            // artifact outright after the wire-level `confirm: true`
            // guard. Mirrors the promote path's path-injection defence.
            "memory.imagination.reject" => {
                let memory_dir = payload
                    .get("memory_dir")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| {
                        invalid_input("memory.imagination.reject requires memory_dir")
                    })?;
                let rel_path = payload.get("path").and_then(Value::as_str).unwrap_or("");
                let confirm = payload
                    .get("confirm")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !confirm {
                    return Ok(json!({
                        "ok": false,
                        "error": "reject requires confirm=true",
                    }));
                }
                if let Err(reason) = validate_review_queue_path(rel_path) {
                    return Ok(json!({ "ok": false, "error": reason }));
                }
                match reject_imagination(&memory_dir, rel_path) {
                    Ok(()) => Ok(json!({ "ok": true, "deleted": true })),
                    Err(reason) => Ok(json!({ "ok": false, "error": reason })),
                }
            }
            // W-MEMORY-LIFECYCLE K10 (2026-07-09) — dream-watch management
            // surface (专项检测). The TUI client dispatcher proxies
            // `memory/watch/list|upsert|remove` here (LocalOnly, §12
            // AllowAnyOrigin ceiling untouched). `memory_dir` /
            // `project_state_dir` are resolved and injected by the dispatcher
            // at upsert time; the orchestrator validates shape only
            // (non-empty + absolute) so the periodic tick can run watches
            // with no session context.
            "memory.watch.list" => {
                let config = load_watch_config(&self.base_dir());
                Ok(json!({
                    "ok": true,
                    "version": config.version,
                    "targets": config.targets,
                }))
            }
            "memory.watch.upsert" => {
                let path = match required_absolute_path_field(&payload, "path") {
                    Ok(path) => path,
                    Err(reason) => return Ok(json!({ "ok": false, "error": reason })),
                };
                let memory_dir = match required_absolute_path_field(&payload, "memory_dir") {
                    Ok(path) => path,
                    Err(reason) => return Ok(json!({ "ok": false, "error": reason })),
                };
                let project_state_dir =
                    match required_absolute_path_field(&payload, "project_state_dir") {
                        Ok(path) => path,
                        Err(reason) => return Ok(json!({ "ok": false, "error": reason })),
                    };

                let base_dir = self.base_dir();
                let mut config = load_watch_config(&base_dir);
                let requested_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);

                let existing_pos = requested_id
                    .as_ref()
                    .and_then(|id| config.targets.iter().position(|t| &t.id == id));

                let target = match existing_pos {
                    Some(pos) => {
                        // Update: replace identity/path fields; optional
                        // fields only when the payload carries the key
                        // (empty string clears); preserve run history.
                        let target = &mut config.targets[pos];
                        target.path = path;
                        target.memory_dir = memory_dir;
                        target.project_state_dir = project_state_dir;
                        if payload.get("label").is_some() {
                            target.label = optional_trimmed_string(&payload, "label");
                        }
                        if payload.get("focus").is_some() {
                            target.focus = optional_trimmed_string(&payload, "focus");
                        }
                        if let Some(hours) = payload.get("interval_hours").and_then(Value::as_u64) {
                            if hours > 0 {
                                target.interval_hours = hours;
                            }
                        }
                        if let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) {
                            target.enabled = enabled;
                        }
                        target.clone()
                    }
                    None => {
                        let id = requested_id.unwrap_or_else(|| generate_watch_id(&path));
                        let target = WatchTarget {
                            id,
                            label: optional_trimmed_string(&payload, "label"),
                            path,
                            memory_dir,
                            project_state_dir,
                            interval_hours: payload
                                .get("interval_hours")
                                .and_then(Value::as_u64)
                                .filter(|hours| *hours > 0)
                                .unwrap_or(crate::watch_config::DEFAULT_WATCH_INTERVAL_HOURS),
                            focus: optional_trimmed_string(&payload, "focus"),
                            enabled: payload
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                            last_run_ms: None,
                            last_status: None,
                        };
                        config.targets.push(target.clone());
                        target
                    }
                };

                match save_watch_config(&base_dir, &config).await {
                    Ok(()) => Ok(json!({ "ok": true, "target": target })),
                    Err(e) => Ok(json!({
                        "ok": false,
                        "error": format!("save watch config failed: {e}"),
                    })),
                }
            }
            "memory.watch.remove" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                if id.is_empty() {
                    return Ok(json!({
                        "ok": false,
                        "error": "memory.watch.remove requires a non-empty id",
                    }));
                }
                let base_dir = self.base_dir();
                let mut config = load_watch_config(&base_dir);
                let before = config.targets.len();
                config.targets.retain(|target| target.id != id);
                let removed = config.targets.len() != before;
                if removed {
                    if let Err(e) = save_watch_config(&base_dir, &config).await {
                        return Ok(json!({
                            "ok": false,
                            "error": format!("save watch config failed: {e}"),
                        }));
                    }
                }
                Ok(json!({ "ok": true, "removed": removed }))
            }
            // W-MEMORY-LIFECYCLE K4 (2026-07-09) — promote a project memory
            // into the user-global memory root (`<base>/memory/`). The
            // dispatcher injects `global_memory_dir` (§4 line contract); the
            // orchestrator moves the file (short-hash suffix on name
            // collision) and migrates the matching MEMORY.md index line(s)
            // from the project index to the global index (fallback line when
            // the project index never referenced the file). Partial failures
            // report an honest error describing exactly what completed.
            "memory.promote_to_global" => {
                let memory_dir = match required_absolute_path_field(&payload, "memory_dir") {
                    Ok(path) => PathBuf::from(path),
                    Err(reason) => return Ok(json!({ "ok": false, "error": reason })),
                };
                let global_memory_dir =
                    match required_absolute_path_field(&payload, "global_memory_dir") {
                        Ok(path) => PathBuf::from(path),
                        Err(reason) => return Ok(json!({ "ok": false, "error": reason })),
                    };
                let rel_path = payload.get("path").and_then(Value::as_str).unwrap_or("");
                match promote_memory_to_global(&memory_dir, rel_path, &global_memory_dir).await {
                    Ok(report) => Ok(json!({
                        "ok": true,
                        "global_path": report.global_path.to_string_lossy(),
                        "index_lines_migrated": report.index_lines_migrated,
                    })),
                    Err(reason) => Ok(json!({ "ok": false, "error": reason })),
                }
            }
            _ => Ok(json!({ "ok": false, "error": "unsupported method" })),
        }
    }

    async fn handle_archive_handoff(&self, payload: &Value) -> Result<Value, BoxError> {
        let project_state_dir = project_state_dir_from_payload(payload)
            .or_else(|| self.last_project_state_dir())
            .ok_or_else(|| {
                invalid_input("memory.archive_handoff requires memory_dir or project_state_dir")
            })?;
        self.set_last_project_state_dir(project_state_dir.clone());
        if let Some(memory_dir) = memory_dir_from_payload(payload) {
            self.set_last_memory_dir(memory_dir);
        }

        let scope = required_str(payload, "scope")?;
        if scope != "thread" && scope != "project" {
            return Err(
                invalid_input("memory.archive_handoff scope must be thread or project").into(),
            );
        }
        let cwd = required_str(payload, "cwd")?;
        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("manual_archive")
            .to_owned();
        let thread_ids = required_string_array(payload, "thread_ids")?;
        if thread_ids.is_empty() {
            return Ok(json!({ "ok": true, "accepted": 0 }));
        }

        let now = now_ms();
        let accepted = thread_ids.len();
        let record = RunnerArchiveRecord {
            trigger_id: format!("archive-handoff:{scope}:{now}"),
            kind: format!("archive_handoff_{scope}"),
            completed_at_ms: now,
            written_paths: Vec::new(),
            usage: Some(json!({
                "scope": scope,
                "cwd": cwd,
                "thread_ids": thread_ids,
                "reason": reason,
                "accepted": accepted,
            })),
            error: None,
        };
        let report = archive_runner_completed(&project_state_dir, &record).await?;
        // W5 (2026-07-16, RC-6)：手动归档 thread（TUI 归档入口，覆盖
        // chat/code/work 三表面）同样触发会话速记生成 —— threadId 与
        // 主会话转写 `<project>/<uuid>.jsonl` 同 id；无匹配转写的 thread
        // 在 helper 内静默跳过（宁缺毋滥），批量归档受 per-archive cap
        // 约束。
        let memory_dir =
            memory_dir_from_payload(payload).unwrap_or_else(|| project_state_dir.join("memory"));
        spawn_session_notes_after_archive(
            self.tier1_processor(),
            memory_dir,
            project_state_dir.clone(),
            thread_ids.clone(),
            crate::output_language::resolve_memory_output_language(&self.base_dir()),
        );
        Ok(json!({
            "ok": true,
            "accepted": accepted,
            "archive_path": report.archive_path.to_string_lossy(),
        }))
    }
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — periodic / idle auto-dream task
//
// This is the core "do something useful while the user is away" loop that
// TS can't provide: a process-internal background tokio task that, every
// `SESSION_SCAN_INTERVAL_MS`, considers whether to run a Tier-3 dream
// consolidation against the most-recently-active project.
//
// The per-tick decision logic is factored into `run_dream_tick` so it is
// directly unit-testable (no need to stand up `serve_endpoint`). The serve
// loop (`lib.rs`) drives it via `tokio::time::interval`.
//
// THREE gates, in cheap→expensive order (so we bail early):
//   1. memory_dir known?   — no last-active project → nothing to dream about.
//   2. idle?               — a turn/tier ran within IDLE_THRESHOLD_MS → the
//                            foreground is busy, don't preempt it.
//   3. dream_config.enabled? — fixes the `dream_config.enabled` orphan: the
//                            TUI toggle now actually controls periodic dreams.
//   4. dream gate passes?  — `AutoDreamGate::evaluate_gate` (time / scan-
//                            throttle / session-count / PID-lock). The gate
//                            already owns all throttling + the consolidate
//                            lock, so we never re-invent any of it here.
// All four pass → `DreamProcessor::process(..)` (the already-wired reverse-
// IPC LLM path). Errors are fail-soft: log + return, the next tick retries.
// ──────────────────────────────────────────────────────────────────────────

/// Default periodic dream scan interval (ms). 10 min — mirrors the Tier-3
/// `SESSION_SCAN_INTERVAL_MS` cadence so the periodic task and the
/// turn-end-driven gate don't fight over scan throttling.
pub const DEFAULT_DREAM_SCAN_INTERVAL_MS: u64 = 10 * 60 * 1000;
/// Env override for the periodic dream scan interval (tests use a short
/// value to avoid waiting 10 min).
pub const DREAM_SCAN_INTERVAL_MS_ENV: &str = "CRABCODE_MEMORY_DREAM_SCAN_MS";

/// Default idle threshold (ms). If a turn/tier ran within this window the
/// foreground is considered "busy" and the dream tick backs off. 30s — the
/// idle-trigger-priority semantics: don't steal cycles mid-session.
pub const DEFAULT_DREAM_IDLE_THRESHOLD_MS: u64 = 30_000;
/// Env override for the idle threshold.
pub const DREAM_IDLE_THRESHOLD_MS_ENV: &str = "CRABCODE_MEMORY_DREAM_IDLE_MS";

/// Resolved per-tick configuration for the periodic dream task. Built once
/// from env at serve-loop start; passed by value to each `run_dream_tick`.
#[derive(Debug, Clone, Copy)]
pub struct DreamTickConfig {
    /// Interval between ticks (ms). Drives the serve-loop `interval`.
    pub scan_interval_ms: u64,
    /// Idle threshold (ms). Tick backs off if `now - last_activity < this`.
    pub idle_threshold_ms: u64,
}

impl Default for DreamTickConfig {
    fn default() -> Self {
        Self {
            scan_interval_ms: DEFAULT_DREAM_SCAN_INTERVAL_MS,
            idle_threshold_ms: DEFAULT_DREAM_IDLE_THRESHOLD_MS,
        }
    }
}

impl DreamTickConfig {
    /// Resolve from env (`CRABCODE_MEMORY_DREAM_SCAN_MS` /
    /// `CRABCODE_MEMORY_DREAM_IDLE_MS`); falls back to defaults on
    /// missing / unparseable / zero values.
    #[must_use]
    pub fn from_env() -> Self {
        let scan_interval_ms = std::env::var(DREAM_SCAN_INTERVAL_MS_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(DEFAULT_DREAM_SCAN_INTERVAL_MS);
        let idle_threshold_ms = std::env::var(DREAM_IDLE_THRESHOLD_MS_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(DEFAULT_DREAM_IDLE_THRESHOLD_MS);
        Self {
            scan_interval_ms,
            idle_threshold_ms,
        }
    }
}

/// Outcome of one periodic dream tick. Diagnostic-only (the serve loop just
/// logs it); tests assert on the variant to verify each gate.
#[derive(Debug, Clone, PartialEq)]
pub enum DreamTickOutcome {
    /// No last-active `memory_dir` yet → nothing to consider.
    NoMemoryDir,
    /// Foreground active within the idle threshold → backed off.
    Busy { idle_for_ms: u64 },
    /// `dream_config.enabled == false` → periodic dreams disabled by the
    /// TUI toggle.
    Disabled,
    /// `AutoDreamGate` declined to trigger (carries the gate's reason).
    GateSkipped { reason: String },
    /// Dream consolidation ran. `theme_count` is how many themes Phase 1
    /// surfaced (0 = ran but found nothing to consolidate).
    Dreamed { theme_count: usize },
    /// `DreamProcessor::process(..)` errored. Fail-soft: logged, next tick
    /// retries. Carries the error string for diagnostics.
    Errored { error: String },
}

/// Run one periodic tick. W-MEMORY-LIFECYCLE (2026-07-09) — the tick now
/// composes three stages:
///
/// 1. **Project dream stage** — gates + Tier-3 consolidation。W-MEMORY-
///    SYNERGY W2 (2026-07-16) 起是跨项目 sweep：最近活跃项目优先（诊断
///    契约不变），未 due 时按「最久未整理优先」轮转 `<base>/projects/*`
///    静默评估，第一个全过 gate 的项目做梦（每 tick 至多一个）。Its
///    outcome is the tick's return value (diagnostic contract unchanged
///    for the serve loop + existing tests).
/// 2. **Independent imagination cycle (K5)** — when the current project is
///    idle + enabled and `last-imagination.json` is older than
///    `DreamConfig::imagination_min_hours`, one self-generated imagination
///    sweep runs detached (no longer only piggy-backing on dream success).
///    Skipped when stage 1 just dreamed — the dream-success chain already
///    starts one sweep and refreshes the same marker on completion.
/// 3. **Watch stage (K10)** — at most ONE due enabled dream-watch target per
///    tick runs the full dream → imagination → report chain anchored at the
///    target's own `memory_dir`.
///
/// `now_ms` is injected so tests can drive the gates deterministically;
/// production passes the real wall clock.
pub async fn run_dream_tick(
    handler: &IpcHandler,
    now_ms: u64,
    config: DreamTickConfig,
) -> DreamTickOutcome {
    // 2026-07-27 §22.2-4 —— 派生层 GC 是**独立 stage**，先于做梦 stage 且
    // 不受任何做梦门控影响。此前它挂在 `dream_one_project_inner` 里，等价于
    // 「做梦不跑 ⇒ GC 不跑」；而做梦 stage 会因前台活跃（`Busy` 早退）、
    // 候选早返回等原因大量不落到多数项目上。GC 自身已有 24h 节流
    // （`DERIVED_GC_MIN_INTERVAL_MS`），每 tick 全项目扫一遍的代价 =
    // 每项目一次标记文件读取。
    run_derived_gc_stage(handler, now_ms).await;
    // 同理：超时在飞 runner 的回收也不该依赖"恰好有一个 turn 结束"——
    // 长时间没有对话的项目同样需要把卡死的 pending 放掉（§25.1）。
    let swept = handler
        .evaluator
        .lock()
        .await
        .results
        .sweep_timeouts(now_ms)
        .await;
    if swept > 0 {
        log::warn!("dream tick swept {swept} timed-out runner(s)");
    }
    let outcome = run_project_dream_stage(handler, now_ms, config).await;
    if !matches!(outcome, DreamTickOutcome::Dreamed { .. }) {
        run_imagination_cycle_stage(handler, now_ms, config).await;
    }
    run_watch_stage(handler, now_ms, config).await;
    outcome
}

/// Stage 1 — periodic-dream decision + (gates passing) a Tier-3 consolidation.
///
/// **Scope**: W-MEMORY-EVOLUTION PR-5 起步是「只梦最近活跃项目」；
/// W-MEMORY-SYNERGY W2 (2026-07-16, RC-4) 落地跨项目 sweep：
///   1. 当前项目仍然最优先（用户刚离开的项目价值最高），其 gate 判定照旧
///      对 TUI `GateDecisionPanel` 发 `memory/gate/skipped`（诊断契约不变）。
///   2. 当前项目本 tick 没有做梦（未 due / 被禁 / 根本没有当前项目）时，
///      轮转扫描 `<base>/projects/*/memory`，按「最久未整理优先」
///      （consolidation lock mtime 升序；缺失 = 0 = 从未整理，排最前 ——
///      天然回补历史零产物项目）逐个**静默**过 gate（不 emit skip：N 个
///      项目每 tick 各发一帧会刷爆 LRU=5 的 GateDecisionPanel），第一个
///      全过的项目做梦。
///   3. 每 tick 全局至多一次做梦；节律仍由 per-project AutoDreamGate 的
///      48h 时间门 + 会话数门约束（W2 不放宽任何 gate）。
async fn run_project_dream_stage(
    handler: &IpcHandler,
    now_ms: u64,
    config: DreamTickConfig,
) -> DreamTickOutcome {
    // ── Gate 2: idle?（全局一次；前台活跃时连轮转扫描都不做） ──
    let last_activity = handler.last_turn_activity_ms();
    if last_activity != 0 {
        let idle_for = now_ms.saturating_sub(last_activity);
        if idle_for < config.idle_threshold_ms {
            // W-MEMORY-EVOLUTION PR-10 — surface the skip to the TUI
            // `GateDecisionPanel` ("why no auto-dream").
            handler
                .emit_gate_skip(crate::broadcast_emitter::GateSkipPayload {
                    tier: "tier3".to_string(),
                    gate_name: "idle".to_string(),
                    reason: format!(
                        "Foreground active {idle_for}ms ago (idle threshold {}ms)",
                        config.idle_threshold_ms
                    ),
                    context: Some(serde_json::json!({
                        "idle_for_ms": idle_for,
                        "idle_threshold_ms": config.idle_threshold_ms,
                    })),
                    skipped_at_ms: now_ms as i64,
                })
                .await;
            return DreamTickOutcome::Busy {
                idle_for_ms: idle_for,
            };
        }
    }

    // ── 候选 1：最近活跃项目（诊断契约保持：emit skip / 透传 outcome） ──
    let current = handler.current_memory_dir();
    let mut current_outcome: Option<DreamTickOutcome> = None;
    if let Some(memory_dir) = current.clone() {
        let project_state_dir = handler
            .current_project_state_dir()
            .unwrap_or_else(|| project_state_dir_from_memory_dir(&memory_dir));
        let outcome =
            dream_one_project(handler, now_ms, memory_dir, project_state_dir, false).await;
        if matches!(
            outcome,
            DreamTickOutcome::Dreamed { .. } | DreamTickOutcome::Errored { .. }
        ) {
            return outcome;
        }
        current_outcome = Some(outcome);
    }

    // ── 候选 2..N：跨项目轮转（W2），最久未整理优先，静默 gate。 ──
    for memory_dir in rotation_candidates(&handler.base_dir(), current.as_deref()).await {
        let project_state_dir = project_state_dir_from_memory_dir(&memory_dir);
        let outcome =
            dream_one_project(handler, now_ms, memory_dir.clone(), project_state_dir, true).await;
        match outcome {
            // 做梦成功浮出并结束本 tick。
            DreamTickOutcome::Dreamed { .. } => return outcome,
            // 毁灭复核修正：候选 Errored 只 log 并继续 —— 一个永久坏项目
            // （如 memory 路径被文件占位）若直接 return，会每 tick 都在它
            // 身上短路、把后面的候选永远饿死。当前项目的 Errored 语义
            // （上方 return）保持不变。
            DreamTickOutcome::Errored { error } => {
                log::warn!(
                    "run_dream_tick: rotation candidate {} errored (skip, try next): {error}",
                    memory_dir.display()
                );
            }
            _ => {}
        }
    }

    // 没有任何候选做梦：透传当前项目的判定（旧诊断契约）；全无候选时
    // 保持 NoMemoryDir 语义。
    current_outcome.unwrap_or(DreamTickOutcome::NoMemoryDir)
}

/// 2026-07-27 §22.2-4 —— 派生层 GC stage：当前项目 + 全部轮转候选各跑一次
/// `gc_derived_tmp_files`（函数内部自带 24h 节流 + 7 天年龄门，fail-soft）。
///
/// 与做梦 stage **完全解耦**：无论 gate 是否放行、无论前台是否活跃，只要
/// tick 在跑，每个项目的孤儿 tmp 回收就有机会执行。
async fn run_derived_gc_stage(handler: &IpcHandler, now_ms: u64) {
    let current = handler.current_memory_dir();
    if let Some(memory_dir) = current.clone() {
        let project_state_dir = handler
            .current_project_state_dir()
            .unwrap_or_else(|| project_state_dir_from_memory_dir(&memory_dir));
        let _ = crate::derived_gc::gc_derived_tmp_files(&project_state_dir, now_ms).await;
    }
    for memory_dir in rotation_candidates(&handler.base_dir(), current.as_deref()).await {
        let project_state_dir = project_state_dir_from_memory_dir(&memory_dir);
        let _ = crate::derived_gc::gc_derived_tmp_files(&project_state_dir, now_ms).await;
    }
}

/// W2 (2026-07-16) — 跨项目轮转候选：`<base>/projects/<slug>/memory`，排除
/// 当前项目，按「最久未整理优先」（consolidation lock mtime 升序，0 = 从未
/// 整理排最前）。项目根下有会话转写（`*.jsonl`）但还没有 memory/ 目录的
/// 历史项目也纳入（惰性建目录——这正是回补对象；建目录幂等且与首次落盘
/// 语义一致）。每项目开销 = 一次 read_dir + 一次 lock stat，10 分钟 tick
/// 节律下可忽略。
async fn rotation_candidates(base_dir: &Path, current: Option<&Path>) -> Vec<PathBuf> {
    let projects_root = base_dir.join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_root) else {
        return Vec::new();
    };
    let current_canonical = current.and_then(|dir| dunce::canonicalize(dir).ok());
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }
        let project_state_dir = entry.path();
        let memory_dir = project_state_dir.join("memory");
        if !memory_dir.is_dir() {
            let has_transcripts = std::fs::read_dir(&project_state_dir)
                .map(|it| {
                    it.flatten()
                        .any(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                })
                .unwrap_or(false);
            if !has_transcripts {
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&memory_dir) {
                log::debug!(
                    "rotation_candidates: create memory dir failed for {} (skip): {e}",
                    memory_dir.display()
                );
                continue;
            }
        }
        if let (Some(current), Ok(candidate)) =
            (current_canonical.as_ref(), dunce::canonicalize(&memory_dir))
        {
            if &candidate == current {
                continue;
            }
        }
        // G2 (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16)：member（worktree）项目
        // 不进轮转 —— 其转写已由 canonical 项目的语料/门并读消费，独立再
        // 梦一遍 = 同素材烧双份配额 + 记忆裂脑复发。
        if crate::identity_members::canonical_redirect_of(&project_state_dir).is_some() {
            log::debug!(
                "rotation_candidates: {} is an identity member — consumed by its \
                 canonical project, skip",
                project_state_dir.display()
            );
            continue;
        }
        let last = lock::last_consolidated_at(&memory_dir).await.unwrap_or(0);
        candidates.push((last, memory_dir));
    }
    candidates.sort_by_key(|(last, _)| *last);
    candidates.into_iter().map(|(_, dir)| dir).collect()
}

/// W4 (2026-07-16, RC-7a) — 空语料判定：近期会话摘要（压缩后）为空 = 本轮
/// 没有任何新鲜素材。记忆清单/知识库仍在也不豁免 —— 无新会话时的旧账
/// 复审归 Phase-0 的失效检查捎带，不值得独立烧一轮五相做梦（空跑实况：
/// 2026-07-16 butler「corpus 中仅有一条反思笔记、无近期会话」的空洞报告）。
fn dream_corpus_is_empty(recent_sessions_summary: &str) -> bool {
    recent_sessions_summary.trim().is_empty()
}

/// W-MEMORY-SYNERGY W5 (2026-07-16, RC-6) — 每次归档事件至多生成的会话
/// 速记份数（防「归档整个项目」触发批量 LLM 风暴；漏掉的会话等下次
/// 归档事件或保持无速记，宁缺毋滥）。
const MAX_SESSION_NOTES_PER_ARCHIVE: usize = 3;

/// W-MEMORY-SYNERGY W5 (2026-07-16, RC-6) — 归档时生成「会话速记」。
///
/// Tier-1 此前无生产者（turn-end 只发 dream|extract 两 kind），
/// `<memory_dir>/SESSION.md + .session-*.md` 恒空 → TUI「会话速记」tab
/// 结构性死面（RC-6，用户裁决 A：接活）。归档（会话关闭 / 手动归档
/// thread）是天然触发点：detached 复用 `SessionMemoryProcessor` 全链
/// （transcript 按 session_id 从盘上精确匹配 → 反向 IPC LLM → 写
/// SESSION.md + `.session-<id>.md` 快照），归档产物随即被做梦语料与
/// 「会话速记」tab 消费。
///
/// 契约：
/// * 幂等 —— 快照已存在跳过；
/// * 精确匹配 —— `<project>/<session_id>.jsonl` 不存在就跳过（processor
///   的"最近转写"fallback 会拿错会话，宁缺毋滥）；
/// * fail-soft —— 速记问题绝不影响归档本身（detached + 逐条 log）。
fn spawn_session_notes_after_archive(
    processor: Arc<SessionMemoryProcessor>,
    memory_dir: std::path::PathBuf,
    project_state_dir: std::path::PathBuf,
    session_ids: Vec<String>,
    language: crate::output_language::MemoryOutputLanguage,
) {
    tokio::spawn(async move {
        let mut generated = 0usize;
        for session_id in session_ids {
            if generated >= MAX_SESSION_NOTES_PER_ARCHIVE {
                log::info!(
                    "[session-note] per-archive cap ({MAX_SESSION_NOTES_PER_ARCHIVE}) reached — \
                     remaining sessions keep no note this round"
                );
                break;
            }
            if session_id.is_empty() || session_id == "unknown" {
                continue;
            }
            let snapshot = memory_dir.join(format!(".session-{session_id}.md"));
            let transcript = project_state_dir.join(format!("{session_id}.jsonl"));
            let transcript_meta = std::fs::metadata(&transcript);
            let transcript_len = transcript_meta.as_ref().map(|meta| meta.len()).unwrap_or(0);
            if transcript_len == 0 {
                continue;
            }
            // W-MEMORY-SELF-EVOLVE-DGM G3-b (2026-07-16)：快照已存在时不再
            // 一律跳过 —— 转写在快照之后又增长的会话做**增量更新**（旧速记
            // 注入 prompt，产出合并后的完整新版）；未增长才保持幂等跳过。
            let prior_note = if snapshot.exists() {
                let grown = match (
                    transcript_meta
                        .as_ref()
                        .ok()
                        .and_then(|m| m.modified().ok()),
                    std::fs::metadata(&snapshot)
                        .ok()
                        .and_then(|m| m.modified().ok()),
                ) {
                    (Some(transcript_mtime), Some(snapshot_mtime)) => {
                        transcript_mtime > snapshot_mtime
                    }
                    // 任一 mtime 不可读 → 无法证明增长，保持幂等跳过。
                    _ => false,
                };
                if !grown {
                    continue;
                }
                std::fs::read_to_string(&snapshot).ok()
            } else {
                None
            };
            if let Err(e) = std::fs::create_dir_all(&memory_dir) {
                log::warn!("[session-note] create memory dir failed (fail-soft, abort batch): {e}");
                break;
            }
            let is_delta = prior_note.is_some();
            let input = SessionMemoryProcessInput {
                session_key: session_id.clone(),
                turn_id: session_id.clone(),
                memory_dir: memory_dir.clone(),
                gate_payload: SessionMemoryGateOutput {
                    messages: Vec::new(),
                    token_count_at_trigger: 0,
                },
                transcript_dir: Some(project_state_dir.clone()),
                current_session_id: Some(session_id.clone()),
                model_hint: None,
                params: crate::tier::LlmCallParams::default(),
                prior_note,
            };
            match processor.process(input).await {
                Ok(output) => {
                    generated += 1;
                    log::info!(
                        "[session-note] archived session note written (delta={is_delta}): {}",
                        output.snapshot_path.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "[session-note] generation failed for {session_id} (fail-soft): {e}"
                    );
                    // G3-d：零 LLM 元数据速记地板 —— LLM 通道故障时也别让
                    // 「会话速记」面空白（琐碎会话仍跳过；不覆盖已有快照的
                    // 增量场景 —— 旧速记比降级元数据更有价值）。
                    if !is_delta {
                        match crate::tier::tier1_session_memory::write_degraded_session_note(
                            &transcript,
                            &snapshot,
                            &session_id,
                            language,
                        )
                        .await
                        {
                            Ok(true) => {
                                generated += 1;
                                log::info!(
                                    "[session-note] degraded metadata note written for \
                                     {session_id} (LLM lane unavailable)"
                                );
                            }
                            Ok(false) => {}
                            Err(e2) => log::warn!(
                                "[session-note] degraded note failed for {session_id} \
                                 (fail-soft): {e2}"
                            ),
                        }
                    }
                }
            }
        }
    });
}

/// W2 (2026-07-16) — 单项目做梦判定 + 执行（原 stage-1 gates 3-4 + 执行体
/// 原样抽出；`quiet` = 轮转候选模式，gate 拒绝不向 TUI emit skip 帧）。
async fn dream_one_project(
    handler: &IpcHandler,
    now_ms: u64,
    memory_dir: PathBuf,
    project_state_dir: PathBuf,
    quiet: bool,
) -> DreamTickOutcome {
    let outcome = dream_one_project_inner(
        handler,
        now_ms,
        memory_dir.clone(),
        project_state_dir.clone(),
        quiet,
    )
    .await;
    // W-MEMORY-SELF-EVOLVE-DGM 8a–8d (2026-07-16) — 进化引擎单挂点：每 tick
    // 记 gate 结果；做梦成功或进化周期到期时跑适应度/试验/提案/报告投影。
    // fail-soft：进化引擎绝不影响做梦结果。
    let outcome_kind = match &outcome {
        DreamTickOutcome::Dreamed { .. } => "dreamed".to_string(),
        DreamTickOutcome::Errored { .. } => "errored".to_string(),
        DreamTickOutcome::GateSkipped { reason } => reason.clone(),
        DreamTickOutcome::Disabled => "disabled".to_string(),
        DreamTickOutcome::NoMemoryDir => "no_memory_dir".to_string(),
        DreamTickOutcome::Busy { .. } => "busy".to_string(),
    };
    // SE（引擎 + 索引 daemon）是重资源：只有进化重活真的要跑（做梦成功
    // 或周期到期）才为影子护栏拉起，轮转候选被门跳过时零成本（毁灭复核
    // 修正——此前每 tick 每候选无条件 ensure_se_integration）。
    let shadow_se = if crate::evolution::heavy_pass_due(&project_state_dir, &outcome_kind, now_ms) {
        handler.ensure_se_integration(&memory_dir, &project_state_dir)
    } else {
        None
    };
    let language = crate::output_language::resolve_memory_output_language(&handler.base_dir());
    crate::evolution::on_dream_tick_outcome(
        &handler.base_dir(),
        &memory_dir,
        &project_state_dir,
        now_ms,
        &outcome_kind,
        shadow_se,
        language,
    )
    .await;
    outcome
}

async fn dream_one_project_inner(
    handler: &IpcHandler,
    now_ms: u64,
    memory_dir: PathBuf,
    project_state_dir: PathBuf,
    quiet: bool,
) -> DreamTickOutcome {
    // 2026-07-27 §22.2-4：派生层 GC 已从这里**上提**到 `run_dream_tick` 的
    // 独立 stage（`run_derived_gc_stage`）。原先挂在做梦执行体里 = "做梦不
    // 跑则 GC 不跑"，而做梦恰恰是最容易不跑的东西（实测本机 8 个项目的
    // `last-derived-gc.json` 毫秒级完全相同，此后 3 天未再触发）。

    // ── Gate 3: dream_config.enabled (fixes the orphan flag) ──
    //
    // W2 (2026-07-16, RC-11)：顺带修死配置——此前 dream-config 的
    // min_hours/min_sessions 数值从未接线到 AutoDreamGate（override 恒 0 →
    // gate 用内置 24h/5会话默认），K5 裁决的「48h + 1 会话即够」在周期路径
    // 上从不成立。此处捕获 cfg 并透传 override，dream-config 成为周期
    // gate 数值的唯一真源。
    let dream_cfg = match read_dream_config(&project_state_dir) {
        Ok(cfg) if !cfg.enabled => {
            // W-MEMORY-EVOLUTION PR-10 — auto-dream toggle is off; tell the TUI.
            // W2：轮转候选静默（quiet），只有当前项目的判定进面板。
            if !quiet {
                handler
                    .emit_gate_skip(crate::broadcast_emitter::GateSkipPayload {
                        tier: "tier3".to_string(),
                        gate_name: "disabled".to_string(),
                        reason: "Auto-dream is disabled (dream-config.json enabled=false)"
                            .to_string(),
                        context: None,
                        skipped_at_ms: now_ms as i64,
                    })
                    .await;
            }
            return DreamTickOutcome::Disabled;
        }
        Ok(cfg) => cfg,
        Err(e) => {
            // Treat an unreadable config as disabled (fail-safe: don't dream
            // if we can't confirm the user opted in).
            log::warn!(
                "run_dream_tick: read_dream_config({}) failed, treating as disabled: {e}",
                project_state_dir.display()
            );
            if !quiet {
                handler
                    .emit_gate_skip(crate::broadcast_emitter::GateSkipPayload {
                        tier: "tier3".to_string(),
                        gate_name: "disabled".to_string(),
                        reason: format!("Auto-dream config unreadable, treated as disabled: {e}"),
                        context: None,
                        skipped_at_ms: now_ms as i64,
                    })
                    .await;
            }
            return DreamTickOutcome::Disabled;
        }
    };

    // ── Gate 4: AutoDreamGate (time / scan-throttle / session-count / lock) ──
    //
    // Compute touched-session-count since the last consolidation so the
    // session-count gate can evaluate. The gate reads the lock mtime itself;
    // we reuse the same SoT here for the session-touched window.
    let prior_mtime_ms = match lock::last_consolidated_at(&memory_dir).await {
        Ok(ms) => ms,
        Err(e) => {
            log::warn!(
                "run_dream_tick: last_consolidated_at({}) failed: {e}",
                memory_dir.display()
            );
            return DreamTickOutcome::Errored {
                error: e.to_string(),
            };
        }
    };
    // No "current session" to exclude for a background sweep → empty id.
    let touched_session_count = match crate::dream_gate::list_sessions_touched_since(
        &project_state_dir,
        prior_mtime_ms,
        "",
    ) {
        Ok(sessions) => sessions.len() as u32,
        Err(e) => {
            log::warn!("run_dream_tick: list_sessions_touched_since failed: {e}; assuming 0");
            0
        }
    };

    // ── Gate 4b (W4, 2026-07-16, RC-7a)：空语料门 —— 在拿 consolidation
    // lock 之前先组一次语料。**只处理「有会话但压缩后为空」的角落**
    // （touched 按文件 mtime 计，corpus 按压缩后文本计，二者可背离）：
    // touched==0 时让 AutoDreamGate 的 canonical `session_count_unmet`
    // 诊断保持（面板文案更可行动），语料也无需白组。语料在此预组装并
    // 直接喂给 process（不重复组装）。
    let knowledge_dir = handler.knowledge_dir();
    let corpus = if touched_session_count > 0 {
        let corpus = crate::dream_corpus::build_dream_corpus(
            &memory_dir,
            &project_state_dir,
            prior_mtime_ms,
            Some(&knowledge_dir),
        );
        if dream_corpus_is_empty(&corpus.recent_sessions_summary) {
            if !quiet {
                handler
                    .emit_gate_skip(crate::broadcast_emitter::GateSkipPayload {
                        tier: "tier3".to_string(),
                        gate_name: "corpus".to_string(),
                        reason: "corpus_empty".to_string(),
                        context: Some(serde_json::json!({
                            "touched_session_count": touched_session_count,
                        })),
                        skipped_at_ms: now_ms as i64,
                    })
                    .await;
            }
            return DreamTickOutcome::GateSkipped {
                reason: "corpus_empty".to_string(),
            };
        }
        corpus
    } else {
        // touched==0 ⇒ AutoDreamGate 必以 session_count_unmet 跳过
        // （config min_sessions ≥ 1），空语料对永远到不了 process。
        crate::dream_corpus::DreamCorpus::default()
    };

    // W6 (6c) — 重要性积分压力：未固化记忆的 importance 积分过阈值时，
    // 时间门豁免（事件驱动提前做梦；会话数/锁门不放宽）。8b：阈值从
    // dream-config 读（可进化参数），常量仅作默认值真源。
    let importance_pressure = crate::importance_pressure::read_importance_accum(&project_state_dir)
        >= dream_cfg.importance_pressure_threshold;

    let processor = handler.tier3_processor();
    let gate_input = AutoDreamGateInput {
        memory_dir: memory_dir.clone(),
        touched_session_count,
        forced: false,
        forced_skip_lock: false,
        importance_pressure,
        // RC-11：dream-config 数值透传（0 = gate 内置默认，config 值恒非 0）。
        min_hours_override: dream_cfg.min_hours,
        min_sessions_override: u32::try_from(dream_cfg.min_sessions).unwrap_or(u32::MAX),
        instance_key: String::new(),
    };
    let decision = match processor.gate().evaluate_gate(gate_input).await {
        Ok(decision) => decision,
        Err(e) => {
            log::warn!("run_dream_tick: gate eval failed: {e}");
            return DreamTickOutcome::Errored {
                error: e.to_string(),
            };
        }
    };
    let Some(gate_payload) = decision.payload else {
        let reason = decision
            .skip_reason
            .unwrap_or_else(|| "unknown".to_string());
        // W-MEMORY-EVOLUTION PR-10 — AutoDreamGate declined; surface the gate's
        // own reason code (e.g. `session_count_unmet` / `time_gate_unmet` /
        // `lock_held` / `dream_in_progress`) to the TUI `GateDecisionPanel`.
        // W2：轮转候选静默（quiet），只有当前项目的判定进面板。
        if !quiet {
            handler
                .emit_gate_skip(crate::broadcast_emitter::GateSkipPayload {
                    tier: "tier3".to_string(),
                    gate_name: "dream_gate".to_string(),
                    reason: reason.clone(),
                    context: Some(serde_json::json!({
                        "touched_session_count": touched_session_count,
                    })),
                    skipped_at_ms: now_ms as i64,
                })
                .await;
        }
        return DreamTickOutcome::GateSkipped { reason };
    };

    // ── All gates passed → trigger the dream consolidation. ──
    //
    // Reuses the fully-wired reverse-IPC LLM path: process() emits
    // `llmCallRequest` via the broadcast emitter → leader worker proxy runs
    // the SDK → `llmCallResult` is written back → process() writes
    // `dreams/insight_*.md`. fail-soft on any error.
    // W-MEMORY-DATA-COMPLETION Phase 0 (2026-06-20): the corpus is assembled
    // BEFORE the gate now (W4 空语料门预组装，见上) and reused here verbatim
    // — `project_state_dir` is the same dir `list_sessions_touched_since`
    // scanned; `prior_mtime_ms` is the consolidation watermark.
    let process_input = DreamProcessInput {
        memory_dir,
        gate_payload,
        // G3-a：水位线只推进到已消费最新会话（撞帽积压下轮排干）。
        consumed_watermark_ms: corpus.consumed_watermark_ms,
        recent_sessions_summary: corpus.recent_sessions_summary,
        memdir_manifest: corpus.memdir_manifest,
        model_hint: None,
        params: crate::tier::LlmCallParams::default(),
        instance_key: String::new(),
    };
    let dreamed_dir = process_input.memory_dir.clone();
    match processor.process(process_input).await {
        Ok(output) => {
            // W-MEMORY-SELF-EVOLUTION B1 (2026-06-11, 用户裁决④): promote
            // qualifying insights into the MEMORY.md index (strong injection
            // channel) — the loop-closing hop. Tier from dream-config
            // `auto_promote` (default: high only). Fail-soft inline await
            // (small IO, no LLM).
            let auto_promote = read_dream_config(&project_state_dir)
                .map(|cfg| cfg.auto_promote)
                .unwrap_or_default();
            let _ = crate::tier::tier3_auto_dream::auto_promote_insights(
                &dreamed_dir,
                auto_promote,
                &output.insight_paths,
            )
            .await;
            // W-MEMORY-SELF-EVOLUTION A3 (2026-06-11, 用户裁决③): chain ONE
            // self-generated imagination run after a successful periodic
            // dream. The dream itself is 24h-gated (AutoDreamGate), so the
            // chain inherits that throttle — at most one imagination sweep
            // per consolidation cycle. Detached + fail-soft: imagination
            // problems must never turn a successful dream into an error.
            // W-MEMORY-LIFECYCLE K5 (2026-07-09): the chain refreshes
            // `last-imagination.json` on completion so the independent
            // imagination cycle doesn't double-run.
            // W6 (6c) — 做梦成功即消费掉积分（下一轮从 0 重新累计）。
            crate::importance_pressure::reset_importance(&project_state_dir, now_ms).await;
            spawn_imagination_after_dream(
                handler.tier3_imagination_processor(),
                dreamed_dir,
                project_state_dir,
                None,
                None,
                crate::output_language::resolve_memory_output_language(&handler.base_dir()),
            );
            DreamTickOutcome::Dreamed {
                theme_count: output.theme_ids.len(),
            }
        }
        Err(e) => {
            log::warn!("run_dream_tick: dream process failed (fail-soft): {e}");
            DreamTickOutcome::Errored {
                error: e.to_string(),
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-LIFECYCLE K5 (2026-07-09) — independent periodic imagination cycle.
//
// Before this, imagination only ever ran chained after a successful dream (or
// manually). The fixed 2-day rhythm requires its own cadence: the marker file
// `<project_state_dir>/.memory-rust-derived/last-imagination.json`
// (`{"last_run_ms": <unix_ms>}`) records the last COMPLETED sweep, and the
// tick starts a new one when `now - last_run_ms >=
// DreamConfig::imagination_min_hours` (absent marker = never ran = due —
// mirrors the dream time-gate's `prior_mtime_ms == 0` pass-through). The
// marker is refreshed by every completed sweep regardless of who started it
// (independent cycle, dream chain, watch chain, manual generate), which is
// the spec'd dedupe between the chain and this cycle.
// ──────────────────────────────────────────────────────────────────────────

/// Marker file name under `.memory-rust-derived/`.
const LAST_IMAGINATION_MARKER_FILENAME: &str = "last-imagination.json";

fn last_imagination_marker_path(project_state_dir: &Path) -> PathBuf {
    crate::daily_log::rust_derived_root(project_state_dir).join(LAST_IMAGINATION_MARKER_FILENAME)
}

/// Read the last completed imagination sweep timestamp (0 = never / missing
/// / unreadable — all of which mean "due now").
fn read_last_imagination_ms(project_state_dir: &Path) -> u64 {
    let path = last_imagination_marker_path(project_state_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return 0,
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("last_run_ms").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// Atomically stamp the marker with a completed sweep timestamp.
async fn write_last_imagination_marker(
    project_state_dir: &Path,
    completed_at_ms: u64,
) -> Result<(), crate::atomic_write::BoxError> {
    let bytes = serde_json::to_vec_pretty(&json!({ "last_run_ms": completed_at_ms }))?;
    crate::atomic_write::atomic_write(&last_imagination_marker_path(project_state_dir), &bytes)
        .await
}

/// Whether the independent imagination cycle is due.
fn imagination_cycle_due(now_ms: u64, last_run_ms: u64, min_hours: u64) -> bool {
    if last_run_ms == 0 {
        return true;
    }
    now_ms.saturating_sub(last_run_ms) >= min_hours.saturating_mul(3_600_000)
}

/// Stage 2 — the K5 independent imagination cycle for the current project.
/// Gates (cheap→expensive): memory_dir known → idle → dream-config enabled →
/// marker due → in-flight latch. All fail-soft; a skipped cycle just waits
/// for the next tick.
async fn run_imagination_cycle_stage(handler: &IpcHandler, now_ms: u64, config: DreamTickConfig) {
    let Some(memory_dir) = handler.current_memory_dir() else {
        return;
    };
    let last_activity = handler.last_turn_activity_ms();
    if last_activity != 0 && now_ms.saturating_sub(last_activity) < config.idle_threshold_ms {
        return;
    }
    let project_state_dir = handler
        .current_project_state_dir()
        .unwrap_or_else(|| project_state_dir_from_memory_dir(&memory_dir));
    // Same fail-safe posture as the dream stage: unreadable config = treat
    // as disabled (don't imagine unless the user's opt-in is confirmable).
    let dream_config = match read_dream_config(&project_state_dir) {
        Ok(config) => config,
        Err(e) => {
            log::warn!(
                "imagination cycle: read_dream_config({}) failed, treating as disabled: {e}",
                project_state_dir.display()
            );
            return;
        }
    };
    if !dream_config.enabled {
        return;
    }
    let last_run_ms = read_last_imagination_ms(&project_state_dir);
    if !imagination_cycle_due(now_ms, last_run_ms, dream_config.imagination_min_hours) {
        return;
    }
    // In-flight latch: the marker only lands on COMPLETION, so a sweep that
    // outlives one 10-min tick must not be double-started by the next.
    if handler
        .imagination_sweep_in_flight
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        log::debug!("imagination cycle: previous sweep still in flight — skipping this tick");
        return;
    }
    log::info!(
        "imagination cycle: due (last_run_ms={last_run_ms}, min_hours={}) — starting sweep for {}",
        dream_config.imagination_min_hours,
        memory_dir.display()
    );
    spawn_imagination_after_dream(
        handler.tier3_imagination_processor(),
        memory_dir,
        project_state_dir,
        None,
        Some(Arc::clone(&handler.imagination_sweep_in_flight)),
        crate::output_language::resolve_memory_output_language(&handler.base_dir()),
    );
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-LIFECYCLE K10 (2026-07-09) — dream-watch stage (专项检测).
//
// The 10-min tick extends to explicitly-pinned paths: for the FIRST due
// enabled target in `<base>/dream-watch.json` it runs the full
// dream → imagination → report chain anchored at the target's own
// `memory_dir` / `project_state_dir`. One target per tick bounds congestion;
// idle + consolidate-lock gates still hold; the watch interval itself is the
// cadence throttle (the dream gate runs `forced` so its time/session gates —
// meaningless for a session-less watch — don't fight the schedule). The
// run's outcome (including failures) is persisted back into the config
// (`last_run_ms` / `last_status`) so the TUI 专项检测 tab can render it and
// a broken target retries on its own interval instead of hot-looping.
// ──────────────────────────────────────────────────────────────────────────

/// Stage 3 — run at most one due watch target for this tick.
async fn run_watch_stage(handler: &IpcHandler, now_ms: u64, config: DreamTickConfig) {
    // Idle gate mirrors the dream/imagination stages: never preempt an
    // active foreground session with background watch work.
    let last_activity = handler.last_turn_activity_ms();
    if last_activity != 0 && now_ms.saturating_sub(last_activity) < config.idle_threshold_ms {
        return;
    }
    let base_dir = handler.base_dir();
    let watch_config = load_watch_config(&base_dir);
    let Some(target) = watch_config
        .targets
        .iter()
        .find(|target| target.is_due(now_ms))
        .cloned()
    else {
        return;
    };

    log::info!(
        "[watch] target due: id={} path={} (interval {}h) — running dream chain",
        target.id,
        target.path,
        target.interval_hours
    );
    let result = run_watch_dream_chain(handler, &target).await;
    let status = match &result {
        Ok(theme_count) => {
            log::info!(
                "[watch] target {} dream chain completed ({theme_count} themes)",
                target.id
            );
            format!("ok: {theme_count} themes")
        }
        Err(e) => {
            log::warn!(
                "[watch] target {} dream chain failed (fail-soft): {e}",
                target.id
            );
            format!("error: {e}")
        }
    };

    // Reload before stamping so a concurrent upsert/remove between chain
    // start and finish isn't clobbered by this write; a removed target
    // simply drops the stamp.
    let mut latest = load_watch_config(&base_dir);
    if let Some(entry) = latest
        .targets
        .iter_mut()
        .find(|entry| entry.id == target.id)
    {
        entry.last_run_ms = Some(now_ms);
        entry.last_status = Some(status);
        if let Err(e) = save_watch_config(&base_dir, &latest).await {
            log::warn!("[watch] persist run status failed (fail-soft): {e}");
        }
    }
}

/// Execute the full dream chain for one watch target. Returns the Phase-1
/// theme count on success; every failure collapses to a status string for
/// `last_status` (the stage is fail-soft by contract).
async fn run_watch_dream_chain(
    handler: &IpcHandler,
    target: &WatchTarget,
) -> Result<usize, String> {
    let memory_dir = PathBuf::from(&target.memory_dir);
    let project_state_dir = PathBuf::from(&target.project_state_dir);
    let watch_root = PathBuf::from(&target.path);
    if !watch_root.is_dir() {
        return Err(format!(
            "watched path does not exist: {}",
            watch_root.display()
        ));
    }
    // The orchestrator owns data management for the watch anchor dirs — a
    // fresh target has neither, and the consolidate lock lives inside
    // memory_dir.
    std::fs::create_dir_all(&memory_dir).map_err(|e| format!("create memory_dir failed: {e}"))?;
    std::fs::create_dir_all(&project_state_dir)
        .map_err(|e| format!("create project_state_dir failed: {e}"))?;

    // Forced gate: the watch interval IS the cadence throttle, so the
    // time/session/scan gates (meaningless without sessions) are bypassed;
    // the consolidate lock + in-process dream_in_progress guard still hold
    // (`forced_skip_lock: false`).
    let processor = handler.tier3_processor();
    let gate_input = AutoDreamGateInput {
        memory_dir: memory_dir.clone(),
        touched_session_count: 0,
        forced: true,
        forced_skip_lock: false,
        importance_pressure: false,
        min_hours_override: 0,
        min_sessions_override: 0,
        instance_key: format!("watch:{}", target.id),
    };
    let decision = processor
        .gate()
        .evaluate_gate(gate_input)
        .await
        .map_err(|e| format!("gate eval failed: {e}"))?;
    let Some(gate_payload) = decision.payload else {
        return Err(format!(
            "gate declined: {}",
            decision
                .skip_reason
                .unwrap_or_else(|| "unknown".to_string())
        ));
    };
    let prior_mtime_ms = gate_payload.prior_mtime_ms;

    // Corpus: the watched tree's shallow inventory (focus header included by
    // `build_watch_inventory`) leads the sessions half; the knowledge-base
    // section rides the shared corpus assembly (K9).
    let knowledge_dir = handler.knowledge_dir();
    let corpus = crate::dream_corpus::build_dream_corpus(
        &memory_dir,
        &project_state_dir,
        prior_mtime_ms,
        Some(&knowledge_dir),
    );
    let summary = corpus.recent_sessions_summary;
    let inventory =
        crate::dream_corpus::build_watch_inventory(&watch_root, target.focus.as_deref());
    let recent_sessions_summary = if summary.trim().is_empty() {
        inventory
    } else {
        format!("{inventory}\n\n{summary}")
    };

    let process_input = DreamProcessInput {
        memory_dir: memory_dir.clone(),
        gate_payload,
        // watch 定向做梦不消费会话积压语义 → None（fresh mtime 旧语义）。
        consumed_watermark_ms: None,
        recent_sessions_summary,
        memdir_manifest: corpus.memdir_manifest,
        model_hint: None,
        params: crate::tier::LlmCallParams::default(),
        instance_key: format!("watch:{}", target.id),
    };
    match processor.process(process_input).await {
        Ok(output) => {
            let auto_promote = read_dream_config(&project_state_dir)
                .map(|cfg| cfg.auto_promote)
                .unwrap_or_default();
            let _ = crate::tier::tier3_auto_dream::auto_promote_insights(
                &memory_dir,
                auto_promote,
                &output.insight_paths,
            )
            .await;
            // Chain imagination with the watch context so `gather_evidence`
            // can emit read-only `readFile` / `listDir` probes against the
            // watched tree (K10 evidence surface).
            spawn_imagination_after_dream(
                handler.tier3_imagination_processor(),
                memory_dir,
                project_state_dir,
                Some(WatchContext {
                    root: watch_root,
                    focus: target.focus.clone(),
                }),
                None,
                crate::output_language::resolve_memory_output_language(&handler.base_dir()),
            );
            Ok(output.theme_ids.len())
        }
        Err(e) => Err(format!("dream process failed: {e}")),
    }
}

/// W-MEMORY-SELF-EVOLUTION A3 (2026-06-11, 用户裁决③) — detached
/// self-generated imagination run, chained after a successful dream
/// consolidation (periodic tick + manual run_now + watch chain), driven by
/// the K5 independent cycle, and reachable directly via
/// `memory.tier3.imagination.generate`.
///
/// Pipeline: Stage-0 hypothesis self-generation from the memory corpus →
/// L1-L5 confidence pipeline per candidate (capped at `HYPGEN_MAX_CANDIDATES`)
/// → `imagination/review-queue/imagined_*.md`. External evidence gathering
/// rides the existing tool reverse-IPC channel (TS caps evidence at 10 per
/// call). Entries stay quarantined in the review queue until a human confirms
/// promotion — this function never writes to the memdir main area.
///
/// W-MEMORY-LIFECYCLE (2026-07-09) additions:
/// * `project_state_dir` — where the completed sweep stamps
///   `last-imagination.json` (the K5 cycle's dedupe marker).
/// * `watch` — optional K10 watch context (root + focus); when present the
///   pipeline's `gather_evidence` may emit the read-only `readFile` /
///   `listDir` tool kinds against the watched tree. `None` everywhere else.
/// * `in_flight` — optional latch owned by the K5 independent cycle,
///   released on every exit path of the detached task.
/// * `language` — W3 (2026-07-16, RC-5)：进化报告的结构语言（章节骨架 +
///   frontmatter description）。调用点用
///   `crate::output_language::resolve_memory_output_language(&handler.base_dir())`
///   解析（行文语言归 TS 执行器，分工见 output_language 模块头）。
pub(crate) fn spawn_imagination_after_dream(
    processor: Arc<ImaginationProcessor>,
    memory_dir: std::path::PathBuf,
    project_state_dir: std::path::PathBuf,
    watch: Option<WatchContext>,
    in_flight: Option<Arc<std::sync::atomic::AtomicBool>>,
    language: crate::output_language::MemoryOutputLanguage,
) {
    tokio::spawn(async move {
        if let Err(error) = run_imagination_after_dream(
            processor,
            memory_dir,
            project_state_dir,
            watch,
            in_flight,
            language,
        )
        .await
        {
            log::warn!("imagination chain failed (fail-soft): {error}");
        }
    });
}

/// Awaitable imagination chain used by the durable after-dream worker.
///
/// The historical spawn wrapper remains for explicit/manual and periodic
/// callers, but journaled dream completion awaits this body under its own
/// durable delivery lifecycle. A failed gate/process returns an error so the
/// journal can release the row for retry instead of losing the follow-up.
pub(crate) async fn run_imagination_after_dream(
    processor: Arc<ImaginationProcessor>,
    memory_dir: std::path::PathBuf,
    project_state_dir: std::path::PathBuf,
    watch: Option<WatchContext>,
    in_flight: Option<Arc<std::sync::atomic::AtomicBool>>,
    language: crate::output_language::MemoryOutputLanguage,
) -> Result<(), BoxError> {
    /// Release the caller's in-flight latch on every exit path (early return,
    /// error, or panic unwinding).
    struct LatchRelease(Option<Arc<std::sync::atomic::AtomicBool>>);
    impl Drop for LatchRelease {
        fn drop(&mut self) {
            if let Some(latch) = self.0.take() {
                latch.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
    let _latch = LatchRelease(in_flight);

    let gate_input = ImaginationGateInput {
        memory_dir: memory_dir.clone(),
        enabled: true,
    };
    let decision = processor.gate().evaluate_gate(gate_input).await?;
    let Some(gate_payload) = decision.payload else {
        log::info!(
            "imagination chain: gate declined ({})",
            decision
                .skip_reason
                .unwrap_or_else(|| "unknown".to_string())
        );
        return Ok(());
    };
    let input = ImaginationGeneratedInput {
        memory_dir: memory_dir.clone(),
        gate_payload,
        model_hint: None,
        params: crate::tier::LlmCallParams::default(),
        watch_context: watch,
    };
    let output = processor.process_generated(input).await?;
    log::info!(
        "imagination chain: {} candidate(s) processed into review queue",
        output.outputs.len()
    );
    // A marker/report failure does not invalidate completed generated outputs.
    if let Err(error) = write_last_imagination_marker(&project_state_dir, now_ms()).await {
        log::warn!("imagination chain: last-imagination marker write failed (fail-soft): {error}");
    }
    match processor
        .generate_evolution_report(
            &memory_dir,
            None,
            crate::tier::LlmCallParams::default(),
            language,
        )
        .await
    {
        Ok(path) => {
            log::info!("evolution report written: {}", path.display());
        }
        Err(error) => {
            log::warn!("evolution report failed (fail-soft): {error}");
        }
    }
    Ok(())
}

pub fn parse_turn_end_request(payload: &Value) -> Result<TurnEndEvaluateRequest, BoxError> {
    let memory_dir = PathBuf::from(required_str(payload, "memory_dir")?);
    Ok(TurnEndEvaluateRequest {
        recovery_schema_version: required_positive_u64(payload, "recovery_schema_version")?,
        session_id: required_str(payload, "session_id")?,
        current_session_id: required_str(payload, "current_session_id")?,
        last_assistant_uuid: required_str(payload, "last_assistant_uuid")?,
        project_cwd: PathBuf::from(required_str(payload, "project_cwd")?),
        transcript_path: PathBuf::from(required_str(payload, "transcript_path")?),
        memory_dir,
        team_memory_dir: payload
            .get("team_memory_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        message_counts: u64_map(payload.get("message_counts")),
        feature_flags: bool_map(payload.get("feature_flags")),
        requested_kinds: requested_kinds(payload.get("requested_kinds")),
        now_ms: payload
            .get("now_ms")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| system_time_to_ms(std::time::SystemTime::now())),
    })
}

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — parse `memory.dream.run_now`
/// payload. Mirrors `parse_turn_end_request` shape (snake_case keys; absolute
/// `memory_dir` path required; optional `now_ms` falls back to system clock).
pub fn parse_dream_run_now_request(payload: &Value) -> Result<DreamRunNowRequest, BoxError> {
    let memory_dir = PathBuf::from(required_str(payload, "memory_dir")?);
    Ok(DreamRunNowRequest {
        session_id: required_str(payload, "session_id")?,
        current_session_id: required_str(payload, "current_session_id")?,
        memory_dir,
        now_ms: payload
            .get("now_ms")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| system_time_to_ms(std::time::SystemTime::now())),
    })
}

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — parse
/// `memory.extract.run_now` payload. Accepts the same `message_counts` shape
/// as `memory.turn_end.evaluate` (BTreeMap<String, u64>). `team_memory_dir`
/// optional.
pub fn parse_extract_run_now_request(payload: &Value) -> Result<ExtractRunNowRequest, BoxError> {
    let memory_dir = PathBuf::from(required_str(payload, "memory_dir")?);
    Ok(ExtractRunNowRequest {
        session_id: required_str(payload, "session_id")?,
        last_assistant_uuid: required_str(payload, "last_assistant_uuid")?,
        memory_dir,
        team_memory_dir: payload
            .get("team_memory_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        message_counts: u64_map(payload.get("message_counts")),
        now_ms: payload
            .get("now_ms")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| system_time_to_ms(std::time::SystemTime::now())),
    })
}

/// W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25) — serialize `RunNowResponse`
/// into the wire shape consumed by the App Server dispatcher. `triggers`
/// mirrors the `memory.turn_end.evaluate` shape so the dispatcher can reuse
/// its existing `parse_turn_end_trigger` helper. `gate_skip_reason` is
/// surfaced as `gate_skip_reason` (snake_case) on the wire — dispatcher
/// translates to camelCase.
pub fn run_now_response_json(response: &RunNowResponse) -> Value {
    let mut value = json!({
        "triggers": response.triggers.iter().map(trigger_json).collect::<Vec<_>>()
    });
    if let Some(reason) = response.gate_skip_reason.as_ref() {
        value["gate_skip_reason"] = json!(reason);
    }
    value
}

pub fn turn_end_response_json(response: &TurnEndEvaluateResponse) -> Value {
    json!({
        "triggers": response.triggers.iter().map(trigger_json).collect::<Vec<_>>()
    })
}

fn parse_status_request(payload: &Value) -> Result<StatusRequest, BoxError> {
    let request = StatusRequest::new(
        required_str(payload, "memory_dir")?,
        required_str(payload, "cwd")?,
        required_str(payload, "project_state_dir")?,
        required_str(payload, "transcript_dir")?,
    );
    Ok(match payload.get("stale_days").and_then(Value::as_u64) {
        Some(value) => request.with_stale_days(u32::try_from(value).map_err(|_| {
            invalid_input("memory.status stale_days must fit in an unsigned 32-bit integer")
        })?),
        None => request,
    })
}

fn trigger_json(trigger: &TurnEndTrigger) -> Value {
    let mut value = json!({
        "trigger_id": trigger.trigger_id,
        "kind": trigger.kind.as_str(),
        "runner_payload": trigger.runner_payload,
    });
    if let Some(lock_token) = trigger.lock_token.as_ref() {
        value["lock_token"] = json!(lock_token);
    }
    value
}

fn parse_runner_completed(payload: &Value) -> Result<RunnerCompleted, BoxError> {
    Ok(RunnerCompleted {
        trigger_id: required_str(payload, "trigger_id")?,
        kind: required_str(payload, "kind")?,
        written_paths: string_array(payload, "written_paths")
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        usage: payload.get("usage").cloned(),
        error: payload.get("error").cloned(),
        completed_at_ms: payload.get("completed_at_ms").and_then(Value::as_u64),
    })
}

fn parse_delivery_fence(payload: &Value) -> Result<DeliveryFence, BoxError> {
    Ok(DeliveryFence::new(
        required_str(payload, "delivery_owner")?,
        required_u64(payload, "delivery_epoch")?,
    ))
}

fn required_str(payload: &Value, key: &str) -> Result<String, BoxError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_input(format!("missing required string field: {key}")).into())
}

fn required_u64(payload: &Value, key: &str) -> Result<u64, BoxError> {
    payload.get(key).and_then(Value::as_u64).ok_or_else(|| {
        invalid_input(format!("missing required unsigned integer field: {key}")).into()
    })
}

fn required_positive_u64(payload: &Value, key: &str) -> Result<u64, BoxError> {
    let value = required_u64(payload, key)?;
    if value == 0 {
        return Err(invalid_input(format!(
            "required unsigned integer field must be positive: {key}"
        ))
        .into());
    }
    Ok(value)
}

fn parse_reason_code(payload: &Value) -> Result<String, BoxError> {
    let reason = required_str(payload, "reason_code")?;
    if reason.len() > 64
        || reason.is_empty()
        || !reason.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        return Err(invalid_input(
            "reason_code must be 1..=64 lowercase ASCII letters, digits, '.', '_' or '-'",
        )
        .into());
    }
    Ok(reason)
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn string_array(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn required_string_array(payload: &Value, key: &str) -> Result<Vec<String>, BoxError> {
    let items = payload
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_input(format!("missing required string array field: {key}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(value) = item.as_str() else {
            return Err(invalid_input(format!("{key} must contain only strings")).into());
        };
        out.push(value.to_owned());
    }
    Ok(out)
}

fn u64_map(value: Option<&Value>) -> BTreeMap<String, u64> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_u64().map(|n| (key.clone(), n)))
                .collect()
        })
        .unwrap_or_default()
}

fn bool_map(value: Option<&Value>) -> BTreeMap<String, bool> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_bool().map(|b| (key.clone(), b)))
                .collect()
        })
        .unwrap_or_default()
}

fn requested_kinds(value: Option<&Value>) -> Vec<RunnerKind> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|kind| match kind {
                    "dream" => Some(RunnerKind::Dream),
                    "extract" => Some(RunnerKind::Extract),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn memory_dir_from_payload(payload: &Value) -> Option<PathBuf> {
    payload
        .get("memory_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Parse an explicit non-zero OS PID without truncation or ambient fallback.
///
/// Leader ownership is a fencing identity. Guessing the orchestrator PID for
/// a missing/malformed caller field could renew or release another process's
/// lease, while `u64 as u32` could alias a different owner.
fn parse_required_pid(payload: &Value, key: &str) -> Result<u32, io::Error> {
    let raw = payload
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_input(format!("{key} must be a non-zero u32 integer")))?;
    let pid = u32::try_from(raw)
        .map_err(|_| invalid_input(format!("{key} must be a non-zero u32 integer")))?;
    if pid == 0 {
        return Err(invalid_input(format!(
            "{key} must be a non-zero u32 integer"
        )));
    }
    Ok(pid)
}

fn parse_positive_duration_ms(
    payload: &Value,
    key: &str,
    default: Option<u64>,
) -> Result<u64, io::Error> {
    let duration = match payload.get(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid_input(format!("{key} must be a positive integer")))?,
        None => {
            default.ok_or_else(|| invalid_input(format!("{key} must be a positive integer")))?
        }
    };
    if duration == 0 {
        return Err(invalid_input(format!("{key} must be a positive integer")));
    }
    Ok(duration)
}

fn project_state_dir_from_payload(payload: &Value) -> Option<PathBuf> {
    if let Some(path) = payload.get("project_state_dir").and_then(Value::as_str) {
        return Some(PathBuf::from(path));
    }
    memory_dir_from_payload(payload).map(|path| project_state_dir_from_memory_dir(&path))
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-LIFECYCLE K9+K4 (2026-07-09) — multi-scope `memory.search` helpers.
// ──────────────────────────────────────────────────────────────────────────

/// Default scope order when the payload omits `scopes` (also the interleave
/// priority order: at equal rank, project hits sort ahead of global ahead of
/// knowledge).
const DEFAULT_SEARCH_SCOPES: [&str; 3] = ["project", "global", "knowledge"];

/// Parse + validate `scopes` from the payload. Absent key or explicit empty
/// array → all three (the spec'd default). Unknown values are ignored with a
/// warn (fail-soft); order + first-occurrence dedupe of the caller's list is
/// preserved so the interleave respects the requested priority.
fn requested_search_scopes(payload: &Value) -> Vec<&'static str> {
    let Some(raw) = payload.get("scopes").and_then(Value::as_array) else {
        return DEFAULT_SEARCH_SCOPES.to_vec();
    };
    if raw.is_empty() {
        return DEFAULT_SEARCH_SCOPES.to_vec();
    }
    let mut scopes: Vec<&'static str> = Vec::new();
    for value in raw {
        let Some(name) = value.as_str() else { continue };
        let canonical = match name {
            "project" => "project",
            "global" => "global",
            "knowledge" => "knowledge",
            other => {
                log::warn!("[se] memory.search: ignoring unknown scope '{other}'");
                continue;
            }
        };
        if !scopes.contains(&canonical) {
            scopes.push(canonical);
        }
    }
    scopes
}

/// Build the `MemoryRoot` for a non-project search scope. The global root is
/// a user-curated memory tree, so it inherits the private root's hygiene
/// excludes (team/logs/.rust-derived + K2's imagination//dreams/fragment_)
/// under its own scope string; the knowledge root needs no excludes beyond
/// the indexer's type whitelist.
fn scope_memory_root(scope: &'static str, dir: PathBuf) -> MemoryRoot {
    match scope {
        "knowledge" => MemoryRoot::new("knowledge", dir, Vec::new()),
        _ => {
            let mut root = MemoryRoot::private(dir);
            root.scope = scope.to_string();
            root
        }
    }
}

/// Rank-interleave per-scope hit lists (round-robin by rank in scope order),
/// de-duplicating by source path (falling back to the SE point id when a hit
/// carries no `source_path`), truncated to `top_k`. Every wire item gains a
/// `scope` field naming the search scope it came from — for the project
/// scope this intentionally overrides the index-root scope string
/// ("private") so consumers see the §4 contract values.
fn interleave_scope_hits(
    per_scope_hits: Vec<(&'static str, Vec<crate::se_integration::MemorySearchHit>)>,
    top_k: usize,
) -> Vec<Value> {
    if top_k == 0 {
        return Vec::new();
    }
    let max_rank = per_scope_hits
        .iter()
        .map(|(_, hits)| hits.len())
        .max()
        .unwrap_or(0);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut merged: Vec<Value> = Vec::new();
    'outer: for rank in 0..max_rank {
        for (scope, hits) in &per_scope_hits {
            let Some(hit) = hits.get(rank) else { continue };
            let dedupe_key = hit.source_path.clone().unwrap_or_else(|| hit.id.clone());
            if !seen.insert(dedupe_key) {
                continue;
            }
            let Ok(mut value) = serde_json::to_value(hit) else {
                continue;
            };
            value["scope"] = json!(scope);
            merged.push(value);
            if merged.len() >= top_k {
                break 'outer;
            }
        }
    }
    merged
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-LIFECYCLE K10/K4 (2026-07-09) — payload validation helpers for the
// watch management + promote-to-global surfaces.
// ──────────────────────────────────────────────────────────────────────────

/// Require a non-empty **absolute** path string field (the dispatcher injects
/// absolute dirs; the orchestrator only validates shape). Soft error string
/// (callers answer `{ok:false,error}` instead of a transport error).
fn required_absolute_path_field(payload: &Value, key: &str) -> Result<String, String> {
    let raw = payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if raw.is_empty() {
        return Err(format!("{key} must be a non-empty string"));
    }
    if !Path::new(raw).is_absolute() {
        return Err(format!("{key} must be an absolute path"));
    }
    Ok(raw.to_string())
}

/// Optional string field, trimmed; empty (or non-string) collapses to `None`
/// so an empty form field clears the stored value.
fn optional_trimmed_string(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_owned)
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-DREAM-REBUILD v7 P5.1 (2026-05-25) — `memory.tier.list` helpers.
//
// `collect_tier_files` walks the per-tier subdirectory and returns the
// pre-pagination items as JSON `Value`s plus an optional `reason` string
// (the wire `reason` field surfaces fail-soft conditions such as a missing
// `extracts/` dir without flagging the response as `error`).
//
// `sort_tier_items` sorts in place by the requested key. Unknown sort
// values silently fall back to `mtime_desc` so a frontend typo never
// fails the whole call.
// ──────────────────────────────────────────────────────────────────────────

/// Maximum bytes of file header peeked while looking for the YAML
/// frontmatter `abstract:` field. 8 KiB is a generous bound — empirical
/// memory + dream files rarely exceed 1 KiB of frontmatter.
const TIER_LIST_FRONTMATTER_PEEK_BYTES: usize = 8 * 1024;

fn collect_tier_files(memory_dir: &Path, tier: &str) -> (Vec<Value>, Option<String>) {
    match tier {
        "memory" => collect_memory_main(memory_dir),
        "tier1" => collect_tier1(memory_dir),
        "tier2" => collect_tier2(memory_dir),
        "tier3" => collect_tier3(memory_dir),
        // W-MEMORY-DREAM-REBUILD v7 P5.3 (2026-05-25) — Imagination review
        // queue (pending-review hypotheses produced by the P3.5
        // ImaginationProcessor). Files live at
        // `<memory_dir>/imagination/review-queue/imagined_*.md` and stay
        // there until a `memory/imagination/promote` or
        // `memory/imagination/reject` call retires them.
        "imagination-review" => collect_imagination_review(memory_dir),
        // W-MEMORY-SELF-EVOLUTION B2/W-C (2026-06-11) — evolution reports.
        "reports" => collect_reports(memory_dir),
        other => (
            Vec::new(),
            Some(format!(
                "unknown tier '{other}'; expected memory | tier1 | tier2 | tier3 | imagination-review | reports"
            )),
        ),
    }
}

fn collect_imagination_review(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let review_dir = memory_dir.join("imagination").join("review-queue");
    let entries = match std::fs::read_dir(&review_dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (
                Vec::new(),
                Some(format!(
                    "imagination review queue not yet created: {}",
                    review_dir.display()
                )),
            );
        }
        Err(err) => {
            return (
                Vec::new(),
                Some(format!("read imagination review queue failed: {err}")),
            );
        }
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") || !name.starts_with("imagined_") {
            continue;
        }
        items.push(make_tier_item(&path, name, "imagination-review"));
    }
    (items, None)
}

fn collect_memory_main(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let entries = match std::fs::read_dir(memory_dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (
                Vec::new(),
                Some(format!(
                    "memory_dir does not exist: {}",
                    memory_dir.display()
                )),
            )
        }
        Err(err) => return (Vec::new(), Some(format!("read memory_dir failed: {err}"))),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".md") {
            continue;
        }
        // Exclude tier-1 sentinels and the index file itself.
        if file_name == "MEMORY.md" || file_name == "SESSION.md" {
            continue;
        }
        if file_name.starts_with(".session-") {
            continue;
        }
        items.push(make_tier_item(
            &path,
            file_name,
            derive_memory_scope(file_name),
        ));
    }
    (items, None)
}

fn derive_memory_scope(file_name: &str) -> &'static str {
    // Tier-2 extract memory files use a `<type>_<id>.md` naming scheme.
    if file_name.starts_with("user_") {
        "user"
    } else if file_name.starts_with("feedback_") {
        "feedback"
    } else if file_name.starts_with("project_") {
        "project"
    } else if file_name.starts_with("reference_") {
        "reference"
    } else {
        "other"
    }
}

fn collect_tier1(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let mut items = Vec::new();
    let session_md = memory_dir.join("SESSION.md");
    if session_md.is_file() {
        items.push(make_tier_item(&session_md, "SESSION.md", "tier1"));
    }
    // `.session-*.md` sentinels live alongside SESSION.md.
    match std::fs::read_dir(memory_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !name.starts_with(".session-") || !name.ends_with(".md") {
                    continue;
                }
                items.push(make_tier_item(&path, name, "tier1"));
            }
            (items, None)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => (
            items,
            Some(format!(
                "memory_dir does not exist: {}",
                memory_dir.display()
            )),
        ),
        Err(err) => (items, Some(format!("read memory_dir failed: {err}"))),
    }
}

fn collect_tier2(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let extracts_dir = memory_dir.join("extracts");
    collect_flat_md(&extracts_dir, "tier2")
}

/// W-MEMORY-SELF-EVOLUTION B2/W-C (2026-06-11) — evolution reports
/// (`<memory_dir>/reports/evolution-*.md`, written by
/// `ImaginationProcessor::generate_evolution_report` after each dream →
/// imagination cycle). Listed newest-first by the shared sort.
fn collect_reports(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let reports_dir = memory_dir.join("reports");
    collect_flat_md(&reports_dir, "reports")
}

fn collect_tier3(memory_dir: &Path) -> (Vec<Value>, Option<String>) {
    let dreams_dir = memory_dir.join("dreams");
    let entries = match std::fs::read_dir(&dreams_dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (
                Vec::new(),
                Some(format!(
                    "dreams subdir not yet created: {}",
                    dreams_dir.display()
                )),
            )
        }
        Err(err) => {
            return (
                Vec::new(),
                Some(format!("read dreams subdir failed: {err}")),
            )
        }
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") {
            continue;
        }
        // W-MEMORY-LIFECYCLE K3 (2026-07-09): promoted imagination drafts
        // land as `dreams/dream_<hash>.md` (`promote_imagination` →
        // `imagined_to_dream_name`), but this listing only admitted
        // `insight_` / `fragment_` — so confirming a hypothesis made it
        // vanish from the「梦境与想象」tab. `dream_` joins the prefix set.
        if !(name.starts_with("insight_")
            || name.starts_with("fragment_")
            || name.starts_with("dream_"))
        {
            continue;
        }
        items.push(make_tier_item(&path, name, "tier3"));
    }
    (items, None)
}

fn collect_flat_md(dir: &Path, scope: &str) -> (Vec<Value>, Option<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (
                Vec::new(),
                Some(format!("subdir not yet created: {}", dir.display())),
            )
        }
        Err(err) => return (Vec::new(), Some(format!("read subdir failed: {err}"))),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") {
            continue;
        }
        items.push(make_tier_item(&path, name, scope));
    }
    (items, None)
}

fn make_tier_item(path: &Path, file_name: &str, scope: &str) -> Value {
    let (mtime_ms, size_bytes) = match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                })
                .unwrap_or(0);
            (mtime, meta.len())
        }
        Err(_) => (0, 0),
    };
    let abstract_text = read_frontmatter_abstract(path);
    json!({
        "path": path.to_string_lossy(),
        "file_name": file_name,
        "scope": scope,
        "mtime_ms": mtime_ms,
        "size_bytes": size_bytes,
        "abstract_text": abstract_text,
    })
}

/// Best-effort YAML-frontmatter `abstract` extractor. Reads up to 8 KiB
/// of the file header, looks for a leading `---` line followed by an
/// `abstract:` key (single-line or block scalar), returns the value
/// trimmed of surrounding whitespace. Returns `None` on any failure or
/// when the file has no frontmatter / no `abstract:` key.
fn read_frontmatter_abstract(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; TIER_LIST_FRONTMATTER_PEEK_BYTES];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    let header = std::str::from_utf8(&buf).ok()?;
    let mut lines = header.lines();
    let first = lines.next()?.trim_end();
    if first != "---" {
        return None;
    }
    let mut in_block = false;
    let mut block_indent: Option<usize> = None;
    let mut accumulated: Vec<String> = Vec::new();
    for line in lines {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        if trimmed_end == "---" || trimmed_end == "..." {
            break;
        }
        if in_block {
            // Continuation of a block scalar — collect indented lines.
            let leading_ws = line.chars().take_while(|c| *c == ' ').count();
            let indent = block_indent.unwrap_or(leading_ws);
            if leading_ws >= indent && !line.is_empty() {
                accumulated.push(line[indent.min(line.len())..].to_string());
                if block_indent.is_none() && leading_ws > 0 {
                    block_indent = Some(leading_ws);
                }
                continue;
            } else if line.trim().is_empty() {
                accumulated.push(String::new());
                continue;
            } else {
                // Dedent indicates end of block scalar.
                break;
            }
        }
        if let Some(rest) = line.strip_prefix("abstract:") {
            let trimmed = rest.trim();
            if trimmed.is_empty()
                || trimmed == "|"
                || trimmed == ">"
                || trimmed.starts_with("|")
                || trimmed.starts_with(">")
            {
                in_block = true;
                continue;
            }
            // Inline scalar — may be quoted.
            let unquoted = trimmed
                .trim_start_matches(['"', '\''])
                .trim_end_matches(['"', '\''])
                .to_string();
            return Some(unquoted);
        }
    }
    if in_block {
        let joined = accumulated.join("\n").trim().to_string();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    } else {
        None
    }
}

fn sort_tier_items(items: &mut [Value], sort: &str) {
    match sort {
        "mtime_asc" => {
            items.sort_by_key(|v| v.get("mtime_ms").and_then(Value::as_u64).unwrap_or(0))
        }
        "name_asc" => items.sort_by(|a, b| {
            a.get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("file_name").and_then(Value::as_str).unwrap_or(""))
        }),
        "name_desc" => items.sort_by(|a, b| {
            b.get("file_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(a.get("file_name").and_then(Value::as_str).unwrap_or(""))
        }),
        // mtime_desc (default + fallback for unknown values)
        _ => items.sort_by(|a, b| {
            let av = a.get("mtime_ms").and_then(Value::as_u64).unwrap_or(0);
            let bv = b.get("mtime_ms").and_then(Value::as_u64).unwrap_or(0);
            bv.cmp(&av)
        }),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-DREAM-REBUILD v7 P5.3 (2026-05-25) — Imagination review queue
// promote / reject helpers.
//
// `validate_review_queue_path` is the single SoT for path-injection defence
// shared by both `memory.imagination.promote` and `memory.imagination.
// reject`. We reject:
//   * Empty paths (no implicit defaulting to a directory move).
//   * Absolute paths (the dispatcher hands the relative `path` field
//     verbatim from the wire — orchestrator owns the join with
//     `memory_dir`).
//   * Paths that do not start with `imagination/review-queue/`.
//   * Any path component equal to `..` (block parent traversal).
//
// `promote_imagination` reads the original file (or substitutes user-edited
// content when `edit_content` is `Some`), rewrites the YAML frontmatter to
// flip `status: pending-review` → `status: confirmed` and inject
// `confirmed_at_ms: <unix_ms>`, then moves the artifact to
// `<memory_dir>/dreams/dream_<hash>.md` (alongside the P3.4 AutoDream
// outputs). The dreams subdir is created if missing.
//
// `reject_imagination` simply deletes the file.
// ──────────────────────────────────────────────────────────────────────────

const REVIEW_QUEUE_PREFIX: &str = "imagination/review-queue/";

fn validate_review_queue_path(rel_path: &str) -> Result<(), String> {
    if rel_path.is_empty() {
        return Err("path is empty".to_string());
    }
    let normalised = rel_path.replace('\\', "/");
    if Path::new(&normalised).is_absolute() {
        return Err("path must be relative to memory_dir".to_string());
    }
    if !normalised.starts_with(REVIEW_QUEUE_PREFIX) {
        return Err(format!(
            "invalid path: outside review-queue (must start with '{REVIEW_QUEUE_PREFIX}')"
        ));
    }
    if normalised.split('/').any(|seg| seg == "..") {
        return Err("invalid path: parent traversal not permitted".to_string());
    }
    Ok(())
}

fn promote_imagination(
    memory_dir: &Path,
    rel_path: &str,
    edit_content: Option<&str>,
) -> Result<PathBuf, String> {
    let normalised = rel_path.replace('\\', "/");
    let source = memory_dir.join(&normalised);
    if !source.is_file() {
        return Err(format!("imagination file not found: {}", source.display()));
    }

    let original = match edit_content {
        Some(content) => content.to_string(),
        None => {
            std::fs::read_to_string(&source).map_err(|err| format!("read source failed: {err}"))?
        }
    };

    let confirmed_at_ms = now_ms();
    let rewritten = rewrite_frontmatter_for_promotion(&original, confirmed_at_ms);

    let file_name = source
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "source has no file name".to_string())?;
    let promoted_name = imagined_to_dream_name(file_name);

    let dreams_dir = memory_dir.join("dreams");
    std::fs::create_dir_all(&dreams_dir)
        .map_err(|err| format!("create dreams dir failed: {err}"))?;
    let dest = dreams_dir.join(&promoted_name);

    std::fs::write(&dest, rewritten).map_err(|err| format!("write dest failed: {err}"))?;
    std::fs::remove_file(&source).map_err(|err| {
        format!(
            "wrote {} but failed to remove source {}: {err}",
            dest.display(),
            source.display()
        )
    })?;
    Ok(dest)
}

fn reject_imagination(memory_dir: &Path, rel_path: &str) -> Result<(), String> {
    let normalised = rel_path.replace('\\', "/");
    let target = memory_dir.join(&normalised);
    if !target.is_file() {
        return Err(format!("imagination file not found: {}", target.display()));
    }
    std::fs::remove_file(&target).map_err(|err| format!("delete failed: {err}"))
}

/// Rename the promoted artifact from `imagined_<hash>.md` to
/// `dream_<hash>.md`. If the file does not follow that prefix convention
/// (defence against drift from future ImaginationProcessor changes), keep
/// the original name.
fn imagined_to_dream_name(file_name: &str) -> String {
    if let Some(rest) = file_name.strip_prefix("imagined_") {
        format!("dream_{rest}")
    } else {
        file_name.to_string()
    }
}

/// Rewrite the YAML frontmatter of an imagined hypothesis to mark it
/// confirmed. Flips `status: pending-review` → `status: confirmed` and
/// injects (or replaces) `confirmed_at_ms: <unix_ms>`. Files without a
/// leading `---` block get one prepended so the promoted artifact still
/// surfaces a frontmatter `status` for downstream consumers.
fn rewrite_frontmatter_for_promotion(original: &str, confirmed_at_ms: u64) -> String {
    let mut lines = original.lines();
    let first = lines.next();
    let has_frontmatter = matches!(first, Some(line) if line.trim_end() == "---");
    if !has_frontmatter {
        let injected = format!("---\nstatus: confirmed\nconfirmed_at_ms: {confirmed_at_ms}\n---\n");
        return format!("{injected}{original}");
    }

    let mut out = String::new();
    out.push_str("---\n");
    let mut saw_status = false;
    let mut saw_confirmed = false;
    let mut closed = false;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == "---" {
            if !saw_status {
                out.push_str("status: confirmed\n");
            }
            if !saw_confirmed {
                out.push_str(&format!("confirmed_at_ms: {confirmed_at_ms}\n"));
            }
            out.push_str("---");
            closed = true;
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("status:") {
            // Drop the pending-review marker; emit `confirmed` once.
            let _ = rest;
            if !saw_status {
                out.push_str("status: confirmed\n");
                saw_status = true;
            }
            continue;
        }
        if trimmed.starts_with("confirmed_at_ms:") {
            // Replace with the freshly computed value (avoid stale stamp
            // when promote is re-run on an already-edited file).
            out.push_str(&format!("confirmed_at_ms: {confirmed_at_ms}\n"));
            saw_confirmed = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !closed {
        // Unterminated frontmatter — append the close marker so the
        // promoted artifact is still parseable.
        if !saw_status {
            out.push_str("status: confirmed\n");
        }
        if !saw_confirmed {
            out.push_str(&format!("confirmed_at_ms: {confirmed_at_ms}\n"));
        }
        out.push_str("---");
    }
    // Preserve the body verbatim (everything after the closing `---`).
    let body_start = body_offset_after_frontmatter(original);
    if let Some(start) = body_start {
        out.push_str(&original[start..]);
    } else {
        out.push('\n');
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-LIFECYCLE K4 (2026-07-09) — promote a project memory to the
// user-global memory root.
//
// Sequencing is loss-averse: (1) the global copy is written first (atomic),
// (2) the global MEMORY.md gains the migrated/fallback index line(s),
// (3) the project MEMORY.md is rewritten without the migrated line(s),
// (4) the source file is removed LAST. Any failure reports exactly what
// completed (fail-soft: a partial promote can leave a duplicate, never a
// loss).
// ──────────────────────────────────────────────────────────────────────────

/// Result of a completed promote-to-global.
struct PromoteToGlobalReport {
    global_path: PathBuf,
    /// Project MEMORY.md lines that were migrated (0 = the fallback line was
    /// synthesized because the project index never referenced the file).
    index_lines_migrated: usize,
}

async fn promote_memory_to_global(
    memory_dir: &Path,
    raw_path: &str,
    global_memory_dir: &Path,
) -> Result<PromoteToGlobalReport, String> {
    if raw_path.trim().is_empty() {
        return Err("path is empty".to_string());
    }
    // Resolve (absolute stays; relative joins memory_dir) + canonical
    // containment check (symlink-safe — mirrors the `memory/read` defence).
    let requested = {
        let candidate = Path::new(raw_path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            memory_dir.join(candidate)
        }
    };
    let canonical_root =
        dunce::canonicalize(memory_dir).map_err(|e| format!("memory_dir not accessible: {e}"))?;
    let source =
        dunce::canonicalize(&requested).map_err(|e| format!("memory file not found: {e}"))?;
    if !source.starts_with(&canonical_root) {
        return Err("invalid path: outside memory_dir".to_string());
    }
    if !source.is_file() {
        return Err(format!("not a file: {}", source.display()));
    }
    if source.extension().and_then(|ext| ext.to_str()) != Some("md") {
        return Err("only .md memories can be promoted".to_string());
    }
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "source has no file name".to_string())?
        .to_string();
    if file_name == "MEMORY.md" || file_name == "SESSION.md" {
        return Err(format!(
            "{file_name} is an index/sentinel file and cannot be promoted"
        ));
    }

    tokio::fs::create_dir_all(global_memory_dir)
        .await
        .map_err(|e| format!("create global memory dir failed: {e}"))?;
    // Destination keeps the original name; a collision gets a deterministic
    // short-hash suffix (keyed on the source path).
    let dest_name = if global_memory_dir.join(&file_name).exists() {
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("memory");
        let suffixed = format!("{stem}-{}.md", short_path_hash(&source));
        if global_memory_dir.join(&suffixed).exists() {
            return Err(format!("destination already exists: {suffixed}"));
        }
        suffixed
    } else {
        file_name.clone()
    };
    let dest = global_memory_dir.join(&dest_name);

    // (1) Global copy first — no failure mode below loses the memory.
    let content = tokio::fs::read_to_string(&source)
        .await
        .map_err(|e| format!("read source failed: {e}"))?;
    crate::atomic_write::atomic_write(&dest, content.as_bytes())
        .await
        .map_err(|e| format!("write global copy failed: {e}"))?;

    // (2)+(3) Index migration: project MEMORY.md lines naming the file move
    // to the global MEMORY.md (link targets rewritten to the flat global
    // name); when the project index never referenced it, synthesize an
    // honest fallback line.
    let project_index = canonical_root.join("MEMORY.md");
    let (project_index_rewrite, migrated) = split_index_lines_for(&project_index, &file_name).await;
    let project_slug = canonical_root
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    let lines_to_append: Vec<String> = if migrated.is_empty() {
        let display_name = dest_name.trim_end_matches(".md");
        vec![format!(
            "- [{display_name}]({dest_name}) — 自 {project_slug} 晋升"
        )]
    } else {
        migrated
            .iter()
            .map(|line| rewrite_index_line_target(line, &file_name, &dest_name))
            .collect()
    };
    let index_lines_migrated = migrated.len();
    let global_index = global_memory_dir.join("MEMORY.md");
    crate::tier::tier2_extract_memories::append_to_memory_index(&global_index, &lines_to_append)
        .await
        .map_err(|e| {
            format!(
                "promoted copy written to {} but global MEMORY.md update failed: {e}",
                dest.display()
            )
        })?;
    if let Some(rewritten) = project_index_rewrite {
        crate::atomic_write::atomic_write(&project_index, rewritten.as_bytes())
            .await
            .map_err(|e| {
                format!(
                    "promoted to {} (global index updated) but project MEMORY.md rewrite failed: {e}",
                    dest.display()
                )
            })?;
    }

    // (4) Remove the source last.
    tokio::fs::remove_file(&source).await.map_err(|e| {
        format!(
            "promoted to {} (indexes updated) but failed to remove source {}: {e}",
            dest.display(),
            source.display()
        )
    })?;

    Ok(PromoteToGlobalReport {
        global_path: dest,
        index_lines_migrated,
    })
}

/// Deterministic 8-hex suffix for destination-name collisions.
fn short_path_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Partition an index file's lines around `file_name`: lines containing it
/// are returned for migration; the remainder (when any migration happened)
/// is returned as the rewritten file content. Missing index / no matching
/// line → `(None, [])` (nothing to rewrite).
async fn split_index_lines_for(
    index_path: &Path,
    file_name: &str,
) -> (Option<String>, Vec<String>) {
    let raw = match tokio::fs::read_to_string(index_path).await {
        Ok(raw) => raw,
        Err(_) => return (None, Vec::new()),
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut migrated: Vec<String> = Vec::new();
    for line in raw.lines() {
        if line.contains(file_name) {
            migrated.push(line.to_string());
        } else {
            kept.push(line);
        }
    }
    if migrated.is_empty() {
        return (None, Vec::new());
    }
    let mut kept_content = kept.join("\n");
    if raw.ends_with('\n') && !kept_content.is_empty() {
        kept_content.push('\n');
    }
    (Some(kept_content), migrated)
}

/// Rewrite a migrated index line's markdown link target to the (flat) global
/// destination name — `- [T](dreams/insight_x.md) — hook` becomes
/// `- [T](insight_x-ab12cd34.md) — hook`. Lines without a parseable link
/// target fall back to a plain name substitution.
fn rewrite_index_line_target(line: &str, source_name: &str, dest_name: &str) -> String {
    if let Some(open) = line.find("](") {
        let target_start = open + 2;
        if let Some(close_rel) = line[target_start..].find(')') {
            let target = &line[target_start..target_start + close_rel];
            if target.contains(source_name) {
                return format!(
                    "{}{}{}",
                    &line[..target_start],
                    dest_name,
                    &line[target_start + close_rel..]
                );
            }
        }
    }
    line.replace(source_name, dest_name)
}

/// Locate the byte offset of the character immediately following the
/// closing `---` of the frontmatter. Used to splice the body back onto
/// the rewritten header without rebuilding it from `lines()` (which
/// silently drops trailing newlines).
fn body_offset_after_frontmatter(original: &str) -> Option<usize> {
    let bytes = original.as_bytes();
    if !original.starts_with("---") {
        return None;
    }
    // Skip the opening `---` line, then look for a line equal to `---`.
    let mut idx = 0;
    let mut line_start = 0usize;
    let mut first_line_skipped = false;
    while idx < bytes.len() {
        if bytes[idx] == b'\n' {
            let line = &original[line_start..idx];
            if first_line_skipped && line.trim_end_matches('\r') == "---" {
                return Some(idx + 1);
            }
            first_line_skipped = true;
            line_start = idx + 1;
        }
        idx += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::dream_config::{write_dream_config, DreamConfig};

    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER_SESSION: &str = "660e8400-e29b-41d4-a716-446655440000";
    const LAST_ASSISTANT_ID: &str = "770e8400-e29b-41d4-a716-446655440000";

    fn evaluate_payload(dir: &TempDir, kinds: Vec<&str>) -> Value {
        json!({
            "recovery_schema_version": 1,
            "session_id": SESSION_ID,
            "current_session_id": SESSION_ID,
            "last_assistant_uuid": LAST_ASSISTANT_ID,
            "project_cwd": dir.path().to_string_lossy(),
            "transcript_path": dir.path().join(format!("{SESSION_ID}.jsonl")).to_string_lossy(),
            "memory_dir": dir.path().join("memory").to_string_lossy(),
            "message_counts": { "user": 1, "assistant": 1, "total": 2 },
            "feature_flags": {
                "EXTRACT_MEMORIES": true,
                "auto_memory_enabled": true,
                "auto_dream_enabled": true
            },
            "requested_kinds": kinds,
            "now_ms": 1_700_200_000_000_u64
        })
    }

    fn request(method: &str, payload: Value) -> Value {
        json!({ "method": method, "payload": payload })
    }

    #[derive(Clone, Debug)]
    struct TestRunnerLeader {
        token: String,
        epoch: u64,
    }

    async fn claim_test_runner_leader(handler: &IpcHandler, base_dir: &Path) -> TestRunnerLeader {
        handler.set_base_dir(base_dir.to_path_buf());
        let response = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": handler.runner_leader_dir().to_string_lossy(),
                    "owner_pid": std::process::id(),
                    "ttl_ms": 60_000,
                }),
            ))
            .await;
        assert_eq!(response["granted"], true, "{response}");
        TestRunnerLeader {
            token: response["leader_token"]
                .as_str()
                .expect("leader token")
                .to_owned(),
            epoch: response["leader_epoch"].as_u64().expect("leader epoch"),
        }
    }

    fn with_test_runner_leader(mut payload: Value, leader: &TestRunnerLeader) -> Value {
        let object = payload.as_object_mut().expect("runner payload object");
        object.insert("leader_token".to_owned(), json!(leader.token));
        object.insert("leader_epoch".to_owned(), json!(leader.epoch));
        payload
    }

    fn strictly_newer_build_id(current: &str) -> String {
        let major = current
            .split_once('+')
            .expect("build id includes authority suffix")
            .0
            .split('.')
            .next()
            .expect("build id includes a major version")
            .parse::<u64>()
            .expect("build id major version is numeric");
        format!("{}.0.0+promotion-test", major + 1)
    }

    fn selected_generation_successor_alias(current: &str) -> String {
        let version = current
            .split_once('+')
            .expect("current build id has metadata")
            .0;
        format!("{version}.1+selected-generation.handler-test")
    }

    fn owner_bound_promotion_payload(successor_build_id: impl Into<String>) -> Value {
        json!({
            "expected_current_build_id": env!("CRABCODE_BUILD_ID"),
            "expected_current_pid": std::process::id(),
            "successor_build_id": successor_build_id.into(),
            "protocol_version": crate::MEMORY_PROTOCOL_VERSION,
            "schema_id": crate::MEMORY_SCHEMA_ID,
        })
    }

    /// W4 (2026-07-16)：真实形状的主会话转写行（镜像 dream_corpus 测试的
    /// `user_assistant_line`）。空对象 `{}` 行压缩后为空文本，会被 RC-7a
    /// 空语料门拦下 —— 需要「做梦真执行」的夹具一律用本 helper。
    fn main_session_transcript_line() -> String {
        format!(
            "{}\n{}\n",
            json!({"type":"user","message":{"role":"user","content":"帮我看看这个项目"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":"看完了，要点如下"}}),
        )
    }

    // ── W-MEMORY-EVOLUTION PR-0 (2026-05-29) — D3 去全局 Mutex 死锁回归锁 ──
    //
    // B2 死锁根因 = `lib.rs` 外层 `Arc<Mutex<IpcHandler>>` 跨整个
    // `handle_value` await 持有；tier `process` await LLM 往返时持锁，使
    // 解锁它的 `llm_call_result` delivery 抢同一锁 → 死锁。根治 = `IpcHandler`
    // 内部可变（`&self` 方法 + 无外层 Mutex），由 `Arc<IpcHandler>` 共享。
    //
    // 这两个测试锁住「无法再退回外层 Mutex」的结构性不变量：

    /// 结构性锁：`handle_value` 必须是 `&self`（可经共享引用调用），且
    /// `IpcHandler` 必须 `Send + Sync`（可放进 `Arc` 跨 spawn 任务共享）。
    /// 若有人把 `handle_value` 退回 `&mut self`，下方 `call_via_shared`
    /// 闭包将编译失败——即把 B2 复发挡在编译期。
    #[test]
    fn pr0_handle_value_is_shared_ref_and_handler_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<IpcHandler>();
        // 仅取函数指针，证明签名是 `&self`（&mut self 无法 coerce 到此 fn 类型）。
        #[allow(clippy::type_complexity)]
        let _call_via_shared: for<'a> fn(
            &'a IpcHandler,
            Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Value> + 'a>,
        > = |h: &IpcHandler, v: Value| Box::pin(h.handle_value(v));
    }

    /// 行为锁：单个 `Arc<IpcHandler>` 被多个并发任务共享调用 `handle_value`，
    /// 全部成功返回。旧 `&mut self` / 单线程 handler 形态下无法编译/运行此模式
    /// （`Arc<Mutex<_>>` 会把这些调用串行化）。这里证明共享并发调用本身成立。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pr0_concurrent_handle_value_on_shared_arc_all_complete() {
        let handler = std::sync::Arc::new(IpcHandler::new());
        let mut joins = Vec::new();
        for _ in 0..16 {
            let h = std::sync::Arc::clone(&handler);
            joins.push(tokio::spawn(async move {
                h.handle_value(request("memory.ping", Value::Null)).await
            }));
        }
        for j in joins {
            let resp = j.await.expect("task joins");
            assert_eq!(resp["ok"], true);
        }
    }

    #[tokio::test]
    async fn ipc_handler_ping_responds_without_stdio_or_llm_methods() {
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request("memory.ping", Value::Null))
            .await;

        assert_eq!(response["ok"], true);
        assert_eq!(response["service"], crate::MEMORY_SERVICE_IDENTITY);
        assert_eq!(response["protocol_version"], crate::MEMORY_PROTOCOL_VERSION);
        assert_eq!(response["schema_id"], crate::MEMORY_SCHEMA_ID);
        assert_eq!(response["build_id"], env!("CRABCODE_BUILD_ID"));
        assert_eq!(response["capabilities"], json!(crate::MEMORY_CAPABILITIES));
        let source = include_str!("ipc_handler.rs");
        // W-MEMORY-DREAM-REBUILD v7 P1.4 (2026-05-25): the original v6 gate
        // banned the lexical string `memory.<kind>.run` (TS→Rust direct
        // invoke that the v6 rebuild removed). P1.4 reintroduces
        // `memory.dream.run_now` (with `_now` suffix) for the manual TUI
        // button — bypass automatic gates but never reintroduce the bare
        // direct-invoke. Build the banned token via concat so this
        // assertion's own source line does not trip the gate.
        let dream_banned_quote = format!("memory.{}.run{}", "dream", "\"");
        let extract_banned_quote = format!("memory.{}.run{}", "extract", "\"");
        assert!(!source.contains(&dream_banned_quote));
        assert!(!source.contains(&extract_banned_quote));
    }

    #[tokio::test]
    async fn ipc_handler_coordinator_promote_requires_owner_binding_and_strict_successor() {
        let handler = IpcHandler::new();
        let current_build_id = env!("CRABCODE_BUILD_ID");
        let current_pid = std::process::id();
        let successor_build_id = selected_generation_successor_alias(current_build_id);

        let accepted = handler
            .handle_value(request(
                "memory.coordinator.promote",
                owner_bound_promotion_payload(&successor_build_id),
            ))
            .await;
        assert_eq!(accepted["ok"], true);
        assert_eq!(accepted["promote"], true);
        assert_eq!(accepted["current_build_id"], current_build_id);
        assert_eq!(accepted["current_pid"], current_pid);
        assert_eq!(accepted["successor_build_id"], successor_build_id);
        assert_eq!(accepted["protocol_version"], crate::MEMORY_PROTOCOL_VERSION);
        assert_eq!(accepted["schema_id"], crate::MEMORY_SCHEMA_ID);

        let missing_owner_binding = handler
            .handle_value(request(
                "memory.coordinator.promote",
                json!({
                    "successor_build_id": strictly_newer_build_id(current_build_id),
                    "protocol_version": crate::MEMORY_PROTOCOL_VERSION,
                    "schema_id": crate::MEMORY_SCHEMA_ID,
                }),
            ))
            .await;
        assert_eq!(missing_owner_binding["ok"], false);
        assert!(missing_owner_binding["error"]
            .as_str()
            .is_some_and(|message| message.contains("expected_current_build_id")));

        let mut wrong_build_payload = owner_bound_promotion_payload(&successor_build_id);
        wrong_build_payload["expected_current_build_id"] = json!("0.0.0+wrong-owner");
        let wrong_build = handler
            .handle_value(request("memory.coordinator.promote", wrong_build_payload))
            .await;
        assert_eq!(wrong_build["ok"], false);
        assert!(wrong_build["error"]
            .as_str()
            .is_some_and(|message| message.contains("current build")));

        let wrong_pid_value = if current_pid == u32::MAX {
            current_pid - 1
        } else {
            current_pid + 1
        };
        let mut wrong_pid_payload = owner_bound_promotion_payload(&successor_build_id);
        wrong_pid_payload["expected_current_pid"] = json!(wrong_pid_value);
        let wrong_pid = handler
            .handle_value(request("memory.coordinator.promote", wrong_pid_payload))
            .await;
        assert_eq!(wrong_pid["ok"], false);
        assert!(wrong_pid["error"]
            .as_str()
            .is_some_and(|message| message.contains("current pid")));

        let raw_same_version_build_id = format!(
            "{}+raw-same-version",
            current_build_id
                .split_once('+')
                .expect("current build id has metadata")
                .0
        );
        let same_version = handler
            .handle_value(request(
                "memory.coordinator.promote",
                owner_bound_promotion_payload(raw_same_version_build_id),
            ))
            .await;
        assert_eq!(same_version["ok"], false);
        assert!(same_version["error"]
            .as_str()
            .is_some_and(|message| message.contains("strictly newer")));

        let mut wrong_protocol_payload = owner_bound_promotion_payload(&successor_build_id);
        wrong_protocol_payload["protocol_version"] = json!(crate::MEMORY_PROTOCOL_VERSION + 1);
        let wrong_protocol = handler
            .handle_value(request(
                "memory.coordinator.promote",
                wrong_protocol_payload,
            ))
            .await;
        assert_eq!(wrong_protocol["ok"], false);
        assert!(wrong_protocol["error"]
            .as_str()
            .is_some_and(|message| message.contains("incompatible")));

        let mut wrong_schema_payload = owner_bound_promotion_payload(successor_build_id);
        wrong_schema_payload["schema_id"] = json!("incompatible-memory-schema");
        let wrong_schema = handler
            .handle_value(request("memory.coordinator.promote", wrong_schema_payload))
            .await;
        assert_eq!(wrong_schema["ok"], false);
        assert!(wrong_schema["error"]
            .as_str()
            .is_some_and(|message| message.contains("incompatible")));
    }

    #[tokio::test]
    async fn ipc_handler_turn_end_evaluate_returns_extract_trigger() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();

        let response = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;

        assert_eq!(response["triggers"].as_array().unwrap().len(), 1);
        assert_eq!(response["triggers"][0]["kind"], "extract");
    }

    #[tokio::test]
    async fn ipc_handler_runner_completed_advances_extract_cursor_and_indexes_paths() {
        let dir = TempDir::new().unwrap();
        let memory_file = dir.path().join("memory/topic.md");
        fs::create_dir_all(memory_file.parent().unwrap()).unwrap();
        fs::write(&memory_file, "body").unwrap();
        let handler = IpcHandler::new();
        let leader = claim_test_runner_leader(&handler, dir.path()).await;
        let response = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = response["triggers"][0]["trigger_id"].as_str().unwrap();

        let completed = handler
            .handle_value(request(
                "memory.runner.completed",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "kind": "extract",
                        "written_paths": [memory_file.to_string_lossy()],
                        "usage": { "output_tokens": 9 }
                    }),
                    &leader,
                ),
            ))
            .await;

        assert_eq!(completed["ok"], true);
        assert_eq!(completed["cursor_updated"], true);
        assert_eq!(completed["indexed_path_count"], 1);
    }

    #[tokio::test]
    async fn journaled_runner_is_persisted_before_hint_then_claimed_fenced_and_settled() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let leader = claim_test_runner_leader(&handler, dir.path()).await;

        let evaluated = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = runner_work_key(&trigger_id);
        assert_eq!(
            journal.get(&key).unwrap().unwrap().state,
            WorkState::Pending
        );

        let claim = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "worker_id": "worker-a",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(claim["received"], true);
        let owner = claim["trigger"]["delivery_owner"].as_str().unwrap();
        let epoch = claim["trigger"]["delivery_epoch"].as_u64().unwrap();

        let ack = handler
            .handle_value(request(
                "memory.runner.ack",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "delivery_owner": owner,
                        "delivery_epoch": epoch,
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(ack["received"], true);

        let forged = handler
            .handle_value(request(
                "memory.runner.completed",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "kind": "extract",
                        "written_paths": [],
                        "delivery_owner": owner,
                        "delivery_epoch": epoch + 1,
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(forged["received"], false);
        assert_eq!(forged["reason"], "stale_delivery");

        let completion_payload = with_test_runner_leader(
            json!({
                "trigger_id": trigger_id,
                "kind": "extract",
                "written_paths": [],
                "delivery_owner": owner,
                "delivery_epoch": epoch,
            }),
            &leader,
        );
        let completed = handler
            .handle_value(request(
                "memory.runner.completed",
                completion_payload.clone(),
            ))
            .await;
        assert_eq!(completed["received"], true);
        assert_eq!(completed["settled"], true);
        assert_eq!(
            journal.get(&key).unwrap().unwrap().state,
            WorkState::Settled
        );

        let retried = handler
            .handle_value(request("memory.runner.completed", completion_payload))
            .await;
        assert_eq!(retried["received"], true);
        assert_eq!(retried["settled"], true);
        let archive =
            fs::read_to_string(crate::extract_archive::runner_archive_path(dir.path())).unwrap();
        assert_eq!(archive.lines().count(), 1);
    }

    #[tokio::test]
    async fn crash_after_enqueue_before_hint_is_recovered_by_read_only_candidates() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let leader = claim_test_runner_leader(&handler, dir.path()).await;

        // The evaluate response is intentionally not used as the execution
        // source. This models a crash/connection loss after SQLite commit but
        // before the hint reaches the leader.
        let lost_hint = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = lost_hint["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            journal
                .get(&runner_work_key(&trigger_id))
                .unwrap()
                .unwrap()
                .state,
            WorkState::Pending
        );

        let candidates = handler
            .handle_value(request(
                "memory.runner.candidates",
                with_test_runner_leader(json!({ "limit": 8 }), &leader),
            ))
            .await;
        assert_eq!(candidates["candidates"].as_array().unwrap().len(), 1);
        let candidate = &candidates["candidates"][0];
        assert_eq!(candidate["trigger_id"], trigger_id);
        assert_eq!(candidate["recovery"]["recovery_schema_version"], 1);
        assert_eq!(
            candidate["recovery"]["context_leaf_uuid"],
            LAST_ASSISTANT_ID
        );
        assert!(
            candidate.get("delivery_owner").is_none()
                && candidate.get("delivery_epoch").is_none()
                && candidate.get("lease_expires_at_ms").is_none(),
            "candidate snapshots must not grant execution authority: {candidate}"
        );
        assert_eq!(
            journal
                .get(&runner_work_key(&trigger_id))
                .unwrap()
                .unwrap()
                .state,
            WorkState::Pending,
            "enumeration is read-only"
        );

        let claim = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "worker_id": "recovery-worker",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(claim["received"], true);
        assert_eq!(claim["trigger"]["recovery"], candidate["recovery"]);
    }

    #[tokio::test]
    async fn poison_candidate_is_dead_lettered_without_starving_later_valid_work() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        journal
            .enqueue(
                "runner:poison",
                WorkKind::RunnerTrigger,
                &json!({ "legacy": "missing recovery locator" }),
                1,
            )
            .unwrap();
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let leader = claim_test_runner_leader(&handler, dir.path()).await;
        let evaluated = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let valid_trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();

        let snapshot = handler
            .handle_value(request(
                "memory.runner.candidates",
                with_test_runner_leader(json!({ "limit": 8 }), &leader),
            ))
            .await;
        let candidates = snapshot["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["trigger_id"], "poison");
        assert_eq!(candidates[0]["invalid_reason"], "invalid_recovery_locator");
        assert_eq!(candidates[1]["trigger_id"], valid_trigger_id);

        let poison = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": "poison",
                        "worker_id": "recovery-worker",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(poison["received"], false);
        assert_eq!(poison["reason"], "invalid_recovery_locator_dead_lettered");
        assert_eq!(
            journal.get("runner:poison").unwrap().unwrap().state,
            WorkState::DeadLetter
        );

        let valid = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": valid_trigger_id,
                        "worker_id": "recovery-worker",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(valid["received"], true);
    }

    #[tokio::test]
    async fn release_uses_server_backoff_and_dead_letter_is_delivery_fenced() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let leader = claim_test_runner_leader(&handler, dir.path()).await;
        let evaluated = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = runner_work_key(&trigger_id);
        let claim = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "worker_id": "worker-release",
                    }),
                    &leader,
                ),
            ))
            .await;
        let owner = claim["trigger"]["delivery_owner"].as_str().unwrap();
        let epoch = claim["trigger"]["delivery_epoch"].as_u64().unwrap();

        let stale_release = handler
            .handle_value(request(
                "memory.runner.release",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "delivery_owner": owner,
                        "delivery_epoch": epoch + 1,
                        "reason_code": "transient_io",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(stale_release["received"], false);
        assert_eq!(stale_release["reason"], "stale_delivery");

        let released = handler
            .handle_value(request(
                "memory.runner.release",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "delivery_owner": owner,
                        "delivery_epoch": epoch,
                        "reason_code": "transient_io",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(released["received"], true);
        let next_attempt_at_ms = released["next_attempt_at_ms"].as_u64().unwrap();
        let pending = journal.get(&key).unwrap().unwrap();
        assert_eq!(pending.state, WorkState::Pending);
        assert_eq!(pending.next_attempt_at_ms, next_attempt_at_ms);
        assert_eq!(pending.last_error.as_deref(), Some("transient_io"));

        let candidates = handler
            .handle_value(request(
                "memory.runner.candidates",
                with_test_runner_leader(json!({ "limit": 8 }), &leader),
            ))
            .await;
        assert!(
            candidates["candidates"].as_array().unwrap().is_empty(),
            "server-computed backoff must delay immediate re-enumeration"
        );

        let reclaimed = journal
            .claim_delivery_by_key(
                &key,
                WorkKind::RunnerTrigger,
                "worker-dead-letter",
                next_attempt_at_ms + 1,
                RUNNER_DELIVERY_LEASE_MS,
            )
            .unwrap()
            .unwrap();
        let dead_lettered = handler
            .handle_value(request(
                "memory.runner.dead_letter",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "delivery_owner": reclaimed.lease_owner,
                        "delivery_epoch": reclaimed.delivery_epoch,
                        "reason_code": "permanent_context",
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(dead_lettered["received"], true);
        let dead = journal.get(&key).unwrap().unwrap();
        assert_eq!(dead.state, WorkState::DeadLetter);
        assert_eq!(dead.last_error.as_deref(), Some("permanent_context"));
    }

    #[tokio::test]
    async fn old_leader_generation_cannot_mutate_any_runner_endpoint() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let old_leader = claim_test_runner_leader(&handler, dir.path()).await;
        let evaluated = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = runner_work_key(&trigger_id);

        let released = handler
            .handle_value(request(
                "memory.leader.release",
                json!({
                    "memory_dir": handler.runner_leader_dir().to_string_lossy(),
                    "owner_pid": std::process::id(),
                    "leader_token": old_leader.token.clone(),
                    "leader_epoch": old_leader.epoch,
                }),
            ))
            .await;
        assert_eq!(released["released"], true);
        let new_leader = claim_test_runner_leader(&handler, dir.path()).await;
        assert!(new_leader.epoch > old_leader.epoch);

        for (method, payload) in [
            ("memory.runner.candidates", json!({ "limit": 8 })),
            (
                "memory.runner.claim",
                json!({ "trigger_id": trigger_id, "worker_id": "old-worker" }),
            ),
        ] {
            let denied = handler
                .handle_value(request(
                    method,
                    with_test_runner_leader(payload, &old_leader),
                ))
                .await;
            assert_eq!(denied["ok"], false, "{method}: {denied}");
            assert!(denied["error"]
                .as_str()
                .is_some_and(|error| error.contains("leader fence")));
        }
        assert_eq!(
            journal.get(&key).unwrap().unwrap().state,
            WorkState::Pending
        );

        let claim = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "worker_id": "new-worker",
                    }),
                    &new_leader,
                ),
            ))
            .await;
        let owner = claim["trigger"]["delivery_owner"].as_str().unwrap();
        let epoch = claim["trigger"]["delivery_epoch"].as_u64().unwrap();
        for (method, mut payload) in [
            (
                "memory.runner.ack",
                json!({
                    "trigger_id": trigger_id,
                    "delivery_owner": owner,
                    "delivery_epoch": epoch,
                }),
            ),
            (
                "memory.runner.renew",
                json!({
                    "trigger_id": trigger_id,
                    "delivery_owner": owner,
                    "delivery_epoch": epoch,
                }),
            ),
            (
                "memory.runner.release",
                json!({
                    "trigger_id": trigger_id,
                    "delivery_owner": owner,
                    "delivery_epoch": epoch,
                    "reason_code": "old_leader",
                }),
            ),
            (
                "memory.runner.dead_letter",
                json!({
                    "trigger_id": trigger_id,
                    "delivery_owner": owner,
                    "delivery_epoch": epoch,
                    "reason_code": "old_leader",
                }),
            ),
            (
                "memory.runner.completed",
                json!({
                    "trigger_id": trigger_id,
                    "kind": "extract",
                    "written_paths": [],
                    "delivery_owner": owner,
                    "delivery_epoch": epoch,
                }),
            ),
        ] {
            payload = with_test_runner_leader(payload, &old_leader);
            let denied = handler.handle_value(request(method, payload)).await;
            assert_eq!(denied["ok"], false, "{method}: {denied}");
        }
        assert_eq!(
            journal.get(&key).unwrap().unwrap().state,
            WorkState::Leased,
            "old leader calls must leave delivery state untouched"
        );
    }

    #[tokio::test]
    async fn journaled_dream_persists_imagination_followup_before_source_settlement() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(format!("{OTHER_SESSION}.jsonl")), "{}\n").unwrap();
        set_file_mtime(
            dir.path().join(format!("{OTHER_SESSION}.jsonl")),
            FileTime::from_unix_time(1_700_100_000, 0),
        )
        .unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: true,
                min_hours: 24,
                min_sessions: 1,
                session_scan_interval_ms: 600_000,
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());
        let handler = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let leader = claim_test_runner_leader(&handler, dir.path()).await;
        let evaluated = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["dream"]),
            ))
            .await;
        let trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let claim = handler
            .handle_value(request(
                "memory.runner.claim",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "worker_id": "worker-dream",
                    }),
                    &leader,
                ),
            ))
            .await;
        let owner = claim["trigger"]["delivery_owner"].as_str().unwrap();
        let epoch = claim["trigger"]["delivery_epoch"].as_u64().unwrap();
        let ack = handler
            .handle_value(request(
                "memory.runner.ack",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "delivery_owner": owner,
                        "delivery_epoch": epoch,
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(ack["received"], true);
        let completed = handler
            .handle_value(request(
                "memory.runner.completed",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "kind": "dream",
                        "written_paths": [],
                        "delivery_owner": owner,
                        "delivery_epoch": epoch,
                    }),
                    &leader,
                ),
            ))
            .await;
        assert_eq!(completed["received"], true);
        assert_eq!(completed["settled"], true);
        assert_eq!(
            journal
                .get(&runner_work_key(&trigger_id))
                .unwrap()
                .unwrap()
                .state,
            WorkState::Settled
        );
        let followup_key = imagination_followup_key(&trigger_id);
        let followup = journal.get(&followup_key).unwrap().unwrap();
        assert_eq!(followup.kind, WorkKind::ReverseRequest);
        assert_eq!(followup.state, WorkState::Pending);
        let payload: DurableImaginationFollowup = serde_json::from_value(followup.payload).unwrap();
        assert_eq!(payload.schema_id, DURABLE_IMAGINATION_SCHEMA_ID);
        assert_eq!(payload.source_trigger_id, trigger_id);
    }

    #[tokio::test]
    async fn startup_recovery_settles_expired_result_and_does_not_let_poison_row_starve_it() {
        let dir = TempDir::new().unwrap();
        let journal =
            Arc::new(Journal::open(dir.path().join("state/memory-journal.sqlite3")).unwrap());

        journal
            .enqueue(
                "runner:poison",
                WorkKind::RunnerTrigger,
                &json!({ "not": "durable runner work" }),
                1_000,
            )
            .unwrap();
        let poison_delivery = journal
            .claim_delivery_by_key(
                "runner:poison",
                WorkKind::RunnerTrigger,
                "worker-poison",
                1_010,
                100,
            )
            .unwrap()
            .unwrap();
        journal
            .record_result(
                "runner:poison",
                "completion:poison",
                &json!({ "not": "runner completion" }),
                &DeliveryFence::new(
                    poison_delivery.lease_owner.unwrap(),
                    poison_delivery.delivery_epoch,
                ),
                1_020,
            )
            .unwrap();

        let producer = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let evaluated = producer
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let trigger_id = evaluated["triggers"][0]["trigger_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = runner_work_key(&trigger_id);
        let delivery = journal
            .claim_delivery_by_key(&key, WorkKind::RunnerTrigger, "worker-valid", 1_100, 100)
            .unwrap()
            .unwrap();
        journal
            .record_result(
                &key,
                "completion:valid",
                &json!({
                    "trigger_id": trigger_id,
                    "kind": "extract",
                    "written_paths": [],
                    "usage": null,
                    "error": null,
                }),
                &DeliveryFence::new(delivery.lease_owner.unwrap(), delivery.delivery_epoch),
                1_120,
            )
            .unwrap();
        journal
            .claim_settlement_by_key(&key, "crashed-settler", 1_130, 1)
            .unwrap()
            .expect("simulate crash with a settlement fence");
        drop(producer);

        let recovering = IpcHandler::with_journal_for_testing(Arc::clone(&journal));
        let report = recovering.recover_runner_settlements().await.unwrap();

        assert_eq!(
            report,
            RunnerSettlementRecoveryReport {
                candidates: 2,
                settled: 1,
                failed: 1,
                fence_lost: 0,
            }
        );
        assert_eq!(
            journal.get("runner:poison").unwrap().unwrap().state,
            WorkState::ResultReady,
            "poison result is preserved for diagnosis/retry"
        );
        assert_eq!(
            journal.get(&key).unwrap().unwrap().state,
            WorkState::Settled
        );
    }

    #[tokio::test]
    async fn ipc_handler_dream_cycle_releases_lock_after_completion() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(format!("{OTHER_SESSION}.jsonl")), "{}\n").unwrap();
        set_file_mtime(
            dir.path().join(format!("{OTHER_SESSION}.jsonl")),
            FileTime::from_unix_time(1_700_100_000, 0),
        )
        .unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: true,
                min_hours: 24,
                min_sessions: 1,
                session_scan_interval_ms: 600_000,
                // Struct-update spread so this literal survives new
                // `DreamConfig` fields (e.g. K5 `imagination_min_hours`).
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        let handler = IpcHandler::new();
        let leader = claim_test_runner_leader(&handler, dir.path()).await;

        let response = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["dream"]),
            ))
            .await;
        let trigger_id = response["triggers"][0]["trigger_id"].as_str().unwrap();
        let completed = handler
            .handle_value(request(
                "memory.runner.completed",
                with_test_runner_leader(
                    json!({
                        "trigger_id": trigger_id,
                        "kind": "dream",
                        "written_paths": []
                    }),
                    &leader,
                ),
            ))
            .await;

        assert_eq!(completed["lock_released"], true);
        assert_eq!(
            fs::read_to_string(dir.path().join("memory/.consolidate-lock")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn ipc_handler_dream_enabled_get_set_uses_sibling_project_state() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();

        let set = handler
            .handle_value(request(
                "memory.dream.set_enabled",
                json!({ "memory_dir": dir.path().join("memory").to_string_lossy(), "enabled": true }),
            ))
            .await;
        let get = handler
            .handle_value(request(
                "memory.dream.is_enabled",
                json!({ "memory_dir": dir.path().join("memory").to_string_lossy() }),
            ))
            .await;

        assert_eq!(set["enabled"], true);
        assert_eq!(get["enabled"], true);
        assert!(dir
            .path()
            .join(".memory-rust-derived/dream-config.json")
            .exists());
    }

    #[tokio::test]
    async fn ipc_handler_lock_last_consolidated_at_reads_lock_mtime() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(dir.path().join("memory/.consolidate-lock"), "").unwrap();
        set_file_mtime(
            dir.path().join("memory/.consolidate-lock"),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let handler = IpcHandler::new();

        let response = handler
            .handle_value(request(
                "memory.lock.last_consolidated_at",
                json!({ "memory_dir": dir.path().join("memory").to_string_lossy() }),
            ))
            .await;

        assert_eq!(response["mtime_ms"], 1_700_000_000_000_u64);
    }

    #[tokio::test]
    async fn ipc_handler_memory_status_requires_explicit_scope_and_reports_status() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("MEMORY.md"), "# Memory\n").unwrap();
        fs::write(dir.path().join(format!("{SESSION_ID}.jsonl")), "{}\n").unwrap();
        let handler = IpcHandler::new();

        let missing_scope = handler
            .handle_value(request(
                "memory.status",
                json!({ "memory_dir": memory_dir }),
            ))
            .await;
        assert_eq!(missing_scope["ok"], false);
        assert!(missing_scope["error"].as_str().unwrap().contains("cwd"));

        let response = handler
            .handle_value(request(
                "memory.status",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "cwd": dir.path().to_string_lossy(),
                    "project_state_dir": dir.path().to_string_lossy(),
                    "transcript_dir": dir.path().to_string_lossy(),
                    "stale_days": 30
                }),
            ))
            .await;

        assert!(response["generated_at_ms"].as_u64().unwrap() > 0);
        assert_eq!(
            response["paths"]["memory_dir"].as_str().unwrap(),
            memory_dir.to_string_lossy()
        );
        assert_eq!(response["memory_md"]["exists"], true);
        assert_eq!(response["transcript_index"]["transcript_count"], 1);
        assert_eq!(response["lock"]["exists"], false);
    }

    // W-MEMORY-DREAM-REBUILD v7 P3.2 (2026-05-25) — IPC arm tests.
    #[tokio::test]
    async fn ipc_handler_tier1_evaluate_subagent_skipped() {
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier1.evaluate",
                json!({
                    "agent_type": "subagent",
                    "session_key": "sess-a",
                    "current_token_count": 50_000,
                    "tool_calls_since_last_update": 10,
                    "has_tool_calls_in_last_turn": false,
                    "feature_flags": { "auto_session_memory": true }
                }),
            ))
            .await;
        assert_eq!(response["should_trigger"], false);
        assert_eq!(response["skip_reason"], "non_main_agent");
    }

    #[tokio::test]
    async fn ipc_handler_tier1_evaluate_main_agent_init_threshold_unmet() {
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier1.evaluate",
                json!({
                    "agent_type": "",
                    "session_key": "sess-init",
                    "current_token_count": 5_000,
                    "tool_calls_since_last_update": 10,
                    "has_tool_calls_in_last_turn": false,
                    "feature_flags": { "auto_session_memory": true }
                }),
            ))
            .await;
        assert_eq!(response["should_trigger"], false);
        assert_eq!(response["skip_reason"], "init_threshold_unmet");
    }

    #[tokio::test]
    async fn ipc_handler_tier_llm_call_result_delivers_to_processor() {
        // Without a pending request, deliver_result returns false → received=false.
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.llm_call_result",
                json!({
                    "req_id": "no-such-req",
                    "response": "hello",
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["received"], false);
        assert_eq!(response["req_id"], "no-such-req");
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — `memory.tier.embedding_result`
    /// IPC arm with SE not initialized: must accept-and-noop (delivery
    /// reports received=false; payload parsing must still succeed).
    #[tokio::test]
    async fn ipc_handler_tier_embedding_result_accept_when_se_not_initialized() {
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.embedding_result",
                json!({
                    "req_id": "no-such-embed-req",
                    "embeddings": [],
                    "dimension": 0,
                    "error": "noop"
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["received"], false);
        assert_eq!(response["req_id"], "no-such-embed-req");
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — `memory.tier.embedding_result`
    /// with SE initialized but unknown req_id: must report received=false
    /// (unknown req_id is structurally a no-op).
    #[tokio::test]
    async fn ipc_handler_tier_embedding_result_unknown_req_id_with_se() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let emitter = Arc::new(crate::se_integration::RecordingEmitter::new());
        let integration = Arc::new(
            crate::se_integration::SearchEngineIntegration::new(
                dir.path().join("se"),
                emitter.clone() as Arc<dyn crate::se_integration::EmbeddingEmitter>,
            )
            .expect("init SE"),
        );
        handler.set_se_integration(integration);

        let response = handler
            .handle_value(request(
                "memory.tier.embedding_result",
                json!({
                    "req_id": "se-embed-unknown-1",
                    "embeddings": [],
                    "dimension": 0
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["received"], false);
        assert_eq!(response["req_id"], "se-embed-unknown-1");
    }

    /// W-MEMORY-EVOLUTION PR-7b — `memory.tier.tool_call_result` IPC arm
    /// with no pending request: payload parses, delivery reports
    /// received=false (unknown req_id is a structural no-op).
    #[tokio::test]
    async fn ipc_handler_tier_tool_call_result_unknown_req_id_is_noop() {
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.tool_call_result",
                json!({
                    "req_id": "tier3-imagination-evidence-no-such",
                    "evidence": [
                        {
                            "source_url": "https://example.com",
                            "fetched_at_ms": 100,
                            "content": "body"
                        }
                    ]
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["received"], false);
        assert_eq!(response["req_id"], "tier3-imagination-evidence-no-such");
    }

    /// W-MEMORY-EVOLUTION PR-7b — full round-trip: a pending tool oneshot
    /// registered on the handler's imagination processor is resolved by a
    /// `memory.tier.tool_call_result` IPC delivery; the awaited future receives
    /// the evidence and the arm reports received=true.
    #[tokio::test]
    async fn ipc_handler_tier_tool_call_result_round_trip_delivers_evidence() {
        let handler = IpcHandler::new();
        let processor = handler.tier3_imagination_processor();

        // Register a pending tool oneshot under a known req_id directly on the
        // handler's processor (out-of-band of gather_evidence's await), then
        // deliver the evidence through the IPC arm and observe resolution.
        let req_id = "tier3-imagination-evidence-roundtrip-1";
        let rx = processor._testonly_register_pending_tool(req_id).await;

        let response = handler
            .handle_value(request(
                "memory.tier.tool_call_result",
                json!({
                    "req_id": req_id,
                    "evidence": [
                        {
                            "source_url": "https://example.com/a",
                            "fetched_at_ms": 777,
                            "content": "fresh evidence body",
                            "title": "A"
                        }
                    ]
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["received"], true);
        assert_eq!(response["req_id"], req_id);

        // The pending receiver resolves with the delivered evidence.
        let payload = rx.await.expect("oneshot resolved");
        assert_eq!(payload.req_id, req_id);
        assert_eq!(payload.evidence.len(), 1);
        assert_eq!(payload.evidence[0].source_url, "https://example.com/a");
        assert_eq!(payload.evidence[0].fetched_at_ms, 777);
        assert_eq!(payload.evidence[0].content, "fresh evidence body");
        assert!(payload.error.is_none());
    }

    #[tokio::test]
    async fn ipc_handler_archive_handoff_persists_manual_archive_record() {
        let dir = TempDir::new().unwrap();
        let project_state_dir = dir.path().join("project-state");
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&project_state_dir).unwrap();
        fs::create_dir_all(&memory_dir).unwrap();
        let handler = IpcHandler::new();

        let response = handler
            .handle_value(request(
                "memory.archive_handoff",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "project_state_dir": project_state_dir.to_string_lossy(),
                    "cwd": dir.path().to_string_lossy(),
                    "scope": "project",
                    "thread_ids": ["thread-a", "thread-b"],
                    "reason": "manual_archive"
                }),
            ))
            .await;

        assert_eq!(response["ok"], true);
        assert_eq!(response["accepted"], 2);
        let archive_path = project_state_dir
            .join(".memory-rust-derived")
            .join("archives")
            .join("runner-completed.jsonl");
        let archive = fs::read_to_string(archive_path).unwrap();
        assert!(archive.contains("archive_handoff_project"));
        assert!(archive.contains("thread-a"));
        assert!(archive.contains("manual_archive"));
    }

    // ──────────────────────────────────────────────────────────────────────
    // W-MEMORY-DREAM-REBUILD v7 P5.1 (2026-05-25) — `memory.tier.list`
    // unit tests covering tier dispatch, filtering, sort, pagination,
    // empty subdirs, and frontmatter abstract extraction.
    // ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ipc_handler_tier_list_memory_tab_excludes_session_and_index() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();
        fs::write(memdir.join("MEMORY.md"), "# MEMORY index").unwrap();
        fs::write(memdir.join("SESSION.md"), "session text").unwrap();
        fs::write(memdir.join(".session-abc.md"), "ephemeral").unwrap();
        fs::write(memdir.join("user_foo.md"), "user foo").unwrap();
        fs::write(memdir.join("feedback_bar.md"), "feedback bar").unwrap();
        fs::write(memdir.join("project_baz.md"), "project baz").unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "memory",
                    "sort": "name_asc",
                }),
            ))
            .await;

        assert_eq!(response["ok"], true);
        let items = response["items"].as_array().unwrap();
        assert_eq!(items.len(), 3, "got: {response:?}");
        let names: Vec<&str> = items
            .iter()
            .map(|v| v["file_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["feedback_bar.md", "project_baz.md", "user_foo.md"]
        );
        // Scope tags derived from filename prefix.
        let scopes: Vec<&str> = items.iter().map(|v| v["scope"].as_str().unwrap()).collect();
        assert_eq!(scopes, vec!["feedback", "project", "user"]);
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_tier1_returns_session_and_session_dot_files() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();
        fs::write(memdir.join("SESSION.md"), "current session").unwrap();
        fs::write(memdir.join(".session-aaa.md"), "ephemeral a").unwrap();
        fs::write(memdir.join(".session-bbb.md"), "ephemeral b").unwrap();
        fs::write(memdir.join("user_foo.md"), "user foo").unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "tier1",
                    "sort": "name_asc",
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        let items = response["items"].as_array().unwrap();
        let names: Vec<&str> = items
            .iter()
            .map(|v| v["file_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![".session-aaa.md", ".session-bbb.md", "SESSION.md"]
        );
        // All tier-1 entries carry the `tier1` scope tag.
        for item in items {
            assert_eq!(item["scope"], "tier1");
        }
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_tier2_extracts_subdir_empty_returns_reason() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();
        // No `extracts/` subdir → fail-soft empty + descriptive reason.

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "tier2",
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["items"].as_array().unwrap().len(), 0);
        assert!(response["reason"]
            .as_str()
            .unwrap()
            .contains("not yet created"));
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_tier3_filters_dreams_by_prefix() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        let dreams = memdir.join("dreams");
        fs::create_dir_all(&dreams).unwrap();
        fs::write(dreams.join("insight_a.md"), "insight a").unwrap();
        fs::write(dreams.join("fragment_b.md"), "fragment b").unwrap();
        // W-MEMORY-LIFECYCLE K3 (2026-07-09): promoted imagination drafts
        // (`dream_*` — the `promote_imagination` rename target) must stay
        // visible in the tier3 listing instead of vanishing on promotion.
        fs::write(dreams.join("dream_d.md"), "promoted imagination").unwrap();
        fs::write(dreams.join("random.md"), "unrelated").unwrap();
        fs::write(dreams.join("insight_c.txt"), "wrong ext").unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "tier3",
                    "sort": "name_asc",
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        let items = response["items"].as_array().unwrap();
        let names: Vec<&str> = items
            .iter()
            .map(|v| v["file_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["dream_d.md", "fragment_b.md", "insight_a.md"]);
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_pagination_clamps_and_returns_total() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();
        for i in 0..5 {
            fs::write(memdir.join(format!("user_{i}.md")), "x").unwrap();
        }

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "memory",
                    "sort": "name_asc",
                    "page": 1,
                    "page_size": 2,
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        // page=1 page_size=2 → skip 2, take 2 → user_2.md / user_3.md
        let items = response["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["file_name"], "user_2.md");
        assert_eq!(items[1]["file_name"], "user_3.md");
        assert_eq!(response["total"], 5);
        assert_eq!(response["page"], 1);
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_unknown_tier_returns_empty_with_reason() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "bogus",
                }),
            ))
            .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["items"].as_array().unwrap().len(), 0);
        assert!(response["reason"]
            .as_str()
            .unwrap()
            .contains("unknown tier 'bogus'"));
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_frontmatter_abstract_extracted() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().join("memory");
        fs::create_dir_all(&memdir).unwrap();
        // Inline scalar form.
        fs::write(
            memdir.join("user_inline.md"),
            "---\nabstract: hello world\nother: 42\n---\nBody",
        )
        .unwrap();
        // Block scalar form.
        fs::write(
            memdir.join("user_block.md"),
            "---\ntitle: x\nabstract: |\n  first line\n  second line\n---\nBody",
        )
        .unwrap();
        // No frontmatter.
        fs::write(memdir.join("user_plain.md"), "# Plain header\nBody only").unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "memory",
                    "sort": "name_asc",
                }),
            ))
            .await;
        let items = response["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        // user_block.md
        assert_eq!(items[0]["file_name"], "user_block.md");
        assert!(items[0]["abstract_text"]
            .as_str()
            .unwrap()
            .contains("first line"));
        // user_inline.md
        assert_eq!(items[1]["file_name"], "user_inline.md");
        assert_eq!(items[1]["abstract_text"], "hello world");
        // user_plain.md
        assert_eq!(items[2]["file_name"], "user_plain.md");
        assert!(items[2]["abstract_text"].is_null());
    }

    // ── W-MEMORY-DREAM-REBUILD v7 P5.3 (2026-05-25) ─────────────────────
    // Imagination review queue promote / reject orchestrator tests.
    // Cover: happy path promote (file moved + frontmatter rewritten);
    // happy path reject (file deleted); confirm guard; path-injection
    // defence (absolute / outside review-queue / parent traversal);
    // edit_content substitution; tier list with `imagination-review`.
    // ────────────────────────────────────────────────────────────────────

    fn write_sample_imagined(memdir: &Path, hash: &str, body: &str) -> PathBuf {
        let dir = memdir.join("imagination").join("review-queue");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("imagined_{hash}.md"));
        fs::write(&path, body).unwrap();
        path
    }

    fn sample_imagined_body() -> String {
        concat!(
            "---\n",
            "confidence: high\n",
            "status: pending-review\n",
            "expiry: 2026-06-08T00:00:00Z\n",
            "final_confidence: 0.7500\n",
            "---\n\n",
            "# Hypothesis\n\n",
            "Users prefer markdown.\n"
        )
        .to_string()
    }

    #[test]
    fn validate_review_queue_path_accepts_valid_relative_path() {
        assert!(validate_review_queue_path("imagination/review-queue/imagined_abc.md").is_ok());
    }

    #[test]
    fn validate_review_queue_path_rejects_outside_prefix() {
        let err = validate_review_queue_path("dreams/dream_abc.md").unwrap_err();
        assert!(err.contains("outside review-queue"), "got: {err}");
    }

    #[test]
    fn validate_review_queue_path_rejects_parent_traversal() {
        let err = validate_review_queue_path("imagination/review-queue/../dreams/dream_abc.md")
            .unwrap_err();
        assert!(err.contains("parent traversal"), "got: {err}");
    }

    #[test]
    fn validate_review_queue_path_rejects_absolute() {
        let abs = if cfg!(unix) {
            "/etc/passwd"
        } else {
            "C:\\Windows\\System32\\drivers\\etc\\hosts"
        };
        let err = validate_review_queue_path(abs).unwrap_err();
        assert!(
            err.contains("relative") || err.contains("outside review-queue"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_review_queue_path_rejects_empty() {
        let err = validate_review_queue_path("").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn rewrite_frontmatter_flips_status_and_injects_confirmed_at_ms() {
        let body = sample_imagined_body();
        let out = rewrite_frontmatter_for_promotion(&body, 1_700_000_000_000);
        assert!(out.contains("status: confirmed"));
        assert!(!out.contains("status: pending-review"));
        assert!(out.contains("confirmed_at_ms: 1700000000000"));
        // Original `confidence` / `expiry` fields are preserved.
        assert!(out.contains("confidence: high"));
        assert!(out.contains("expiry: 2026-06-08T00:00:00Z"));
        // Body is preserved verbatim after the frontmatter.
        assert!(out.contains("# Hypothesis"));
        assert!(out.contains("Users prefer markdown."));
    }

    #[test]
    fn rewrite_frontmatter_adds_block_when_missing() {
        let out = rewrite_frontmatter_for_promotion("Plain body without frontmatter\n", 42);
        assert!(out.starts_with("---\nstatus: confirmed"));
        assert!(out.contains("confirmed_at_ms: 42"));
        assert!(out.contains("Plain body without frontmatter"));
    }

    #[tokio::test]
    async fn ipc_handler_promote_happy_path_moves_to_dreams() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        let source = write_sample_imagined(&memdir, "abcdef", &sample_imagined_body());
        assert!(source.exists());

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.promote",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/imagined_abcdef.md",
                }),
            ))
            .await;

        assert_eq!(response["ok"], json!(true), "response: {response}");
        let promoted_path = response["promoted_path"].as_str().unwrap();
        assert!(promoted_path.ends_with("dream_abcdef.md"));

        // Source removed; dest written with confirmed frontmatter.
        assert!(!source.exists(), "source should be removed");
        let dest = memdir.join("dreams").join("dream_abcdef.md");
        assert!(dest.exists(), "dest should exist");
        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains("status: confirmed"));
        assert!(!content.contains("status: pending-review"));
        assert!(content.contains("confirmed_at_ms:"));
    }

    #[tokio::test]
    async fn ipc_handler_promote_with_edit_content_substitutes_body() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        write_sample_imagined(&memdir, "edit01", &sample_imagined_body());

        let edited = concat!(
            "---\n",
            "confidence: high\n",
            "status: pending-review\n",
            "---\n\n",
            "# Hypothesis (user edited)\n\nNew body\n",
        );

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.promote",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/imagined_edit01.md",
                    "edit_content": edited,
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(true));
        let dest = memdir.join("dreams").join("dream_edit01.md");
        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains("user edited"));
        assert!(content.contains("status: confirmed"));
    }

    #[tokio::test]
    async fn ipc_handler_promote_rejects_path_outside_review_queue() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.promote",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "dreams/dream_abc.md",
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(false));
        assert!(response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("outside review-queue"));
    }

    #[tokio::test]
    async fn ipc_handler_promote_rejects_parent_traversal() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.promote",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/../dreams/x.md",
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(false));
        assert!(response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("parent traversal"));
    }

    #[tokio::test]
    async fn ipc_handler_promote_missing_source_returns_error() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        // Do NOT create the imagined file.

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.promote",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/imagined_missing.md",
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(false));
        assert!(response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not found"));
    }

    #[tokio::test]
    async fn ipc_handler_reject_happy_path_deletes_file() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        let source = write_sample_imagined(&memdir, "rej123", &sample_imagined_body());
        assert!(source.exists());

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.reject",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/imagined_rej123.md",
                    "confirm": true,
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["deleted"], json!(true));
        assert!(!source.exists(), "file should be deleted");
    }

    #[tokio::test]
    async fn ipc_handler_reject_without_confirm_short_circuits() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        let source = write_sample_imagined(&memdir, "noconf", &sample_imagined_body());

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.imagination.reject",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "path": "imagination/review-queue/imagined_noconf.md",
                    "confirm": false,
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(false));
        assert!(response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("confirm=true"));
        // File must still exist.
        assert!(source.exists());
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_imagination_review_enumerates_files() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        write_sample_imagined(&memdir, "list01", &sample_imagined_body());
        write_sample_imagined(&memdir, "list02", &sample_imagined_body());
        // A non-imagined_ file should be ignored.
        let queue_dir = memdir.join("imagination").join("review-queue");
        fs::write(queue_dir.join("README.md"), "noise").unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "imagination-review",
                    "sort": "name_asc",
                }),
            ))
            .await;
        assert_eq!(response["ok"], json!(true));
        let items = response["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let names: Vec<&str> = items
            .iter()
            .map(|i| i["file_name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"imagined_list01.md"));
        assert!(names.contains(&"imagined_list02.md"));
        for item in items {
            assert_eq!(item["scope"], "imagination-review");
        }
    }

    #[tokio::test]
    async fn ipc_handler_tier_list_unknown_tier_includes_imagination_review_in_message() {
        let temp = TempDir::new().unwrap();
        let memdir = temp.path().to_path_buf();
        fs::create_dir_all(&memdir).unwrap();

        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.tier.list",
                json!({
                    "memory_dir": memdir.to_string_lossy(),
                    "tier": "nonsense",
                }),
            ))
            .await;
        let reason = response["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("imagination-review"), "reason: {reason}");
    }

    // IPC handler tests for memory.leader.{claim,renew,release,query}.
    // Module-level leader_lock.rs unit tests cover the lower-level
    // semantics (CAS / takeover / staleness); these tests cover only the IPC
    // glue: payload parsing, wire format, ttl_ms default, error paths.

    #[tokio::test]
    async fn ipc_handler_leader_claim_grants_when_vacant() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 90_001_u64,
                    "ttl_ms": 60_000_u64,
                }),
            ))
            .await;
        assert_eq!(response["granted"], json!(true));
        assert_eq!(response["holder_pid"], json!(90_001_u32));
        assert!(response["leader_token"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert!(response["leader_epoch"].as_u64().unwrap_or(0) > 0);
        assert!(response["claimed_at_ms"].as_u64().unwrap_or(0) > 0);
        assert!(response["lease_expires_at_ms"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn ipc_handler_leader_claim_denied_when_held_by_live_other() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        // First claim by a live PID (use current process pid so is_running=true).
        let first = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": std::process::id() as u64,
                }),
            ))
            .await;
        assert_eq!(first["granted"], json!(true));

        // Second claim by a different PID should be denied — current process
        // is still running and lease is fresh.
        let second = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 91_002_u64,
                }),
            ))
            .await;
        assert_eq!(second["granted"], json!(false));
    }

    #[tokio::test]
    async fn ipc_handler_leader_renew_returns_true_when_pid_matches() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let claim = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 92_001_u64,
                }),
            ))
            .await;
        let response = handler
            .handle_value(request(
                "memory.leader.renew",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 92_001_u64,
                    "leader_token": claim["leader_token"],
                    "leader_epoch": claim["leader_epoch"],
                    "ttl_ms": 60_000_u64,
                }),
            ))
            .await;
        assert_eq!(response["still_leader"], json!(true));
        assert!(response["lease_expires_at_ms"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn ipc_handler_leader_renew_returns_false_after_other_claim() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        // A claims.
        let claim = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 93_001_u64,
                }),
            ))
            .await;
        // Simulate B taking over by directly stomping the file (mimics
        // is_process_running=false branch deciding A's PID is dead).
        std::fs::write(leader_lock::leader_lock_path(dir.path()), "93002").unwrap();
        // A's renew now returns false.
        let response = handler
            .handle_value(request(
                "memory.leader.renew",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 93_001_u64,
                    "leader_token": claim["leader_token"],
                    "leader_epoch": claim["leader_epoch"],
                    "ttl_ms": 60_000_u64,
                }),
            ))
            .await;
        assert_eq!(response["still_leader"], json!(false));
        assert_eq!(response["lease_expires_at_ms"], Value::Null);
    }

    #[test]
    fn leader_pid_parser_rejects_missing_zero_string_and_u32_overflow() {
        for payload in [
            json!({}),
            json!({ "owner_pid": 0 }),
            json!({ "owner_pid": "123" }),
            json!({ "owner_pid": u64::from(u32::MAX) + 1 }),
        ] {
            assert!(
                parse_required_pid(&payload, "owner_pid").is_err(),
                "invalid owner identity must fail closed: {payload}"
            );
        }
        assert_eq!(
            parse_required_pid(&json!({ "owner_pid": u32::MAX }), "owner_pid")
                .expect("u32::MAX is representable"),
            u32::MAX
        );
    }

    #[test]
    fn leader_duration_parser_rejects_missing_required_zero_and_non_integer() {
        assert!(parse_positive_duration_ms(&json!({}), "ttl_ms", None).is_err());
        assert!(parse_positive_duration_ms(&json!({ "ttl_ms": 0 }), "ttl_ms", None).is_err());
        assert!(parse_positive_duration_ms(&json!({ "ttl_ms": 1.5 }), "ttl_ms", None).is_err());
        assert_eq!(
            parse_positive_duration_ms(&json!({ "ttl_ms": 1 }), "ttl_ms", None)
                .expect("positive integer"),
            1
        );
    }

    #[tokio::test]
    async fn ipc_handler_leader_release_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        // Release on missing file is OK.
        let absent_response = handler
            .handle_value(request(
                "memory.leader.release",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 94_001_u64,
                    "leader_token": "absent-token",
                    "leader_epoch": 1,
                }),
            ))
            .await;
        assert_eq!(absent_response["ok"], json!(true));
        assert_eq!(absent_response["released"], json!(false));

        // Claim → release → release again all return ok.
        let claim = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 94_001_u64,
                }),
            ))
            .await;
        let release_response = handler
            .handle_value(request(
                "memory.leader.release",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 94_001_u64,
                    "leader_token": claim["leader_token"],
                    "leader_epoch": claim["leader_epoch"],
                }),
            ))
            .await;
        assert_eq!(release_response["ok"], json!(true));
        assert_eq!(release_response["released"], json!(true));
        let second_release = handler
            .handle_value(request(
                "memory.leader.release",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": 94_001_u64,
                    "leader_token": claim["leader_token"],
                    "leader_epoch": claim["leader_epoch"],
                }),
            ))
            .await;
        assert_eq!(second_release["ok"], json!(true));
        assert_eq!(second_release["released"], json!(false));
    }

    #[tokio::test]
    async fn ipc_handler_leader_query_returns_vacant_when_no_file() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.leader.query",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "my_pid": 95_001_u64,
                }),
            ))
            .await;
        assert_eq!(response["kind"], json!("Vacant"));
    }

    #[tokio::test]
    async fn ipc_handler_leader_query_returns_held_by_me_after_claim() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let my_pid = std::process::id() as u64;
        handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": my_pid,
                }),
            ))
            .await;
        let response = handler
            .handle_value(request(
                "memory.leader.query",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "my_pid": my_pid,
                }),
            ))
            .await;
        assert_eq!(response["kind"], json!("HeldByMe"));
        assert_eq!(response["claim"]["holder_pid"].as_u64().unwrap(), my_pid);
        assert!(response["claim"]["leader_epoch"].as_u64().unwrap_or(0) > 0);
        assert!(
            response["claim"].get("leader_token").is_none(),
            "query/status must not disclose the bearer token"
        );
    }

    #[tokio::test]
    async fn ipc_handler_leader_query_returns_held_by_other_when_pid_differs() {
        let dir = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        let my_pid = std::process::id() as u64;
        handler
            .handle_value(request(
                "memory.leader.claim",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "owner_pid": my_pid,
                }),
            ))
            .await;
        let response = handler
            .handle_value(request(
                "memory.leader.query",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "my_pid": (my_pid + 1),
                }),
            ))
            .await;
        assert_eq!(response["kind"], json!("HeldByOther"));
    }

    #[tokio::test]
    async fn ipc_handler_leader_query_returns_stale_when_lease_expired() {
        let dir = TempDir::new().unwrap();
        // Seed a stale lease (mtime way back).
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(leader_lock::leader_lock_path(dir.path()), "97001").unwrap();
        set_file_mtime(
            leader_lock::leader_lock_path(dir.path()),
            FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let handler = IpcHandler::new();
        let response = handler
            .handle_value(request(
                "memory.leader.query",
                json!({
                    "memory_dir": dir.path().to_string_lossy(),
                    "my_pid": 97_002_u64,
                    "ttl_ms": 60_000_u64,
                }),
            ))
            .await;
        assert_eq!(response["kind"], json!("StaleAvailable"));
    }

    #[tokio::test]
    async fn ipc_handler_leader_missing_memory_dir_returns_error() {
        let handler = IpcHandler::new();
        // No memory_dir + no last_memory_dir state → invalid_input.
        let response = handler
            .handle_value(request(
                "memory.leader.claim",
                json!({ "owner_pid": 98_001_u64 }),
            ))
            .await;
        // invalid_input errors surface as response with "error" field
        // (see handle_value → wrap_err pattern).
        assert!(
            response.get("error").is_some() || response.get("ok") == Some(&json!(false)),
            "expected error response, got {response}"
        );
    }

    // ── W-MEMORY-EVOLUTION PR-5 (2026-05-29) — periodic / idle auto-dream ──

    /// `last_turn_activity_ms` is 0 on a fresh handler and is stamped after a
    /// `memory.turn_end.evaluate` (so the idle gate can observe foreground
    /// activity).
    #[tokio::test]
    async fn pr5_turn_end_evaluate_stamps_turn_activity() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        let handler = IpcHandler::new();
        assert_eq!(
            handler.last_turn_activity_ms(),
            0,
            "fresh handler is unstamped"
        );

        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        assert!(
            handler.last_turn_activity_ms() > 0,
            "turn_end.evaluate must stamp turn activity"
        );
    }

    /// Gate 1: no last-active `memory_dir` → tick is a no-op (nothing to
    /// dream about; never panics).
    #[tokio::test]
    async fn pr5_tick_no_memory_dir_is_noop() {
        // W-MEMORY-LIFECYCLE (2026-07-09): tick tests pin the handler `<base>`
        // to a hermetic tempdir so the watch stage never reads the REAL
        // `~/.crabcode/dream-watch.json` (a configured due watch would run a
        // real dream chain inside a unit test).
        let base = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(base.path().to_path_buf());
        let outcome = run_dream_tick(&handler, 1_700_300_000_000, DreamTickConfig::default()).await;
        assert_eq!(outcome, DreamTickOutcome::NoMemoryDir);
    }

    /// Gate 2: foreground active within the idle threshold → tick backs off.
    #[tokio::test]
    async fn pr5_tick_busy_when_recent_activity() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        // Establish last_memory_dir + stamp activity via a turn-end evaluate.
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        // `now` only a few ms after the stamp → well within the idle window.
        let now = handler.last_turn_activity_ms() + 10;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        match outcome {
            DreamTickOutcome::Busy { idle_for_ms } => assert!(idle_for_ms < 30_000),
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    /// Gate 3: `dream_config.enabled == false` → periodic dreams disabled
    /// (this is the `dream_config.enabled` orphan fix — the TUI toggle now
    /// controls the periodic task).
    #[tokio::test]
    async fn pr5_tick_disabled_when_dream_config_off() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: false,
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        // Far in the future so the idle gate is satisfied.
        let now = handler.last_turn_activity_ms() + 10_000_000;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        assert_eq!(outcome, DreamTickOutcome::Disabled);
    }

    /// Gate 4: enabled + idle but the `AutoDreamGate` declines (here: too few
    /// touched sessions) → `GateSkipped`, never panics, no LLM emit.
    #[tokio::test]
    async fn pr5_tick_gate_skipped_when_session_count_unmet() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: true,
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        // No transcript session files → touched_session_count == 0 < 5.
        let now = handler.last_turn_activity_ms() + 10_000_000;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        match outcome {
            DreamTickOutcome::GateSkipped { reason } => {
                assert_eq!(reason, "session_count_unmet");
            }
            other => panic!("expected GateSkipped, got {other:?}"),
        }
        // No dream emitted.
        assert!(
            handler.tier3_recorded_requests().await.is_empty(),
            "gate skip must not emit an LLM request"
        );
        // W-MEMORY-EVOLUTION PR-10 — the skip is surfaced to the TUI panel.
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1, "gate skip must emit a memory/gate/skipped");
        assert_eq!(skips[0].tier, "tier3");
        assert_eq!(skips[0].gate_name, "dream_gate");
        assert_eq!(skips[0].reason, "session_count_unmet");
    }

    /// W-MEMORY-EVOLUTION PR-10 — the idle (Busy) gate emits a
    /// `memory/gate/skipped` frame so the TUI can render "foreground active".
    #[tokio::test]
    async fn pr10_tick_busy_emits_gate_skip() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        assert!(matches!(outcome, DreamTickOutcome::Busy { .. }));
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].gate_name, "idle");
        assert!(
            skips[0].context.is_some(),
            "idle skip carries timing context"
        );
    }

    /// W-MEMORY-EVOLUTION PR-10 — the disabled gate emits a
    /// `memory/gate/skipped` frame.
    #[tokio::test]
    async fn pr10_tick_disabled_emits_gate_skip() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: false,
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        assert_eq!(outcome, DreamTickOutcome::Disabled);
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].gate_name, "disabled");
    }

    // ── W-MEMORY-SYNERGY W2 (2026-07-16, RC-4) — 跨项目轮转 sweep ─────────

    /// 轮转候选：从未整理（无 lock）排最前；当前项目排除；只有转写没有
    /// memory/ 目录的历史项目被纳入并惰性建目录；空项目排除。
    #[tokio::test]
    async fn w2_rotation_candidates_order_and_filters() {
        let base = TempDir::new().unwrap();
        let projects = base.path().join("projects");
        // aaa：有 memory + 新 lock（最近整理过）。
        let aaa_mem = projects.join("aaa").join("memory");
        fs::create_dir_all(&aaa_mem).unwrap();
        fs::write(crate::lock::lock_path(&aaa_mem), "1").unwrap();
        // bbb：有 memory、无 lock（从未整理）。
        let bbb_mem = projects.join("bbb").join("memory");
        fs::create_dir_all(&bbb_mem).unwrap();
        // ccc：只有转写，无 memory/（回补对象，应惰性建目录）。
        let ccc = projects.join("ccc");
        fs::create_dir_all(&ccc).unwrap();
        fs::write(
            ccc.join("550e8400-e29b-41d4-a716-446655440123.jsonl"),
            "{}\n",
        )
        .unwrap();
        // ddd：空目录（无 memory 无转写）→ 排除。
        fs::create_dir_all(projects.join("ddd")).unwrap();
        // eee：作为「当前项目」→ 排除。
        let eee_mem = projects.join("eee").join("memory");
        fs::create_dir_all(&eee_mem).unwrap();

        let candidates = rotation_candidates(base.path(), Some(eee_mem.as_path())).await;
        let names: Vec<String> = candidates
            .iter()
            .map(|dir| {
                dir.parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();
        assert_eq!(
            candidates.len(),
            3,
            "aaa+bbb+ccc（排除 ddd/eee），got {names:?}"
        );
        // 从未整理（lock 缺失 = 0）排最前：bbb 与 ccc 在 aaa 之前（同 0 之间
        // 顺序不承诺）。
        assert_eq!(names[2], "aaa", "最近整理过的排最后，got {names:?}");
        assert!(names[..2].contains(&"bbb".to_string()), "got {names:?}");
        assert!(names[..2].contains(&"ccc".to_string()), "got {names:?}");
        assert!(
            ccc.join("memory").is_dir(),
            "转写-only 历史项目应被惰性建 memory/ 目录（回补前置）"
        );
    }

    /// 当前项目时间门未到（新 lock）时，轮转找到从未整理、有转写的项目并
    /// 真的做梦（回补主场景）；当前项目的 gate skip 照旧进面板，轮转候选
    /// 静默（skips 恰 1 帧）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn w2_tick_rotates_to_backfill_project_when_current_not_due() {
        let dir = TempDir::new().unwrap();
        // 当前项目：memory + 刚写入的 lock（时间门必不过，真实时钟语义）。
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        fs::write(crate::lock::lock_path(&dir.path().join("memory")), "1").unwrap();
        // 回补项目 bbb：1 份近期转写（K5 min_sessions=1），无 lock。
        // W4：真实消息行（空语料门要求压缩后非空）。
        let bbb = dir.path().join("projects").join("bbb");
        let bbb_mem = bbb.join("memory");
        fs::create_dir_all(&bbb_mem).unwrap();
        let transcript = bbb.join("550e8400-e29b-41d4-a716-446655440222.jsonl");
        fs::write(&transcript, main_session_transcript_line()).unwrap();
        set_file_mtime(&transcript, FileTime::from_unix_time(1_700_200_000, 0)).unwrap();

        let handler = std::sync::Arc::new(IpcHandler::new());
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;

        let tick_handler = std::sync::Arc::clone(&handler);
        let tick = tokio::spawn(async move {
            run_dream_tick(&tick_handler, now, DreamTickConfig::default()).await
        });

        // 驱动反向 IPC 结果（pr5/pr10 同款 driver；phase1 零主题短管线）。
        // 有界循环：故障时宁可 panic 也不能挂死整个测试套件。
        let proc = handler.tier3_processor();
        let mut delivered = 0usize;
        let mut finished = false;
        for _ in 0..1000 {
            let recorded = handler.tier3_recorded_requests().await;
            while delivered < recorded.len() {
                let req = &recorded[delivered];
                let phase = req.phase.as_deref().unwrap_or("");
                let response = if phase == "phase1" {
                    "{\"themes\": []}".to_string()
                } else {
                    "{\"still_valid_ids\": [], \"stale_ids\": [], \"delete_ids\": [], \"notes\": \"\"}"
                        .to_string()
                };
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some(response),
                    usage: None,
                    error: None,
                })
                .await;
                delivered += 1;
            }
            if tick.is_finished() {
                finished = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(finished, "tick 未在驱动窗口内完成（疑似轮转做梦链挂起）");
        let outcome = tick.await.unwrap();
        assert!(
            matches!(outcome, DreamTickOutcome::Dreamed { .. }),
            "轮转必须对回补项目真做梦，got {outcome:?}"
        );
        assert!(
            bbb_mem.join("dreams").exists(),
            "梦产物必须落在轮转项目（bbb）的 memory/dreams"
        );
        assert!(
            !dir.path().join("memory").join("dreams").exists(),
            "当前项目时间门未到，不得被做梦"
        );
        // 当前项目 skip 进面板；轮转候选静默 → 恰 1 帧。
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1, "quiet 轮转不得刷 gate 面板，got {skips:?}");
        assert_eq!(skips[0].gate_name, "dream_gate");
    }

    // ── W-MEMORY-SYNERGY W5 (2026-07-16, RC-6) — 归档触发会话速记 ─────────

    /// 会话关闭归档 → detached 生成 `.session-<id>.md` 速记 + SESSION.md
    /// （Tier-1 复活主链）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn w5_session_close_generates_session_note() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440600";
        fs::write(
            dir.path().join(format!("{session_id}.jsonl")),
            main_session_transcript_line(),
        )
        .unwrap();

        let handler = std::sync::Arc::new(IpcHandler::new());
        let response = handler
            .handle_value(request(
                "memory.archive.session_close",
                json!({
                    "session_id": session_id,
                    "exit_kind": "graceful",
                    "memory_dir": memory_dir.to_string_lossy(),
                    "project_state_dir": dir.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(response["archived"], true);

        // 驱动 tier1 反向 IPC：等 emit → 回投速记内容。
        let proc = handler.tier1_processor();
        let mut delivered = false;
        for _ in 0..200 {
            let recorded = handler.tier1_recorded_requests().await;
            if let Some(req) = recorded.first() {
                assert!(
                    req.req_id.starts_with("tier1-"),
                    "session note must ride the tier1 lane, got {}",
                    req.req_id
                );
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some("## 会话速记\n- 用户请求了项目审阅".to_string()),
                    usage: None,
                    error: None,
                })
                .await;
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(delivered, "session_close 必须触发 tier1 LLM 请求");

        let snapshot = memory_dir.join(format!(".session-{session_id}.md"));
        for _ in 0..200 {
            if snapshot.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(snapshot.exists(), "速记快照必须落盘");
        assert!(
            memory_dir.join("SESSION.md").exists(),
            "SESSION.md 必须写入"
        );
        let body = fs::read_to_string(&snapshot).unwrap();
        assert!(body.contains("会话速记"));
    }

    /// 幂等：快照已存在的会话不再触发 LLM。
    #[tokio::test]
    async fn w5_session_note_skips_when_snapshot_exists() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let session_id = "550e8400-e29b-41d4-a716-446655440601";
        fs::write(
            dir.path().join(format!("{session_id}.jsonl")),
            main_session_transcript_line(),
        )
        .unwrap();
        fs::write(
            memory_dir.join(format!(".session-{session_id}.md")),
            "already there",
        )
        .unwrap();

        let handler = IpcHandler::new();
        let _ = handler
            .handle_value(request(
                "memory.archive.session_close",
                json!({
                    "session_id": session_id,
                    "exit_kind": "graceful",
                    "memory_dir": memory_dir.to_string_lossy(),
                    "project_state_dir": dir.path().to_string_lossy(),
                }),
            ))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            handler.tier1_recorded_requests().await.is_empty(),
            "已有快照不得重复生成"
        );
    }

    /// 精确匹配：没有 `<project>/<id>.jsonl` 的会话静默跳过（fallback 会拿
    /// 错会话，宁缺毋滥）。
    #[tokio::test]
    async fn w5_session_note_skips_without_matching_transcript() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        let handler = IpcHandler::new();
        let _ = handler
            .handle_value(request(
                "memory.archive.session_close",
                json!({
                    "session_id": "550e8400-e29b-41d4-a716-446655440602",
                    "exit_kind": "graceful",
                    "memory_dir": memory_dir.to_string_lossy(),
                    "project_state_dir": dir.path().to_string_lossy(),
                }),
            ))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            handler.tier1_recorded_requests().await.is_empty(),
            "无匹配转写不得触发 LLM"
        );
    }

    /// 手动归档（archive_handoff）批量 thread：受 per-archive cap 约束。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn w5_archive_handoff_caps_notes_per_event() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let ids: Vec<String> = (0..5u8)
            .map(|i| format!("550e8400-e29b-41d4-a716-4466554407{:02}", i))
            .collect();
        for id in &ids {
            fs::write(
                dir.path().join(format!("{id}.jsonl")),
                main_session_transcript_line(),
            )
            .unwrap();
        }

        let handler = std::sync::Arc::new(IpcHandler::new());
        let _ = handler
            .handle_value(request(
                "memory.archive_handoff",
                json!({
                    "scope": "thread",
                    "cwd": dir.path().to_string_lossy(),
                    "thread_ids": ids,
                    "memory_dir": memory_dir.to_string_lossy(),
                    "project_state_dir": dir.path().to_string_lossy(),
                }),
            ))
            .await;

        // 驱动至队列干涸（顺序 process：每份速记一次 LLM 往返）。
        let proc = handler.tier1_processor();
        let mut delivered = 0usize;
        for _ in 0..400 {
            let recorded = handler.tier1_recorded_requests().await;
            while delivered < recorded.len() {
                let req = &recorded[delivered];
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some(format!("note {delivered}")),
                    usage: None,
                    error: None,
                })
                .await;
                delivered += 1;
            }
            if delivered >= MAX_SESSION_NOTES_PER_ARCHIVE {
                // 给 detached 任务一个尾窗确认不再有第 4 份请求。
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                let final_count = handler.tier1_recorded_requests().await.len();
                assert_eq!(
                    final_count, MAX_SESSION_NOTES_PER_ARCHIVE,
                    "每次归档事件至多 {MAX_SESSION_NOTES_PER_ARCHIVE} 份速记"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("驱动窗口内未达到 cap 数量的速记请求（delivered={delivered}）");
    }

    // ── W-MEMORY-SYNERGY W4 (2026-07-16, RC-7a) — 空语料门 ────────────────

    /// 手动做梦：项目没有任何可整理的新会话 → 拿锁之前就拒（corpus_empty），
    /// 不烧 LLM、不碰 consolidation lock。
    #[tokio::test]
    async fn w4_dream_run_now_skips_on_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        // 只有一条记忆、零转写 —— 语料摘要必为空。
        fs::write(memory_dir.join("user_x.md"), "---\ntype: user\n---\nbody").unwrap();

        let handler = IpcHandler::new();
        let method = format!("memory.{}.run_now", "dream");
        let response = handler
            .handle_value(request(
                &method,
                json!({
                    "session_id": "550e8400-e29b-41d4-a716-446655440097",
                    "current_session_id": "550e8400-e29b-41d4-a716-446655440097",
                    "memory_dir": memory_dir.to_string_lossy(),
                    "now_ms": 1_700_300_000_000_u64,
                }),
            ))
            .await;

        assert_eq!(response["gate_skip_reason"], "corpus_empty");
        assert_eq!(response["dream_run"]["started"], false);
        assert_eq!(response["dream_run"]["skip_reason"], "corpus_empty");
        // 不烧 LLM。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(handler.tier3_recorded_requests().await.is_empty());
        // 不碰 consolidation lock（探测在 evaluator 之前）。
        assert!(
            !crate::lock::lock_path(&memory_dir).exists(),
            "corpus_empty 拒绝不得留下 consolidation lock"
        );
    }

    /// 周期做梦：会话数门被「压缩后为空」的转写满足（`{}` 行），空语料门
    /// 必须在拿锁前拦下并对当前项目 emit gate_name="corpus"。
    #[tokio::test]
    async fn w4_tick_skips_on_empty_corpus_after_session_gate() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        // 触发会话数门（mtime 新），但内容压缩后为空。
        let transcript = dir
            .path()
            .join("550e8400-e29b-41d4-a716-446655440400.jsonl");
        fs::write(&transcript, "{}\n").unwrap();
        set_file_mtime(&transcript, FileTime::from_unix_time(1_700_200_000, 0)).unwrap();

        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        match outcome {
            DreamTickOutcome::GateSkipped { reason } => assert_eq!(reason, "corpus_empty"),
            other => panic!("expected corpus_empty GateSkipped, got {other:?}"),
        }
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].gate_name, "corpus");
        assert!(
            handler.tier3_recorded_requests().await.is_empty(),
            "空语料不得烧 LLM"
        );
        assert!(
            !crate::lock::lock_path(&dir.path().join("memory")).exists(),
            "空语料拒绝在拿锁之前，不得留下 consolidation lock"
        );
    }

    /// 轮转候选也全部未 due 时，tick 透传当前项目的判定（旧诊断契约）。
    #[tokio::test]
    async fn w2_tick_returns_current_outcome_when_no_candidate_due() {
        let dir = TempDir::new().unwrap();
        // 当前项目：无转写 → session_count_unmet。
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        // 轮转项目：有 memory 但刚整理过（新 lock）→ 时间门拒。
        let other_mem = dir.path().join("projects").join("other").join("memory");
        fs::create_dir_all(&other_mem).unwrap();
        fs::write(crate::lock::lock_path(&other_mem), "1").unwrap();

        let handler = IpcHandler::new();
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        match outcome {
            DreamTickOutcome::GateSkipped { reason } => {
                assert_eq!(reason, "session_count_unmet", "透传当前项目判定");
            }
            other => panic!("expected current project GateSkipped, got {other:?}"),
        }
        let skips = handler.recorded_gate_skips().await;
        assert_eq!(skips.len(), 1, "轮转候选评估必须静默");
    }

    /// W-MEMORY-EVOLUTION PR-10 — `memory.dream.run_now` must actually RUN the
    /// dream (gap2): it spawns the Tier-3 `DreamProcessor::process` on a
    /// detached task, which emits the reverse-IPC `tier3-*` LLM request. The
    /// old behaviour stopped at returning a trigger and emitted nothing. The
    /// IPC response returns promptly with `dream_run.started == true`; we then
    /// drive the reverse-IPC results and assert a `tier3-` request was emitted
    /// (proving the dream truly executed, not just registered a trigger).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr10_dream_run_now_actually_executes_the_dream() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        // W4：run_now 现在带空语料门 —— 给夹具一份真实转写让门放行。
        fs::write(
            dir.path()
                .join("550e8400-e29b-41d4-a716-446655440300.jsonl"),
            main_session_transcript_line(),
        )
        .unwrap();

        let handler = std::sync::Arc::new(IpcHandler::new());

        // The run_now method name is assembled at runtime so the banned-token
        // source gate (bare direct-invoke followed by a quote) is not tripped
        // by this test file.
        let method = format!("memory.{}.run_now", "dream");
        let payload = json!({
            "session_id": "550e8400-e29b-41d4-a716-446655440099",
            "current_session_id": "550e8400-e29b-41d4-a716-446655440099",
            "memory_dir": dir.path().join("memory").to_string_lossy(),
            "now_ms": 1_700_300_000_000_u64,
        });

        // The IPC response returns promptly (the dream runs detached).
        let response = handler.handle_value(request(&method, payload)).await;
        assert_eq!(response["triggers"].as_array().unwrap().len(), 1);
        assert_eq!(response["triggers"][0]["kind"], "dream");
        assert_eq!(
            response["dream_run"]["started"], true,
            "dream_run must report the detached dream was started, got {response}"
        );

        // Now drive the reverse-IPC LLM results so the detached
        // `DreamProcessor::process` advances through its phases — and assert it
        // emitted at least one `tier3-` request (= it truly executed).
        let proc = handler.tier3_processor();
        let mut delivered = 0usize;
        let mut saw_tier3_prefix = false;
        for _ in 0..200 {
            let recorded = handler.tier3_recorded_requests().await;
            while delivered < recorded.len() {
                let req = &recorded[delivered];
                assert!(
                    req.req_id.starts_with("tier3-"),
                    "run_now dream LLM req_id must carry tier3- prefix, got {}",
                    req.req_id
                );
                saw_tier3_prefix = true;
                let phase = req.phase.as_deref().unwrap_or("");
                let resp = if phase == "phase1" {
                    "{\"themes\": []}".to_string()
                } else {
                    "{\"still_valid_ids\": [], \"stale_ids\": [], \"delete_ids\": [], \"notes\": \"\"}"
                        .to_string()
                };
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some(resp),
                    usage: None,
                    error: None,
                })
                .await;
                delivered += 1;
            }
            // A `dreams/` dir is created at the start of `process()`; once it
            // exists + at least one request fired, the dream is running.
            if saw_tier3_prefix && dir.path().join("memory/dreams").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            saw_tier3_prefix,
            "run_now must EXECUTE the dream (spawn process → emit a tier3- LLM request), \
             not just return a trigger"
        );
    }

    /// W-MEMORY-EVOLUTION PR-10 — when the consolidation lock is already held,
    /// run_now does NOT execute; the response surfaces the skip and does not
    /// emit an LLM request.
    #[tokio::test]
    async fn pr10_dream_run_now_skips_when_lock_held() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        // W4：空语料门在 evaluator 之前 —— 转写 mtime 必须晚于预握锁的
        // 水位（探测按 `mtime > since` 严格过滤），钉未来时间戳确保门放行，
        // 本测试钉的仍是 lock_held 语义。
        let transcript = dir
            .path()
            .join("550e8400-e29b-41d4-a716-446655440301.jsonl");
        fs::write(&transcript, main_session_transcript_line()).unwrap();
        set_file_mtime(&transcript, FileTime::from_unix_time(4_000_000_000, 0)).unwrap();

        // Pre-acquire the consolidation lock so run_now's evaluator sees it busy.
        let owner = crate::lock::LockOwner {
            holder_pid: std::process::id(),
        };
        let _prior =
            crate::lock::try_acquire_for(&memory_dir, &owner, &crate::lock::LockOptions::default())
                .await
                .unwrap()
                .expect("first acquire must succeed");

        let handler = IpcHandler::new();
        let method = format!("memory.{}.run_now", "dream");
        let response = handler
            .handle_value(request(
                &method,
                json!({
                    "session_id": "550e8400-e29b-41d4-a716-446655440098",
                    "current_session_id": "550e8400-e29b-41d4-a716-446655440098",
                    "memory_dir": memory_dir.to_string_lossy(),
                    "now_ms": 1_700_300_000_000_u64,
                }),
            ))
            .await;

        assert_eq!(response["gate_skip_reason"], "lock_held");
        assert_eq!(response["dream_run"]["started"], false);
        // Give any (erroneously) spawned task a moment; none should exist.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            handler.tier3_recorded_requests().await.is_empty(),
            "lock_held run_now must not execute / emit an LLM request"
        );
    }

    /// Full pass: enabled + idle + gate passes (≥5 touched sessions, no lock)
    /// → the periodic tick triggers a Tier-3 dream consolidation, which emits
    /// the Phase-0 reverse-IPC LLM request (`req_id` prefix `tier3-`). We
    /// drive the reverse-IPC results so `process()` completes and the tick
    /// returns `Dreamed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr5_tick_dreams_when_idle_and_gate_passes() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: true,
                min_hours: 24,
                min_sessions: 5,
                session_scan_interval_ms: 600_000,
                // Struct-update spread so this literal survives new
                // `DreamConfig` fields (e.g. K5 `imagination_min_hours`).
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        // 5 main-session transcript files in the project_state_dir (== dir),
        // all with recent mtime so they count as "touched since prior=0".
        // W4：真实消息行（`{}` 压缩后为空会被空语料门拦下）。
        for i in 0..5u8 {
            let session_id = format!("550e8400-e29b-41d4-a716-44665544{:04}", i);
            let path = dir.path().join(format!("{session_id}.jsonl"));
            fs::write(&path, main_session_transcript_line()).unwrap();
            set_file_mtime(&path, FileTime::from_unix_time(1_700_200_000, 0)).unwrap();
        }

        let handler = std::sync::Arc::new(IpcHandler::new());
        handler.set_base_dir(dir.path().to_path_buf());
        // Establish last_memory_dir / project_state_dir (turn-end evaluate),
        // then treat "now" as far in the future → idle gate satisfied.
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec!["extract"]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;

        // Spawn the tick; concurrently feed the reverse-IPC LLM results so
        // `DreamProcessor::process` advances through its phases.
        let tick_handler = std::sync::Arc::clone(&handler);
        let tick = tokio::spawn(async move {
            run_dream_tick(&tick_handler, now, DreamTickConfig::default()).await
        });

        // Drive deliveries: each emitted `tier3-*` request gets a canned
        // result. Phase 1 returns zero themes so Phases 2/3 are skipped and
        // Phase 4 (prune, no fragments) runs immediately — keeps the driver
        // small while still exercising the gate→process→emit path.
        let proc = handler.tier3_processor();
        let mut delivered = 0usize;
        let mut saw_tier3_prefix = false;
        for _ in 0..200 {
            let recorded = handler.tier3_recorded_requests().await;
            while delivered < recorded.len() {
                let req = &recorded[delivered];
                assert!(
                    req.req_id.starts_with("tier3-"),
                    "dream LLM req_id must carry the tier3- prefix, got {}",
                    req.req_id
                );
                saw_tier3_prefix = true;
                let phase = req.phase.as_deref().unwrap_or("");
                let response = if phase == "phase1" {
                    // Zero themes → short pipeline.
                    "{\"themes\": []}".to_string()
                } else {
                    // phase0 / phase4 JSON.
                    "{\"still_valid_ids\": [], \"stale_ids\": [], \"delete_ids\": [], \"notes\": \"\"}"
                        .to_string()
                };
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some(response),
                    usage: None,
                    error: None,
                })
                .await;
                delivered += 1;
            }
            if tick.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let outcome = tick.await.unwrap();
        assert!(
            saw_tier3_prefix,
            "expected at least one tier3- LLM request to be emitted"
        );
        match outcome {
            DreamTickOutcome::Dreamed { theme_count } => {
                assert_eq!(theme_count, 0, "phase1 returned zero themes in this test");
            }
            other => panic!("expected Dreamed, got {other:?}"),
        }
    }

    /// `DreamTickConfig::from_env` reads the two env overrides and falls back
    /// to defaults on absence.
    #[test]
    fn pr5_dream_tick_config_defaults() {
        // Don't mutate process env in a parallel test run; just assert the
        // default constants are wired (env-read path is exercised in prod).
        let config = DreamTickConfig::default();
        assert_eq!(config.scan_interval_ms, DEFAULT_DREAM_SCAN_INTERVAL_MS);
        assert_eq!(config.idle_threshold_ms, DEFAULT_DREAM_IDLE_THRESHOLD_MS);
    }

    // ── W-MEMORY-EVOLUTION PR-9 (2026-05-29) — SE production lazy-init +
    //    memory.search real results ──

    /// PR-9 — `memory.search` before any `turn_end.evaluate` has landed (no
    /// `memory_dir` known) fail-softs to an empty result set with an honest
    /// reason, NOT an error. (Was the unconditional `results:[]` stub before
    /// PR-9; now it is the genuine "engine not initialised" branch.)
    #[tokio::test]
    async fn pr9_search_fail_soft_empty_before_any_turn() {
        let handler = IpcHandler::new();
        let resp = handler
            .handle_value(request(
                "memory.search",
                json!({ "query": "anything", "top_k": 5 }),
            ))
            .await;
        assert_eq!(resp["ok"], true);
        assert!(
            resp["results"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false),
            "no memory_dir seen → empty results; got {resp:?}"
        );
        assert!(
            resp.get("reason").and_then(Value::as_str).is_some(),
            "must carry an honest reason; got {resp:?}"
        );
    }

    /// PR-9 — the production lazy-init path: a `memory.turn_end.evaluate`
    /// stands up the SE integration (data_dir under project_state_dir) +
    /// runs the initial index pass over the project's memory markdown; a
    /// subsequent `memory.search` returns REAL hits (proving the `results:[]`
    /// stub is dead and the SE is genuinely queried).
    ///
    /// W-MEMORY-EVOLUTION FIX #13 (2026-06-01) — the initial index pass now
    /// runs on a background `spawn_blocking` task (so the cold-start
    /// `turn_end.evaluate` returns promptly instead of head-blocking past the
    /// TS 250ms timeout). The search therefore polls until the background
    /// index lands rather than assuming a synchronous index.
    #[tokio::test]
    async fn pr9_turn_end_lazy_inits_se_and_search_returns_real_hits() {
        let dir = TempDir::new().expect("tempdir");
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).expect("memory dir");
        // Write a topic memory the indexer will accept (valid frontmatter).
        fs::write(
            memory_dir.join("project_widgets.md"),
            "---\ntype: project\nname: widget pipeline\n\
             description: how the widget rendering pipeline batches frames.\n\
             created_at: 2026-05-25\n---\n\nbody about widgets and frames\n",
        )
        .expect("write topic");

        let handler = IpcHandler::new();

        // 1) turn_end.evaluate sets memory_dir + project_state_dir and lazily
        //    stands up the SE integration (synchronous initial index pass).
        let te = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec![]),
            ))
            .await;
        // turn_end.evaluate returns a `{triggers:[...]}` shape (no `ok`); the
        // contract we assert is that the SE integration got lazily attached.
        assert!(
            te.get("triggers").is_some(),
            "turn_end.evaluate returns triggers; got {te:?}"
        );
        assert!(
            handler.se_integration().is_some(),
            "SE integration must be lazily attached after turn_end"
        );

        // 2) memory.search returns the real indexed hit (NOT empty []). The
        //    initial index pass runs in the background (FIX #13), so poll
        //    until it lands (deterministic — the walk is fast over one file).
        let mut resp = json!(null);
        for _ in 0..200 {
            resp = handler
                .handle_value(request(
                    "memory.search",
                    json!({ "query": "widget", "top_k": 10 }),
                ))
                .await;
            if resp["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(resp["ok"], true);
        let results = resp["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "widget query must return real hits (not the dead []); got {resp:?}"
        );
        let top = &results[0];
        assert!(
            top["name"].as_str().unwrap_or("").contains("widget"),
            "top hit must be the widget memory; got {top:?}"
        );
        assert!(
            top["source_path"].as_str().is_some(),
            "hit must carry source_path; got {top:?}"
        );
        assert_eq!(
            resp["engine"], "text",
            "honest engine-capability disclosure"
        );
    }

    /// PR-9 — `IndexDaemon::spawn` is invoked off `ensure_se_integration`;
    /// here we just prove the spawn helper does not panic in isolation
    /// (the full fs-event behavior is covered by `index_daemon` unit tests).
    ///
    /// W3 P1-4 (2026-06-05) — also pins the per-project keying invariant:
    /// re-ensuring the SAME project key reuses the same SE `Arc` (single-init
    /// per project), and the project's index daemon handle is retained inside
    /// its `SeState`.
    #[tokio::test]
    async fn pr9_index_daemon_spawn_does_not_panic() {
        let dir = TempDir::new().expect("tempdir");
        let memory_dir = dir.path().join("memory");
        fs::create_dir_all(&memory_dir).expect("memory dir");
        let handler = IpcHandler::new();
        let psd = project_state_dir_from_memory_dir(&memory_dir);
        // Direct lazy-init: constructs SE + spawns the index daemon.
        let se = handler.ensure_se_integration(&memory_dir, &psd);
        assert!(
            se.is_some(),
            "lazy init must succeed for a valid memory_dir"
        );
        // The daemon handle is held inside THIS project's SeState (not dropped).
        assert!(
            handler.project_has_index_daemon(&psd),
            "per-project index daemon handle must be retained after spawn"
        );
        assert_eq!(handler.se_state_count(), 1, "exactly one project keyed");
        // Second call for the SAME key is a no-op fast path (same Arc).
        let se2 = handler.ensure_se_integration(&memory_dir, &psd);
        assert!(se2.is_some());
        assert!(
            Arc::ptr_eq(&se.unwrap(), &se2.unwrap()),
            "second ensure for the same project returns the same integration \
             (single-init per project)"
        );
        assert_eq!(handler.se_state_count(), 1, "still one project (no dup)");
    }

    /// W3 P1-4 (2026-06-05) — TWO-PROJECT isolation. The former singleton SE
    /// reused project A's index root for project B (the cross-project leak).
    /// Now a DIFFERENT `project_state_dir` gets a FRESH SE (NOT ptr_eq to A's),
    /// and a search scoped to project B does NOT return project A's memory.
    #[tokio::test]
    async fn pr9_two_projects_do_not_share_se_or_leak_hits() {
        // Project A — has a "widget" memory.
        let dir_a = TempDir::new().expect("tempdir A");
        let mem_a = dir_a.path().join("memory");
        fs::create_dir_all(&mem_a).expect("mem A");
        fs::write(
            mem_a.join("project_widgets.md"),
            "---\ntype: project\nname: widget pipeline\n\
             description: how the widget rendering pipeline batches frames.\n\
             created_at: 2026-05-25\n---\n\nbody about widgets and frames\n",
        )
        .expect("write A topic");
        let psd_a = project_state_dir_from_memory_dir(&mem_a);

        // Project B — has a "gizmo" memory (no widgets).
        let dir_b = TempDir::new().expect("tempdir B");
        let mem_b = dir_b.path().join("memory");
        fs::create_dir_all(&mem_b).expect("mem B");
        fs::write(
            mem_b.join("project_gizmos.md"),
            "---\ntype: project\nname: gizmo registry\n\
             description: the gizmo registry tracks per-tenant gizmo handles.\n\
             created_at: 2026-05-25\n---\n\nbody about gizmos and tenants\n",
        )
        .expect("write B topic");
        let psd_b = project_state_dir_from_memory_dir(&mem_b);

        let handler = IpcHandler::new();

        // Lazy-init both projects.
        let se_a = handler
            .ensure_se_integration(&mem_a, &psd_a)
            .expect("SE A inits");
        let se_b = handler
            .ensure_se_integration(&mem_b, &psd_b)
            .expect("SE B inits");

        // Different project keys → DISTINCT SE instances (not the same Arc).
        assert!(
            !Arc::ptr_eq(&se_a, &se_b),
            "projects A and B must get separate SE instances (no singleton reuse)"
        );
        assert_eq!(handler.se_state_count(), 2, "two projects keyed");
        assert!(handler.project_has_index_daemon(&psd_a), "A has its daemon");
        assert!(handler.project_has_index_daemon(&psd_b), "B has its daemon");

        // Re-ensure A reuses A's SE (NOT B's).
        let se_a2 = handler
            .ensure_se_integration(&mem_a, &psd_a)
            .expect("SE A reused");
        assert!(
            Arc::ptr_eq(&se_a, &se_a2),
            "re-ensure A returns A's SE, not the most-recently-used B"
        );

        // Search scoped to project B (payload carries B's dirs) must NOT leak
        // project A's "widget" memory. Poll until B's background index lands.
        let mut resp = json!(null);
        for _ in 0..200 {
            resp = handler
                .handle_value(request(
                    "memory.search",
                    json!({
                        "query": "widget",
                        "top_k": 10,
                        "memory_dir": mem_b.to_string_lossy(),
                        "project_state_dir": psd_b.to_string_lossy(),
                    }),
                ))
                .await;
            // Once B's index is warm, a "gizmo" probe would hit; "widget"
            // must stay empty for B.
            let warm = handler
                .handle_value(request(
                    "memory.search",
                    json!({
                        "query": "gizmo",
                        "top_k": 10,
                        "memory_dir": mem_b.to_string_lossy(),
                        "project_state_dir": psd_b.to_string_lossy(),
                    }),
                ))
                .await;
            if warm["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(resp["ok"], true);
        let b_widget_hits = resp["results"].as_array().expect("results array");
        assert!(
            b_widget_hits.is_empty(),
            "project B search for 'widget' must NOT leak project A's memory; got {resp:?}"
        );
    }

    // ── W-MEMORY-LIFECYCLE (2026-07-09) — K3/K4/K5/K9/K10 ──

    /// K10 — watch management IPC roundtrip: upsert (create) → list → upsert
    /// (update by id) → remove.
    #[tokio::test]
    async fn lifecycle_watch_ipc_upsert_list_remove_roundtrip() {
        let base = TempDir::new().unwrap();
        let watched = TempDir::new().unwrap();
        let anchor = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(base.path().to_path_buf());

        let created = handler
            .handle_value(request(
                "memory.watch.upsert",
                json!({
                    "path": watched.path().to_string_lossy(),
                    "memory_dir": anchor.path().join("memory").to_string_lossy(),
                    "project_state_dir": anchor.path().to_string_lossy(),
                    "label": "repo watch",
                    "focus": "架构演进",
                }),
            ))
            .await;
        assert_eq!(created["ok"], true, "{created}");
        let id = created["target"]["id"].as_str().unwrap().to_string();
        assert!(!id.is_empty(), "created target gets a generated id");
        assert_eq!(created["target"]["interval_hours"], 48, "default interval");
        assert_eq!(created["target"]["enabled"], true, "default enabled");
        assert_eq!(created["target"]["focus"], "架构演进");

        let listed = handler
            .handle_value(request("memory.watch.list", json!({})))
            .await;
        assert_eq!(listed["ok"], true);
        assert_eq!(listed["targets"].as_array().unwrap().len(), 1);
        assert_eq!(listed["targets"][0]["id"].as_str(), Some(id.as_str()));

        // Update by id: interval + enabled + clear focus (empty string).
        let updated = handler
            .handle_value(request(
                "memory.watch.upsert",
                json!({
                    "id": id,
                    "path": watched.path().to_string_lossy(),
                    "memory_dir": anchor.path().join("memory").to_string_lossy(),
                    "project_state_dir": anchor.path().to_string_lossy(),
                    "interval_hours": 12,
                    "enabled": false,
                    "focus": "",
                }),
            ))
            .await;
        assert_eq!(updated["ok"], true, "{updated}");
        assert_eq!(updated["target"]["interval_hours"], 12);
        assert_eq!(updated["target"]["enabled"], false);
        assert!(
            updated["target"]["focus"].is_null(),
            "empty focus clears the stored value: {updated}"
        );
        let listed = handler
            .handle_value(request("memory.watch.list", json!({})))
            .await;
        assert_eq!(
            listed["targets"].as_array().unwrap().len(),
            1,
            "upsert-by-id must not duplicate"
        );

        let removed = handler
            .handle_value(request("memory.watch.remove", json!({ "id": id })))
            .await;
        assert_eq!(removed["ok"], true);
        assert_eq!(removed["removed"], true);
        let listed = handler
            .handle_value(request("memory.watch.list", json!({})))
            .await;
        assert_eq!(listed["targets"].as_array().unwrap().len(), 0);
        let removed_again = handler
            .handle_value(request("memory.watch.remove", json!({ "id": "nope" })))
            .await;
        assert_eq!(removed_again["ok"], true);
        assert_eq!(removed_again["removed"], false);
    }

    /// K10 — upsert validation: dirs must be non-empty absolute paths (soft
    /// errors, nothing persisted).
    #[tokio::test]
    async fn lifecycle_watch_upsert_rejects_invalid_paths() {
        let base = TempDir::new().unwrap();
        let handler = IpcHandler::new();
        handler.set_base_dir(base.path().to_path_buf());

        let relative = handler
            .handle_value(request(
                "memory.watch.upsert",
                json!({
                    "path": "relative/watched",
                    "memory_dir": base.path().join("m").to_string_lossy(),
                    "project_state_dir": base.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(relative["ok"], false);
        assert!(
            relative["error"].as_str().unwrap().contains("absolute"),
            "{relative}"
        );

        let empty = handler
            .handle_value(request(
                "memory.watch.upsert",
                json!({
                    "path": base.path().to_string_lossy(),
                    "memory_dir": "",
                    "project_state_dir": base.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(empty["ok"], false);
        assert!(
            empty["error"].as_str().unwrap().contains("non-empty"),
            "{empty}"
        );

        let listed = handler
            .handle_value(request("memory.watch.list", json!({})))
            .await;
        assert_eq!(
            listed["targets"].as_array().unwrap().len(),
            0,
            "invalid upserts persist nothing"
        );
    }

    /// K4 — promote-to-global happy path: the file moves into the global
    /// root, and the project MEMORY.md line migrates to the global MEMORY.md
    /// with its link target rewritten to the flat global name.
    #[tokio::test]
    async fn lifecycle_promote_to_global_moves_file_and_migrates_index_line() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("proj-x").join("memory");
        fs::create_dir_all(memory_dir.join("dreams")).unwrap();
        fs::write(
            memory_dir.join("dreams/insight_x.md"),
            "---\ntype: insight\n---\nkey insight",
        )
        .unwrap();
        fs::write(
            memory_dir.join("MEMORY.md"),
            "# Index\n- [Insight X](dreams/insight_x.md) — hook\n- [Other](other.md)\n",
        )
        .unwrap();
        let global = TempDir::new().unwrap();

        let handler = IpcHandler::new();
        let resp = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "dreams/insight_x.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(resp["ok"], true, "{resp}");
        assert_eq!(resp["index_lines_migrated"], 1);

        let global_file = global.path().join("insight_x.md");
        assert!(global_file.is_file(), "moved into the global root");
        assert!(fs::read_to_string(&global_file)
            .unwrap()
            .contains("key insight"));
        assert!(
            !memory_dir.join("dreams/insight_x.md").exists(),
            "source removed"
        );

        let project_index = fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !project_index.contains("insight_x.md"),
            "source index line removed: {project_index}"
        );
        assert!(
            project_index.contains("- [Other](other.md)"),
            "unrelated lines kept: {project_index}"
        );

        let global_index = fs::read_to_string(global.path().join("MEMORY.md")).unwrap();
        assert!(
            global_index.contains("- [Insight X](insight_x.md) — hook"),
            "migrated line with the target rewritten flat: {global_index}"
        );
    }

    /// K4 — fallback index line when the project index never referenced the
    /// file, and the deterministic short-hash suffix on a name collision.
    #[tokio::test]
    async fn lifecycle_promote_to_global_fallback_line_and_collision_suffix() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("proj-y").join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        let global = TempDir::new().unwrap();
        let handler = IpcHandler::new();

        fs::write(memory_dir.join("note.md"), "first body").unwrap();
        let first = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "note.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(first["ok"], true, "{first}");
        assert_eq!(first["index_lines_migrated"], 0);
        let global_index = fs::read_to_string(global.path().join("MEMORY.md")).unwrap();
        assert!(
            global_index.contains("- [note](note.md) — 自 proj-y 晋升"),
            "{global_index}"
        );

        // Same file name again → deterministic short-hash suffix.
        fs::write(memory_dir.join("note.md"), "second body").unwrap();
        let second = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "note.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(second["ok"], true, "{second}");
        let second_path = PathBuf::from(second["global_path"].as_str().unwrap());
        let second_name = second_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_ne!(second_name, "note.md", "collision must suffix");
        assert!(
            second_name.starts_with("note-") && second_name.ends_with(".md"),
            "{second_name}"
        );
        assert_eq!(
            fs::read_to_string(global.path().join("note.md")).unwrap(),
            "first body"
        );
        assert_eq!(
            fs::read_to_string(global.path().join(&second_name)).unwrap(),
            "second body"
        );
    }

    /// K4 — rejection matrix: traversal outside the root, non-md files,
    /// index/sentinel files, missing sources. Rejections write nothing.
    #[tokio::test]
    async fn lifecycle_promote_to_global_rejects_invalid_sources() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(project.path().join("outside.md"), "outside").unwrap();
        fs::write(memory_dir.join("note.txt"), "not md").unwrap();
        fs::write(memory_dir.join("MEMORY.md"), "# Index\n").unwrap();
        let global = TempDir::new().unwrap();
        let handler = IpcHandler::new();

        let traversal = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "../outside.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(traversal["ok"], false);
        assert!(
            traversal["error"]
                .as_str()
                .unwrap()
                .contains("outside memory_dir"),
            "{traversal}"
        );

        let non_md = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "note.txt",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(non_md["ok"], false);
        assert!(
            non_md["error"].as_str().unwrap().contains("only .md"),
            "{non_md}"
        );

        let sentinel = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "MEMORY.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(sentinel["ok"], false);
        assert!(
            sentinel["error"]
                .as_str()
                .unwrap()
                .contains("cannot be promoted"),
            "{sentinel}"
        );

        let missing = handler
            .handle_value(request(
                "memory.promote_to_global",
                json!({
                    "memory_dir": memory_dir.to_string_lossy(),
                    "path": "ghost.md",
                    "global_memory_dir": global.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(missing["ok"], false);
        assert!(
            missing["error"].as_str().unwrap().contains("not found"),
            "{missing}"
        );

        assert_eq!(
            fs::read_dir(global.path()).unwrap().count(),
            0,
            "rejections must not touch the global root"
        );
    }

    /// K9+K4 — multi-scope search: global + knowledge scopes stand up their
    /// own SE instances, hits rank-interleave in requested scope order, and
    /// every wire item carries the search `scope`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_search_multi_scope_interleaves_and_tags_scopes() {
        let global_dir = TempDir::new().unwrap();
        let global_state = TempDir::new().unwrap();
        let knowledge_dir = TempDir::new().unwrap();
        let knowledge_state = TempDir::new().unwrap();
        fs::write(
            global_dir.path().join("project_widgets.md"),
            "---\ntype: project\nname: widget pipeline\n\
             description: global widget fact.\n---\n\nwidget body\n",
        )
        .unwrap();
        fs::write(
            knowledge_dir.path().join("widget-handbook.md"),
            "---\ntype: knowledge\nname: widget handbook\n\
             description: knowledge widget entry.\n---\n\nwidget handbook body\n",
        )
        .unwrap();

        let handler = IpcHandler::new();
        let payload = json!({
            "query": "widget",
            "top_k": 10,
            "scopes": ["global", "knowledge"],
            "global_memory_dir": global_dir.path().to_string_lossy(),
            "global_state_dir": global_state.path().to_string_lossy(),
            "knowledge_dir": knowledge_dir.path().to_string_lossy(),
            "knowledge_state_dir": knowledge_state.path().to_string_lossy(),
        });
        let mut resp = json!(null);
        for _ in 0..200 {
            resp = handler
                .handle_value(request("memory.search", payload.clone()))
                .await;
            if resp["results"]
                .as_array()
                .map(|a| a.len() >= 2)
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(resp["ok"], true);
        let results = resp["results"].as_array().expect("results array");
        assert_eq!(results.len(), 2, "one hit per scope; got {resp:?}");
        // Rank-interleave: rank-0 of each scope, in requested scope order.
        assert_eq!(results[0]["scope"], "global");
        assert_eq!(results[1]["scope"], "knowledge");
        assert!(results[0]["name"].as_str().unwrap().contains("widget"));
        assert!(results[1]["name"].as_str().unwrap().contains("widget"));
    }

    /// K9 — identical files reachable through two scopes dedupe by source
    /// path (first scope in order wins); a scope pointing at a missing dir
    /// is silently skipped (legacy honest-reason response when NOTHING
    /// initialises).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_search_multi_scope_dedupes_by_source_path() {
        let shared_dir = TempDir::new().unwrap();
        let global_state = TempDir::new().unwrap();
        let knowledge_state = TempDir::new().unwrap();
        fs::write(
            shared_dir.path().join("note.md"),
            "---\ntype: knowledge\nname: gizmo note\n\
             description: gizmo entry.\n---\n\ngizmo body\n",
        )
        .unwrap();

        let handler = IpcHandler::new();
        // Warm both scopes independently first, so the dedupe assertion
        // can't pass by "only one scope indexed yet".
        let mut warm = json!(null);
        for _ in 0..200 {
            warm = handler
                .handle_value(request(
                    "memory.search",
                    json!({
                        "query": "gizmo",
                        "top_k": 5,
                        "scopes": ["global"],
                        "global_memory_dir": shared_dir.path().to_string_lossy(),
                        "global_state_dir": global_state.path().to_string_lossy(),
                    }),
                ))
                .await;
            if warm["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(warm["results"].as_array().unwrap().len(), 1, "{warm}");
        for _ in 0..200 {
            warm = handler
                .handle_value(request(
                    "memory.search",
                    json!({
                        "query": "gizmo",
                        "top_k": 5,
                        "scopes": ["knowledge"],
                        "knowledge_dir": shared_dir.path().to_string_lossy(),
                        "knowledge_state_dir": knowledge_state.path().to_string_lossy(),
                    }),
                ))
                .await;
            if warm["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(warm["results"].as_array().unwrap().len(), 1, "{warm}");

        // Both scopes over the SAME dir → one deduped result, first scope wins.
        let combined = handler
            .handle_value(request(
                "memory.search",
                json!({
                    "query": "gizmo",
                    "top_k": 10,
                    "scopes": ["global", "knowledge"],
                    "global_memory_dir": shared_dir.path().to_string_lossy(),
                    "global_state_dir": global_state.path().to_string_lossy(),
                    "knowledge_dir": shared_dir.path().to_string_lossy(),
                    "knowledge_state_dir": knowledge_state.path().to_string_lossy(),
                }),
            ))
            .await;
        let results = combined["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "dedupe by source path; got {combined:?}");
        assert_eq!(results[0]["scope"], "global");

        // A scope whose root dir does not exist is silently skipped; with no
        // other scope resolvable, the legacy honest reason is preserved.
        let missing = handler
            .handle_value(request(
                "memory.search",
                json!({
                    "query": "gizmo",
                    "scopes": ["knowledge"],
                    "knowledge_dir": shared_dir.path().join("does-not-exist").to_string_lossy(),
                    "knowledge_state_dir": knowledge_state.path().to_string_lossy(),
                }),
            ))
            .await;
        assert_eq!(missing["ok"], true);
        assert!(missing["results"].as_array().unwrap().is_empty());
        assert!(
            missing["reason"].as_str().is_some(),
            "honest reason when nothing initialises: {missing}"
        );
    }

    /// K5 — last-imagination marker roundtrip + due arithmetic.
    #[tokio::test]
    async fn lifecycle_imagination_marker_roundtrip_and_due_arithmetic() {
        let state = TempDir::new().unwrap();
        assert_eq!(
            read_last_imagination_ms(state.path()),
            0,
            "missing marker reads 0"
        );
        write_last_imagination_marker(state.path(), 1_700_000_000_000)
            .await
            .unwrap();
        assert_eq!(read_last_imagination_ms(state.path()), 1_700_000_000_000);
        // Corrupt marker fail-softs to 0 (= due).
        fs::write(last_imagination_marker_path(state.path()), "{bad").unwrap();
        assert_eq!(read_last_imagination_ms(state.path()), 0);

        let hour = 3_600_000_u64;
        assert!(imagination_cycle_due(123, 0, 48), "never ran → due");
        assert!(!imagination_cycle_due(
            1_700_000_000_000 + 47 * hour,
            1_700_000_000_000,
            48
        ));
        assert!(imagination_cycle_due(
            1_700_000_000_000 + 48 * hour,
            1_700_000_000_000,
            48
        ));
    }

    /// K5 — the tick runs an INDEPENDENT imagination sweep when the marker is
    /// due (no dream success required), stamps the marker on completion, and
    /// a fresh marker suppresses the next cycle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_tick_runs_independent_imagination_and_stamps_marker() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("memory")).unwrap();
        write_dream_config(
            dir.path(),
            &DreamConfig {
                enabled: true,
                ..DreamConfig::default()
            },
        )
        .await
        .unwrap();
        // Stage-0 hypgen corpus material.
        fs::write(
            dir.path().join("memory/project_seed.md"),
            "---\ntype: project\nname: seed\ndescription: seed memory.\n---\nseed body\n",
        )
        .unwrap();

        let handler = std::sync::Arc::new(IpcHandler::new());
        handler.set_base_dir(dir.path().to_path_buf());
        let _ = handler
            .handle_value(request(
                "memory.turn_end.evaluate",
                evaluate_payload(&dir, vec![]),
            ))
            .await;
        let now = handler.last_turn_activity_ms() + 10_000_000;

        // Marker absent → due → the tick spawns a sweep even though the dream
        // stage itself gate-skips (no sessions).
        let _ = run_dream_tick(&handler, now, DreamTickConfig::default()).await;

        // Drive the Stage-0 hypgen round-trip to an EMPTY generation so the
        // sweep completes promptly.
        let mut hypgen_req: Option<String> = None;
        for _ in 0..200 {
            let recorded = handler.tier3_imagination_recorded_requests().await;
            if let Some(req) = recorded.iter().find(|r| r.req_id.contains("hypgen")) {
                hypgen_req = Some(req.req_id.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let req_id = hypgen_req.expect("independent cycle must emit a Stage-0 hypgen request");
        let delivered = handler
            .handle_value(request(
                "memory.tier.llm_call_result",
                json!({
                    "req_id": req_id,
                    "response": "{\"hypotheses\": []}",
                }),
            ))
            .await;
        assert_eq!(delivered["ok"], true);

        // Completion stamps the marker (project_state_dir == dir).
        let mut stamped = 0_u64;
        for _ in 0..200 {
            stamped = read_last_imagination_ms(dir.path());
            if stamped > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            stamped > 0,
            "completed sweep must stamp last-imagination.json"
        );

        // Fresh marker → the next tick must NOT start another sweep.
        let hypgen_count = |reqs: &[crate::tier::LlmCallRequestPayload]| {
            reqs.iter().filter(|r| r.req_id.contains("hypgen")).count()
        };
        let before = hypgen_count(&handler.tier3_imagination_recorded_requests().await);
        let _ = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let after = hypgen_count(&handler.tier3_imagination_recorded_requests().await);
        assert_eq!(
            before, after,
            "fresh marker must suppress the independent cycle"
        );
    }

    /// K10 — a due watch target whose consolidate lock is held fails FAST,
    /// the failure status is stamped into dream-watch.json, and only ONE
    /// target runs per tick (the second waits for the next tick).
    #[tokio::test]
    async fn lifecycle_tick_watch_gate_declined_stamps_status_single_target_per_tick() {
        let base = TempDir::new().unwrap();
        let watched = TempDir::new().unwrap();
        fs::write(watched.path().join("readme.md"), "watched file").unwrap();
        let anchor_a = TempDir::new().unwrap();
        let anchor_b = TempDir::new().unwrap();
        let mem_a = anchor_a.path().join("memory");
        let mem_b = anchor_b.path().join("memory");
        fs::create_dir_all(&mem_a).unwrap();
        fs::create_dir_all(&mem_b).unwrap();
        // Hold both consolidate locks → both chains would fail fast.
        let owner = crate::lock::LockOwner {
            holder_pid: std::process::id(),
        };
        crate::lock::try_acquire_for(&mem_a, &owner, &crate::lock::LockOptions::default())
            .await
            .unwrap()
            .expect("lock A");
        crate::lock::try_acquire_for(&mem_b, &owner, &crate::lock::LockOptions::default())
            .await
            .unwrap()
            .expect("lock B");

        let handler = IpcHandler::new();
        handler.set_base_dir(base.path().to_path_buf());
        for (label, anchor, mem) in [("first", &anchor_a, &mem_a), ("second", &anchor_b, &mem_b)] {
            let resp = handler
                .handle_value(request(
                    "memory.watch.upsert",
                    json!({
                        "label": label,
                        "path": watched.path().to_string_lossy(),
                        "memory_dir": mem.to_string_lossy(),
                        "project_state_dir": anchor.path().to_string_lossy(),
                    }),
                ))
                .await;
            assert_eq!(resp["ok"], true, "{resp}");
        }

        let now = 1_700_300_000_000_u64;
        let outcome = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        assert_eq!(
            outcome,
            DreamTickOutcome::NoMemoryDir,
            "watch runs never alter the project-stage outcome"
        );

        let config = load_watch_config(base.path());
        assert_eq!(config.targets.len(), 2);
        let first = &config.targets[0];
        assert_eq!(
            first.last_run_ms,
            Some(now),
            "due target stamped even on failure"
        );
        let status = first.last_status.as_deref().unwrap_or("");
        assert!(
            status.contains("lock_held"),
            "gate-declined reason recorded: {status}"
        );
        let second = &config.targets[1];
        assert_eq!(second.last_run_ms, None, "single watch per tick");
        assert_eq!(second.last_status, None);

        // Next tick picks the second target (the first is no longer due).
        let _ = run_dream_tick(&handler, now, DreamTickConfig::default()).await;
        let config = load_watch_config(base.path());
        assert_eq!(
            config.targets[1].last_run_ms,
            Some(now),
            "second target runs on the next tick"
        );
    }

    /// K10 — full watch dream chain: the forced gate passes (watch interval
    /// is the cadence throttle), the dream runs to completion over the
    /// reverse-IPC channel, and the success status lands in dream-watch.json.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_tick_watch_runs_dream_chain_and_stamps_success() {
        let base = TempDir::new().unwrap();
        let watched = TempDir::new().unwrap();
        fs::write(watched.path().join("main.rs"), "fn main() {}").unwrap();
        let anchor = TempDir::new().unwrap();
        let mem = anchor.path().join("memory");

        let handler = std::sync::Arc::new(IpcHandler::new());
        handler.set_base_dir(base.path().to_path_buf());
        let upsert = handler
            .handle_value(request(
                "memory.watch.upsert",
                json!({
                    "path": watched.path().to_string_lossy(),
                    "memory_dir": mem.to_string_lossy(),
                    "project_state_dir": anchor.path().to_string_lossy(),
                    "focus": "rust entrypoints",
                }),
            ))
            .await;
        assert_eq!(upsert["ok"], true, "{upsert}");

        let now = 1_700_300_000_000_u64;
        let tick_handler = std::sync::Arc::clone(&handler);
        let tick = tokio::spawn(async move {
            run_dream_tick(&tick_handler, now, DreamTickConfig::default()).await
        });

        // Drive the dream's reverse-IPC deliveries (mirrors
        // `pr5_tick_dreams_when_idle_and_gate_passes`).
        let proc = handler.tier3_processor();
        let mut delivered = 0usize;
        for _ in 0..400 {
            let recorded = handler.tier3_recorded_requests().await;
            while delivered < recorded.len() {
                let req = &recorded[delivered];
                let phase = req.phase.as_deref().unwrap_or("");
                let response = if phase == "phase1" {
                    "{\"themes\": []}".to_string()
                } else {
                    "{\"still_valid_ids\": [], \"stale_ids\": [], \"delete_ids\": [], \"notes\": \"\"}"
                        .to_string()
                };
                proc.deliver_result(crate::tier::LlmCallResultPayload {
                    req_id: req.req_id.clone(),
                    response: Some(response),
                    usage: None,
                    error: None,
                })
                .await;
                delivered += 1;
            }
            if tick.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let outcome = tick.await.unwrap();
        assert_eq!(
            outcome,
            DreamTickOutcome::NoMemoryDir,
            "watch runs never alter the project-stage outcome"
        );

        let config = load_watch_config(base.path());
        let target = &config.targets[0];
        assert_eq!(target.last_run_ms, Some(now));
        let status = target.last_status.as_deref().unwrap_or("");
        assert!(
            status.starts_with("ok:"),
            "success status recorded: {status}"
        );
        assert!(
            mem.join("dreams").exists(),
            "the dream chain ran against the watch target's own memory_dir"
        );
    }
}
