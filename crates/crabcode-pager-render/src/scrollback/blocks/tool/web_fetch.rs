//! WebFetchToolCallBlock — URL fetch with content preview.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use super::{
    TOOL_HEADER_RANGE,
    failure::{failure_lines, safe_single_line},
};
use crate::appearance::RendererLanguage;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::theme::Theme;

const MAX_INLINE_LINES: usize = 10;
const TRUNCATED_INLINE_LINES: usize = 3;
const MAX_HEADER_FIELD_WIDTH: usize = 512;
const MAX_METADATA_FIELD_WIDTH: usize = 128;

/// Web fetch tool call — fetching a URL and returning markdown content.
#[derive(Debug, Clone)]
pub struct WebFetchToolCallBlock {
    /// The fetched URL.
    pub url: String,
    /// HTTP status code (e.g. 200, 404).
    /// `Option` because the block exists pre-completion (pending/running state)
    /// before any response data arrives.
    pub status_code: Option<u16>,
    /// HTTP reason phrase (e.g. "OK", "Not Found").
    pub status_text: Option<String>,
    /// Content type (e.g. "markdown", "text/plain").
    pub content_type: Option<String>,
    /// Content size in bytes.
    pub bytes: Option<usize>,
    /// Error message if the tool call failed (None = success).
    pub error: Option<String>,
    /// Fetched content (markdown or raw text).
    pub output: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
}

impl WebFetchToolCallBlock {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            status_code: None,
            status_text: None,
            content_type: None,
            bytes: None,
            error: None,
            output: None,
            started_at: None,
            elapsed_ms: None,
        }
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn copy_text(&self) -> String {
        self.output.clone().unwrap_or_default()
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
        match self.elapsed_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    /// Format byte count as human-readable (e.g. "14.2 KB").
    fn format_bytes(bytes: usize) -> String {
        if bytes < 1024 {
            format!("{bytes} B")
        } else if bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else {
            format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        }
    }

    /// Render the header line: **Fetch** `url`
    ///
    /// When `max_width` is `Some`, the URL is truncated with ellipsis to fit.
    /// When `None`, the full URL is rendered (for expanded view / fullscreen).
    fn header_line(
        &self,
        theme: &Theme,
        muted: bool,
        max_width: Option<usize>,
        language: RendererLanguage,
    ) -> Line<'static> {
        let text_style = if muted {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let url_style = if muted {
            theme.muted()
        } else {
            theme.fg(theme.command)
        };

        let prefix = language.text("抓取网页 ", "Fetch ");
        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        let display_url = match max_width {
            Some(w) => safe_single_line(&self.url, w.saturating_sub(prefix_width)),
            None => safe_single_line(&self.url, MAX_HEADER_FIELD_WIDTH),
        };

        Line::from(vec![
            Span::styled(prefix, bold_style),
            Span::styled(display_url, url_style),
        ])
    }

    /// Header line with only the URL span selectable (exclude "Fetch " prefix).
    fn header_block_line(&self, line: Line<'static>) -> BlockLine {
        let url_end = 2.min(line.spans.len()).max(1);
        BlockLine {
            selectable: Selectable::Spans(1..url_end),
            selection_range: Some(TOOL_HEADER_RANGE),
            selection_text: Some(self.url.clone()),
            content: line,
            ..Default::default()
        }
    }

    /// Build the metadata line: status, content_type, size.
    fn metadata_line(&self, theme: &Theme, language: RendererLanguage) -> Option<Line<'static>> {
        let label_style = theme.muted();
        let value_style = theme.primary();

        let mut parts: Vec<Vec<Span<'static>>> = Vec::new();

        if let Some(code) = self.status_code {
            let status = self
                .status_text
                .as_deref()
                .filter(|status| !status.is_empty())
                .map_or_else(
                    || code.to_string(),
                    |status| {
                        format!(
                            "{code} {}",
                            safe_single_line(status, MAX_METADATA_FIELD_WIDTH)
                        )
                    },
                );
            parts.push(vec![
                Span::styled(language.text("状态：", "status: "), label_style),
                Span::styled(status, value_style),
            ]);
        }
        if let Some(ref ct) = self.content_type {
            parts.push(vec![
                Span::styled(language.text("类型：", "content type: "), label_style),
                Span::styled(safe_single_line(ct, MAX_METADATA_FIELD_WIDTH), value_style),
            ]);
        }
        if let Some(bytes) = self.bytes {
            parts.push(vec![
                Span::styled(language.text("大小：", "size: "), label_style),
                Span::styled(Self::format_bytes(bytes), value_style),
            ]);
        }

        if parts.is_empty() {
            return None;
        }

        let indent = "  ";
        let mut spans: Vec<Span<'static>> = vec![Span::styled(indent.to_owned(), label_style)];
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ", label_style));
            }
            spans.extend(part);
        }

        Some(Line::from(spans))
    }
}

impl BlockContent for WebFetchToolCallBlock {
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
                // Header lines: "Fetch " prefix excluded, URL selectable.
                let mut lines: Vec<BlockLine> = wrapped
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let total = line.spans.len();
                        BlockLine {
                            selectable: Selectable::Spans(1..total),
                            selection_range: Some(TOOL_HEADER_RANGE),
                            selection_text: if i == 0 { Some(self.url.clone()) } else { None },
                            joiner: if i == 0 { None } else { Some(" ".to_string()) },
                            content: line,
                            ..Default::default()
                        }
                    })
                    .collect();

                // Metadata line (status, content_type, size).
                if let Some(meta) = self.metadata_line(&theme, language) {
                    if ctx.mode == DisplayMode::Expanded {
                        lines.push(BlockLine::separator(Line::from("")));
                    }
                    lines.push(BlockLine::separator(meta));
                }

                if let Some(error) = &self.error {
                    lines.push(Line::from("").into());
                    lines.extend(
                        failure_lines(
                            error,
                            ctx.mode,
                            ctx.content_width(),
                            theme.fg(theme.accent_error),
                        )
                        .into_iter()
                        .map(|line| line.with_panel_background(theme.bg_dark)),
                    );
                }

                let max_inline = if ctx.mode == DisplayMode::Truncated {
                    TRUNCATED_INLINE_LINES
                } else {
                    MAX_INLINE_LINES
                };
                if let Some(preview) = self.output.as_deref() {
                    if ctx.mode == DisplayMode::Expanded {
                        lines.push(Line::from("").into());
                        lines.push(
                            BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark),
                        );
                    }

                    let indent = "  ";
                    let preview = sanitize_bounded_terminal_text(preview);
                    let total_lines = preview.lines().count();

                    for (i, line) in preview.lines().enumerate() {
                        if i >= max_inline {
                            let remaining = total_lines - max_inline;
                            let hint = match language {
                                RendererLanguage::ZhCn => {
                                    format!("{indent}…（另有 {remaining} 行，按 Enter 查看）")
                                }
                                RendererLanguage::EnUs => format!(
                                    "{indent}... ({remaining} more lines, press Enter to view)"
                                ),
                            };
                            lines.push(
                                BlockLine::from(Line::from(Span::styled(hint, theme.dim())))
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

                    if ctx.mode == DisplayMode::Expanded {
                        lines.push(
                            BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark),
                        );
                    }
                } else if self.error.is_none() {
                    if ctx.mode == DisplayMode::Expanded {
                        lines.push(Line::from("").into());
                    }
                    let empty = if ctx.is_running {
                        language.text("  正在抓取…", "  Fetching...")
                    } else {
                        language.text("  （无内容）", "  (no content)")
                    };
                    lines.push(Line::from(Span::styled(empty, theme.muted())).into());
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
        self.output.is_some() || self.error.is_some()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    // No special running-state handling: fetch completes in one shot (no streaming).
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

    fn ctx(mode: DisplayMode) -> BlockContext {
        ctx_for_language(mode, RendererLanguage::EnUs)
    }

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

    fn rendered_text(block: &WebFetchToolCallBlock, mode: DisplayMode) -> String {
        block
            .output(&ctx(mode))
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

    fn rendered_text_for_language(
        block: &WebFetchToolCallBlock,
        mode: DisplayMode,
        language: RendererLanguage,
    ) -> String {
        block
            .output(&ctx_for_language(mode, language))
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
            .join("\n")
    }

    #[test]
    fn defaults_to_a_bounded_visible_preview() {
        let block = WebFetchToolCallBlock::new("https://example.com")
            .with_output("first\nsecond\nthird\nfourth");

        assert_eq!(block.default_display_mode(), DisplayMode::Truncated);
        assert_eq!(
            block.next_fold_mode(DisplayMode::Collapsed, false),
            DisplayMode::Truncated
        );
        assert_eq!(
            block.next_fold_mode(DisplayMode::Truncated, false),
            DisplayMode::Expanded
        );
        let rendered = rendered_text(&block, block.default_display_mode());
        assert!(rendered.contains("first"), "rendered:\n{rendered}");
        assert!(!rendered.contains("fourth"), "rendered:\n{rendered}");
    }

    #[test]
    fn running_preview_does_not_claim_the_fetch_returned_no_content() {
        let block = WebFetchToolCallBlock::new("https://example.com");
        let mut running = ctx_for_language(DisplayMode::Truncated, RendererLanguage::ZhCn);
        running.is_running = true;
        let rendered = block
            .output(&running)
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

        assert!(rendered.contains("正在抓取"), "rendered:\n{rendered}");
        assert!(!rendered.contains("无内容"), "rendered:\n{rendered}");
    }

    #[test]
    fn fixed_labels_follow_renderer_language_and_preview_is_sanitized() {
        let mut block = WebFetchToolCallBlock::new("https://example.com");
        block.status_code = Some(200);
        block.status_text = Some("OK".to_string());
        block.bytes = Some(2_048);
        block.output = Some("title\u{1b}[2J\nbody".to_string());

        let zh = rendered_text_for_language(&block, DisplayMode::Truncated, RendererLanguage::ZhCn);
        assert!(zh.contains("抓取网页 https://example.com"), "zh:\n{zh}");
        assert!(zh.contains("状态：200 OK"), "zh:\n{zh}");
        assert!(zh.contains("大小：2.0 KB"), "zh:\n{zh}");
        assert!(!zh.contains('\u{1b}'), "zh:\n{zh}");

        let en = rendered_text_for_language(&block, DisplayMode::Truncated, RendererLanguage::EnUs);
        assert!(en.contains("Fetch https://example.com"), "en:\n{en}");
        assert!(en.contains("status: 200 OK"), "en:\n{en}");
        assert!(en.contains("size: 2.0 KB"), "en:\n{en}");
    }

    #[test]
    fn header_url_is_sanitized_and_bounded_without_mutating_source() {
        let url = format!(
            "https://example.com/\u{1b}]52;c;payload\u{7}\u{202e}/{}\nnext",
            "segment/".repeat(2_000)
        );
        let block = WebFetchToolCallBlock::new(url.clone()).with_error("failed");

        for mode in [DisplayMode::Collapsed, DisplayMode::Expanded] {
            let rendered = rendered_text(&block, mode);
            for control in ['\u{1b}', '\u{7}', '\u{202e}'] {
                assert!(
                    !rendered.contains(control),
                    "unsafe {control:?}: {rendered:?}"
                );
            }
            assert!(rendered.contains("␛]52;c;payload"), "{rendered:?}");
            assert!(rendered.contains("⟪U+202E⟫"), "{rendered:?}");
            assert!(rendered.lines().next().unwrap_or_default().len() < 2_000);
        }
        assert_eq!(block.url, url);
    }

    #[test]
    fn metadata_fields_are_terminal_safe_single_line_and_bounded() {
        let status = format!("OK\nforged\u{1b}]52;c;payload\u{7}{}", "x".repeat(20_000));
        let content_type = format!("text/plain\nforged\u{202e}{}", "y".repeat(20_000));
        let mut block = WebFetchToolCallBlock::new("https://example.com");
        block.status_code = Some(200);
        block.status_text = Some(status.clone());
        block.content_type = Some(content_type.clone());

        let rendered = rendered_text(&block, DisplayMode::Expanded);
        for control in ['\u{1b}', '\u{7}', '\u{202e}'] {
            assert!(
                !rendered.contains(control),
                "unsafe {control:?}: {rendered:?}"
            );
        }
        assert!(rendered.contains("200 OK forged"), "{rendered:?}");
        assert!(rendered.contains("text/plain forged"), "{rendered:?}");
        assert!(rendered.contains("␛]52;c;payload"), "{rendered:?}");
        assert!(rendered.contains("⟪U+202E⟫"), "{rendered:?}");
        assert!(
            rendered.len() < 1_000,
            "metadata was not bounded: {rendered:?}"
        );
        assert_eq!(block.status_text.as_deref(), Some(status.as_str()));
        assert_eq!(block.content_type.as_deref(), Some(content_type.as_str()));
    }

    #[test]
    fn truncated_caps_inline_content_tighter_than_expanded() {
        let content: Vec<String> = (1..=12).map(|i| format!("l{i:02} body")).collect();
        let block =
            WebFetchToolCallBlock::new("https://example.com").with_output(content.join("\n"));

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
