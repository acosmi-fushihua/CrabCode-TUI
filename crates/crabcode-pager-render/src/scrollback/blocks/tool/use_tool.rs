//! UseToolCallBlock — MCP integration tool dispatch.

use crate::mcp_display::{MCP_TOOL_NAME_DELIMITER, mcp_titleize_segment};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::theme::Theme;

use super::failure::{bounded_text_lines, failure_lines, safe_single_line};

const MAX_INLINE_LINES: usize = 10;
const TRUNCATED_INLINE_LINES: usize = 3;
const MAX_HEADER_FIELD_WIDTH: usize = 512;
const MAX_INPUT_ARGS: usize = 40;
const TRUNCATED_INPUT_ARGS: usize = 8;
const MAX_ARG_KEY_WIDTH: usize = 128;

/// Use tool call — dispatching to an MCP integration tool.
#[derive(Debug, Clone)]
pub struct UseToolCallBlock {
    /// The qualified tool name (e.g. "linear__save_issue").
    pub tool_name: String,
    /// Input arguments as key-value pairs (extracted from tool_input JSON).
    pub input_args: Vec<(String, String)>,
    /// Output text from the dispatched tool.
    pub output: Option<String>,
    /// Error message if the tool call failed.
    pub error: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
}

impl UseToolCallBlock {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_args: Vec::new(),
            output: None,
            error: None,
            started_at: None,
            elapsed_ms: None,
        }
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
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

    pub fn copy_text(&self) -> String {
        let mut out = format!("tool: {}\n", self.tool_name);
        for (k, v) in &self.input_args {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push('\n');
        out.push_str(self.output.as_deref().unwrap_or("(no output)"));
        out
    }

    /// Split `tool_name` on the (validated-unambiguous)
    /// `MCP_TOOL_NAME_DELIMITER` and title-case each segment. Returns
    /// `(server_title, action_title)` for qualified names, or
    /// `("", titleized_tool_name)` for unqualified ones (which fall
    /// through to a single-span render in `header_line`).
    fn split_name(&self) -> (String, String) {
        match self.tool_name.split_once(MCP_TOOL_NAME_DELIMITER) {
            Some((server, action)) => (
                mcp_titleize_segment(&safe_single_line(server, MAX_HEADER_FIELD_WIDTH)),
                mcp_titleize_segment(&safe_single_line(action, MAX_HEADER_FIELD_WIDTH)),
            ),
            None => (
                String::new(),
                mcp_titleize_segment(&safe_single_line(&self.tool_name, MAX_HEADER_FIELD_WIDTH)),
            ),
        }
    }

    /// Render the header line: **Server** `Action`
    fn header_line(&self, theme: &Theme, muted: bool, max_width: Option<usize>) -> Line<'static> {
        let text_style = if muted {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let action_style = if muted {
            theme.muted()
        } else {
            theme.fg(theme.command)
        };

        let (server, action) = self.split_name();

        let line = if server.is_empty() {
            Line::from(vec![Span::styled(action, bold_style)])
        } else {
            Line::from(vec![
                Span::styled(format!("{server} "), bold_style),
                Span::styled(action, action_style),
            ])
        };
        crate::render::line_utils::truncate_line(
            line,
            max_width.unwrap_or(MAX_HEADER_FIELD_WIDTH).max(1),
        )
    }
}

impl BlockContent for UseToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        match ctx.mode {
            DisplayMode::Collapsed => {
                let mut lines = vec![
                    self.header_line(&theme, muted_collapsed, Some(ctx.content_width()))
                        .into(),
                ];
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
                let header = self.header_line(&theme, false, None);
                let wrapped = crate::render::wrapping::wrap_header_flush(
                    header,
                    ctx.width as usize,
                    ctx.bullet_indent(),
                );
                let mut lines: Vec<BlockLine> = wrapped.into_iter().map(BlockLine::from).collect();

                if let Some(error) = &self.error {
                    lines.push(Line::from("").into());
                    lines.extend(failure_lines(
                        error,
                        ctx.mode,
                        ctx.content_width(),
                        theme.fg(theme.accent_error),
                    ));
                }

                // Input arguments
                if !self.input_args.is_empty() {
                    lines.push(Line::from("").into());
                    let max_args = if ctx.mode == DisplayMode::Truncated {
                        TRUNCATED_INPUT_ARGS
                    } else {
                        MAX_INPUT_ARGS
                    };
                    for (key, val) in self.input_args.iter().take(max_args) {
                        let key_budget =
                            ctx.content_width().saturating_sub(4).min(MAX_ARG_KEY_WIDTH);
                        let key = safe_single_line(key, key_budget);
                        let prefix = format!("  {key}: ");
                        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix.as_str());
                        let value_budget = ctx.content_width().saturating_sub(prefix_width);
                        let value = safe_single_line(val, value_budget);
                        lines.push(BlockLine::styled(Line::from(vec![
                            Span::styled(prefix, theme.muted()),
                            Span::styled(value, theme.primary()),
                        ])));
                    }
                    if self.input_args.len() > max_args {
                        let omitted = self.input_args.len() - max_args;
                        lines.push(BlockLine::styled(Line::from(Span::styled(
                            format!("  … ({omitted} argument(s) omitted; full values retained)"),
                            theme.dim(),
                        ))));
                    }
                }

                let max_inline = if ctx.mode == DisplayMode::Truncated {
                    TRUNCATED_INLINE_LINES
                } else {
                    MAX_INLINE_LINES
                };
                if let Some(ref output) = self.output {
                    lines.push(Line::from("").into());
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));

                    let mut output_lines = bounded_text_lines(
                        output,
                        ctx.content_width(),
                        theme.primary(),
                        // Keep the existing N visible result rows; the shared
                        // helper uses the additional row for its omission hint.
                        max_inline.saturating_add(1),
                        Some("full output retained"),
                        None,
                    );
                    for line in &mut output_lines {
                        *line = std::mem::take(line).with_panel_background(theme.bg_dark);
                    }
                    lines.extend(output_lines);

                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));
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
        !self.input_args.is_empty() || self.output.is_some() || self.error.is_some()
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

    fn preamble(&self, _ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        Some(Text::from(vec![self.header_line(&theme, false, None)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::BlockContext;

    fn ctx(mode: DisplayMode) -> BlockContext {
        BlockContext {
            width: 80,
            mode,
            is_running: false,
            raw: false,
            max_lines: None,
            appearance: Default::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn rendered_text(block: &UseToolCallBlock, mode: DisplayMode) -> String {
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

    #[test]
    fn truncated_caps_inline_output_tighter_than_expanded() {
        let mut block = UseToolCallBlock::new("linear__list_issues");
        let content: Vec<String> = (1..=12).map(|i| format!("l{i:02} row")).collect();
        block.output = Some(content.join("\n"));

        let truncated = rendered_text(&block, DisplayMode::Truncated);
        assert!(truncated.contains("l03"), "truncated:\n{truncated}");
        assert!(!truncated.contains("l04"), "truncated:\n{truncated}");
        assert!(
            truncated.contains("more content omitted"),
            "truncated:\n{truncated}"
        );

        let expanded = rendered_text(&block, DisplayMode::Expanded);
        assert!(expanded.contains("l10"), "expanded:\n{expanded}");
        assert!(!expanded.contains("l11"), "expanded:\n{expanded}");
        assert!(
            expanded.contains("more content omitted"),
            "expanded:\n{expanded}"
        );
    }

    #[test]
    fn expanded_failure_with_output_sanitizes_and_bounds_every_dynamic_field() {
        let tool_name = format!(
            "server\nforged\u{1b}]52;c;header\u{7}__action\u{202e}{}",
            "h".repeat(20_000)
        );
        let key = format!("key\nforged\u{1b}[31m{}", "k".repeat(20_000));
        let value = format!("value\r\u{2066}{}", "v".repeat(20_000));
        let output = (0..300)
            .map(|index| format!("row {index} \u{1b}]8;;file:///tmp/pwn\u{7}\u{0000}"))
            .collect::<Vec<_>>()
            .join("\n");
        let error = "failed\n\u{1b}[31mboom\u{1b}[0m\u{202e}".to_string();

        let mut block = UseToolCallBlock::new(tool_name.clone()).with_error(error.clone());
        block.input_args.push((key.clone(), value.clone()));
        block.output = Some(output.clone());

        let rendered = block.output(&ctx(DisplayMode::Expanded));
        let text = rendered_text(&block, DisplayMode::Expanded);
        assert!(
            rendered.lines.len() < 40,
            "unbounded card: {}",
            rendered.lines.len()
        );
        for control in ['\r', '\u{1b}', '\u{7}', '\u{0000}', '\u{202e}', '\u{2066}'] {
            assert!(!text.contains(control), "unsafe {control:?}: {text:?}");
        }
        assert!(text.contains("␛]52;c;header"), "{text:?}");
        assert!(text.contains("⟪U+202E⟫"), "{text:?}");
        assert!(text.contains("failed"), "{text:?}");
        assert!(text.contains("row 0"), "{text:?}");
        assert!(text.contains("full output retained"), "{text:?}");
        assert!(rendered.lines.iter().all(|line| {
            line.content
                .spans
                .iter()
                .all(|span| !span.content.contains('\n'))
        }));

        // Painting is a projection only: copy/search continue to read the
        // complete stored values.
        assert_eq!(block.tool_name, tool_name);
        assert_eq!(block.input_args, vec![(key, value)]);
        assert_eq!(block.output.as_deref(), Some(output.as_str()));
        assert_eq!(block.error.as_deref(), Some(error.as_str()));
        let copied = block.copy_text();
        assert!(copied.contains("row 299"));
        assert!(copied.contains("\u{1b}]8;;file:///tmp/pwn"));
    }
}
