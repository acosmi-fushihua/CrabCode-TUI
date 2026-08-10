//! WebSearchToolCallBlock — web search with citations preview.

use std::collections::HashSet;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use super::{TOOL_HEADER_RANGE, failure::failure_lines};
use crate::appearance::RendererLanguage;
use crate::render::line_utils::truncate_str;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::theme::Theme;

const MAX_INLINE_LINES: usize = 10;
const TRUNCATED_INLINE_LINES: usize = 3;

/// Max number of domain names shown in the sources summary line.
const MAX_INLINE_SOURCES: usize = 3;

/// One structured result returned by WebSearch.
///
/// Keeping these fields typed lets the renderer show a compact source preview
/// without exposing the backend's machine-oriented result envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
}

/// Web search tool call — searching the web and returning markdown results.
#[derive(Debug, Clone)]
pub struct WebSearchToolCallBlock {
    /// The search query.
    pub query: String,
    /// Markdown-formatted search results.
    pub content: Option<String>,
    /// Structured search results, when supplied by the runtime.
    pub results: Vec<WebSearchResult>,
    /// Source URLs from the search.
    pub citations: Vec<String>,
    /// Error message if the tool call failed (None = success).
    pub error: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
    /// Header label override (default is localized by the renderer).
    pub label: Option<String>,
    /// True for X search (backend); suppresses the content body since
    /// structured post results are not exposed to the TUI client.
    pub is_x_search: bool,
}

impl WebSearchToolCallBlock {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            content: None,
            results: Vec::new(),
            citations: Vec::new(),
            error: None,
            started_at: None,
            elapsed_ms: None,
            label: None,
            is_x_search: false,
        }
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn copy_text(&self) -> String {
        if !self.results.is_empty() {
            return self
                .results
                .iter()
                .map(|result| {
                    let mut line = format!("{}\n{}", result.title, result.url);
                    if let Some(snippet) = result.snippet.as_deref() {
                        line.push('\n');
                        line.push_str(snippet);
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n\n");
        }
        self.content
            .as_deref()
            .map(visible_content)
            .unwrap_or_default()
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

    /// Render the header line: **Web Search** `query` `(N sources)`
    ///
    /// In collapsed mode (`max_width` is `Some`), reserves space for the source
    /// count suffix and truncates the query to fit — so the suffix is always
    /// visible. In expanded mode (`None`), renders the full query with no suffix.
    fn header_line(
        &self,
        theme: &Theme,
        muted: bool,
        max_width: Option<usize>,
        language: RendererLanguage,
    ) -> Line<'static> {
        let query = single_line(&self.query);
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

        let prefix = self
            .label
            .clone()
            .unwrap_or_else(|| language.text("联网搜索 ", "Web Search ").to_string());

        match max_width {
            Some(w) => {
                // Collapsed shows deduplicated domain count as "sites".
                // The fullscreen footer shows raw citation count as "Sources".
                let site_count = self.unique_domains().len();
                let suffix = if site_count > 0 {
                    match language {
                        RendererLanguage::ZhCn => format!("（{site_count} 个来源）"),
                        RendererLanguage::EnUs => {
                            let s = if site_count == 1 { "" } else { "s" };
                            format!(" ({site_count} site{s})")
                        }
                    }
                } else {
                    String::new()
                };

                // Only show suffix if prefix + suffix fit within width.
                // Otherwise drop it to avoid overflow on narrow terminals.
                let prefix_width = unicode_width::UnicodeWidthStr::width(prefix.as_str());
                let suffix_width = unicode_width::UnicodeWidthStr::width(suffix.as_str());
                let suffix_fits = prefix_width + suffix_width < w;
                let effective_suffix = if suffix_fits { &suffix } else { "" };

                let query_budget = w
                    .saturating_sub(prefix_width)
                    .saturating_sub(unicode_width::UnicodeWidthStr::width(effective_suffix));
                let display_query = truncate_str(&query, query_budget);

                let mut spans = vec![
                    Span::styled(prefix, bold_style),
                    Span::styled(display_query, query_style),
                ];
                if !effective_suffix.is_empty() {
                    spans.push(Span::styled(effective_suffix.to_string(), theme.dim()));
                }
                Line::from(spans)
            }
            None => {
                // Expanded: full query, no suffix.
                Line::from(vec![
                    Span::styled(prefix, bold_style),
                    Span::styled(query, query_style),
                ])
            }
        }
    }

    /// Header line with only the query span selectable (exclude label prefix/suffix).
    fn header_block_line(&self, line: Line<'static>) -> BlockLine {
        // Spans: [prefix, query, optional_suffix] — only the query (index 1).
        let query_end = 2.min(line.spans.len()).max(1);
        BlockLine {
            selectable: Selectable::Spans(1..query_end),
            selection_range: Some(TOOL_HEADER_RANGE),
            selection_text: Some(self.query.clone()),
            content: line,
            ..Default::default()
        }
    }

    /// Unique domain names from citations, deduplicated and order-preserved.
    fn unique_domains(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.citations
            .iter()
            .filter_map(|url| extract_domain(url))
            .filter(|d| seen.insert(d.clone()))
            .collect()
    }

    /// Build the sources summary line from citations.
    ///
    /// Extracts domain names from URLs and renders a compact one-liner:
    /// `Sources: stripe.com, react.dev, stackoverflow.com (+2 more)`
    fn sources_line(&self, theme: &Theme, language: RendererLanguage) -> Option<Line<'static>> {
        let unique = self.unique_domains();
        if unique.is_empty() {
            return None;
        }

        let label_style = theme.muted();
        let value_style = theme.primary();

        let mut spans: Vec<Span<'static>> = vec![Span::styled(
            language.text("  来源：", "  Sources: "),
            label_style,
        )];

        let shown = unique.len().min(MAX_INLINE_SOURCES);
        for (i, domain) in unique.iter().take(shown).enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ", label_style));
            }
            spans.push(Span::styled(domain.clone(), value_style));
        }

        let remaining = unique.len().saturating_sub(MAX_INLINE_SOURCES);
        if remaining > 0 {
            spans.push(Span::styled(
                match language {
                    RendererLanguage::ZhCn => format!("（另有 {remaining} 个）"),
                    RendererLanguage::EnUs => format!(" (+{remaining} more)"),
                },
                label_style,
            ));
        }

        Some(Line::from(spans))
    }

    /// Render one structured result as a bounded single-line preview.
    fn result_preview_line(
        &self,
        result: &WebSearchResult,
        index: usize,
        max_width: usize,
    ) -> String {
        let title = single_line(&result.title);
        let location = extract_domain(&result.url).unwrap_or_else(|| single_line(&result.url));
        let snippet = result
            .snippet
            .as_deref()
            .map(single_line)
            .unwrap_or_default();

        let mut line = format!("  {}. {title}", index + 1);
        if !location.is_empty() {
            line.push_str(" · ");
            line.push_str(&location);
        }
        if !snippet.is_empty() {
            line.push_str(" — ");
            line.push_str(&snippet);
        }
        truncate_str(&line, max_width)
    }
}

/// Extract the host/domain from a URL for display purposes.
fn extract_domain(raw: &str) -> Option<String> {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_owned()))
}

/// Collapse backend-provided line breaks and control whitespace before a
/// result is placed on the terminal's single-line preview surface.
fn single_line(raw: &str) -> String {
    sanitize_bounded_terminal_text(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn visible_content(raw: &str) -> String {
    sanitize_bounded_terminal_text(raw)
        .lines()
        .take_while(|line| !line.trim_start().starts_with("REMINDER:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

impl BlockContent for WebSearchToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let language = ctx.appearance.language;
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        match ctx.mode {
            DisplayMode::Collapsed => {
                let mut lines = vec![self.header_block_line(self.header_line(
                    &theme,
                    muted_collapsed,
                    Some(ctx.content_width()),
                    language,
                ))];
                if let Some(error) = &self.error {
                    lines.extend(failure_lines(
                        error,
                        ctx.mode,
                        ctx.content_width(),
                        theme.fg(theme.accent_error),
                    ));
                }
                BlockOutput { lines }
            }
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let header = self.header_line(&theme, false, None, language);
                let wrapped = crate::render::wrapping::wrap_header_flush(
                    header,
                    ctx.width as usize,
                    ctx.bullet_indent(),
                );
                // Header lines: label prefix excluded, query selectable.
                let mut lines: Vec<BlockLine> = wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let total = line.spans.len();
                        BlockLine {
                            selectable: Selectable::Spans(1..total),
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

                if let Some(error) = &self.error {
                    lines.push(Line::from("").into());
                    lines.extend(failure_lines(
                        error,
                        ctx.mode,
                        ctx.content_width(),
                        theme.fg(theme.accent_error),
                    ));
                }

                let max_inline = if ctx.mode == DisplayMode::Truncated {
                    TRUNCATED_INLINE_LINES
                } else {
                    MAX_INLINE_LINES
                };
                if !self.results.is_empty() {
                    lines.push(BlockLine::separator(Line::from("")));
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));

                    for (index, result) in self.results.iter().take(max_inline).enumerate() {
                        lines.push(
                            BlockLine::from(Line::from(Span::styled(
                                self.result_preview_line(result, index, ctx.content_width()),
                                theme.primary(),
                            )))
                            .with_panel_background(theme.bg_dark),
                        );
                    }

                    let remaining = self.results.len().saturating_sub(max_inline);
                    if remaining > 0 {
                        let hint = match language {
                            RendererLanguage::ZhCn => {
                                format!("  … 另有 {remaining} 条，按 Enter 查看")
                            }
                            RendererLanguage::EnUs => {
                                format!("  ... ({remaining} more results, press Enter to view)")
                            }
                        };
                        lines.push(
                            BlockLine::from(Line::from(Span::styled(hint, theme.dim())))
                                .with_panel_background(theme.bg_dark),
                        );
                    }

                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));
                } else if let Some(ref content) = self.content {
                    lines.push(BlockLine::separator(Line::from("")));

                    // Top padding inside the content box.
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));

                    let indent = "  ";
                    let safe_content = visible_content(content);
                    let content_lines: Vec<&str> = safe_content.lines().collect();

                    for (i, line) in content_lines.iter().enumerate() {
                        if i >= max_inline {
                            let remaining = content_lines.len() - max_inline;
                            lines.push(
                                BlockLine::from(Line::from(Span::styled(
                                    match language {
                                        RendererLanguage::ZhCn => format!(
                                            "{indent}… 另有 {remaining} 行，按 Enter 查看",
                                        ),
                                        RendererLanguage::EnUs => format!(
                                            "{indent}... ({remaining} more lines, press Enter to view)",
                                        ),
                                    },
                                    theme.dim(),
                                )))
                                .with_panel_background(theme.bg_dark),
                            );
                            break;
                        }
                        lines.push(
                            BlockLine::from(Line::from(Span::styled(
                                format!("{indent}{line}"),
                                theme.primary(),
                            )))
                            .with_panel_background(theme.bg_dark),
                        );
                    }

                    // Bottom padding inside the content box.
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));
                } else if !self.is_x_search {
                    lines.push(Line::from("").into());
                    let empty = if ctx.is_running {
                        language.text("  正在搜索…", "  Searching...")
                    } else {
                        language.text("  暂无搜索结果", "  (no results)")
                    };
                    lines.push(Line::from(Span::styled(empty, theme.muted())).into());
                }

                // Sources summary line (after content, matching fullscreen order).
                if let Some(sources) = self.sources_line(&theme, language) {
                    lines.push(Line::from("").into());
                    lines.push(sources.into());
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
        self.error.is_some()
            || ((self.content.is_some() || !self.results.is_empty()) && !self.is_x_search)
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed => DisplayMode::Truncated,
            DisplayMode::Truncated => DisplayMode::Expanded,
            DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        let mut lines = vec![self.header_line(&theme, false, None, ctx.appearance.language)];
        if let Some(error) = &self.error {
            lines.push(Line::from(""));
            lines.extend(
                failure_lines(
                    error,
                    DisplayMode::Expanded,
                    ctx.content_width(),
                    theme.fg(theme.accent_error),
                )
                .into_iter()
                .map(|line| line.content),
            );
        }
        Some(Text::from(lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::BlockContext;

    fn ctx_for_language(mode: DisplayMode, language: RendererLanguage) -> BlockContext {
        let mut appearance = crate::appearance::AppearanceConfig::default();
        appearance.language = language;
        BlockContext {
            width: 80,
            mode,
            is_running: false,
            raw: false,
            max_lines: None,
            appearance,
            is_selected: false,
            cwd: None,
        }
    }

    fn rendered_text(block: &WebSearchToolCallBlock, mode: DisplayMode) -> String {
        rendered_text_for_language(block, mode, RendererLanguage::EnUs)
    }

    fn rendered_text_for_language(
        block: &WebSearchToolCallBlock,
        mode: DisplayMode,
        language: RendererLanguage,
    ) -> String {
        block
            .output(&ctx_for_language(mode, language))
            .lines
            .iter()
            .map(|l| {
                l.content
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn result(title: &str, host: &str, snippet: &str) -> WebSearchResult {
        WebSearchResult {
            title: title.to_string(),
            url: format!("https://{host}/article"),
            snippet: Some(snippet.to_string()),
        }
    }

    #[test]
    fn defaults_to_visible_preview_and_cycles_through_full_results() {
        let block = WebSearchToolCallBlock::new("renderer");
        assert_eq!(block.default_display_mode(), DisplayMode::Truncated);
        assert_eq!(
            block.next_fold_mode(DisplayMode::Collapsed, false),
            DisplayMode::Truncated
        );
        assert_eq!(
            block.next_fold_mode(DisplayMode::Truncated, false),
            DisplayMode::Expanded
        );
        assert_eq!(
            block.next_fold_mode(DisplayMode::Expanded, false),
            DisplayMode::Collapsed
        );
    }

    #[test]
    fn fixed_labels_follow_renderer_language() {
        let mut block = WebSearchToolCallBlock::new("Rust async");
        block.citations = vec!["https://example.com/result".to_string()];

        let zh = rendered_text_for_language(&block, DisplayMode::Truncated, RendererLanguage::ZhCn);
        assert!(zh.contains("联网搜索 Rust async"), "zh:\n{zh}");
        assert!(zh.contains("来源：example.com"), "zh:\n{zh}");
        assert!(zh.contains("暂无搜索结果"), "zh:\n{zh}");

        let en = rendered_text_for_language(&block, DisplayMode::Truncated, RendererLanguage::EnUs);
        assert!(en.contains("Web Search Rust async"), "en:\n{en}");
        assert!(en.contains("Sources: example.com"), "en:\n{en}");
        assert!(en.contains("(no results)"), "en:\n{en}");
    }

    #[test]
    fn structured_results_render_as_three_readable_preview_rows() {
        let mut block = WebSearchToolCallBlock::new("Rust runtimes");
        block.results = vec![
            result("Tokio guide", "tokio.rs", "Async runtime guide"),
            result("Async book", "rust-lang.org", "Language patterns"),
            result("Runtime notes", "example.com", "Practical notes"),
            result("Fourth source", "fourth.example", "Only when expanded"),
        ];
        block.citations = block
            .results
            .iter()
            .map(|result| result.url.clone())
            .collect();
        block.content = Some("REMINDER: internal tool instructions".to_string());

        let preview = rendered_text(&block, DisplayMode::Truncated);
        for expected in [
            "1. Tokio guide · tokio.rs — Async runtime guide",
            "2. Async book · rust-lang.org — Language patterns",
            "3. Runtime notes · example.com — Practical notes",
        ] {
            assert!(
                preview.contains(expected),
                "missing {expected:?}:\n{preview}"
            );
        }
        assert!(!preview.contains("Fourth source"), "preview:\n{preview}");
        assert!(preview.contains("1 more result"), "preview:\n{preview}");
        assert!(!preview.contains("REMINDER"), "preview:\n{preview}");

        let expanded = rendered_text(&block, DisplayMode::Expanded);
        assert!(expanded.contains("Fourth source"), "expanded:\n{expanded}");
        assert!(!expanded.contains("REMINDER"), "expanded:\n{expanded}");
    }

    #[test]
    fn structured_preview_neutralizes_terminal_controls() {
        let mut block = WebSearchToolCallBlock::new("safe display");
        block.results = vec![WebSearchResult {
            title: "\u{1b}[31mcolored\u{1b}[0m".to_string(),
            url: "https://example.com/result".to_string(),
            snippet: Some("safe\u{202e}snippet".to_string()),
        }];

        let preview = rendered_text(&block, DisplayMode::Truncated);
        assert!(!preview.contains('\u{1b}'), "preview:\n{preview}");
        assert!(preview.contains("␛[31mcolored"), "preview:\n{preview}");
        assert!(preview.contains("⟪U+202E⟫"), "preview:\n{preview}");
    }

    #[test]
    fn commentary_fallback_is_sanitized_and_drops_internal_reminders() {
        let mut block = WebSearchToolCallBlock::new("unsafe\u{1b}[31m query");
        block.content = Some(
            "Useful result\u{202e}\nhttps://example.com\nREMINDER: internal instructions\nsecret tail"
                .to_string(),
        );

        let preview = rendered_text(&block, DisplayMode::Truncated);
        assert!(!preview.contains('\u{1b}'), "preview:\n{preview}");
        assert!(preview.contains("␛[31m query"), "preview:\n{preview}");
        assert!(
            preview.contains("Useful result⟪U+202E⟫"),
            "preview:\n{preview}"
        );
        assert!(
            preview.contains("https://example.com"),
            "preview:\n{preview}"
        );
        assert!(!preview.contains("REMINDER"), "preview:\n{preview}");
        assert!(!preview.contains("secret tail"), "preview:\n{preview}");

        let copied = block.copy_text();
        assert!(copied.contains("Useful result⟪U+202E⟫"), "copied: {copied}");
        assert!(!copied.contains("REMINDER"), "copied: {copied}");
        assert!(!copied.contains("secret tail"), "copied: {copied}");
    }

    #[test]
    fn truncated_caps_inline_content_tighter_than_expanded() {
        let mut block = WebSearchToolCallBlock::new("rust async traits");
        let content: Vec<String> = (1..=12).map(|i| format!("l{i:02} result")).collect();
        block.content = Some(content.join("\n"));

        let truncated = rendered_text(&block, DisplayMode::Truncated);
        assert!(truncated.contains("l03"), "truncated:\n{truncated}");
        assert!(!truncated.contains("l04"), "truncated:\n{truncated}");
        assert!(
            truncated.contains("(9 more lines"),
            "truncated:\n{truncated}"
        );

        let expanded = rendered_text(&block, DisplayMode::Expanded);
        assert!(expanded.contains("l10"), "expanded:\n{expanded}");
        assert!(!expanded.contains("l11"), "expanded:\n{expanded}");
        assert!(expanded.contains("(2 more lines"), "expanded:\n{expanded}");
    }
}
