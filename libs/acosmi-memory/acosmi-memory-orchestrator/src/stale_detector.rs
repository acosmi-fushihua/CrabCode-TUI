use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use walkdir::{DirEntry, WalkDir};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const SKIP_BASENAMES: &[&str] = &["MEMORY.md", "SESSION.md"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StaleReason {
    OldMtime { days: u32 },
    DanglingRef { target: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleReport {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub stale: bool,
    pub stale_reason: String,
    pub reasons: Vec<StaleReason>,
}

impl StaleReport {
    #[must_use]
    pub fn payload_fields(&self) -> HashMap<String, Value> {
        let mut fields = HashMap::new();
        fields.insert("stale".to_owned(), json!(self.stale));
        fields.insert("stale_reason".to_owned(), json!(self.stale_reason));
        fields
    }
}

pub fn detect_stale(
    memory_dir: &Path,
    cwd: &Path,
    max_age_days: u32,
) -> Result<Vec<StaleReport>, BoxError> {
    detect_stale_at(memory_dir, cwd, max_age_days, SystemTime::now())
}

pub fn detect_stale_at(
    memory_dir: &Path,
    cwd: &Path,
    max_age_days: u32,
    now: SystemTime,
) -> Result<Vec<StaleReport>, BoxError> {
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    let mut reports = Vec::new();
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
        let metadata = entry.metadata()?;
        let relative_path = entry
            .path()
            .strip_prefix(memory_dir)
            .unwrap_or(entry.path())
            .to_path_buf();

        let mut reasons = Vec::new();
        if let Ok(age) = now.duration_since(metadata.modified()?) {
            if age > Duration::from_secs(u64::from(max_age_days) * SECONDS_PER_DAY) {
                reasons.push(StaleReason::OldMtime {
                    days: age.as_secs().saturating_div(SECONDS_PER_DAY) as u32,
                });
            }
        }

        for reference in extract_references(&content) {
            if !reference_exists(cwd, memory_dir, &reference) {
                reasons.push(StaleReason::DanglingRef {
                    target: reference.to_reason_target(),
                });
            }
        }

        if reasons.is_empty() {
            continue;
        }

        reports.push(StaleReport {
            path: entry.path().to_path_buf(),
            relative_path,
            stale: true,
            stale_reason: reasons
                .iter()
                .map(stale_reason_text)
                .collect::<Vec<_>>()
                .join("; "),
            reasons,
        });
    }

    reports.sort_by(|left, right| {
        relative_key(&left.relative_path).cmp(&relative_key(&right.relative_path))
    });
    Ok(reports)
}

fn stale_reason_text(reason: &StaleReason) -> String {
    match reason {
        StaleReason::OldMtime { days } => format!("old_mtime:{days}d"),
        StaleReason::DanglingRef { target } => format!("dangling_ref:{target}"),
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ReferenceKind {
    Path,
    Function,
    Flag,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Reference {
    kind: ReferenceKind,
    target: String,
}

impl Reference {
    fn to_reason_target(&self) -> String {
        let prefix = match self.kind {
            ReferenceKind::Path => "path",
            ReferenceKind::Function => "function",
            ReferenceKind::Flag => "flag",
        };
        format!("{prefix}:{}", self.target)
    }
}

fn extract_references(content: &str) -> Vec<Reference> {
    let mut refs = BTreeSet::new();
    for line in content.lines() {
        collect_marker_refs(line, "path:", ReferenceKind::Path, &mut refs);
        collect_marker_refs(line, "function:", ReferenceKind::Function, &mut refs);
        collect_marker_refs(line, "flag:", ReferenceKind::Flag, &mut refs);
    }
    refs.into_iter().collect()
}

fn collect_marker_refs(
    line: &str,
    marker: &str,
    kind: ReferenceKind,
    refs: &mut BTreeSet<Reference>,
) {
    let mut rest = line;
    while let Some(idx) = rest.find(marker) {
        let after_marker = &rest[idx + marker.len()..];
        if let Some((target, consumed)) = parse_reference_target(after_marker) {
            refs.insert(Reference {
                kind: kind.clone(),
                target,
            });
            rest = &after_marker[consumed..];
        } else {
            break;
        }
    }
}

fn parse_reference_target(raw: &str) -> Option<(String, usize)> {
    let trimmed_start = raw.len() - raw.trim_start().len();
    let raw = raw.trim_start();
    if raw.is_empty() {
        return None;
    }

    let first = raw.chars().next()?;
    let (target, consumed) = if first == '"' || first == '\'' || first == '`' {
        let quote_len = first.len_utf8();
        let tail = &raw[quote_len..];
        let end = tail.find(first)?;
        (&tail[..end], quote_len + end + quote_len)
    } else {
        let end = raw
            .char_indices()
            .find_map(|(idx, ch)| {
                if ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '}') {
                    Some(idx)
                } else {
                    None
                }
            })
            .unwrap_or(raw.len());
        (&raw[..end], end)
    };

    let target = trim_target(target);
    if target.is_empty() {
        return None;
    }

    Some((target.to_owned(), trimmed_start + consumed))
}

fn trim_target(target: &str) -> &str {
    target
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches(['.', ',', ';', ')', ']', '}'])
}

fn reference_exists(cwd: &Path, memory_dir: &Path, reference: &Reference) -> bool {
    match reference.kind {
        ReferenceKind::Path => path_reference_exists(cwd, &reference.target),
        ReferenceKind::Function | ReferenceKind::Flag => {
            literal_exists_under_cwd(cwd, memory_dir, &reference.target)
        }
    }
}

fn path_reference_exists(cwd: &Path, target: &str) -> bool {
    if target.starts_with("http://") || target.starts_with("https://") {
        return true;
    }

    let target = target.split('#').next().unwrap_or(target);
    let path = Path::new(target);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if candidate.exists() {
        return true;
    }

    strip_line_suffix(target)
        .map(|without_line| {
            let path = Path::new(without_line);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        })
        .is_some_and(|candidate| candidate.exists())
}

fn strip_line_suffix(target: &str) -> Option<&str> {
    let (prefix, suffix) = target.rsplit_once(':')?;
    if !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
        Some(prefix)
    } else {
        None
    }
}

fn literal_exists_under_cwd(cwd: &Path, memory_dir: &Path, literal: &str) -> bool {
    if literal.is_empty() || !cwd.exists() {
        return false;
    }

    for entry in WalkDir::new(cwd)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| should_search_project_entry(entry, memory_dir))
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > 2 * 1024 * 1024 {
            continue;
        }

        if std::fs::read_to_string(entry.path())
            .map(|content| content.contains(literal))
            .unwrap_or(false)
        {
            return true;
        }
    }

    false
}

fn should_search_project_entry(entry: &DirEntry, memory_dir: &Path) -> bool {
    if entry.path().starts_with(memory_dir) {
        return false;
    }
    if !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !matches!(
        name.as_ref(),
        ".git" | "node_modules" | "target" | "dist" | ".next"
    )
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::UNIX_EPOCH;

    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn memory_doc(body: &str) -> String {
        format!("---\ntype: project\ndescription: stale test\n---\n{body}")
    }

    fn set_mtime(path: &Path, seconds: i64) {
        set_file_mtime(path, FileTime::from_unix_time(seconds, 0)).unwrap();
    }

    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    #[test]
    fn stale_detector_reports_old_mtime() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let path = memory.path().join("old.md");
        write_file(&path, &memory_doc("old but still valid references\n"));
        set_mtime(&path, 2_000_000_000 - (100 * SECONDS_PER_DAY) as i64);

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, fixed_now()).unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].reasons,
            vec![StaleReason::OldMtime { days: 100 }]
        );
        assert!(reports[0].stale);
    }

    #[test]
    fn stale_detector_keeps_fresh_memory_without_references_clean() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let path = memory.path().join("fresh.md");
        write_file(&path, &memory_doc("fresh body\n"));
        set_mtime(&path, 2_000_000_000 - (2 * SECONDS_PER_DAY) as i64);

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, fixed_now()).unwrap();

        assert!(reports.is_empty());
    }

    #[test]
    fn stale_detector_reports_dangling_path_reference() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let path = memory.path().join("dangling_path.md");
        write_file(
            &path,
            &memory_doc("Uses path: src/missing.rs for context\n"),
        );

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, SystemTime::now()).unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].reasons,
            vec![StaleReason::DanglingRef {
                target: "path:src/missing.rs".to_owned()
            }]
        );
    }

    #[test]
    fn stale_detector_accepts_existing_path_reference_with_line_suffix() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        write_file(&cwd.path().join("src/lib.rs"), "fn present() {}\n");
        write_file(
            &memory.path().join("valid_path.md"),
            &memory_doc("See path: src/lib.rs:1\n"),
        );

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, SystemTime::now()).unwrap();

        assert!(reports.is_empty());
    }

    #[test]
    fn stale_detector_reports_missing_function_and_flag_literals() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        write_file(
            &memory.path().join("missing_symbols.md"),
            &memory_doc("Uses function: missing_symbol and flag: MISSING_FLAG\n"),
        );

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, SystemTime::now()).unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].reasons,
            vec![
                StaleReason::DanglingRef {
                    target: "function:missing_symbol".to_owned()
                },
                StaleReason::DanglingRef {
                    target: "flag:MISSING_FLAG".to_owned()
                }
            ]
        );
        assert_eq!(reports[0].payload_fields().get("stale"), Some(&json!(true)));
    }

    #[test]
    fn stale_detector_accepts_function_and_flag_literals_found_in_cwd() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        write_file(
            &cwd.path().join("src/feature.rs"),
            "fn present_symbol() {}\nconst PRESENT_FLAG: bool = true;\n",
        );
        write_file(
            &memory.path().join("valid_symbols.md"),
            &memory_doc("Uses function: present_symbol and flag: PRESENT_FLAG\n"),
        );

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, SystemTime::now()).unwrap();

        assert!(reports.is_empty());
    }

    #[test]
    fn stale_detector_combines_old_mtime_and_dangling_reasons() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let path = memory.path().join("combined.md");
        write_file(&path, &memory_doc("See path: src/missing.rs\n"));
        set_mtime(&path, 2_000_000_000 - (120 * SECONDS_PER_DAY) as i64);

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, fixed_now()).unwrap();

        assert_eq!(reports.len(), 1);
        assert!(reports[0].stale_reason.contains("old_mtime:120d"));
        assert!(reports[0]
            .stale_reason
            .contains("dangling_ref:path:src/missing.rs"));
    }

    #[test]
    fn stale_detector_skips_memory_index_session_logs_and_legacy_derived() {
        let memory = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        write_file(
            &memory.path().join("MEMORY.md"),
            "- [Missing](missing.md)\n",
        );
        write_file(&memory.path().join("SESSION.md"), "path: missing\n");
        write_file(
            &memory.path().join("logs/2026/05/2026-05-06.md"),
            "path: missing\n",
        );
        write_file(
            &memory.path().join(".rust-derived/derived.md"),
            &memory_doc("path: missing\n"),
        );

        let reports = detect_stale_at(memory.path(), cwd.path(), 90, SystemTime::now()).unwrap();

        assert!(reports.is_empty());
    }
}
