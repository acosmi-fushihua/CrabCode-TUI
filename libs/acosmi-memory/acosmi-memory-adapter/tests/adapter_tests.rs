use std::collections::HashMap;

use acosmi_memory_adapter::{
    fields_to_payload, frontmatter_ts_compat, map_type, parse_ts_memory, scoped_path_to_point_id,
    truncate_overview,
};
use acosmi_memory_core::session_types::MemoryCategory;
use pretty_assertions::assert_eq;
use serde_json::json;
use uuid::Uuid;

fn markdown(type_: &str, body: &str) -> String {
    format!("---\nname: Test Memory\ndescription: {type_} description\ntype: {type_}\n---\n{body}")
}

#[test]
fn map_user_to_profile() {
    assert_eq!(map_type("user"), Some(MemoryCategory::Profile));
}

#[test]
fn map_feedback_to_preferences() {
    assert_eq!(map_type("feedback"), Some(MemoryCategory::Preferences));
}

#[test]
fn map_project_to_events() {
    assert_eq!(map_type("project"), Some(MemoryCategory::Events));
}

#[test]
fn map_reference_to_entities() {
    assert_eq!(map_type("reference"), Some(MemoryCategory::Entities));
}

#[test]
fn invalid_type_maps_to_none_not_patterns() {
    assert_eq!(map_type("fact"), None);
    assert_eq!(map_type("patterns"), None);
}

#[test]
fn parse_valid_user_memory_builds_candidate() {
    let parsed = parse_ts_memory(
        "/tmp/user-profile.md",
        &markdown("user", "body"),
        "project-a",
    )
    .unwrap();
    let candidate = parsed.candidate.unwrap();

    assert_eq!(candidate.category, MemoryCategory::Profile);
    assert_eq!(candidate.abstract_text, "user description");
    assert_eq!(candidate.overview, "body");
    assert_eq!(candidate.content, "body");
    assert_eq!(candidate.source_session, "");
    assert_eq!(candidate.user, "project-a");
    assert_eq!(candidate.language, "auto");
    assert_eq!(parsed.name_hint.as_deref(), Some("Test Memory"));
    assert_eq!(parsed.file_stem, "user-profile");
    assert!(parsed.has_frontmatter);
    assert_eq!(parsed.raw_type.as_deref(), Some("user"));
}

#[test]
fn parse_valid_feedback_memory_builds_candidate() {
    let parsed = parse_ts_memory(
        "/tmp/feedback.md",
        &markdown("feedback", "body"),
        "project-a",
    )
    .unwrap();
    assert_eq!(
        parsed.candidate.unwrap().category,
        MemoryCategory::Preferences
    );
}

#[test]
fn parse_valid_project_memory_builds_candidate() {
    let parsed =
        parse_ts_memory("/tmp/project.md", &markdown("project", "body"), "project-a").unwrap();
    assert_eq!(parsed.candidate.unwrap().category, MemoryCategory::Events);
}

#[test]
fn parse_valid_reference_memory_builds_candidate() {
    let parsed = parse_ts_memory(
        "/tmp/reference.md",
        &markdown("reference", "body"),
        "project-a",
    )
    .unwrap();
    assert_eq!(parsed.candidate.unwrap().category, MemoryCategory::Entities);
}

#[test]
fn frontmatter_missing_is_not_fatal() {
    let parsed = parse_ts_memory("/tmp/plain.md", "plain markdown body", "project-a").unwrap();

    assert!(!parsed.has_frontmatter);
    assert_eq!(parsed.raw_type, None);
    assert_eq!(parsed.name_hint, None);
    assert!(parsed.candidate.is_none());
    assert_eq!(parsed.body, "plain markdown body");
}

#[test]
fn quote_fallback_parses_ts_problematic_values() {
    let content =
        "---\nname: Memory: with colon\ndescription: Glob **/*.{ts,tsx} & !node_modules\ntype: feedback\n---\nbody";

    let parsed = parse_ts_memory("/tmp/problem.md", content, "project-a").unwrap();

    assert_eq!(parsed.name_hint.as_deref(), Some("Memory: with colon"));
    assert_eq!(
        parsed.candidate.unwrap().abstract_text,
        "Glob **/*.{ts,tsx} & !node_modules"
    );
}

#[test]
fn quoted_frontmatter_values_stay_valid() {
    let content =
        "---\nname: \"Already: quoted\"\ndescription: \"Use * as text\"\ntype: feedback\n---\nbody";
    let (frontmatter, body, has_frontmatter) = frontmatter_ts_compat(content).unwrap();

    assert!(has_frontmatter);
    assert_eq!(frontmatter.name.as_deref(), Some("Already: quoted"));
    assert_eq!(frontmatter.description.as_deref(), Some("Use * as text"));
    assert_eq!(frontmatter.type_.as_deref(), Some("feedback"));
    assert_eq!(body, "body");
}

#[test]
fn frontmatter_delimiters_accept_ts_compatible_trailing_whitespace() {
    let content = "--- \t \ntype: user\ndescription: spaced delimiter\n---   \nbody";
    let parsed = parse_ts_memory("/tmp/spaced.md", content, "project-a").unwrap();
    let candidate = parsed.candidate.unwrap();

    assert!(parsed.has_frontmatter);
    assert_eq!(candidate.category, MemoryCategory::Profile);
    assert_eq!(candidate.abstract_text, "spaced delimiter");
    assert_eq!(candidate.content, "body");
}

#[test]
fn frontmatter_delimiters_accept_crlf_like_ts_regex() {
    let content = "---\r\ntype: feedback\r\ndescription: crlf delimiter\r\n---\r\nbody";
    let parsed = parse_ts_memory("/tmp/crlf.md", content, "project-a").unwrap();
    let candidate = parsed.candidate.unwrap();

    assert!(parsed.has_frontmatter);
    assert_eq!(candidate.category, MemoryCategory::Preferences);
    assert_eq!(candidate.abstract_text, "crlf delimiter");
    assert_eq!(candidate.content, "body");
}

#[test]
fn missing_type_is_not_fatal_and_candidate_is_none() {
    let content = "---\nname: No Type\ndescription: description\n---\nbody";
    let parsed = parse_ts_memory("/tmp/no-type.md", content, "project-a").unwrap();

    assert!(parsed.has_frontmatter);
    assert_eq!(parsed.raw_type, None);
    assert!(parsed.candidate.is_none());
}

#[test]
fn invalid_type_is_preserved_and_candidate_is_none() {
    let content = "---\nname: Bad Type\ndescription: description\ntype: fact\n---\nbody";
    let parsed = parse_ts_memory("/tmp/bad-type.md", content, "project-a").unwrap();

    assert_eq!(parsed.raw_type.as_deref(), Some("fact"));
    assert!(parsed.candidate.is_none());
}

#[test]
fn name_missing_is_none_and_file_stem_is_available() {
    let content = "---\ndescription: description\ntype: user\n---\nbody";
    let parsed = parse_ts_memory("/tmp/user-memory.md", content, "project-a").unwrap();

    assert_eq!(parsed.name_hint, None);
    assert_eq!(parsed.file_stem, "user-memory");
    assert!(parsed.candidate.is_some());
}

#[test]
fn empty_body_produces_empty_overview_and_content() {
    let parsed = parse_ts_memory("/tmp/empty.md", &markdown("user", ""), "project-a").unwrap();
    let candidate = parsed.candidate.unwrap();

    assert_eq!(candidate.overview, "");
    assert_eq!(candidate.content, "");
}

#[test]
fn short_body_overview_equals_content() {
    let parsed = parse_ts_memory(
        "/tmp/short.md",
        &markdown("user", "short body"),
        "project-a",
    )
    .unwrap();
    let candidate = parsed.candidate.unwrap();

    assert_eq!(candidate.overview, candidate.content);
}

#[test]
fn overview_keeps_199_cjk_chars() {
    let body = "记".repeat(199);
    assert_eq!(truncate_overview(&body, 200).chars().count(), 199);
}

#[test]
fn overview_keeps_200_cjk_chars() {
    let body = "记".repeat(200);
    assert_eq!(truncate_overview(&body, 200).chars().count(), 200);
}

#[test]
fn overview_truncates_201_cjk_chars_on_char_boundary() {
    let body = "记".repeat(201);
    let overview = truncate_overview(&body, 200);

    assert_eq!(overview.chars().count(), 200);
    assert!(overview.is_char_boundary(overview.len()));
}

#[test]
fn scoped_point_id_is_stable_uuid() {
    let first = scoped_path_to_point_id("private", "nested/feedback");
    let second = scoped_path_to_point_id("private", "nested/feedback");

    assert_eq!(first, second);
    assert!(Uuid::parse_str(&first).is_ok());
}

#[test]
fn private_and_team_same_relative_path_have_distinct_point_ids() {
    let private = scoped_path_to_point_id("private", "feedback");
    let team = scoped_path_to_point_id("team", "feedback");

    assert_ne!(private, team);
}

#[test]
fn point_id_uses_relative_path_not_only_stem() {
    let top_level = scoped_path_to_point_id("private", "feedback");
    let nested = scoped_path_to_point_id("private", "nested/feedback");

    assert_ne!(top_level, nested);
}

#[test]
fn point_id_normalizes_path_separator_and_nfc() {
    let decomposed = scoped_path_to_point_id("private", "nested/cafe\u{301}");
    let composed_with_backslash = scoped_path_to_point_id("private", "nested\\café");

    assert_eq!(decomposed, composed_with_backslash);
}

#[test]
fn fields_to_payload_roundtrips_json_values() {
    let mut fields = HashMap::new();
    fields.insert("number".to_owned(), json!(42));
    fields.insert("string".to_owned(), json!("value"));
    fields.insert("array".to_owned(), json!(["a", "b"]));
    fields.insert("object".to_owned(), json!({ "nested": true }));
    fields.insert("null".to_owned(), serde_json::Value::Null);

    let payload = fields_to_payload(fields);

    assert_eq!(payload.0.get("number"), Some(&json!(42)));
    assert_eq!(payload.0.get("string"), Some(&json!("value")));
    assert_eq!(payload.0.get("array"), Some(&json!(["a", "b"])));
    assert_eq!(payload.0.get("object"), Some(&json!({ "nested": true })));
    assert_eq!(payload.0.get("null"), Some(&serde_json::Value::Null));
}
