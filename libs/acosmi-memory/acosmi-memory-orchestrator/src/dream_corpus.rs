//! W-MEMORY-DATA-COMPLETION Phase 0 (2026-06-20) — dream corpus assembly.
//!
//! # Root cause this module fixes (P0)
//!
//! Both Tier-3 dream call sites — the periodic sweep `run_dream_tick` and the
//! manual `build_dream_now_input` — used to pass `recent_sessions_summary:
//! String::new()` + `memdir_manifest: String::new()` into `DreamProcessInput`,
//! under the stale assumption (`DreamProcessInput` doc: "TS side built") that a
//! TS caller assembles the digest. The periodic sweep has **no TS caller** (it
//! is a pure background task), so that path's corpus was *permanently* empty →
//! Phase-1 "Orient" saw only the `"(no recent sessions)"` / `"(memdir empty)"`
//! placeholders → `themes.is_empty()` → early-return with zero insights, after
//! already burning the Phase-0 + Phase-1 LLM round-trips. Dreaming cost quota
//! and produced nothing.
//!
//! # Why this is the root fix (not a patch)
//!
//! The data the dream needs lives right next to the orchestrator on disk —
//! main-session `.jsonl` transcripts in `project_state_dir`, and the memdir
//! index `MEMORY.md` in `memory_dir`. CLAUDE.md §15 #6 mandates that the Rust
//! data layer **reads those TS-side files directly** rather than relying on a
//! TS-built digest. Assembling the corpus in-orchestrator means *both* the
//! periodic and manual paths feed real data with no external caller to depend
//! on, so the empty-corpus failure cannot recur on either path.
//!
//! If instead the fix passed a TS-built digest in, the periodic path (no TS
//! caller) would stay empty → P0 reproduces verbatim on the background sweep;
//! and TS would have to re-implement transcript compaction that Rust already
//! has (`read_transcript_content`), a guaranteed drift point. Hence: Rust reads.
//!
//! Fail-soft: any read error degrades that half to an empty string (the prompt
//! builder substitutes its placeholder). A corpus read failure never aborts a
//! dream.

use std::path::{Path, PathBuf};

use crate::dream_gate::project_state_dir_from_memory_dir;
use crate::memory_md_analyze::analyze_memory_md;
use crate::transcript_index::{
    read_transcript_content, scan_transcript_dir, TASK_TYPE_MAIN_SESSION,
};

/// Max chars of each transcript body folded into the corpus. Aligned with
/// imagination's `HYPGEN_MAX_TRANSCRIPT_CHARS` (8_000) so dream and imagination
/// see comparably-sized per-session context. `read_transcript_content`
/// tail-truncates (keeps the most recent turns) past this cap.
pub(crate) const DREAM_MAX_TRANSCRIPT_CHARS: usize = 8_000;

/// Max number of recent main-session transcripts folded into the corpus. A
/// dream needs *several* sessions to surface a cross-session theme — one
/// session rarely yields one — so this is deliberately > 1. Bounded so a
/// long-lived project doesn't blow the Phase-1 prompt.
pub(crate) const DREAM_MAX_SESSIONS: usize = 8;

/// K9 (W-MEMORY-LIFECYCLE 2026-07-09) — knowledge-base corpus caps: at most
/// this many entries (most-recently-modified first) …
pub(crate) const KNOWLEDGE_SUMMARY_MAX_ENTRIES: usize = 10;
/// … and the whole `## 个人知识库摘要` section is truncated to this many chars
/// so an over-stuffed knowledge base cannot blow the Phase-1 prompt.
pub(crate) const KNOWLEDGE_SUMMARY_MAX_CHARS: usize = 2_000;

/// K10 (W-MEMORY-LIFECYCLE 2026-07-09) — watch-inventory bounds: shallow
/// directory listing only (≤ this depth) …
pub(crate) const WATCH_INVENTORY_MAX_DEPTH: usize = 3;
/// … and at most this many entries (names only).
pub(crate) const WATCH_INVENTORY_MAX_ENTRIES: usize = 200;

/// Directory names never descended into (or listed) by the watch inventory.
const WATCH_INVENTORY_SKIP_DIRS: [&str; 3] = [".git", "node_modules", "target"];

/// Assemble `(recent_sessions_summary, memdir_manifest)` for `DreamProcessInput`.
///
/// * `memory_dir` — the project memdir (`.../<slug>/memory`); `MEMORY.md` lives
///   here and `analyze_memory_md` parses its index links.
/// * `project_state_dir` — the project slug dir (`.../<slug>`) where main-session
///   `.jsonl` transcripts live. Passed in (not re-derived from `memory_dir`)
///   because `run_dream_tick` resolves it from `handler.current_project_state_dir()`
///   which can differ from `memory_dir.parent()`; using a different dir than the
///   gate's `list_sessions_touched_since` scanned would desync corpus from gate.
/// * `since_ms` — the prior-consolidation watermark; only sessions modified
///   after it are folded in (mirrors `list_sessions_touched_since`).
///
/// K9 (W-MEMORY-LIFECYCLE 2026-07-09) — the signature gained an optional
/// personal-knowledge-base source. When `knowledge_dir` is `Some` and the
/// directory holds enabled entries, the manifest half gains a
/// `## 个人知识库摘要` section (top `KNOWLEDGE_SUMMARY_MAX_ENTRIES` entries by
/// mtime, `- <name>: <description>` lines, whole section capped at
/// `KNOWLEDGE_SUMMARY_MAX_CHARS`) so dreaming reflects over the user's curated
/// knowledge, not only session transcripts. `None` keeps the pre-K9 corpus
/// bit-for-bit.
pub(crate) fn build_dream_corpus(
    memory_dir: &Path,
    project_state_dir: &Path,
    since_ms: u64,
    knowledge_dir: Option<&Path>,
) -> DreamCorpus {
    let (summary, consumed_watermark_ms) =
        build_recent_sessions_summary(project_state_dir, since_ms);
    let mut manifest = build_memdir_manifest(memory_dir);
    if let Some(knowledge_dir) = knowledge_dir {
        let knowledge = build_knowledge_summary(knowledge_dir);
        if !knowledge.is_empty() {
            if !manifest.is_empty() {
                manifest.push_str("\n\n");
            }
            manifest.push_str(&knowledge);
        }
    }
    DreamCorpus {
        recent_sessions_summary: summary,
        memdir_manifest: manifest,
        consumed_watermark_ms,
    }
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-a (2026-07-16) — 语料装配结果。
/// `consumed_watermark_ms` = 本轮**实际选入语料**的会话里最新的 mtime
/// （None = 没有会话入选）。做梦成功后水位线只推进到该值
/// （`lock::record_consolidation_complete_at`），撞帽未消费的积压留给下轮。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DreamCorpus {
    pub recent_sessions_summary: String,
    pub memdir_manifest: String,
    pub consumed_watermark_ms: Option<u64>,
}

#[cfg(test)]
impl DreamCorpus {
    /// 测试便捷拆解（保持旧断言形态）。
    pub(crate) fn parts(self) -> (String, String) {
        (self.recent_sessions_summary, self.memdir_manifest)
    }
}

/// Convenience wrapper for the manual path: derive `project_state_dir` from
/// `memory_dir` (the manual `build_dream_now_input` has no handler context, so
/// `memory_dir.parent()` is the correct slug dir — same derivation the manual
/// gate uses). `knowledge_dir` semantics mirror `build_dream_corpus` (K9).
pub(crate) fn build_dream_corpus_for_memory_dir(
    memory_dir: &Path,
    since_ms: u64,
    knowledge_dir: Option<&Path>,
) -> DreamCorpus {
    let project_state_dir = project_state_dir_from_memory_dir(memory_dir);
    build_dream_corpus(memory_dir, &project_state_dir, since_ms, knowledge_dir)
}

/// Most-recently-modified main-session transcripts touched since `since_ms`,
/// each compacted to plain conversation text, joined with section headers.
/// Mirrors `list_sessions_touched_since`'s filter (main-session only, mtime past
/// the watermark) so the corpus matches the sessions the gate counted.
fn build_recent_sessions_summary(project_state_dir: &Path, since_ms: u64) -> (String, Option<u64>) {
    let mut metas = match scan_transcript_dir(project_state_dir) {
        Ok(metas) => metas,
        Err(_) => return (String::new(), None),
    };
    // G2 (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16)：canonical 项目并读成员
    // （worktree）项目的转写 —— 所有检出的经验汇入同一份记忆。与
    // `list_sessions_touched_since` 严格同口径（gate 数到的会话语料必须
    // 拿得到）。成员读错 fail-soft 跳过。
    for member_dir in crate::identity_members::member_project_state_dirs(project_state_dir) {
        if let Ok(member_metas) = scan_transcript_dir(&member_dir) {
            metas.extend(member_metas);
        }
    }
    metas.retain(|m| {
        m.task_type.as_deref() == Some(TASK_TYPE_MAIN_SESSION) && m.mtime_ms > since_ms
    });
    // G3-a (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16)：**最旧优先**消费，撞帽时
    // 排干积压而不是永久丢弃。此前取最新 8 个 + 成功后水位线盖到 now，被
    // 截掉的最旧 touched 会话永远落到水位线之下（静默数据丢失）。现在按
    // mtime 升序取前 DREAM_MAX_SESSIONS 个（时序叙事对整理质量也更自然），
    // 返回已消费最新 mtime 作为水位线推进目标；积压 ≤ 帽时两种取法等价。
    metas.sort_by_key(|m| m.mtime_ms);
    if metas.len() > DREAM_MAX_SESSIONS {
        log::info!(
            "dream corpus: {} touched sessions exceed cap {DREAM_MAX_SESSIONS} — \
             consuming the oldest first; the backlog drains on following cycles",
            metas.len()
        );
    }
    metas.truncate(DREAM_MAX_SESSIONS);
    let consumed_watermark_ms = metas.iter().map(|m| m.mtime_ms).max();

    let mut sections: Vec<String> = Vec::new();
    for meta in &metas {
        let body = match read_transcript_content(&meta.path, DREAM_MAX_TRANSCRIPT_CHARS) {
            Ok(body) => body,
            Err(_) => continue,
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let label = meta
            .session_id
            .as_deref()
            .or_else(|| meta.path.file_name().and_then(|n| n.to_str()))
            .unwrap_or("session");
        sections.push(format!("## Session {label}\n\n{body}"));
    }
    (sections.join("\n\n"), consumed_watermark_ms)
}

/// Newline-separated manifest of the memdir index links (`- <title> (<target>)`,
/// with the hook appended when present). Empty when MEMORY.md is absent or has
/// no links.
fn build_memdir_manifest(memory_dir: &Path) -> String {
    let report = match analyze_memory_md(memory_dir) {
        Ok(report) => report,
        Err(_) => return String::new(),
    };
    report
        .links
        .iter()
        .map(|link| match &link.hook {
            Some(hook) if !hook.is_empty() => {
                format!("- {} ({}) — {}", link.title, link.target, hook)
            }
            _ => format!("- {} ({})", link.title, link.target),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One knowledge-base entry surfaced into the corpus.
struct KnowledgeEntry {
    name: String,
    description: String,
    mtime_ms: u64,
}

/// K9 — `## 个人知识库摘要` section: enabled `*.md` entries under
/// `knowledge_dir`, newest first, `- <name>: <description>` per entry
/// (frontmatter `name`/`description`; fallback = file stem + first body
/// line). Empty string when the directory is missing / unreadable / holds no
/// enabled entries (the section is then omitted entirely). Fail-soft
/// throughout — a knowledge read problem never aborts a dream.
fn build_knowledge_summary(knowledge_dir: &Path) -> String {
    let entries = match std::fs::read_dir(knowledge_dir) {
        Ok(entries) => entries,
        Err(_) => return String::new(),
    };

    let mut items: Vec<KnowledgeEntry> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file()
            || path
                .file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !n.ends_with(".md"))
        {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let scan = scan_knowledge_frontmatter(&raw);
        if !scan.enabled {
            // Disabled entries are excluded from every consumption channel
            // (SoT K9) — the dream corpus included.
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("knowledge")
            .to_string();
        let name = if scan.name.is_empty() {
            stem
        } else {
            scan.name
        };
        let description = if scan.description.is_empty() {
            first_body_line(&raw)
        } else {
            scan.description
        };
        let mtime_ms = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        items.push(KnowledgeEntry {
            name,
            description,
            mtime_ms,
        });
    }
    if items.is_empty() {
        return String::new();
    }

    // Newest first — recent knowledge carries the freshest signal.
    items.sort_by_key(|i| std::cmp::Reverse(i.mtime_ms));
    items.truncate(KNOWLEDGE_SUMMARY_MAX_ENTRIES);

    let mut section = String::from("## 个人知识库摘要\n");
    for item in &items {
        if item.description.is_empty() {
            section.push_str(&format!("\n- {}", item.name));
        } else {
            section.push_str(&format!("\n- {}: {}", item.name, item.description));
        }
    }
    truncate_chars(&section, KNOWLEDGE_SUMMARY_MAX_CHARS)
}

/// Minimal knowledge frontmatter scan: `name` / `description` / `enabled`
/// (default `true`; only a literal `false` disables). Mirrors
/// `tier3_auto_dream::scan_insight_frontmatter`'s hand-rolled style — the
/// entries are small user-authored markdown, no YAML crate needed.
struct KnowledgeFrontmatter {
    name: String,
    description: String,
    enabled: bool,
}

fn scan_knowledge_frontmatter(raw: &str) -> KnowledgeFrontmatter {
    let mut out = KnowledgeFrontmatter {
        name: String::new(),
        description: String::new(),
        enabled: true,
    };
    let mut in_frontmatter = false;
    for (i, line) in raw.lines().enumerate() {
        let t = line.trim();
        if i == 0 {
            if t == "---" {
                in_frontmatter = true;
                continue;
            }
            break;
        }
        if !in_frontmatter || t == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("name:") {
            out.name = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("description:") {
            out.description = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("enabled:") {
            out.enabled = !v.trim().eq_ignore_ascii_case("false");
        }
    }
    out
}

/// First non-empty line after the frontmatter block (fallback description).
fn first_body_line(raw: &str) -> String {
    let mut lines = raw.lines().peekable();
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next(); // opening ---
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    lines
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Truncate to at most `max` chars on a char boundary (multi-byte safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// K10 (W-MEMORY-LIFECYCLE 2026-07-09) — shallow directory inventory for a
/// 专项检测 watch target, folded into the dream corpus so the model can orient
/// inside the watched tree. Names only (relative to `root`, directories with a
/// trailing `/`), depth ≤ `WATCH_INVENTORY_MAX_DEPTH`, at most
/// `WATCH_INVENTORY_MAX_ENTRIES` entries; `.git` / `node_modules` / `target`
/// are never listed or descended into. The header carries the watch `focus`
/// text when present. Fail-soft: unreadable directories are skipped.
pub(crate) fn build_watch_inventory(root: &Path, focus: Option<&str>) -> String {
    let mut section = format!("## 检测目标目录清单（{}）\n", root.display());
    if let Some(focus) = focus {
        let focus = focus.trim();
        if !focus.is_empty() {
            // Keep the header single-line regardless of stored focus text.
            let one_line = focus.replace(['\r', '\n'], " ");
            section.push_str(&format!("关注点: {one_line}\n"));
        }
    }
    section.push('\n');

    let mut entries: Vec<String> = Vec::new();
    let mut clipped = false;
    collect_inventory_entries(root, Path::new(""), 1, &mut entries, &mut clipped);
    if entries.is_empty() {
        section.push_str("(目录为空或不可读)");
        return section;
    }
    for entry in &entries {
        section.push_str(&format!("- {entry}\n"));
    }
    if clipped {
        section.push_str(&format!(
            "- …（超过 {WATCH_INVENTORY_MAX_ENTRIES} 项，其余省略）\n"
        ));
    }
    section.trim_end().to_string()
}

/// Depth-first, name-sorted walk under `dir`. `rel` is the path of `dir`
/// relative to the inventory root (`""` for the root itself); `depth` is the
/// depth of `dir`'s CHILDREN. Appends to `entries` until the cap trips.
fn collect_inventory_entries(
    dir: &Path,
    rel: &Path,
    depth: usize,
    entries: &mut Vec<String>,
    clipped: &mut bool,
) {
    if depth > WATCH_INVENTORY_MAX_DEPTH {
        return;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(_) => return, // fail-soft: unreadable subtree skipped.
    };
    let mut children: Vec<(String, bool, PathBuf)> = Vec::new();
    for entry in read.filter_map(Result::ok) {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // `file_type` does not follow symlinks — a symlinked dir is listed as
        // a plain entry but never descended into (no cycle risk).
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && WATCH_INVENTORY_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        children.push((name, is_dir, entry.path()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, is_dir, path) in children {
        if entries.len() >= WATCH_INVENTORY_MAX_ENTRIES {
            *clipped = true;
            return;
        }
        let child_rel = rel.join(&name);
        // Normalize to forward slashes so the inventory reads identically on
        // every platform (it is prompt text, not a filesystem path).
        let display = child_rel.to_string_lossy().replace('\\', "/");
        if is_dir {
            entries.push(format!("{display}/"));
            collect_inventory_entries(&path, &child_rel, depth + 1, entries, clipped);
        } else {
            entries.push(display);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    const SESSION_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SESSION_B: &str = "660e8400-e29b-41d4-a716-446655440000";

    fn write_main_session(project_state_dir: &Path, session_id: &str, mtime_s: i64, body: &str) {
        let path = project_state_dir.join(format!("{session_id}.jsonl"));
        fs::write(&path, body).unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(mtime_s, 0)).unwrap();
    }

    fn user_assistant_line(user: &str, assistant: &str) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"user","message":{"role":"user","content":user}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":assistant}}),
        )
    }

    #[test]
    fn corpus_includes_real_transcript_text_and_memdir_links() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        write_main_session(
            project.path(),
            SESSION_A,
            1_700_000_100,
            &user_assistant_line("how does dream corpus work", "it reads recent transcripts"),
        );
        write_main_session(
            project.path(),
            SESSION_B,
            1_700_000_200,
            &user_assistant_line("what about the memdir", "it lists MEMORY.md index links"),
        );

        fs::write(
            memory_dir.join("MEMORY.md"),
            "## Index\n- [Alpha topic](alpha.md) — a useful hook\n- [Beta topic](beta.md)\n",
        )
        .unwrap();

        let (summary, manifest) = build_dream_corpus(&memory_dir, project.path(), 0, None).parts();

        // Real dialogue text from BOTH sessions is present (not a placeholder).
        assert!(
            summary.contains("how does dream corpus work"),
            "summary: {summary}"
        );
        assert!(
            summary.contains("it reads recent transcripts"),
            "summary: {summary}"
        );
        assert!(
            summary.contains("what about the memdir"),
            "summary: {summary}"
        );
        assert!(!summary.trim().is_empty());

        // Manifest lists MEMORY.md links, hook appended when present.
        assert!(
            manifest.contains("- Alpha topic (alpha.md) — a useful hook"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("- Beta topic (beta.md)"),
            "manifest: {manifest}"
        );
    }

    #[test]
    fn corpus_summary_filters_by_since_ms_watermark() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        write_main_session(
            project.path(),
            SESSION_A,
            1_700_000_100,
            &user_assistant_line("old session content", "old reply"),
        );
        write_main_session(
            project.path(),
            SESSION_B,
            1_700_000_300,
            &user_assistant_line("new session content", "new reply"),
        );

        // Watermark between the two mtimes (in ms): only the newer survives.
        let corpus = build_dream_corpus(&memory_dir, project.path(), 1_700_000_200_000, None);
        let summary = &corpus.recent_sessions_summary;
        assert!(
            summary.contains("new session content"),
            "summary: {summary}"
        );
        assert!(
            !summary.contains("old session content"),
            "summary: {summary}"
        );
        // G3-a：消费水位线 = 入选会话里最新的 mtime。
        assert_eq!(corpus.consumed_watermark_ms, Some(1_700_000_300_000));
    }

    /// G2 (W-MEMORY-SELF-EVOLVE-DGM 2026-07-16) — canonical 项目并读成员
    /// （worktree）项目的转写；水位线覆盖成员里最新的会话。
    #[test]
    fn corpus_merges_member_worktree_transcripts() {
        let base = TempDir::new().unwrap();
        let projects = base.path().join("projects");
        let canonical = projects.join("D--crabcode");
        let member = projects.join("D--crabcode-wt-x");
        let memory_dir = canonical.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::create_dir_all(&member).unwrap();
        let derived = canonical.join(".memory-rust-derived");
        fs::create_dir_all(&derived).unwrap();
        fs::write(
            derived.join("identity-members.json"),
            r#"{"members":["D--crabcode-wt-x"]}"#,
        )
        .unwrap();

        write_main_session(
            &canonical,
            SESSION_A,
            1_700_000_100,
            &user_assistant_line("main tree session", "ok"),
        );
        write_main_session(
            &member,
            SESSION_B,
            1_700_000_300,
            &user_assistant_line("worktree session content", "ok"),
        );

        let corpus = build_dream_corpus(&memory_dir, &canonical, 0, None);
        assert!(
            corpus.recent_sessions_summary.contains("main tree session"),
            "summary: {}",
            corpus.recent_sessions_summary
        );
        assert!(
            corpus
                .recent_sessions_summary
                .contains("worktree session content"),
            "成员转写必须并入语料: {}",
            corpus.recent_sessions_summary
        );
        assert_eq!(
            corpus.consumed_watermark_ms,
            Some(1_700_000_300_000),
            "水位线覆盖成员里最新会话"
        );
    }

    #[test]
    fn corpus_is_failsoft_on_missing_dirs() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("does-not-exist");
        // No transcripts, no MEMORY.md → both halves empty, no panic.
        let corpus = build_dream_corpus(&memory_dir, project.path(), 0, None);
        assert!(corpus.recent_sessions_summary.is_empty());
        assert!(corpus.memdir_manifest.is_empty());
        assert_eq!(corpus.consumed_watermark_ms, None, "无会话 = 无消费水位线");
    }

    #[test]
    fn corpus_for_memory_dir_derives_project_state_dir_from_parent() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        write_main_session(
            project.path(),
            SESSION_A,
            1_700_000_100,
            &user_assistant_line("derive parent dir", "ok"),
        );

        let (summary, _) = build_dream_corpus_for_memory_dir(&memory_dir, 0, None).parts();
        assert!(summary.contains("derive parent dir"), "summary: {summary}");
    }

    // ── K9: personal knowledge base summary ──

    fn write_knowledge(dir: &Path, file: &str, frontmatter: &str, body: &str, mtime_s: i64) {
        let path = dir.join(file);
        fs::write(&path, format!("---\n{frontmatter}\n---\n\n{body}\n")).unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(mtime_s, 0)).unwrap();
    }

    #[test]
    fn corpus_with_knowledge_dir_appends_summary_section() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(
            memory_dir.join("MEMORY.md"),
            "## Index\n- [Alpha](alpha.md)\n",
        )
        .unwrap();
        let knowledge = project.path().join("knowledge");
        fs::create_dir_all(&knowledge).unwrap();
        write_knowledge(
            &knowledge,
            "rust-style.md",
            "type: knowledge\nname: Rust 风格约定\ndescription: 团队 Rust 代码风格\nenabled: true",
            "正文",
            1_700_000_200,
        );
        // Disabled entry — excluded from every consumption channel.
        write_knowledge(
            &knowledge,
            "secret.md",
            "type: knowledge\nname: 已停用条目\ndescription: 不应出现\nenabled: false",
            "正文",
            1_700_000_300,
        );
        // No name/description → falls back to file stem + first body line.
        write_knowledge(
            &knowledge,
            "fallback-entry.md",
            "type: knowledge",
            "首行描述文本",
            1_700_000_100,
        );

        let (_, manifest) =
            build_dream_corpus(&memory_dir, project.path(), 0, Some(&knowledge)).parts();

        // Existing memdir manifest half is intact, knowledge section appended.
        assert!(
            manifest.contains("- Alpha (alpha.md)"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("## 个人知识库摘要"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("- Rust 风格约定: 团队 Rust 代码风格"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("- fallback-entry: 首行描述文本"),
            "fallback = stem + first body line, manifest: {manifest}"
        );
        assert!(
            !manifest.contains("已停用条目"),
            "disabled entries excluded"
        );
        // Newest-first ordering: the newer entry precedes the older one.
        let newer = manifest.find("Rust 风格约定").unwrap();
        let older = manifest.find("fallback-entry").unwrap();
        assert!(newer < older, "entries must be mtime-descending");
    }

    #[test]
    fn corpus_with_none_or_missing_knowledge_dir_omits_section() {
        let project = TempDir::new().unwrap();
        let memory_dir = project.path().join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(
            memory_dir.join("MEMORY.md"),
            "## Index\n- [Alpha](alpha.md)\n",
        )
        .unwrap();
        write_main_session(
            project.path(),
            SESSION_A,
            1_700_000_100,
            &user_assistant_line("old call path", "unchanged"),
        );

        let legacy = build_dream_corpus(&memory_dir, project.path(), 0, None);
        assert!(!legacy.memdir_manifest.contains("个人知识库摘要"));

        // Missing / empty knowledge dir → section omitted entirely.
        let missing = project.path().join("no-such-knowledge");
        let (_, manifest) =
            build_dream_corpus(&memory_dir, project.path(), 0, Some(&missing)).parts();
        assert!(!manifest.contains("个人知识库摘要"));
    }

    #[test]
    fn knowledge_summary_caps_entries_and_chars() {
        let project = TempDir::new().unwrap();
        let knowledge = project.path().join("knowledge");
        fs::create_dir_all(&knowledge).unwrap();
        // 12 entries with distinct mtimes → only the newest 10 survive.
        for i in 0..12i64 {
            write_knowledge(
                &knowledge,
                &format!("entry-{i:02}.md"),
                &format!("name: 条目{i:02}\ndescription: {}", "描".repeat(300)),
                "body",
                1_700_000_000 + i,
            );
        }

        let section = build_knowledge_summary(&knowledge);
        assert!(section.starts_with("## 个人知识库摘要"));
        // Newest (11) present, oldest (00, 01) clipped by the entry cap.
        assert!(section.contains("条目11"));
        assert!(!section.contains("条目00"));
        assert!(!section.contains("条目01"));
        assert!(
            section.chars().count() <= KNOWLEDGE_SUMMARY_MAX_CHARS,
            "whole section capped at {KNOWLEDGE_SUMMARY_MAX_CHARS} chars, got {}",
            section.chars().count()
        );
    }

    // ── K10: watch inventory ──

    #[test]
    fn watch_inventory_lists_shallow_tree_with_focus_and_skips_vendor_dirs() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("src/util")).unwrap();
        fs::write(root.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.path().join("src/util/helper.rs"), "// helper").unwrap();
        fs::write(root.path().join("README.md"), "# readme").unwrap();
        // Skipped vendor dirs — never listed, never descended into.
        fs::create_dir_all(root.path().join(".git/objects")).unwrap();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.path().join("target/debug")).unwrap();
        // Depth 4 — beyond the ≤3 shallow bound.
        fs::create_dir_all(root.path().join("a/b/c")).unwrap();
        fs::write(root.path().join("a/b/c/deep.txt"), "deep").unwrap();

        let inventory = build_watch_inventory(root.path(), Some("关注 util 模块"));

        assert!(
            inventory.starts_with("## 检测目标目录清单（"),
            "{inventory}"
        );
        assert!(inventory.contains("关注点: 关注 util 模块"), "{inventory}");
        assert!(inventory.contains("- README.md"), "{inventory}");
        assert!(inventory.contains("- src/"), "{inventory}");
        assert!(inventory.contains("- src/main.rs"), "{inventory}");
        assert!(inventory.contains("- src/util/"), "{inventory}");
        assert!(inventory.contains("- src/util/helper.rs"), "{inventory}");
        // Depth ≤ 3: `a/b/c/` (depth 3) is listed, its contents are not.
        assert!(inventory.contains("- a/b/c/"), "{inventory}");
        assert!(!inventory.contains("deep.txt"), "{inventory}");
        // Vendor dirs skipped entirely.
        assert!(!inventory.contains(".git"), "{inventory}");
        assert!(!inventory.contains("node_modules"), "{inventory}");
        assert!(!inventory.contains("target/"), "{inventory}");
    }

    #[test]
    fn watch_inventory_caps_entries_and_handles_empty_root() {
        let root = TempDir::new().unwrap();
        for i in 0..(WATCH_INVENTORY_MAX_ENTRIES + 20) {
            fs::write(root.path().join(format!("file-{i:04}.txt")), "x").unwrap();
        }
        let inventory = build_watch_inventory(root.path(), None);
        let listed = inventory
            .lines()
            .filter(|l| l.starts_with("- ") && !l.starts_with("- …"))
            .count();
        assert_eq!(listed, WATCH_INVENTORY_MAX_ENTRIES);
        assert!(inventory.contains("其余省略"), "{inventory}");
        assert!(!inventory.contains("关注点:"), "no focus line when None");

        let empty = TempDir::new().unwrap();
        let inventory = build_watch_inventory(empty.path(), None);
        assert!(inventory.contains("(目录为空或不可读)"), "{inventory}");
    }
}
