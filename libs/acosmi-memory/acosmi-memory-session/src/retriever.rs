// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Hierarchical retriever — recursive tree search with score propagation.
//!
//! Ported from `openviking/retrieve/hierarchical_retriever.py`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use log::{debug, warn};

use acosmi_memory_core::retrieve_types::{
    MatchedContext, QueryResult, RelatedContext, RetrieveContextType, ThinkingTrace,
    TraceEventType, TypedQuery,
};

use crate::traits::{
    validate_embed_result_for_collection, BoxError, Embedder, FileSystem, Reranker, VectorHit,
    VectorStore,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max rounds of stable top-k before stopping.
pub const MAX_CONVERGENCE_ROUNDS: usize = 3;
/// Max related URIs to fetch.
pub const MAX_RELATIONS: usize = 5;
/// Score propagation factor: final = α*child + (1-α)*parent.
pub const SCORE_PROPAGATION_ALPHA: f64 = 0.5;
/// Global-search top-k.
pub const GLOBAL_SEARCH_TOPK: usize = 3;
/// Default number of payload records scanned by fallback search.
pub const DEFAULT_FALLBACK_SCAN_LIMIT: usize = 512;
/// Environment kill switch for vector-enhanced memory search.
pub const VECTOR_SEARCH_DISABLED_ENV: &str = "CRABCODE_MEMORY_VECTOR_SEARCH_DISABLED";
/// Environment feature flag for vector-enhanced memory search.
pub const VECTOR_SEARCH_ENABLED_ENV: &str = "CRABCODE_MEMORY_VECTOR_SEARCH_ENABLED";

// ---------------------------------------------------------------------------
// Public status/config types
// ---------------------------------------------------------------------------

/// Retrieval execution mode used by the latest query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalExecutionMode {
    /// Dense-vector embedding and vector search are active.
    Vector,
    /// Payload/filter scan is active because vector search is unavailable or disabled.
    Fallback,
}

impl RetrievalExecutionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Fallback => "fallback",
        }
    }
}

/// Configuration for vector-enhanced retrieval and its deterministic fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrieverConfig {
    /// Whether dense-vector search may be used.
    pub vector_enabled: bool,
    /// Whether payload/filter fallback may be used when vector search is unavailable.
    pub fallback_enabled: bool,
    /// Maximum payload records to scan in fallback mode.
    pub fallback_scan_limit: usize,
}

impl Default for RetrieverConfig {
    fn default() -> Self {
        Self {
            vector_enabled: vector_search_enabled_from_env(),
            fallback_enabled: true,
            fallback_scan_limit: DEFAULT_FALLBACK_SCAN_LIMIT,
        }
    }
}

impl RetrieverConfig {
    /// Return a config with vector search explicitly enabled or disabled.
    #[must_use]
    pub fn with_vector_enabled(mut self, enabled: bool) -> Self {
        self.vector_enabled = enabled;
        self
    }

    /// Return a config with fallback search explicitly enabled or disabled.
    #[must_use]
    pub fn with_fallback_enabled(mut self, enabled: bool) -> Self {
        self.fallback_enabled = enabled;
        self
    }

    /// Return a config with a specific fallback payload scan cap.
    #[must_use]
    pub fn with_fallback_scan_limit(mut self, limit: usize) -> Self {
        self.fallback_scan_limit = limit;
        self
    }
}

/// Current retriever mode and last fallback reason.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalStatus {
    /// Whether vector search is enabled by config/feature flag.
    pub vector_enabled: bool,
    /// Mode used by the latest retrieval call.
    pub mode: RetrievalExecutionMode,
    /// Whether the latest retrieval used fallback search.
    pub fallback_active: bool,
    /// Human-readable reason for fallback mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

impl RetrievalStatus {
    fn vector(vector_enabled: bool) -> Self {
        Self {
            vector_enabled,
            mode: RetrievalExecutionMode::Vector,
            fallback_active: false,
            fallback_reason: None,
        }
    }

    fn fallback(vector_enabled: bool, reason: impl Into<String>) -> Self {
        Self {
            vector_enabled,
            mode: RetrievalExecutionMode::Fallback,
            fallback_active: true,
            fallback_reason: Some(reason.into()),
        }
    }
}

impl Default for RetrievalStatus {
    fn default() -> Self {
        Self::vector(true)
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Score-ordered URI for the priority queue.
#[derive(Debug, Clone, PartialEq)]
struct ScoredUri {
    score: f64,
    uri: String,
}

impl Eq for ScoredUri {}

impl PartialOrd for ScoredUri {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredUri {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
    }
}

/// Scored candidate during recursive search.
#[derive(Debug, Clone)]
struct ScoredCandidate {
    hit: VectorHit,
    final_score: f64,
}

// ---------------------------------------------------------------------------
// HierarchicalRetriever
// ---------------------------------------------------------------------------

/// Recursive tree-based retrieval with score propagation.
pub struct HierarchicalRetriever<VS: VectorStore, EMB: Embedder, FS: FileSystem, RR: Reranker> {
    vs: VS,
    embedder: EMB,
    fs: FS,
    reranker: Option<RR>,
    threshold: f64,
    config: RetrieverConfig,
    status: Arc<Mutex<RetrievalStatus>>,
}

impl<VS: VectorStore, EMB: Embedder, FS: FileSystem, RR: Reranker>
    HierarchicalRetriever<VS, EMB, FS, RR>
{
    /// Create a new retriever.
    pub fn new(vs: VS, embedder: EMB, fs: FS, threshold: f64) -> Self {
        Self::with_config(vs, embedder, fs, threshold, RetrieverConfig::default())
    }

    /// Create a new retriever with explicit configuration.
    pub fn with_config(
        vs: VS,
        embedder: EMB,
        fs: FS,
        threshold: f64,
        config: RetrieverConfig,
    ) -> Self {
        let status = RetrievalStatus::vector(config.vector_enabled);
        Self {
            vs,
            embedder,
            fs,
            reranker: None,
            threshold,
            config,
            status: Arc::new(Mutex::new(status)),
        }
    }

    /// Create with optional reranker.
    pub fn with_reranker(vs: VS, embedder: EMB, fs: FS, reranker: RR, threshold: f64) -> Self {
        let config = RetrieverConfig::default();
        let status = RetrievalStatus::vector(config.vector_enabled);
        Self {
            vs,
            embedder,
            fs,
            reranker: Some(reranker),
            threshold,
            config,
            status: Arc::new(Mutex::new(status)),
        }
    }

    /// Snapshot the latest retriever status.
    #[must_use]
    pub fn status(&self) -> RetrievalStatus {
        self.status.lock().unwrap().clone()
    }

    /// Return the active retriever configuration.
    #[must_use]
    pub fn config(&self) -> &RetrieverConfig {
        &self.config
    }

    /// Retrieve matching contexts for a typed query.
    pub async fn retrieve(
        &self,
        query: &TypedQuery,
        limit: usize,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<QueryResult, BoxError> {
        let collection = Self::type_to_collection(query.context_type);

        if !self.config.vector_enabled {
            let reason = "vector search disabled by config";
            return self
                .fallback_retrieve(collection, query, limit, metadata_filter, reason)
                .await;
        }

        // Embed query
        let embed_result = match self.embedder.embed(&query.query).await {
            Ok(result) => result,
            Err(e) => {
                let reason = format!("embedder unavailable: {e}");
                return self
                    .fallback_retrieve(collection, query, limit, metadata_filter, reason)
                    .await;
            }
        };
        if let Err(e) = validate_embed_result_for_collection(
            &self.vs,
            collection,
            &self.embedder,
            &embed_result,
        )
        .await
        {
            let reason = format!("embedding invalid for vector search: {e}");
            return self
                .fallback_retrieve(collection, query, limit, metadata_filter, reason)
                .await;
        }
        let query_vector = &embed_result.dense_vector;
        let sparse = embed_result.sparse_vector.as_ref();

        // Global search for starting points
        let global_hits = match self
            .global_vector_search(collection, query_vector, sparse, metadata_filter)
            .await
        {
            Ok(hits) => hits,
            Err(e) => {
                let reason = format!("vector search unavailable: {e}");
                return self
                    .fallback_retrieve(collection, query, limit, metadata_filter, reason)
                    .await;
            }
        };

        let mut starting_points =
            Self::merge_starting_points(&global_hits, &query.target_directories);

        if starting_points.is_empty() {
            // FIX-R2: add root URIs for type as fallback starting points
            let root_uris = Self::get_root_uris_for_type(query.context_type);
            if root_uris.is_empty() {
                let result = QueryResult {
                    query: query.clone(),
                    matched_contexts: Vec::new(),
                    searched_directories: Vec::new(),
                    thinking_trace: Self::mode_trace(
                        RetrievalExecutionMode::Vector,
                        self.config.vector_enabled,
                        None,
                    ),
                };
                self.set_status(RetrievalStatus::vector(self.config.vector_enabled));
                return Ok(result);
            }
            // Use root URIs as starting points with neutral score
            for uri in &root_uris {
                if !starting_points.iter().any(|(u, _)| u == uri) {
                    starting_points.push((uri.clone(), 0.5));
                }
            }
        }

        // Recursive search
        let candidates = match self
            .recursive_search(
                collection,
                query_vector,
                sparse,
                &starting_points,
                limit,
                metadata_filter,
            )
            .await
        {
            Ok(candidates) => candidates,
            Err(e) => {
                let reason = format!("recursive vector search unavailable: {e}");
                return self
                    .fallback_retrieve(collection, query, limit, metadata_filter, reason)
                    .await;
            }
        };

        let mut matches: Vec<MatchedContext> = candidates
            .into_iter()
            .take(limit)
            .map(|c| MatchedContext {
                uri: c.hit.id.clone(),
                context_type: query.context_type,
                is_leaf: c
                    .hit
                    .fields
                    .get("is_leaf")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                abstract_text: c
                    .hit
                    .fields
                    .get("abstract")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                overview: None,
                // FIX-R7: extract category from hit fields
                category: c
                    .hit
                    .fields
                    .get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                score: c.final_score,
                match_reason: String::new(),
                relations: Vec::new(),
            })
            .collect();

        // FIX-R1: Apply reranking if available
        if let Some(ref reranker) = self.reranker {
            let docs: Vec<String> = matches
                .iter()
                .map(|m| format!("{} {}", m.abstract_text, m.uri))
                .collect();
            match reranker.rerank(&query.query, &docs, limit).await {
                Ok(reranked) => {
                    let original = matches.clone();
                    matches.clear();
                    for rr in reranked {
                        if rr.index < original.len() {
                            let mut m = original[rr.index].clone();
                            m.score = rr.score;
                            matches.push(m);
                        }
                    }
                    debug!("Reranked {} results", matches.len());
                }
                Err(e) => {
                    warn!("Rerank failed, using original order: {e}");
                }
            }
        }

        // FIX-R3: Load relations for matched contexts
        self.load_relations(&mut matches).await;

        // FIX-R6: populate searched_directories from starting points
        let searched_dirs: Vec<String> =
            starting_points.iter().map(|(uri, _)| uri.clone()).collect();

        self.set_status(RetrievalStatus::vector(self.config.vector_enabled));
        Ok(QueryResult {
            query: query.clone(),
            matched_contexts: matches,
            searched_directories: searched_dirs,
            thinking_trace: Self::mode_trace(
                RetrievalExecutionMode::Vector,
                self.config.vector_enabled,
                None,
            ),
        })
    }

    // -----------------------------------------------------------------------
    // Global search
    // -----------------------------------------------------------------------

    async fn global_vector_search(
        &self,
        collection: &str,
        query_vector: &[f32],
        sparse: Option<&HashMap<String, f64>>,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<VectorHit>, BoxError> {
        self.vs
            .search(
                collection,
                query_vector,
                sparse,
                GLOBAL_SEARCH_TOPK,
                metadata_filter,
            )
            .await
    }

    // -----------------------------------------------------------------------
    // Starting points
    // -----------------------------------------------------------------------

    fn merge_starting_points(
        global_hits: &[VectorHit],
        target_dirs: &[String],
    ) -> Vec<(String, f64)> {
        let mut map: HashMap<String, f64> = HashMap::new();

        // Add global hits
        for hit in global_hits {
            if !hit.score.is_finite() {
                continue;
            }
            let parent = hit
                .fields
                .get("parent_uri")
                .and_then(|v| v.as_str())
                .unwrap_or(&hit.id);
            let entry = map.entry(parent.to_owned()).or_insert(0.0);
            *entry = entry.max(hit.score);
        }

        // Add target directories
        for dir in target_dirs {
            map.entry(dir.clone()).or_insert(1.0);
        }

        let mut points: Vec<(String, f64)> = map.into_iter().collect();
        points.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        points
    }

    // -----------------------------------------------------------------------
    // Recursive search
    // -----------------------------------------------------------------------

    async fn recursive_search(
        &self,
        collection: &str,
        query_vector: &[f32],
        sparse: Option<&HashMap<String, f64>>,
        starting_points: &[(String, f64)],
        limit: usize,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<ScoredCandidate>, BoxError> {
        let alpha = SCORE_PROPAGATION_ALPHA;

        let mut dir_queue = BinaryHeap::new();
        for (uri, score) in starting_points {
            dir_queue.push(ScoredUri {
                score: *score,
                uri: uri.clone(),
            });
        }

        let mut visited: HashSet<String> = HashSet::new();
        let mut collected: Vec<ScoredCandidate> = Vec::new();
        let mut prev_topk: HashSet<String> = HashSet::new();
        let mut convergence_rounds = 0usize;

        while let Some(current) = dir_queue.pop() {
            if visited.contains(&current.uri) {
                continue;
            }
            visited.insert(current.uri.clone());
            debug!("[RecursiveSearch] Entering URI: {}", current.uri);

            let mut filter = metadata_filter.cloned().unwrap_or_default();
            filter.insert("parent_uri".to_owned(), serde_json::json!(current.uri));

            let pre_filter_limit = (limit * 2).max(20);
            let results = self
                .vs
                .search(
                    collection,
                    query_vector,
                    sparse,
                    pre_filter_limit,
                    Some(&filter),
                )
                .await?;

            if results.is_empty() {
                continue;
            }

            for hit in results {
                let score = hit.score;
                if !score.is_finite() {
                    warn!("[RecursiveSearch] Skipping non-finite score for {}", hit.id);
                    continue;
                }
                let final_score = alpha * score + (1.0 - alpha) * current.score;
                if !final_score.is_finite() {
                    warn!(
                        "[RecursiveSearch] Skipping non-finite final score for {}",
                        hit.id
                    );
                    continue;
                }

                if final_score < self.threshold {
                    debug!(
                        "[RecursiveSearch] {} score {:.4} below threshold {:.4}",
                        hit.id, final_score, self.threshold
                    );
                    continue;
                }

                let uri = hit.id.clone();
                if !collected.iter().any(|c| c.hit.id == uri) {
                    collected.push(ScoredCandidate { hit, final_score });
                }

                let is_leaf = collected
                    .last()
                    .and_then(|c| c.hit.fields.get("is_leaf"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !visited.contains(&uri) {
                    if is_leaf {
                        visited.insert(uri);
                    } else {
                        dir_queue.push(ScoredUri {
                            score: final_score,
                            uri,
                        });
                    }
                }
            }

            // Convergence check
            collected.sort_by(|a, b| {
                b.final_score
                    .partial_cmp(&a.final_score)
                    .unwrap_or(Ordering::Equal)
            });
            let current_topk: HashSet<String> = collected
                .iter()
                .take(limit)
                .map(|c| c.hit.id.clone())
                .collect();

            if current_topk == prev_topk && current_topk.len() >= limit {
                convergence_rounds += 1;
                if convergence_rounds >= MAX_CONVERGENCE_ROUNDS {
                    break;
                }
            } else {
                convergence_rounds = 0;
                prev_topk = current_topk;
            }
        }

        collected.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(Ordering::Equal)
        });
        collected.truncate(limit);
        Ok(collected)
    }

    // -----------------------------------------------------------------------
    // Fallback search
    // -----------------------------------------------------------------------

    async fn fallback_retrieve(
        &self,
        collection: &str,
        query: &TypedQuery,
        limit: usize,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
        reason: impl Into<String>,
    ) -> Result<QueryResult, BoxError> {
        let reason = reason.into();
        if !self.config.fallback_enabled {
            return Err(
                format!("vector search unavailable and fallback disabled: {reason}").into(),
            );
        }

        warn!("[MemoryRetriever] Falling back to payload search: {reason}");

        let mut matches = if limit == 0 {
            Vec::new()
        } else {
            let records = self
                .fallback_records(collection, query.context_type, limit, metadata_filter)
                .await?;
            self.records_to_matches(query, records, limit)
        };

        self.load_relations(&mut matches).await;
        self.set_status(RetrievalStatus::fallback(
            self.config.vector_enabled,
            reason.clone(),
        ));

        Ok(QueryResult {
            query: query.clone(),
            matched_contexts: matches,
            searched_directories: query.target_directories.clone(),
            thinking_trace: Self::mode_trace(
                RetrievalExecutionMode::Fallback,
                self.config.vector_enabled,
                Some(&reason),
            ),
        })
    }

    async fn fallback_records(
        &self,
        collection: &str,
        context_type: RetrieveContextType,
        limit: usize,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        let filter = Self::fallback_filter(context_type, metadata_filter);
        let scan_limit = self.config.fallback_scan_limit.max(limit).max(1);

        match self
            .vs
            .filter_query(collection, &filter, scan_limit, 0, None, None, false)
            .await
        {
            Ok(records) => Ok(records),
            Err(e) if is_not_implemented_error(&*e) => {
                let page = self
                    .vs
                    .scroll(collection, Some(&filter), scan_limit, None, None)
                    .await?;
                Ok(page.records)
            }
            Err(e) => Err(format!("fallback payload search failed: {e}").into()),
        }
    }

    fn records_to_matches(
        &self,
        query: &TypedQuery,
        records: Vec<HashMap<String, serde_json::Value>>,
        limit: usize,
    ) -> Vec<MatchedContext> {
        let mut scored: Vec<(f64, String, MatchedContext)> = records
            .into_iter()
            .filter(|record| Self::matches_target_directories(record, &query.target_directories))
            .map(|record| {
                let uri = record_uri(&record);
                let score = deterministic_payload_score(&query.query, &record);
                let matched = Self::record_to_matched_context(query.context_type, record, score);
                (score, uri, matched)
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, matched)| matched)
            .collect()
    }

    fn fallback_filter(
        context_type: RetrieveContextType,
        metadata_filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> HashMap<String, serde_json::Value> {
        let mut filter = metadata_filter.cloned().unwrap_or_default();
        if !is_structured_filter(&filter) {
            filter
                .entry("context_type".to_owned())
                .or_insert_with(|| serde_json::json!(context_type_value(context_type)));
        }
        filter
    }

    fn record_to_matched_context(
        context_type: RetrieveContextType,
        record: HashMap<String, serde_json::Value>,
        score: f64,
    ) -> MatchedContext {
        let uri = record_uri(&record);
        MatchedContext {
            uri,
            context_type,
            is_leaf: record
                .get("is_leaf")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            abstract_text: record
                .get("abstract")
                .or_else(|| record.get("overview"))
                .or_else(|| record.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
            overview: record
                .get("overview")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            category: record
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
            score,
            match_reason: "fallback_payload_search".to_owned(),
            relations: Vec::new(),
        }
    }

    fn matches_target_directories(
        record: &HashMap<String, serde_json::Value>,
        target_directories: &[String],
    ) -> bool {
        if target_directories.is_empty() {
            return true;
        }

        let uri = record_uri(record);
        let parent_uri = record.get("parent_uri").and_then(|v| v.as_str());

        target_directories.iter().any(|dir| {
            let prefix = format!("{}/", dir.trim_end_matches('/'));
            uri == *dir
                || uri.starts_with(&prefix)
                || parent_uri.is_some_and(|parent| parent == dir || parent.starts_with(&prefix))
        })
    }

    async fn load_relations(&self, matches: &mut [MatchedContext]) {
        for m in matches {
            let relations_uri = format!("{}/.relations", m.uri.trim_end_matches(".md"));
            if let Ok(content) = self.fs.read(&relations_uri).await {
                m.relations = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| RelatedContext {
                        uri: l.trim().to_owned(),
                        abstract_text: String::new(),
                    })
                    .collect();
            }
        }
    }

    fn set_status(&self, status: RetrievalStatus) {
        *self.status.lock().unwrap() = status;
    }

    fn mode_trace(
        mode: RetrievalExecutionMode,
        vector_enabled: bool,
        fallback_reason: Option<&str>,
    ) -> ThinkingTrace {
        let mut data = HashMap::new();
        data.insert("mode".to_owned(), serde_json::json!(mode.as_str()));
        data.insert(
            "vector_enabled".to_owned(),
            serde_json::json!(vector_enabled),
        );
        data.insert(
            "fallback_active".to_owned(),
            serde_json::json!(mode == RetrievalExecutionMode::Fallback),
        );
        if let Some(reason) = fallback_reason {
            data.insert("fallback_reason".to_owned(), serde_json::json!(reason));
        }

        let mut trace = ThinkingTrace::default();
        trace.add_event(
            TraceEventType::SearchSummary,
            0.0,
            format!("memory search mode: {}", mode.as_str()),
            data,
            None,
        );
        trace
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Map context type to vector collection name.
    fn type_to_collection(ct: RetrieveContextType) -> &'static str {
        match ct {
            RetrieveContextType::Memory => "context",
            RetrieveContextType::Skill => "context",
            RetrieveContextType::Resource => "context",
        }
    }

    /// FIX-R2: Return root URIs for a given context type.
    fn get_root_uris_for_type(ct: RetrieveContextType) -> Vec<String> {
        match ct {
            RetrieveContextType::Memory => vec![
                "viking://memories".to_owned(),
                "viking://memories/profile.md".to_owned(),
            ],
            RetrieveContextType::Resource => vec!["viking://resources".to_owned()],
            RetrieveContextType::Skill => vec!["viking://skills".to_owned()],
        }
    }
}

fn vector_search_enabled_from_env() -> bool {
    if env_flag(VECTOR_SEARCH_DISABLED_ENV).unwrap_or(false) {
        return false;
    }
    env_flag(VECTOR_SEARCH_ENABLED_ENV).unwrap_or(true)
}

fn env_flag(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn is_not_implemented_error(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    error.to_string().contains("not implemented")
}

fn is_structured_filter(filter: &HashMap<String, serde_json::Value>) -> bool {
    filter
        .keys()
        .any(|key| matches!(key.as_str(), "must" | "must_not" | "should" | "min_should"))
}

fn context_type_value(context_type: RetrieveContextType) -> &'static str {
    match context_type {
        RetrieveContextType::Memory => "memory",
        RetrieveContextType::Resource => "resource",
        RetrieveContextType::Skill => "skill",
    }
}

fn record_uri(record: &HashMap<String, serde_json::Value>) -> String {
    record
        .get("uri")
        .or_else(|| record.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

fn deterministic_payload_score(query: &str, record: &HashMap<String, serde_json::Value>) -> f64 {
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return 0.0;
    }

    let text = searchable_record_text(record);
    let text_tokens = tokenize(&text);
    let overlap = query_tokens
        .iter()
        .filter(|token| text_tokens.contains(*token))
        .count();

    let mut score = overlap as f64 / query_tokens.len() as f64;
    let query_lower = query.trim().to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();
    if !query_lower.is_empty() && text_lower.contains(&query_lower) {
        score += 1.0;
    }
    score
}

fn searchable_record_text(record: &HashMap<String, serde_json::Value>) -> String {
    let mut text = String::new();
    for key in [
        "abstract",
        "overview",
        "content",
        "description",
        "name",
        "tags",
        "category",
        "uri",
        "id",
    ] {
        if let Some(value) = record.get(key) {
            append_search_text(value, &mut text);
            text.push(' ');
        }
    }
    text
}

fn append_search_text(value: &serde_json::Value, text: &mut String) {
    match value {
        serde_json::Value::String(value) => text.push_str(value),
        serde_json::Value::Number(value) => text.push_str(&value.to_string()),
        serde_json::Value::Bool(value) => text.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Array(values) => {
            for value in values {
                append_search_text(value, text);
                text.push(' ');
            }
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => {}
    }
}

fn tokenize(text: &str) -> HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_ascii_lowercase();
            (token.len() >= 2).then_some(token)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn merge_starting_points_dedup() {
        let hits = vec![
            VectorHit {
                id: "child1".into(),
                score: 0.9,
                fields: {
                    let mut m = HashMap::new();
                    m.insert("parent_uri".into(), serde_json::json!("dir_a"));
                    m
                },
            },
            VectorHit {
                id: "child2".into(),
                score: 0.8,
                fields: {
                    let mut m = HashMap::new();
                    m.insert("parent_uri".into(), serde_json::json!("dir_a"));
                    m
                },
            },
        ];
        let dirs = vec!["dir_b".to_owned()];
        let pts = HierarchicalRetriever::<MockVs, MockEmb, MockFs, MockRr>::merge_starting_points(
            &hits, &dirs,
        );
        assert_eq!(pts.len(), 2); // dir_a (deduped) + dir_b
        assert!(pts[0].1 >= pts[1].1); // sorted desc
    }

    #[test]
    fn merge_starting_points_skips_non_finite_scores() {
        let hits = vec![VectorHit {
            id: "bad".into(),
            score: f64::NAN,
            fields: HashMap::new(),
        }];

        let pts = HierarchicalRetriever::<MockVs, MockEmb, MockFs, MockRr>::merge_starting_points(
            &hits,
            &[],
        );

        assert!(pts.is_empty());
    }

    #[test]
    fn scored_uri_ordering() {
        let a = ScoredUri {
            score: 0.5,
            uri: "a".into(),
        };
        let b = ScoredUri {
            score: 0.9,
            uri: "b".into(),
        };
        assert!(b > a);
    }

    #[tokio::test]
    async fn retrieve_falls_back_to_payload_search_when_embedder_is_unavailable() {
        let vs = fallback_store().await;
        let retriever: HierarchicalRetriever<_, _, _, MockRr> = HierarchicalRetriever::with_config(
            vs,
            FailingEmb("embed API rate limited"),
            MockFs,
            0.0,
            RetrieverConfig::default().with_fallback_scan_limit(16),
        );
        let query = typed_memory_query("rust testing", &["viking://user/memories/preferences"]);

        let result = retriever.retrieve(&query, 5, None).await.unwrap();

        assert_eq!(result.matched_contexts.len(), 1);
        assert_eq!(
            result.matched_contexts[0].uri,
            "viking://user/memories/preferences/rust.md"
        );
        assert_eq!(
            result.matched_contexts[0].match_reason,
            "fallback_payload_search"
        );

        let status = retriever.status();
        assert_eq!(status.mode, RetrievalExecutionMode::Fallback);
        assert!(status.fallback_active);
        assert!(status
            .fallback_reason
            .as_deref()
            .unwrap_or("")
            .contains("rate limited"));
        assert_eq!(trace_mode(&result), Some("fallback"));
    }

    #[tokio::test]
    async fn retrieve_kill_switch_skips_embedder_and_uses_fallback_status() {
        let vs = fallback_store().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever: HierarchicalRetriever<_, _, _, MockRr> = HierarchicalRetriever::with_config(
            vs,
            CountingEmb {
                calls: Arc::clone(&calls),
            },
            MockFs,
            0.0,
            RetrieverConfig::default().with_vector_enabled(false),
        );
        let query = typed_memory_query("rust testing", &[]);

        let result = retriever.retrieve(&query, 5, None).await.unwrap();

        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(result.matched_contexts.len(), 1);
        let status = retriever.status();
        assert_eq!(status.mode, RetrievalExecutionMode::Fallback);
        assert!(!status.vector_enabled);
        assert_eq!(
            status.fallback_reason.as_deref(),
            Some("vector search disabled by config")
        );
        assert_eq!(trace_mode(&result), Some("fallback"));
    }

    #[tokio::test]
    async fn retrieve_vector_success_reports_vector_mode() {
        let vs = fallback_store().await;
        let retriever: HierarchicalRetriever<_, _, _, MockRr> =
            HierarchicalRetriever::new(vs, StaticEmb, MockFs, 0.0);
        let query = typed_memory_query("rust testing", &["viking://user/memories/preferences"]);

        let result = retriever.retrieve(&query, 5, None).await.unwrap();

        assert_eq!(result.matched_contexts.len(), 1);
        assert_eq!(retriever.status().mode, RetrievalExecutionMode::Vector);
        assert_eq!(trace_mode(&result), Some("vector"));
    }

    async fn fallback_store() -> crate::memory_vector_store::InMemoryVectorStore {
        let vs = crate::memory_vector_store::InMemoryVectorStore::new();
        let schema = crate::collection_schemas::CollectionSchemas::context_collection(3);
        vs.create_collection("context", &schema).await.unwrap();

        vs.upsert(
            "context",
            "viking://user/memories/preferences/rust.md",
            &[1.0, 0.0, 0.0],
            test_fields(&[
                (
                    "uri",
                    serde_json::json!("viking://user/memories/preferences/rust.md"),
                ),
                (
                    "parent_uri",
                    serde_json::json!("viking://user/memories/preferences"),
                ),
                ("context_type", serde_json::json!("memory")),
                ("is_leaf", serde_json::json!(true)),
                ("abstract", serde_json::json!("Rust testing preference")),
                (
                    "content",
                    serde_json::json!("The user prefers deterministic Rust tests."),
                ),
                ("category", serde_json::json!("preferences")),
            ]),
        )
        .await
        .unwrap();
        vs.upsert(
            "context",
            "viking://resources/rust.md",
            &[1.0, 0.0, 0.0],
            test_fields(&[
                ("uri", serde_json::json!("viking://resources/rust.md")),
                ("context_type", serde_json::json!("resource")),
                ("is_leaf", serde_json::json!(true)),
                ("abstract", serde_json::json!("Rust testing reference")),
            ]),
        )
        .await
        .unwrap();

        vs
    }

    fn test_fields(entries: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    fn typed_memory_query(query: &str, target_directories: &[&str]) -> TypedQuery {
        TypedQuery {
            query: query.to_owned(),
            context_type: RetrieveContextType::Memory,
            intent: "find memories".to_owned(),
            priority: 1,
            target_directories: target_directories
                .iter()
                .map(|dir| (*dir).to_owned())
                .collect(),
        }
    }

    fn trace_mode(result: &QueryResult) -> Option<&str> {
        result
            .thinking_trace
            .events
            .first()
            .and_then(|event| event.data.get("mode"))
            .and_then(serde_json::Value::as_str)
    }

    // Mock types
    struct MockVs;
    #[async_trait::async_trait]
    impl VectorStore for MockVs {
        async fn search(
            &self,
            _: &str,
            _: &[f32],
            _: Option<&HashMap<String, f64>>,
            _: usize,
            _: Option<&HashMap<String, serde_json::Value>>,
        ) -> Result<Vec<VectorHit>, BoxError> {
            Ok(Vec::new())
        }
        async fn upsert(
            &self,
            _: &str,
            _: &str,
            _: &[f32],
            _: HashMap<String, serde_json::Value>,
        ) -> Result<(), BoxError> {
            Ok(())
        }
        async fn update(
            &self,
            _: &str,
            _: &str,
            _: HashMap<String, serde_json::Value>,
        ) -> Result<(), BoxError> {
            Ok(())
        }
        async fn delete(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
    }
    struct MockEmb;
    #[async_trait::async_trait]
    impl Embedder for MockEmb {
        async fn embed(&self, _: &str) -> Result<crate::traits::EmbedResult, BoxError> {
            Ok(crate::traits::EmbedResult {
                dense_vector: Vec::new(),
                sparse_vector: None,
            })
        }
    }

    #[derive(Clone)]
    struct FailingEmb(&'static str);

    #[async_trait::async_trait]
    impl Embedder for FailingEmb {
        fn dense_vector_dim(&self) -> Option<usize> {
            Some(3)
        }

        async fn embed(&self, _: &str) -> Result<crate::traits::EmbedResult, BoxError> {
            Err(self.0.into())
        }
    }

    #[derive(Clone)]
    struct StaticEmb;

    #[async_trait::async_trait]
    impl Embedder for StaticEmb {
        fn dense_vector_dim(&self) -> Option<usize> {
            Some(3)
        }

        async fn embed(&self, _: &str) -> Result<crate::traits::EmbedResult, BoxError> {
            Ok(crate::traits::EmbedResult {
                dense_vector: vec![1.0, 0.0, 0.0],
                sparse_vector: None,
            })
        }
    }

    #[derive(Clone)]
    struct CountingEmb {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Embedder for CountingEmb {
        fn dense_vector_dim(&self) -> Option<usize> {
            Some(3)
        }

        async fn embed(&self, _: &str) -> Result<crate::traits::EmbedResult, BoxError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(crate::traits::EmbedResult {
                dense_vector: vec![1.0, 0.0, 0.0],
                sparse_vector: None,
            })
        }
    }

    struct MockFs;
    #[async_trait::async_trait]
    impl FileSystem for MockFs {
        async fn read(&self, _: &str) -> Result<String, BoxError> {
            Ok(String::new())
        }
        async fn read_bytes(&self, _: &str) -> Result<Vec<u8>, BoxError> {
            Ok(Vec::new())
        }
        async fn write(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn write_bytes(&self, _: &str, _: &[u8]) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mkdir(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn ls(&self, _: &str) -> Result<Vec<crate::traits::FsEntry>, BoxError> {
            Ok(Vec::new())
        }
        async fn rm(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mv(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn stat(&self, _: &str) -> Result<crate::traits::FsStat, BoxError> {
            Err("not implemented".into())
        }
        async fn grep(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
        ) -> Result<Vec<crate::traits::GrepMatch>, BoxError> {
            Ok(Vec::new())
        }
        async fn exists(&self, _: &str) -> Result<bool, BoxError> {
            Ok(false)
        }
        async fn append(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn link(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
    }

    struct MockRr;
    #[async_trait::async_trait]
    impl Reranker for MockRr {
        async fn rerank(
            &self,
            _: &str,
            _: &[String],
            _: usize,
        ) -> Result<Vec<crate::traits::RerankResult>, BoxError> {
            Ok(Vec::new())
        }
    }
}
