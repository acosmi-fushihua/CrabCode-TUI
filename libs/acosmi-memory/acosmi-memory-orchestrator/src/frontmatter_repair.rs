use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::atomic_write::{atomic_write, BoxError};
use crate::daily_log::rust_derived_root;

pub const AUTO_REPAIR_ENV: &str = "CRABCODE_MEMORY_AUTO_REPAIR";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairMode {
    DryRun,
    Apply,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairStatus {
    DryRun,
    Applied,
    Noop,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairIssueKind {
    MissingFrontmatter,
    MissingName,
    MissingDescription,
    MissingType,
    InvalidType,
    BareLog,
    MemoryIndex,
    LegacyRustDerived,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairSuggestion {
    pub kind: RepairIssueKind,
    pub field: Option<String>,
    pub replacement: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontmatterRepairReport {
    pub status: RepairStatus,
    pub original_path: PathBuf,
    pub repaired_path: Option<PathBuf>,
    pub applied_to_original: bool,
    pub suggestions: Vec<RepairSuggestion>,
}

pub async fn repair_frontmatter(
    project_state_dir: &Path,
    memory_file_path: &Path,
) -> Result<FrontmatterRepairReport, BoxError> {
    let mode = repair_mode_from_env();
    repair_frontmatter_with_mode(project_state_dir, memory_file_path, mode).await
}

pub async fn repair_frontmatter_with_mode(
    project_state_dir: &Path,
    memory_file_path: &Path,
    mode: RepairMode,
) -> Result<FrontmatterRepairReport, BoxError> {
    if has_component(memory_file_path, ".rust-derived") {
        return Ok(skipped_report(
            memory_file_path,
            RepairIssueKind::LegacyRustDerived,
        ));
    }
    if is_memory_index(memory_file_path) {
        return Ok(skipped_report(
            memory_file_path,
            RepairIssueKind::MemoryIndex,
        ));
    }
    if is_bare_memory_log(project_state_dir, memory_file_path) {
        return Ok(skipped_report(memory_file_path, RepairIssueKind::BareLog));
    }

    let content = tokio::fs::read_to_string(memory_file_path).await?;
    let parsed = parse_markdown_ts_compat(&content);
    let plan = build_repair_plan(memory_file_path, &parsed);

    if plan.suggestions.is_empty() {
        return Ok(FrontmatterRepairReport {
            status: RepairStatus::Noop,
            original_path: memory_file_path.to_path_buf(),
            repaired_path: None,
            applied_to_original: false,
            suggestions: Vec::new(),
        });
    }

    let repaired_content = render_repaired_markdown(&parsed, &plan);
    let repaired_path = repaired_output_path(project_state_dir, memory_file_path)?;
    ensure_no_legacy_component(&repaired_path)?;
    atomic_write(&repaired_path, repaired_content.as_bytes()).await?;

    let applied_to_original = mode == RepairMode::Apply;
    if applied_to_original {
        atomic_write(memory_file_path, repaired_content.as_bytes()).await?;
    }

    Ok(FrontmatterRepairReport {
        status: if applied_to_original {
            RepairStatus::Applied
        } else {
            RepairStatus::DryRun
        },
        original_path: memory_file_path.to_path_buf(),
        repaired_path: Some(repaired_path),
        applied_to_original,
        suggestions: plan.suggestions,
    })
}

pub fn repair_mode_from_env() -> RepairMode {
    match std::env::var(AUTO_REPAIR_ENV) {
        Ok(value) if value == "1" => RepairMode::Apply,
        _ => RepairMode::DryRun,
    }
}

fn skipped_report(memory_file_path: &Path, kind: RepairIssueKind) -> FrontmatterRepairReport {
    FrontmatterRepairReport {
        status: RepairStatus::Skipped,
        original_path: memory_file_path.to_path_buf(),
        repaired_path: None,
        applied_to_original: false,
        suggestions: vec![RepairSuggestion {
            kind,
            field: None,
            replacement: None,
        }],
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedMarkdown {
    has_frontmatter: bool,
    frontmatter_text: String,
    body: String,
    fields: FrontmatterFields,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FrontmatterFields {
    name: Option<String>,
    description: Option<String>,
    type_: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RepairPlan {
    name: String,
    description: String,
    type_: Option<String>,
    replace_invalid_type: bool,
    suggestions: Vec<RepairSuggestion>,
}

fn build_repair_plan(memory_file_path: &Path, parsed: &ParsedMarkdown) -> RepairPlan {
    let file_stem = file_stem(memory_file_path);
    let mut suggestions = Vec::new();

    if !parsed.has_frontmatter {
        suggestions.push(RepairSuggestion {
            kind: RepairIssueKind::MissingFrontmatter,
            field: None,
            replacement: None,
        });
    }

    let name = parsed.fields.name.clone().unwrap_or_else(|| {
        let value = file_stem.clone();
        suggestions.push(RepairSuggestion {
            kind: RepairIssueKind::MissingName,
            field: Some("name".to_owned()),
            replacement: Some(value.clone()),
        });
        value
    });

    let description = parsed.fields.description.clone().unwrap_or_else(|| {
        let value = description_from_body(&parsed.body).unwrap_or_else(|| name.clone());
        suggestions.push(RepairSuggestion {
            kind: RepairIssueKind::MissingDescription,
            field: Some("description".to_owned()),
            replacement: Some(value.clone()),
        });
        value
    });

    let inferred_type = infer_type(memory_file_path);
    let mut replace_invalid_type = false;
    let type_ = match parsed.fields.type_.as_deref() {
        Some(value) if is_valid_type(value) => Some(value.to_owned()),
        Some(_) => {
            replace_invalid_type = inferred_type.is_some();
            suggestions.push(RepairSuggestion {
                kind: RepairIssueKind::InvalidType,
                field: Some("type".to_owned()),
                replacement: inferred_type.clone(),
            });
            inferred_type
        }
        None => {
            suggestions.push(RepairSuggestion {
                kind: RepairIssueKind::MissingType,
                field: Some("type".to_owned()),
                replacement: inferred_type.clone(),
            });
            inferred_type
        }
    };

    RepairPlan {
        name,
        description,
        type_,
        replace_invalid_type,
        suggestions,
    }
}

fn render_repaired_markdown(parsed: &ParsedMarkdown, plan: &RepairPlan) -> String {
    let mut lines = if parsed.has_frontmatter {
        parsed
            .frontmatter_text
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    upsert_field(&mut lines, "name", &yaml_quoted(&plan.name), false);
    upsert_field(
        &mut lines,
        "description",
        &yaml_quoted(&plan.description),
        false,
    );
    if let Some(type_) = plan.type_.as_deref() {
        upsert_field(&mut lines, "type", type_, plan.replace_invalid_type);
    }

    format!("---\n{}\n---\n{}", lines.join("\n"), parsed.body)
}

fn upsert_field(lines: &mut Vec<String>, key: &str, value: &str, replace_existing: bool) {
    if let Some(line) = lines.iter_mut().find(|line| line_key(line) == Some(key)) {
        if replace_existing {
            *line = format!("{key}: {value}");
        }
        return;
    }

    lines.push(format!("{key}: {value}"));
}

fn parse_markdown_ts_compat(markdown: &str) -> ParsedMarkdown {
    let Some((frontmatter_text, body_start)) = extract_frontmatter(markdown) else {
        return ParsedMarkdown {
            has_frontmatter: false,
            frontmatter_text: String::new(),
            body: markdown.to_owned(),
            fields: FrontmatterFields::default(),
        };
    };

    let frontmatter_text = frontmatter_text.to_owned();
    let body = markdown[body_start..].to_owned();
    let fields = parse_frontmatter_fields(&frontmatter_text);

    ParsedMarkdown {
        has_frontmatter: true,
        frontmatter_text,
        body,
        fields,
    }
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

fn parse_frontmatter_fields(frontmatter_text: &str) -> FrontmatterFields {
    let mut fields = FrontmatterFields::default();
    for line in frontmatter_text.lines() {
        let Some(key) = line_key(line) else {
            continue;
        };
        let Some(value) = line.split_once(':').map(|(_, value)| parse_scalar(value)) else {
            continue;
        };
        match key {
            "name" => fields.name = Some(value),
            "description" => fields.description = Some(value),
            "type" => fields.type_ = Some(value),
            _ => {}
        }
    }
    fields
}

fn line_key(line: &str) -> Option<&str> {
    let (key, _) = line.split_once(':')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(key)
}

fn parse_scalar(raw: &str) -> String {
    let value = raw.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

fn description_from_body(body: &str) -> Option<String> {
    let line = body
        .lines()
        .map(strip_markdown_marker)
        .find(|line| !line.is_empty())?;
    Some(truncate_chars(&line, 80))
}

fn strip_markdown_marker(line: &str) -> String {
    line.trim()
        .trim_start_matches('#')
        .trim_start_matches('-')
        .trim_start_matches('*')
        .trim_start_matches('>')
        .trim()
        .trim_matches('`')
        .trim_matches('*')
        .trim_matches('_')
        .trim()
        .to_owned()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn infer_type(path: &Path) -> Option<String> {
    let normalized = file_stem(path).to_ascii_lowercase().replace('-', "_");
    ["user", "feedback", "project", "reference"]
        .into_iter()
        .find(|candidate| {
            normalized == *candidate || normalized.starts_with(&format!("{candidate}_"))
        })
        .map(str::to_owned)
}

fn is_valid_type(value: &str) -> bool {
    matches!(value, "user" | "feedback" | "project" | "reference")
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("memory")
        .to_owned()
}

fn yaml_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn repaired_output_path(
    project_state_dir: &Path,
    memory_file_path: &Path,
) -> Result<PathBuf, BoxError> {
    let file_name = memory_file_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "memory file has no file name: {}",
                memory_file_path.display()
            ),
        )
    })?;
    Ok(rust_derived_root(project_state_dir)
        .join("repaired")
        .join(file_name))
}

fn is_memory_index(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "MEMORY.md")
}

fn is_bare_memory_log(project_state_dir: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project_state_dir) else {
        return false;
    };
    let mut components = relative.components();
    matches!(
        (components.next(), components.next()),
        (
            Some(Component::Normal(first)),
            Some(Component::Normal(second))
        ) if first == OsStr::new("memory") && second == OsStr::new("logs")
    )
}

fn has_component(path: &Path, needle: &str) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(name) if name == OsStr::new(needle)))
}

fn ensure_no_legacy_component(path: &Path) -> Result<(), BoxError> {
    if has_component(path, ".rust-derived") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to write frontmatter repair under legacy path: {}",
                path.display()
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::OnceLock;

    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::*;

    /// 串行化本模块里改 `AUTO_REPAIR_ENV` 的测试（`std::env::set_var` 是进程
    /// 全局的）。用 tokio 的 Mutex 而非 `std::sync::Mutex`：guard 必须跨
    /// `.await` 存活，std 的 guard 跨 await 会触发 `clippy::await_holding_lock`
    /// 且在多线程 runtime 上不是 `Send`。
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_memory(dir: &TempDir, relative: &str, content: &str) -> PathBuf {
        let path = dir.path().join("memory").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn frontmatter_repair_dry_run_writes_sibling_repaired_copy() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "project_notes.md",
            "# Launch notes\nKeep the rollout context.",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(report.status, RepairStatus::DryRun);
        assert!(!report.applied_to_original);
        assert!(report
            .suggestions
            .iter()
            .any(|suggestion| suggestion.kind == RepairIssueKind::MissingFrontmatter));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Launch notes\nKeep the rollout context."
        );

        let repaired_path = report.repaired_path.unwrap();
        assert_eq!(
            repaired_path,
            dir.path()
                .join(".memory-rust-derived")
                .join("repaired")
                .join("project_notes.md")
        );
        let repaired = fs::read_to_string(repaired_path).unwrap();
        assert!(repaired.contains("name: \"project_notes\""));
        assert!(repaired.contains("description: \"Launch notes\""));
        assert!(repaired.contains("type: project"));
        assert!(!dir.path().join("memory/.rust-derived/repaired").exists());
    }

    #[tokio::test]
    async fn frontmatter_repair_missing_type_is_not_fatal() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "feedback_rule.md",
            "---\nname: Rule\ndescription: Keep it tight\n---\nBody",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(report.status, RepairStatus::DryRun);
        assert!(report
            .suggestions
            .iter()
            .any(|suggestion| suggestion.kind == RepairIssueKind::MissingType));
        let repaired = fs::read_to_string(report.repaired_path.unwrap()).unwrap();
        assert!(repaired.contains("type: feedback"));
    }

    #[tokio::test]
    async fn frontmatter_repair_invalid_type_is_not_fatal() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "reference_links.md",
            "---\nname: Links\ndescription: Dashboards\ntype: fact\n---\nBody",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(report.status, RepairStatus::DryRun);
        assert!(report
            .suggestions
            .iter()
            .any(|suggestion| suggestion.kind == RepairIssueKind::InvalidType));
        let repaired = fs::read_to_string(report.repaired_path.unwrap()).unwrap();
        assert!(repaired.contains("type: reference"));
        assert!(!repaired.contains("type: fact"));
    }

    #[tokio::test]
    async fn frontmatter_repair_ts_problematic_yaml_values_are_not_fatal() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "feedback_globs.md",
            "---\ndescription: Glob **/*.{ts,tsx} & !node_modules\ntype: feedback\n---\nBody",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(report.status, RepairStatus::DryRun);
        let repaired = fs::read_to_string(report.repaired_path.unwrap()).unwrap();
        assert!(repaired.contains("description: Glob **/*.{ts,tsx} & !node_modules"));
        assert!(repaired.contains("name: \"feedback_globs\""));
    }

    #[tokio::test]
    async fn frontmatter_repair_missing_name_and_description_are_suggested() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "user_profile.md",
            "---\ntype: user\n---\n- Prefers precise status updates.",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert!(report
            .suggestions
            .iter()
            .any(|suggestion| suggestion.kind == RepairIssueKind::MissingName));
        assert!(report
            .suggestions
            .iter()
            .any(|suggestion| suggestion.kind == RepairIssueKind::MissingDescription));
        let repaired = fs::read_to_string(report.repaired_path.unwrap()).unwrap();
        assert!(repaired.contains("name: \"user_profile\""));
        assert!(repaired.contains("description: \"Prefers precise status updates.\""));
    }

    #[tokio::test]
    async fn frontmatter_repair_env_switch_is_required_to_modify_original() {
        let _guard = env_lock().lock().await;
        std::env::remove_var(AUTO_REPAIR_ENV);

        let dir = TempDir::new().unwrap();
        let path = write_memory(&dir, "project_plan.md", "Plan body");

        let dry_run = repair_frontmatter(dir.path(), &path).await.unwrap();
        assert_eq!(dry_run.status, RepairStatus::DryRun);
        assert!(!fs::read_to_string(&path).unwrap().starts_with("---"));

        std::env::set_var(AUTO_REPAIR_ENV, "0");
        let still_dry_run = repair_frontmatter(dir.path(), &path).await.unwrap();
        assert_eq!(still_dry_run.status, RepairStatus::DryRun);
        assert!(!fs::read_to_string(&path).unwrap().starts_with("---"));

        std::env::set_var(AUTO_REPAIR_ENV, "1");
        let applied = repair_frontmatter(dir.path(), &path).await.unwrap();
        assert_eq!(applied.status, RepairStatus::Applied);
        assert!(applied.applied_to_original);
        assert!(fs::read_to_string(&path).unwrap().starts_with("---"));

        std::env::remove_var(AUTO_REPAIR_ENV);
    }

    #[tokio::test]
    async fn frontmatter_repair_skips_bare_logs_and_legacy_paths() {
        let dir = TempDir::new().unwrap();
        let bare_log = write_memory(&dir, "logs/2026/05/2026-05-06.md", "No frontmatter");
        let legacy = write_memory(&dir, ".rust-derived/repaired/old.md", "No frontmatter");

        let log_report = repair_frontmatter_with_mode(dir.path(), &bare_log, RepairMode::DryRun)
            .await
            .unwrap();
        let legacy_report = repair_frontmatter_with_mode(dir.path(), &legacy, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(log_report.status, RepairStatus::Skipped);
        assert_eq!(legacy_report.status, RepairStatus::Skipped);
        assert!(!dir.path().join(".memory-rust-derived/repaired").exists());
        assert!(dir
            .path()
            .join("memory/logs/2026/05/2026-05-06.md")
            .exists());
        assert!(dir
            .path()
            .join("memory/.rust-derived/repaired/old.md")
            .exists());
    }

    #[tokio::test]
    async fn frontmatter_repair_noops_when_frontmatter_is_valid() {
        let dir = TempDir::new().unwrap();
        let path = write_memory(
            &dir,
            "project_ok.md",
            "---\nname: OK\ndescription: Ready\ntype: project\n---\nBody",
        );

        let report = repair_frontmatter_with_mode(dir.path(), &path, RepairMode::DryRun)
            .await
            .unwrap();

        assert_eq!(report.status, RepairStatus::Noop);
        assert!(report.repaired_path.is_none());
        assert!(!dir.path().join(".memory-rust-derived/repaired").exists());
    }
}
