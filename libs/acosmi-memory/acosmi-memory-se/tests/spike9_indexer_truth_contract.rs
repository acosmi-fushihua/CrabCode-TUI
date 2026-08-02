use std::path::{Path, PathBuf};

use acosmi_memory_se::segment_store::{CollectionConfig, SearchEngine};
use acosmi_memory_se::{Distance, Payload};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map};
use tempfile::TempDir;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;
use walkdir::WalkDir;

const COLLECTION: &str = "spike9_topic_memory";
const NAMESPACE_MEMORY: Uuid = Uuid::from_u128(0x6f76_6b00_0000_0000_0000_0000_0000_0001);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    Private,
    Team,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Team => "team",
        }
    }
}

#[derive(Debug)]
struct MemoryRoot {
    path: PathBuf,
    scope: Scope,
}

#[derive(Default, Debug)]
struct IndexStats {
    indexed: usize,
    skipped_team_from_private: usize,
    skipped_logs: usize,
    skipped_legacy_rust_derived: usize,
    skipped_missing_frontmatter: usize,
    skipped_missing_or_invalid_type: usize,
    skipped_basename: usize,
    indexed_records: Vec<IndexedRecord>,
}

#[derive(Clone, Debug)]
struct IndexedRecord {
    point_id: String,
    scope: Scope,
    relative_path_no_ext: String,
}

#[derive(Default, Debug, Deserialize)]
struct TsFrontmatter {
    description: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
}

#[derive(Debug)]
struct ParsedMarkdown {
    frontmatter: TsFrontmatter,
    content: String,
    has_frontmatter: bool,
}

fn test_store() -> (TempDir, SearchEngine) {
    let dir = TempDir::new().unwrap();
    let store = SearchEngine::new(dir.path()).unwrap();
    store
        .create_collection(
            COLLECTION,
            &CollectionConfig {
                dimension: 1,
                distance: Distance::Cosine,
                sparse_vectors: false,
                ..Default::default()
            },
        )
        .unwrap();
    (dir, store)
}

fn scoped_path_to_point_id(scope: Scope, relative_path_no_ext: &str) -> String {
    let key = format!(
        "{}:{}",
        scope.as_str(),
        relative_path_no_ext.nfc().collect::<String>()
    );
    Uuid::new_v5(&NAMESPACE_MEMORY, key.as_bytes()).to_string()
}

fn parse_frontmatter_ts_compat(markdown: &str) -> Result<ParsedMarkdown, serde_yaml::Error> {
    let frontmatter_regex = Regex::new(r"(?s)^---\s*\n(.*?)---\s*\n?").unwrap();
    let Some(captures) = frontmatter_regex.captures(markdown) else {
        return Ok(ParsedMarkdown {
            frontmatter: TsFrontmatter::default(),
            content: markdown.to_owned(),
            has_frontmatter: false,
        });
    };

    let full_match = captures.get(0).unwrap();
    let frontmatter_text = captures.get(1).map_or("", |m| m.as_str());
    let content = markdown[full_match.end()..].to_owned();

    let frontmatter = serde_yaml::from_str(frontmatter_text)
        .or_else(|_| serde_yaml::from_str(&quote_problematic_values(frontmatter_text)))?;

    Ok(ParsedMarkdown {
        frontmatter,
        content,
        has_frontmatter: true,
    })
}

fn quote_problematic_values(frontmatter_text: &str) -> String {
    frontmatter_text
        .lines()
        .map(|line| {
            let Some((key, value)) = line.split_once(": ") else {
                return line.to_owned();
            };
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
                || value.is_empty()
                || ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
                || !has_yaml_special_chars(value)
            {
                return line.to_owned();
            }

            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{key}: \"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_yaml_special_chars(value: &str) -> bool {
    value.contains(": ")
        || value.chars().any(|c| {
            matches!(
                c,
                '{' | '}' | '[' | ']' | '*' | '&' | '#' | '!' | '|' | '>' | '%' | '@' | '`'
            )
        })
}

fn index_spike9_roots(store: &SearchEngine, roots: &[MemoryRoot]) -> IndexStats {
    let mut stats = IndexStats::default();

    for root in roots {
        if !root.path.exists() {
            continue;
        }

        for entry in WalkDir::new(&root.path).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "md") {
                continue;
            }

            let path = entry.path();
            let relative = path.strip_prefix(&root.path).unwrap();
            if skip_before_adapter(relative, root.scope, &mut stats) {
                continue;
            }

            match index_one_file(store, path, relative, root.scope) {
                IndexOutcome::Indexed(record) => {
                    stats.indexed += 1;
                    stats.indexed_records.push(record);
                }
                IndexOutcome::SkippedMissingFrontmatter => {
                    stats.skipped_missing_frontmatter += 1;
                }
                IndexOutcome::SkippedMissingOrInvalidType => {
                    stats.skipped_missing_or_invalid_type += 1;
                }
            }
        }
    }

    stats
}

fn skip_before_adapter(relative: &Path, scope: Scope, stats: &mut IndexStats) -> bool {
    let first_component = relative
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or("");
    if scope == Scope::Private && first_component == "team" {
        stats.skipped_team_from_private += 1;
        return true;
    }
    if first_component == "logs" {
        stats.skipped_logs += 1;
        return true;
    }
    if first_component == ".rust-derived" {
        stats.skipped_legacy_rust_derived += 1;
        return true;
    }
    if matches!(
        relative.file_name().and_then(|s| s.to_str()),
        Some("MEMORY.md" | "SESSION.md")
    ) {
        stats.skipped_basename += 1;
        return true;
    }

    false
}

enum IndexOutcome {
    Indexed(IndexedRecord),
    SkippedMissingFrontmatter,
    SkippedMissingOrInvalidType,
}

fn index_one_file(
    store: &SearchEngine,
    path: &Path,
    relative: &Path,
    scope: Scope,
) -> IndexOutcome {
    let content = std::fs::read_to_string(path).unwrap();
    let parsed = parse_frontmatter_ts_compat(&content).unwrap();
    if !parsed.has_frontmatter {
        return IndexOutcome::SkippedMissingFrontmatter;
    }
    let Some(type_) = parsed.frontmatter.type_.as_deref() else {
        return IndexOutcome::SkippedMissingOrInvalidType;
    };
    if !matches!(type_, "user" | "feedback" | "project" | "reference") {
        return IndexOutcome::SkippedMissingOrInvalidType;
    }

    let relative_path_no_ext = relative_path_no_ext(relative);
    let point_id = scoped_path_to_point_id(scope, &relative_path_no_ext);
    let mut payload = Map::new();
    payload.insert("scope".to_owned(), json!(scope.as_str()));
    payload.insert("relative_path".to_owned(), json!(relative_path_no_ext));
    payload.insert("type".to_owned(), json!(type_));
    payload.insert(
        "description".to_owned(),
        json!(parsed.frontmatter.description.unwrap_or_default()),
    );
    payload.insert("content".to_owned(), json!(parsed.content));
    let payload = Payload::from(payload);

    store
        .upsert(COLLECTION, &point_id, &[1.0_f32], Some(&payload))
        .unwrap();

    IndexOutcome::Indexed(IndexedRecord {
        point_id,
        scope,
        relative_path_no_ext,
    })
}

fn relative_path_no_ext(relative: &Path) -> String {
    relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
        .nfc()
        .collect()
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn valid_memory(type_: &str, body: &str) -> String {
    format!("---\ntype: {type_}\ndescription: {type_} memory\n---\n{body}\n")
}

fn spike9_roots(memory_root: &Path) -> Vec<MemoryRoot> {
    vec![
        MemoryRoot {
            path: memory_root.to_path_buf(),
            scope: Scope::Private,
        },
        MemoryRoot {
            path: memory_root.join("team"),
            scope: Scope::Team,
        },
    ]
}

#[test]
fn point_id_uses_scope_and_relative_path_no_ext_not_stem() {
    let private_feedback = scoped_path_to_point_id(Scope::Private, "feedback");
    let team_feedback = scoped_path_to_point_id(Scope::Team, "feedback");
    let private_nested_feedback = scoped_path_to_point_id(Scope::Private, "nested/feedback");

    assert_ne!(private_feedback, team_feedback);
    assert_ne!(private_feedback, private_nested_feedback);
    assert_ne!(team_feedback, private_nested_feedback);
    assert!(Uuid::parse_str(&private_feedback).is_ok());
}

#[test]
fn private_and_team_same_name_index_as_distinct_points() {
    let (_store_dir, store) = test_store();
    let memory_dir = TempDir::new().unwrap();
    write_file(
        &memory_dir.path().join("feedback.md"),
        &valid_memory("feedback", "private feedback"),
    );
    write_file(
        &memory_dir.path().join("team/feedback.md"),
        &valid_memory("feedback", "team feedback"),
    );

    let stats = index_spike9_roots(&store, &spike9_roots(memory_dir.path()));
    assert_eq!(stats.indexed, 2);
    assert_eq!(stats.skipped_team_from_private, 1);
    assert_eq!(store.point_count(COLLECTION), Some(2));

    let private = stats
        .indexed_records
        .iter()
        .find(|r| r.scope == Scope::Private && r.relative_path_no_ext == "feedback")
        .unwrap();
    let team = stats
        .indexed_records
        .iter()
        .find(|r| r.scope == Scope::Team && r.relative_path_no_ext == "feedback")
        .unwrap();
    assert_ne!(private.point_id, team.point_id);
    assert_eq!(
        private.point_id,
        scoped_path_to_point_id(Scope::Private, "feedback")
    );
    assert_eq!(
        team.point_id,
        scoped_path_to_point_id(Scope::Team, "feedback")
    );
}

#[test]
fn nested_team_is_not_reindexed_as_private_scope() {
    let (_store_dir, store) = test_store();
    let memory_dir = TempDir::new().unwrap();
    write_file(
        &memory_dir.path().join("team/foo.md"),
        &valid_memory("project", "team-only memory"),
    );

    let stats = index_spike9_roots(&store, &spike9_roots(memory_dir.path()));
    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.skipped_team_from_private, 1);

    let hits = store.scroll(COLLECTION, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.get("scope"), Some(&json!("team")));
    assert_eq!(hits[0].payload.get("relative_path"), Some(&json!("foo")));
    assert!(!hits.iter().any(|hit| {
        hit.payload.get("scope") == Some(&json!("private"))
            && hit.payload.get("relative_path") == Some(&json!("team/foo"))
    }));
}

#[test]
fn logs_and_legacy_rust_derived_are_skipped_before_adapter() {
    let (_store_dir, store) = test_store();
    let memory_dir = TempDir::new().unwrap();
    write_file(
        &memory_dir.path().join("logs/2026/05/2026-05-06.md"),
        "not frontmatter, but should never reach adapter",
    );
    write_file(
        &memory_dir.path().join(".rust-derived/ghost.md"),
        &valid_memory("project", "legacy derived data"),
    );

    let stats = index_spike9_roots(&store, &spike9_roots(memory_dir.path()));
    assert_eq!(stats.indexed, 0);
    assert_eq!(stats.skipped_logs, 1);
    assert_eq!(stats.skipped_legacy_rust_derived, 1);
    assert_eq!(stats.skipped_missing_frontmatter, 0);
    assert_eq!(store.point_count(COLLECTION), Some(0));
}

#[test]
fn frontmatter_ts_compat_allows_missing_and_quote_fallback() {
    let no_frontmatter = parse_frontmatter_ts_compat("plain body").unwrap();
    assert!(!no_frontmatter.has_frontmatter);
    assert_eq!(no_frontmatter.content, "plain body");
    assert!(no_frontmatter.frontmatter.type_.is_none());

    let fallback = parse_frontmatter_ts_compat(
        "---\ntype: project\ndescription: run: cargo test * & ship!\n---\nBody\n",
    )
    .unwrap();
    assert!(fallback.has_frontmatter);
    assert_eq!(fallback.frontmatter.type_.as_deref(), Some("project"));
    assert_eq!(
        fallback.frontmatter.description.as_deref(),
        Some("run: cargo test * & ship!")
    );
    assert_eq!(fallback.content, "Body\n");
}

#[test]
fn indexer_skips_missing_frontmatter_missing_type_and_invalid_type() {
    let (_store_dir, store) = test_store();
    let memory_dir = TempDir::new().unwrap();
    write_file(&memory_dir.path().join("plain.md"), "no yaml here\n");
    write_file(
        &memory_dir.path().join("missing_type.md"),
        "---\ndescription: no type\n---\nBody\n",
    );
    write_file(
        &memory_dir.path().join("invalid_type.md"),
        "---\ntype: invalid\ndescription: bad type\n---\nBody\n",
    );

    let stats = index_spike9_roots(&store, &spike9_roots(memory_dir.path()));
    assert_eq!(stats.indexed, 0);
    assert_eq!(stats.skipped_missing_frontmatter, 1);
    assert_eq!(stats.skipped_missing_or_invalid_type, 2);
    assert_eq!(store.point_count(COLLECTION), Some(0));
}
