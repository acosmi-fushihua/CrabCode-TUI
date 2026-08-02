use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const SKIP_BASENAMES: &[&str] = &["MEMORY.md", "SESSION.md"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyHashRecord {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub hash: [u8; 32],
    pub hash_hex: String,
    pub body_bytes: u64,
    pub mtime_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DedupGroup {
    pub hash: [u8; 32],
    pub hash_hex: String,
    pub primary: BodyHashRecord,
    pub duplicates: Vec<BodyHashRecord>,
}

/// SHA-256 of a memory body. Callers must pass body text with frontmatter
/// already excluded.
#[must_use]
pub fn body_hash(body: &str) -> [u8; 32] {
    let digest = Sha256::digest(body.as_bytes());
    digest.into()
}

#[must_use]
pub fn body_hash_hex(body: &str) -> String {
    hex_encode(&body_hash(body))
}

/// Return markdown body text with TS-style leading frontmatter excluded.
#[must_use]
pub fn memory_body(markdown: &str) -> &str {
    let Some((_, body_start)) = extract_frontmatter(markdown) else {
        return markdown;
    };
    &markdown[body_start..]
}

#[must_use]
pub fn memory_body_hash(markdown: &str) -> [u8; 32] {
    body_hash(memory_body(markdown))
}

#[must_use]
pub fn memory_body_hash_hex(markdown: &str) -> String {
    hex_encode(&memory_body_hash(markdown))
}

pub fn find_body_duplicates(
    memory_dir: &Path,
    candidate_hash: &[u8; 32],
) -> Result<Vec<(PathBuf, u64)>, BoxError> {
    let mut matches = scan_body_hashes(memory_dir)?
        .into_iter()
        .filter(|record| &record.hash == candidate_hash)
        .map(|record| (record.path, record.mtime_ms))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(matches)
}

pub fn find_dedup_groups(memory_dir: &Path) -> Result<Vec<DedupGroup>, BoxError> {
    let mut by_hash: BTreeMap<String, Vec<BodyHashRecord>> = BTreeMap::new();
    for record in scan_body_hashes(memory_dir)? {
        by_hash
            .entry(record.hash_hex.clone())
            .or_default()
            .push(record);
    }

    let mut groups = Vec::new();
    for (hash_hex, mut records) in by_hash {
        if records.len() < 2 {
            continue;
        }
        records.sort_by(|left, right| {
            left.mtime_ms.cmp(&right.mtime_ms).then_with(|| {
                relative_key(&left.relative_path).cmp(&relative_key(&right.relative_path))
            })
        });

        let primary = records.remove(0);
        groups.push(DedupGroup {
            hash: primary.hash,
            hash_hex,
            primary,
            duplicates: records,
        });
    }

    Ok(groups)
}

/// Payload-side marker for a duplicate point. This intentionally returns only
/// metadata; truth-source markdown files are never deleted or rewritten here.
#[must_use]
pub fn dedup_payload_fields(dedup_of: impl Into<String>) -> HashMap<String, Value> {
    let mut fields = HashMap::new();
    fields.insert("dedup_of".to_owned(), json!(dedup_of.into()));
    fields
}

pub fn scan_body_hashes(memory_dir: &Path) -> Result<Vec<BodyHashRecord>, BoxError> {
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    for entry in WalkDir::new(memory_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = entry?;
        if !entry.file_type().is_file() || !is_memory_markdown(entry.path()) {
            continue;
        }

        let content = std::fs::read_to_string(entry.path())?;
        let body = memory_body(&content);
        let hash = body_hash(body);
        let metadata = entry.metadata()?;
        let mtime_ms = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let relative_path = entry
            .path()
            .strip_prefix(memory_dir)
            .unwrap_or(entry.path())
            .to_path_buf();

        records.push(BodyHashRecord {
            path: entry.path().to_path_buf(),
            relative_path,
            hash,
            hash_hex: hex_encode(&hash),
            body_bytes: body.len() as u64,
            mtime_ms,
        });
    }

    records.sort_by(|left, right| {
        relative_key(&left.relative_path).cmp(&relative_key(&right.relative_path))
    });
    Ok(records)
}

fn extract_frontmatter(markdown: &str) -> Option<(&str, usize)> {
    if !markdown.starts_with("---") {
        return None;
    }

    let content_start = opening_content_start(markdown)?;
    let close_rel = markdown[content_start..].find("---")?;
    let close_start = content_start + close_rel;
    let body_start = consume_whitespace(markdown, close_start + 3);

    Some((&markdown[content_start..close_start], body_start))
}

fn opening_content_start(markdown: &str) -> Option<usize> {
    let mut last_newline_end = None;
    for (offset, ch) in markdown[3..].char_indices() {
        if !ch.is_whitespace() {
            break;
        }
        if ch == '\n' {
            last_newline_end = Some(3 + offset + ch.len_utf8());
        }
    }
    last_newline_end
}

fn consume_whitespace(markdown: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, ch) in markdown[start..].char_indices() {
        if !ch.is_whitespace() {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    end
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }

    let name = entry.file_name().to_string_lossy();
    !(entry.file_type().is_dir() && (name == "logs" || name == ".rust-derived"))
}

fn is_memory_markdown(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("md")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !SKIP_BASENAMES.contains(&name))
}

fn relative_key(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn memory_doc(description: &str, body: &str) -> String {
        format!("---\ntype: project\ndescription: {description}\n---\n{body}")
    }

    #[test]
    fn dedup_hash_body_hash_excludes_frontmatter() {
        let first = memory_doc("first", "same body\n");
        let second = memory_doc("second", "same body\n");
        let changed = memory_doc("second", "different body\n");

        assert_eq!(memory_body_hash(&first), memory_body_hash(&second));
        assert_ne!(memory_body_hash(&first), memory_body_hash(&changed));
        assert_eq!(body_hash("same body\n"), memory_body_hash(&first));
    }

    #[test]
    fn dedup_hash_find_body_duplicates_returns_matching_files_only() {
        let dir = TempDir::new().unwrap();
        let primary = dir.path().join("primary.md");
        let duplicate = dir.path().join("nested").join("duplicate.md");
        let unrelated = dir.path().join("unrelated.md");

        write_file(&primary, &memory_doc("primary", "shared body\n"));
        write_file(&duplicate, &memory_doc("duplicate", "shared body\n"));
        write_file(&unrelated, &memory_doc("other", "other body\n"));

        let hash = body_hash("shared body\n");
        let matches = find_body_duplicates(dir.path(), &hash).unwrap();

        assert_eq!(matches.len(), 2);
        assert!(matches.iter().any(|(path, _)| path == &primary));
        assert!(matches.iter().any(|(path, _)| path == &duplicate));
        assert!(!matches.iter().any(|(path, _)| path == &unrelated));
    }

    #[test]
    fn dedup_hash_groups_duplicates_without_touching_markdown() {
        let dir = TempDir::new().unwrap();
        let older = dir.path().join("older.md");
        let newer = dir.path().join("newer.md");
        let unique = dir.path().join("unique.md");
        write_file(&older, &memory_doc("older", "shared body\n"));
        write_file(&newer, &memory_doc("newer", "shared body\n"));
        write_file(&unique, &memory_doc("unique", "unique body\n"));

        set_file_mtime(&older, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
        set_file_mtime(&newer, FileTime::from_unix_time(1_700_000_100, 0)).unwrap();
        let before = fs::read_to_string(&newer).unwrap();

        let groups = find_dedup_groups(dir.path()).unwrap();
        let newer_record = scan_body_hashes(dir.path())
            .unwrap()
            .into_iter()
            .find(|record| record.path == newer)
            .unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].primary.path, older);
        assert_eq!(groups[0].duplicates, vec![newer_record]);
        assert_eq!(
            fs::read_to_string(&groups[0].duplicates[0].path).unwrap(),
            before
        );
    }

    #[test]
    fn dedup_hash_scanner_skips_indexes_logs_session_and_legacy_derived() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir.path().join("topic.md"),
            &memory_doc("topic", "topic\n"),
        );
        write_file(&dir.path().join("MEMORY.md"), "- [Topic](topic.md)\n");
        write_file(&dir.path().join("SESSION.md"), "scratch\n");
        write_file(
            &dir.path().join("logs/2026/05/2026-05-06.md"),
            "daily log\n",
        );
        write_file(
            &dir.path().join(".rust-derived/derived.md"),
            &memory_doc("derived", "derived\n"),
        );

        let records = scan_body_hashes(dir.path()).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].relative_path, PathBuf::from("topic.md"));
    }

    #[test]
    fn dedup_hash_payload_metadata_only_marks_duplicate_point() {
        let fields = dedup_payload_fields("private:primary");

        assert_eq!(fields.get("dedup_of"), Some(&json!("private:primary")));
        assert_eq!(fields.len(), 1);
    }
}
