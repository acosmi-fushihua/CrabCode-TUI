use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub const MEMORY_MD_FILE: &str = "MEMORY.md";
pub const MEMORY_MD_MAX_LINES: usize = 200;
pub const MEMORY_MD_MAX_BYTES: u64 = 25 * 1024;
pub const MEMORY_MD_MAX_ENTRY_CHARS: usize = 150;

const SKIP_BASENAMES: &[&str] = &["MEMORY.md", "SESSION.md"];

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryMdReport {
    pub path: PathBuf,
    pub exists: bool,
    pub line_count: usize,
    pub byte_size: u64,
    pub overflow_ratio: f32,
    pub long_entries: Vec<usize>,
    pub links: Vec<MemoryMdLink>,
    pub dangling_refs: Vec<MemoryMdDanglingRef>,
    pub missing_index: Vec<PathBuf>,
    pub duplicates: Vec<MemoryMdDuplicate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryMdLink {
    pub line: usize,
    pub title: String,
    pub target: String,
    pub normalized_target: Option<String>,
    pub hook: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryMdDanglingRef {
    pub line: usize,
    pub target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryMdDuplicate {
    pub target: String,
    pub lines: Vec<usize>,
}

pub fn analyze_memory_md(memory_dir: &Path) -> Result<MemoryMdReport, BoxError> {
    let memory_md_path = memory_dir.join(MEMORY_MD_FILE);
    let memory_files = scan_memory_markdown_files(memory_dir)?;

    if !memory_md_path.exists() {
        return Ok(MemoryMdReport {
            path: memory_md_path,
            exists: false,
            line_count: 0,
            byte_size: 0,
            overflow_ratio: 0.0,
            long_entries: Vec::new(),
            links: Vec::new(),
            dangling_refs: Vec::new(),
            missing_index: memory_files,
            duplicates: Vec::new(),
        });
    }

    let content = std::fs::read_to_string(&memory_md_path)?;
    let byte_size = content.len() as u64;
    let line_count = content.lines().count();
    let long_entries = content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            if line.chars().count() > MEMORY_MD_MAX_ENTRY_CHARS {
                Some(idx + 1)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let links = content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| parse_memory_md_link(memory_dir, idx + 1, line))
        .collect::<Vec<_>>();

    let mut linked_targets = BTreeSet::new();
    let mut by_target: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut dangling_refs = Vec::new();
    for link in &links {
        let Some(normalized) = link.normalized_target.clone() else {
            continue;
        };

        by_target
            .entry(normalized.clone())
            .or_default()
            .push(link.line);
        linked_targets.insert(PathBuf::from(&normalized));

        if !memory_dir.join(&normalized).exists() {
            dangling_refs.push(MemoryMdDanglingRef {
                line: link.line,
                target: link.target.clone(),
            });
        }
    }

    let duplicates = by_target
        .into_iter()
        .filter_map(|(target, lines)| {
            if lines.len() > 1 {
                Some(MemoryMdDuplicate { target, lines })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let missing_index = memory_files
        .into_iter()
        .filter(|relative| !linked_targets.contains(relative))
        .collect::<Vec<_>>();

    Ok(MemoryMdReport {
        path: memory_md_path,
        exists: true,
        line_count,
        byte_size,
        overflow_ratio: overflow_ratio(line_count, byte_size),
        long_entries,
        links,
        dangling_refs,
        missing_index,
        duplicates,
    })
}

fn parse_memory_md_link(memory_dir: &Path, line_number: usize, line: &str) -> Option<MemoryMdLink> {
    let open = line.find('[')?;
    let close = line[open + 1..].find("](")? + open + 1;
    let target_start = close + 2;
    let target_end = line[target_start..].find(')')? + target_start;

    let title = line[open + 1..close].trim().to_owned();
    let target = line[target_start..target_end].trim().to_owned();
    let hook = parse_hook(&line[target_end + 1..]);
    let normalized_target = normalize_local_target(memory_dir, &target);

    Some(MemoryMdLink {
        line: line_number,
        title,
        target,
        normalized_target,
        hook,
    })
}

fn parse_hook(raw: &str) -> Option<String> {
    let hook = raw
        .trim()
        .trim_start_matches(['-', '\u{2013}', '\u{2014}'])
        .trim();
    if hook.is_empty() {
        None
    } else {
        Some(hook.to_owned())
    }
}

fn normalize_local_target(memory_dir: &Path, target: &str) -> Option<String> {
    if target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return None;
    }

    let target = target
        .split('#')
        .next()
        .unwrap_or(target)
        .split('?')
        .next()
        .unwrap_or(target)
        .trim();
    if target.is_empty() {
        return None;
    }

    let path = Path::new(target);
    let relative = if path.is_absolute() {
        path.strip_prefix(memory_dir).ok()?
    } else {
        path
    };
    normalize_relative_path(relative)
}

fn normalize_relative_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn scan_memory_markdown_files(memory_dir: &Path) -> Result<Vec<PathBuf>, BoxError> {
    if !memory_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(memory_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = entry?;
        if !entry.file_type().is_file() || !is_memory_markdown(entry.path()) {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(memory_dir)
            .unwrap_or(entry.path())
            .to_path_buf();
        files.push(relative);
    }

    files.sort_by_key(|left| relative_key(left));
    Ok(files)
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

fn overflow_ratio(line_count: usize, byte_size: u64) -> f32 {
    let line_ratio = line_count as f32 / MEMORY_MD_MAX_LINES as f32;
    let byte_ratio = byte_size as f32 / MEMORY_MD_MAX_BYTES as f32;
    line_ratio.max(byte_ratio)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn memory_md_analyze_parses_links_and_hooks_without_writing() {
        let dir = TempDir::new().unwrap();
        let memory_md = dir.path().join("MEMORY.md");
        write_file(
            &dir.path().join("topic.md"),
            "---\ntype: project\n---\nbody\n",
        );
        write_file(&memory_md, "- [Topic](topic.md) - useful hook\n");
        let before = fs::read_to_string(&memory_md).unwrap();

        let report = analyze_memory_md(dir.path()).unwrap();

        assert!(report.exists);
        assert_eq!(report.line_count, 1);
        assert_eq!(report.links.len(), 1);
        assert_eq!(report.links[0].title, "Topic");
        assert_eq!(
            report.links[0].normalized_target,
            Some("topic.md".to_owned())
        );
        assert_eq!(report.links[0].hook, Some("useful hook".to_owned()));
        assert!(report.dangling_refs.is_empty());
        assert!(report.missing_index.is_empty());
        assert_eq!(fs::read_to_string(&memory_md).unwrap(), before);
    }

    #[test]
    fn memory_md_analyze_detects_dangling_duplicates_and_long_entries() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir.path().join("topic.md"),
            "---\ntype: project\n---\nbody\n",
        );
        let long_tail = "x".repeat(MEMORY_MD_MAX_ENTRY_CHARS);
        write_file(
            &dir.path().join("MEMORY.md"),
            &format!(
                "- [Topic](topic.md) - hook\n- [Again](./topic.md) - dup\n- [Missing](missing.md) - {long_tail}\n"
            ),
        );

        let report = analyze_memory_md(dir.path()).unwrap();

        assert_eq!(
            report.duplicates,
            vec![MemoryMdDuplicate {
                target: "topic.md".to_owned(),
                lines: vec![1, 2]
            }]
        );
        assert_eq!(
            report.dangling_refs,
            vec![MemoryMdDanglingRef {
                line: 3,
                target: "missing.md".to_owned()
            }]
        );
        assert_eq!(report.long_entries, vec![3]);
        assert!(report.overflow_ratio > 0.0);
    }

    #[test]
    fn memory_md_analyze_reports_missing_index_file_and_unindexed_memories() {
        let dir = TempDir::new().unwrap();
        write_file(&dir.path().join("a.md"), "---\ntype: project\n---\na\n");
        write_file(
            &dir.path().join("nested/b.md"),
            "---\ntype: project\n---\nb\n",
        );

        let report = analyze_memory_md(dir.path()).unwrap();

        assert!(!report.exists);
        assert_eq!(report.line_count, 0);
        assert_eq!(
            report.missing_index,
            vec![PathBuf::from("a.md"), PathBuf::from("nested/b.md")]
        );
    }

    #[test]
    fn memory_md_analyze_detects_memory_files_missing_from_existing_index() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir.path().join("listed.md"),
            "---\ntype: project\n---\na\n",
        );
        write_file(
            &dir.path().join("unlisted.md"),
            "---\ntype: project\n---\nb\n",
        );
        write_file(&dir.path().join("MEMORY.md"), "- [Listed](listed.md)\n");

        let report = analyze_memory_md(dir.path()).unwrap();

        assert_eq!(report.missing_index, vec![PathBuf::from("unlisted.md")]);
    }

    #[test]
    fn memory_md_analyze_skips_session_logs_and_legacy_derived_files() {
        let dir = TempDir::new().unwrap();
        write_file(&dir.path().join("topic.md"), "---\ntype: project\n---\na\n");
        write_file(&dir.path().join("SESSION.md"), "scratch\n");
        write_file(&dir.path().join("logs/2026/05/2026-05-06.md"), "log\n");
        write_file(
            &dir.path().join(".rust-derived/derived.md"),
            "---\ntype: project\n---\nderived\n",
        );
        write_file(&dir.path().join("MEMORY.md"), "");

        let report = analyze_memory_md(dir.path()).unwrap();

        assert_eq!(report.missing_index, vec![PathBuf::from("topic.md")]);
    }
}
