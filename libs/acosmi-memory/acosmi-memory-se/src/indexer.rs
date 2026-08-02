//! Phase 1 memory topic indexer.
//!
//! This module indexes TS-produced `.md` memory truth-source files into a
//! [`SearchEngine`] topic collection. The `.md` files remain the source of
//! truth; records here are rebuildable search-engine payloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use acosmi_memory_adapter::{fields_to_payload, parse_ts_memory, scoped_path_to_point_id};
use serde_json::json;
use walkdir::WalkDir;

use crate::segment_store::SearchEngine;
use crate::vector_store_adapter::phase1_collection_config;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const SKIP_BASENAMES: &[&str] = &["MEMORY.md", "SESSION.md"];
const ZERO_VECTOR: &[f32] = &[0.0_f32];

/// One root participating in a memory indexing pass.
///
/// Phase 1 callers should pass one private root and one team root explicitly.
/// The private root should exclude at least `team`, `logs`, and `.rust-derived`
/// so nested team memories are not reindexed with private scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRoot {
    pub scope: String,
    pub path: PathBuf,
    pub exclude_prefixes: Vec<PathBuf>,
}

impl MemoryRoot {
    #[must_use]
    pub fn new(
        scope: impl Into<String>,
        path: impl Into<PathBuf>,
        exclude_prefixes: Vec<PathBuf>,
    ) -> Self {
        Self {
            scope: scope.into(),
            path: path.into(),
            exclude_prefixes,
        }
    }

    /// Construct the canonical private root for Phase 1.
    ///
    /// W-MEMORY-LIFECYCLE K2 (2026-07-09) — two evolution-hygiene excludes on
    /// top of the Phase 1 trio (aligned with TS `memoryScan.ts` which already
    /// excludes the imagination dir, so the anti-pollution gate is not
    /// weakened by the new `imagined`/`insight` type mappings):
    ///   * `imagination/` — unconfirmed hypotheses (review queue) must never
    ///     be retrievable; promotion moves them to `dreams/dream_*.md` first.
    ///   * `dreams/fragment_` — low-value weak-signal fragments stay out of
    ///     the index; only `insight_*` / promoted `dream_*` artifacts are
    ///     retrieval-worthy.
    #[must_use]
    pub fn private(path: impl Into<PathBuf>) -> Self {
        Self::new(
            "private",
            path,
            vec![
                PathBuf::from("team"),
                PathBuf::from("logs"),
                PathBuf::from(".rust-derived"),
                PathBuf::from("imagination/"),
                PathBuf::from("dreams/fragment_"),
            ],
        )
    }

    /// Construct the canonical team root for Phase 1.
    #[must_use]
    pub fn team(path: impl Into<PathBuf>) -> Self {
        Self::new("team", path, Vec::new())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    /// Root entries supplied to this pass.
    pub roots_scanned: usize,
    /// Missing root directories skipped without failing the pass.
    pub roots_missing: usize,
    /// `.md` files found before skip decisions.
    pub md_files_seen: usize,
    /// `.md` files that reached adapter parsing.
    pub files_scanned: usize,
    /// Files successfully upserted into the topic collection.
    pub indexed: usize,
    /// Total non-fatal skips across all skip reasons.
    pub skipped: usize,
    pub skipped_excluded_prefix: usize,
    pub skipped_team_from_private: usize,
    pub skipped_logs: usize,
    pub skipped_legacy_rust_derived: usize,
    pub skipped_basename: usize,
    pub skipped_missing_frontmatter: usize,
    pub skipped_missing_type: usize,
    pub skipped_invalid_type: usize,
    /// W-MEMORY-KB-UPLIFT P0 — knowledge entries gated out of retrieval
    /// (`enabled: false` / `pending_review: true`; agent drafts and evolution
    /// proposals awaiting human review).
    pub skipped_disabled_knowledge: usize,
    /// Auditable per-file skip ledger.
    pub skip_reasons: Vec<IndexSkip>,
    /// Auditable non-fatal errors. One bad file does not abort the full pass.
    pub errors: Vec<IndexError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSkip {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub scope: String,
    pub reason: IndexSkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSkipReason {
    ExcludedPrefix,
    TeamFromPrivate,
    LogFile,
    LegacyRustDerived,
    SkipBasename,
    MissingFrontmatter,
    MissingType,
    InvalidType {
        raw_type: String,
    },
    /// W-MEMORY-KB-UPLIFT P0 — un-enabled knowledge entry (review gate).
    DisabledKnowledgeEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexError {
    pub path: PathBuf,
    pub relative_path: Option<PathBuf>,
    pub scope: String,
    pub message: String,
}

enum IndexOne {
    Indexed,
    Skipped(IndexSkipReason),
}

/// Ensure the Phase 1 topic collection exists with the dim=1 zero-vector config.
pub fn ensure_memory_topic_collection(
    engine: &SearchEngine,
    collection: &str,
) -> Result<bool, BoxError> {
    engine.create_collection(collection, &phase1_collection_config())
}

/// Scan memory roots and upsert valid topic memory markdown files.
///
/// Skips and per-file errors are aggregated into [`IndexStats`]. Only failures
/// that prevent collection setup itself from running are returned as `Err`.
pub fn index_memory_roots(
    engine: &Arc<SearchEngine>,
    roots: &[MemoryRoot],
    user: &str,
    collection: &str,
) -> Result<IndexStats, BoxError> {
    ensure_memory_topic_collection(engine, collection)?;

    let mut stats = IndexStats::default();

    for root in roots {
        stats.roots_scanned += 1;

        if !root.path.exists() {
            stats.roots_missing += 1;
            continue;
        }

        for entry in WalkDir::new(&root.path) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    stats.errors.push(IndexError {
                        path: error.path().unwrap_or(root.path.as_path()).to_path_buf(),
                        relative_path: error
                            .path()
                            .and_then(|path| path.strip_prefix(&root.path).ok())
                            .map(Path::to_path_buf),
                        scope: root.scope.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };

            if !entry.file_type().is_file() || !is_markdown_file(entry.path()) {
                continue;
            }

            stats.md_files_seen += 1;
            let path = entry.path();
            let relative = match path.strip_prefix(&root.path) {
                Ok(relative) => relative,
                Err(error) => {
                    stats.errors.push(IndexError {
                        path: path.to_path_buf(),
                        relative_path: None,
                        scope: root.scope.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };

            if let Some(reason) = skip_before_adapter(&root.scope, relative, &root.exclude_prefixes)
            {
                record_skip(&mut stats, path, relative, &root.scope, reason);
                continue;
            }

            stats.files_scanned += 1;
            match index_one_file(engine, path, relative, user, &root.scope, collection) {
                Ok(IndexOne::Indexed) => stats.indexed += 1,
                Ok(IndexOne::Skipped(reason)) => {
                    record_skip(&mut stats, path, relative, &root.scope, reason);
                }
                Err(error) => {
                    tracing::warn!(?path, error = %error, "memory indexer: file failed");
                    stats.errors.push(IndexError {
                        path: path.to_path_buf(),
                        relative_path: Some(relative.to_path_buf()),
                        scope: root.scope.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    Ok(stats)
}

fn index_one_file(
    engine: &SearchEngine,
    path: &Path,
    relative_path: &Path,
    user: &str,
    scope: &str,
    collection: &str,
) -> Result<IndexOne, BoxError> {
    let content = std::fs::read_to_string(path)?;
    let metadata = path.metadata()?;
    let mtime_ms = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);

    let parsed = parse_ts_memory(&path.to_string_lossy(), &content, user)?;
    if !parsed.has_frontmatter {
        return Ok(IndexOne::Skipped(IndexSkipReason::MissingFrontmatter));
    }

    let relative_path_value = relative_path_key(relative_path);
    let relative_path_no_ext = relative_path_key(&relative_path.with_extension(""));
    let point_id = scoped_path_to_point_id(scope, &relative_path_no_ext);

    // W-MEMORY-KB-UPLIFT P0 (2026-07-17) — review gate at index time: an
    // un-enabled knowledge entry (`enabled: false` agent draft / evolution
    // proposal, or `pending_review: true`) must never enter the index, so the
    // "启用前绝不参与召回" contract holds for search AND passive recall. The
    // best-effort delete sweeps any stale point left by a pre-gating index
    // (the file still exists on disk, so the liveness filter alone would keep
    // resurfacing it).
    if scope == "knowledge"
        && (parsed.enabled == Some(false) || parsed.pending_review == Some(true))
    {
        let _ = engine.delete(collection, &point_id);
        return Ok(IndexOne::Skipped(IndexSkipReason::DisabledKnowledgeEntry));
    }

    let Some(raw_type) = parsed.raw_type.clone() else {
        return Ok(IndexOne::Skipped(IndexSkipReason::MissingType));
    };

    let Some(candidate) = parsed.candidate else {
        return Ok(IndexOne::Skipped(IndexSkipReason::InvalidType { raw_type }));
    };

    let name = parsed.name_hint.unwrap_or(parsed.file_stem);

    let mut fields = HashMap::new();
    fields.insert("scope".to_owned(), json!(scope));
    fields.insert("relative_path".to_owned(), json!(relative_path_value));
    fields.insert(
        "relative_path_no_ext".to_owned(),
        json!(relative_path_no_ext),
    );
    fields.insert("category".to_owned(), json!(candidate.category));
    fields.insert("type".to_owned(), json!(raw_type));
    fields.insert("name".to_owned(), json!(name));
    fields.insert("mtime_ms".to_owned(), json!(mtime_ms));
    fields.insert(
        "source_path".to_owned(),
        json!(path.to_string_lossy().into_owned()),
    );
    fields.insert("abstract".to_owned(), json!(candidate.abstract_text));
    fields.insert("overview".to_owned(), json!(candidate.overview));
    fields.insert("content".to_owned(), json!(candidate.content));
    fields.insert("user".to_owned(), json!(candidate.user));
    fields.insert("language".to_owned(), json!(candidate.language));
    // W-MEMORY-KB-UPLIFT P0 — knowledge injection mode into the payload
    // (only when present; regular memory files carry no `injection`).
    // `manual` entries are filtered out of passive recall at query time.
    if let Some(injection) = parsed.injection.as_deref() {
        fields.insert("injection".to_owned(), json!(injection));
    }

    let payload = fields_to_payload(fields);
    engine.upsert(collection, &point_id, ZERO_VECTOR, Some(&payload))?;

    Ok(IndexOne::Indexed)
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("md")
}

fn skip_before_adapter(
    scope: &str,
    relative: &Path,
    exclude_prefixes: &[PathBuf],
) -> Option<IndexSkipReason> {
    let first_component = first_component(relative);

    if scope == "private" && first_component == Some("team") {
        return Some(IndexSkipReason::TeamFromPrivate);
    }

    if first_component == Some("logs") {
        return Some(IndexSkipReason::LogFile);
    }

    if first_component == Some(".rust-derived") {
        return Some(IndexSkipReason::LegacyRustDerived);
    }

    if relative
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SKIP_BASENAMES.contains(&name))
    {
        return Some(IndexSkipReason::SkipBasename);
    }

    if exclude_prefixes
        .iter()
        .any(|prefix| matches_exclude_prefix(relative, prefix))
    {
        return Some(IndexSkipReason::ExcludedPrefix);
    }

    None
}

/// Exclusion-prefix matching (W-MEMORY-LIFECYCLE K2, 2026-07-09).
///
/// Two semantics, chosen by the shape of the prefix:
///
/// * **Bare single-component prefixes** (`team`, `logs`, `.rust-derived`) keep
///   the long-standing whole-component subtree semantics of
///   [`Path::starts_with`]: `team` matches `team/x.md` but never
///   `teamwork.md`.
/// * **Prefixes containing a `/`** (embedded or trailing) match as *string*
///   prefixes over the slash-normalized relative path, which component
///   matching cannot express: `dreams/fragment_` is a *filename prefix*
///   inside `dreams/` (matches `dreams/fragment_b.md`, not
///   `dreams/insight_a.md`), and a trailing slash (`imagination/`) marks an
///   explicit subtree (matches `imagination/review-queue/x.md`, not a
///   sibling file `imaginationX.md`).
fn matches_exclude_prefix(relative: &Path, prefix: &Path) -> bool {
    if prefix.as_os_str().is_empty() {
        return false;
    }
    // Whole-component subtree semantics (existing behaviour, kept first so
    // bare directory-name prefixes behave exactly as before).
    if relative.starts_with(prefix) {
        return true;
    }
    // String-prefix semantics only for separator-bearing patterns; bare
    // single-component prefixes deliberately never take this path (a string
    // match of `team` would over-match `teamwork.md`).
    let raw = prefix.to_string_lossy().replace('\\', "/");
    if !raw.contains('/') {
        return false;
    }
    relative_path_key(relative).starts_with(&raw)
}

fn first_component(path: &Path) -> Option<&str> {
    path.components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
}

fn record_skip(
    stats: &mut IndexStats,
    path: &Path,
    relative_path: &Path,
    scope: &str,
    reason: IndexSkipReason,
) {
    stats.skipped += 1;
    match &reason {
        IndexSkipReason::ExcludedPrefix => stats.skipped_excluded_prefix += 1,
        IndexSkipReason::TeamFromPrivate => stats.skipped_team_from_private += 1,
        IndexSkipReason::LogFile => stats.skipped_logs += 1,
        IndexSkipReason::LegacyRustDerived => stats.skipped_legacy_rust_derived += 1,
        IndexSkipReason::SkipBasename => stats.skipped_basename += 1,
        IndexSkipReason::MissingFrontmatter => stats.skipped_missing_frontmatter += 1,
        IndexSkipReason::MissingType => stats.skipped_missing_type += 1,
        IndexSkipReason::InvalidType { .. } => stats.skipped_invalid_type += 1,
        IndexSkipReason::DisabledKnowledgeEntry => stats.skipped_disabled_knowledge += 1,
    }

    stats.skip_reasons.push(IndexSkip {
        path: path.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        scope: scope.to_owned(),
        reason,
    });
}

fn relative_path_key(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use serde_json::Value;
    use tempfile::TempDir;

    use crate::segment_store::SearchEngine;

    use super::*;

    const COLLECTION: &str = "lifecycle_k2_topic_memory";

    fn test_engine() -> (TempDir, Arc<SearchEngine>) {
        let dir = TempDir::new().unwrap();
        let engine = Arc::new(SearchEngine::new(dir.path()).unwrap());
        ensure_memory_topic_collection(&engine, COLLECTION).unwrap();
        (dir, engine)
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn doc(type_: &str, name: &str) -> String {
        format!(
            "---\ntype: {type_}\nname: {name}\ndescription: {name} description\n---\nbody of {name}\n"
        )
    }

    fn indexed_relative_paths(engine: &SearchEngine) -> HashSet<String> {
        engine
            .scroll(COLLECTION, 1_000)
            .unwrap()
            .into_iter()
            .filter_map(|hit| {
                hit.payload
                    .get("relative_path")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// K2 — exclusion-prefix matching semantics: bare single-component
    /// prefixes keep whole-component subtree matching; separator-bearing
    /// prefixes match as string prefixes (filename-prefix patterns +
    /// explicit trailing-slash subtrees).
    #[test]
    fn exclude_prefix_component_vs_string_semantics() {
        let excludes = vec![
            PathBuf::from("team"),
            PathBuf::from("imagination/"),
            PathBuf::from("dreams/fragment_"),
        ];
        let skip = |rel: &str| skip_before_adapter("custom", Path::new(rel), &excludes);

        // Bare component prefix: subtree matches, sibling name-prefix does not.
        assert_eq!(skip("team/x.md"), Some(IndexSkipReason::ExcludedPrefix));
        assert_eq!(skip("teamwork.md"), None, "bare prefix must not over-match");

        // Trailing-slash subtree: subtree matches, sibling file does not.
        assert_eq!(
            skip("imagination/review-queue/imagined_a.md"),
            Some(IndexSkipReason::ExcludedPrefix)
        );
        assert_eq!(
            skip("imaginationX.md"),
            None,
            "trailing slash pins the subtree"
        );

        // Filename-prefix pattern inside a subdir.
        assert_eq!(
            skip("dreams/fragment_b.md"),
            Some(IndexSkipReason::ExcludedPrefix)
        );
        assert_eq!(skip("dreams/insight_a.md"), None);
        assert_eq!(skip("dreams/dream_c.md"), None);
        assert_eq!(skip("reports/evolution-2026-07-09.md"), None);
    }

    /// W-MEMORY-KB-UPLIFT P0 — the knowledge review gate: `enabled: false`
    /// drafts and `pending_review: true` proposals never index from the
    /// knowledge root; re-enabling re-indexes; a stale pre-gating point is
    /// swept on the pass that skips its file; and the gate is
    /// knowledge-scope-only.
    #[test]
    fn knowledge_root_gates_unenabled_entries_and_sweeps_stale_points() {
        let (_engine_dir, engine) = test_engine();
        let knowledge_dir = TempDir::new().unwrap();

        write_file(
            &knowledge_dir.path().join("draft-agent.md"),
            "---\ntype: knowledge\nname: agent draft\nenabled: false\ninjection: manual\npending_review: true\nsource: agent_draft\n---\ndraft body awaiting review\n",
        );
        write_file(
            &knowledge_dir.path().join("proposal-evolution.md"),
            "---\ntype: knowledge\nname: evolution proposal\nenabled: false\ninjection: manual\npending_review: true\n---\nproposal body\n",
        );
        write_file(
            &knowledge_dir.path().join("kb-live.md"),
            "---\ntype: knowledge\nname: live entry\nenabled: true\ninjection: manual\n---\nlive body\n",
        );

        let roots = vec![MemoryRoot::new(
            "knowledge",
            knowledge_dir.path().to_path_buf(),
            Vec::new(),
        )];
        let stats = index_memory_roots(&engine, &roots, "local", COLLECTION).unwrap();
        assert_eq!(stats.indexed, 1, "only the enabled entry may index");
        assert_eq!(stats.skipped_disabled_knowledge, 2);
        let paths = indexed_relative_paths(&engine);
        assert!(paths.contains("kb-live.md"));
        assert!(!paths.contains("draft-agent.md"));
        assert!(!paths.contains("proposal-evolution.md"));

        // `injection` must land in the payload for query-time manual gating.
        let injection_modes: Vec<String> = engine
            .scroll(COLLECTION, 1_000)
            .unwrap()
            .into_iter()
            .filter_map(|hit| {
                hit.payload
                    .get("injection")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(injection_modes, vec!["manual".to_string()]);

        // Stale-point sweep: an entry indexed while enabled, then disabled,
        // must vanish from the index on the next pass (its file still exists,
        // so the query-time liveness filter alone would keep resurfacing it).
        let flip = knowledge_dir.path().join("kb-flip.md");
        write_file(
            &flip,
            "---\ntype: knowledge\nname: flip entry\nenabled: true\n---\nflip body\n",
        );
        index_memory_roots(&engine, &roots, "local", COLLECTION).unwrap();
        assert!(indexed_relative_paths(&engine).contains("kb-flip.md"));
        write_file(
            &flip,
            "---\ntype: knowledge\nname: flip entry\nenabled: false\n---\nflip body\n",
        );
        index_memory_roots(&engine, &roots, "local", COLLECTION).unwrap();
        assert!(
            !indexed_relative_paths(&engine).contains("kb-flip.md"),
            "disabling an entry must sweep its stale point on the next pass"
        );

        // Non-knowledge scopes are untouched by the gate: the same
        // frontmatter under a private root indexes normally.
        let private_dir = TempDir::new().unwrap();
        write_file(
            &private_dir.path().join("note.md"),
            "---\ntype: knowledge\nname: private note\nenabled: false\n---\nnot a knowledge-root file\n",
        );
        let private_roots = vec![MemoryRoot::new(
            "private",
            private_dir.path().to_path_buf(),
            Vec::new(),
        )];
        let stats = index_memory_roots(&engine, &private_roots, "local", COLLECTION).unwrap();
        assert_eq!(stats.indexed, 1, "gate is knowledge-scope-only");
    }

    /// K2 — end-to-end over a real engine: review-queue files and weak
    /// fragments are never indexed from a private root, while promoted /
    /// evolution artifacts (`dreams/insight_*`, `dreams/dream_*`,
    /// `reports/*`) index once their types (`insight` / `imagined` /
    /// `report`, mapped in `acosmi-memory-adapter::category_map`) are valid.
    #[test]
    fn private_root_excludes_quarantine_but_indexes_evolution_artifacts() {
        let (_engine_dir, engine) = test_engine();
        let memory_dir = TempDir::new().unwrap();

        // Quarantined / low-value artifacts — must be excluded by prefix even
        // though their `type` values are now index-valid.
        write_file(
            &memory_dir
                .path()
                .join("imagination/review-queue/imagined_a.md"),
            &doc("imagined", "quarantined hypothesis"),
        );
        write_file(
            &memory_dir.path().join("dreams/fragment_low.md"),
            &doc("insight", "weak fragment"),
        );

        // Evolution artifacts that must index.
        write_file(
            &memory_dir.path().join("dreams/insight_keep.md"),
            &doc("insight", "strong insight"),
        );
        write_file(
            &memory_dir.path().join("dreams/dream_confirmed.md"),
            &doc("imagined", "confirmed imagination"),
        );
        write_file(
            &memory_dir.path().join("reports/evolution-2026-07-09.md"),
            &doc("report", "evolution report"),
        );

        let stats = index_memory_roots(
            &engine,
            &[MemoryRoot::private(memory_dir.path())],
            "lifecycle-user",
            COLLECTION,
        )
        .unwrap();

        assert_eq!(
            stats.indexed, 3,
            "three evolution artifacts index: {stats:?}"
        );
        assert_eq!(
            stats.skipped_excluded_prefix, 2,
            "review-queue + fragment are prefix-excluded: {stats:?}"
        );
        assert_eq!(
            stats.skipped_invalid_type, 0,
            "K2 types are valid: {stats:?}"
        );

        let paths = indexed_relative_paths(&engine);
        assert!(paths.contains("dreams/insight_keep.md"), "{paths:?}");
        assert!(paths.contains("dreams/dream_confirmed.md"), "{paths:?}");
        assert!(
            paths.contains("reports/evolution-2026-07-09.md"),
            "{paths:?}"
        );
        assert!(
            !paths.contains("imagination/review-queue/imagined_a.md"),
            "review queue stays unindexed: {paths:?}"
        );
        assert!(
            !paths.contains("dreams/fragment_low.md"),
            "fragments stay unindexed: {paths:?}"
        );
    }

    /// K9 — a knowledge root (custom scope, no extra excludes beyond the
    /// type whitelist) indexes `type: knowledge` entries with its own scope.
    #[test]
    fn knowledge_root_indexes_knowledge_typed_entries_with_scope() {
        let (_engine_dir, engine) = test_engine();
        let knowledge_dir = TempDir::new().unwrap();
        write_file(
            &knowledge_dir.path().join("getting-started.md"),
            &doc("knowledge", "getting started"),
        );

        let stats = index_memory_roots(
            &engine,
            &[MemoryRoot::new(
                "knowledge",
                knowledge_dir.path(),
                Vec::new(),
            )],
            "lifecycle-user",
            COLLECTION,
        )
        .unwrap();

        assert_eq!(stats.indexed, 1, "{stats:?}");
        let hits = engine.scroll(COLLECTION, 1_000).unwrap();
        let hit = hits
            .iter()
            .find(|hit| {
                hit.payload.get("relative_path").and_then(Value::as_str)
                    == Some("getting-started.md")
            })
            .expect("knowledge entry indexed");
        assert_eq!(
            hit.payload.get("scope").and_then(Value::as_str),
            Some("knowledge")
        );
        assert_eq!(
            hit.payload.get("type").and_then(Value::as_str),
            Some("knowledge")
        );
    }
}
