use std::sync::atomic::AtomicBool;

use acosmi_memory_se::segment_store::{CollectionConfig, SearchEngine, VectorIndexIssueKind};
use acosmi_memory_se::{Distance, HnswConfig, Payload, VectorStorageType};
use serde_json::json;
use tempfile::TempDir;

fn vector_config(dimension: usize, hnsw: bool) -> CollectionConfig {
    CollectionConfig {
        dimension,
        distance: Distance::Cosine,
        sparse_vectors: false,
        hnsw: hnsw.then_some(HnswConfig {
            m: 16,
            ef_construct: 100,
            full_scan_threshold: 10_000,
            max_indexing_threads: 0,
            on_disk: None,
            payload_m: None,
            inline_storage: None,
        }),
        quantization: None,
        storage_type: VectorStorageType::InRamChunkedMmap,
        datatype: None,
    }
}

fn payload(value: serde_json::Value) -> Payload {
    Payload::from(
        value
            .as_object()
            .expect("payload must be an object")
            .clone(),
    )
}

fn point_id(index: usize) -> String {
    format!("00000000-0000-0000-0003-{index:012x}")
}

fn hit_ids(hits: &[acosmi_memory_se::segment_store::SearchHit]) -> Vec<String> {
    hits.iter().map(|hit| hit.id.clone()).collect()
}

#[test]
fn multidimensional_vectors_search_by_cosine_similarity() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_vectors";

    engine
        .create_collection(collection, &vector_config(3, false))
        .unwrap();

    let rust_id = point_id(1);
    let music_id = point_id(2);
    let mixed_id = point_id(3);

    engine
        .upsert(
            collection,
            &rust_id,
            &[0.98, 0.02, 0.0],
            Some(&payload(json!({"topic": "rust", "kind": "memory"}))),
        )
        .unwrap();
    engine
        .upsert(
            collection,
            &music_id,
            &[0.0, 1.0, 0.0],
            Some(&payload(json!({"topic": "music", "kind": "memory"}))),
        )
        .unwrap();
    engine
        .upsert(
            collection,
            &mixed_id,
            &[0.75, 0.25, 0.0],
            Some(&payload(json!({"topic": "mixed", "kind": "memory"}))),
        )
        .unwrap();

    let hits = engine
        .search(collection, &[1.0, 0.0, 0.0], None, 3, None)
        .unwrap();

    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, rust_id);
    assert_eq!(hits[0].payload.get("topic"), Some(&json!("rust")));
    assert!(hits.iter().all(|hit| hit.score.is_finite()));
    assert!(hits[0].score >= hits[1].score);
    assert!(hits[1].score >= hits[2].score);
}

#[test]
fn optimize_collection_preserves_vector_search_results() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_hnsw";
    let stopped = AtomicBool::new(false);

    engine
        .create_collection(collection, &vector_config(4, true))
        .unwrap();

    let vectors = [
        [1.0, 0.0, 0.0, 0.0],
        [0.95, 0.05, 0.0, 0.0],
        [0.90, 0.10, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    for (index, vector) in vectors.iter().enumerate() {
        engine
            .upsert(
                collection,
                &point_id(index),
                vector,
                Some(&payload(json!({"idx": index}))),
            )
            .unwrap();
    }

    let query = [1.0, 0.0, 0.0, 0.0];
    let before = engine.search(collection, &query, None, 3, None).unwrap();

    assert_eq!(
        hit_ids(&before),
        vec![point_id(0), point_id(1), point_id(2)]
    );

    assert!(engine.optimize_collection(collection, &stopped).unwrap());
    assert_eq!(engine.point_count(collection), Some(vectors.len()));

    let after = engine.search(collection, &query, None, 3, None).unwrap();
    assert_eq!(hit_ids(&after), hit_ids(&before));
    assert!(after.iter().all(|hit| hit.score.is_finite()));

    assert!(engine.optimize_collection(collection, &stopped).unwrap());
    let refreshed = engine.search(collection, &query, None, 3, None).unwrap();
    assert_eq!(hit_ids(&refreshed), hit_ids(&before));
}

#[test]
fn zero_vector_upsert_remains_scroll_only_compatible_but_search_rejects_it() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase1_scroll_placeholder";

    engine
        .create_collection(collection, &vector_config(1, false))
        .unwrap();
    engine
        .upsert(
            collection,
            &point_id(100),
            &[0.0_f32],
            Some(&payload(json!({"phase": "phase1"}))),
        )
        .unwrap();

    let scrolled = engine.scroll(collection, 10).unwrap();
    assert_eq!(scrolled.len(), 1);
    assert_eq!(scrolled[0].payload.get("phase"), Some(&json!("phase1")));

    let err = engine
        .search(collection, &[0.0_f32], None, 1, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("must not be all-zero"));
    assert!(err.contains("scroll_filtered"));
}

#[test]
fn search_rejects_empty_zero_nonfinite_and_dimension_mismatch_queries() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_invalid_queries";

    engine
        .create_collection(collection, &vector_config(3, false))
        .unwrap();
    engine
        .upsert(collection, &point_id(200), &[1.0, 0.0, 0.0], None)
        .unwrap();

    let empty = engine
        .search(collection, &[], None, 1, None)
        .unwrap_err()
        .to_string();
    assert!(empty.contains("must not be empty"));

    let zero = engine
        .search(collection, &[0.0, 0.0, 0.0], None, 1, None)
        .unwrap_err()
        .to_string();
    assert!(zero.contains("must not be all-zero"));

    let nonfinite = engine
        .search(collection, &[1.0, f32::NAN, 0.0], None, 1, None)
        .unwrap_err()
        .to_string();
    assert!(nonfinite.contains("finite"));

    let mismatch = engine
        .search(collection, &[1.0, 0.0], None, 1, None)
        .unwrap_err()
        .to_string();
    assert!(mismatch.contains("dimension mismatch"));
}

#[test]
fn upsert_rejects_empty_nonfinite_and_dimension_mismatch_vectors() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_invalid_upserts";

    engine
        .create_collection(collection, &vector_config(3, false))
        .unwrap();

    let empty = engine
        .upsert(collection, &point_id(300), &[], None)
        .unwrap_err()
        .to_string();
    assert!(empty.contains("must not be empty"));

    let nonfinite = engine
        .upsert(collection, &point_id(301), &[1.0, f32::INFINITY, 0.0], None)
        .unwrap_err()
        .to_string();
    assert!(nonfinite.contains("finite"));

    let mismatch = engine
        .upsert(collection, &point_id(302), &[1.0, 0.0], None)
        .unwrap_err()
        .to_string();
    assert!(mismatch.contains("dimension mismatch"));
}

#[test]
fn create_collection_rejects_zero_dimension() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();

    let err = engine
        .create_collection("bad_dim", &vector_config(0, false))
        .unwrap_err()
        .to_string();

    assert!(err.contains("dimension must be greater than 0"));
}

#[test]
fn vector_index_verify_reports_healthy_explicit_collection() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_verify_healthy";

    engine
        .create_collection(collection, &vector_config(3, false))
        .unwrap();
    engine
        .upsert(
            collection,
            &point_id(400),
            &[1.0, 0.0, 0.0],
            Some(&payload(json!({"kind": "explicit"}))),
        )
        .unwrap();

    let report = engine.verify_vector_index(collection, Some(3));

    assert!(report.ok(), "unexpected issues: {:#?}", report.issues);
    assert_eq!(report.actual_dimension, Some(3));
    assert_eq!(report.point_count, 1);
    assert_eq!(report.checked_points, 1);
}

#[test]
fn vector_index_verify_detects_collection_dimension_mismatch() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_verify_dim_mismatch";

    engine
        .create_collection(collection, &vector_config(3, false))
        .unwrap();

    let report = engine.verify_vector_index(collection, Some(4));

    assert!(!report.ok());
    assert!(report.issues.iter().any(|issue| {
        issue.kind == VectorIndexIssueKind::DimensionMismatch
            && issue.expected_dimension == Some(4)
            && issue.actual_dimension == Some(3)
    }));
}

#[test]
fn vector_index_repair_rebuilds_missing_hnsw_files_without_touching_payloads() {
    let temp_dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = "phase3_repair_hnsw";
    let stopped = AtomicBool::new(false);

    engine
        .create_collection(collection, &vector_config(4, true))
        .unwrap();

    for index in 0..8 {
        let mut vector = [0.0_f32; 4];
        vector[index % 4] = 1.0;
        engine
            .upsert(
                collection,
                &point_id(500 + index),
                &vector,
                Some(&payload(json!({"idx": index, "truth": "payload"}))),
            )
            .unwrap();
    }

    assert!(engine.optimize_collection(collection, &stopped).unwrap());

    let old_segment_path = engine.active_segment_path(collection).unwrap();
    let old_index_path = old_segment_path.join("vector_index");
    assert!(old_index_path.exists());
    remove_path(&old_index_path);

    let broken = engine.verify_vector_index(collection, Some(4));
    assert!(broken.issues.iter().any(|issue| {
        matches!(
            issue.kind,
            VectorIndexIssueKind::VectorIndexMissing
                | VectorIndexIssueKind::SegmentLoadFailed
                | VectorIndexIssueKind::VectorSearchFailed
        )
    }));

    let repair = engine.repair_vector_index(collection, Some(4));

    assert!(
        repair.rebuild.is_some(),
        "repair did not attempt rebuild: {:#?}",
        repair
    );
    assert!(
        repair.after.ok(),
        "remaining issues: {:#?}",
        repair.after.issues
    );
    assert!(repair.repaired);

    let rebuild = repair.rebuild.unwrap();
    assert_eq!(rebuild.point_count, 8);
    assert_eq!(rebuild.old_segment_path, old_segment_path);
    assert_ne!(rebuild.new_segment_path, rebuild.old_segment_path);

    let hits = engine
        .search(collection, &[1.0, 0.0, 0.0, 0.0], None, 4, None)
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits
        .iter()
        .all(|hit| hit.payload.get("truth") == Some(&json!("payload"))));
}

fn remove_path(path: &std::path::Path) {
    if path.is_dir() {
        std::fs::remove_dir_all(path).unwrap();
    } else {
        std::fs::remove_file(path).unwrap();
    }
}
