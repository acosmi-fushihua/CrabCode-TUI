use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use acosmi_memory_core::message::Message;
use acosmi_memory_core::retrieve_types::{RetrieveContextType, TypedQuery};
use acosmi_memory_core::session_types::{CandidateMemory, DedupDecision, MemoryCategory};
use acosmi_memory_core::user::UserIdentifier;
use acosmi_memory_se::segment_store::{CollectionConfig, SearchEngine};
use acosmi_memory_se::{
    filter_map_to_filter_json, filter_map_to_search_filter, phase1_collection_config, Distance,
    SearchEngineVectorStore, VectorStorageType,
};
use acosmi_memory_session::compressor::SessionCompressor;
use acosmi_memory_session::deduplicator::MemoryDeduplicator;
use acosmi_memory_session::retriever::HierarchicalRetriever;
use acosmi_memory_session::{
    BoxError, EmbedResult, Embedder, FileSystem, FsEntry, FsStat, GrepMatch, LlmProvider, Reranker,
    VectorStore,
};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

const COLLECTION: &str = "phase1_adapter";

fn test_adapter() -> (TempDir, Arc<SearchEngine>, SearchEngineVectorStore) {
    let dir = TempDir::new().unwrap();
    let engine = Arc::new(SearchEngine::new(dir.path()).unwrap());
    engine
        .create_collection(COLLECTION, &phase1_collection_config())
        .unwrap();
    let adapter = SearchEngineVectorStore::new(Arc::clone(&engine));
    (dir, engine, adapter)
}

fn test_vector_adapter(
    collection: &str,
    dimension: usize,
) -> (TempDir, Arc<SearchEngine>, SearchEngineVectorStore) {
    let dir = TempDir::new().unwrap();
    let engine = Arc::new(SearchEngine::new(dir.path()).unwrap());
    engine
        .create_collection(collection, &vector_collection_config(dimension))
        .unwrap();
    let adapter = SearchEngineVectorStore::new(Arc::clone(&engine));
    (dir, engine, adapter)
}

fn vector_collection_config(dimension: usize) -> CollectionConfig {
    CollectionConfig {
        dimension,
        distance: Distance::Cosine,
        sparse_vectors: false,
        hnsw: None,
        quantization: None,
        storage_type: VectorStorageType::InRamChunkedMmap,
        datatype: None,
    }
}

fn uuid(index: usize) -> String {
    format!("00000000-0000-0000-0001-{index:012x}")
}

fn fields(entries: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn qdrant_bucket_filter(bucket: &str) -> HashMap<String, serde_json::Value> {
    fields(&[(
        "must",
        json!([{
            "key": "bucket",
            "match": { "value": bucket }
        }]),
    )])
}

#[derive(Clone, Default)]
struct FakeEmbedder;

#[async_trait]
impl Embedder for FakeEmbedder {
    fn dense_vector_dim(&self) -> Option<usize> {
        Some(3)
    }

    async fn embed(&self, text: &str) -> Result<EmbedResult, BoxError> {
        let lower = text.to_lowercase();
        let dense_vector = if lower.contains("rust") {
            vec![1.0, 0.0, 0.0]
        } else if lower.contains("python") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.25, 0.25, 1.0]
        };

        EmbedResult {
            dense_vector,
            sparse_vector: None,
        }
        .into_validated(3)
    }
}

#[derive(Clone)]
struct StaticLlm {
    response: String,
}

impl StaticLlm {
    fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for StaticLlm {
    async fn completion(&self, _: &str) -> Result<String, BoxError> {
        Ok(self.response.clone())
    }
}

#[derive(Clone, Default)]
struct MemoryFs {
    files: Arc<Mutex<HashMap<String, String>>>,
}

#[async_trait]
impl FileSystem for MemoryFs {
    async fn read(&self, uri: &str) -> Result<String, BoxError> {
        self.files
            .lock()
            .unwrap()
            .get(uri)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, uri).into())
    }

    async fn read_bytes(&self, uri: &str) -> Result<Vec<u8>, BoxError> {
        Ok(self.read(uri).await?.into_bytes())
    }

    async fn write(&self, uri: &str, content: &str) -> Result<(), BoxError> {
        self.files
            .lock()
            .unwrap()
            .insert(uri.to_owned(), content.to_owned());
        Ok(())
    }

    async fn write_bytes(&self, uri: &str, bytes: &[u8]) -> Result<(), BoxError> {
        self.write(uri, &String::from_utf8_lossy(bytes)).await
    }

    async fn mkdir(&self, _: &str) -> Result<(), BoxError> {
        Ok(())
    }

    async fn ls(&self, _: &str) -> Result<Vec<FsEntry>, BoxError> {
        Ok(Vec::new())
    }

    async fn rm(&self, uri: &str) -> Result<(), BoxError> {
        self.files.lock().unwrap().remove(uri);
        Ok(())
    }

    async fn mv(&self, from: &str, to: &str) -> Result<(), BoxError> {
        if let Some(content) = self.files.lock().unwrap().remove(from) {
            self.files.lock().unwrap().insert(to.to_owned(), content);
        }
        Ok(())
    }

    async fn stat(&self, _: &str) -> Result<FsStat, BoxError> {
        Err("stat not implemented in test fs".into())
    }

    async fn grep(&self, _: &str, _: &str, _: bool, _: bool) -> Result<Vec<GrepMatch>, BoxError> {
        Ok(Vec::new())
    }

    async fn exists(&self, uri: &str) -> Result<bool, BoxError> {
        Ok(self.files.lock().unwrap().contains_key(uri))
    }

    async fn append(&self, uri: &str, content: &str) -> Result<(), BoxError> {
        self.files
            .lock()
            .unwrap()
            .entry(uri.to_owned())
            .and_modify(|existing| existing.push_str(content))
            .or_insert_with(|| content.to_owned());
        Ok(())
    }

    async fn link(&self, from: &str, to: &str) -> Result<(), BoxError> {
        let relations_uri = format!("{}/.relations", from.trim_end_matches(".md"));
        self.append(&relations_uri, &format!("{to}\n")).await
    }
}

#[derive(Clone)]
struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _: &str,
        _: &[String],
        _: usize,
    ) -> Result<Vec<acosmi_memory_session::RerankResult>, BoxError> {
        Ok(Vec::new())
    }
}

#[test]
fn vector_store_adapter_phase1_config_uses_dim1_zero_vector_contract() {
    let cfg = phase1_collection_config();

    assert_eq!(cfg.dimension, 1);
    assert_eq!(cfg.distance, Distance::Cosine);
    assert!(!cfg.sparse_vectors);
    assert!(cfg.hnsw.is_none());
    assert!(cfg.quantization.is_none());
    assert_eq!(cfg.storage_type, VectorStorageType::InRamChunkedMmap);
    assert!(cfg.datatype.is_none());
}

#[tokio::test]
async fn vector_store_adapter_upsert_writes_zero_vector_payload_to_search_engine() {
    let (_dir, engine, adapter) = test_adapter();
    let id = uuid(1);

    adapter
        .upsert(
            COLLECTION,
            &id,
            &[0.0_f32],
            fields(&[
                ("scope", json!("private")),
                ("bucket", json!("even")),
                ("path", json!("projects/rust")),
            ]),
        )
        .await
        .unwrap();

    let hits = engine.scroll(COLLECTION, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);
    assert_eq!(hits[0].payload.get("scope"), Some(&json!("private")));
    assert_eq!(hits[0].payload.get("path"), Some(&json!("projects/rust")));
}

#[tokio::test]
async fn vector_store_adapter_delete_removes_record_through_search_engine() {
    let (_dir, engine, adapter) = test_adapter();
    let id = uuid(2);

    adapter
        .upsert(
            COLLECTION,
            &id,
            &[0.0_f32],
            fields(&[("bucket", json!("odd"))]),
        )
        .await
        .unwrap();
    assert_eq!(engine.point_count(COLLECTION), Some(1));

    adapter.delete(COLLECTION, &id).await.unwrap();

    assert_eq!(engine.point_count(COLLECTION), Some(0));
    assert!(engine.scroll(COLLECTION, 10).unwrap().is_empty());
}

#[test]
fn vector_store_adapter_filter_map_to_filter_json_accepts_real_qdrant_filter_shape() {
    let filter = qdrant_bucket_filter("even");

    let filter_json = filter_map_to_filter_json(&filter).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&filter_json).unwrap();

    assert_eq!(parsed["must"][0]["key"], json!("bucket"));
    assert_eq!(parsed["must"][0]["match"]["value"], json!("even"));
    assert!(filter_map_to_search_filter(&filter).is_ok());
}

#[test]
fn vector_store_adapter_filter_map_to_filter_json_accepts_simple_session_equality_filter() {
    let filter = fields(&[("context_type", json!("memory")), ("is_leaf", json!(true))]);

    let filter_json = filter_map_to_filter_json(&filter).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&filter_json).unwrap();
    let must = parsed["must"].as_array().unwrap();

    assert!(must
        .iter()
        .any(|condition| condition["key"] == json!("context_type")
            && condition["match"]["value"] == json!("memory")));
    assert!(must
        .iter()
        .any(|condition| condition["key"] == json!("is_leaf")
            && condition["match"]["value"] == json!(true)));
    assert!(filter_map_to_search_filter(&filter).is_ok());
}

#[tokio::test]
async fn vector_store_adapter_filter_json_output_is_accepted_by_scroll_filtered() {
    let (_dir, engine, adapter) = test_adapter();

    adapter
        .upsert(
            COLLECTION,
            &uuid(3),
            &[0.0_f32],
            fields(&[("bucket", json!("even"))]),
        )
        .await
        .unwrap();
    adapter
        .upsert(
            COLLECTION,
            &uuid(4),
            &[0.0_f32],
            fields(&[("bucket", json!("odd"))]),
        )
        .await
        .unwrap();

    let filter_json = filter_map_to_filter_json(&qdrant_bucket_filter("even")).unwrap();
    let hits = engine
        .scroll_filtered(COLLECTION, &filter_json, 10)
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.get("bucket"), Some(&json!("even")));
}

#[test]
fn vector_store_adapter_filter_map_rejects_session_mock_filter_dsl() {
    let filter = fields(&[(
        "must",
        json!({
            "field": "bucket",
            "conds": ["even"]
        }),
    )]);

    let err = filter_map_to_filter_json(&filter).unwrap_err().to_string();
    assert!(err.contains("SearchEngine/Qdrant Filter JSON shape"));
}

#[tokio::test]
async fn vector_store_adapter_search_uses_real_dense_vectors_and_simple_filter() {
    let collection = "phase3_adapter";
    let (_dir, _engine, adapter) = test_vector_adapter(collection, 3);
    let memory_id = uuid(6);
    let other_id = uuid(7);

    let info = adapter
        .get_collection_info(collection)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.vector_dim, 3);
    assert_eq!(info.count, 0);

    adapter
        .upsert(
            collection,
            &memory_id,
            &[1.0_f32, 0.0, 0.0],
            fields(&[
                ("context_type", json!("memory")),
                ("is_leaf", json!(true)),
                ("title", json!("closest")),
            ]),
        )
        .await
        .unwrap();
    adapter
        .upsert(
            collection,
            &other_id,
            &[0.0_f32, 1.0, 0.0],
            fields(&[
                ("context_type", json!("project")),
                ("is_leaf", json!(true)),
                ("title", json!("filtered out")),
            ]),
        )
        .await
        .unwrap();

    let filter = fields(&[("context_type", json!("memory")), ("is_leaf", json!(true))]);

    let hits = adapter
        .search(collection, &[0.95_f32, 0.05, 0.0], None, 10, Some(&filter))
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, memory_id);
    assert!(hits[0].score.is_finite());
    assert_eq!(hits[0].fields.get("title"), Some(&json!("closest")));
}

#[tokio::test]
async fn vector_store_adapter_update_merges_payload_fields() {
    let (_dir, engine, adapter) = test_adapter();
    let id = uuid(5);

    adapter
        .upsert(
            COLLECTION,
            &id,
            &[0.0_f32],
            fields(&[("bucket", json!("even"))]),
        )
        .await
        .unwrap();

    adapter
        .update(COLLECTION, &id, fields(&[("title", json!("updated"))]))
        .await
        .unwrap();

    let hits = engine.scroll(COLLECTION, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.get("bucket"), Some(&json!("even")));
    assert_eq!(hits[0].payload.get("title"), Some(&json!("updated")));
}

#[tokio::test]
async fn vector_store_adapter_round_trips_external_uri_ids_for_session_consumers() {
    let collection = "phase3_uri_adapter";
    let (_dir, _engine, adapter) = test_vector_adapter(collection, 3);
    let id = "viking://context/memory/rust";

    adapter
        .upsert(
            collection,
            id,
            &[1.0_f32, 0.0, 0.0],
            fields(&[("context_type", json!("memory")), ("is_leaf", json!(true))]),
        )
        .await
        .unwrap();

    let hits = adapter
        .search(collection, &[1.0_f32, 0.0, 0.0], None, 1, None)
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);
    assert_eq!(hits[0].fields.get("id"), Some(&json!(id)));
    assert_eq!(hits[0].fields.get("uri"), Some(&json!(id)));
}

#[tokio::test]
async fn vector_store_adapter_search_rejects_phase1_zero_query_with_scroll_hint() {
    let (_dir, _engine, adapter) = test_adapter();

    let err = adapter
        .search(
            COLLECTION,
            &[0.0_f32],
            None,
            10,
            Some(&qdrant_bucket_filter("even")),
        )
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("non-zero Phase 3 dense vector"));
    assert!(err.contains("scroll_filtered"));
}

#[tokio::test]
async fn vector_store_adapter_scroll_paginates_and_filters_without_vector_search() {
    let collection = "phase3_scroll_adapter";
    let (_dir, _engine, adapter) = test_vector_adapter(collection, 3);

    for index in 0..5 {
        adapter
            .upsert(
                collection,
                &format!("viking://scroll/{index}"),
                &[0.0_f32, 0.0, 0.0],
                fields(&[("bucket", json!(if index % 2 == 0 { "even" } else { "odd" }))]),
            )
            .await
            .unwrap();
    }

    let filter = fields(&[("bucket", json!("even"))]);
    let first_page = adapter
        .scroll(collection, Some(&filter), 2, None, None)
        .await
        .unwrap();
    assert_eq!(first_page.records.len(), 2);
    assert!(first_page.next_cursor.is_some());

    let second_page = adapter
        .scroll(
            collection,
            Some(&filter),
            2,
            first_page.next_cursor.as_deref(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(second_page.records.len(), 1);
    assert!(second_page.next_cursor.is_none());
    assert!(second_page
        .records
        .iter()
        .all(|record| record.get("bucket") == Some(&json!("even"))));
}

#[tokio::test]
async fn vector_store_adapter_search_full_and_remove_by_uri_are_available() {
    let collection = "phase3_full_adapter";
    let (_dir, _engine, adapter) = test_vector_adapter(collection, 3);
    let parent = "viking://docs/readme";
    let child = "viking://docs/readme/section";
    let other = "viking://docs/other";

    for id in [parent, child, other] {
        adapter
            .upsert(
                collection,
                id,
                &[1.0_f32, 0.0, 0.0],
                fields(&[("kind", json!("doc"))]),
            )
            .await
            .unwrap();
    }

    let results = adapter
        .search_full(
            collection,
            Some(&[1.0_f32, 0.0, 0.0]),
            None,
            Some(&fields(&[("kind", json!("doc"))])),
            2,
            1,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);

    let removed = adapter.remove_by_uri(collection, parent).await.unwrap();
    assert_eq!(removed, 2);
    assert_eq!(adapter.count(collection, None).await.unwrap(), 1);
}

#[tokio::test]
async fn vector_store_adapter_b7_memory_deduplicator_uses_search_engine_vector_store() {
    let (_dir, _engine, adapter) = test_vector_adapter("context", 3);
    let existing_uri = "viking://user/memories/preferences/rust.md";

    adapter
        .upsert(
            "context",
            existing_uri,
            &[1.0_f32, 0.0, 0.0],
            fields(&[
                ("uri", json!(existing_uri)),
                ("context_type", json!("memory")),
                ("is_leaf", json!(true)),
                ("abstract", json!("Rust testing preference")),
                ("overview", json!("Prefer deterministic Rust tests")),
                (
                    "content",
                    json!("The user prefers deterministic Rust tests."),
                ),
            ]),
        )
        .await
        .unwrap();

    let deduplicator = MemoryDeduplicator::new(
        adapter,
        FakeEmbedder,
        StaticLlm::new(r#"{"decision":"skip","reason":"same preference","list":[]}"#),
    );
    let candidate = CandidateMemory {
        category: MemoryCategory::Preferences,
        abstract_text: "Rust testing preference".to_owned(),
        overview: "Prefer deterministic Rust tests".to_owned(),
        content: "The user prefers deterministic Rust tests.".to_owned(),
        source_session: "s1".to_owned(),
        user: "default".to_owned(),
        language: "en".to_owned(),
    };

    let result = deduplicator.deduplicate(&candidate).await.unwrap();
    assert_eq!(result.decision, DedupDecision::Skip);
    assert_eq!(result.similar_memories, vec![existing_uri.to_owned()]);
}

#[tokio::test]
async fn vector_store_adapter_b7_hierarchical_retriever_uses_search_engine_vector_store() {
    let (_dir, _engine, adapter) = test_vector_adapter("context", 3);
    let parent = "viking://user/memories/preferences";
    let leaf = "viking://user/memories/preferences/rust.md";

    adapter
        .upsert(
            "context",
            leaf,
            &[1.0_f32, 0.0, 0.0],
            fields(&[
                ("uri", json!(leaf)),
                ("context_type", json!("memory")),
                ("is_leaf", json!(true)),
                ("parent_uri", json!(parent)),
                ("abstract", json!("Rust preference")),
                ("category", json!("preferences")),
            ]),
        )
        .await
        .unwrap();

    let retriever: HierarchicalRetriever<_, _, _, NoopReranker> =
        HierarchicalRetriever::new(adapter, FakeEmbedder, MemoryFs::default(), 0.0);
    let query = TypedQuery {
        query: "rust preference".to_owned(),
        context_type: RetrieveContextType::Memory,
        intent: "find memories".to_owned(),
        priority: 1,
        target_directories: vec![parent.to_owned()],
    };

    let result = retriever.retrieve(&query, 5, None).await.unwrap();
    assert_eq!(result.matched_contexts.len(), 1);
    assert_eq!(result.matched_contexts[0].uri, leaf);
}

#[tokio::test]
async fn vector_store_adapter_b7_session_compressor_indexes_created_memory_with_search_engine_vector_store(
) {
    let (_dir, _engine, adapter) = test_vector_adapter("context", 3);
    let fs = MemoryFs::default();
    let llm = StaticLlm::new(
        r#"{"memories":[{"category":"preferences","abstract":"Rust testing preference","overview":"Prefer deterministic Rust tests","content":"The user prefers deterministic Rust tests."}]}"#,
    );
    let compressor = SessionCompressor::new(adapter.clone(), FakeEmbedder, llm, fs);
    let messages = vec![Message::create_user(
        "Please remember that I prefer deterministic Rust tests.",
    )];

    let memories = compressor
        .extract_long_term_memories(&messages, &UserIdentifier::default_user(), "session-1")
        .await
        .unwrap();

    assert_eq!(memories.len(), 1);

    let hits = adapter
        .search(
            "context",
            &[1.0_f32, 0.0, 0.0],
            None,
            5,
            Some(&fields(&[("context_type", json!("memory"))])),
        )
        .await
        .unwrap();

    assert!(hits.iter().any(|hit| hit.id == memories[0].uri));
}
