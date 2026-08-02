use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub const TASK_TYPE_AGENT: &str = "AgentTask";
pub const TASK_TYPE_MAIN_SESSION: &str = "MainSessionTask";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptMeta {
    pub path: PathBuf,
    pub mtime_ms: u64,
    pub size_bytes: u64,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub task_type: Option<String>,
}

pub fn scan_transcript_dir(project_dir: &Path) -> Result<Vec<TranscriptMeta>, BoxError> {
    let mut metas = Vec::new();
    let project_dir = match dunce::canonicalize(project_dir) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(metas),
        Err(e) => return Err(e.into()),
    };

    let entries = match fs::read_dir(&project_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(metas),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_file() {
            if let Some(meta) = scan_main_session_file(&path)? {
                metas.push(meta);
            }
        } else if file_type.is_dir() {
            scan_session_subagents_dir(&path, &mut metas)?;
        }
    }

    metas.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(metas)
}

/// W-MEMORY-EVOLUTION PR-2 (2026-05-29) — read + compact a transcript `.jsonl`
/// into plain conversation text for Tier-1/2/3 LLM payloads.
///
/// `scan_transcript_dir` only indexes metadata; this reads the actual body.
/// Each line is one JSON record (`{"type":"user"|"assistant","message":{
/// "role":..,"content":<string|blocks[]>},"isSidechain":bool,..}`). We keep
/// only top-level (non-sidechain) user/assistant turns and extract their text
/// (string content directly; block-array content → concatenated `text` blocks,
/// skipping `thinking`/`tool_use`/`tool_result`). Lines that fail to parse are
/// skipped (robust against partial/corrupt tails).
///
/// Compaction: turns are joined `"<role>: <text>"` separated by blank lines.
/// If the result exceeds `max_chars`, the **tail** (most recent turns) is kept
/// — recent context matters most for session-note consolidation — and the head
/// is replaced with a truncation marker.
pub fn read_transcript_content(path: &Path, max_chars: usize) -> Result<String, BoxError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.into()),
    };

    let mut turns: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // skip unparseable / partial lines
        };
        // Skip subagent (sidechain) turns — they belong to forked agents, not
        // the main session note.
        if value
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let message = match value.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("type").and_then(serde_json::Value::as_str))
            .unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = extract_message_text(message.get("content"));
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        turns.push(format!("{role}: {text}"));
    }

    let joined = turns.join("\n\n");
    Ok(tail_truncate(&joined, max_chars))
}

/// Extract plain text from a transcript `message.content` value: a bare string
/// is returned as-is; a block array contributes its `text` blocks (joined by
/// space), skipping `thinking` / `tool_use` / `tool_result` blocks.
pub(crate) fn extract_message_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::new();
            for block in blocks {
                if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                    if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                        if !t.is_empty() {
                            parts.push(t);
                        }
                    }
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

/// Keep the tail of `text` so the total stays within `max_chars`, prefixing a
/// truncation marker when content was dropped. Truncates on a char boundary.
fn tail_truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    const MARKER: &str = "…[earlier turns truncated]…\n\n";
    let keep = max_chars.saturating_sub(MARKER.chars().count());
    let total = text.chars().count();
    let skip = total.saturating_sub(keep);
    let tail: String = text.chars().skip(skip).collect();
    format!("{MARKER}{tail}")
}

fn scan_main_session_file(path: &Path) -> Result<Option<TranscriptMeta>, BoxError> {
    let Some(file_name) = file_name_str(path) else {
        return Ok(None);
    };
    let Some(session_id) = parse_main_session_file_name(file_name) else {
        return Ok(None);
    };

    transcript_meta(path, Some(session_id), None, Some(TASK_TYPE_MAIN_SESSION))
}

fn scan_session_subagents_dir(
    session_dir: &Path,
    metas: &mut Vec<TranscriptMeta>,
) -> Result<(), BoxError> {
    let Some(session_id) = file_name_str(session_dir).filter(|name| is_uuid(name)) else {
        return Ok(());
    };
    let subagents_dir = session_dir.join("subagents");
    match fs::read_dir(&subagents_dir) {
        Ok(_) => scan_subagent_tree(&subagents_dir, session_id, metas),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn scan_subagent_tree(
    dir: &Path,
    session_id: &str,
    metas: &mut Vec<TranscriptMeta>,
) -> Result<(), BoxError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            scan_subagent_tree(&path, session_id, metas)?;
        } else if file_type.is_file() {
            let Some(file_name) = file_name_str(&path) else {
                continue;
            };
            let Some(task_id) = parse_agent_transcript_file_name(file_name) else {
                continue;
            };
            if let Some(meta) = transcript_meta(
                &path,
                Some(session_id.to_string()),
                Some(task_id),
                Some(TASK_TYPE_AGENT),
            )? {
                metas.push(meta);
            }
        }
    }

    Ok(())
}

fn transcript_meta(
    path: &Path,
    session_id: Option<String>,
    task_id: Option<String>,
    task_type: Option<&str>,
) -> Result<Option<TranscriptMeta>, BoxError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(None);
    }

    let mtime_ms = modified_time_ms(metadata.modified()?)?;
    Ok(Some(TranscriptMeta {
        path: normalize_existing_path(path),
        mtime_ms,
        size_bytes: metadata.len(),
        session_id,
        task_id,
        task_type: task_type.map(str::to_string),
    }))
}

fn parse_main_session_file_name(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".jsonl")?;
    if is_uuid(stem) {
        return Some(stem.to_string());
    }

    let (session_id, topic_id) = stem.split_once("-topic-")?;
    if is_uuid(session_id) && !topic_id.is_empty() {
        Some(session_id.to_string())
    } else {
        None
    }
}

fn parse_agent_transcript_file_name(file_name: &str) -> Option<String> {
    let stem = file_name.strip_prefix("agent-")?.strip_suffix(".jsonl")?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

fn file_name_str(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    value.bytes().enumerate().all(|(idx, byte)| match idx {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

fn modified_time_ms(time: SystemTime) -> Result<u64, BoxError> {
    Ok(time.duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| normalize_path_components(path))
}

fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER_SESSION_ID: &str = "660e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn transcript_index_scans_main_and_subagent_transcripts() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join(format!("{SESSION_ID}.jsonl"));
        fs::write(&main, "{}\n").unwrap();

        let agent = dir
            .path()
            .join(SESSION_ID)
            .join("subagents")
            .join("workflows")
            .join("run-1")
            .join("agent-a12345678.jsonl");
        fs::create_dir_all(agent.parent().unwrap()).unwrap();
        fs::write(&agent, "{}\n").unwrap();

        let metas = scan_transcript_dir(dir.path()).unwrap();
        assert_eq!(metas.len(), 2);

        let main_meta = metas
            .iter()
            .find(|meta| meta.path == canonical(&main))
            .unwrap();
        assert_eq!(main_meta.session_id.as_deref(), Some(SESSION_ID));
        assert_eq!(main_meta.task_id, None);
        assert_eq!(main_meta.task_type.as_deref(), Some(TASK_TYPE_MAIN_SESSION));

        let agent_meta = metas
            .iter()
            .find(|meta| meta.path == canonical(&agent))
            .unwrap();
        assert_eq!(agent_meta.session_id.as_deref(), Some(SESSION_ID));
        assert_eq!(agent_meta.task_id.as_deref(), Some("a12345678"));
        assert_eq!(agent_meta.task_type.as_deref(), Some(TASK_TYPE_AGENT));
    }

    #[test]
    fn transcript_index_empty_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        assert!(scan_transcript_dir(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn transcript_index_skips_damaged_and_unmatched_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("not-a-session.jsonl"), "{}\n").unwrap();
        fs::write(dir.path().join(format!("{SESSION_ID}.txt")), "{}\n").unwrap();
        fs::write(dir.path().join(format!("{SESSION_ID}.jsonl")), "").unwrap();

        let bad_agent = dir
            .path()
            .join("not-a-session")
            .join("subagents")
            .join("agent-a12345678.jsonl");
        fs::create_dir_all(bad_agent.parent().unwrap()).unwrap();
        fs::write(&bad_agent, "{}\n").unwrap();

        let empty_agent = dir
            .path()
            .join(OTHER_SESSION_ID)
            .join("subagents")
            .join("agent-.jsonl");
        fs::create_dir_all(empty_agent.parent().unwrap()).unwrap();
        fs::write(&empty_agent, "{}\n").unwrap();

        assert!(scan_transcript_dir(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn transcript_index_records_mtime_and_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(format!("{SESSION_ID}.jsonl"));
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(1_700_000_123, 0)).unwrap();

        let metas = scan_transcript_dir(dir.path()).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].size_bytes, 16);
        assert_eq!(metas[0].mtime_ms, 1_700_000_123_000);
    }

    #[test]
    fn transcript_index_normalizes_paths() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(format!("{SESSION_ID}.jsonl"));
        fs::write(&path, "{}\n").unwrap();

        let input = dir.path().join(".").join("nested").join("..");
        fs::create_dir_all(dir.path().join("nested")).unwrap();

        let metas = scan_transcript_dir(&input).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].path, canonical(&path));
    }

    #[test]
    fn transcript_index_accepts_topic_session_files() {
        let dir = TempDir::new().unwrap();
        let path = dir
            .path()
            .join(format!("{SESSION_ID}-topic-review%2Fnotes.jsonl"));
        fs::write(&path, "{}\n").unwrap();

        let metas = scan_transcript_dir(dir.path()).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id.as_deref(), Some(SESSION_ID));
        assert_eq!(metas[0].task_type.as_deref(), Some(TASK_TYPE_MAIN_SESSION));
    }

    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap()
    }

    // ── W-MEMORY-EVOLUTION PR-2 — read_transcript_content ──

    #[test]
    fn read_transcript_content_extracts_user_and_assistant_text() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"hello worker"},"isSidechain":false}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hi there"}]},"isSidechain":false}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();

        let out = read_transcript_content(&path, 10_000).unwrap();
        assert!(out.contains("user: hello worker"), "got: {out}");
        assert!(out.contains("assistant: hi there"), "got: {out}");
        // thinking + tool blocks excluded.
        assert!(!out.contains("hmm"), "thinking must be excluded: {out}");
    }

    #[test]
    fn read_transcript_content_skips_sidechain_and_bad_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"keep me"},"isSidechain":false}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":"subagent noise"},"isSidechain":true}"#,
            r#"not json at all"#,
            r#"{"type":"summary","summary":"x"}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();

        let out = read_transcript_content(&path, 10_000).unwrap();
        assert!(out.contains("keep me"));
        assert!(!out.contains("subagent noise"), "sidechain excluded");
        assert!(!out.contains("summary"));
    }

    #[test]
    fn read_transcript_content_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let out = read_transcript_content(&dir.path().join("none.jsonl"), 1000).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn read_transcript_content_tail_truncates_keeping_recent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":"turn-{i:04}"}},"isSidechain":false}}"#
            ));
        }
        fs::write(&path, lines.join("\n")).unwrap();

        let out = read_transcript_content(&path, 120).unwrap();
        assert!(out.chars().count() <= 120, "len {}", out.chars().count());
        assert!(out.contains("…[earlier turns truncated]…"), "marker: {out}");
        // Most recent turn preserved; earliest dropped.
        assert!(out.contains("turn-0049"), "recent kept: {out}");
        assert!(!out.contains("turn-0000"), "oldest dropped: {out}");
    }
}
