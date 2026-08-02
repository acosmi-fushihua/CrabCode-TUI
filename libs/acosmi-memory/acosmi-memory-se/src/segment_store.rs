// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! [`SearchEngine`] — process-in-memory search engine backed by acosmi-segment.
//!
//! Each "collection" maps to a dedicated [`Segment`] instance stored on disk.
//! Operations (upsert, search, delete) go directly through the segment API
//! without any network I/O.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use acosmi_common::budget::ResourcePermit;
use acosmi_common::counter::hardware_accumulator::HwMeasurementAcc;
use acosmi_common::counter::hardware_counter::HardwareCounterCell;
use acosmi_segment::data_types::named_vectors::NamedVectors;
use acosmi_segment::data_types::query_context::QueryContext;
use acosmi_segment::data_types::vectors::{QueryVector, VectorInternal, DEFAULT_VECTOR_NAME};
use acosmi_segment::entry::entry_point::{NonAppendableSegmentEntry, SegmentEntry};
use acosmi_segment::index::hnsw_index::num_rayon_threads;
use acosmi_segment::segment::Segment;
use acosmi_segment::segment_constructor::segment_builder::SegmentBuilder;
use acosmi_segment::segment_constructor::{build_segment, load_segment};
use acosmi_segment::types::{
    Distance, ExtendedPointId, Filter, HnswConfig, HnswGlobalConfig, Indexes, Payload,
    PayloadFieldSchema, PayloadSchemaType, PayloadStorageType, QuantizationConfig, ScoredPoint,
    SearchParams, SegmentConfig, VectorDataConfig, VectorStorageDatatype, VectorStorageType,
    WithPayload, WithVector,
};
use parking_lot::RwLock;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A search result returned by [`SearchEngine`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub score: f32,
    pub payload: HashMap<String, serde_json::Value>,
}

/// A scroll result (no score, just id + payload).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScrollHit {
    pub id: String,
    pub payload: HashMap<String, serde_json::Value>,
}

/// Configuration for creating a collection.
#[derive(Debug, Clone)]
pub struct CollectionConfig {
    /// Dense vector dimension.
    pub dimension: usize,
    /// Distance metric (Cosine, Dot, Euclid, Manhattan).
    pub distance: Distance,
    /// Whether to also create a sparse vector storage.
    pub sparse_vectors: bool,
    /// HNSW index configuration. `None` = Plain (brute-force) index.
    pub hnsw: Option<HnswConfig>,
    /// Quantization configuration. `None` = no quantization (full Float32).
    pub quantization: Option<QuantizationConfig>,
    /// Vector storage type. Default: `InRamChunkedMmap`.
    pub storage_type: VectorStorageType,
    /// Vector storage datatype. `None` = Float32.
    pub datatype: Option<VectorStorageDatatype>,
}

impl Default for CollectionConfig {
    fn default() -> Self {
        Self {
            dimension: 1536,
            distance: Distance::Cosine,
            sparse_vectors: false,
            hnsw: None,
            quantization: None,
            storage_type: VectorStorageType::InRamChunkedMmap,
            datatype: None,
        }
    }
}

/// Kind of vector-index production-readiness issue found by
/// [`SearchEngine::verify_vector_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexIssueKind {
    CollectionMissing,
    SegmentDirectoryMissing,
    SegmentStateMissing,
    SegmentLoadFailed,
    VectorStorageMissing,
    VectorIndexMissing,
    DimensionMismatch,
    MissingVector,
    NonDenseVector,
    NonFiniteVector,
    VectorSearchFailed,
    RebuildFailed,
}

impl VectorIndexIssueKind {
    fn repairable_by_rebuild(self) -> bool {
        matches!(
            self,
            Self::SegmentDirectoryMissing
                | Self::SegmentStateMissing
                | Self::SegmentLoadFailed
                | Self::VectorStorageMissing
                | Self::VectorIndexMissing
                | Self::VectorSearchFailed
        )
    }

    fn blocks_rebuild(self) -> bool {
        matches!(
            self,
            Self::CollectionMissing
                | Self::DimensionMismatch
                | Self::MissingVector
                | Self::NonDenseVector
                | Self::NonFiniteVector
                | Self::RebuildFailed
        )
    }
}

/// One issue in a vector-index verification report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexIssue {
    pub kind: VectorIndexIssueKind,
    pub message: String,
    pub point_id: Option<String>,
    pub path: Option<PathBuf>,
    pub expected_dimension: Option<usize>,
    pub actual_dimension: Option<usize>,
    pub repairable: bool,
}

impl VectorIndexIssue {
    fn new(kind: VectorIndexIssueKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            point_id: None,
            path: None,
            expected_dimension: None,
            actual_dimension: None,
            repairable: kind.repairable_by_rebuild(),
        }
    }

    fn with_point_id(mut self, point_id: ExtendedPointId) -> Self {
        self.point_id = Some(point_id.to_string());
        self
    }

    fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    fn with_dimensions(mut self, expected: Option<usize>, actual: Option<usize>) -> Self {
        self.expected_dimension = expected;
        self.actual_dimension = actual;
        self
    }
}

/// Verification report for a collection's dense-vector index.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexReport {
    pub collection: String,
    pub expected_dimension: Option<usize>,
    pub actual_dimension: Option<usize>,
    pub point_count: usize,
    pub checked_points: usize,
    pub segment_path: Option<PathBuf>,
    pub issues: Vec<VectorIndexIssue>,
}

impl VectorIndexReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }

    fn has_rebuildable_issue(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.kind.repairable_by_rebuild())
    }

    fn has_rebuild_blocker(&self) -> bool {
        self.issues.iter().any(|issue| issue.kind.blocks_rebuild())
    }
}

/// Result of rebuilding a collection segment/index from currently loaded data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexRebuildStats {
    pub collection: String,
    pub point_count: usize,
    pub old_segment_path: PathBuf,
    pub new_segment_path: PathBuf,
}

/// Verify/repair result. `rebuild` is present only when repair attempted a
/// recoverable segment rebuild.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexRepairReport {
    pub collection: String,
    pub before: VectorIndexReport,
    pub rebuild: Option<VectorIndexRebuildStats>,
    pub after: VectorIndexReport,
    pub repaired: bool,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct CollectionHandle {
    segment: Segment,
    config: CollectionConfig,
    /// Monotonically increasing operation number for this segment.
    op_counter: AtomicU64,
}

impl CollectionHandle {
    fn next_op(&self) -> u64 {
        self.op_counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}

// ---------------------------------------------------------------------------
// SearchEngine
// ---------------------------------------------------------------------------

/// In-process search engine backed by acosmi-segment library.
///
/// Thread-safe via `RwLock` over the collections map.
/// Individual `Segment` instances handle their own internal synchronisation.
pub struct SearchEngine {
    /// `collection_name` → `CollectionHandle`.
    collections: RwLock<HashMap<String, CollectionHandle>>,
    /// Root directory for all segment data.
    data_dir: PathBuf,
}

impl SearchEngine {
    /// Create a new store rooted at `data_dir`.
    ///
    /// The directory is created if it does not exist.
    pub fn new(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        Ok(Self {
            collections: RwLock::new(HashMap::new()),
            data_dir,
        })
    }

    /// Load the newest ready segment for `name` from disk.
    ///
    /// This is intentionally explicit: callers choose when persisted index
    /// state should be trusted. Newly written recovery segments are complete
    /// before this method can see them because segment construction writes the
    /// version marker last.
    pub fn load_collection(
        &self,
        name: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let col_dir = self.data_dir.join(name);
        if !col_dir.exists() {
            return Ok(false);
        }

        let mut candidates = ready_segment_candidates(&col_dir)?;
        if candidates.is_empty() {
            return Ok(false);
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified_key));

        let stopped = AtomicBool::new(false);
        let mut load_errors = Vec::new();

        for candidate in candidates {
            match load_segment(&candidate.path, candidate.uuid, &stopped) {
                Ok(segment) => {
                    let config = collection_config_from_segment(&segment)?;
                    let op_counter = AtomicU64::new(segment.version.unwrap_or(0));
                    self.collections.write().insert(
                        name.to_owned(),
                        CollectionHandle {
                            segment,
                            config,
                            op_counter,
                        },
                    );
                    return Ok(true);
                }
                Err(error) => {
                    load_errors.push(format!("{}: {error}", candidate.path.display()));
                }
            }
        }

        Err(format!(
            "failed to load any ready segment for collection {name}: {}",
            load_errors.join("; ")
        )
        .into())
    }

    // --- Collection management -----------------------------------------------

    /// Create a new collection. Returns `true` if created, `false` if it already exists.
    pub fn create_collection(
        &self,
        name: &str,
        cfg: &CollectionConfig,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if cfg.dimension == 0 {
            return Err("collection dimension must be greater than 0".into());
        }

        let mut map = self.collections.write();
        if map.contains_key(name) {
            return Ok(false);
        }

        let col_dir = self.data_dir.join(name);
        std::fs::create_dir_all(&col_dir)?;

        // Build SegmentConfig with a single named dense vector ("dense").
        //
        // NOTE: build_segment() creates an *appendable* (mutable) segment.
        // Appendable segments only support Plain index — HNSW graphs are built
        // during segment *optimization* (converting to non-appendable format).
        // We always use Indexes::Plain here and store the HNSW config in
        // CollectionHandle for future optimization support.
        let mut vector_data = HashMap::new();
        vector_data.insert(
            DEFAULT_VECTOR_NAME.to_owned(),
            VectorDataConfig {
                size: cfg.dimension,
                distance: cfg.distance,
                storage_type: cfg.storage_type,
                index: Indexes::Plain {},
                quantization_config: cfg.quantization.clone(),
                multivector_config: None,
                datatype: cfg.datatype,
            },
        );

        let seg_config = SegmentConfig {
            vector_data,
            sparse_vector_data: Default::default(),
            payload_storage_type: PayloadStorageType::Mmap,
        };

        let segment = build_segment(&col_dir, &seg_config, true)?;

        map.insert(
            name.to_owned(),
            CollectionHandle {
                segment,
                config: cfg.clone(),
                op_counter: AtomicU64::new(0),
            },
        );

        let index_label = if cfg.hnsw.is_some() {
            "Plain (HNSW config stored for optimization)"
        } else {
            "Plain"
        };
        log::info!(
            "Created segment collection: {name} (dim={}, index={index_label})",
            cfg.dimension
        );
        Ok(true)
    }

    /// Check if a collection exists.
    pub fn collection_exists(&self, name: &str) -> bool {
        self.collections.read().contains_key(name)
    }

    /// Return a collection's configuration.
    pub fn collection_config(&self, name: &str) -> Option<CollectionConfig> {
        self.collections.read().get(name).map(|h| h.config.clone())
    }

    /// List all collection names.
    pub fn list_collections(&self) -> Vec<String> {
        self.collections.read().keys().cloned().collect()
    }

    /// Drop a collection, removing it from memory (disk data retained).
    pub fn drop_collection(&self, name: &str) -> bool {
        self.collections.write().remove(name).is_some()
    }

    /// Return the active segment path for diagnostics and tests.
    pub fn active_segment_path(&self, collection: &str) -> Option<PathBuf> {
        self.collections
            .read()
            .get(collection)
            .map(|handle| handle.segment.segment_path.clone())
    }

    /// Verify a collection's vector index and persisted segment files.
    ///
    /// The check is read-only. It validates the in-memory collection shape,
    /// loaded point vectors, searchability, and a separate load from the active
    /// segment directory to catch damaged or missing on-disk index files.
    #[must_use]
    pub fn verify_vector_index(
        &self,
        collection: &str,
        expected_dimension: Option<usize>,
    ) -> VectorIndexReport {
        let mut report = VectorIndexReport {
            collection: collection.to_owned(),
            expected_dimension,
            actual_dimension: None,
            point_count: 0,
            checked_points: 0,
            segment_path: None,
            issues: Vec::new(),
        };

        let map = self.collections.read();
        let Some(handle) = map.get(collection) else {
            report.issues.push(VectorIndexIssue::new(
                VectorIndexIssueKind::CollectionMissing,
                format!("collection not loaded: {collection}"),
            ));
            return report;
        };

        report.actual_dimension = Some(handle.config.dimension);
        report.point_count = handle.segment.available_point_count();
        report.segment_path = Some(handle.segment.segment_path.clone());

        if let Some(expected) = expected_dimension {
            if expected != handle.config.dimension {
                report.issues.push(
                    VectorIndexIssue::new(
                        VectorIndexIssueKind::DimensionMismatch,
                        format!(
                            "collection vector dimension mismatch: expected {expected}, got {}",
                            handle.config.dimension
                        ),
                    )
                    .with_dimensions(Some(expected), Some(handle.config.dimension)),
                );
            }
        }

        report.checked_points += verify_segment_vectors(
            &handle.segment,
            handle.config.dimension,
            "active segment",
            &mut report.issues,
            true,
        );

        verify_segment_files(&handle.segment, &mut report.issues);

        if let Err(error) = smoke_search_segment(&handle.segment, handle.config.dimension) {
            report.issues.push(VectorIndexIssue::new(
                VectorIndexIssueKind::VectorSearchFailed,
                format!("active segment vector search failed: {error}"),
            ));
        }

        verify_segment_reload(&handle.segment, handle.config.dimension, &mut report.issues);

        report
    }

    /// Rebuild the active segment/index from currently loaded vectors and
    /// payloads.
    ///
    /// This does not touch memory `.md` truth-source files. A complete new
    /// segment is built in a temporary directory and moved into the collection
    /// directory before the in-memory handle is swapped, leaving the previous
    /// segment directory available for recovery.
    pub fn rebuild_vector_index(
        &self,
        collection: &str,
    ) -> Result<VectorIndexRebuildStats, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let stopped = AtomicBool::new(false);
        let col_dir = self.data_dir.join(collection);
        std::fs::create_dir_all(&col_dir)?;

        let target_config = handle.segment.segment_config.clone();
        let temp_dir = tempfile::tempdir_in(&col_dir)
            .map_err(|e| format!("create temp dir for vector index rebuild: {e}"))?;
        let mut builder = SegmentBuilder::new(
            temp_dir.path(),
            &target_config,
            &HnswGlobalConfig::default(),
        )?;

        if !builder.update(&[&handle.segment], &stopped)? {
            return Err("vector index rebuild was stopped before completion".into());
        }

        let num_threads = num_rayon_threads(0) as u32;
        let permit = ResourcePermit::dummy(num_threads);
        let hw = HardwareCounterCell::disposable();
        let progress_tracker = acosmi_common::progress_tracker::ProgressTracker::new_for_test();

        let new_segment = builder.build(
            &col_dir,
            Uuid::new_v4(),
            permit,
            &stopped,
            &mut rand::rng(),
            &hw,
            progress_tracker,
        )?;

        let point_count = new_segment.available_point_count();
        let old_segment_path = handle.segment.segment_path.clone();
        let new_segment_path = new_segment.segment_path.clone();
        let new_config = collection_config_from_segment(&new_segment)?;

        handle.segment = new_segment;
        handle.config = new_config;
        handle
            .op_counter
            .store(handle.segment.version.unwrap_or(0), Ordering::Relaxed);

        Ok(VectorIndexRebuildStats {
            collection: collection.to_owned(),
            point_count,
            old_segment_path,
            new_segment_path,
        })
    }

    /// Verify and repair rebuildable vector index damage.
    ///
    /// Missing or invalid embeddings are reported but not regenerated here:
    /// Rust must not call LLM/embedding providers on this path.
    pub fn repair_vector_index(
        &self,
        collection: &str,
        expected_dimension: Option<usize>,
    ) -> VectorIndexRepairReport {
        let before = self.verify_vector_index(collection, expected_dimension);

        let rebuild = if before.has_rebuildable_issue() && !before.has_rebuild_blocker() {
            match self.rebuild_vector_index(collection) {
                Ok(stats) => Some(stats),
                Err(error) => {
                    let mut after = before.clone();
                    after.issues.push(VectorIndexIssue::new(
                        VectorIndexIssueKind::RebuildFailed,
                        format!("vector index rebuild failed: {error}"),
                    ));
                    return VectorIndexRepairReport {
                        collection: collection.to_owned(),
                        before,
                        rebuild: None,
                        after,
                        repaired: false,
                    };
                }
            }
        } else {
            None
        };

        let after = self.verify_vector_index(collection, expected_dimension);
        let repaired = rebuild.is_some() && after.ok();

        VectorIndexRepairReport {
            collection: collection.to_owned(),
            before,
            rebuild,
            after,
            repaired,
        }
    }

    // --- Point operations ----------------------------------------------------

    /// Upsert a point with a dense vector and optional payload.
    ///
    /// `point_id` is a UUID string which is converted to `ExtendedPointId::Uuid`.
    pub fn upsert(
        &self,
        collection: &str,
        point_id: &str,
        dense_vector: &[f32],
        payload: Option<&Payload>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let op_num = handle.next_op();
        let id = parse_point_id(point_id)?;
        let hw = HardwareCounterCell::disposable();

        validate_dense_vector("upsert", dense_vector, &handle.config)?;

        // Build named vectors with the default dense vector.
        let vectors = NamedVectors::from_ref(DEFAULT_VECTOR_NAME, dense_vector.into());

        handle.segment.upsert_point(op_num, id, vectors, &hw)?;

        // Set payload if provided.
        if let Some(p) = payload {
            handle.segment.set_payload(op_num, id, p, &None, &hw)?;
        }

        Ok(())
    }

    /// Merge payload fields into an existing point.
    ///
    /// `point_id` is a UUID string which is converted to `ExtendedPointId::Uuid`.
    /// Returns `true` when the point exists and the payload was updated.
    pub fn set_payload(
        &self,
        collection: &str,
        point_id: &str,
        payload: &Payload,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let op_num = handle.next_op();
        let id = parse_point_id(point_id)?;
        let hw = HardwareCounterCell::disposable();

        let updated = handle
            .segment
            .set_payload(op_num, id, payload, &None, &hw)?;
        Ok(updated)
    }

    /// Search for nearest neighbours in a collection.
    ///
    /// Returns up to `limit` results sorted by descending score.
    /// `search_params` allows tuning HNSW `ef` and quantization behaviour.
    /// Pass `None` for default parameters.
    pub fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        filter: Option<&Filter>,
        limit: usize,
        search_params: Option<&SearchParams>,
    ) -> Result<Vec<SearchHit>, Box<dyn std::error::Error + Send + Sync>> {
        let map = self.collections.read();
        let handle = map
            .get(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        if limit == 0 {
            return Ok(Vec::new());
        }

        validate_dense_vector("search", query_vector, &handle.config)?;

        let query = QueryVector::Nearest(query_vector.to_vec().into());
        let query_context = QueryContext::new(usize::MAX, HwMeasurementAcc::disposable());
        let segment_query_context = query_context.get_segment_query_context();

        let default_params = SearchParams::default();
        let params = search_params.unwrap_or(&default_params);

        let results = handle.segment.search_batch(
            DEFAULT_VECTOR_NAME,
            &[&query],
            &WithPayload::from(true),
            &WithVector::from(false),
            filter,
            limit,
            Some(params),
            &segment_query_context,
        )?;

        // search_batch returns Vec<Vec<ScoredPoint>>, we want the first batch.
        let scored_points = results.into_iter().next().unwrap_or_default();

        scored_points
            .into_iter()
            .map(scored_point_to_hit)
            .collect::<Result<Vec<_>, _>>()
    }

    /// Delete a point by ID. Returns `true` if the point was deleted.
    pub fn delete(
        &self,
        collection: &str,
        point_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let op_num = handle.next_op();
        let id = parse_point_id(point_id)?;
        let hw = HardwareCounterCell::disposable();

        let deleted = handle.segment.delete_point(op_num, id, &hw)?;
        Ok(deleted)
    }

    /// Flush a collection's segment to disk.
    pub fn flush(&self, collection: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let map = self.collections.read();
        let handle = map
            .get(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        handle.segment.flush(true)?;
        Ok(())
    }

    /// Create a payload field index for faster filtered search.
    pub fn create_field_index(
        &self,
        collection: &str,
        field_name: &str,
        field_type: PayloadSchemaType,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let op_num = handle.next_op();
        let hw = HardwareCounterCell::disposable();
        let key = field_name
            .parse()
            .map_err(|_| format!("invalid payload key: {field_name}"))?;
        let schema = PayloadFieldSchema::FieldType(field_type);

        let created = handle
            .segment
            .create_field_index(op_num, &key, Some(&schema), &hw)?;
        Ok(created)
    }

    /// Create a payload field index with full JSON schema configuration.
    /// schema_json is deserialized as PayloadFieldSchema (serde untagged):
    ///   - Simple: "keyword" / "integer" / "float" / "text" / "bool"
    ///   - Parameterized: {"type":"text","tokenizer":"multilingual","lowercase":true}
    pub fn create_field_index_v2(
        &self,
        collection: &str,
        field_name: &str,
        schema_json: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = map
            .get_mut(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let op_num = handle.next_op();
        let hw = HardwareCounterCell::disposable();
        let key = field_name
            .parse()
            .map_err(|_| format!("invalid payload key: {field_name}"))?;
        let schema: PayloadFieldSchema = serde_json::from_str(schema_json)
            .map_err(|e| format!("invalid field schema JSON: {e}"))?;

        let created = handle
            .segment
            .create_field_index(op_num, &key, Some(&schema), &hw)?;
        Ok(created)
    }

    /// Get the number of indexed points in a collection.
    pub fn point_count(&self, collection: &str) -> Option<usize> {
        let map = self.collections.read();
        map.get(collection)
            .map(|h| h.segment.available_point_count())
    }

    /// Scroll all points in a collection (no vector similarity, just enumerate).
    ///
    /// Returns up to `limit` points with their payloads.
    pub fn scroll(
        &self,
        collection: &str,
        limit: usize,
    ) -> Result<Vec<ScrollHit>, Box<dyn std::error::Error + Send + Sync>> {
        let map = self.collections.read();
        let handle = map
            .get(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let is_stopped = AtomicBool::new(false);
        let hw = HardwareCounterCell::disposable();

        let point_ids = handle
            .segment
            .read_filtered(None, Some(limit), None, &is_stopped, &hw);

        let mut results = Vec::with_capacity(point_ids.len());
        for pid in point_ids {
            let payload_raw = handle.segment.payload(pid, &hw)?;
            let payload: HashMap<String, serde_json::Value> = payload_raw
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            let id = match pid {
                ExtendedPointId::NumId(n) => n.to_string(),
                ExtendedPointId::Uuid(u) => u.to_string(),
            };
            results.push(ScrollHit { id, payload });
        }

        Ok(results)
    }

    /// Scroll points matching a Qdrant filter (JSON-encoded).
    ///
    /// `filter_json` is a JSON string representing a Qdrant `Filter`.
    /// Returns up to `limit` matching points with their payloads.
    pub fn scroll_filtered(
        &self,
        collection: &str,
        filter_json: &str,
        limit: usize,
    ) -> Result<Vec<ScrollHit>, Box<dyn std::error::Error + Send + Sync>> {
        let map = self.collections.read();
        let handle = map
            .get(collection)
            .ok_or_else(|| format!("collection not found: {collection}"))?;

        let filter: Filter =
            serde_json::from_str(filter_json).map_err(|e| format!("invalid filter JSON: {e}"))?;

        let is_stopped = AtomicBool::new(false);
        let hw = HardwareCounterCell::disposable();

        let point_ids =
            handle
                .segment
                .read_filtered(None, Some(limit), Some(&filter), &is_stopped, &hw);

        let mut results = Vec::with_capacity(point_ids.len());
        for pid in point_ids {
            let payload_raw = handle.segment.payload(pid, &hw)?;
            let payload: HashMap<String, serde_json::Value> = payload_raw
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            let id = match pid {
                ExtendedPointId::NumId(n) => n.to_string(),
                ExtendedPointId::Uuid(u) => u.to_string(),
            };
            results.push(ScrollHit { id, payload });
        }

        Ok(results)
    }

    // --- Segment optimization ------------------------------------------------

    /// Optimize a collection by converting its appendable (Plain) segment into
    /// a non-appendable segment with an HNSW index.
    ///
    /// Returns `Ok(true)` if HNSW was built, `Ok(false)` if skipped (no HNSW
    /// config, empty collection, or collection not found).
    ///
    /// The caller provides a `stopped` flag that can be used to cancel the
    /// operation cooperatively.
    pub fn optimize_collection(
        &self,
        name: &str,
        stopped: &AtomicBool,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let mut map = self.collections.write();
        let handle = match map.get_mut(name) {
            Some(h) => h,
            None => return Ok(false),
        };

        // Only optimize if the collection was created with HNSW config.
        let hnsw_config = match &handle.config.hnsw {
            Some(cfg) => *cfg,
            None => return Ok(false),
        };

        // Nothing to optimize if the segment is empty.
        if handle.segment.available_point_count() == 0 {
            return Ok(false);
        }

        let col_dir = self.data_dir.join(name);

        // Build target SegmentConfig with HNSW index (non-appendable).
        let mut vector_data = HashMap::new();
        vector_data.insert(
            DEFAULT_VECTOR_NAME.to_owned(),
            VectorDataConfig {
                size: handle.config.dimension,
                distance: handle.config.distance,
                storage_type: handle.config.storage_type,
                index: Indexes::Hnsw(hnsw_config),
                quantization_config: handle.config.quantization.clone(),
                multivector_config: None,
                datatype: handle.config.datatype,
            },
        );

        let target_config = SegmentConfig {
            vector_data,
            sparse_vector_data: Default::default(),
            payload_storage_type: PayloadStorageType::Mmap,
        };

        // Create a temporary directory for the builder inside the collection dir.
        let temp_dir = tempfile::tempdir_in(&col_dir)
            .map_err(|e| format!("create temp dir for optimization: {e}"))?;

        // SegmentBuilder: copy data from existing segment → build HNSW graph.
        let mut builder = SegmentBuilder::new(
            temp_dir.path(),
            &target_config,
            &HnswGlobalConfig::default(),
        )?;

        builder.update(&[&handle.segment], stopped)?;

        let num_threads = num_rayon_threads(0) as u32;
        let permit = ResourcePermit::dummy(num_threads);
        let hw = HardwareCounterCell::disposable();
        let progress_tracker = acosmi_common::progress_tracker::ProgressTracker::new_for_test();

        let new_segment = builder.build(
            &col_dir,
            Uuid::new_v4(),
            permit,
            stopped,
            &mut rand::rng(),
            &hw,
            progress_tracker,
        )?;

        let point_count = new_segment.available_point_count();

        // Replace the old segment (old Segment drops automatically, cleaning disk files).
        handle.segment = new_segment;

        log::info!("Optimized segment collection: {name} (points={point_count}, index=HNSW)");

        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_point_id(s: &str) -> Result<ExtendedPointId, Box<dyn std::error::Error + Send + Sync>> {
    let uuid = Uuid::parse_str(s)?;
    Ok(ExtendedPointId::Uuid(uuid))
}

struct ReadySegmentCandidate {
    path: PathBuf,
    uuid: Uuid,
    modified_key: std::time::SystemTime,
}

fn ready_segment_candidates(
    col_dir: &Path,
) -> Result<Vec<ReadySegmentCandidate>, Box<dyn std::error::Error + Send + Sync>> {
    let mut candidates = Vec::new();

    for entry in std::fs::read_dir(col_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir()
            || !path
                .join(acosmi_segment::segment::SEGMENT_STATE_FILE)
                .is_file()
        {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(file_name) else {
            continue;
        };

        let modified_key = path
            .join(acosmi_segment::segment::SEGMENT_STATE_FILE)
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        candidates.push(ReadySegmentCandidate {
            path,
            uuid,
            modified_key,
        });
    }

    Ok(candidates)
}

fn collection_config_from_segment(
    segment: &Segment,
) -> Result<CollectionConfig, Box<dyn std::error::Error + Send + Sync>> {
    let vector_config = segment
        .segment_config
        .vector_data
        .get(DEFAULT_VECTOR_NAME)
        .ok_or("segment is missing the default dense vector config")?;

    Ok(CollectionConfig {
        dimension: vector_config.size,
        distance: vector_config.distance,
        sparse_vectors: !segment.segment_config.sparse_vector_data.is_empty(),
        hnsw: match &vector_config.index {
            Indexes::Plain {} => None,
            Indexes::Hnsw(config) => Some(*config),
        },
        quantization: vector_config.quantization_config.clone(),
        storage_type: vector_config.storage_type,
        datatype: vector_config.datatype,
    })
}

fn verify_segment_files(segment: &Segment, issues: &mut Vec<VectorIndexIssue>) {
    if !segment.segment_path.is_dir() {
        issues.push(
            VectorIndexIssue::new(
                VectorIndexIssueKind::SegmentDirectoryMissing,
                format!(
                    "segment directory is missing: {}",
                    segment.segment_path.display()
                ),
            )
            .with_path(segment.segment_path.clone()),
        );
        return;
    }

    let state_path = segment
        .segment_path
        .join(acosmi_segment::segment::SEGMENT_STATE_FILE);
    if !state_path.is_file() {
        issues.push(
            VectorIndexIssue::new(
                VectorIndexIssueKind::SegmentStateMissing,
                format!("segment state file is missing: {}", state_path.display()),
            )
            .with_path(state_path),
        );
    }

    let storage_path = default_vector_storage_path(&segment.segment_path);
    if !storage_path.exists() {
        issues.push(
            VectorIndexIssue::new(
                VectorIndexIssueKind::VectorStorageMissing,
                format!(
                    "default vector storage is missing: {}",
                    storage_path.display()
                ),
            )
            .with_path(storage_path),
        );
    }

    let Some(vector_config) = segment.segment_config.vector_data.get(DEFAULT_VECTOR_NAME) else {
        return;
    };

    if vector_config.index.is_indexed() {
        let index_path = default_vector_index_path(&segment.segment_path);
        if !index_path.exists() {
            issues.push(
                VectorIndexIssue::new(
                    VectorIndexIssueKind::VectorIndexMissing,
                    format!("default vector index is missing: {}", index_path.display()),
                )
                .with_path(index_path),
            );
        }
    }
}

fn verify_segment_reload(
    segment: &Segment,
    expected_dimension: usize,
    issues: &mut Vec<VectorIndexIssue>,
) {
    if !segment.segment_path.is_dir() {
        return;
    }

    let stopped = AtomicBool::new(false);
    match load_segment(&segment.segment_path, segment.uuid, &stopped) {
        Ok(loaded) => {
            verify_segment_vectors(
                &loaded,
                expected_dimension,
                "reloaded segment",
                issues,
                false,
            );
            if let Err(error) = smoke_search_segment(&loaded, expected_dimension) {
                issues.push(VectorIndexIssue::new(
                    VectorIndexIssueKind::VectorSearchFailed,
                    format!("reloaded segment vector search failed: {error}"),
                ));
            }
        }
        Err(error) => {
            issues.push(
                VectorIndexIssue::new(
                    VectorIndexIssueKind::SegmentLoadFailed,
                    format!("segment reload failed: {error}"),
                )
                .with_path(segment.segment_path.clone()),
            );
        }
    }
}

fn verify_segment_vectors(
    segment: &Segment,
    expected_dimension: usize,
    source: &str,
    issues: &mut Vec<VectorIndexIssue>,
    count_checked: bool,
) -> usize {
    let hw = HardwareCounterCell::disposable();
    let mut checked = 0;

    for point_id in segment.iter_points() {
        if count_checked {
            checked += 1;
        }

        match segment.vector(DEFAULT_VECTOR_NAME, point_id, &hw) {
            Ok(Some(VectorInternal::Dense(vector))) => {
                if vector.len() != expected_dimension {
                    issues.push(
                        VectorIndexIssue::new(
                            VectorIndexIssueKind::DimensionMismatch,
                            format!(
                                "{source} vector dimension mismatch for point {point_id}: expected {expected_dimension}, got {}",
                                vector.len()
                            ),
                        )
                        .with_point_id(point_id)
                        .with_dimensions(Some(expected_dimension), Some(vector.len())),
                    );
                }
                if vector.iter().any(|value| !value.is_finite()) {
                    issues.push(
                        VectorIndexIssue::new(
                            VectorIndexIssueKind::NonFiniteVector,
                            format!(
                                "{source} vector contains non-finite values for point {point_id}"
                            ),
                        )
                        .with_point_id(point_id),
                    );
                }
            }
            Ok(Some(_)) => {
                issues.push(
                    VectorIndexIssue::new(
                        VectorIndexIssueKind::NonDenseVector,
                        format!("{source} point {point_id} does not store a dense vector"),
                    )
                    .with_point_id(point_id),
                );
            }
            Ok(None) => {
                issues.push(
                    VectorIndexIssue::new(
                        VectorIndexIssueKind::MissingVector,
                        format!("{source} point {point_id} is missing its dense vector"),
                    )
                    .with_point_id(point_id),
                );
            }
            Err(error) => {
                issues.push(
                    VectorIndexIssue::new(
                        VectorIndexIssueKind::MissingVector,
                        format!(
                            "{source} failed to read dense vector for point {point_id}: {error}"
                        ),
                    )
                    .with_point_id(point_id),
                );
            }
        }
    }

    checked
}

fn smoke_search_segment(
    segment: &Segment,
    dimension: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if dimension == 0 || segment.available_point_count() == 0 {
        return Ok(());
    }

    let mut query = vec![0.0_f32; dimension];
    query[0] = 1.0;

    let query = QueryVector::Nearest(query.into());
    let query_context = QueryContext::new(usize::MAX, HwMeasurementAcc::disposable());
    let segment_query_context = query_context.get_segment_query_context();
    let default_params = SearchParams::default();

    segment.search_batch(
        DEFAULT_VECTOR_NAME,
        &[&query],
        &WithPayload::from(false),
        &WithVector::from(false),
        None,
        1,
        Some(&default_params),
        &segment_query_context,
    )?;

    Ok(())
}

fn default_vector_storage_path(segment_path: &Path) -> PathBuf {
    segment_path.join(prefixed_default_vector_file("vector_storage"))
}

fn default_vector_index_path(segment_path: &Path) -> PathBuf {
    segment_path.join(prefixed_default_vector_file("vector_index"))
}

fn prefixed_default_vector_file(prefix: &str) -> String {
    if DEFAULT_VECTOR_NAME.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}-{DEFAULT_VECTOR_NAME}")
    }
}

fn validate_dense_vector(
    operation: &str,
    vector: &[f32],
    cfg: &CollectionConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if vector.is_empty() {
        return Err(format!("{operation} dense vector must not be empty").into());
    }

    if vector.len() != cfg.dimension {
        return Err(format!(
            "{operation} dense vector dimension mismatch: expected {}, got {}",
            cfg.dimension,
            vector.len()
        )
        .into());
    }

    if vector.iter().any(|value| !value.is_finite()) {
        return Err(format!("{operation} dense vector must contain only finite values").into());
    }

    if operation == "search" && vector.iter().all(|value| *value == 0.0) {
        return Err(
            "search dense vector must not be all-zero; Phase 1/2 zero-vector records are \
             scroll-only placeholders and must use scroll_filtered"
                .into(),
        );
    }

    Ok(())
}

fn scored_point_to_hit(
    sp: ScoredPoint,
) -> Result<SearchHit, Box<dyn std::error::Error + Send + Sync>> {
    if !sp.score.is_finite() {
        return Err("search produced a non-finite score".into());
    }

    let id = match sp.id {
        ExtendedPointId::NumId(n) => n.to_string(),
        ExtendedPointId::Uuid(u) => u.to_string(),
    };

    let payload = sp
        .payload
        .map(|p| {
            p.into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<String, serde_json::Value>>()
        })
        .unwrap_or_default();

    Ok(SearchHit {
        id,
        score: sp.score,
        payload,
    })
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn test_config(dimension: usize) -> CollectionConfig {
        CollectionConfig {
            dimension,
            distance: Distance::Cosine,
            sparse_vectors: false,
            ..Default::default()
        }
    }

    #[test]
    fn verify_detects_missing_vector_and_does_not_reembed_in_repair() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let engine = SearchEngine::new(temp_dir.path()).unwrap();
        let collection = "missing_vector";
        let point_id = "00000000-0000-0000-0000-000000009001";

        engine
            .create_collection(collection, &test_config(3))
            .unwrap();
        engine
            .upsert(collection, point_id, &[1.0, 0.0, 0.0], None)
            .unwrap();

        {
            let mut map = engine.collections.write();
            let handle = map.get_mut(collection).unwrap();
            let op_num = handle.next_op();
            let point_id = parse_point_id(point_id).unwrap();
            handle
                .segment
                .delete_vector(op_num, point_id, DEFAULT_VECTOR_NAME)
                .unwrap();
        }

        let report = engine.verify_vector_index(collection, Some(3));
        assert!(report.issues.iter().any(|issue| {
            issue.kind == VectorIndexIssueKind::MissingVector
                && issue.point_id.as_deref() == Some(point_id)
        }));

        let repair = engine.repair_vector_index(collection, Some(3));
        assert!(repair.rebuild.is_none());
        assert!(!repair.repaired);
        assert!(repair.after.issues.iter().any(|issue| {
            issue.kind == VectorIndexIssueKind::MissingVector
                && issue.point_id.as_deref() == Some(point_id)
        }));
    }
}
