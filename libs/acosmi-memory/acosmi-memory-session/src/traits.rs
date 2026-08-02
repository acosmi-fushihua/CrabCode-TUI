// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Async trait abstractions for IO injection.
//!
//! These traits decouple business logic from concrete storage, LLM, and
//! embedding backends. Implementors provide the actual IO; consumers (Session,
//! Compressor, Retriever) operate purely against these interfaces.

use std::collections::HashMap;

use async_trait::async_trait;

/// Error type for trait operations.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// LLM Provider
// ---------------------------------------------------------------------------

/// Abstraction over a text-based LLM completion API.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Generate a completion for the given prompt.
    async fn completion(&self, prompt: &str) -> Result<String, BoxError>;
}

// ---------------------------------------------------------------------------
// Vector Store
// ---------------------------------------------------------------------------

/// A single vector search hit.
#[derive(Debug, Clone)]
pub struct VectorHit {
    /// URI or ID of the matched record.
    pub id: String,
    /// Relevance score.
    pub score: f64,
    /// Field values returned with the hit.
    pub fields: HashMap<String, serde_json::Value>,
}

/// Distance metric for vector similarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DistanceMetric {
    /// Cosine similarity.
    #[default]
    Cosine,
    /// Euclidean (L2) distance.
    Euclid,
    /// Dot product.
    DotProduct,
}

/// A field definition within a collection schema.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FieldDef {
    /// Field name.
    pub name: String,
    /// Field type (e.g. "string", "path", "vector", "sparse_vector",
    /// "date_time", "int64", "bool").
    pub field_type: String,
    /// Whether this field is indexed.
    #[serde(default)]
    pub indexed: bool,
    /// Whether this field is the primary key.
    #[serde(default)]
    pub is_primary: bool,
    /// Vector dimension (only for `field_type = "vector"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
}

/// Schema for creating a collection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CollectionSchema {
    /// Vector dimension (default: 2048).
    pub vector_dim: usize,
    /// Distance metric.
    pub distance: DistanceMetric,
    /// Field definitions.
    pub fields: Vec<FieldDef>,
}

impl Default for CollectionSchema {
    fn default() -> Self {
        Self {
            vector_dim: 2048,
            distance: DistanceMetric::default(),
            fields: Vec::new(),
        }
    }
}

/// Collection metadata and statistics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CollectionInfo {
    /// Collection name.
    pub name: String,
    /// Vector dimension.
    pub vector_dim: usize,
    /// Record count.
    pub count: u64,
    /// Status string (e.g. "ready", "loading").
    pub status: String,
}

/// Result of a scroll operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScrollResult {
    /// Records in this batch.
    pub records: Vec<HashMap<String, serde_json::Value>>,
    /// Cursor for next batch; `None` when exhausted.
    pub next_cursor: Option<String>,
}

/// Typed errors for vector store operations.
#[derive(Debug)]
pub enum VectorStoreError {
    /// Collection does not exist.
    CollectionNotFound(String),
    /// Record does not exist.
    RecordNotFound(String),
    /// Duplicate key on insert.
    DuplicateKey(String),
    /// Backend connection failure.
    ConnectionError(String),
    /// Schema validation failure.
    SchemaError(String),
    /// Catch-all.
    Other(BoxError),
}

impl std::fmt::Display for VectorStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CollectionNotFound(n) => write!(f, "collection not found: {n}"),
            Self::RecordNotFound(id) => write!(f, "record not found: {id}"),
            Self::DuplicateKey(id) => write!(f, "duplicate key: {id}"),
            Self::ConnectionError(msg) => write!(f, "connection error: {msg}"),
            Self::SchemaError(msg) => write!(f, "schema error: {msg}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for VectorStoreError {}

/// Abstraction over a vector database (e.g. VikingDB, Qdrant, Weaviate).
///
/// Core methods (`search`, `upsert`, `update`, `delete`) must be implemented.
/// All other methods provide default implementations returning
/// `Err("not implemented")` so existing implementors remain backward-compatible.
#[async_trait]
pub trait VectorStore: Send + Sync {
    // ===================================================================
    // Core methods (required — backward-compatible with Phase 1)
    // ===================================================================

    /// Search by dense and/or sparse vector.
    async fn search(
        &self,
        collection: &str,
        vector: &[f32],
        sparse_vector: Option<&HashMap<String, f64>>,
        limit: usize,
        filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<VectorHit>, BoxError>;

    /// Upsert a record with vector and fields.
    async fn upsert(
        &self,
        collection: &str,
        id: &str,
        vector: &[f32],
        fields: HashMap<String, serde_json::Value>,
    ) -> Result<(), BoxError>;

    /// Update specific fields of a record (no vector change).
    async fn update(
        &self,
        collection: &str,
        id: &str,
        fields: HashMap<String, serde_json::Value>,
    ) -> Result<(), BoxError>;

    /// Delete a record by ID.
    async fn delete(&self, collection: &str, id: &str) -> Result<(), BoxError>;

    // ===================================================================
    // Collection management
    // ===================================================================

    /// Create a new collection.
    async fn create_collection(
        &self,
        _name: &str,
        _schema: &CollectionSchema,
    ) -> Result<bool, BoxError> {
        Err("create_collection not implemented".into())
    }

    /// Drop a collection.
    async fn drop_collection(&self, _name: &str) -> Result<bool, BoxError> {
        Err("drop_collection not implemented".into())
    }

    /// Check if a collection exists.
    async fn collection_exists(&self, _name: &str) -> Result<bool, BoxError> {
        Err("collection_exists not implemented".into())
    }

    /// List all collection names.
    async fn list_collections(&self) -> Result<Vec<String>, BoxError> {
        Err("list_collections not implemented".into())
    }

    /// Get collection metadata and statistics.
    async fn get_collection_info(&self, _name: &str) -> Result<Option<CollectionInfo>, BoxError> {
        Err("get_collection_info not implemented".into())
    }

    // ===================================================================
    // CRUD — single record extensions
    // ===================================================================

    /// Insert a single record, returning its ID.
    async fn insert_record(
        &self,
        _collection: &str,
        _data: HashMap<String, serde_json::Value>,
    ) -> Result<String, BoxError> {
        Err("insert_record not implemented".into())
    }

    /// Get records by IDs.
    async fn get(
        &self,
        _collection: &str,
        _ids: &[String],
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        Err("get not implemented".into())
    }

    /// Check if a record exists.
    async fn record_exists(&self, _collection: &str, _id: &str) -> Result<bool, BoxError> {
        Err("record_exists not implemented".into())
    }

    // ===================================================================
    // CRUD — batch operations
    // ===================================================================

    /// Batch insert multiple records, returning their IDs.
    async fn batch_insert(
        &self,
        _collection: &str,
        _data: Vec<HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<String>, BoxError> {
        Err("batch_insert not implemented".into())
    }

    /// Batch upsert multiple records, returning their IDs.
    async fn batch_upsert(
        &self,
        _collection: &str,
        _data: Vec<HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<String>, BoxError> {
        Err("batch_upsert not implemented".into())
    }

    /// Batch delete by filter, returning count of deleted records.
    async fn batch_delete(
        &self,
        _collection: &str,
        _filter: &HashMap<String, serde_json::Value>,
    ) -> Result<u64, BoxError> {
        Err("batch_delete not implemented".into())
    }

    /// Remove records by URI (including directory descendants).
    async fn remove_by_uri(&self, _collection: &str, _uri: &str) -> Result<u64, BoxError> {
        Err("remove_by_uri not implemented".into())
    }

    // ===================================================================
    // Advanced search
    // ===================================================================

    /// Full hybrid search with pagination and output field selection.
    #[allow(clippy::too_many_arguments)]
    async fn search_full(
        &self,
        _collection: &str,
        _query_vector: Option<&[f32]>,
        _sparse_query_vector: Option<&HashMap<String, f64>>,
        _filter: Option<&HashMap<String, serde_json::Value>>,
        _limit: usize,
        _offset: usize,
        _output_fields: Option<&[String]>,
        _with_vector: bool,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        Err("search_full not implemented".into())
    }

    /// Pure scalar filtering without vector search.
    #[allow(clippy::too_many_arguments)]
    async fn filter_query(
        &self,
        _collection: &str,
        _filter: &HashMap<String, serde_json::Value>,
        _limit: usize,
        _offset: usize,
        _output_fields: Option<&[String]>,
        _order_by: Option<&str>,
        _order_desc: bool,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        Err("filter_query not implemented".into())
    }

    /// Scroll through large result sets.
    async fn scroll(
        &self,
        _collection: &str,
        _filter: Option<&HashMap<String, serde_json::Value>>,
        _limit: usize,
        _cursor: Option<&str>,
        _output_fields: Option<&[String]>,
    ) -> Result<ScrollResult, BoxError> {
        Err("scroll not implemented".into())
    }

    // ===================================================================
    // Aggregation
    // ===================================================================

    /// Count records matching an optional filter.
    async fn count(
        &self,
        _collection: &str,
        _filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<u64, BoxError> {
        Err("count not implemented".into())
    }

    // ===================================================================
    // Index management
    // ===================================================================

    /// Create an index on a field.
    async fn create_index(
        &self,
        _collection: &str,
        _field: &str,
        _index_type: &str,
    ) -> Result<bool, BoxError> {
        Err("create_index not implemented".into())
    }

    /// Drop an index on a field.
    async fn drop_index(&self, _collection: &str, _field: &str) -> Result<bool, BoxError> {
        Err("drop_index not implemented".into())
    }

    // ===================================================================
    // Lifecycle
    // ===================================================================

    /// Clear all data in a collection (keep schema).
    async fn clear(&self, _collection: &str) -> Result<bool, BoxError> {
        Err("clear not implemented".into())
    }

    /// Optimize collection for better performance.
    async fn optimize(&self, _collection: &str) -> Result<bool, BoxError> {
        Err("optimize not implemented".into())
    }

    /// Close storage connection and release resources.
    async fn close(&self) -> Result<(), BoxError> {
        Err("close not implemented".into())
    }

    // ===================================================================
    // Health & Status
    // ===================================================================

    /// Check if backend is healthy.
    async fn health_check(&self) -> Result<bool, BoxError> {
        Err("health_check not implemented".into())
    }

    /// Get storage statistics.
    async fn get_stats(&self) -> Result<HashMap<String, serde_json::Value>, BoxError> {
        Err("get_stats not implemented".into())
    }
}

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

/// Result of an embedding operation.
#[derive(Debug, Clone)]
pub struct EmbedResult {
    /// Dense vector.
    ///
    /// Phase 3 consumers require this vector to be non-empty, finite,
    /// exactly the collection vector dimension, and not all zeros. Phase 1
    /// zero vectors are an indexing placeholder only; they must not be
    /// returned from an [`Embedder`].
    pub dense_vector: Vec<f32>,
    /// Sparse vector (term → weight).
    pub sparse_vector: Option<HashMap<String, f64>>,
}

impl EmbedResult {
    /// Validate dense vector values when the expected dimension is not known.
    ///
    /// This keeps legacy embedders that cannot report their dimension from
    /// smuggling failed embeddings into vector search as empty, all-zero, or
    /// non-finite vectors.
    pub fn validate_dense_vector_shape(&self) -> Result<(), BoxError> {
        if self.dense_vector.is_empty() {
            return Err("embedder returned empty dense_vector".into());
        }
        if self.dense_vector.iter().any(|value| !value.is_finite()) {
            return Err("embedder returned non-finite dense_vector value".into());
        }
        if self.dense_vector.iter().all(|value| *value == 0.0) {
            return Err("embedder returned all-zero dense_vector".into());
        }
        Ok(())
    }

    /// Validate the dense vector shape before handing it to vector search.
    ///
    /// `expected_dim` must match the target collection's dense vector
    /// dimension. Embedding failures must be returned as `Err` by the
    /// embedder; callers must not represent failures as empty or all-zero
    /// vectors.
    pub fn validate_dense_vector(&self, expected_dim: usize) -> Result<(), BoxError> {
        if self.dense_vector.is_empty() {
            return Err("embedder returned empty dense_vector".into());
        }
        if expected_dim == 0 {
            return Err("expected dense_vector dimension must be greater than 0".into());
        }
        if self.dense_vector.len() != expected_dim {
            return Err(format!(
                "embedder dense_vector dimension mismatch: expected {expected_dim}, got {}",
                self.dense_vector.len()
            )
            .into());
        }
        self.validate_dense_vector_shape()
    }

    /// Return this result after validating the dense vector contract.
    pub fn into_validated(self, expected_dim: usize) -> Result<Self, BoxError> {
        self.validate_dense_vector(expected_dim)?;
        Ok(self)
    }
}

/// Abstraction over a text embedding model.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Dense vector dimension produced by this embedder when known.
    ///
    /// Phase 3 production embedders should return `Some(dim)` and create
    /// collections with the same dimension. `None` is kept for legacy test
    /// doubles and wrappers that cannot report the dimension yet.
    fn dense_vector_dim(&self) -> Option<usize> {
        None
    }

    /// Embed text into dense and optionally sparse vectors.
    ///
    /// Implementors must return an error when embedding cannot be produced.
    /// They must not use empty vectors, all-zero vectors, or dimension-mismatched
    /// vectors as fallbacks.
    async fn embed(&self, text: &str) -> Result<EmbedResult, BoxError>;
}

/// Validate an embedding against the vector collection it will be used with.
///
/// The collection schema is the source of truth for dense-vector dimension.
/// `Embedder::dense_vector_dim()` is used only as a fallback for legacy stores
/// that do not expose collection metadata yet, and as a consistency check when
/// both sides report a dimension.
pub async fn validate_embed_result_for_collection<VS, EMB>(
    vs: &VS,
    collection: &str,
    embedder: &EMB,
    embed_result: &EmbedResult,
) -> Result<(), BoxError>
where
    VS: VectorStore + ?Sized,
    EMB: Embedder + ?Sized,
{
    let collection_dim = collection_vector_dim(vs, collection).await?;
    let embedder_dim = embedder.dense_vector_dim();

    if let (Some(collection_dim), Some(embedder_dim)) = (collection_dim, embedder_dim) {
        if collection_dim != embedder_dim {
            return Err(format!(
                "embedder dense_vector dimension mismatch for collection {collection}: \
                 collection expects {collection_dim}, embedder reports {embedder_dim}"
            )
            .into());
        }
    }

    match collection_dim.or(embedder_dim) {
        Some(dim) => embed_result.validate_dense_vector(dim),
        None => embed_result.validate_dense_vector_shape(),
    }
}

async fn collection_vector_dim<VS>(vs: &VS, collection: &str) -> Result<Option<usize>, BoxError>
where
    VS: VectorStore + ?Sized,
{
    match vs.get_collection_info(collection).await {
        Ok(Some(info)) => Ok(Some(info.vector_dim)),
        Ok(None) => Ok(None),
        Err(err)
            if err
                .to_string()
                .contains("get_collection_info not implemented") =>
        {
            Ok(None)
        }
        Err(err) => {
            Err(format!("failed to read vector collection metadata for {collection}: {err}").into())
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Minimal deterministic embedder for unit tests.
    ///
    /// This does not call a network service. It produces a stable non-zero
    /// dense vector with the requested dimension, or returns the configured
    /// error verbatim for error propagation tests.
    #[derive(Debug, Clone)]
    pub(crate) struct TestEmbedder {
        dim: usize,
        error: Option<String>,
    }

    impl TestEmbedder {
        pub(crate) fn new(dim: usize) -> Self {
            Self { dim, error: None }
        }

        pub(crate) fn failing(message: impl Into<String>) -> Self {
            Self {
                dim: 1,
                error: Some(message.into()),
            }
        }
    }

    #[async_trait]
    impl Embedder for TestEmbedder {
        fn dense_vector_dim(&self) -> Option<usize> {
            Some(self.dim)
        }

        async fn embed(&self, text: &str) -> Result<EmbedResult, BoxError> {
            if let Some(message) = &self.error {
                return Err(message.clone().into());
            }

            let mut dense_vector = vec![0.0_f32; self.dim];
            if self.dim > 0 {
                dense_vector[0] = 1.0;
                for (index, byte) in text.bytes().enumerate() {
                    dense_vector[index % self.dim] += (byte as f32 + 1.0) / 255.0;
                }
            }

            EmbedResult {
                dense_vector,
                sparse_vector: None,
            }
            .into_validated(self.dim)
        }
    }
}

#[cfg(test)]
mod embedder_contract_tests {
    use super::test_support::TestEmbedder;
    use super::*;

    fn assert_embedder<T: Embedder>(_embedder: &T) {}

    #[tokio::test]
    async fn test_embedder_uses_embedder_trait_name_and_reports_dimension() {
        let embedder = TestEmbedder::new(3);
        assert_embedder(&embedder);
        assert_eq!(embedder.dense_vector_dim(), Some(3));

        let result = embedder.embed("phase three vector contract").await.unwrap();
        assert_eq!(result.dense_vector.len(), 3);
        result.validate_dense_vector(3).unwrap();
    }

    #[test]
    fn dense_vector_validation_rejects_empty_vectors() {
        let result = EmbedResult {
            dense_vector: Vec::new(),
            sparse_vector: None,
        };

        let err = result.validate_dense_vector(3).unwrap_err();
        assert_eq!(err.to_string(), "embedder returned empty dense_vector");
    }

    #[test]
    fn dense_vector_shape_validation_works_without_reported_dimension() {
        let valid = EmbedResult {
            dense_vector: vec![1.0, 0.0],
            sparse_vector: None,
        };
        valid.validate_dense_vector_shape().unwrap();

        let zero = EmbedResult {
            dense_vector: vec![0.0, -0.0],
            sparse_vector: None,
        };
        assert_eq!(
            zero.validate_dense_vector_shape().unwrap_err().to_string(),
            "embedder returned all-zero dense_vector"
        );
    }

    #[test]
    fn dense_vector_validation_rejects_dimension_mismatch() {
        let result = EmbedResult {
            dense_vector: vec![1.0, 2.0],
            sparse_vector: None,
        };

        let err = result.validate_dense_vector(3).unwrap_err();
        assert_eq!(
            err.to_string(),
            "embedder dense_vector dimension mismatch: expected 3, got 2"
        );
    }

    #[test]
    fn dense_vector_validation_rejects_non_finite_values() {
        let result = EmbedResult {
            dense_vector: vec![1.0, f32::NAN],
            sparse_vector: None,
        };

        let err = result.validate_dense_vector(2).unwrap_err();
        assert_eq!(
            err.to_string(),
            "embedder returned non-finite dense_vector value"
        );
    }

    #[test]
    fn dense_vector_validation_rejects_zero_vectors() {
        let result = EmbedResult {
            dense_vector: vec![0.0, -0.0],
            sparse_vector: None,
        };

        let err = result.validate_dense_vector(2).unwrap_err();
        assert_eq!(err.to_string(), "embedder returned all-zero dense_vector");
    }

    #[tokio::test]
    async fn test_embedder_propagates_errors_without_vector_fallback() {
        let embedder = TestEmbedder::failing("embed API unavailable");

        let err = embedder.embed("anything").await.unwrap_err();
        assert_eq!(err.to_string(), "embed API unavailable");
    }
}

// ---------------------------------------------------------------------------
// File System
// ---------------------------------------------------------------------------

/// A directory listing entry.
#[derive(Debug, Clone)]
pub struct FsEntry {
    /// Entry name.
    pub name: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

/// File/directory metadata (stat result).
#[derive(Debug, Clone)]
pub struct FsStat {
    /// Entry name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// Last modification time (ISO 8601 string).
    pub mod_time: String,
}

/// A single grep match.
#[derive(Debug, Clone)]
pub struct GrepMatch {
    /// URI of the matched file.
    pub uri: String,
    /// Line number (1-indexed).
    pub line: u64,
    /// Content of the matching line.
    pub content: String,
}

/// Helper for callers that must distinguish "file not found"
/// (recoverable — caller may want to create the file) from real IO
/// failure (must propagate, never silently fall back).
///
/// Implementation tries (1) downcast to [`std::io::Error`] and match
/// `kind()`, then (2) a conservative string heuristic on the
/// formatted error message for [`FileSystem`] implementations that
/// wrap their errors as `String`. The downcast path is the
/// authoritative one; the heuristic exists for backward compatibility
/// only and intentionally rejects any ambiguous case.
///
/// Closes Step 2 Phase D.7 — root cause R1 (Step 1 §六) /
/// HIGH-extractor.rs:259: previously
/// `fs.read(uri).await.unwrap_or_default()` collapsed every error
/// (NotFound + permission denied + transient IO + parse error) into
/// the empty-string fallback path, which then **overwrote the entire
/// user profile** with the new candidate. After this helper exists,
/// extractor and similar callers can match `Err(e) if is_not_found_error(&e)`
/// for the recoverable branch and propagate everything else.
#[must_use]
pub fn is_not_found_error(err: &BoxError) -> bool {
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        return io_err.kind() == std::io::ErrorKind::NotFound;
    }
    let s = err.to_string().to_lowercase();
    s.contains("no such file")
        || (s.contains("not found") && !s.contains("not found in"))
        || s.contains("the system cannot find")
}

/// Abstraction over a Viking-URI-aware file system (e.g. AGFS).
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Read file content as string.
    async fn read(&self, uri: &str) -> Result<String, BoxError>;

    /// Read file content as bytes.
    async fn read_bytes(&self, uri: &str) -> Result<Vec<u8>, BoxError>;

    /// Write string content.
    async fn write(&self, uri: &str, content: &str) -> Result<(), BoxError>;

    /// Write binary content.
    async fn write_bytes(&self, uri: &str, content: &[u8]) -> Result<(), BoxError>;

    /// Create a directory.
    async fn mkdir(&self, uri: &str) -> Result<(), BoxError>;

    /// List directory contents.
    async fn ls(&self, uri: &str) -> Result<Vec<FsEntry>, BoxError>;

    /// Remove a file or directory.
    async fn rm(&self, uri: &str) -> Result<(), BoxError>;

    /// Move/rename a file or directory.
    async fn mv(&self, from_uri: &str, to_uri: &str) -> Result<(), BoxError>;

    /// Get file/directory metadata.
    async fn stat(&self, uri: &str) -> Result<FsStat, BoxError>;

    /// Content search by pattern (grep).
    async fn grep(
        &self,
        uri: &str,
        pattern: &str,
        recursive: bool,
        case_insensitive: bool,
    ) -> Result<Vec<GrepMatch>, BoxError>;

    /// Check if a file or directory exists.
    async fn exists(&self, uri: &str) -> Result<bool, BoxError>;

    /// Append string content to a file.
    async fn append(&self, uri: &str, content: &str) -> Result<(), BoxError>;

    /// Create a symbolic link between two URIs.
    async fn link(&self, source_uri: &str, target_uri: &str) -> Result<(), BoxError>;
}

// ---------------------------------------------------------------------------
// Reranker
// ---------------------------------------------------------------------------

/// A single reranking result.
#[derive(Debug, Clone)]
pub struct RerankResult {
    /// Original index in the input list.
    pub index: usize,
    /// Reranked relevance score (higher = more relevant).
    pub score: f64,
}

/// Abstraction over a reranking model (e.g. Cohere, BGE-reranker).
///
/// Rerankers improve precision by re-scoring candidate results against
/// the original query using cross-encoder attention.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Rerank a list of documents against a query.
    ///
    /// Returns scores for each document, sorted by relevance (highest first).
    async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_k: usize,
    ) -> Result<Vec<RerankResult>, BoxError>;
}
