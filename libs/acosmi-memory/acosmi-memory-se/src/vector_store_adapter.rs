// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Adapter from [`SearchEngine`] to the session [`VectorStore`] trait.
//!
//! The Phase 1 memory index stores `.md` truth-source records with a dim=1
//! zero vector and retrieves them through [`SearchEngine::scroll_filtered`].
//! Once a real Phase 3 embedder is available, the same adapter supports vector
//! search for session consumers.

use std::collections::HashMap;
use std::sync::Arc;

use acosmi_memory_session::{
    BoxError, CollectionInfo, CollectionSchema, DistanceMetric, ScrollResult, VectorHit,
    VectorStore,
};
use acosmi_segment::types::{Filter, Payload};
use async_trait::async_trait;
use serde_json::json;
use tokio::task;
use uuid::Uuid;

use crate::segment_store::{CollectionConfig, ScrollHit, SearchEngine, SearchHit};
use crate::{Distance, VectorStorageType};

/// Phase 1 collection config for memory `.md` truth-source indexing.
///
/// Contract:
/// - vector dimension is exactly `1`
/// - callers upsert with `&[0.0_f32]`
/// - retrieval uses [`SearchEngine::scroll_filtered`], not vector search
#[must_use]
pub fn phase1_collection_config() -> CollectionConfig {
    CollectionConfig {
        dimension: 1,
        distance: Distance::Cosine,
        sparse_vectors: false,
        hnsw: None,
        quantization: None,
        storage_type: VectorStorageType::InRamChunkedMmap,
        datatype: None,
    }
}

/// [`VectorStore`] wrapper around an in-process [`SearchEngine`].
#[derive(Clone)]
pub struct SearchEngineVectorStore {
    engine: Arc<SearchEngine>,
}

impl SearchEngineVectorStore {
    /// Create a new adapter over a shared [`SearchEngine`].
    #[must_use]
    pub fn new(engine: Arc<SearchEngine>) -> Self {
        Self { engine }
    }

    /// Access the wrapped search engine.
    #[must_use]
    pub fn engine(&self) -> &Arc<SearchEngine> {
        &self.engine
    }
}

#[async_trait]
impl VectorStore for SearchEngineVectorStore {
    async fn search(
        &self,
        collection: &str,
        vector: &[f32],
        sparse_vector: Option<&HashMap<String, f64>>,
        limit: usize,
        filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<VectorHit>, BoxError> {
        ensure_search_vector(vector)?;
        ensure_sparse_boundary(sparse_vector)?;

        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let vector = vector.to_vec();
        let filter = filter.map(filter_map_to_search_filter).transpose()?;

        task::spawn_blocking(move || {
            engine
                .search(&collection, &vector, filter.as_ref(), limit, None)
                .map(|hits| hits.into_iter().map(search_hit_to_vector_hit).collect())
        })
        .await
        .map_err(join_error)?
    }

    async fn upsert(
        &self,
        collection: &str,
        id: &str,
        vector: &[f32],
        mut fields: HashMap<String, serde_json::Value>,
    ) -> Result<(), BoxError> {
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let external_id = id.to_owned();
        let point_id = point_id_for_external_id(&external_id);
        let vector = vector.to_vec();
        ensure_upsert_vector(&vector)?;
        fields.insert("id".to_owned(), json!(external_id));
        if external_id.starts_with("viking://") && !fields.contains_key("uri") {
            fields.insert("uri".to_owned(), json!(external_id));
        }
        let payload = fields_to_payload(fields);

        task::spawn_blocking(move || engine.upsert(&collection, &point_id, &vector, Some(&payload)))
            .await
            .map_err(join_error)?
    }

    async fn update(
        &self,
        collection: &str,
        id: &str,
        fields: HashMap<String, serde_json::Value>,
    ) -> Result<(), BoxError> {
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let point_id = point_id_for_external_id(id);
        let payload = fields_to_payload(fields);

        task::spawn_blocking(move || {
            engine
                .set_payload(&collection, &point_id, &payload)
                .map(|_| ())
        })
        .await
        .map_err(join_error)?
    }

    async fn delete(&self, collection: &str, id: &str) -> Result<(), BoxError> {
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let point_id = point_id_for_external_id(id);

        task::spawn_blocking(move || engine.delete(&collection, &point_id).map(|_| ()))
            .await
            .map_err(join_error)?
    }

    async fn create_collection(
        &self,
        name: &str,
        schema: &CollectionSchema,
    ) -> Result<bool, BoxError> {
        let engine = Arc::clone(&self.engine);
        let name = name.to_owned();
        let cfg = CollectionConfig {
            dimension: schema.vector_dim,
            distance: match schema.distance {
                DistanceMetric::Cosine => Distance::Cosine,
                DistanceMetric::Euclid => Distance::Euclid,
                DistanceMetric::DotProduct => Distance::Dot,
            },
            ..CollectionConfig::default()
        };

        task::spawn_blocking(move || engine.create_collection(&name, &cfg))
            .await
            .map_err(join_error)?
    }

    async fn drop_collection(&self, name: &str) -> Result<bool, BoxError> {
        let engine = Arc::clone(&self.engine);
        let name = name.to_owned();

        task::spawn_blocking(move || Ok::<bool, BoxError>(engine.drop_collection(&name)))
            .await
            .map_err(join_error)?
    }

    async fn collection_exists(&self, name: &str) -> Result<bool, BoxError> {
        let engine = Arc::clone(&self.engine);
        let name = name.to_owned();

        task::spawn_blocking(move || Ok::<bool, BoxError>(engine.collection_exists(&name)))
            .await
            .map_err(join_error)?
    }

    async fn list_collections(&self) -> Result<Vec<String>, BoxError> {
        let engine = Arc::clone(&self.engine);

        task::spawn_blocking(move || Ok::<Vec<String>, BoxError>(engine.list_collections()))
            .await
            .map_err(join_error)?
    }

    async fn get_collection_info(&self, name: &str) -> Result<Option<CollectionInfo>, BoxError> {
        let engine = Arc::clone(&self.engine);
        let name = name.to_owned();

        task::spawn_blocking(move || {
            Ok::<Option<CollectionInfo>, BoxError>(engine.collection_config(&name).map(|cfg| {
                let count = engine.point_count(&name).unwrap_or(0) as u64;
                CollectionInfo {
                    name,
                    vector_dim: cfg.dimension,
                    count,
                    status: "ready".to_owned(),
                }
            }))
        })
        .await
        .map_err(join_error)?
    }

    async fn remove_by_uri(&self, collection: &str, uri: &str) -> Result<u64, BoxError> {
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let uri = uri.to_owned();

        task::spawn_blocking(move || {
            let prefix = format!("{uri}/");
            let hits = engine.scroll(&collection, usize::MAX)?;
            let mut removed = 0_u64;

            for hit in hits {
                let record_uri = hit
                    .payload
                    .get("uri")
                    .or_else(|| hit.payload.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if (record_uri == uri || record_uri.starts_with(&prefix))
                    && engine.delete(&collection, &hit.id)?
                {
                    removed += 1;
                }
            }

            Ok::<u64, BoxError>(removed)
        })
        .await
        .map_err(join_error)?
    }

    #[allow(clippy::too_many_arguments)]
    async fn search_full(
        &self,
        collection: &str,
        query_vector: Option<&[f32]>,
        sparse_query_vector: Option<&HashMap<String, f64>>,
        filter: Option<&HashMap<String, serde_json::Value>>,
        limit: usize,
        offset: usize,
        output_fields: Option<&[String]>,
        with_vector: bool,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        if with_vector {
            return Err(
                "SearchEngineVectorStore::search_full does not return stored vectors".into(),
            );
        }
        ensure_sparse_boundary(sparse_query_vector)?;

        if let Some(vector) = query_vector {
            ensure_search_vector(vector)?;
            let hits = self
                .search(
                    collection,
                    vector,
                    None,
                    limit.saturating_add(offset),
                    filter,
                )
                .await?;
            return Ok(hits
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|hit| project_fields(hit.fields, output_fields))
                .collect());
        }

        let empty_filter = HashMap::new();
        let filter = filter.unwrap_or(&empty_filter);
        self.filter_query(
            collection,
            filter,
            limit,
            offset,
            output_fields,
            None,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn filter_query(
        &self,
        collection: &str,
        filter: &HashMap<String, serde_json::Value>,
        limit: usize,
        offset: usize,
        output_fields: Option<&[String]>,
        order_by: Option<&str>,
        _order_desc: bool,
    ) -> Result<Vec<HashMap<String, serde_json::Value>>, BoxError> {
        if order_by.is_some() {
            return Err("SearchEngineVectorStore::filter_query order_by is not implemented".into());
        }

        let page = self
            .scroll(
                collection,
                Some(filter),
                limit.saturating_add(offset),
                None,
                output_fields,
            )
            .await?;
        Ok(page.records.into_iter().skip(offset).take(limit).collect())
    }

    async fn scroll(
        &self,
        collection: &str,
        filter: Option<&HashMap<String, serde_json::Value>>,
        limit: usize,
        cursor: Option<&str>,
        output_fields: Option<&[String]>,
    ) -> Result<ScrollResult, BoxError> {
        if limit == 0 {
            return Ok(ScrollResult {
                records: Vec::new(),
                next_cursor: None,
            });
        }

        let start = parse_cursor(cursor)?;
        let fetch_limit = start.saturating_add(limit).saturating_add(1);
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();
        let filter_json = filter.map(filter_map_to_filter_json).transpose()?;

        let hits = task::spawn_blocking(move || match filter_json {
            Some(filter_json) => engine.scroll_filtered(&collection, &filter_json, fetch_limit),
            None => engine.scroll(&collection, fetch_limit),
        })
        .await
        .map_err(join_error)??;

        let has_more = hits.len() > start.saturating_add(limit);
        let records = hits
            .into_iter()
            .skip(start)
            .take(limit)
            .map(scroll_hit_to_record)
            .map(|record| project_fields(record, output_fields))
            .collect();

        Ok(ScrollResult {
            records,
            next_cursor: has_more.then(|| start.saturating_add(limit).to_string()),
        })
    }

    async fn count(
        &self,
        collection: &str,
        filter: Option<&HashMap<String, serde_json::Value>>,
    ) -> Result<u64, BoxError> {
        if filter.is_none() {
            let engine = Arc::clone(&self.engine);
            let collection = collection.to_owned();
            return task::spawn_blocking(move || {
                Ok::<u64, BoxError>(engine.point_count(&collection).unwrap_or(0) as u64)
            })
            .await
            .map_err(join_error)?;
        }

        let page = self
            .scroll(collection, filter, usize::MAX, None, None)
            .await?;
        Ok(page.records.len() as u64)
    }

    async fn optimize(&self, collection: &str) -> Result<bool, BoxError> {
        let engine = Arc::clone(&self.engine);
        let collection = collection.to_owned();

        task::spawn_blocking(move || {
            let stopped = std::sync::atomic::AtomicBool::new(false);
            engine.optimize_collection(&collection, &stopped)
        })
        .await
        .map_err(join_error)?
    }

    async fn health_check(&self) -> Result<bool, BoxError> {
        Ok(true)
    }
}

/// Convert a [`VectorStore`] filter map into the Qdrant-compatible JSON string
/// accepted by [`SearchEngine::scroll_filtered`].
///
/// The input may use the real Qdrant filter shape, for example
/// `{"must":[{"key":"scope","match":{"value":"private"}}]}`. Phase 3 also
/// accepts the simple equality maps used by session consumers, such as
/// `{"context_type":"memory","is_leaf":true}`, and converts them into a
/// Qdrant `must` filter.
pub fn filter_map_to_filter_json(
    filter: &HashMap<String, serde_json::Value>,
) -> Result<String, BoxError> {
    let filter = filter_map_to_search_filter(filter)?;
    serde_json::to_string(&filter)
        .map_err(|e| format!("serialize validated filter JSON failed: {e}").into())
}

/// Convert a [`VectorStore`] filter map into the [`SearchEngine::search`] filter type.
///
/// Phase 1 does not call vector search, but keeping this conversion typed makes
/// the adapter boundary explicit for Phase 3.
pub fn filter_map_to_search_filter(
    filter: &HashMap<String, serde_json::Value>,
) -> Result<Filter, BoxError> {
    let value = serde_json::to_value(filter)
        .map_err(|e| format!("serialize filter map to JSON value failed: {e}"))?;
    match validate_filter_value(value) {
        Ok(filter) => Ok(filter),
        Err(qdrant_error) => {
            simple_equality_filter_to_search_filter(filter).map_err(|simple_error| {
                format!(
                    "filter must use SearchEngine/Qdrant Filter JSON shape or a simple \
                     equality map: {qdrant_error}; simple equality conversion failed: \
                     {simple_error}"
                )
                .into()
            })
        }
    }
}

fn validate_filter_value(value: serde_json::Value) -> Result<Filter, BoxError> {
    serde_json::from_value::<Filter>(value)
        .map_err(|e| format!("filter must use SearchEngine/Qdrant Filter JSON shape: {e}").into())
}

fn fields_to_payload(fields: HashMap<String, serde_json::Value>) -> Payload {
    Payload::from(fields.into_iter().collect::<serde_json::Map<_, _>>())
}

fn simple_equality_filter_to_search_filter(
    filter: &HashMap<String, serde_json::Value>,
) -> Result<Filter, BoxError> {
    if filter.is_empty() {
        return Ok(Filter::new());
    }

    if filter.keys().any(|key| is_qdrant_filter_key(key)) {
        return Err(
            "reserved Qdrant filter keys cannot be used by the simple equality converter".into(),
        );
    }

    let mut must = Vec::with_capacity(filter.len());
    for (key, value) in filter {
        if key == "uri_prefix" {
            let prefix = value
                .as_str()
                .ok_or("uri_prefix filter value must be a string")?;
            must.push(json!({
                "key": "uri",
                "match": { "text": prefix },
            }));
            continue;
        }

        let value = simple_match_value(key, value)?;

        must.push(json!({
            "key": key,
            "match": { "value": value },
        }));
    }

    validate_filter_value(json!({ "must": must }))
}

fn search_hit_to_vector_hit(hit: SearchHit) -> VectorHit {
    let id = external_id_from_payload(&hit.id, &hit.payload);
    let mut fields = hit.payload;
    fields.insert("id".to_owned(), json!(id.clone()));

    VectorHit {
        id,
        score: f64::from(hit.score),
        fields,
    }
}

fn scroll_hit_to_record(hit: ScrollHit) -> HashMap<String, serde_json::Value> {
    let id = external_id_from_payload(&hit.id, &hit.payload);
    let mut fields = hit.payload;
    fields.insert("id".to_owned(), json!(id));
    fields
}

fn simple_match_value(key: &str, value: &serde_json::Value) -> Result<serde_json::Value, BoxError> {
    match value {
        serde_json::Value::String(_) | serde_json::Value::Bool(_) => Ok(value.clone()),
        serde_json::Value::Number(number) => {
            let integer = number
                .as_i64()
                .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
                .ok_or_else(|| {
                    format!("field {key} has unsupported equality value; expected an i64 integer")
                })?;
            Ok(json!(integer))
        }
        _ => Err(format!(
            "field {key} has unsupported equality value; expected string, bool, or integer"
        )
        .into()),
    }
}

fn is_qdrant_filter_key(key: &str) -> bool {
    matches!(key, "must" | "must_not" | "should" | "min_should")
}

fn point_id_for_external_id(id: &str) -> String {
    Uuid::parse_str(id)
        .unwrap_or_else(|_| Uuid::new_v5(&Uuid::NAMESPACE_OID, id.as_bytes()))
        .to_string()
}

fn ensure_upsert_vector(vector: &[f32]) -> Result<(), BoxError> {
    if vector.is_empty() {
        return Err("SearchEngineVectorStore::upsert requires a non-empty dense vector".into());
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(
            "SearchEngineVectorStore::upsert rejects non-finite dense vector values".into(),
        );
    }
    Ok(())
}

fn ensure_search_vector(vector: &[f32]) -> Result<(), BoxError> {
    ensure_upsert_vector(vector)?;
    if vector.iter().all(|value| *value == 0.0) {
        return Err(
            "SearchEngineVectorStore::search requires a non-zero Phase 3 dense vector; \
             Phase 1 all-zero vectors must use SearchEngine::scroll_filtered"
                .into(),
        );
    }
    Ok(())
}

fn ensure_sparse_boundary(sparse_vector: Option<&HashMap<String, f64>>) -> Result<(), BoxError> {
    if sparse_vector.is_some_and(|sparse| !sparse.is_empty()) {
        return Err("SearchEngineVectorStore does not support sparse vectors yet".into());
    }
    Ok(())
}

fn external_id_from_payload(
    fallback: &str,
    payload: &HashMap<String, serde_json::Value>,
) -> String {
    payload
        .get("id")
        .or_else(|| payload.get("uri"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn project_fields(
    mut fields: HashMap<String, serde_json::Value>,
    output_fields: Option<&[String]>,
) -> HashMap<String, serde_json::Value> {
    if let Some(output_fields) = output_fields {
        fields.retain(|key, _| output_fields.iter().any(|field| field == key));
    }
    fields
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, BoxError> {
    cursor
        .map(|raw| {
            raw.parse::<usize>()
                .map_err(|e| format!("invalid SearchEngineVectorStore scroll cursor: {e}").into())
        })
        .unwrap_or(Ok(0))
}

fn join_error(err: task::JoinError) -> BoxError {
    format!("SearchEngineVectorStore spawn_blocking join failed: {err}").into()
}
