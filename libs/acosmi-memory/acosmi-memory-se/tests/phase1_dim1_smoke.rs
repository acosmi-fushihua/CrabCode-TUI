use acosmi_memory_se::segment_store::{CollectionConfig, SearchEngine};
use acosmi_memory_se::{Distance, Payload, PayloadSchemaType, VectorStorageType};
use serde_json::json;
use tempfile::TempDir;

fn phase1_collection_config(dimension: usize) -> CollectionConfig {
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

fn make_payload(value: serde_json::Value) -> Payload {
    Payload::from(
        value
            .as_object()
            .expect("payload must be an object")
            .clone(),
    )
}

fn point_id(dimension: usize, index: usize) -> String {
    format!("00000000-0000-0000-{:04x}-{:012x}", dimension, index)
}

fn run_zero_vector_smoke(dimension: usize) {
    let temp_dir = TempDir::new().unwrap();
    let store = SearchEngine::new(temp_dir.path()).unwrap();
    let collection = format!("phase1_zero_vector_dim_{dimension}");

    assert!(store
        .create_collection(&collection, &phase1_collection_config(dimension))
        .unwrap());

    let zero_vector = vec![0.0_f32; dimension];
    let ids: Vec<String> = (0..100).map(|index| point_id(dimension, index)).collect();

    for (index, id) in ids.iter().enumerate() {
        let payload = make_payload(json!({
            "phase": "phase1",
            "bucket": if index % 2 == 0 { "even" } else { "odd" },
            "dimension": dimension,
            "index": index,
        }));
        store
            .upsert(&collection, id, &zero_vector, Some(&payload))
            .unwrap();
    }

    assert_eq!(store.point_count(&collection), Some(100));

    let scrolled = store.scroll(&collection, 128).unwrap();
    assert_eq!(scrolled.len(), 100);

    assert!(store
        .create_field_index(&collection, "bucket", PayloadSchemaType::Keyword)
        .unwrap());

    let even_filter = r#"{"must":[{"key":"bucket","match":{"value":"even"}}]}"#;
    let even_hits = store
        .scroll_filtered(&collection, even_filter, 128)
        .unwrap();
    assert_eq!(even_hits.len(), 50);
    assert!(even_hits
        .iter()
        .all(|hit| hit.payload.get("bucket") == Some(&json!("even"))));

    for id in &ids {
        assert!(store.delete(&collection, id).unwrap());
    }

    assert_eq!(store.point_count(&collection), Some(0));
    assert!(store.scroll(&collection, 128).unwrap().is_empty());
    assert!(store
        .scroll_filtered(&collection, even_filter, 128)
        .unwrap()
        .is_empty());
}

#[test]
fn phase1_dim1_smoke() {
    run_zero_vector_smoke(1);
}

#[test]
fn phase1_dim2_zero_vector_control() {
    run_zero_vector_smoke(2);
}

#[test]
fn phase1_dim8_zero_vector_control() {
    run_zero_vector_smoke(8);
}
