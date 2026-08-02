use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use acosmi_memory_adapter::scoped_path_to_point_id;
use acosmi_memory_se::indexer::{
    ensure_memory_topic_collection, index_memory_roots, IndexSkipReason, MemoryRoot,
};
use acosmi_memory_se::segment_store::SearchEngine;
use acosmi_memory_se::PayloadSchemaType;
use serde_json::{json, Value};
use tempfile::TempDir;

const COLLECTION: &str = "phase1_topic_memory";

fn test_engine() -> (TempDir, Arc<SearchEngine>) {
    let dir = TempDir::new().unwrap();
    let engine = Arc::new(SearchEngine::new(dir.path()).unwrap());
    ensure_memory_topic_collection(&engine, COLLECTION).unwrap();
    (dir, engine)
}

fn phase1_roots(memory_root: &Path) -> Vec<MemoryRoot> {
    vec![
        MemoryRoot::private(memory_root),
        MemoryRoot::team(memory_root.join("team")),
    ]
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn memory_doc(type_: &str, name: Option<&str>, body: &str) -> String {
    let name_line = name.map_or(String::new(), |name| format!("name: {name}\n"));
    format!("---\n{name_line}type: {type_}\ndescription: {type_} description\n---\n{body}\n")
}

fn scope_filter(scope: &str) -> String {
    json!({
        "must": [{
            "key": "scope",
            "match": { "value": scope }
        }]
    })
    .to_string()
}

fn hits_by_relative_path(
    hits: Vec<acosmi_memory_se::segment_store::ScrollHit>,
) -> HashMap<String, acosmi_memory_se::segment_store::ScrollHit> {
    hits.into_iter()
        .map(|hit| {
            let relative_path = hit
                .payload
                .get("relative_path")
                .and_then(Value::as_str)
                .unwrap()
                .to_owned();
            (relative_path, hit)
        })
        .collect()
}

#[test]
fn phase1_indexer_indexes_private_and_team_roots_with_distinct_payloads() {
    let (_engine_dir, engine) = test_engine();
    let memory_dir = TempDir::new().unwrap();

    write_file(
        &memory_dir.path().join("feedback.md"),
        &memory_doc(
            "feedback",
            Some("Private Feedback"),
            "private feedback body",
        ),
    );
    write_file(
        &memory_dir.path().join("nested/project.md"),
        &memory_doc("project", None, "private project body"),
    );
    write_file(
        &memory_dir.path().join("team/feedback.md"),
        &memory_doc("feedback", Some("Team Feedback"), "team feedback body"),
    );
    write_file(
        &memory_dir.path().join("team/reference.md"),
        &memory_doc("reference", Some("Team Reference"), "team reference body"),
    );
    write_file(&memory_dir.path().join("MEMORY.md"), "# private index\n");
    write_file(&memory_dir.path().join("SESSION.md"), "# session scratch\n");
    write_file(
        &memory_dir.path().join("logs/2026/05/2026-05-06.md"),
        "daily log without frontmatter\n",
    );
    write_file(
        &memory_dir.path().join(".rust-derived/ghost.md"),
        &memory_doc("project", Some("Derived Ghost"), "legacy derived body"),
    );
    write_file(&memory_dir.path().join("plain.md"), "plain body\n");
    write_file(
        &memory_dir.path().join("missing_type.md"),
        "---\ndescription: no type\n---\nmissing type body\n",
    );
    write_file(
        &memory_dir.path().join("invalid_type.md"),
        "---\ntype: unknown\ndescription: invalid type\n---\ninvalid type body\n",
    );

    let stats = index_memory_roots(
        &engine,
        &phase1_roots(memory_dir.path()),
        "project-a",
        COLLECTION,
    )
    .unwrap();

    assert_eq!(stats.indexed, 4);
    assert_eq!(stats.skipped_team_from_private, 2);
    assert_eq!(stats.skipped_logs, 1);
    assert_eq!(stats.skipped_legacy_rust_derived, 1);
    assert_eq!(stats.skipped_basename, 2);
    assert_eq!(stats.skipped_missing_frontmatter, 1);
    assert_eq!(stats.skipped_missing_type, 1);
    assert_eq!(stats.skipped_invalid_type, 1);
    assert!(stats.errors.is_empty());

    assert!(stats
        .skip_reasons
        .iter()
        .any(|skip| skip.reason == IndexSkipReason::LogFile));
    assert!(stats
        .skip_reasons
        .iter()
        .any(|skip| skip.reason == IndexSkipReason::LegacyRustDerived));

    engine
        .create_field_index(COLLECTION, "scope", PayloadSchemaType::Keyword)
        .unwrap();

    let private_hits = engine
        .scroll_filtered(COLLECTION, &scope_filter("private"), 16)
        .unwrap();
    let team_hits = engine
        .scroll_filtered(COLLECTION, &scope_filter("team"), 16)
        .unwrap();

    assert_eq!(private_hits.len(), 2);
    assert_eq!(team_hits.len(), 2);

    let private_hits = hits_by_relative_path(private_hits);
    let team_hits = hits_by_relative_path(team_hits);

    let private_feedback = private_hits.get("feedback.md").unwrap();
    let team_feedback = team_hits.get("feedback.md").unwrap();
    assert_ne!(private_feedback.id, team_feedback.id);
    assert_eq!(
        private_feedback.id,
        scoped_path_to_point_id("private", "feedback")
    );
    assert_eq!(
        team_feedback.id,
        scoped_path_to_point_id("team", "feedback")
    );

    assert_eq!(
        private_feedback.payload.get("scope"),
        Some(&json!("private"))
    );
    assert_eq!(
        private_feedback.payload.get("category"),
        Some(&json!("preferences"))
    );
    assert_eq!(
        private_feedback.payload.get("type"),
        Some(&json!("feedback"))
    );
    assert_eq!(
        private_feedback.payload.get("name"),
        Some(&json!("Private Feedback"))
    );
    assert_eq!(
        private_feedback.payload.get("relative_path_no_ext"),
        Some(&json!("feedback"))
    );
    assert!(private_feedback
        .payload
        .get("mtime_ms")
        .is_some_and(Value::is_number));
    assert!(private_feedback
        .payload
        .get("source_path")
        .and_then(Value::as_str)
        .is_some_and(|path| path.ends_with("feedback.md")));

    let nested_project = private_hits.get("nested/project.md").unwrap();
    assert_eq!(nested_project.payload.get("name"), Some(&json!("project")));
    assert_eq!(
        nested_project.payload.get("relative_path_no_ext"),
        Some(&json!("nested/project"))
    );

    let all_hits = engine.scroll(COLLECTION, 16).unwrap();
    assert_eq!(all_hits.len(), 4);
    assert!(!all_hits.iter().any(|hit| {
        hit.payload
            .get("relative_path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.starts_with("logs/") || path.starts_with(".rust-derived/"))
    }));
    assert!(!all_hits.iter().any(|hit| {
        hit.payload.get("scope") == Some(&json!("private"))
            && hit.payload.get("relative_path") == Some(&json!("team/feedback.md"))
    }));
}

#[test]
fn phase1_indexer_aggregates_file_errors_and_continues() {
    let (_engine_dir, engine) = test_engine();
    let memory_dir = TempDir::new().unwrap();

    write_file(
        &memory_dir.path().join("valid.md"),
        &memory_doc("user", Some("Valid Memory"), "valid body"),
    );
    write_file(
        &memory_dir.path().join("bad_yaml.md"),
        "---\ntype: project\n- [\n---\nbody\n",
    );

    let stats = index_memory_roots(
        &engine,
        &[MemoryRoot::private(memory_dir.path())],
        "project-a",
        COLLECTION,
    )
    .unwrap();

    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.errors.len(), 1);
    assert!(stats.errors[0]
        .path
        .to_string_lossy()
        .ends_with("bad_yaml.md"));
    assert_eq!(engine.point_count(COLLECTION), Some(1));
}
