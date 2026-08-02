//! MemorySearchToolCallBlock — structured memory search results display.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::TOOL_HEADER_RANGE;
use crate::render::line_utils::truncate_str;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};
use crate::theme::Theme;

/// A single memory search result parsed from the tool output.
#[derive(Debug, Clone)]
pub struct MemoryResult {
    pub score: f64,
    pub path: String,
    pub name: Option<String>,
    pub snippet: Option<String>,
    pub scope: Option<String>,
    pub memory_type: Option<String>,
}

/// Memory search tool call block with structured result display.
#[derive(Debug, Clone)]
pub struct MemorySearchToolCallBlock {
    pub query: String,
    pub results: Vec<MemoryResult>,
    pub error: Option<String>,
    pub started_at: Option<std::time::Instant>,
    pub elapsed_ms: Option<i64>,
}

impl MemorySearchToolCallBlock {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            results: Vec::new(),
            error: None,
            started_at: None,
            elapsed_ms: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn set_error(&mut self, error: Option<String>) {
        if self.elapsed_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
        self.error = error;
    }

    pub fn finish(&mut self) {
        if self.elapsed_ms.is_some() {
            return;
        }
        if let Some(start) = self.started_at {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        self.elapsed_ms.or_else(|| {
            self.started_at
                .map(|start| start.elapsed().as_millis() as i64)
        })
    }

    fn header_line(&self, theme: &Theme, muted: bool, max_width: Option<usize>) -> Line<'static> {
        let text_style = if muted {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let query_style = if muted {
            theme.muted()
        } else {
            theme.fg(theme.command)
        };

        let prefix = "Memory Search ";
        let count = self.results.len();
        let suffix = if count > 0 {
            let s = if count == 1 { "" } else { "s" };
            format!(" ({count} result{s})")
        } else {
            String::new()
        };

        match max_width {
            Some(w) => {
                let suffix_fits = prefix.len() + suffix.len() < w;
                let effective_suffix = if suffix_fits { &suffix } else { "" };
                let query_budget = w
                    .saturating_sub(prefix.len())
                    .saturating_sub(effective_suffix.len());
                let display_query = truncate_str(&self.query, query_budget);

                let mut spans = vec![
                    Span::styled(prefix, bold_style),
                    Span::styled(display_query, query_style),
                ];
                if !effective_suffix.is_empty() {
                    spans.push(Span::styled(effective_suffix.to_string(), theme.dim()));
                }
                Line::from(spans)
            }
            None => Line::from(vec![
                Span::styled(prefix, bold_style),
                Span::styled(self.query.clone(), query_style),
                Span::styled(suffix, theme.dim()),
            ]),
        }
    }

    /// Header line with only the query span selectable (exclude label/suffix).
    fn header_block_line(&self, line: Line<'static>) -> BlockLine {
        let query_end = 2.min(line.spans.len()).max(1);
        BlockLine {
            selectable: Selectable::Spans(1..query_end),
            selection_range: Some(TOOL_HEADER_RANGE),
            selection_text: Some(self.query.clone()),
            content: line,
            ..Default::default()
        }
    }
}

impl BlockContent for MemorySearchToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        match ctx.mode {
            DisplayMode::Collapsed => BlockOutput {
                lines: vec![self.header_block_line(self.header_line(
                    &theme,
                    muted_collapsed,
                    Some(ctx.content_width()),
                ))],
            },
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let header = self.header_line(&theme, false, None);
                let wrapped = crate::render::wrapping::wrap_header_flush(
                    header,
                    ctx.width as usize,
                    ctx.bullet_indent(),
                );
                let mut lines: Vec<BlockLine> = wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        // First span is label (or indent on continuations); only
                        // the query span is selectable on the first visual row.
                        let selectable = if i == 0 {
                            let query_end = 2.min(line.spans.len()).max(1);
                            Selectable::Spans(1..query_end)
                        } else {
                            Selectable::Spans(1..line.spans.len())
                        };
                        BlockLine {
                            selectable,
                            selection_range: Some(TOOL_HEADER_RANGE),
                            selection_text: if i == 0 {
                                Some(self.query.clone())
                            } else {
                                None
                            },
                            joiner: if i == 0 { None } else { Some(" ".to_string()) },
                            content: line,
                            ..Default::default()
                        }
                    })
                    .collect();

                if self.results.is_empty() && self.error.is_none() {
                    lines.push(BlockLine::separator(Line::from("")));
                    lines.push(BlockLine::separator(Line::from(Span::styled(
                        "  (no results)",
                        theme.muted(),
                    ))));
                }

                for (i, r) in self.results.iter().enumerate() {
                    lines.push(Line::from("").into());

                    // Preserve every display field already emitted by the
                    // CrabCode MemorySearchTool result. No missing upstream
                    // line range/source field is synthesized.
                    let idx_span = Span::styled(format!("  {}. ", i + 1), theme.muted());
                    let path_display = shorten_path(&r.path);
                    let path_span = Span::styled(
                        path_display.to_string(),
                        theme.primary().add_modifier(Modifier::BOLD),
                    );
                    let mut result_spans = vec![idx_span, path_span];
                    if let Some(name) = r.name.as_deref().filter(|name| !name.is_empty()) {
                        result_spans.push(Span::styled(format!(" — {name}"), theme.muted()));
                    }
                    let mut metadata = format!("  (score: {:.2}", r.score);
                    if let Some(scope) = r.scope.as_deref().filter(|scope| !scope.is_empty()) {
                        metadata.push_str(", scope: ");
                        metadata.push_str(scope);
                    }
                    if let Some(memory_type) = r
                        .memory_type
                        .as_deref()
                        .filter(|memory_type| !memory_type.is_empty())
                    {
                        metadata.push_str(", type: ");
                        metadata.push_str(memory_type);
                    }
                    metadata.push(')');
                    result_spans.push(Span::styled(metadata, theme.dim()));
                    lines.push(BlockLine::styled(Line::from(result_spans)));

                    // Snippet preview (first 3 non-empty lines, with bg_dark)
                    if let Some(snippet) = &r.snippet {
                        let snippet_lines: Vec<&str> = snippet
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .take(3)
                            .collect();
                        for snippet_line in &snippet_lines {
                            let trimmed = snippet_line.trim();
                            let display =
                                truncate_str(trimmed, ctx.content_width().saturating_sub(4));
                            lines.push(
                                BlockLine::from(Line::from(Span::styled(
                                    format!("    {display}"),
                                    theme.muted(),
                                )))
                                .with_panel_background(theme.bg_dark),
                            );
                        }
                    }
                }

                if let Some(ref err) = self.error {
                    lines.push(Line::from("").into());
                    lines.push(
                        Line::from(Span::styled(
                            format!("  {err}"),
                            theme.fg(theme.accent_error),
                        ))
                        .into(),
                    );
                }

                BlockOutput { lines }
            }
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        let theme = Theme::current();
        if self.error.is_some() {
            Some(AccentStyle::static_color(theme.accent_error))
        } else if ctx.is_running {
            Some(AccentStyle::animated(theme.accent_running))
        } else {
            Some(AccentStyle::static_color(theme.accent_tool))
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if self.error.is_some() {
            let theme = Theme::current();
            Some(AccentStyle::static_color(theme.accent_error))
        } else if ctx.mode == DisplayMode::Collapsed {
            None
        } else {
            self.accent(ctx)
        }
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        self.error.is_none() && !self.results.is_empty()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed => DisplayMode::Expanded,
            _ => DisplayMode::Collapsed,
        }
    }

    fn collapse_mode(&self, _is_running: bool) -> DisplayMode {
        DisplayMode::Collapsed
    }
}

fn shorten_path(path: &str) -> &str {
    // The renderer has no configuration-root authority. CrabCode supplies an
    // absolute result path, so retain the fixed component's outside-known-root
    // fallback and support both native separator spellings.
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

pub fn parse_memory_results(output: &str) -> Vec<MemoryResult> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(output) else {
        return Vec::new();
    };

    entries
        .into_iter()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            let path = object.get("path")?.as_str()?.to_string();
            let score = object.get("score")?.as_f64()?;
            Some(MemoryResult {
                score,
                path,
                name: object
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                snippet: object
                    .get("snippet")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                scope: object
                    .get("scope")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                memory_type: object
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_result() {
        let output = r#"[{
            "path":"/memory/project/MEMORY.md",
            "name":"Project conventions",
            "score":0.72,
            "snippet":"Always run the focused test.",
            "scope":"private",
            "type":"project"
        }]"#;
        let results = parse_memory_results(output);
        assert_eq!(results.len(), 1);
        assert!((results[0].score - 0.72).abs() < 0.01);
        assert_eq!(results[0].path, "/memory/project/MEMORY.md");
        assert_eq!(results[0].name.as_deref(), Some("Project conventions"));
        assert_eq!(
            results[0].snippet.as_deref(),
            Some("Always run the focused test.")
        );
        assert_eq!(results[0].scope.as_deref(), Some("private"));
        assert_eq!(results[0].memory_type.as_deref(), Some("project"));
    }

    #[test]
    fn parse_multiple_results() {
        let output = r#"[
          {"path":"/memory/a.md","name":null,"score":0.85,"snippet":null,"scope":"global","type":null},
          {"path":"/knowledge/b.md","name":"B","score":0.42,"snippet":"content","scope":"knowledge","type":"personal"}
        ]"#;
        let results = parse_memory_results(output);
        assert_eq!(results.len(), 2);
        assert!((results[0].score - 0.85).abs() < 0.01);
        assert_eq!(results[0].scope.as_deref(), Some("global"));
        assert_eq!(results[0].name, None);
        assert!((results[1].score - 0.42).abs() < 0.01);
        assert_eq!(results[1].scope.as_deref(), Some("knowledge"));
    }

    #[test]
    fn parse_no_results() {
        let output = "No relevant memories found.";
        let results = parse_memory_results(output);
        assert!(results.is_empty());
    }

    #[test]
    fn shorten_memory_path() {
        assert_eq!(
            shorten_path("/memory/project/sessions/2026-05-01.md"),
            "2026-05-01.md"
        );
        assert_eq!(shorten_path("/some/other/path.md"), "path.md");
        assert_eq!(shorten_path(r"C:\memory\project\entry.md"), "entry.md");
    }

    fn ctx(mode: DisplayMode) -> BlockContext {
        BlockContext {
            width: 100,
            mode,
            is_running: false,
            raw: false,
            max_lines: None,
            appearance: Default::default(),
            is_selected: false,
            cwd: None,
        }
    }

    #[test]
    fn expanded_output_preserves_backend_result_fields() {
        let mut block = MemorySearchToolCallBlock::new("project conventions");
        block.results = parse_memory_results(
            r#"[{
              "path":"/memory/project/MEMORY.md",
              "name":"Conventions",
              "score":7.5,
              "snippet":"Run the focused test.",
              "scope":"private",
              "type":"project"
            }]"#,
        );
        let rendered = block
            .output(&ctx(DisplayMode::Expanded))
            .lines
            .iter()
            .map(|line| {
                line.content
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "MEMORY.md",
            "Conventions",
            "score: 7.50",
            "scope: private",
            "type: project",
            "Run the focused test.",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}: {rendered}"
            );
        }
    }
}
