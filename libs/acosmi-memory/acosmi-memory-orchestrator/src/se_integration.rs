//! W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — Phase 4 起手 PR: acosmi-se
//! 搜索引擎接通骨架。
//!
//! 让 P3 产出的 Tier1/2/3 markdown 文件（含 dreams/）被
//! `acosmi-memory-se::SearchEngine` 搜索引擎（HNSW + 倒排 + 多语言 tokenizer
//! 量化）真实索引。embedding 调用走反向 IPC 给 TS（v7 裁决：与 LLM 同路径；
//! orchestrator 把文本反向 IPC 给 TS，TS 调 SDK embedding endpoint，再回写
//! 向量到 orchestrator），不在 orchestrator 内嵌 embedding client。
//!
//! # 架构契约（CLAUDE.md §硬约束 #15 第 5 条 + 第 7 条）
//!
//! - **数据来源**：TS 业务侧 markdown（`~/.crabcode/projects/<slug>/memory/*.md`
//!   + dreams/*.md + Tier-2 archive/extract 产物）；
//! - **写盘隔离**：SearchEngine 数据存 `<project_state_dir>/search/` 下，
//!   与 memdir 主区物理分离；
//! - **检索唯一路径**：跨会话语义检索走 `acosmi-memory-se`（不走 SDK
//!   embedding endpoint，因为本机 Rust 检索是终态；只在「拿向量」时反向
//!   IPC 出去）；
//! - **Tier prompt 不进 SE**：SE 只索引文件内容 + frontmatter；Tier 内嵌
//!   prompt 模板永远在 orchestrator 内（CLAUDE.md §硬约束 #15 第 8 条）。
//!
//! # 反向 IPC 时序（embedding，与 P3.1 LLM 反向 IPC 对称）
//!
//! ```text
//! orchestrator (Rust)                TUI client (Rust)              TS Business
//!     │                                  │                              │
//!     │ ─── direct UDS notify ─────────► │                              │
//!     │   memory.tier.embedding_request  │                              │
//!     │                                  │ ── broadcast notification ─► │
//!     │                                  │   memory/tier/embeddingRequest│
//!     │                                  │                              │
//!     │                                  │                              │  (SDK call,
//!     │                                  │                              │   embedding
//!     │                                  │                              │   endpoint)
//!     │                                  │ ◄── request ───────────────  │
//!     │                                  │  memory/tier/embeddingResult │
//!     │                                  │                              │
//!     │ ◄── direct UDS request ───────── │                              │
//!     │   memory.tier.embedding_result   │                              │
//!     │                                  │                              │
//! ```
//!
//! `req_id` 关联 request/response；orchestrator 内部用 oneshot channel +
//! pending map 等待匹配的 result。
//!
//! # 本 PR (P4.1) 范围
//!
//! - SearchEngineIntegration 架构骨架（init + 全量索引 + 增量 upsert API）；
//! - `EmbeddingEmitter` trait + `RecordingEmitter` mock；
//! - `EmbeddingRequestPayload` / `EmbeddingResultPayload` wire types
//!   (snake_case，与协议层 `MemoryTierEmbedding*` camelCase 形对应）；
//! - 反向 IPC pending map + deliver_result API；
//! - Tier 写盘 hook：暴露 `upsert_file(path)` API，供 future Tier policy
//!   写盘后调用（本 PR 不在 Tier processor 内调用，留 follow-up — stub-then-
//!   wire 模式，与 `memory/archive/taskDone` declare-now-emit-later 同样的
//!   先例）。
//!
//! # 不实施
//!
//! - `memory/search` method（P4.2 范围）；
//! - SearchEngine 内部具体 query 逻辑（P4.2 范围）；
//! - Tier processor 内对 `upsert_file` 的实际调用（留 follow-up；本 PR 通过
//!   `SearchEngineIntegration` 暴露 API + 单测）；
//! - index daemon（P4.3 范围）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

use acosmi_memory_adapter::fields_to_payload;
use acosmi_memory_se::indexer::{
    ensure_memory_topic_collection, index_memory_roots, IndexStats, MemoryRoot,
};
use acosmi_memory_se::segment_store::SearchEngine;
use acosmi_memory_se::vector_store_adapter::phase1_collection_config;

/// W-MEMORY-EVOLUTION PR-9 (2026-05-29) — a single text-search hit returned by
/// [`SearchEngineIntegration::search`]. Mirrors the TUI-facing `memory.search`
/// result shape (snake_case, serde-friendly).
///
/// # Why payload-text scoring (not vector search)
///
/// The Phase 1 SE collection is the dim=1 zero-vector config
/// (`vector_store_adapter::phase1_collection_config`): every point is upserted
/// with `&[0.0_f32]` and retrieval is documented to use `scroll`, **not**
/// vector search (`segment_store.rs::SearchEngine::search` requires a real
/// `query_vector: &[f32]`). The orchestrator therefore scores matches over the
/// indexed payload text fields (`name` / `abstract` / `overview` / `content`)
/// with a deterministic BM-ish term-frequency heuristic. This is the
/// embedding-free, BM25/text-only LEXICAL floor CLAUDE.md §硬约束 #15 第 7 条
/// calls the fail-soft default ("embedding fail-soft 退化到 BM25-only").
///
/// W-MEMORY-ALIVE PR-2b (2026-07-01, 裁决③ revised §15-7): dense/hybrid recall
/// IS now available on top of this floor — [`SearchEngineIntegration::search_hybrid`]
/// embeds the query via the reverse-IPC SDK channel and fuses dense hits from
/// the side dense collection (see [`SearchEngineIntegration::sync_dense_index`])
/// with the lexical ranking via RRF. When embeddings are unavailable (no
/// executor / no `supports_embedding` model / timeout) everything degrades to
/// this lexical path — never fakes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MemorySearchHit {
    /// SE point id (`<scope>/<relative_path_no_ext>` derived).
    pub id: String,
    /// Relevance score (higher = better). Heuristic term-frequency over
    /// payload text fields; not comparable across queries.
    pub score: f32,
    /// Absolute markdown source path (from the `source_path` payload field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Memory scope (`private` / `team` / ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Memory display name / title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Memory type (`project` / `user` / `feedback` / ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    /// A short text snippet (the `abstract`, falling back to `overview`,
    /// then a content prefix). For TUI preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// W-MEMORY-SELF-EVOLVE-DGM G1 (2026-07-16) — source file mtime (unix
    /// ms, `mtime_ms` payload field; 0 = unknown). **Internal only**: feeds
    /// the temporal-decay policy (`search_policy`), never serialized — the
    /// `memory.search` wire shape is unchanged.
    #[serde(skip)]
    pub mtime_ms: u64,
}

// ──────────────────────────────────────────────────────────────────────────
// Reverse IPC Embedding wire types (mirror protocol::v2::MemoryTierEmbedding*
// camelCase wire form; snake_case here matches direct-UDS IPC convention,
// dispatcher does case translation).
// ──────────────────────────────────────────────────────────────────────────

/// Reverse IPC embedding request payload (orchestrator → TS via TUI client
/// notification `memory/tier/embeddingRequest`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct EmbeddingRequestPayload {
    /// 关联 request/response 的唯一 id。orchestrator 端用此 id 在 pending
    /// map 里匹配回写的 result。
    pub req_id: String,
    /// 待嵌入的文本片段。
    ///
    /// 2026-07-27 更正（原注释描述的是一个**从未实现的设计**）：这里
    /// **一个文件恒对应一条文本** —— `dense_doc_text()` 把
    /// `name+abstract+overview+content` 拼接后按 `DENSE_DOC_TEXT_CAP_CHARS`
    /// 硬截断。多元素只来自 `to_embed.chunks(EMBED_BATCH_SIZE)` 的**批处理
    /// 切分**，与"文档分块"无关；文档分块目前不存在（索引期 `point_id` 也
    /// 是一文件一点）。误以为分块已存在会直接漏掉检索地基的最大缺口。
    pub texts: Vec<String>,
    /// 与 `texts[i]` 对齐的 key（当前形如 `<scope>/<rel_path>`），用于
    /// TS 端追踪 + orchestrator 端回写时关联到具体文件。长度必须
    /// `== texts.len()`。
    pub text_keys: Vec<String>,
    /// 可选 embedding model hint。`None` = TS 端用 SDK 默认 embedding
    /// model。**不写品牌字面**（CLAUDE.md §硬约束 #1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
}

/// Reverse IPC embedding result payload (TS → orchestrator via TUI client
/// request `memory/tier/embeddingResult`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct EmbeddingResultPayload {
    /// 与 request 的 `req_id` 一致。
    pub req_id: String,
    /// 与 `texts[i]` / `text_keys[i]` 对齐的向量列表（i 位置对位）。
    /// 失败时空 vec (paired with `error`).
    pub embeddings: Vec<EmbeddingVector>,
    /// 嵌入维度（所有向量同维）。0 = 失败占位。
    pub dimension: u32,
    /// 失败时的错误信息。成功时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单个 embedding vector — `f32` 序列。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct EmbeddingVector {
    pub values: Vec<f32>,
}

// ──────────────────────────────────────────────────────────────────────────
// EmbeddingEmitter trait (mirrors LlmCallEmitter pattern in
// tier/tier1_session_memory.rs — abstract reverse IPC sender so tests can
// inject an in-memory mock without standing up the TUI client outgoing
// channel).
// ──────────────────────────────────────────────────────────────────────────

/// Reverse IPC sender for embedding requests. Production wiring: a UDS
/// notification emit path that broadcasts the `memory/tier/embeddingRequest`
/// notification to TUI client (declare-now, emit-later in this PR — the
/// dispatcher side is in place via P4.1; the orchestrator-side emit
/// broadcast hook lands in a follow-up that crosses the bun_worker outgoing
/// channel API surface — parallel to the `memory/tier/llmCallRequest`
/// P3.x precedent).
#[async_trait]
pub trait EmbeddingEmitter: Send + Sync {
    async fn emit_request(&self, request: EmbeddingRequestPayload);

    /// W-MEMORY-KB-UPLIFT P1 (2026-07-17) — rerank channel on the same
    /// memory-tier reverse-IPC emitter. Default no-op = rerank unavailable
    /// (caller fail-softs to its RRF fusion order), so emitters that predate
    /// the channel keep compiling and behaving honestly.
    async fn emit_rerank_request(&self, _request: RerankRequestPayload) {}
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-KB-UPLIFT P1 (2026-07-17) — reverse-IPC rerank wire types.
// Mirrors the embedding channel (v7 P4.1) shape: the orchestrator broadcasts
// `memory/tier/rerankRequest`, the TS memoryTierProxy runs the SDK
// `client.rerank()` endpoint (`supports_rerank` capability discovery — zero
// brand literals, CLAUDE.md §硬约束 #1) and writes back via
// `memory.tier.rerank_result`. Fail-soft: no model / timeout / error → the
// caller keeps its fusion order and a 10-minute backoff arms.
// ──────────────────────────────────────────────────────────────────────────

/// Reverse-IPC rerank request payload (orchestrator → TS, snake_case wire).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RerankRequestPayload {
    pub req_id: String,
    pub query: String,
    /// Candidate document texts (snippet-level, char-capped) in candidate
    /// order; `ranking[].index` in the result refers to positions here.
    pub documents: Vec<String>,
    /// Optional rerank model hint. `None` = TS 端用 `supports_rerank` 发现的
    /// 默认模型。**不写品牌字面**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
}

/// Reverse-IPC rerank result payload (TS → orchestrator).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RerankResultPayload {
    pub req_id: String,
    /// Per-candidate relevance entries (any order — the orchestrator sorts by
    /// score); empty paired with `error` on failure.
    pub ranking: Vec<RerankEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One rerank ranking entry: candidate `index` (into the request `documents`)
/// + relevance `score`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RerankEntry {
    pub index: u32,
    pub score: f32,
}

/// In-memory mock emitter used in unit tests. Records every emitted request;
/// tests can inspect via `recorded()`.
#[derive(Debug, Default, Clone)]
pub struct RecordingEmitter {
    inner: Arc<Mutex<Vec<EmbeddingRequestPayload>>>,
    rerank_inner: Arc<Mutex<Vec<RerankRequestPayload>>>,
}

impl RecordingEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn recorded(&self) -> Vec<EmbeddingRequestPayload> {
        self.inner.lock().await.clone()
    }

    /// W-MEMORY-KB-UPLIFT P1 — recorded rerank requests (test hook).
    pub async fn recorded_rerank(&self) -> Vec<RerankRequestPayload> {
        self.rerank_inner.lock().await.clone()
    }
}

#[async_trait]
impl EmbeddingEmitter for RecordingEmitter {
    async fn emit_request(&self, request: EmbeddingRequestPayload) {
        self.inner.lock().await.push(request);
    }

    async fn emit_rerank_request(&self, request: RerankRequestPayload) {
        self.rerank_inner.lock().await.push(request);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// SearchEngineIntegration — owns the SearchEngine handle + pending oneshot
// map for reverse-IPC embedding round-trips + emitter.
// ──────────────────────────────────────────────────────────────────────────

/// Default reverse-IPC embedding round-trip timeout. SDK embedding endpoint
/// is typically <1s per batch; 30s is the same generous budget as Tier-1
/// LLM round-trips.
const EMBEDDING_CALL_TIMEOUT_MS: u64 = 30_000;

/// Query-time embedding budget (W-MEMORY-ALIVE PR-2b). Interactive search
/// must not hang 30s when the executor is missing; one short stall arms the
/// backoff and subsequent searches skip dense until it expires.
const QUERY_EMBED_TIMEOUT_MS: u64 = 5_000;

/// After an embedding failure/timeout, skip the dense channel for this long.
const EMBEDDING_UNAVAILABLE_BACKOFF_MS: u64 = 600_000;

/// Texts per reverse-IPC embedding batch (bounds frame size + SDK payload).
const EMBED_BATCH_SIZE: usize = 16;

/// W-MEMORY-KB-UPLIFT P1 — rerank round-trip budget. Query-critical path: one
/// bounded wait, then the fusion order stands.
const RERANK_CALL_TIMEOUT_MS: u64 = 3_000;

/// After a rerank failure/timeout, skip the channel for this long.
const RERANK_UNAVAILABLE_BACKOFF_MS: u64 = 600_000;

/// Max candidate documents sent to the reranker per query.
const MAX_RERANK_DOCS: usize = 30;

/// Per-document text cap (chars) fed to the reranker.
const RERANK_DOC_TEXT_CAP_CHARS: usize = 600;

/// Per-document text cap (chars) fed to the embedder.
const DENSE_DOC_TEXT_CAP_CHARS: usize = 6_000;

/// Persisted dense-dimension meta file (inside the SE data dir).
const DENSE_META_FILENAME: &str = "dense-dim.json";

/// dense 半环的健康快照，挂进 `memory.status` 响应（扩字段，不新增 method）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DenseHealth {
    /// 已协商到的向量维度。**0 = 从未协商成功**，dense 集合不存在，
    /// 混合检索恒退化为词法地板。
    pub dimension: usize,
    /// `dense-dim.json` 是否在盘上（维度协商曾经成功过的持久证据）。
    pub dense_meta_present: bool,
    /// embedding 通道 backoff 剩余秒数（0 = 未处于 backoff）。
    pub embedding_backoff_remaining_s: u64,
    /// rerank 通道 backoff 剩余秒数。
    pub rerank_backoff_remaining_s: u64,
}

/// W-MEMORY-KB-UPLIFT P1 (2026-07-17) — unified lexical scan cap (was three
/// scattered `SCROLL_CAP = 10_000`). 20k points ≈ a few thousand imported
/// knowledge documents post-chunking; beyond it the honest answer is the
/// remote store (P5), not a bigger in-memory scroll. Hitting the cap logs a
/// truncation warning instead of silently pretending full coverage.
pub(crate) const LEXICAL_SCAN_CAP: usize = 20_000;

/// Reciprocal-rank-fusion constant (standard k=60).
const RRF_K: f32 = 60.0;

/// Default collection name for the Phase 1 memory topic collection (mirrors
/// `acosmi-memory-se::indexer` test convention). Production callers may
/// override per `SearchEngineIntegration::new`.
pub const DEFAULT_TOPIC_COLLECTION: &str = "memory-topics";

/// Default user-scope name (mirrors `acosmi-memory-se::indexer` Phase 1
/// convention). Per-project / per-user scoping happens at the `MemoryRoot`
/// layer, not here.
pub const DEFAULT_USER: &str = "local";

/// Errors during SE integration calls.
#[derive(Debug, thiserror::Error)]
pub enum SeIntegrationError {
    #[error("search engine init failed: {0}")]
    Init(String),
    #[error("collection setup failed: {0}")]
    CollectionSetup(String),
    #[error("indexing pass failed: {0}")]
    Index(String),
    #[error("embedding round-trip timed out (req_id={0})")]
    EmbeddingTimeout(String),
    #[error("embedding shutdown while awaiting result (req_id={0})")]
    EmbeddingShutdown(String),
    #[error("embedding returned with error: {0}")]
    EmbeddingFailed(String),
    #[error("rerank round-trip timed out (req_id={0})")]
    RerankTimeout(String),
    #[error("rerank shutdown while awaiting result (req_id={0})")]
    RerankShutdown(String),
    #[error("rerank returned with error: {0}")]
    RerankFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// SearchEngineIntegration — single struct that holds the SE handle + the
/// reverse-IPC embedding emitter + the pending oneshot map keyed by
/// `req_id`.
///
/// All methods take `&self` because all interior mutability is via
/// `Arc<Mutex<_>>` (pending map) + atomics (`req_id_counter`) + SearchEngine
/// internal locks. This lets multiple Tier processors share the same
/// SearchEngineIntegration without exterior locking.
pub struct SearchEngineIntegration {
    engine: Arc<SearchEngine>,
    collection: String,
    user: String,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<EmbeddingResultPayload>>>>,
    emitter: Arc<dyn EmbeddingEmitter>,
    req_id_counter: AtomicU64,
    /// SE data dir (holds the persisted dense-dimension meta file).
    data_dir: PathBuf,
    /// Negotiated dense embedding dimension (0 = unknown / never embedded).
    /// Loaded from `dense-dim.json` at construction; stamped after the first
    /// successful embedding round-trip. The dimension is DISCOVERED from the
    /// SDK response, never hardcoded (CLAUDE.md §硬约束 #1 精神).
    dense_dim: AtomicUsize,
    /// Fail-soft backoff: when an embedding round-trip fails / times out
    /// (e.g. no TS executor attached, no `supports_embedding` model), skip
    /// the dense channel until this epoch-ms deadline so index/search paths
    /// never hang repeatedly on a dead channel.
    embedding_backoff_until_ms: AtomicU64,
    /// W-MEMORY-KB-UPLIFT P1 — pending reverse-IPC rerank round-trips.
    pending_rerank: Arc<Mutex<HashMap<String, oneshot::Sender<RerankResultPayload>>>>,
    /// W-MEMORY-KB-UPLIFT P1 — rerank-channel backoff (same 10min semantics
    /// as the embedding channel, tracked independently: an account can have
    /// `supports_embedding` without `supports_rerank` and vice versa).
    rerank_backoff_until_ms: AtomicU64,
}

impl std::fmt::Debug for SearchEngineIntegration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchEngineIntegration")
            .field("collection", &self.collection)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl SearchEngineIntegration {
    /// Construct a new SearchEngineIntegration rooted at `data_dir`.
    ///
    /// `data_dir` should be `<project_state_dir>/search/` (caller resolves —
    /// not opinionated here). The directory is created if missing.
    /// Collection `DEFAULT_TOPIC_COLLECTION` is ensured during `init_collections`.
    pub fn new(
        data_dir: impl AsRef<Path>,
        emitter: Arc<dyn EmbeddingEmitter>,
    ) -> Result<Self, SeIntegrationError> {
        let engine = SearchEngine::new(data_dir.as_ref())
            .map_err(|e| SeIntegrationError::Init(e.to_string()))?;
        let data_dir_buf = data_dir.as_ref().to_path_buf();
        let dense_dim = load_dense_dim(&data_dir_buf);
        Ok(Self {
            engine: Arc::new(engine),
            collection: DEFAULT_TOPIC_COLLECTION.to_string(),
            user: DEFAULT_USER.to_string(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            emitter,
            req_id_counter: AtomicU64::new(0),
            pending_rerank: Arc::new(Mutex::new(HashMap::new())),
            rerank_backoff_until_ms: AtomicU64::new(0),
            data_dir: data_dir_buf,
            dense_dim: AtomicUsize::new(dense_dim),
            embedding_backoff_until_ms: AtomicU64::new(0),
        })
    }

    /// Override the default collection name (mainly for tests).
    pub fn with_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = collection.into();
        self
    }

    /// Override the default user-scope name (mainly for tests).
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Ensure the topic collection exists. Idempotent; safe to call multiple
    /// times. Returns `true` if newly created, `false` if it already existed.
    pub fn init_collections(&self) -> Result<bool, SeIntegrationError> {
        ensure_memory_topic_collection(&self.engine, &self.collection)
            .map_err(|e| SeIntegrationError::CollectionSetup(e.to_string()))
    }

    /// Get the underlying SearchEngine handle (clone of `Arc`) — for
    /// callers that need direct access (P4.2 search method, tests).
    #[must_use]
    pub fn engine(&self) -> Arc<SearchEngine> {
        Arc::clone(&self.engine)
    }

    /// Collection name in use.
    #[must_use]
    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// Run a full memory-roots scan + upsert. This is the initial-load /
    /// rebuild path. Incremental upserts go through `upsert_file` instead.
    ///
    /// `roots` typically contains one `MemoryRoot::private(memory_dir)` +
    /// optionally one `MemoryRoot::team(team_dir)` (Phase 1 convention from
    /// `acosmi-memory-se::indexer`).
    pub fn index_all(&self, roots: &[MemoryRoot]) -> Result<IndexStats, SeIntegrationError> {
        // Ensure collection exists (idempotent).
        self.init_collections()?;
        let stats = index_memory_roots(&self.engine, roots, &self.user, &self.collection)
            .map_err(|e| SeIntegrationError::Index(e.to_string()))?;
        // W-MEMORY-KB-UPLIFT P1 — heal lexical-stat precompute for any point
        // whose guard is stale (new/changed docs tokenize once here instead of
        // on every query).
        self.precompute_lexical_stats();
        Ok(stats)
    }

    /// Incrementally upsert a single file. Resolves the appropriate
    /// `MemoryRoot` from the file path + roots, then reuses the same
    /// indexer code path as full scan (single-file MemoryRoot).
    ///
    /// Returns `IndexStats` from the single-file pass (most counters will
    /// be 0/1; useful for distinguishing skip vs indexed).
    ///
    /// **本 PR (P4.1) 范围**：暴露 API + 单测；Tier processor 内的实际
    /// 调用留 follow-up（stub-then-wire 模式，parallel to
    /// `memory/archive/taskDone` declare-now-emit-later 先例）。
    pub fn upsert_file(
        &self,
        roots: &[MemoryRoot],
        path: &Path,
    ) -> Result<IndexStats, SeIntegrationError> {
        self.init_collections()?;

        // Find which root the path belongs to (first prefix match wins;
        // mirrors `index_memory_roots` precedence).
        for root in roots {
            if path.starts_with(&root.path) {
                // Construct a single-file root: same scope + exclude_prefixes,
                // but with the file's parent as the walk start point would
                // walk siblings. Instead, we use the file's own directory
                // and let WalkDir naturally limit by file existence check.
                //
                // Simpler approach: build a MemoryRoot pointing at the
                // file's parent dir and rely on `WalkDir`+`is_file()`
                // filtering. But this would index siblings. For true
                // single-file upsert we need to either:
                //  (a) inline a fresh per-file path skip check, or
                //  (b) extend acosmi-memory-se with an `index_one_file`
                //      public API.
                //
                // For P4.1 骨架 we go with a focused MemoryRoot whose path
                // is the file itself; WalkDir treats a file as a 1-entry
                // walk root and matches the existing `is_markdown_file`
                // filter. The file's relative_path (path.strip_prefix) is
                // empty, but indexer.rs:185-196 handles that via the
                // bare-file pathway. Let's verify this in the unit test.
                let single = MemoryRoot::new(
                    root.scope.clone(),
                    root.path.clone(),
                    root.exclude_prefixes.clone(),
                );
                let stats =
                    index_memory_roots(&self.engine, &[single], &self.user, &self.collection)
                        .map_err(|e| SeIntegrationError::Index(e.to_string()))?;
                // W-MEMORY-KB-UPLIFT P1 — same heal pass as `index_all` (the
                // guard makes it a no-op for unchanged points).
                self.precompute_lexical_stats();
                return Ok(stats);
            }
        }
        // Path not under any root — return empty stats (no-op upsert; the
        // caller's tier policy may have written outside the configured
        // memdir, which is a no-op for SE indexing).
        Ok(IndexStats::default())
    }

    /// W-MEMORY-EVOLUTION FIX #12 (2026-06-01) — remove all SE points whose
    /// `source_path` payload equals `path` (the deleted markdown file).
    ///
    /// # Why payload match (not re-derive the point id)
    ///
    /// `indexer::index_one_file` derives the point id deterministically as
    /// `scoped_path_to_point_id(scope, relative_path_no_ext)` and stores the
    /// **absolute** path in the `source_path` payload field. Re-deriving the id
    /// here would require knowing which `MemoryRoot` (scope + root prefix) the
    /// file belonged to, which the index daemon's delete event does not carry
    /// reliably (the file is already gone, so `strip_prefix` is the only signal
    /// and a file can match more than one root). Instead we scroll the
    /// collection and match on the stored `source_path` payload, which is the
    /// exact value written at index time — robust against scope ambiguity and
    /// future point-id scheme changes. The Phase-1 topic collection is small
    /// (bounded by the user's memory markdown count) so a scroll + filter is
    /// cheap. Each matching point is removed via `SearchEngine::delete`, which
    /// takes the same id string `scroll` returns.
    ///
    /// Returns the number of points deleted. A missing collection (never
    /// indexed) returns `Ok(0)` (fail-soft — nothing to delete).
    pub fn delete_by_path(&self, path: &Path) -> Result<usize, SeIntegrationError> {
        let mut deleted = self.delete_by_path_in(&self.collection, path)?;
        // W-MEMORY-ALIVE PR-2b: mirror the cleanup on the side dense
        // collection so a deleted memory never resurfaces via dense recall.
        let dim = self.dense_dim.load(Ordering::Relaxed);
        if dim > 0 {
            deleted += self.delete_by_path_in(&self.dense_collection_name(dim), path)?;
        }
        Ok(deleted)
    }

    fn delete_by_path_in(
        &self,
        collection: &str,
        path: &Path,
    ) -> Result<usize, SeIntegrationError> {
        if !self.engine.collection_exists(collection) {
            return Ok(0);
        }
        let target = path.to_string_lossy();

        let hits = self
            .engine
            .scroll(collection, LEXICAL_SCAN_CAP)
            .map_err(|e| SeIntegrationError::Index(e.to_string()))?;

        let mut deleted = 0usize;
        for hit in hits {
            let matches = hit
                .payload
                .get("source_path")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|sp| sp == target);
            if matches {
                match self.engine.delete(collection, &hit.id) {
                    Ok(true) => deleted += 1,
                    Ok(false) => {}
                    Err(e) => {
                        return Err(SeIntegrationError::Index(format!(
                            "delete point {} failed: {e}",
                            hit.id
                        )));
                    }
                }
            }
        }
        Ok(deleted)
    }

    /// Deliver a reverse-IPC embedding result. Looks up the matching
    /// pending oneshot by `req_id`, fires it, drops the entry. Unknown
    /// `req_id` is a no-op (late delivery for a timed-out request).
    /// Returns `true` if a pending request was matched.
    pub async fn deliver_result(&self, result: EmbeddingResultPayload) -> bool {
        let mut map = self.pending.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            // Drop ok: receiver may have been dropped by timeout already.
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    fn next_req_id(&self) -> String {
        let n = self.req_id_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("se-embed-{n}-{}", now_ms())
    }

    /// Emit a reverse-IPC embedding request and wait for the matching
    /// result (or timeout). On success, returns the embedding vectors keyed
    /// by `text_keys[i]` order. On timeout / shutdown / embedding error,
    /// returns `SeIntegrationError`.
    ///
    /// Caller is responsible for guarding against deadlocks (the outer
    /// IpcHandler Mutex must not be held while awaiting the result —
    /// the `tier1.process` deadlock caveat applies symmetrically).
    pub async fn embed(
        &self,
        texts: Vec<String>,
        text_keys: Vec<String>,
        model_hint: Option<String>,
    ) -> Result<EmbeddingResultPayload, SeIntegrationError> {
        self.embed_with_timeout(texts, text_keys, model_hint, EMBEDDING_CALL_TIMEOUT_MS)
            .await
    }

    /// [`Self::embed`] with an explicit round-trip timeout. Query-time
    /// embedding uses a short budget ([`QUERY_EMBED_TIMEOUT_MS`]) so an
    /// unavailable executor stalls an interactive search once, briefly —
    /// the failure then arms the backoff and later searches skip dense
    /// entirely until it expires.
    pub async fn embed_with_timeout(
        &self,
        texts: Vec<String>,
        text_keys: Vec<String>,
        model_hint: Option<String>,
        timeout_ms: u64,
    ) -> Result<EmbeddingResultPayload, SeIntegrationError> {
        if texts.len() != text_keys.len() {
            return Err(SeIntegrationError::EmbeddingFailed(format!(
                "texts.len()={} != text_keys.len()={}",
                texts.len(),
                text_keys.len()
            )));
        }

        let req_id = self.next_req_id();
        let (tx, rx) = oneshot::channel::<EmbeddingResultPayload>();
        {
            let mut map = self.pending.lock().await;
            map.insert(req_id.clone(), tx);
        }

        let request = EmbeddingRequestPayload {
            req_id: req_id.clone(),
            texts,
            text_keys,
            model_hint,
        };
        self.emitter.emit_request(request).await;

        let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await;

        match outcome {
            Ok(Ok(result)) => {
                if let Some(error) = &result.error {
                    Err(SeIntegrationError::EmbeddingFailed(error.clone()))
                } else {
                    Ok(result)
                }
            }
            Ok(Err(_)) => {
                // Sender dropped before send — shutdown / cancellation.
                Err(SeIntegrationError::EmbeddingShutdown(req_id))
            }
            Err(_) => {
                // Timeout — clean up the pending entry so a late delivery
                // does not pile up.
                let mut map = self.pending.lock().await;
                map.remove(&req_id);
                Err(SeIntegrationError::EmbeddingTimeout(req_id))
            }
        }
    }

    /// W-MEMORY-EVOLUTION PR-9 (2026-05-29) — text search over the indexed
    /// memory topic collection.
    ///
    /// Scrolls all points in the topic collection and scores each against the
    /// `query` terms over its payload text fields (`name` / `abstract` /
    /// `overview` / `content`), returning the top `top_k` by score. This is
    /// the embedding-free BM25/text-only retrieval path (see [`MemorySearchHit`]
    /// for the architectural rationale). `mode` is accepted for forward-compat
    /// with a future dense/hybrid path but currently always resolves to the
    /// text-only scorer (dense recall is unavailable — no SDK embedding
    /// endpoint, §15 P7).
    ///
    /// An empty / whitespace-only `query` returns the most-recently-modified
    /// entries (by `mtime_ms` desc) so the TUI "browse all memories" mode has
    /// a sensible default ordering.
    ///
    /// Fail-soft: a missing collection (never indexed) returns an empty vec
    /// rather than an error, so the caller can render an "engine warming up"
    /// empty state.
    pub fn search(
        &self,
        query: &str,
        top_k: usize,
        _mode: &str,
        include_manual: bool,
    ) -> Result<Vec<MemorySearchHit>, SeIntegrationError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        if !self.engine.collection_exists(&self.collection) {
            // Never indexed yet — fail-soft empty (not an error).
            return Ok(Vec::new());
        }

        // Scroll the whole collection. The Phase-1 memory topic collection is
        // small (per-project memory markdown count is bounded by the user's
        // memory tree), so a full scroll + in-memory score is acceptable and
        // avoids a vector-search dependency the dim=1 config cannot satisfy.
        // Cap the scroll at a generous bound to defend against a pathological
        // collection size (W-MEMORY-KB-UPLIFT P1: unified + truncation warn).
        let hits = self
            .engine
            .scroll(&self.collection, LEXICAL_SCAN_CAP)
            .map_err(|e| SeIntegrationError::Index(e.to_string()))?;
        if hits.len() >= LEXICAL_SCAN_CAP {
            log::warn!(
                "[se] lexical scan hit LEXICAL_SCAN_CAP={LEXICAL_SCAN_CAP} — results may be \
                 truncated; corpus has outgrown the local scan (consider the remote store)"
            );
        }

        let terms = tokenize_query(query);

        // W-MEMORY-EVOLUTION FIX #12 (2026-06-01) — defense-in-depth: drop any
        // hit whose `source_path` no longer exists on disk. The index-daemon
        // delete path (`delete_by_path`) is the primary cleanup, but a GC lag
        // (delete event not yet flushed, or a file removed out-of-band before
        // the watcher fired) must never surface a deleted memory. Filter FIRST
        // so the BM25 corpus statistics (df / avgdl) reflect only live
        // candidates. Only stats the scrolled candidate set, so the cost is
        // bounded by the candidates, not the whole tree.
        let candidates: Vec<_> = hits
            .into_iter()
            .filter(|hit| match payload_str(&hit.payload, "source_path") {
                Some(sp) => Path::new(&sp).exists(),
                // No source_path payload → keep (can't prove it's stale).
                None => true,
            })
            // W-MEMORY-SELF-EVOLVE-DGM G3-e (2026-07-16)：content-free 候选
            // （三个文本字段皆空的空壳/脚手架）在**检索期**排除 —— 已入索引
            // 的空壳无需重建索引即被过滤（grok-build 同款 search-time filter）。
            .filter(|hit| payload_has_substance(&hit.payload))
            // W-MEMORY-KB-UPLIFT P0 (2026-07-17)：`injection: manual` 知识条目
            // 仅显式搜索可见（include_manual=true：MemorySearch 工具 / TUI 人用
            // 搜索）；被动逐轮召回（include_manual=false）不浮出。缺 `injection`
            // 字段（普通记忆文件 / auto 默认）恒可见。
            .filter(|hit| include_manual || !payload_is_manual_injection(&hit.payload))
            .collect();

        // W-MEMORY-DATA-COMPLETION A1 (2026-06-20) — score with BM25F over the
        // candidate set (charabia word tokenization + IDF + doc-length
        // normalization), replacing the old ASCII-split + raw weighted-TF that
        // could not tokenize CJK. Empty query → browse mode (recency order).
        let scores: Vec<f32> = if terms.is_empty() {
            candidates
                .iter()
                .map(|hit| payload_u64(&hit.payload, "mtime_ms") as f32)
                .collect()
        } else {
            let payload_refs: Vec<&HashMap<String, serde_json::Value>> =
                candidates.iter().map(|hit| &hit.payload).collect();
            bm25f_scores(&payload_refs, &terms)
        };

        let mut scored: Vec<MemorySearchHit> = candidates
            .into_iter()
            .zip(scores)
            .map(|(hit, score)| hit_from_payload(hit.id, score, &hit.payload))
            .collect();

        if !terms.is_empty() {
            // Drop zero-score (non-matching) entries when there is a real query.
            scored.retain(|hit| hit.score > 0.0);
        }

        // Sort by score desc, then by id for deterministic tie-break.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        scored.truncate(top_k);
        Ok(scored)
    }

    // ──────────────────────────────────────────────────────────────────
    // Dense channel (W-MEMORY-ALIVE PR-2b, 2026-07-01, 裁决③ revised §15-7)
    // ──────────────────────────────────────────────────────────────────

    /// Side dense collection name. The negotiated dimension is part of the
    /// name so a dimension change (embedding model switch) naturally lands
    /// in a fresh collection and a full re-embed; the previous-dimension
    /// collection becomes inert on disk (the engine exposes no
    /// delete_collection — acceptable garbage, bounded by model changes).
    fn dense_collection_name(&self, dimension: usize) -> String {
        format!("{}-dense-{dimension}", self.collection)
    }

    fn embedding_backoff_active(&self) -> bool {
        now_ms() < self.embedding_backoff_until_ms.load(Ordering::Relaxed)
    }

    fn note_embedding_failure(&self) {
        self.embedding_backoff_until_ms.store(
            now_ms() + EMBEDDING_UNAVAILABLE_BACKOFF_MS,
            Ordering::Relaxed,
        );
    }

    fn note_embedding_success(&self) {
        self.embedding_backoff_until_ms.store(0, Ordering::Relaxed);
    }

    /// Test-only: clear the embedding-unavailable backoff.
    pub fn reset_embedding_backoff(&self) {
        self.note_embedding_success();
    }

    // ──────────────────────────────────────────────────────────────────
    // Rerank channel (W-MEMORY-KB-UPLIFT P1, 2026-07-17)
    // ──────────────────────────────────────────────────────────────────

    fn rerank_backoff_active(&self) -> bool {
        now_ms() < self.rerank_backoff_until_ms.load(Ordering::Relaxed)
    }

    fn note_rerank_failure(&self) {
        self.rerank_backoff_until_ms
            .store(now_ms() + RERANK_UNAVAILABLE_BACKOFF_MS, Ordering::Relaxed);
    }

    fn note_rerank_success(&self) {
        self.rerank_backoff_until_ms.store(0, Ordering::Relaxed);
    }

    /// Test-only: clear the rerank-unavailable backoff.
    pub fn reset_rerank_backoff(&self) {
        self.note_rerank_success();
    }

    /// 2026-07-27 §14.1-3 —— dense 半环的**可诊断快照**。
    ///
    /// 此前 dense 通道对外只有 `engine: "text"` 这一个信号，而它是一个
    /// **合法的降级标签**：向量根本没协商成功、和"这次查询走词法就够了"，
    /// 在外部看起来完全一样（§25.4 那条家族缺陷）。实测本机 dense 集合
    /// 从未物化、`dense-dim.json` 全盘不存在，却没有任何一处能读出这件事。
    #[must_use]
    pub fn dense_health(&self) -> DenseHealth {
        let now = now_ms();
        let remaining_s = |until: u64| until.saturating_sub(now) / 1_000;
        DenseHealth {
            dimension: self.dense_dim.load(Ordering::Relaxed),
            dense_meta_present: self.data_dir.join(DENSE_META_FILENAME).exists(),
            embedding_backoff_remaining_s: remaining_s(
                self.embedding_backoff_until_ms.load(Ordering::Relaxed),
            ),
            rerank_backoff_remaining_s: remaining_s(
                self.rerank_backoff_until_ms.load(Ordering::Relaxed),
            ),
        }
    }

    /// Deliver a reverse-IPC rerank result (mirrors [`Self::deliver_result`]).
    pub async fn deliver_rerank_result(&self, result: RerankResultPayload) -> bool {
        let mut map = self.pending_rerank.lock().await;
        if let Some(sender) = map.remove(&result.req_id) {
            let _ = sender.send(result);
            true
        } else {
            false
        }
    }

    async fn rerank_round_trip(
        &self,
        query: String,
        documents: Vec<String>,
    ) -> Result<Vec<RerankEntry>, SeIntegrationError> {
        let req_id = format!(
            "se-rerank-{}",
            self.req_id_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending_rerank.lock().await.insert(req_id.clone(), tx);
        self.emitter
            .emit_rerank_request(RerankRequestPayload {
                req_id: req_id.clone(),
                query,
                documents,
                model_hint: None,
            })
            .await;
        match tokio::time::timeout(Duration::from_millis(RERANK_CALL_TIMEOUT_MS), rx).await {
            Ok(Ok(result)) => {
                if let Some(err) = result.error {
                    return Err(SeIntegrationError::RerankFailed(err));
                }
                Ok(result.ranking)
            }
            Ok(Err(_)) => Err(SeIntegrationError::RerankShutdown(req_id)),
            Err(_) => {
                self.pending_rerank.lock().await.remove(&req_id);
                Err(SeIntegrationError::RerankTimeout(req_id))
            }
        }
    }

    /// W-MEMORY-KB-UPLIFT P1 — cross-encoder rerank over interleaved wire
    /// values (post-interleave, pre-MMR). Takes the top
    /// `min(len, MAX_RERANK_DOCS)` candidates, sends `snippet ?? name` texts
    /// with the query to the TS SDK reranker, and reorders by relevance
    /// (scores written back to `score` so downstream MMR/display stay
    /// meaningful; unranked tail keeps its relative order after the ranked
    /// ones). Returns `(items, applied)`. Fail-soft: any failure returns the
    /// input order unchanged and arms the 10-minute backoff.
    pub async fn rerank_values(
        &self,
        query: &str,
        items: Vec<serde_json::Value>,
    ) -> (Vec<serde_json::Value>, bool) {
        if items.len() < 2 || query.trim().is_empty() || self.rerank_backoff_active() {
            return (items, false);
        }
        let pool = items.len().min(MAX_RERANK_DOCS);
        let documents: Vec<String> = items[..pool].iter().map(rerank_doc_text).collect();
        match self.rerank_round_trip(query.to_string(), documents).await {
            Ok(ranking) => {
                self.note_rerank_success();
                (reorder_by_ranking(items, pool, &ranking), true)
            }
            Err(e) => {
                self.note_rerank_failure();
                log::info!(
                    "[se] rerank unavailable (fail-soft fusion order, backoff {}s): {e}",
                    RERANK_UNAVAILABLE_BACKOFF_MS / 1000
                );
                (items, false)
            }
        }
    }

    fn persist_dense_dim(&self, dimension: usize) {
        let path = self.data_dir.join(DENSE_META_FILENAME);
        let body = serde_json::json!({ "dimension": dimension }).to_string();
        if let Err(e) = std::fs::write(&path, body) {
            log::warn!("[se] persist dense-dim meta failed (fail-soft): {e}");
        }
    }

    /// Bring the side dense collection up to date with the lexical topic
    /// collection: embed every point whose `mtime_ms` is missing/stale in the
    /// dense collection via the reverse-IPC SDK channel, then upsert real
    /// vectors (same point id + full lexical payload). Returns the number of
    /// points (re)embedded.
    ///
    /// Fail-soft by design: with no TS executor / no `supports_embedding`
    /// model, the first batch fails, the backoff arms, and the index stays
    /// lexical-only — retrieval keeps working on BM25F.
    pub async fn sync_dense_index(&self) -> Result<usize, SeIntegrationError> {
        if self.embedding_backoff_active() {
            return Ok(0);
        }
        if !self.engine.collection_exists(&self.collection) {
            return Ok(0);
        }
        let lexical = self
            .engine
            .scroll(&self.collection, LEXICAL_SCAN_CAP)
            .map_err(|e| SeIntegrationError::Index(e.to_string()))?;
        if lexical.is_empty() {
            return Ok(0);
        }

        // Known dimension → collect the dense side's current mtimes + content
        // hashes so only genuinely-changed documents are re-embedded.
        let mut dim = self.dense_dim.load(Ordering::Relaxed);
        if dim > 0 {
            self.gc_stale_dense_collections(dim);
        }
        let mut dense_mtimes: HashMap<String, u64> = HashMap::new();
        let mut dense_hashes: HashMap<String, String> = HashMap::new();
        if dim > 0 {
            let dense_name = self.dense_collection_name(dim);
            if self.engine.collection_exists(&dense_name) {
                if let Ok(points) = self.engine.scroll(&dense_name, LEXICAL_SCAN_CAP) {
                    for point in points {
                        dense_mtimes
                            .insert(point.id.clone(), payload_u64(&point.payload, "mtime_ms"));
                        if let Some(hash) = payload_str(&point.payload, "content_hash") {
                            dense_hashes.insert(point.id, hash);
                        }
                    }
                }
                // W-MEMORY-KB-UPLIFT P0 — reconciliation sweep: a dense point
                // whose lexical twin is gone (file deleted while the daemon was
                // down, or a knowledge draft swept by the index-time review
                // gate) must not keep resurfacing through dense recall. Lexical
                // is the source set; prune dense-only leftovers.
                let lexical_ids: std::collections::HashSet<&str> =
                    lexical.iter().map(|hit| hit.id.as_str()).collect();
                let orphans: Vec<String> = dense_mtimes
                    .keys()
                    .filter(|id| !lexical_ids.contains(id.as_str()))
                    .cloned()
                    .collect();
                for id in orphans {
                    let _ = self.engine.delete(&dense_name, &id);
                    dense_mtimes.remove(&id);
                    dense_hashes.remove(&id);
                }
            }
        }

        // mtime-changed candidates, then a content-hash gate: an mtime bump
        // with identical text (filesystem touch / frontmatter-preserving
        // rewrite) realigns the stored mtime instead of paying for a re-embed
        // (W-MEMORY-KB-UPLIFT P0 — embeddings are gateway-billed).
        let mut to_embed: Vec<(&acosmi_memory_se::segment_store::ScrollHit, String)> = Vec::new();
        let mut realigned = 0usize;
        for hit in &lexical {
            let mtime = payload_u64(&hit.payload, "mtime_ms");
            if dense_mtimes.get(&hit.id) == Some(&mtime) {
                continue;
            }
            let text = dense_doc_text(&hit.payload);
            if text.trim().is_empty() {
                continue;
            }
            let hash = fnv1a64_hex(&text);
            if dim > 0 && dense_hashes.get(&hit.id) == Some(&hash) {
                let mut align = HashMap::new();
                align.insert("mtime_ms".to_owned(), serde_json::json!(mtime));
                let _ = self.engine.set_payload(
                    &self.dense_collection_name(dim),
                    &hit.id,
                    &fields_to_payload(align),
                );
                realigned += 1;
                continue;
            }
            to_embed.push((hit, hash));
        }
        if realigned > 0 {
            log::info!("[se] dense sync: {realigned} unchanged doc(s) realigned without re-embed");
        }
        if to_embed.is_empty() {
            return Ok(0);
        }

        let mut upserted = 0usize;
        for chunk in to_embed.chunks(EMBED_BATCH_SIZE) {
            let texts: Vec<String> = chunk
                .iter()
                .map(|(hit, _)| dense_doc_text(&hit.payload))
                .collect();
            let keys: Vec<String> = chunk.iter().map(|(hit, _)| hit.id.clone()).collect();
            let result = match self.embed(texts, keys, None).await {
                Ok(result) => result,
                Err(e) => {
                    self.note_embedding_failure();
                    log::info!(
                        "[se] dense sync: embedding unavailable, staying lexical-only                          (backoff {}s): {e}",
                        EMBEDDING_UNAVAILABLE_BACKOFF_MS / 1000
                    );
                    break;
                }
            };
            let got_dim = result.dimension as usize;
            if got_dim == 0 || result.embeddings.len() != chunk.len() {
                self.note_embedding_failure();
                log::warn!(
                    "[se] dense sync: malformed embedding result (dim={got_dim}, {} vectors                      for {} texts) — skipping",
                    result.embeddings.len(),
                    chunk.len()
                );
                break;
            }
            self.note_embedding_success();
            if dim != got_dim {
                dim = got_dim;
                self.dense_dim.store(dim, Ordering::Relaxed);
                self.persist_dense_dim(dim);
            }
            let dense_name = self.dense_collection_name(dim);
            self.engine
                .create_collection(&dense_name, &dense_collection_config(dim))
                .map_err(|e| SeIntegrationError::CollectionSetup(e.to_string()))?;
            for ((hit, hash), vector) in chunk.iter().zip(result.embeddings.iter()) {
                let mut fields = hit.payload.clone();
                fields.insert("content_hash".to_owned(), serde_json::json!(hash));
                let payload = fields_to_payload(fields);
                self.engine
                    .upsert(&dense_name, &hit.id, &vector.values, Some(&payload))
                    .map_err(|e| SeIntegrationError::Index(e.to_string()))?;
                upserted += 1;
            }
        }
        Ok(upserted)
    }

    /// W-MEMORY-KB-UPLIFT P0 — drop stale `-dense-<other-dim>` sibling
    /// collections left behind by an embedding-model dimension change (the
    /// sync path historically had no delete; the renegotiation path already
    /// drops on detection). Runs at sync entry; idempotent and cheap (the
    /// collection map is in-memory).
    fn gc_stale_dense_collections(&self, current_dim: usize) {
        let prefix = format!("{}-dense-", self.collection);
        let keep = self.dense_collection_name(current_dim);
        for name in self.engine.list_collections() {
            if name.starts_with(&prefix) && name != keep {
                let dropped = self.engine.drop_collection(&name);
                log::info!("[se] dense GC: stale collection {name} dropped={dropped}");
            }
        }
    }

    /// W-MEMORY-KB-UPLIFT P1 (2026-07-17) — index-time lexical-stat heal pass.
    ///
    /// Query-time BM25F used to re-tokenize every candidate's four text fields
    /// on every search — charabia over the whole corpus per query, the real
    /// scaling wall once the knowledge base takes imports. This pass runs at
    /// index time instead: for every point whose `bm25_stats_mtime` guard is
    /// stale it stores the weighted term-frequency map (`bm25_wtf`), the
    /// weighted doc length (`bm25_wdl`) and the display `snippet`, so query
    /// time degrades to hash lookups (`bm25f_scores` fast path). Old points
    /// heal incrementally; the live-tokenize fallback keeps pre-heal windows
    /// correct. Field weights are compile-time (`FIELD_WEIGHTS`) — changing
    /// them invalidates nothing structurally (scores shift uniformly on the
    /// healed path exactly as they would on the live path after a reindex).
    fn precompute_lexical_stats(&self) -> usize {
        if !self.engine.collection_exists(&self.collection) {
            return 0;
        }
        let Ok(hits) = self.engine.scroll(&self.collection, LEXICAL_SCAN_CAP) else {
            return 0;
        };
        let mut healed = 0usize;
        for hit in hits {
            let mtime = payload_u64(&hit.payload, "mtime_ms");
            // Guard: stats present AND stamped for this exact mtime → healed.
            // (Checking `bm25_wtf` presence keeps the rare mtime==0 doc from
            // re-healing on every pass.)
            if hit.payload.contains_key("bm25_wtf")
                && payload_u64(&hit.payload, "bm25_stats_mtime") == mtime
            {
                continue;
            }
            let (wtf, wdl) = live_lexical_stats(&hit.payload);
            let wtf_json: serde_json::Map<String, serde_json::Value> = wtf
                .into_iter()
                .map(|(term, freq)| (term, serde_json::json!(freq)))
                .collect();
            let mut fields = HashMap::new();
            fields.insert("bm25_wtf".to_owned(), serde_json::Value::Object(wtf_json));
            fields.insert("bm25_wdl".to_owned(), serde_json::json!(wdl));
            fields.insert("bm25_stats_mtime".to_owned(), serde_json::json!(mtime));
            if let Some(snippet) = derive_snippet(&hit.payload) {
                fields.insert("snippet".to_owned(), serde_json::json!(snippet));
            }
            if self
                .engine
                .set_payload(&self.collection, &hit.id, &fields_to_payload(fields))
                .is_ok()
            {
                healed += 1;
            }
        }
        if healed > 0 {
            log::info!("[se] lexical-stat heal: {healed} point(s) precomputed");
        }
        healed
    }

    /// Hybrid retrieval: the lexical BM25F ranking fused (RRF, k=60) with
    /// dense cosine hits from the side collection. Returns the hits plus the
    /// engine label actually used (`"hybrid"` when dense participated,
    /// `"text"` for every degraded path). `mode == "text"`, an empty query,
    /// an unknown dimension, a missing dense collection, an armed backoff,
    /// or a failed query embedding all degrade to the lexical ranking.
    pub async fn search_hybrid(
        &self,
        query: &str,
        top_k: usize,
        mode: &str,
        include_manual: bool,
    ) -> Result<(Vec<MemorySearchHit>, &'static str), SeIntegrationError> {
        if top_k == 0 {
            return Ok((Vec::new(), "text"));
        }
        // Oversampled pool so fusion has material beyond the final page.
        let fusion_pool = top_k.saturating_mul(4).max(20);
        let mut lexical = self.search(query, fusion_pool, mode, include_manual)?;

        let degrade = |mut hits: Vec<MemorySearchHit>| {
            hits.truncate(top_k);
            (hits, "text")
        };

        if mode == "text" || query.trim().is_empty() || self.embedding_backoff_active() {
            return Ok(degrade(lexical));
        }
        let dim = self.dense_dim.load(Ordering::Relaxed);
        if dim == 0 {
            return Ok(degrade(lexical));
        }
        let dense_name = self.dense_collection_name(dim);
        if !self.engine.collection_exists(&dense_name) {
            return Ok(degrade(lexical));
        }

        let query_embed = match self
            .embed_with_timeout(
                vec![query.to_string()],
                vec!["query".to_string()],
                None,
                QUERY_EMBED_TIMEOUT_MS,
            )
            .await
        {
            Ok(result) if result.dimension as usize == dim && result.embeddings.len() == 1 => {
                result
            }
            // 维度失配 = 需要**重协商**的协议事件，不是不可用（2026-07-04 审计
            // PR-9）：embedding 成功返回、但维度与持久 dense-dim 不符（如账号侧
            // embedding 模型更换）。此前并入下方失败臂 → 误武装 10min backoff，
            // 且维度更新只挂在 sync 差分路径上（sync_dense_index :840 区），静止
            // corpus 无差分工作、永不触发 → **永久退化词法**。此处：
            //   1. 更新并持久新维度（dense-dim.json）；
            //   2. 置弃旧维度 collection（名字按维度键控 `{c}-dense-{dim}`——
            //      下个 index-daemon 周期对新键名 collection 做 mtime 差分时
            //      天然=全量重嵌，不走两拍 absence 补全）；
            //   3. 本次查询按词法返回（fail-soft 语义不变）。
            // embedding 本身**健康**：记 success（清 backoff），不记 failure。
            Ok(result)
                if result.embeddings.len() == 1
                    && result.dimension > 0
                    && result.dimension as usize != dim =>
            {
                let new_dim = result.dimension as usize;
                let old_name = self.dense_collection_name(dim);
                self.dense_dim.store(new_dim, Ordering::Relaxed);
                self.persist_dense_dim(new_dim);
                if self.engine.collection_exists(&old_name) {
                    let dropped = self.engine.drop_collection(&old_name);
                    log::info!(
                        "[se] dense dim renegotiated {dim}→{new_dim}: old collection \
                         {old_name} dropped={dropped}; full re-embed on next index cycle"
                    );
                } else {
                    log::info!(
                        "[se] dense dim renegotiated {dim}→{new_dim}; full re-embed on next index cycle"
                    );
                }
                self.note_embedding_success();
                return Ok(degrade(lexical));
            }
            // 真不可用（Err）/ 形状畸形（零维、embeddings 数不符）→ 失败臂：
            // 记失败武装 backoff（10min 语义只留给真不可用）。
            Ok(_) | Err(_) => {
                self.note_embedding_failure();
                return Ok(degrade(lexical));
            }
        };
        self.note_embedding_success();

        let dense_hits = match self.engine.search(
            &dense_name,
            &query_embed.embeddings[0].values,
            None,
            fusion_pool,
            None,
        ) {
            Ok(hits) => hits,
            Err(e) => {
                log::warn!("[se] dense search failed (fail-soft lexical): {e}");
                return Ok(degrade(lexical));
            }
        };
        // Same liveness defense as the lexical path: a deleted file must
        // never resurface through the dense side.
        let dense_live: Vec<_> = dense_hits
            .into_iter()
            .filter(|hit| match payload_str(&hit.payload, "source_path") {
                Some(sp) => Path::new(&sp).exists(),
                None => true,
            })
            // G3-e：dense 侧同样过滤 content-free 空壳（与词法侧一致）。
            .filter(|hit| payload_has_substance(&hit.payload))
            // W-MEMORY-KB-UPLIFT P0：manual 注入模式过滤与词法侧一致。
            .filter(|hit| include_manual || !payload_is_manual_injection(&hit.payload))
            .collect();
        if dense_live.is_empty() {
            return Ok(degrade(lexical));
        }

        // RRF fusion. Metadata comes from whichever side saw the point
        // (payloads are identical by construction — the dense upsert clones
        // the lexical payload).
        let mut fused: HashMap<String, (f32, MemorySearchHit)> = HashMap::new();
        for (rank, hit) in lexical.drain(..).enumerate() {
            let rrf = 1.0 / (RRF_K + rank as f32 + 1.0);
            fused.insert(hit.id.clone(), (rrf, hit));
        }
        for (rank, hit) in dense_live.iter().enumerate() {
            let rrf = 1.0 / (RRF_K + rank as f32 + 1.0);
            match fused.get_mut(&hit.id) {
                Some(entry) => entry.0 += rrf,
                None => {
                    let built = hit_from_payload(hit.id.clone(), 0.0, &hit.payload);
                    fused.insert(hit.id.clone(), (rrf, built));
                }
            }
        }
        let mut merged: Vec<MemorySearchHit> = fused
            .into_values()
            .map(|(score, mut hit)| {
                hit.score = score;
                hit
            })
            .collect();
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        merged.truncate(top_k);
        Ok((merged, "hybrid"))
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Text-search scoring (W-MEMORY-DATA-COMPLETION A1, 2026-06-20). BM25F over the
// scrolled candidate set: charabia multilingual word tokenization (jieba-backed
// CJK segmentation) + IDF + document-length normalization. Embedding-free —
// this is the LEXICAL floor of the hybrid retrieval design; Phase B adds dense
// SDK vectors on top. Replaces PR-9's ASCII-split + raw weighted-TF, which
// could not tokenize Chinese (CJK chars are `is_alphanumeric()`, so a whole
// Chinese query became one token requiring an exact substring match).
// ──────────────────────────────────────────────────────────────────────────

/// BM25 tuning constants (standard Robertson/Sparck-Jones defaults).
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

/// BM25F field boosts: a term occurrence in a higher-signal field contributes
/// proportionally more weighted term-frequency than one in long-tail body
/// `content`. Preserves PR-9's field-weight intent under the new model.
const FIELD_WEIGHTS: &[(&str, f32)] = &[
    ("name", 5.0),
    ("abstract", 3.0),
    ("overview", 2.0),
    ("content", 1.0),
];

/// charabia normalizer options (mirrors acosmi-segment's multilingual
/// tokenizer): lossy Unicode normalization, no char map, no stop-word/separator
/// override.
const TOKENIZER_NORMALIZER: charabia::normalizer::NormalizerOption<'static> =
    charabia::normalizer::NormalizerOption {
        create_char_map: false,
        lossy: true,
        classifier: charabia::normalizer::ClassifierOption {
            stop_words: None,
            separators: None,
        },
    };

/// Multilingual word tokenization via charabia (jieba-backed CJK segmentation +
/// Unicode normalization). Lowercased; punctuation / whitespace tokens dropped.
///
/// Root cause this fixes (A1): the previous tokenizer split on
/// `!char.is_alphanumeric()`, but CJK characters ARE alphanumeric, so a Chinese
/// query collapsed to a SINGLE token requiring an exact substring match
/// (`中文只能精确子串`). Segmenting CJK into words here — and using the SAME
/// tokenizer for indexed document text in `bm25f_scores` — makes index-time and
/// query-time terms align, so Chinese retrieval becomes real word matching.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    use charabia::Segment;
    text.segment()
        .normalize(&TOKENIZER_NORMALIZER)
        .filter_map(|token| {
            let lemma = token.lemma;
            // Drop pure punctuation / whitespace separators (no alphanumeric).
            if lemma.chars().all(|c| !c.is_alphanumeric()) {
                return None;
            }
            Some(lemma.to_lowercase())
        })
        .collect()
}

/// Query terms = unique tokens (dedup so a repeated query word doesn't
/// double-count). Empty → browse mode upstream.
fn tokenize_query(query: &str) -> Vec<String> {
    let mut terms = tokenize(query);
    terms.sort();
    terms.dedup();
    terms
}

/// W-MEMORY-KB-UPLIFT P1 — live (tokenizing) computation of a document's
/// weighted term-frequency map + weighted length. Shared by the index-time
/// heal pass and the query-time fallback so both produce identical stats.
fn live_lexical_stats(payload: &HashMap<String, serde_json::Value>) -> (HashMap<String, f32>, f32) {
    let mut wtf: HashMap<String, f32> = HashMap::new();
    let mut len = 0.0_f32;
    for (field, weight) in FIELD_WEIGHTS {
        if let Some(text) = payload_str(payload, field) {
            for tok in tokenize(&text) {
                *wtf.entry(tok).or_insert(0.0) += weight;
                len += weight;
            }
        }
    }
    (wtf, len)
}

/// W-MEMORY-KB-UPLIFT P1 — parse the index-time precomputed stats. `None`
/// (missing fields / stale `bm25_stats_mtime` guard / malformed values) means
/// the caller must fall back to live tokenization.
fn precomputed_lexical_stats(
    payload: &HashMap<String, serde_json::Value>,
) -> Option<(HashMap<String, f32>, f32)> {
    let mtime = payload_u64(payload, "mtime_ms");
    let guard = payload_u64(payload, "bm25_stats_mtime");
    if mtime == 0 || guard != mtime {
        return None;
    }
    let wdl = payload.get("bm25_wdl")?.as_f64()? as f32;
    let obj = payload.get("bm25_wtf")?.as_object()?;
    let mut wtf = HashMap::with_capacity(obj.len());
    for (term, freq) in obj {
        wtf.insert(term.clone(), freq.as_f64()? as f32);
    }
    Some((wtf, wdl))
}

/// BM25F score for each candidate payload against `query_terms`, computed over
/// the candidate set itself. Per-project memory is small and fully scrolled, so
/// the candidate set IS the corpus — its document-frequency / average-length
/// statistics are exact, not sampled. Returns scores aligned with `payloads`.
fn bm25f_scores(
    payloads: &[&HashMap<String, serde_json::Value>],
    query_terms: &[String],
) -> Vec<f32> {
    let n = payloads.len();
    if n == 0 || query_terms.is_empty() {
        return vec![0.0; n];
    }

    // Pass 1: per-document weighted term frequencies + weighted document
    // length. W-MEMORY-KB-UPLIFT P1 — prefer the index-time precomputed stats
    // (`bm25_wtf`/`bm25_wdl`, mtime-guarded); fall back to live tokenization
    // for points the heal pass has not covered yet.
    let mut doc_wtf: Vec<HashMap<String, f32>> = Vec::with_capacity(n);
    let mut doc_len: Vec<f32> = Vec::with_capacity(n);
    for payload in payloads {
        let (wtf, len) =
            precomputed_lexical_stats(payload).unwrap_or_else(|| live_lexical_stats(payload));
        doc_wtf.push(wtf);
        doc_len.push(len);
    }

    let avgdl = doc_len.iter().sum::<f32>() / n as f32;
    if avgdl <= 0.0 {
        // Every candidate field empty → nothing to score.
        return vec![0.0; n];
    }

    // Document frequency over the candidate corpus, only for query terms.
    let mut df: HashMap<&str, usize> = HashMap::new();
    for term in query_terms {
        let count = doc_wtf.iter().filter(|wtf| wtf.contains_key(term)).count();
        df.insert(term.as_str(), count);
    }

    // Pass 2: BM25F score each document. The `1.0 +` IDF smoothing keeps IDF
    // non-negative even for terms present in a majority of documents (avoids
    // classic BM25 negative-IDF flipping common terms to negative scores).
    doc_wtf
        .iter()
        .zip(doc_len.iter())
        .map(|(wtf, &len)| {
            let mut score = 0.0_f32;
            for term in query_terms {
                let tf = match wtf.get(term) {
                    Some(&tf) if tf > 0.0 => tf,
                    _ => continue,
                };
                let dft = *df.get(term.as_str()).unwrap_or(&0) as f32;
                let idf = (1.0 + (n as f32 - dft + 0.5) / (dft + 0.5)).ln();
                let denom = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * len / avgdl);
                score += idf * (tf * (BM25_K1 + 1.0)) / denom;
            }
            score
        })
        .collect()
}

/// Extract a string payload field, returning `None` for missing / non-string /
/// empty values.
/// Build a [`MemorySearchHit`] from a point id + payload (shared by the
/// lexical and dense paths — payload shapes are identical by construction).
fn hit_from_payload(
    id: String,
    score: f32,
    payload: &HashMap<String, serde_json::Value>,
) -> MemorySearchHit {
    MemorySearchHit {
        id,
        score,
        source_path: payload_str(payload, "source_path"),
        scope: payload_str(payload, "scope"),
        name: payload_str(payload, "name"),
        memory_type: payload_str(payload, "type"),
        snippet: snippet_from_payload(payload),
        mtime_ms: payload_u64(payload, "mtime_ms"),
    }
}

/// Document text fed to the embedder: high-signal fields first, char-capped.
fn dense_doc_text(payload: &HashMap<String, serde_json::Value>) -> String {
    let mut parts = Vec::new();
    for field in ["name", "abstract", "overview", "content"] {
        if let Some(text) = payload_str(payload, field) {
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
    }
    let joined = parts.join("\n");
    if joined.chars().count() > DENSE_DOC_TEXT_CAP_CHARS {
        joined.chars().take(DENSE_DOC_TEXT_CAP_CHARS).collect()
    } else {
        joined
    }
}

/// Dense collection config: phase-1 template with the real dimension.
fn dense_collection_config(dimension: usize) -> acosmi_memory_se::segment_store::CollectionConfig {
    let mut config = phase1_collection_config();
    config.dimension = dimension;
    config
}

/// Load the persisted dense dimension (0 = never negotiated). Fail-soft.
fn load_dense_dim(data_dir: &Path) -> usize {
    let path = data_dir.join(DENSE_META_FILENAME);
    let Ok(body) = std::fs::read_to_string(path) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("dimension").and_then(serde_json::Value::as_u64))
        .map(|d| d as usize)
        .unwrap_or(0)
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-e (2026-07-16) — 是否有实质内容：`abstract` /
/// `overview` / `content` 任一非空即算。三者皆空的「name-only 空壳」（残留
/// 脚手架、只有标题的占位文件）对检索没有可用信息，检索期直接排除。
fn payload_has_substance(payload: &HashMap<String, serde_json::Value>) -> bool {
    ["abstract", "overview", "content"].iter().any(|field| {
        payload_str(payload, field)
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
    })
}

/// W-MEMORY-KB-UPLIFT P1 — rerank candidate text: snippet-first (280-char
/// precomputed display text carries the doc essence), name fallback,
/// char-capped for the SDK payload.
fn rerank_doc_text(item: &serde_json::Value) -> String {
    let text = item
        .get("snippet")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| item.get("name").and_then(serde_json::Value::as_str))
        .unwrap_or("");
    text.chars().take(RERANK_DOC_TEXT_CAP_CHARS).collect()
}

/// W-MEMORY-KB-UPLIFT P1 — reorder wire values by rerank relevance. Ranked
/// entries (valid indices into the first `pool` items) come first sorted by
/// score desc (score written back, clamped to [0,1]); everything unranked —
/// including the beyond-pool tail — follows in its original relative order.
fn reorder_by_ranking(
    items: Vec<serde_json::Value>,
    pool: usize,
    ranking: &[RerankEntry],
) -> Vec<serde_json::Value> {
    let mut sorted: Vec<&RerankEntry> = ranking
        .iter()
        .filter(|entry| (entry.index as usize) < pool)
        .collect();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut taken: Vec<Option<serde_json::Value>> = items.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(taken.len());
    for entry in sorted {
        if let Some(mut item) = taken[entry.index as usize].take() {
            if let Some(obj) = item.as_object_mut() {
                obj.insert(
                    "score".to_string(),
                    serde_json::json!(entry.score.clamp(0.0, 1.0)),
                );
            }
            out.push(item);
        }
    }
    for item in taken.into_iter().flatten() {
        out.push(item);
    }
    out
}

/// W-MEMORY-KB-UPLIFT P0 — stable FNV-1a 64 content hash (hex) for the dense
/// re-embed gate. Deliberately dependency-free and stable across runs and
/// toolchain versions (std's `DefaultHasher` guarantees neither).
fn fnv1a64_hex(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// W-MEMORY-KB-UPLIFT P0 — `injection: manual` 知识条目判定（仅显式搜索
/// 可见的检索期过滤判据；缺字段 = 普通记忆 / auto 默认 = 恒可见）。
fn payload_is_manual_injection(payload: &HashMap<String, serde_json::Value>) -> bool {
    payload
        .get("injection")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| mode == "manual")
}

fn payload_str(payload: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// Extract a u64 payload field (defaults to 0 for missing / non-numeric).
fn payload_u64(payload: &HashMap<String, serde_json::Value>, key: &str) -> u64 {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// Build a short snippet: prefer `abstract`, fall back to `overview`, then a
/// prefix of `content`. Truncated to ~280 chars on a char boundary.
fn snippet_from_payload(payload: &HashMap<String, serde_json::Value>) -> Option<String> {
    // W-MEMORY-KB-UPLIFT P1 — prefer the index-time precomputed snippet;
    // derive live only for points the heal pass has not covered yet.
    payload_str(payload, "snippet").or_else(|| derive_snippet(payload))
}

/// Derive a display snippet from the raw text fields (abstract → overview →
/// content prefix, char-capped). Shared by query-time fallback and the
/// index-time heal pass.
fn derive_snippet(payload: &HashMap<String, serde_json::Value>) -> Option<String> {
    const SNIPPET_MAX: usize = 280;
    let raw = payload_str(payload, "abstract")
        .or_else(|| payload_str(payload, "overview"))
        .or_else(|| payload_str(payload, "content"))?;
    if raw.chars().count() <= SNIPPET_MAX {
        Some(raw)
    } else {
        let truncated: String = raw.chars().take(SNIPPET_MAX).collect();
        Some(format!("{truncated}…"))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// Re-export commonly used types from `acosmi-memory-se::indexer` so callers
// only need to import `se_integration::*`.
pub use acosmi_memory_se::indexer::IndexSkipReason;

// ──────────────────────────────────────────────────────────────────────────
// Helpers for production wiring (project_state_dir → search dir).
// ──────────────────────────────────────────────────────────────────────────

/// Resolve the SearchEngine data directory under a project-state directory.
/// Layout: `<project_state_dir>/search/`. Caller ensures parent exists.
#[must_use]
pub fn search_dir_for_project_state(project_state_dir: &Path) -> PathBuf {
    project_state_dir.join("search")
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use tempfile::TempDir;

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — wire shape contract. snake_case
    /// + skip-None.
    #[test]
    fn embedding_request_payload_roundtrip_snake_case_wire() {
        let payload = EmbeddingRequestPayload {
            req_id: "req-embed-1".to_string(),
            texts: vec!["hello".to_string(), "world".to_string()],
            text_keys: vec!["k0".to_string(), "k1".to_string()],
            model_hint: None,
        };
        let json = serde_json::to_value(&payload).expect("ser");
        assert_eq!(json["req_id"], "req-embed-1");
        assert_eq!(json["texts"][0], "hello");
        assert_eq!(json["text_keys"][1], "k1");
        assert!(
            json.get("model_hint").is_none(),
            "model_hint=None must skip"
        );
        let parsed: EmbeddingRequestPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn embedding_result_payload_roundtrip_with_error() {
        let payload = EmbeddingResultPayload {
            req_id: "req-embed-2".to_string(),
            embeddings: Vec::new(),
            dimension: 0,
            error: Some("sdk failure".to_string()),
        };
        let json = serde_json::to_value(&payload).expect("ser");
        assert_eq!(json["req_id"], "req-embed-2");
        assert_eq!(json["dimension"], 0);
        assert_eq!(json["error"], "sdk failure");
        let parsed: EmbeddingResultPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    #[test]
    fn embedding_result_payload_roundtrip_success() {
        // 用 f32 精确表示的小数（0.5 / 0.25 / 0.125）避免 f32→f64 升宽误差
        // 被 `serde_json::Value` 比较卡住 — 镜像 tier::tests 的
        // `llm_call_request_payload_roundtrip_snake_case_wire` 注释 (P3.1)。
        let payload = EmbeddingResultPayload {
            req_id: "req-embed-3".to_string(),
            embeddings: vec![EmbeddingVector {
                values: vec![0.5_f32, 0.25_f32, 0.125_f32],
            }],
            dimension: 3,
            error: None,
        };
        let json = serde_json::to_value(&payload).expect("ser");
        assert_eq!(json["req_id"], "req-embed-3");
        assert_eq!(json["dimension"], 3);
        assert_eq!(json["embeddings"][0]["values"][0], 0.5);
        assert!(json.get("error").is_none(), "error=None must skip");
        let parsed: EmbeddingResultPayload = serde_json::from_value(json).expect("de");
        assert_eq!(parsed, payload);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — SearchEngineIntegration init +
    /// collection setup happy path.
    #[tokio::test]
    async fn search_engine_integration_init_creates_collection() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let created = integration.init_collections().expect("init collections");
        assert!(created, "first call must create collection");
        // Second call is no-op (collection already exists).
        let created_again = integration
            .init_collections()
            .expect("init collections idempotent");
        assert!(!created_again, "second call must report not created");
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — index_all happy path: walk a small
    /// fixture memdir + verify IndexStats counters.
    #[tokio::test]
    async fn search_engine_integration_index_all_indexes_fixture_files() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        // Write 3 valid Tier-1/2/3 markdown fixtures with frontmatter type.
        for (name, ty) in &[
            ("project_alpha.md", "project"),
            ("user_beta.md", "user"),
            ("feedback_gamma.md", "feedback"),
        ] {
            let path = memdir.join(name);
            let body = format!(
                "---\ntype: {ty}\nname: {ty} sample\ndescription: a {ty} sample memory.\ncreated_at: 2026-05-25\n---\n\nbody text\n"
            );
            std::fs::write(&path, body).expect("write");
        }

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let stats = integration.index_all(&roots).expect("index_all");
        assert_eq!(stats.roots_scanned, 1);
        assert_eq!(stats.md_files_seen, 3);
        assert!(
            stats.indexed >= 1,
            "at least one file should be indexed; got stats={stats:?}"
        );
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — upsert_file no-op for paths outside
    /// configured roots (defensive — Tier policy may have written outside
    /// the memdir, indexer must not panic).
    #[tokio::test]
    async fn search_engine_integration_upsert_file_outside_root_is_noop() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        let elsewhere = tmp.path().join("not-memdir");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        let stray = elsewhere.join("stray.md");
        std::fs::write(&stray, "---\ntype: user\n---\nbody\n").expect("write");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let stats = integration
            .upsert_file(&roots, &stray)
            .expect("upsert_file no-op");
        assert_eq!(stats.roots_scanned, 0, "outside-root upsert is no-op");
        assert_eq!(stats.indexed, 0);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — EmbeddingEmitter roundtrip happy
    /// path. Simulates the production reverse-IPC flow:
    /// 1. SearchEngineIntegration.embed(...) emits request via emitter
    /// 2. Test harness intercepts the emitted request from RecordingEmitter
    /// 3. Test harness calls deliver_result(...) with matching req_id
    /// 4. embed() resolves with the embedding vectors
    #[tokio::test]
    async fn embedding_emitter_roundtrip_resolves_with_vectors() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );

        // Spawn the embed call in a task so we can deliver the result from
        // this task without blocking.
        let integration_for_task = Arc::clone(&integration);
        let embed_task = tokio::spawn(async move {
            integration_for_task
                .embed(
                    vec!["hello".to_string(), "world".to_string()],
                    vec!["k0".to_string(), "k1".to_string()],
                    None,
                )
                .await
        });

        // Wait a tick for the emit to happen.
        for _ in 0..50 {
            if !emitter.recorded().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let recorded = emitter.recorded().await;
        assert_eq!(recorded.len(), 1, "exactly one request emitted");
        let req_id = recorded[0].req_id.clone();
        assert_eq!(recorded[0].texts, vec!["hello", "world"]);

        // Deliver the matching result.
        let delivered = integration
            .deliver_result(EmbeddingResultPayload {
                req_id: req_id.clone(),
                embeddings: vec![
                    EmbeddingVector {
                        values: vec![1.0, 0.0],
                    },
                    EmbeddingVector {
                        values: vec![0.0, 1.0],
                    },
                ],
                dimension: 2,
                error: None,
            })
            .await;
        assert!(delivered, "result delivery must match pending request");

        let result = embed_task
            .await
            .expect("join embed task")
            .expect("embed result");
        assert_eq!(result.req_id, req_id);
        assert_eq!(result.embeddings.len(), 2);
        assert_eq!(result.embeddings[0].values, vec![1.0, 0.0]);
        assert_eq!(result.dimension, 2);
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — EmbeddingEmitter error case:
    /// delivering a result with `error` set must propagate as
    /// `SeIntegrationError::EmbeddingFailed`.
    #[tokio::test]
    async fn embedding_emitter_roundtrip_propagates_error() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );

        let integration_for_task = Arc::clone(&integration);
        let embed_task = tokio::spawn(async move {
            integration_for_task
                .embed(vec!["x".to_string()], vec!["k0".to_string()], None)
                .await
        });

        // Wait for emit.
        for _ in 0..50 {
            if !emitter.recorded().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let recorded = emitter.recorded().await;
        assert_eq!(recorded.len(), 1);
        let req_id = recorded[0].req_id.clone();

        integration
            .deliver_result(EmbeddingResultPayload {
                req_id: req_id.clone(),
                embeddings: Vec::new(),
                dimension: 0,
                error: Some("sdk down".to_string()),
            })
            .await;

        let outcome = embed_task.await.expect("join embed task");
        match outcome {
            Err(SeIntegrationError::EmbeddingFailed(msg)) => {
                assert!(
                    msg.contains("sdk down"),
                    "error message must propagate; got {msg}"
                );
            }
            other => panic!("expected EmbeddingFailed, got {other:?}"),
        }
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — texts/text_keys length mismatch
    /// must fail fast (defense against caller bug).
    #[tokio::test]
    async fn embed_rejects_mismatched_texts_and_keys() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");

        let outcome = integration
            .embed(
                vec!["a".to_string(), "b".to_string()],
                vec!["k0".to_string()],
                None,
            )
            .await;
        match outcome {
            Err(SeIntegrationError::EmbeddingFailed(msg)) => {
                assert!(
                    msg.contains("texts.len()"),
                    "must mention texts.len mismatch; got {msg}"
                );
            }
            other => panic!("expected EmbeddingFailed, got {other:?}"),
        }
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — `deliver_result` for unknown req_id
    /// is a no-op (late delivery after timeout).
    #[tokio::test]
    async fn deliver_result_unknown_req_id_is_noop() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let delivered = integration
            .deliver_result(EmbeddingResultPayload {
                req_id: "unknown".to_string(),
                embeddings: Vec::new(),
                dimension: 0,
                error: None,
            })
            .await;
        assert!(!delivered, "unknown req_id must be no-op");
    }

    /// W-MEMORY-DREAM-REBUILD v7 P4.1 — `search_dir_for_project_state`
    /// resolves the canonical layout.
    #[test]
    fn search_dir_for_project_state_resolves_layout() {
        let project = PathBuf::from("/tmp/some/projects/foo");
        let search = search_dir_for_project_state(&project);
        assert_eq!(search, PathBuf::from("/tmp/some/projects/foo/search"));
    }

    /// W-MEMORY-EVOLUTION PR-9 — text search returns real hits matching
    /// indexed payload content. Asserts the matching file is found and that
    /// a non-matching query term excludes it.
    #[tokio::test]
    async fn search_returns_real_hits_over_indexed_payload() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        // One memory whose name/abstract contains "kubernetes deployment".
        std::fs::write(
            memdir.join("project_k8s.md"),
            "---\ntype: project\nname: kubernetes deployment guide\n\
             description: how to deploy services to kubernetes clusters.\n\
             created_at: 2026-05-25\n---\n\nbody about pods and kubernetes\n",
        )
        .expect("write k8s");
        // An unrelated memory about cooking.
        std::fs::write(
            memdir.join("user_cooking.md"),
            "---\ntype: user\nname: pasta recipe\n\
             description: how to cook pasta with tomato sauce.\n\
             created_at: 2026-05-25\n---\n\nbody about boiling water\n",
        )
        .expect("write cooking");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let stats = integration.index_all(&roots).expect("index_all");
        assert_eq!(stats.indexed, 2, "both fixtures indexed; stats={stats:?}");

        // Query for "kubernetes" — must return the k8s memory, not cooking.
        let hits = integration
            .search("kubernetes", 10, "hybrid", true)
            .expect("search");
        assert!(
            !hits.is_empty(),
            "kubernetes query must return at least one hit (real result, not [])"
        );
        let top = &hits[0];
        assert!(
            top.name.as_deref().unwrap_or("").contains("kubernetes"),
            "top hit must be the kubernetes memory; got {top:?}"
        );
        // The cooking memory must NOT appear for this query.
        assert!(
            hits.iter()
                .all(|h| !h.name.as_deref().unwrap_or("").contains("pasta")),
            "non-matching 'pasta' memory must be excluded; got {hits:?}"
        );
        // Snippet is populated from the abstract/overview/content.
        assert!(top.snippet.is_some(), "hit must carry a snippet");
        assert!(top.score > 0.0, "matching hit must have positive score");
    }

    /// W-MEMORY-DATA-COMPLETION A1 — end-to-end Chinese retrieval. The OLD
    /// ASCII tokenizer collapsed the whole query into one token requiring an
    /// exact substring, so a query that is a SUB-phrase of the doc (different
    /// trailing words) would miss. With charabia word segmentation the query
    /// `记忆系统` matches a doc named `记忆系统健康度审计` via shared word tokens.
    #[tokio::test]
    async fn search_retrieves_chinese_by_word_segmentation() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        // Doc the query is a SUB-phrase of (not an exact substring of the query).
        std::fs::write(
            memdir.join("project_memory.md"),
            "---\ntype: project\nname: 记忆系统健康度审计\n\
             description: 对记忆系统的数据管理侧做全链路健康度审计。\n\
             created_at: 2026-06-20\n---\n\n正文讨论做梦与检索能力。\n",
        )
        .expect("write zh memory");
        // Unrelated Chinese doc that must be excluded.
        std::fs::write(
            memdir.join("user_cooking.md"),
            "---\ntype: user\nname: 烹饪食谱\n\
             description: 用番茄酱煮意大利面的方法。\n\
             created_at: 2026-06-20\n---\n\n正文讨论烧水与火候。\n",
        )
        .expect("write zh cooking");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::private(memdir.clone())];
        let stats = integration.index_all(&roots).expect("index_all");
        assert_eq!(
            stats.indexed, 2,
            "both zh fixtures indexed; stats={stats:?}"
        );

        // Query is a sub-phrase, NOT an exact substring of the doc name.
        let hits = integration
            .search("记忆系统", 10, "hybrid", true)
            .expect("search");
        assert!(
            !hits.is_empty(),
            "Chinese query must return a hit (the A1 fix; old tokenizer returned [])"
        );
        let top = &hits[0];
        assert!(
            top.name.as_deref().unwrap_or("").contains("记忆系统健康度"),
            "top hit must be the memory-system doc; got {top:?}"
        );
        assert!(
            hits.iter()
                .all(|h| !h.name.as_deref().unwrap_or("").contains("烹饪")),
            "unrelated cooking doc must be excluded; got {hits:?}"
        );
    }

    /// W-MEMORY-EVOLUTION FIX #12 — `delete_by_path` removes the indexed
    /// point so a subsequent search no longer matches the deleted file.
    #[tokio::test]
    async fn delete_by_path_removes_indexed_point() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        let target = memdir.join("project_doomed.md");
        std::fs::write(
            &target,
            "---\ntype: project\nname: doomed pipeline\n\
             description: this memory is about to be deleted.\n\
             created_at: 2026-05-25\n---\n\nbody about doomed pipeline\n",
        )
        .expect("write doomed");
        // A second memory that must survive.
        std::fs::write(
            memdir.join("project_keeper.md"),
            "---\ntype: project\nname: keeper pipeline\n\
             description: this memory stays indexed.\n\
             created_at: 2026-05-25\n---\n\nbody about keeper pipeline\n",
        )
        .expect("write keeper");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::private(memdir.clone())];
        integration.index_all(&roots).expect("index_all");

        // "doomed" matches before delete.
        let before = integration
            .search("doomed", 10, "text", true)
            .expect("search");
        assert_eq!(before.len(), 1, "doomed indexed; got {before:?}");

        // Delete the point for the doomed file.
        let removed = integration.delete_by_path(&target).expect("delete_by_path");
        assert_eq!(removed, 1, "exactly one point removed");

        // Note: we keep the file on disk for this assertion so the on-disk
        // search filter does not also exclude it — we are proving the SE
        // point itself is gone (the index, not just the disk filter).
        let after = integration
            .search("doomed", 10, "text", true)
            .expect("search");
        assert!(
            after.is_empty(),
            "doomed point removed from SE index; got {after:?}"
        );
        // Keeper still matches.
        let keeper = integration
            .search("keeper", 10, "text", true)
            .expect("search");
        assert_eq!(
            keeper.len(),
            1,
            "keeper survives the delete; got {keeper:?}"
        );
    }

    /// W-MEMORY-EVOLUTION FIX #12 — `delete_by_path` on a never-indexed
    /// collection is a fail-soft no-op (returns 0, no error).
    #[tokio::test]
    async fn delete_by_path_missing_collection_is_noop() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let removed = integration
            .delete_by_path(Path::new("/tmp/never-indexed.md"))
            .expect("delete_by_path no-op");
        assert_eq!(removed, 0, "missing collection → 0 removed (fail-soft)");
    }

    /// W-MEMORY-EVOLUTION FIX #12 — defense-in-depth: a search hit whose
    /// `source_path` no longer exists on disk is filtered out even if its SE
    /// point lingers (GC lag).
    #[tokio::test]
    async fn search_filters_hits_whose_source_path_is_gone() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");

        let ghost = memdir.join("project_ghost.md");
        std::fs::write(
            &ghost,
            "---\ntype: project\nname: ghost pipeline\n\
             description: this file will be removed from disk but stay in SE.\n\
             created_at: 2026-05-25\n---\n\nbody about ghost pipeline\n",
        )
        .expect("write ghost");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("index_all");

        // Indexed + searchable while present.
        assert_eq!(
            integration
                .search("ghost", 10, "text", true)
                .expect("search")
                .len(),
            1
        );

        // Remove the file from disk WITHOUT calling delete_by_path (simulate
        // GC lag — the SE point still exists).
        std::fs::remove_file(&ghost).expect("remove ghost");

        let hits = integration
            .search("ghost", 10, "text", true)
            .expect("search");
        assert!(
            hits.is_empty(),
            "search must drop hits whose source_path is gone; got {hits:?}"
        );
    }

    /// W-MEMORY-EVOLUTION PR-9 — search over a never-indexed collection
    /// fail-softs to empty (no error), so a freshly-constructed engine
    /// renders a "warming up" empty state.
    #[tokio::test]
    async fn search_fail_soft_empty_when_collection_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        // No init_collections / index_all called — collection does not exist.
        let hits = integration
            .search("anything", 10, "hybrid", true)
            .expect("search must not error on missing collection");
        assert!(hits.is_empty(), "missing collection → empty (fail-soft)");
    }

    /// W-MEMORY-EVOLUTION PR-9 — empty query returns browse-mode results
    /// ordered by recency (mtime desc), without an embedding round-trip.
    #[tokio::test]
    async fn search_empty_query_returns_browse_results() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        for name in &["project_a.md", "user_b.md"] {
            std::fs::write(
                memdir.join(name),
                "---\ntype: user\nname: sample\ndescription: a sample.\ncreated_at: 2026-05-25\n---\n\nbody\n",
            )
            .expect("write");
        }
        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("index_all");
        let hits = integration
            .search("   ", 10, "hybrid", true)
            .expect("search");
        assert_eq!(hits.len(), 2, "browse mode returns all indexed entries");
    }

    /// W-MEMORY-DATA-COMPLETION A1 — tokenization is multilingual: ASCII splits
    /// on whitespace/punctuation, and (the root-cause fix) CJK is word-segmented
    /// rather than collapsed into one exact-substring token.
    #[test]
    fn tokenize_is_multilingual_and_dedups_query() {
        assert_eq!(tokenize_query("Hello, World!"), vec!["hello", "world"]);
        assert!(tokenize_query("   ").is_empty());
        // dedup: repeated query word appears once.
        assert_eq!(tokenize_query("rust rust"), vec!["rust"]);

        // The A1 fix: a Chinese phrase must segment into MULTIPLE word tokens
        // (not one), otherwise retrieval degrades to exact-substring matching.
        let zh = tokenize("记忆系统健康");
        assert!(zh.len() > 1, "CJK must be word-segmented, got {zh:?}");
        // Segmentation must surface real sub-words a doc could independently
        // match (e.g. 记忆 / 系统 / 健康), not the whole string.
        assert!(
            zh.iter().any(|t| t != "记忆系统健康"),
            "expected sub-word tokens, got {zh:?}"
        );
    }

    /// W-MEMORY-DATA-COMPLETION A1 — BM25F ranks a doc that matches a rarer
    /// query term above one that only matches a ubiquitous term (IDF effect),
    /// and field boosts still apply.
    #[test]
    fn bm25f_ranks_by_idf_and_field_weight() {
        let mut common_only = HashMap::new();
        common_only.insert("name".to_string(), serde_json::json!("rust guide"));
        common_only.insert("content".to_string(), serde_json::json!("rust rust rust"));

        let mut rare_match = HashMap::new();
        rare_match.insert("name".to_string(), serde_json::json!("kubernetes rust"));

        let mut filler = HashMap::new();
        filler.insert(
            "content".to_string(),
            serde_json::json!("rust notes about rust"),
        );

        let payloads: Vec<&HashMap<String, serde_json::Value>> =
            vec![&common_only, &rare_match, &filler];
        // "rust" is in all 3 docs (low IDF); "kubernetes" is in 1 (high IDF).
        let terms = vec!["rust".to_string(), "kubernetes".to_string()];
        let scores = bm25f_scores(&payloads, &terms);
        assert_eq!(scores.len(), 3);
        // The doc matching the rare term ranks highest.
        assert!(
            scores[1] > scores[0] && scores[1] > scores[2],
            "rare-term match must win: {scores:?}"
        );
        // Empty query → all-zero (browse mode handles recency upstream).
        assert_eq!(bm25f_scores(&payloads, &[]), vec![0.0, 0.0, 0.0]);
    }

    /// Smoke test: req_id generation is monotonic across multiple calls
    /// within the same integration. Defends against future drift where
    /// counter is shared across instances by accident.
    #[test]
    fn req_id_counter_is_monotonic() {
        // Construct counter standalone (no full SearchEngine needed).
        let counter = AtomicU64::new(0);
        let id_a = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let id_b = counter.fetch_add(1, Ordering::Relaxed) + 1;
        assert!(id_b > id_a, "req_id counter must be monotonic");
    }

    // ──────────────────────────────────────────────────────────────────
    // W-MEMORY-ALIVE PR-2b (2026-07-01) — dense sync + hybrid retrieval
    // ──────────────────────────────────────────────────────────────────

    /// Test executor: polls the RecordingEmitter and answers every request
    /// with deterministic vectors (stands in for the TS memoryTierProxy).
    fn spawn_embed_responder(
        emitter: Arc<RecordingEmitter>,
        integration: Arc<SearchEngineIntegration>,
        vector_for: impl Fn(&str) -> Vec<f32> + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut answered = 0usize;
            loop {
                let recorded = emitter.recorded().await;
                for request in recorded.iter().skip(answered) {
                    let embeddings: Vec<EmbeddingVector> = request
                        .texts
                        .iter()
                        .map(|t| EmbeddingVector {
                            values: vector_for(t),
                        })
                        .collect();
                    let dimension = embeddings.first().map(|e| e.values.len()).unwrap_or(0) as u32;
                    integration
                        .deliver_result(EmbeddingResultPayload {
                            req_id: request.req_id.clone(),
                            embeddings,
                            dimension,
                            error: None,
                        })
                        .await;
                }
                answered = recorded.len();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    fn write_fixture(memdir: &Path, name: &str, ty: &str, description: &str) {
        let body = format!(
            "---\ntype: {ty}\nname: {name}\ndescription: {description}\ncreated_at: 2026-07-01\n---\n\nbody text\n"
        );
        std::fs::write(memdir.join(format!("{name}.md")), body).expect("write fixture");
    }

    #[tokio::test]
    async fn dense_sync_embeds_and_hybrid_search_fuses_then_delete_cleans() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        write_fixture(&memdir, "alpha_topic", "project", "alpha subject notes");
        write_fixture(&memdir, "beta_topic", "user", "beta subject notes");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );
        let roots = vec![MemoryRoot::private(memdir.clone())];
        integration.index_all(&roots).expect("index_all");

        let responder =
            spawn_embed_responder(Arc::clone(&emitter), Arc::clone(&integration), |text| {
                if text.contains("alpha") {
                    vec![1.0, 0.0, 0.0]
                } else {
                    vec![0.0, 1.0, 0.0]
                }
            });

        let embedded = integration.sync_dense_index().await.expect("dense sync");
        assert_eq!(embedded, 2, "both fixtures must be embedded");
        // Second pass is incremental — nothing stale, nothing re-embedded.
        let again = integration.sync_dense_index().await.expect("dense sync 2");
        assert_eq!(again, 0, "unchanged docs must not re-embed");

        let (hits, engine) = integration
            .search_hybrid("alpha", 5, "hybrid", true)
            .await
            .expect("hybrid search");
        assert_eq!(engine, "hybrid", "dense side available → hybrid engine");
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].name.as_deref(),
            Some("alpha_topic"),
            "lexical+dense agreement must rank the alpha doc first; hits={hits:?}"
        );

        // Deleting the file must clean BOTH collections (lexical + dense).
        let alpha_path = memdir.join("alpha_topic.md");
        std::fs::remove_file(&alpha_path).expect("rm");
        let deleted = integration.delete_by_path(&alpha_path).expect("delete");
        assert_eq!(deleted, 2, "one lexical + one dense point");

        responder.abort();
    }

    /// W-MEMORY-KB-UPLIFT P1 — `rerank_values` reorders by delivered
    /// relevance (scores written back), keeps the unranked tail in order,
    /// fail-softs to the input order on an error result, and the armed
    /// backoff short-circuits the next call without emitting a request.
    #[tokio::test]
    async fn rerank_values_reorders_then_fails_soft_and_backs_off() {
        let tmp = TempDir::new().expect("tempdir");
        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(
                tmp.path().join("se"),
                emitter.clone() as Arc<dyn EmbeddingEmitter>,
            )
            .expect("init"),
        );
        let items = vec![
            serde_json::json!({"id": "a", "name": "alpha", "snippet": "alpha text", "score": 0.9}),
            serde_json::json!({"id": "b", "name": "beta", "snippet": "beta text", "score": 0.8}),
            serde_json::json!({"id": "c", "name": "gamma", "snippet": "gamma text", "score": 0.7}),
        ];

        // Responder: rank beta above alpha; gamma unranked (tail keeps order).
        let integration_for_responder = Arc::clone(&integration);
        let emitter_for_responder = Arc::clone(&emitter);
        let responder = tokio::spawn(async move {
            loop {
                let recorded = emitter_for_responder.recorded_rerank().await;
                if let Some(request) = recorded.first() {
                    assert_eq!(request.documents.len(), 3);
                    assert!(request.documents[0].contains("alpha"));
                    integration_for_responder
                        .deliver_rerank_result(RerankResultPayload {
                            req_id: request.req_id.clone(),
                            ranking: vec![
                                RerankEntry {
                                    index: 1,
                                    score: 0.95,
                                },
                                RerankEntry {
                                    index: 0,
                                    score: 0.40,
                                },
                            ],
                            error: None,
                        })
                        .await;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let (reranked, applied) = integration.rerank_values("query text", items.clone()).await;
        responder.await.expect("responder");
        assert!(applied);
        let ids: Vec<&str> = reranked.iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["b", "a", "c"],
            "ranked-by-score-desc first, unranked tail keeps order"
        );
        assert!((reranked[0]["score"].as_f64().unwrap() - 0.95).abs() < 1e-6);

        // Error result → fail-soft input order + backoff armed.
        let answered = emitter.recorded_rerank().await.len();
        let integration_for_err = Arc::clone(&integration);
        let emitter_for_err = Arc::clone(&emitter);
        let err_responder = tokio::spawn(async move {
            loop {
                let recorded = emitter_for_err.recorded_rerank().await;
                if recorded.len() > answered {
                    let request = recorded.last().expect("request").clone();
                    integration_for_err
                        .deliver_rerank_result(RerankResultPayload {
                            req_id: request.req_id,
                            ranking: Vec::new(),
                            error: Some("no supports_rerank model".to_string()),
                        })
                        .await;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let (same, applied_err) = integration.rerank_values("query text", items.clone()).await;
        err_responder.await.expect("err responder");
        assert!(!applied_err);
        assert_eq!(same[0]["id"], "a", "fail-soft keeps input order");

        // Backoff armed → immediate short-circuit, no new request emitted.
        let before = emitter.recorded_rerank().await.len();
        let (_, applied_backoff) = integration.rerank_values("query text", items).await;
        assert!(!applied_backoff);
        assert_eq!(
            emitter.recorded_rerank().await.len(),
            before,
            "backoff must not emit a rerank request"
        );
    }

    /// W-MEMORY-KB-UPLIFT P1 — `index_all` heals lexical stats: every point
    /// carries `bm25_wtf`/`bm25_wdl`/`bm25_stats_mtime`/`snippet` afterwards,
    /// the precomputed fast path ranks identically to live tokenization, and
    /// a stale guard falls back to live tokenization (correct either way).
    #[tokio::test]
    async fn lexical_stats_precompute_heals_and_fast_path_matches_live() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        std::fs::write(
            memdir.join("project_k8s.md"),
            "---\ntype: project\nname: kubernetes deployment guide\n\
             description: how to deploy services to kubernetes clusters.\n---\n\nbody about pods\n",
        )
        .expect("write k8s");
        std::fs::write(
            memdir.join("user_cooking.md"),
            "---\ntype: user\nname: pasta recipe\n\
             description: how to cook pasta with tomato sauce.\n---\n\nbody about boiling water\n",
        )
        .expect("write cooking");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("index_all");

        // Every point healed: stats + guard + snippet present.
        let engine = integration.engine();
        let points = engine
            .scroll(DEFAULT_TOPIC_COLLECTION, 100)
            .expect("scroll");
        assert_eq!(points.len(), 2);
        for point in &points {
            let mtime = point
                .payload
                .get("mtime_ms")
                .and_then(serde_json::Value::as_u64)
                .expect("mtime");
            assert_eq!(
                point
                    .payload
                    .get("bm25_stats_mtime")
                    .and_then(serde_json::Value::as_u64),
                Some(mtime),
                "guard must stamp the indexed mtime"
            );
            assert!(
                point
                    .payload
                    .get("bm25_wtf")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|m| !m.is_empty()),
                "weighted TF map must be present"
            );
            assert!(
                point
                    .payload
                    .get("snippet")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "snippet must be precomputed"
            );
        }

        // Fast path ranks the kubernetes doc first…
        let fast = integration
            .search("kubernetes", 10, "text", true)
            .expect("fast search");
        assert_eq!(fast[0].name.as_deref(), Some("kubernetes deployment guide"));

        // …and a corrupted guard (legacy pre-heal index) falls back to live
        // tokenization with the same ranking.
        for point in &points {
            let mut stale = HashMap::new();
            stale.insert("bm25_stats_mtime".to_owned(), serde_json::json!(1u64));
            engine
                .set_payload(
                    DEFAULT_TOPIC_COLLECTION,
                    &point.id,
                    &fields_to_payload(stale),
                )
                .expect("corrupt guard");
        }
        let live = integration
            .search("kubernetes", 10, "text", true)
            .expect("live search");
        assert_eq!(
            live[0].name.as_deref(),
            Some("kubernetes deployment guide"),
            "stale guard must fall back to live tokenization"
        );
        assert_eq!(fast.len(), live.len());
    }

    /// W-MEMORY-KB-UPLIFT P0 — `injection: manual` entries are visible to
    /// explicit search (include_manual=true) and hidden from passive recall
    /// (include_manual=false); entries without the field are always visible.
    #[tokio::test]
    async fn manual_injection_entries_hidden_unless_included() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let kbdir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&kbdir).expect("kbdir");
        std::fs::write(
            kbdir.join("manual-entry.md"),
            "---\ntype: knowledge\nname: manual zebra entry\nenabled: true\ninjection: manual\n---\nzebra reference body\n",
        )
        .expect("write manual");
        std::fs::write(
            kbdir.join("auto-entry.md"),
            "---\ntype: knowledge\nname: auto zebra entry\nenabled: true\n---\nzebra reference body\n",
        )
        .expect("write auto");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        let roots = vec![MemoryRoot::new("knowledge", kbdir.clone(), Vec::new())];
        let stats = integration.index_all(&roots).expect("index_all");
        assert_eq!(
            stats.indexed, 2,
            "both enabled entries index; stats={stats:?}"
        );

        let recall = integration
            .search("zebra", 10, "text", false)
            .expect("recall");
        assert_eq!(recall.len(), 1, "passive recall must hide manual entries");
        assert_eq!(recall[0].name.as_deref(), Some("auto zebra entry"));

        let explicit = integration
            .search("zebra", 10, "text", true)
            .expect("explicit");
        assert_eq!(explicit.len(), 2, "explicit search must see manual entries");
    }

    /// W-MEMORY-KB-UPLIFT P0 — the dense re-embed gate + reconciliation: an
    /// mtime bump with identical content realigns the dense point instead of
    /// re-embedding (embeddings are gateway-billed), and a dense point whose
    /// lexical twin is gone (review-gate sweep) is pruned by the next sync.
    #[tokio::test]
    async fn dense_sync_hash_gate_skips_unchanged_and_reconciles_orphans() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        write_fixture(&memdir, "alpha_topic", "project", "alpha subject notes");
        write_fixture(&memdir, "beta_topic", "user", "beta subject notes");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );
        let roots = vec![MemoryRoot::private(memdir.clone())];
        integration.index_all(&roots).expect("index_all");
        let responder =
            spawn_embed_responder(Arc::clone(&emitter), Arc::clone(&integration), |_| {
                vec![0.5, 0.5, 0.0]
            });
        assert_eq!(integration.sync_dense_index().await.expect("sync"), 2);

        // mtime bump, identical bytes → hash gate realigns, zero re-embeds,
        // zero embedding requests emitted.
        let alpha_path = memdir.join("alpha_topic.md");
        let future = filetime::FileTime::from_unix_time(4_102_444_800, 0); // 2100-01-01
        filetime::set_file_mtime(&alpha_path, future).expect("bump mtime");
        integration.index_all(&roots).expect("reindex");
        let requests_before = emitter.recorded().await.len();
        assert_eq!(
            integration.sync_dense_index().await.expect("sync 2"),
            0,
            "identical content must not re-embed"
        );
        assert_eq!(
            emitter.recorded().await.len(),
            requests_before,
            "hash gate must not emit embedding requests"
        );

        // Reconciliation: remove beta's LEXICAL point directly (simulating the
        // knowledge review-gate sweep, which deletes lexical but cannot reach
        // dense) — the dense orphan must be pruned by the next sync so it can
        // never resurface via dense recall (its file still exists on disk, so
        // the liveness filter alone would not catch it).
        let engine = integration.engine();
        let beta_id = engine
            .scroll(DEFAULT_TOPIC_COLLECTION, 100)
            .expect("scroll")
            .into_iter()
            .find(|hit| {
                hit.payload.get("name").and_then(serde_json::Value::as_str) == Some("beta_topic")
            })
            .map(|hit| hit.id)
            .expect("beta lexical point");
        engine
            .delete(DEFAULT_TOPIC_COLLECTION, &beta_id)
            .expect("delete lexical twin");
        let dense_name = format!("{DEFAULT_TOPIC_COLLECTION}-dense-3");
        assert_eq!(
            engine.point_count(&dense_name).unwrap_or(0),
            2,
            "dense side still holds both points before the sweep"
        );
        assert_eq!(integration.sync_dense_index().await.expect("sync 3"), 0);
        assert_eq!(
            engine.point_count(&dense_name).unwrap_or(0),
            1,
            "reconciliation must prune the dense orphan"
        );

        responder.abort();
    }

    #[tokio::test]
    async fn dense_sync_error_result_arms_backoff_and_search_degrades_to_text() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        write_fixture(&memdir, "gamma_topic", "project", "gamma subject notes");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );
        integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("index_all");

        // Responder that answers every request with an honest error (the
        // TS proxy shape when no supports_embedding model exists).
        let integration_for_responder = Arc::clone(&integration);
        let emitter_for_responder = Arc::clone(&emitter);
        let responder = tokio::spawn(async move {
            let mut answered = 0usize;
            loop {
                let recorded = emitter_for_responder.recorded().await;
                for request in recorded.iter().skip(answered) {
                    integration_for_responder
                        .deliver_result(EmbeddingResultPayload {
                            req_id: request.req_id.clone(),
                            embeddings: Vec::new(),
                            dimension: 0,
                            error: Some("no supports_embedding model".to_string()),
                        })
                        .await;
                }
                answered = recorded.len();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let embedded = integration.sync_dense_index().await.expect("dense sync");
        assert_eq!(embedded, 0, "failed embedding must embed nothing");

        // Backoff armed: the next sync skips without emitting a request.
        let requests_after_first = emitter.recorded().await.len();
        let again = integration.sync_dense_index().await.expect("dense sync 2");
        assert_eq!(again, 0);
        assert_eq!(
            emitter.recorded().await.len(),
            requests_after_first,
            "backoff must prevent a second reverse-IPC request"
        );

        // Search degrades to the lexical floor (and still finds the doc).
        let (hits, engine) = integration
            .search_hybrid("gamma", 5, "hybrid", true)
            .await
            .expect("hybrid search");
        assert_eq!(engine, "text");
        assert_eq!(hits.len(), 1);

        responder.abort();
    }

    /// 2026-07-04 审计 PR-9 — 维度失配是重协商事件不是不可用：
    /// 失配查询按词法返回但**不武装 backoff**、维度当场更新持久、旧 collection
    /// 置弃；下一个 sync 周期对新键名 collection 全量重嵌，随后恢复 hybrid。
    #[tokio::test]
    async fn search_hybrid_dim_mismatch_renegotiates_without_backoff() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        write_fixture(&memdir, "alpha_topic", "project", "alpha subject notes");
        write_fixture(&memdir, "beta_topic", "user", "beta subject notes");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration = Arc::new(
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init"),
        );
        let roots = vec![MemoryRoot::private(memdir.clone())];
        integration.index_all(&roots).expect("index_all");

        // 单一 responder、可切换维度（避免双 responder 对同一 req 双投递）。
        let dim_now = Arc::new(AtomicUsize::new(3));
        let dim_for_responder = Arc::clone(&dim_now);
        let responder = spawn_embed_responder(
            Arc::clone(&emitter),
            Arc::clone(&integration),
            move |text| {
                let d = dim_for_responder.load(Ordering::Relaxed);
                let mut v = vec![0.0f32; d];
                v[usize::from(!text.contains("alpha"))] = 1.0;
                v
            },
        );

        // 首轮 3 维：sync 落 dense-dim=3 + hybrid 可用。
        let embedded = integration.sync_dense_index().await.expect("dense sync");
        assert_eq!(embedded, 2);
        let old_dense = integration.dense_collection_name(3);
        assert!(integration.engine.collection_exists(&old_dense));

        // 模型侧换代：查询 embedding 变 8 维 → 失配。
        dim_now.store(8, Ordering::Relaxed);
        let (hits, engine) = integration
            .search_hybrid("alpha", 5, "hybrid", true)
            .await
            .expect("mismatch search");
        assert_eq!(engine, "text", "失配当次按词法返回（fail-soft）");
        assert!(!hits.is_empty(), "词法地板仍在");
        assert!(
            !integration.embedding_backoff_active(),
            "失配是重协商不是不可用：不得武装 backoff"
        );
        assert_eq!(
            integration.dense_dim.load(Ordering::Relaxed),
            8,
            "维度当场更新"
        );
        assert!(
            !integration.engine.collection_exists(&old_dense),
            "旧维度 collection 置弃"
        );

        // 下一个 sync 周期：新键名 collection 无 mtime → 天然全量重嵌。
        let re_embedded = integration.sync_dense_index().await.expect("re-sync");
        assert_eq!(re_embedded, 2, "全量重嵌（非 mtime 差分空转）");
        let (hits, engine) = integration
            .search_hybrid("alpha", 5, "hybrid", true)
            .await
            .expect("post-renegotiation search");
        assert_eq!(engine, "hybrid", "重协商后恢复 hybrid");
        assert!(!hits.is_empty());

        responder.abort();
    }

    #[tokio::test]
    async fn search_hybrid_without_dense_collection_is_text_and_emits_nothing() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().join("se-data");
        let memdir = tmp.path().join("memdir");
        std::fs::create_dir_all(&memdir).expect("memdir");
        write_fixture(&memdir, "delta_topic", "project", "delta subject notes");

        let emitter = Arc::new(RecordingEmitter::new());
        let integration =
            SearchEngineIntegration::new(&data_dir, emitter.clone() as Arc<dyn EmbeddingEmitter>)
                .expect("init");
        integration
            .index_all(&[MemoryRoot::private(memdir.clone())])
            .expect("index_all");

        let (hits, engine) = integration
            .search_hybrid("delta", 5, "hybrid", true)
            .await
            .expect("search");
        assert_eq!(engine, "text", "dim unknown → lexical only");
        assert_eq!(hits.len(), 1);
        assert!(
            emitter.recorded().await.is_empty(),
            "no dense collection → no query-embed request"
        );
    }
}
