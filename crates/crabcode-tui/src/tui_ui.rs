use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anstyle::{Ansi256Color, AnsiColor, Color as AnstyleColor, Style as AnstyleStyle};
use crabcode_pager_render::appearance::RendererLanguage;
use crabcode_pager_render::audited_glyphs::{check_mark, chevron, filled_dot};
#[cfg(test)]
use crabcode_pager_render::audited_theme::CrabCodeThemeKind;
use crabcode_pager_render::audited_theme::color_support;
use crabcode_pager_render::modal_window::{
    ModalSizing, ModalWindowConfig, Shortcut, embedded, render_modal_window,
};
#[cfg(test)]
use crabcode_pager_render::scrollback::selection::SelectionBox;
use crabcode_ratatui_inline::LinkSpan;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
#[cfg(test)]
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

#[cfg(test)]
use crate::crabcode_mermaid_affordance::{
    display_width as mermaid_display_width, is_mermaid_info, layout as mermaid_affordance_layout,
    segment_fits,
};
use crate::modal_surface::ActiveModal;
use crate::picker_surface::{
    PickerEntry, PickerHitAreas, PickerRow, PickerRowProductExt, PickerState,
    PickerStateProductExt, empty_picker_hit_areas, render_picker_content, render_picker_search_bar,
    render_picker_search_bar_with_label,
};
#[cfg(test)]
use crate::sdk_projection::Projection;
#[cfg(test)]
use crate::sdk_projection::{
    AdvisorInvocationState, AdvisorPresentation, AdvisorResultPresentation,
    DirectProgressPresentation,
};
use crate::sdk_projection::{ProjectedItem, ProjectedKind};
use crate::sdk_runtime::RawEnvelope;
use crate::selection_surface::centered_selection_geometry;
use crate::status_surface::{StatusItem, render_status_row};
use crate::task_panel::{TaskPanelRow, TaskPanelSnapshot, TaskPanelStatus};
#[cfg(test)]
use crate::terminal_output::render_terminal_lines;
#[cfg(test)]
use crate::text_safety::MAX_RENDER_FIELD_BYTES;
use crate::text_safety::{sanitize_bounded_terminal_text, sanitize_terminal_text};
use crate::tui_app::{
    GoalTaskState, GoalVerdict, GoalVerificationState, McpMenuAction, McpSettingsView,
    OAuthBrowserNotice, OverlayKind, QuestionDialogAction, QuestionFocus, RequestDialog, TuiApp,
    UiLanguage, canonical_goal_phase_ordinal, localized_permission_mode_label,
    question_dialog_actions,
};
#[cfg(test)]
use crate::tui_app::{
    TranscriptDisplayMode, TranscriptItemInteraction, TranscriptSelectionDirection,
    projected_item_is_visible,
};
use crate::tui_links::{LinkTarget, safe_standard_url_target};
#[cfg(test)]
use crate::tui_links::{
    MermaidAffordanceAction, SoftWrapJoiner, VisibleLink, VisibleLinkGroup, highlight_visible_link,
    local_link_to_file_target, resolve_link_target, visible_link_groups_from_soft_wrapped_lines,
};
use crate::tui_render::{CrabCodeTheme, fit_line_to_width, truncate_str, wrap_line};
use crate::turn_lifecycle::{AgentState, TurnActivity, WaitingReason, Watchers};
use crate::workspace_search::WorkspaceSearchKind;

#[cfg(test)]
const MAX_RENDERED_LINES_PER_ITEM: usize = 100_000;
#[cfg(test)]
const MAX_TRANSCRIPT_LAYOUT_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_COMPLETION_VISIBLE_ROWS: u16 = 8;
const PINNED_MARKDOWN_TAB_WIDTH: usize = 4;
const PINNED_MARKDOWN_H6: Color = Color::Rgb(90, 90, 90);
#[cfg(test)]
const MERMAID_AFFORDANCE_BODY_INDENT: u16 = 2;
#[cfg(test)]
const DIRECT_NESTED_LIVE_MESSAGE_LIMIT: usize = 3;
#[cfg(test)]
const DIRECT_MCP_PROGRESS_BAR_CELLS: usize = 20;
#[cfg(test)]
const DIRECT_MCP_PROGRESS_BLOCKS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
// The compact renderer's irreducible chrome is one header row, three
// transcript rows, a three-row composer, and one footer row. This is a floor,
// not a fixed minimal viewport: live transcript, todos, completion, and
// centered surfaces grow the viewport every frame up to the terminal height.
const MINIMAL_LAYOUT_FLOOR_ROWS: u16 = 8;
#[cfg(test)]
const ADVISOR_REVIEWED_MESSAGE_EN: &str =
    "Advisor has reviewed the conversation and will apply the feedback";
#[cfg(test)]
const ADVISOR_REVIEWED_MESSAGE_ZH: &str = "顾问已审阅对话，将应用反馈";
const fn renderer_language(language: UiLanguage) -> RendererLanguage {
    match language {
        UiLanguage::ZhCn => RendererLanguage::ZhCn,
        UiLanguage::EnUs => RendererLanguage::EnUs,
    }
}

/// Build the compact empty-transcript welcome shared by both transcript
/// owners.
///
/// The top ribbon already carries global session metadata, so this surface
/// keeps only the hierarchy needed to start working: product identity,
/// current readiness, one primary action, and two real renderer-owned
/// commands. Keeping the model line-oriented also makes the exact same copy
/// fit full, standard, and narrow terminals without a large ASCII banner.
pub(crate) fn empty_transcript_welcome_lines(
    app: &TuiApp,
    width: u16,
    theme: CrabCodeTheme,
) -> Vec<Line<'static>> {
    let language = app.ui_language();
    let initializing = matches!(app.projection.session_state(), Some("initializing"));
    let active = app.busy() || initializing;
    let status = match (language, active) {
        (UiLanguage::ZhCn, true) => "正在准备会话…",
        (UiLanguage::ZhCn, false) => "已就绪，可以开始",
        (UiLanguage::EnUs, true) => "Preparing the session…",
        (UiLanguage::EnUs, false) => "Ready to start",
    };
    let primary_action = match (language, width < 72) {
        (UiLanguage::ZhCn, _) => "› 直接描述目标，或粘贴错误信息与日志",
        (UiLanguage::EnUs, true) => "› Describe a goal or paste an error",
        (UiLanguage::EnUs, false) => "› Describe a goal, paste an error, or inspect this workspace",
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                "CrabCode",
                Style::default()
                    .fg(theme.accent_assistant)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                language.text("  原生 Rust TUI", "  native Rust TUI"),
                Style::default().fg(theme.gray),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "● ",
                Style::default().fg(if active {
                    theme.accent_running
                } else {
                    theme.accent_success
                }),
            ),
            Span::styled(
                status,
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::default(),
        Line::styled(
            language.text("快速开始", "Quick start"),
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(primary_action, Style::default().fg(theme.text_secondary)),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Enter", Style::default().fg(theme.accent_system)),
            Span::styled(
                language.text(" 发送", " send"),
                Style::default().fg(theme.gray),
            ),
            Span::styled("  ·  ", Style::default().fg(theme.gray_dim)),
            Span::styled("/help", Style::default().fg(theme.accent_system)),
            Span::styled(
                language.text(" 操作说明", " controls"),
                Style::default().fg(theme.gray),
            ),
            Span::styled("  ·  ", Style::default().fg(theme.gray_dim)),
            Span::styled("/model", Style::default().fg(theme.accent_system)),
            Span::styled(
                language.text(" 选择模型", " choose model"),
                Style::default().fg(theme.gray),
            ),
        ]),
    ];
    let width = usize::from(width.max(1));
    lines
        .into_iter()
        .map(|line| fit_line_to_width(line, width))
        .collect()
}

/// Closed renderer-local syntax palette denominator from fixed CrabCode.
///
/// This is intentionally outside `crabcode-markdown`: that crate remains the
/// byte-equivalent fixed Rust markdown core, while the product setting-to-
/// syntax mapping is a CrabCode rendering adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrabCodeSyntaxTheme {
    #[cfg(test)]
    Ansi,
    MonokaiExtended,
    #[cfg(test)]
    Github,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LogicalLinkRange {
    target: LinkTarget,
    start_byte: usize,
    end_byte: usize,
    /// Stable renderer-assigned ID. Fragments carrying the same ID are one
    /// logical Markdown link even when the renderer emits them on different
    /// pre-wrap lines.
    semantic_id: Option<u32>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct AnnotatedLine {
    line: Line<'static>,
    links: Vec<LogicalLinkRange>,
    source_line: usize,
}

#[cfg(test)]
impl AnnotatedLine {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            links: Vec::new(),
            source_line: 0,
        }
    }

    #[cfg(test)]
    fn with_source_line(mut self, source_line: usize) -> Self {
        self.source_line = source_line;
        self
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct CrabCodeCodeBlockSpan {
    info: String,
    body: String,
    output_line_range: Range<usize>,
    source_byte_range: Range<usize>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct CrabCodeMarkdownRender {
    lines: Vec<AnnotatedLine>,
    line_source_map: Vec<usize>,
    code_blocks: Vec<CrabCodeCodeBlockSpan>,
}

#[cfg(test)]
impl CrabCodeMarkdownRender {
    #[cfg(test)]
    fn from_lines(
        source: &str,
        lines: Vec<AnnotatedLine>,
        source_byte_base: usize,
        output_line_base: usize,
    ) -> Self {
        let line_source_map = lines
            .iter()
            .map(|line| line.source_line)
            .collect::<Vec<_>>();
        let code_blocks = crate::crabcode_markdown::closed_fenced_code_blocks(source)
            .into_iter()
            .map(|block| {
                let first_source_line = source[..block.source_byte_range.start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count();
                let last_source_line = if block.source_byte_range.is_empty() {
                    first_source_line
                } else {
                    source[..block.source_byte_range.end.saturating_sub(1)]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                };
                let output_start =
                    line_source_map.partition_point(|source_line| *source_line < first_source_line);
                let output_end = if block.source_byte_range.is_empty() {
                    output_start
                } else {
                    line_source_map.partition_point(|source_line| *source_line <= last_source_line)
                };
                CrabCodeCodeBlockSpan {
                    info: block.info,
                    body: block.body,
                    output_line_range: output_line_base.saturating_add(output_start)
                        ..output_line_base.saturating_add(output_end),
                    source_byte_range: source_byte_base
                        .saturating_add(block.source_byte_range.start)
                        ..source_byte_base.saturating_add(block.source_byte_range.end),
                }
            })
            .collect();
        Self {
            lines,
            line_source_map,
            code_blocks,
        }
    }

    fn prefix(&self, lines: usize) -> Self {
        let lines = lines.min(self.lines.len());
        Self {
            lines: self.lines[..lines].to_vec(),
            line_source_map: self.line_source_map[..lines].to_vec(),
            code_blocks: self
                .code_blocks
                .iter()
                .filter(|block| block.output_line_range.end <= lines)
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct CrabCodeMarkdownStream {
    raw_source: String,
    expanded_source: String,
    normalized_source: String,
    frozen_source_bytes: usize,
    frozen: CrabCodeMarkdownRender,
    output: CrabCodeMarkdownRender,
    renderer: crabcode_markdown_renderer::StreamingMarkdownRenderer,
    configured_style: Option<crabcode_markdown_renderer::MarkdownStyle>,
    configured_width: Option<usize>,
    configured_pretty: Option<bool>,
    configured_syntax_theme: Option<Option<CrabCodeSyntaxTheme>>,
    finished: bool,
    last_render_source_start: usize,
}

#[cfg(test)]
impl Default for CrabCodeMarkdownStream {
    fn default() -> Self {
        Self {
            raw_source: String::new(),
            expanded_source: String::new(),
            normalized_source: String::new(),
            frozen_source_bytes: 0,
            frozen: CrabCodeMarkdownRender::default(),
            output: CrabCodeMarkdownRender::default(),
            renderer: crabcode_markdown_renderer::StreamingMarkdownRenderer::new(
                crabcode_markdown_renderer::MarkdownStyle::default(),
                true,
            ),
            configured_style: None,
            configured_width: None,
            configured_pretty: None,
            configured_syntax_theme: None,
            finished: false,
            last_render_source_start: 0,
        }
    }
}

#[cfg(test)]
impl CrabCodeMarkdownStream {
    #[allow(clippy::too_many_arguments)]
    fn synchronize(
        &mut self,
        source: &str,
        streaming: bool,
        raw: bool,
        _default_style: Style,
        theme: CrabCodeTheme,
        syntax_theme: Option<CrabCodeSyntaxTheme>,
        width: usize,
        media_paths: &[PathBuf],
    ) -> &CrabCodeMarkdownRender {
        let width = width.max(1);
        let style = crabcode_markdown_style(theme);
        let append_only = source.starts_with(&self.raw_source);
        let syntax_changed = self.configured_syntax_theme != Some(syntax_theme);
        let source_reset = !append_only || syntax_changed;
        let previous_raw_len = self.raw_source.len();
        let previous_frozen_bytes = self.renderer.frozen_bytes();
        let mut renderer_reset = false;

        if source_reset {
            self.renderer.clear();
            self.expanded_source.clear();
            self.finished = false;
            renderer_reset = true;
        }
        self.configured_syntax_theme = Some(syntax_theme);

        if self.configured_style != Some(style) {
            self.renderer.set_style(style);
            self.configured_style = Some(style);
            self.finished = false;
            renderer_reset = true;
        }
        let pretty = !raw;
        if self.configured_pretty != Some(pretty) {
            self.renderer.set_pretty(pretty);
            self.configured_pretty = Some(pretty);
            self.finished = false;
            renderer_reset = true;
        }
        if self.configured_width != Some(width) {
            self.renderer.set_max_table_width(Some(width));
            self.configured_width = Some(width);
            self.finished = false;
            renderer_reset = true;
        } else if source_reset {
            // `clear()` intentionally resets the renderer's table width.
            self.renderer.set_max_table_width(Some(width));
        }

        let raw_suffix = if !source_reset {
            source
                .get(previous_raw_len..)
                .expect("append-only source suffix starts on a UTF-8 boundary")
        } else {
            source
        };
        let expanded_suffix = expand_pinned_markdown_tabs(raw_suffix);
        if source_reset {
            self.expanded_source.clear();
            self.expanded_source.push_str(&expanded_suffix);
        } else {
            self.expanded_source.push_str(&expanded_suffix);
        }

        self.last_render_source_start =
            if streaming && !source_reset && !renderer_reset && !raw_suffix.is_empty() {
                previous_frozen_bytes
            } else {
                0
            };

        if streaming {
            let syntax = syntax_theme.map(crabcode_markdown_syntax_highlighter);
            if raw_suffix.is_empty() {
                if renderer_reset {
                    self.renderer.render(syntax);
                }
            } else {
                self.renderer.push_and_render(&expanded_suffix, syntax);
            }
            self.finished = false;
        } else {
            if !raw_suffix.is_empty() {
                self.renderer.push(&expanded_suffix);
            }
            if !self.finished || renderer_reset || !raw_suffix.is_empty() {
                self.renderer
                    .finish(syntax_theme.map(crabcode_markdown_syntax_highlighter));
            }
            self.finished = true;
        }

        source.clone_into(&mut self.raw_source);
        self.renderer
            .source()
            .clone_into(&mut self.normalized_source);
        self.frozen_source_bytes = self.renderer.frozen_bytes();
        let frozen_lines = self.renderer.frozen_lines_count();
        self.output = crabcode_markdown_render_from_view(self.renderer.view(), media_paths);
        self.frozen = self.output.prefix(frozen_lines);
        &self.output
    }

    #[cfg(test)]
    fn push_and_render(
        &mut self,
        chunk: &str,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
    ) -> &CrabCodeMarkdownRender {
        let mut source = self.raw_source.clone();
        source.push_str(chunk);
        self.synchronize(
            &source,
            true,
            false,
            default_style,
            theme,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            width,
            &[],
        )
    }

    #[cfg(test)]
    fn finish(
        &mut self,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
    ) -> &CrabCodeMarkdownRender {
        let source = self.raw_source.clone();
        self.synchronize(
            &source,
            false,
            false,
            default_style,
            theme,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            width,
            &[],
        )
    }
}

fn expand_pinned_markdown_tabs(source: &str) -> std::borrow::Cow<'_, str> {
    if !source.contains('\t') {
        return std::borrow::Cow::Borrowed(source);
    }
    std::borrow::Cow::Owned(source.replace('\t', &" ".repeat(PINNED_MARKDOWN_TAB_WIDTH)))
}

#[cfg(test)]
fn crabcode_syntax_theme(
    kind: CrabCodeThemeKind,
    syntax_highlighting_disabled: bool,
) -> Option<CrabCodeSyntaxTheme> {
    if syntax_highlighting_disabled {
        return None;
    }
    Some(match kind {
        CrabCodeThemeKind::Dark | CrabCodeThemeKind::DarkDaltonized => {
            CrabCodeSyntaxTheme::MonokaiExtended
        }
        CrabCodeThemeKind::Light | CrabCodeThemeKind::LightDaltonized => {
            CrabCodeSyntaxTheme::Github
        }
        CrabCodeThemeKind::LightAnsi | CrabCodeThemeKind::DarkAnsi => CrabCodeSyntaxTheme::Ansi,
        CrabCodeThemeKind::Auto => {
            unreachable!("Auto must be resolved before selecting a syntax palette")
        }
    })
}

#[cfg(test)]
fn app_syntax_theme(app: &TuiApp) -> Option<CrabCodeSyntaxTheme> {
    // Fixed ThemePicker uses `settings.syntaxHighlightingDisabled ?? false`;
    // before renderer_context there is no transcript content to highlight.
    crabcode_syntax_theme(
        app.renderer_theme_kind(),
        app.renderer_syntax_highlighting_disabled().unwrap_or(false),
    )
}

fn crabcode_markdown_syntax_highlighter(
    syntax_theme: CrabCodeSyntaxTheme,
) -> &'static crabcode_markdown_renderer::Syntect {
    #[cfg(test)]
    static ANSI: OnceLock<crabcode_markdown_renderer::Syntect> = OnceLock::new();
    static MONOKAI_EXTENDED: OnceLock<crabcode_markdown_renderer::Syntect> = OnceLock::new();
    #[cfg(test)]
    static GITHUB: OnceLock<crabcode_markdown_renderer::Syntect> = OnceLock::new();
    let (slot, name) = match syntax_theme {
        #[cfg(test)]
        CrabCodeSyntaxTheme::Ansi => (&ANSI, two_face::theme::EmbeddedThemeName::Ansi),
        CrabCodeSyntaxTheme::MonokaiExtended => (
            &MONOKAI_EXTENDED,
            two_face::theme::EmbeddedThemeName::MonokaiExtended,
        ),
        #[cfg(test)]
        CrabCodeSyntaxTheme::Github => (&GITHUB, two_face::theme::EmbeddedThemeName::Github),
    };
    slot.get_or_init(|| {
        let themes = two_face::theme::extra();
        crabcode_markdown_renderer::Syntect {
            theme: themes.get(name).clone(),
            syntax_set: two_face::syntax::extra_newlines(),
        }
    })
}

/// Copy payload for the pinned scrollback block-content action.
///
/// Markdown blocks follow the upstream per-entry raw/pretty contract. Tool
/// uses are intentionally absent: their mutable upstream block becomes
/// copyable only when result content exists, and `TuiApp` correlates that
/// exact result before calling this function.
pub(crate) fn projected_item_copy_text(item: &ProjectedItem, raw: bool) -> Option<String> {
    match item.kind {
        ProjectedKind::User => Some(item.text.clone()),
        ProjectedKind::Assistant | ProjectedKind::Thinking if raw => Some(item.text.clone()),
        ProjectedKind::Assistant | ProjectedKind::Thinking => {
            Some(rendered_markdown_plain_text(&item.text))
        }
        ProjectedKind::ToolResult | ProjectedKind::TerminalOutput => Some(item.text.clone()),
        ProjectedKind::ToolUse
        | ProjectedKind::System
        | ProjectedKind::Progress
        | ProjectedKind::Warning
        | ProjectedKind::Error => None,
    }
}

/// Width-independent plain text supplied to the fixed transcript-search core.
///
/// Markdown uses the same rendered/plain source as the upstream scrollback
/// index. Tool and system projections retain their exact stored title/body
/// fields; UI-only borders, timestamps, status hints, and wrapped rows are not
/// indexed.
pub(crate) fn projected_item_searchable_text(item: &ProjectedItem) -> Option<String> {
    let parts = match item.kind {
        ProjectedKind::User => vec![item.text.clone()],
        ProjectedKind::Assistant | ProjectedKind::Thinking => {
            vec![rendered_markdown_plain_text(&item.text)]
        }
        ProjectedKind::ToolUse
        | ProjectedKind::ToolResult
        | ProjectedKind::TerminalOutput
        | ProjectedKind::System
        | ProjectedKind::Progress
        | ProjectedKind::Warning
        | ProjectedKind::Error => vec![item.title.clone(), item.text.clone()],
    };
    let joined = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn rendered_markdown_plain_text(source: &str) -> String {
    let mut renderer = crabcode_markdown_renderer::StreamingMarkdownRenderer::new(
        crabcode_markdown_style(CrabCodeTheme::NIGHT),
        true,
    );
    let expanded = expand_pinned_markdown_tabs(source);
    renderer.push(&expanded);
    renderer.finish(Some(crabcode_markdown_syntax_highlighter(
        CrabCodeSyntaxTheme::MonokaiExtended,
    )));
    renderer
        .view()
        .lines
        .iter()
        .enumerate()
        .fold(String::new(), |mut output, (index, line)| {
            if index > 0 {
                output.push('\n');
            }
            for span in &line.spans {
                output.push_str(&span.content);
            }
            output
        })
}

fn ratatui_color_to_anstyle(color: Color) -> Option<AnstyleColor> {
    Some(match color {
        Color::Reset => return None,
        Color::Rgb(red, green, blue) => AnstyleColor::Rgb(anstyle::RgbColor(red, green, blue)),
        Color::Indexed(index) => AnstyleColor::Ansi256(Ansi256Color(index)),
        Color::Black => AnstyleColor::Ansi(AnsiColor::Black),
        Color::Red => AnstyleColor::Ansi(AnsiColor::Red),
        Color::Green => AnstyleColor::Ansi(AnsiColor::Green),
        Color::Yellow => AnstyleColor::Ansi(AnsiColor::Yellow),
        Color::Blue => AnstyleColor::Ansi(AnsiColor::Blue),
        Color::Magenta => AnstyleColor::Ansi(AnsiColor::Magenta),
        Color::Cyan => AnstyleColor::Ansi(AnsiColor::Cyan),
        Color::Gray => AnstyleColor::Ansi(AnsiColor::White),
        Color::DarkGray => AnstyleColor::Ansi(AnsiColor::BrightBlack),
        Color::LightRed => AnstyleColor::Ansi(AnsiColor::BrightRed),
        Color::LightGreen => AnstyleColor::Ansi(AnsiColor::BrightGreen),
        Color::LightYellow => AnstyleColor::Ansi(AnsiColor::BrightYellow),
        Color::LightBlue => AnstyleColor::Ansi(AnsiColor::BrightBlue),
        Color::LightMagenta => AnstyleColor::Ansi(AnsiColor::BrightMagenta),
        Color::LightCyan => AnstyleColor::Ansi(AnsiColor::BrightCyan),
        Color::White => AnstyleColor::Ansi(AnsiColor::BrightWhite),
    })
}

fn markdown_foreground(color: Color) -> AnstyleStyle {
    AnstyleStyle::new().fg_color(ratatui_color_to_anstyle(color))
}

fn markdown_background(color: Color) -> AnstyleStyle {
    AnstyleStyle::new().bg_color(ratatui_color_to_anstyle(color))
}

fn crabcode_markdown_style(theme: CrabCodeTheme) -> crabcode_markdown_renderer::MarkdownStyle {
    let heading_colors = [
        theme.markdown_h1,
        theme.markdown_h2,
        theme.markdown_h3,
        theme.gray_bright,
        theme.gray,
        PINNED_MARKDOWN_H6,
    ];
    let heading_inner = std::array::from_fn(|index| {
        let style = markdown_foreground(heading_colors[index]);
        if index < 5 { style.bold() } else { style }
    });
    let heading_outer = heading_colors.map(|color| markdown_foreground(color).dimmed().hidden());
    crabcode_markdown_renderer::MarkdownStyle {
        heading_inner,
        heading_outer,
        strong_inner: markdown_foreground(theme.markdown_text).bold(),
        strong_outer: AnstyleStyle::new().dimmed().hidden(),
        emphasis_inner: markdown_foreground(theme.markdown_text).italic(),
        emphasis_outer: AnstyleStyle::new().dimmed().hidden(),
        strikethrough_inner: markdown_foreground(theme.markdown_text).strikethrough(),
        strikethrough_outer: AnstyleStyle::new().dimmed().hidden(),
        inline_code_inner: markdown_foreground(theme.markdown_code).bold(),
        inline_code_outer: markdown_foreground(theme.markdown_code).dimmed().hidden(),
        blockquote_outer: markdown_foreground(theme.gray).dimmed(),
        task_checked: markdown_foreground(theme.accent_success),
        task_unchecked: markdown_foreground(theme.text_secondary).dimmed(),
        list_item: markdown_foreground(theme.gray),
        rule: markdown_foreground(theme.gray),
        link_outer: markdown_foreground(theme.gray),
        link_text: markdown_foreground(theme.link).underline(),
        link_url: markdown_foreground(theme.gray),
        link_title: markdown_foreground(theme.gray),
        code_outer: markdown_foreground(theme.markdown_code).dimmed().hidden(),
        code_language: markdown_foreground(theme.markdown_h3).hidden(),
        code_untagged: markdown_foreground(theme.markdown_text),
        code_background: markdown_background(theme.markdown_code_bg),
        table_outer: markdown_foreground(theme.markdown_h2).hidden(),
        text: markdown_foreground(theme.markdown_text),
        math: markdown_foreground(theme.markdown_text).italic(),
    }
}

#[cfg(test)]
fn display_column_to_exact_byte_offset(text: &str, target: usize) -> Option<usize> {
    let mut width = 0_usize;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if width == target {
            return Some(offset);
        }
        width = width.checked_add(grapheme.width())?;
        if width > target {
            return None;
        }
    }
    (width == target).then_some(text.len())
}

#[cfg(test)]
fn crabcode_markdown_render_from_view(
    view: crabcode_markdown_renderer::MarkdownRenderView<'_>,
    media_paths: &[PathBuf],
) -> CrabCodeMarkdownRender {
    assert_eq!(
        view.lines.len(),
        view.line_source_map.len(),
        "Markdown renderer line/source-map cardinality invariant"
    );
    let mut links_by_line = vec![Vec::new(); view.lines.len()];
    for hyperlink in view.hyperlinks {
        let line = view
            .lines
            .get(hyperlink.line_index)
            .expect("Markdown renderer hyperlink line index is in range");
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let start_byte = display_column_to_exact_byte_offset(&text, hyperlink.column_range.start)
            .expect("Markdown renderer hyperlink start is a grapheme-cell boundary");
        let end_byte = display_column_to_exact_byte_offset(&text, hyperlink.column_range.end)
            .expect("Markdown renderer hyperlink end is a grapheme-cell boundary");
        assert!(
            start_byte < end_byte,
            "Markdown renderer hyperlink range is non-empty"
        );
        if let Some(target) = semantic_markdown_target(&hyperlink.url, media_paths) {
            links_by_line[hyperlink.line_index].push(LogicalLinkRange {
                target,
                start_byte,
                end_byte,
                semantic_id: Some(hyperlink.id),
            });
        }
    }
    let lines = view
        .lines
        .iter()
        .cloned()
        .zip(view.line_source_map.iter().copied())
        .zip(links_by_line)
        .map(|((line, source_line), links)| AnnotatedLine {
            line,
            links,
            source_line,
        })
        .collect();
    let code_blocks = view
        .code_blocks
        .iter()
        .map(|block| CrabCodeCodeBlockSpan {
            info: block.info.clone(),
            body: block.body.clone(),
            output_line_range: block.output_line_range.clone(),
            source_byte_range: block.source_byte_range.clone(),
        })
        .collect();
    CrabCodeMarkdownRender {
        lines,
        line_source_map: view.line_source_map.to_vec(),
        code_blocks,
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct RenderedTranscriptPart {
    lines: Vec<Line<'static>>,
    link_groups: Vec<VisibleLinkGroup>,
}

#[derive(Debug, Clone, Default)]
#[cfg(test)]
struct VisibleTranscript {
    lines: Vec<Line<'static>>,
    link_groups: Vec<VisibleLinkGroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum NoticeKind {
    Startup,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct NoticeRecord {
    kind: NoticeKind,
    text: String,
}

#[derive(Debug, Default)]
pub(crate) struct ArtifactProvenance {
    media_by_tool_use: HashMap<String, Vec<PathBuf>>,
    image_numbers: HashMap<PathBuf, usize>,
    scanned_sequences: HashSet<u64>,
    image_preview: Option<(
        PathBuf,
        Arc<crate::crabcode_image_overlay::CrabCodeImagePreview>,
    )>,
}

impl ArtifactProvenance {
    #[cfg(test)]
    fn ingest_unseen(&mut self, raw_envelopes: &[RawEnvelope]) {
        for envelope in raw_envelopes {
            self.ingest_envelope(envelope);
        }
    }

    pub(crate) fn ingest_envelope(&mut self, envelope: &RawEnvelope) {
        if !self.scanned_sequences.insert(envelope.sequence) {
            return;
        }
        if matches!(
            &envelope.classification,
            crate::sdk_runtime::EnvelopeClass::User
        ) {
            self.ingest_message(&envelope.value);
        }
    }

    fn ingest_message(&mut self, message: &serde_json::Value) {
        if message.get("type").and_then(serde_json::Value::as_str) != Some("user") {
            return;
        }
        let Some(artifacts) = message
            .get("tool_artifacts")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for artifact in artifacts {
            self.ingest_artifact(artifact);
        }
    }

    fn ingest_artifact(&mut self, artifact: &serde_json::Value) {
        let Some(object) = artifact.as_object() else {
            return;
        };
        let Some(kind @ ("image" | "video")) =
            object.get("kind").and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let Some(producer) = object
            .get("producerToolUseId")
            .and_then(serde_json::Value::as_str)
            .filter(|producer| !producer.is_empty())
        else {
            return;
        };
        let Some(location) = object
            .get("location")
            .and_then(serde_json::Value::as_object)
        else {
            return;
        };
        if location.get("type").and_then(serde_json::Value::as_str) != Some("runtimePath") {
            return;
        }
        let Some(path) = location
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
        else {
            return;
        };
        let media = self
            .media_by_tool_use
            .entry(producer.to_string())
            .or_default();
        if !media.contains(&path) {
            media.push(path.clone());
        }
        if kind == "image" && !self.image_numbers.contains_key(&path) {
            let display_number = self.image_numbers.len().saturating_add(1);
            self.image_numbers.insert(path, display_number);
        }
    }

    #[cfg(test)]
    fn media_paths_for(&self, item: &ProjectedItem) -> &[PathBuf] {
        item.tool_use_id
            .as_deref()
            .and_then(|tool_use_id| self.media_by_tool_use.get(tool_use_id))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn admitted_image_path(&self, target: &LinkTarget) -> Option<(PathBuf, usize)> {
        let LinkTarget::File(path) = target else {
            return None;
        };
        self.image_numbers
            .get(path.as_ref())
            .copied()
            .map(|display_number| (path.to_path_buf(), display_number))
    }

    pub(crate) fn image_preview(
        &mut self,
        target: &LinkTarget,
    ) -> Option<Arc<crate::crabcode_image_overlay::CrabCodeImagePreview>> {
        let (path, display_number) = self.admitted_image_path(target)?;
        if let Some((cached_path, preview)) = self.image_preview.as_ref()
            && cached_path == &path
        {
            return Some(Arc::clone(preview));
        }
        let preview = Arc::new(crate::crabcode_image_overlay::load_provenance_image(
            &path,
            display_number,
        ));
        self.image_preview = Some((path, Arc::clone(&preview)));
        Some(preview)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg(test)]
struct DirectNestedProgressIdentity {
    progress_type: String,
    parent_tool_use_id: String,
    raw_sequence: u64,
}

#[cfg(test)]
impl DirectNestedProgressIdentity {
    fn family(&self) -> (String, String) {
        (self.progress_type.clone(), self.parent_tool_use_id.clone())
    }
}

#[cfg(test)]
fn direct_nested_progress_identity(item: &ProjectedItem) -> Option<DirectNestedProgressIdentity> {
    let DirectProgressPresentation::Nested {
        progress_type,
        parent_tool_use_id,
        ..
    } = item.presentation.direct_progress.as_ref()?
    else {
        return None;
    };
    let [raw_sequence] = item.raw_sequences.as_slice() else {
        return None;
    };
    Some(DirectNestedProgressIdentity {
        progress_type: progress_type.clone(),
        parent_tool_use_id: parent_tool_use_id.clone(),
        raw_sequence: *raw_sequence,
    })
}

#[cfg(test)]
fn item_has_direct_nested_progress(item: &ProjectedItem) -> bool {
    matches!(
        item.presentation.direct_progress.as_ref(),
        Some(DirectProgressPresentation::Nested { .. })
    )
}

#[cfg(test)]
fn direct_agent_progress_is_external_user(item: &ProjectedItem) -> bool {
    matches!(
        item.presentation.direct_progress.as_ref(),
        Some(DirectProgressPresentation::Nested {
            progress_type,
            message_kind,
            ..
        }) if progress_type == "agent_progress"
            && *message_kind == crate::sdk_projection::DirectNestedMessageKind::User
    )
}

/// Fixed historical Agent/Skill progress is sliced by outer progress message,
/// never by the number of projected content blocks inside that message.
///
/// One direct progress envelope can expand to several `ProjectedItem`s. Its
/// one raw sequence is therefore the exact renderer-local group identity. A
/// nested item without that invariant is suppressed rather than grouped from
/// a parsed key or inferred message contents.
#[derive(Debug, Default)]
#[cfg(test)]
struct DirectNestedProgressLayoutPlan {
    visible_groups: HashSet<DirectNestedProgressIdentity>,
    prompt_item_keys: HashSet<String>,
    suppressed_item_keys: HashSet<String>,
}

#[cfg(test)]
impl DirectNestedProgressLayoutPlan {
    fn build(
        items: &[ProjectedItem],
        committed: &HashSet<String>,
        presentation_verbose: bool,
    ) -> Self {
        let mut plan = Self::default();
        let mut groups_by_family =
            HashMap::<(String, String), Vec<DirectNestedProgressIdentity>>::new();
        let mut seen_groups = HashSet::<DirectNestedProgressIdentity>::new();
        let mut first_item_by_group = HashMap::<DirectNestedProgressIdentity, String>::new();
        let mut prompt_by_group = HashMap::<DirectNestedProgressIdentity, String>::new();

        for item in items {
            if committed.contains(&item.key)
                || !projected_item_is_visible(item, presentation_verbose)
                || !item_has_direct_nested_progress(item)
            {
                continue;
            }
            if direct_agent_progress_is_external_user(item) {
                plan.suppressed_item_keys.insert(item.key.clone());
                continue;
            }
            let Some(identity) = direct_nested_progress_identity(item) else {
                plan.suppressed_item_keys.insert(item.key.clone());
                continue;
            };
            first_item_by_group
                .entry(identity.clone())
                .or_insert_with(|| item.key.clone());
            if let Some(DirectProgressPresentation::Nested { prompt, .. }) =
                item.presentation.direct_progress.as_ref()
            {
                prompt_by_group
                    .entry(identity.clone())
                    .or_insert_with(|| prompt.clone());
            }
            if seen_groups.insert(identity.clone()) {
                groups_by_family
                    .entry(identity.family())
                    .or_default()
                    .push(identity);
            }
        }

        for ((progress_type, _), groups) in groups_by_family {
            let visible_start = if presentation_verbose {
                0
            } else {
                groups
                    .len()
                    .saturating_sub(DIRECT_NESTED_LIVE_MESSAGE_LIMIT)
            };
            plan.visible_groups
                .extend(groups[visible_start..].iter().cloned());

            if presentation_verbose
                && progress_type == "agent_progress"
                && let Some(first_group) = groups.first()
                && prompt_by_group
                    .get(first_group)
                    .is_some_and(|prompt| !prompt.is_empty())
                && let Some(first_item_key) = first_item_by_group.get(first_group)
            {
                plan.prompt_item_keys.insert(first_item_key.clone());
            }
        }
        plan
    }

    fn item_is_visible(&self, item: &ProjectedItem) -> bool {
        if self.suppressed_item_keys.contains(&item.key) {
            return false;
        }
        if !item_has_direct_nested_progress(item) {
            return true;
        }
        direct_nested_progress_identity(item)
            .is_some_and(|identity| self.visible_groups.contains(&identity))
    }

    fn item_shows_prompt(&self, item: &ProjectedItem) -> bool {
        self.prompt_item_keys.contains(&item.key)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg(test)]
struct DirectHookTranscriptCounts {
    pre_tool_use: usize,
    post_tool_use: usize,
}

#[cfg(test)]
impl DirectHookTranscriptCounts {
    #[cfg(test)]
    fn for_item(item: &ProjectedItem, projection: &Projection, transcript_mode: bool) -> Self {
        if !transcript_mode || item.presentation.direct_progress.is_some() {
            return Self::default();
        }
        let Some(tool_use_id) = item.tool_use_id.as_deref() else {
            return Self::default();
        };
        match item.kind {
            ProjectedKind::ToolUse if item.presentation.tool.is_some() => Self {
                pre_tool_use: projection
                    .direct_hook_progress_presentation(tool_use_id, "PreToolUse")
                    .map_or(0, |presentation| presentation.in_progress_count),
                post_tool_use: 0,
            },
            ProjectedKind::ToolResult | ProjectedKind::TerminalOutput
                if item
                    .presentation
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.result.is_some() && tool.is_error != Some(true)) =>
            {
                Self {
                    pre_tool_use: 0,
                    post_tool_use: projection
                        .direct_hook_progress_presentation(tool_use_id, "PostToolUse")
                        .map_or(0, |presentation| presentation.in_progress_count),
                }
            }
            _ => Self::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ItemLayoutRevision {
    kind: ProjectedKind,
    streaming: bool,
    raw_sequence_count: usize,
    last_raw_sequence: Option<u64>,
    title_bytes: usize,
    text_bytes: usize,
    tool_use_id_bytes: usize,
    media_paths: Vec<PathBuf>,
    display_mode: TranscriptDisplayMode,
    raw: bool,
    selected: bool,
    presentation_verbose: bool,
    nested_show_prompt: bool,
    direct_hook_transcript_counts: DirectHookTranscriptCounts,
}

#[cfg(test)]
impl ItemLayoutRevision {
    #[allow(clippy::too_many_arguments)]
    fn from_item(
        item: &ProjectedItem,
        media_paths: &[PathBuf],
        display_mode: TranscriptDisplayMode,
        raw: bool,
        selected: bool,
        presentation_verbose: bool,
        nested_show_prompt: bool,
        direct_hook_transcript_counts: DirectHookTranscriptCounts,
    ) -> Self {
        Self {
            kind: item.kind,
            streaming: item.streaming,
            raw_sequence_count: item.raw_sequences.len(),
            last_raw_sequence: item.raw_sequences.last().copied(),
            title_bytes: item.title.len(),
            text_bytes: item.text.len(),
            tool_use_id_bytes: item.tool_use_id.as_ref().map_or(0, String::len),
            media_paths: media_paths.to_vec(),
            display_mode,
            raw,
            selected,
            presentation_verbose,
            nested_show_prompt,
            direct_hook_transcript_counts,
        }
    }
}

#[derive(Debug)]
#[cfg(test)]
struct TranscriptLayoutEntry {
    key: String,
    source_index: usize,
    revision: ItemLayoutRevision,
    markdown_stream: CrabCodeMarkdownStream,
    line_count: usize,
    ends_with_blank: bool,
    cached_render: Option<RenderedTranscriptPart>,
    cache_charge: usize,
    last_used: u64,
}

#[derive(Debug)]
#[cfg(test)]
enum LayoutAnchor {
    Notices {
        row: usize,
        line_count: usize,
        width: usize,
    },
    Item {
        key: String,
        row: usize,
        item_line_count: usize,
        width: usize,
    },
}

/// Width-specific transcript layout. It stores one small record per projected
/// item and a bounded LRU of rendered lines; the complete backend projection
/// remains authoritative and is never removed.
#[derive(Debug, Default)]
#[cfg(test)]
pub(crate) struct TranscriptLayoutCache {
    width: usize,
    theme: Option<CrabCodeTheme>,
    syntax_theme: Option<Option<CrabCodeSyntaxTheme>>,
    ui_language: UiLanguage,
    rendered_ui_language: Option<UiLanguage>,
    total_lines: usize,
    source_version: u64,
    source_item_count: usize,
    presentation_generation: u64,
    direct_hook_transcript_mode: bool,
    committed_keys: HashSet<String>,
    entries: Vec<TranscriptLayoutEntry>,
    notice_revision: Vec<NoticeRecord>,
    notice_render: RenderedTranscriptPart,
    artifact_provenance: ArtifactProvenance,
    cache_bytes: usize,
    tick: u64,
    #[cfg(test)]
    render_passes: usize,
}

#[cfg(test)]
impl TranscriptLayoutCache {
    fn set_ui_language(&mut self, ui_language: UiLanguage) {
        self.ui_language = ui_language;
    }

    /// Retain the bounded presentation provenance before Projection is allowed
    /// to evict the complete raw diagnostic envelope.
    pub(crate) fn ingest_artifact_envelope(&mut self, envelope: &RawEnvelope) {
        self.artifact_provenance.ingest_envelope(envelope);
    }

    pub(crate) fn artifact_image_preview(
        &mut self,
        target: &LinkTarget,
    ) -> Option<Arc<crate::crabcode_image_overlay::CrabCodeImagePreview>> {
        self.artifact_provenance.image_preview(target)
    }

    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.total_lines
    }

    #[allow(clippy::too_many_arguments)]
    fn synchronize(
        &mut self,
        items: &[ProjectedItem],
        raw_envelopes: &[RawEnvelope],
        projection: &Projection,
        source_version: u64,
        committed: &HashSet<String>,
        item_interactions: &HashMap<String, TranscriptItemInteraction>,
        selected_key: Option<&str>,
        presentation_verbose: bool,
        direct_hook_transcript_mode: bool,
        presentation_generation: u64,
        notices: &[NoticeRecord],
        theme: CrabCodeTheme,
        syntax_theme: Option<CrabCodeSyntaxTheme>,
        width: usize,
        scroll: &mut crate::tui_render::ScrollState,
        viewport_height: usize,
    ) {
        let width = width.max(1);
        let theme_changed = self.theme != Some(theme);
        let syntax_theme_changed = self.syntax_theme != Some(syntax_theme);
        let ui_language_changed = self.rendered_ui_language != Some(self.ui_language);
        if self.source_version != source_version {
            self.artifact_provenance.ingest_unseen(raw_envelopes);
        }
        if self.width == width
            && self.source_version == source_version
            && self.source_item_count == items.len()
            && self.presentation_generation == presentation_generation
            && self.direct_hook_transcript_mode == direct_hook_transcript_mode
            && self.committed_keys == *committed
            && self.notice_revision == notices
            && !theme_changed
            && !syntax_theme_changed
            && !ui_language_changed
        {
            scroll.clamp(self.total_lines, viewport_height);
            return;
        }

        let anchor = (!scroll.is_following())
            .then(|| self.anchor_at(scroll.offset()))
            .flatten();
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        let old_width = self.width;
        if old_width != width || self.notice_revision != notices || theme_changed {
            self.notice_render = render_notice_records(notices, width, theme);
            self.notice_revision = notices.to_vec();
        }
        let mut old_by_key = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect::<HashMap<_, _>>();
        let mut entries = Vec::with_capacity(items.len().saturating_sub(committed.len()));
        let nested_progress_plan =
            DirectNestedProgressLayoutPlan::build(items, committed, presentation_verbose);

        for (source_index, item) in items.iter().enumerate() {
            if committed.contains(&item.key)
                || !projected_item_is_visible(item, presentation_verbose)
                || !nested_progress_plan.item_is_visible(item)
            {
                continue;
            }
            let media_paths = self.artifact_provenance.media_paths_for(item);
            let display_mode = item_interactions
                .get(&item.key)
                .map_or(TranscriptDisplayMode::Expanded, |state| state.mode);
            let raw = item_interactions
                .get(&item.key)
                .is_some_and(|state| state.raw);
            let selected = selected_key == Some(item.key.as_str());
            let nested_show_prompt = nested_progress_plan.item_shows_prompt(item);
            let direct_hook_transcript_counts =
                DirectHookTranscriptCounts::for_item(item, projection, direct_hook_transcript_mode);
            let revision = ItemLayoutRevision::from_item(
                item,
                media_paths,
                display_mode,
                raw,
                selected,
                presentation_verbose,
                nested_show_prompt,
                direct_hook_transcript_counts,
            );
            let mut old_entry = old_by_key.remove(&item.key);
            if old_entry.as_ref().is_some_and(|entry| {
                old_width == width
                    && !theme_changed
                    && !syntax_theme_changed
                    && !ui_language_changed
                    && entry.revision == revision
                    && (item.kind == ProjectedKind::TerminalOutput
                        || entry.markdown_stream.raw_source.as_str()
                            == sanitize_bounded_terminal_text(&item.text).as_ref())
            }) {
                let mut entry = old_entry
                    .take()
                    .expect("matching existing transcript entry remains available");
                entry.source_index = source_index;
                entries.push(entry);
                continue;
            }
            let mut markdown_stream = old_entry
                .filter(|entry| {
                    old_width == width
                        && entry.revision.kind == revision.kind
                        && entry.revision.media_paths == revision.media_paths
                })
                .map_or_else(CrabCodeMarkdownStream::default, |entry| {
                    entry.markdown_stream
                });
            let rendered = render_projected_item_with_state_context(
                item,
                width,
                theme,
                media_paths,
                display_mode,
                selected,
                &mut markdown_stream,
                ProjectedItemRenderContext {
                    show_nested_agent_prompt: nested_show_prompt,
                    direct_hook_transcript_counts,
                    raw,
                    syntax_theme,
                    ui_language: self.ui_language,
                },
            );
            #[cfg(test)]
            {
                self.render_passes = self.render_passes.saturating_add(1);
            }
            let ends_with_blank = rendered.lines.last().is_some_and(|line| line.width() == 0);
            let cache_charge = rendered_part_charge(&rendered);
            entries.push(TranscriptLayoutEntry {
                key: item.key.clone(),
                source_index,
                revision,
                markdown_stream,
                line_count: rendered.lines.len(),
                ends_with_blank,
                cached_render: Some(rendered),
                cache_charge,
                last_used: tick,
            });
        }

        self.entries = entries;
        self.width = width;
        self.theme = Some(theme);
        self.syntax_theme = Some(syntax_theme);
        self.rendered_ui_language = Some(self.ui_language);
        self.source_version = source_version;
        self.source_item_count = items.len();
        self.presentation_generation = presentation_generation;
        self.direct_hook_transcript_mode = direct_hook_transcript_mode;
        self.committed_keys.clone_from(committed);
        self.recalculate_totals();
        let selected_cache_entry = selected_key
            .and_then(|selected| self.entries.iter().position(|entry| entry.key == selected));
        self.trim_cache(selected_cache_entry.as_slice());

        match anchor {
            Some(LayoutAnchor::Notices {
                row,
                line_count,
                width: anchor_width,
            }) if !self.notice_render.lines.is_empty() => {
                let effective = self.notice_render.lines.len();
                let row = scaled_anchor_row(row, line_count, effective, anchor_width, width);
                scroll.set_offset(row, self.total_lines, viewport_height);
                return;
            }
            Some(LayoutAnchor::Item {
                key,
                row,
                item_line_count,
                width: anchor_width,
            }) => {
                if let Some((start, entry)) = self.entry_start_and_value(&key) {
                    let effective = self.effective_entry_lines(entry);
                    let row =
                        scaled_anchor_row(row, item_line_count, effective, anchor_width, width);
                    scroll.set_offset(start.saturating_add(row), self.total_lines, viewport_height);
                    return;
                }
            }
            _ => {}
        }
        scroll.clamp(self.total_lines, viewport_height);
    }

    fn visible_lines(
        &mut self,
        items: &[ProjectedItem],
        offset: usize,
        viewport_height: usize,
        theme: CrabCodeTheme,
    ) -> VisibleTranscript {
        if viewport_height == 0 || self.total_lines == 0 {
            return VisibleTranscript::default();
        }
        self.tick = self.tick.saturating_add(1);
        let tick = self.tick;
        let visible_end = offset.saturating_add(viewport_height).min(self.total_lines);
        let mut output = VisibleTranscript {
            lines: Vec::with_capacity(viewport_height),
            link_groups: Vec::new(),
        };
        let mut protected = Vec::new();
        let notice_end = self.notice_render.lines.len();
        if notice_end > offset {
            let local_start = offset;
            let local_end = visible_end.min(notice_end).max(local_start);
            append_visible_slice(&self.notice_render, local_start, local_end, &mut output);
        }
        let mut start = notice_end;

        for index in 0..self.entries.len() {
            let effective = self.effective_entry_lines(&self.entries[index]);
            let end = start.saturating_add(effective);
            if end > offset && start < visible_end {
                protected.push(index);
                if self.entries[index].cached_render.is_none() {
                    let source_index = self.entries[index].source_index;
                    let media_paths = self
                        .artifact_provenance
                        .media_paths_for(&items[source_index])
                        .to_vec();
                    let display_mode = self.entries[index].revision.display_mode;
                    let raw = self.entries[index].revision.raw;
                    let selected = self.entries[index].revision.selected;
                    let show_nested_agent_prompt = self.entries[index].revision.nested_show_prompt;
                    let direct_hook_transcript_counts =
                        self.entries[index].revision.direct_hook_transcript_counts;
                    let rendered = render_projected_item_with_state_context(
                        &items[source_index],
                        self.width,
                        theme,
                        &media_paths,
                        display_mode,
                        selected,
                        &mut self.entries[index].markdown_stream,
                        ProjectedItemRenderContext {
                            show_nested_agent_prompt,
                            direct_hook_transcript_counts,
                            raw,
                            syntax_theme: self
                                .syntax_theme
                                .expect("synchronized transcript syntax theme"),
                            ui_language: self.ui_language,
                        },
                    );
                    #[cfg(test)]
                    {
                        self.render_passes = self.render_passes.saturating_add(1);
                    }
                    self.entries[index].cache_charge = rendered_part_charge(&rendered);
                    self.entries[index].cached_render = Some(rendered);
                }
                self.entries[index].last_used = tick;
                let local_start = offset.saturating_sub(start);
                let local_end = visible_end
                    .saturating_sub(start)
                    .min(effective)
                    .max(local_start);
                if let Some(rendered) = self.entries[index].cached_render.as_ref() {
                    append_visible_slice(rendered, local_start, local_end, &mut output);
                }
            }
            start = end;
            if start >= visible_end {
                break;
            }
            // All non-final items retain their inter-item spacer.
            if index + 1 < self.entries.len() {
                start =
                    start.saturating_add(self.entries[index].line_count.saturating_sub(effective));
            }
        }
        self.recalculate_cache_bytes();
        self.trim_cache(&protected);
        output
    }

    fn anchor_at(&self, offset: usize) -> Option<LayoutAnchor> {
        let notice_lines = self.notice_render.lines.len();
        if offset < notice_lines {
            return Some(LayoutAnchor::Notices {
                row: offset,
                line_count: notice_lines,
                width: self.width,
            });
        }
        let mut start = notice_lines;
        for entry in &self.entries {
            let effective = self.effective_entry_lines(entry);
            let end = start.saturating_add(effective);
            if offset < end {
                return Some(LayoutAnchor::Item {
                    key: entry.key.clone(),
                    row: offset.saturating_sub(start),
                    item_line_count: effective,
                    width: self.width,
                });
            }
            start = start.saturating_add(entry.line_count);
        }
        None
    }

    fn entry_start_and_value(&self, key: &str) -> Option<(usize, &TranscriptLayoutEntry)> {
        let mut start = self.notice_render.lines.len();
        for entry in &self.entries {
            if entry.key == key {
                return Some((start, entry));
            }
            start = start.saturating_add(entry.line_count);
        }
        None
    }

    pub(crate) fn item_bounds(&self, key: &str) -> Option<(usize, usize)> {
        self.entry_start_and_value(key)
            .map(|(start, entry)| (start, self.effective_entry_lines(entry)))
    }

    /// Center the rendered row containing the current fixed-search match.
    ///
    /// The selected entry is protected from the bounded render-cache eviction
    /// above, so production normally resolves the exact regex occurrence on
    /// its painted rows. The logical source line is the fixed upstream
    /// fallback for an item whose render is unavailable.
    pub(crate) fn reveal_search_match(
        &self,
        matched: &crate::transcript_search::TranscriptMatch,
        regex: Option<&regex::Regex>,
        scroll: &mut crate::tui_render::ScrollState,
        viewport_height: usize,
    ) {
        let Some((start, entry)) = self.entry_start_and_value(&matched.key) else {
            return;
        };
        let height = self.effective_entry_lines(entry);
        if height == 0 {
            return;
        }
        let rendered_row = regex
            .zip(entry.cached_render.as_ref())
            .and_then(|(regex, rendered)| {
                let mut ordinal = 0usize;
                for (row, line) in rendered.lines.iter().enumerate().take(height) {
                    let text = line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    for found in regex.find_iter(&text) {
                        if found.start() == found.end() {
                            continue;
                        }
                        if ordinal == matched.ordinal_in_item {
                            return Some(row);
                        }
                        ordinal = ordinal.saturating_add(1);
                    }
                }
                None
            })
            .unwrap_or(matched.line_in_item)
            .min(height.saturating_sub(1));
        let target = start.saturating_add(rendered_row);
        let centered = target.saturating_sub(viewport_height / 2);
        scroll.set_offset(centered, self.total_lines, viewport_height);
    }

    fn effective_entry_lines(&self, entry: &TranscriptLayoutEntry) -> usize {
        if self
            .entries
            .last()
            .is_some_and(|last| std::ptr::eq(last, entry))
            && entry.ends_with_blank
        {
            entry.line_count.saturating_sub(1)
        } else {
            entry.line_count
        }
    }

    fn recalculate_totals(&mut self) {
        self.total_lines = self
            .entries
            .iter()
            .fold(self.notice_render.lines.len(), |total, entry| {
                total.saturating_add(entry.line_count)
            });
        if self
            .entries
            .last()
            .is_some_and(|entry| entry.ends_with_blank)
        {
            self.total_lines = self.total_lines.saturating_sub(1);
        }
        self.recalculate_cache_bytes();
    }

    fn recalculate_cache_bytes(&mut self) {
        self.cache_bytes = self
            .entries
            .iter()
            .filter(|entry| entry.cached_render.is_some())
            .fold(rendered_part_charge(&self.notice_render), |total, entry| {
                total.saturating_add(entry.cache_charge)
            });
    }

    fn trim_cache(&mut self, protected: &[usize]) {
        while self.cache_bytes > MAX_TRANSCRIPT_LAYOUT_CACHE_BYTES {
            let Some(index) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(index, entry)| {
                    entry.cached_render.is_some() && !protected.contains(index)
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
            else {
                break;
            };
            self.entries[index].cached_render = None;
            self.cache_bytes = self
                .cache_bytes
                .saturating_sub(self.entries[index].cache_charge);
        }
    }

    #[cfg(test)]
    fn render_passes(&self) -> usize {
        self.render_passes
    }
}

#[cfg(test)]
fn scaled_anchor_row(
    row: usize,
    old_line_count: usize,
    new_line_count: usize,
    old_width: usize,
    new_width: usize,
) -> usize {
    if old_width == new_width || old_line_count <= 1 || new_line_count <= 1 {
        row.min(new_line_count.saturating_sub(1))
    } else {
        row.saturating_mul(new_line_count.saturating_sub(1))
            .saturating_add(old_line_count.saturating_sub(2) / 2)
            / old_line_count.saturating_sub(1)
    }
}

#[cfg(test)]
fn append_visible_slice(
    rendered: &RenderedTranscriptPart,
    local_start: usize,
    local_end: usize,
    output: &mut VisibleTranscript,
) {
    let output_row = output.lines.len();
    output
        .lines
        .extend(rendered.lines[local_start..local_end].iter().cloned());
    for group in &rendered.link_groups {
        let fragments = group
            .fragments
            .iter()
            .filter(|fragment| fragment.row >= local_start && fragment.row < local_end)
            .map(|fragment| VisibleLink {
                target: fragment.target.clone(),
                row: output_row.saturating_add(fragment.row.saturating_sub(local_start)),
                start_column: fragment.start_column,
                end_column: fragment.end_column,
            })
            .collect::<Vec<_>>();
        if !fragments.is_empty() {
            output.link_groups.push(VisibleLinkGroup {
                target: group.target.clone(),
                fragments,
            });
        }
    }
}

#[cfg(test)]
fn rendered_part_charge(rendered: &RenderedTranscriptPart) -> usize {
    let line_charge = rendered.lines.iter().fold(0_usize, |total, line| {
        line.spans
            .iter()
            .fold(total.saturating_add(64), |total, span| {
                total.saturating_add(64).saturating_add(span.content.len())
            })
    });
    rendered
        .link_groups
        .iter()
        .fold(line_charge, |total, group| {
            let retained_target_bytes = match &group.target {
                LinkTarget::Mermaid { source, .. } => source.len(),
                LinkTarget::Url(_) | LinkTarget::File(_) => 0,
            };
            total
                .saturating_add(64)
                .saturating_add(group.target.display_text().len())
                .saturating_add(retained_target_bytes)
                .saturating_add(group.fragments.len().saturating_mul(64))
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderOutcome {
    pub(crate) cursor: Option<(u16, u16)>,
    pub(crate) hyperlinks: Vec<LinkSpan>,
}

/// Output of the sole transcript paint callback used by [`render_with_transcript`].
///
/// The outer CrabCode shell owns product chrome (header, composer, panels and
/// modals), while `AppView -> AgentView` owns every transcript cell and every
/// transcript hyperlink. Keeping that boundary explicit prevents a second
/// transcript renderer from being called accidentally during the owner
/// cutover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptRenderOutcome {
    pub(crate) cursor: Option<(u16, u16)>,
    pub(crate) hyperlinks: Vec<LinkSpan>,
    pub(crate) selected_artifact_preview:
        Option<Arc<crate::crabcode_image_overlay::CrabCodeImagePreview>>,
}

/// Centered interaction surfaces replace normal minimal-mode interaction and
/// must hold native-scrollback commits. `insert_before` would otherwise scroll
/// the popup between two frames and permanently print content behind it.
pub(crate) fn minimal_centered_surface_active(app: &TuiApp) -> bool {
    app.setup_surface_exclusive()
        || app.session_picker_active()
        || app.history_search.is_some()
        || app.workspace_search.is_some()
        || app.model_picker.is_some()
        || app.model_management.is_some()
        || app.usage_plugin_management.is_some()
        || app.mcp_settings.is_some()
        || app.overlay.is_some()
        || app.dialog.is_some()
        || app.fatal.is_some()
}

/// Compute minimal mode's viewport from the exact row count measured by the
/// persistent transcript owner and the current product chrome.
pub(crate) fn minimal_viewport_height_for_transcript_rows(
    app: &TuiApp,
    terminal_width: u16,
    terminal_height: u16,
    transcript_rows: u16,
) -> u16 {
    let ceiling = terminal_height.max(1);
    let floor = MINIMAL_LAYOUT_FLOOR_ROWS.min(ceiling);
    let normal = 2_u16 // non-compact header
        .saturating_add(transcript_rows.max(4))
        .saturating_add(work_panel_height(&work_panel_state(app), terminal_width))
        .saturating_add(completion_overlay_rows(app))
        .saturating_add(goal_console_height(app, terminal_width))
        .saturating_add(oauth_browser_banner_height(app, terminal_width))
        .saturating_add(composer_height(app, terminal_width))
        .saturating_add(1) // footer
        .saturating_add(goal_status_height(app))
        .max(floor)
        .min(ceiling);

    let centered = minimal_centered_panel_rows(app, terminal_width, ceiling);
    normal.max(centered).min(ceiling)
}

/// Historical measurement retained only for legacy renderer regression tests.
#[cfg(test)]
pub(crate) fn minimal_viewport_height(
    app: &mut TuiApp,
    terminal_width: u16,
    terminal_height: u16,
    committed: &HashSet<String>,
) -> u16 {
    let ceiling = terminal_height.max(1);
    let floor = MINIMAL_LAYOUT_FLOOR_ROWS.min(ceiling);
    // The transcript paints through a one-column horizontal margin on both
    // sides. Reserve the optional scrollbar gutter while measuring so a
    // reflow after the viewport is capped cannot make the target too short.
    let transcript_width = usize::from(terminal_width.saturating_sub(3).max(1));

    app.synchronize_transcript_interactions();
    let notices = transcript_notice_records(app);
    let presentation_verbose = app.presentation_verbose();
    let direct_hook_transcript_mode = !app.composer_focused();
    let theme = app.renderer_theme();
    let syntax_theme = app_syntax_theme(app);
    let ui_language = app.ui_language();
    app.transcript_layout.set_ui_language(ui_language);
    app.transcript_layout.synchronize(
        app.projection.items(),
        app.projection.raw_envelopes(),
        &app.projection,
        app.projection.raw_envelope_count(),
        committed,
        &app.transcript_item_interactions,
        app.selected_transcript_key.as_deref(),
        presentation_verbose,
        direct_hook_transcript_mode,
        app.transcript_presentation_generation,
        &notices,
        theme,
        syntax_theme,
        transcript_width,
        &mut app.scroll,
        usize::from(ceiling),
    );

    // Empty transcripts deliberately render two instructional rows. The
    // transcript block also owns top and bottom borders.
    let transcript_rows = u16::try_from(app.transcript_layout.total_lines().max(2))
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .max(4);
    let normal = 2_u16 // non-compact header
        .saturating_add(transcript_rows)
        .saturating_add(work_panel_height(&work_panel_state(app), terminal_width))
        .saturating_add(completion_overlay_rows(app))
        .saturating_add(goal_console_height(app, terminal_width))
        .saturating_add(composer_height(app, terminal_width))
        .saturating_add(1) // footer
        .saturating_add(goal_status_height(app))
        .max(floor)
        .min(ceiling);

    let centered = minimal_centered_panel_rows(app, terminal_width, ceiling);
    normal.max(centered).min(ceiling)
}

fn minimal_centered_panel_rows(app: &TuiApp, terminal_width: u16, ceiling: u16) -> u16 {
    let mut target = 0;
    if let Some(overlay) = app
        .overlay
        .as_ref()
        .filter(|overlay| overlay.kind != OverlayKind::GoalConsole)
    {
        // Viewport geometry is content-owned, not offset-owned. Counting only
        // rows after `scroll` makes the modal shrink while navigating and
        // feeds that smaller height back into the next max-offset clamp.
        let content_rows =
            u16::try_from(overlay.body.split('\n').count().max(1)).unwrap_or(u16::MAX);
        target = target.max(centered_host_rows(
            content_rows.saturating_add(2),
            82,
            ceiling,
        ));
    }
    if let Some(picker) = app.model_picker.as_ref() {
        let visible_rows = picker
            .visible_to()
            .saturating_sub(picker.visible_from())
            .max(1);
        let content_rows = u16::try_from(visible_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(5);
        target = target.max(centered_host_rows(content_rows, 82, ceiling));
    }
    if let Some(management) = app.model_management.as_ref() {
        let content_rows = u16::try_from(
            management
                .rows(app.ui_language())
                .len()
                .min(18)
                .saturating_add(management.details(app.ui_language()).len().min(5))
                .saturating_add(6),
        )
        .unwrap_or(u16::MAX);
        target = target.max(centered_host_rows(content_rows, 92, ceiling));
    }
    if let Some(management) = app.usage_plugin_management.as_ref() {
        let detail_limit = management.detail_line_limit();
        let content_rows = u16::try_from(
            management
                .rows(app.ui_language())
                .len()
                .min(18)
                .saturating_add(
                    management
                        .detail_lines(
                            app.ui_language(),
                            usage_management_content_width(terminal_width),
                        )
                        .len()
                        .min(detail_limit),
                )
                .saturating_add(usize::from(!management.tabs(app.ui_language()).is_empty()))
                .saturating_add(7),
        )
        .unwrap_or(u16::MAX);
        target = target.max(centered_host_rows(content_rows, 96, ceiling));
    }
    if let Some(settings) = app.mcp_settings.as_ref() {
        let visible_rows = match settings.view() {
            McpSettingsView::List => settings
                .servers()
                .len()
                .min(crate::tui_app::MCP_SETTINGS_VISIBLE_OPTIONS),
            McpSettingsView::Server { server_index } => {
                settings.servers().get(server_index).map_or(1, |server| {
                    settings.server_menu_options(server).len().saturating_add(8)
                })
            }
            McpSettingsView::Tools { server_index } => {
                settings.servers().get(server_index).map_or(1, |server| {
                    server
                        .tools
                        .len()
                        .min(crate::tui_app::MCP_SETTINGS_VISIBLE_OPTIONS)
                })
            }
            McpSettingsView::ToolDetail { .. } => crate::tui_app::MCP_SETTINGS_VISIBLE_OPTIONS,
        }
        .max(1);
        let content_rows = u16::try_from(visible_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(4);
        target = target.max(centered_host_rows(content_rows, 82, ceiling));
    }
    if let Some(search) = app.workspace_search.as_ref() {
        let preview_on_right = terminal_width
            >= match search.kind() {
                WorkspaceSearchKind::QuickOpen => 120,
                WorkspaceSearchKind::GlobalSearch => 140,
            };
        let content_rows = if preview_on_right {
            u16::try_from(search.kind().visible_results())
                .unwrap_or(u16::MAX)
                .saturating_add(4)
        } else {
            match search.kind() {
                WorkspaceSearchKind::QuickOpen => 32,
                WorkspaceSearchKind::GlobalSearch => 25,
            }
        };
        target = target.max(centered_host_rows(content_rows, 94, ceiling));
    }
    if let Some(dialog) = app.dialog.as_ref() {
        let modal_width = percent_width(terminal_width, 90).saturating_sub(2).max(1);
        target = target.max(centered_host_rows(
            minimal_dialog_rows(dialog, modal_width),
            82,
            ceiling,
        ));
    }
    if let Some(fatal) = app.fatal.as_ref() {
        let modal_width = percent_width(terminal_width, 84).saturating_sub(2).max(1);
        let latest_stderr = app.stderr_notices().next_back();
        let mut panel_rows = 1_u16 // fail-closed heading
            .saturating_add(1) // gap
            .saturating_add(wrapped_text_rows(fatal, modal_width))
            .saturating_add(1) // gap
            .saturating_add(1) // retained-diagnostics / exit hint
            .saturating_add(2); // modal border
        if let Some(stderr) = latest_stderr {
            panel_rows = panel_rows
                .saturating_add(wrapped_text_rows(
                    &format!("Latest runtime stderr: {stderr}"),
                    modal_width,
                ))
                .saturating_add(1); // gap before the generic stop reason
        }
        target = target.max(centered_host_rows(
            panel_rows,
            if latest_stderr.is_some() { 60 } else { 45 },
            ceiling,
        ));
    }
    target
}

fn centered_host_rows(panel_rows: u16, vertical_percent: u16, ceiling: u16) -> u16 {
    // `centered` allocates the popup as a percentage of the live viewport.
    // Invert that allocation and add one rounding row so the popup itself has
    // at least `panel_rows` after Ratatui's percentage rounding.
    panel_rows
        .saturating_mul(100)
        .div_ceil(vertical_percent.max(1))
        .saturating_add(1)
        .max(MINIMAL_LAYOUT_FLOOR_ROWS.min(ceiling))
        .min(ceiling)
}

fn minimal_dialog_rows(dialog: &RequestDialog, content_width: u16) -> u16 {
    let rows = match dialog {
        RequestDialog::Permission {
            tool_name,
            display_name,
            description,
            input,
            choices,
            ..
        } => {
            let title = display_name.as_deref().unwrap_or(tool_name);
            let description = description
                .as_deref()
                .unwrap_or("This tool requires approval.");
            let input_body =
                plan_approval_body(tool_name, input).unwrap_or_else(|| bounded_json(input));
            wrapped_text_rows(title, content_width)
                .saturating_add(wrapped_text_rows(description, content_width))
                .saturating_add(2) // blank rows around the input body
                .saturating_add(wrapped_text_rows(&input_body, content_width))
                .saturating_add(u16::from(!choices.is_empty()))
        }
        RequestDialog::Question(dialog) => {
            let question = dialog.current_question();
            wrapped_text_rows(&question.header, content_width)
                .saturating_add(wrapped_text_rows(&question.question, content_width))
                .saturating_add(2)
                .saturating_add(
                    u16::try_from(question.options.len().saturating_add(1)).unwrap_or(u16::MAX),
                )
                .saturating_add(3)
                .saturating_add(u16::from(dialog.current_answer().other_selected) * 4)
                .saturating_add(u16::from(dialog.validation_error.is_some()))
        }
        RequestDialog::Elicitation {
            server_name,
            message,
            mode,
            url,
            schema,
            validation_error,
            ..
        } => {
            let mut description_rows = wrapped_text_rows(server_name, content_width)
                .saturating_add(wrapped_text_rows(message, content_width));
            if let Some(url) = url {
                description_rows =
                    description_rows.saturating_add(wrapped_text_rows(url, content_width));
            }
            if let Some(schema) = schema {
                description_rows = description_rows
                    .saturating_add(wrapped_text_rows(&compact_json(schema), content_width));
            }
            if let Some(error) = validation_error {
                description_rows =
                    description_rows.saturating_add(wrapped_text_rows(error, content_width));
            }
            description_rows
                .max(2)
                .saturating_add(if mode == "url" { 0 } else { 5 })
                .saturating_add(2) // action row allocation
                .saturating_add(2) // inner vertical margin
        }
        RequestDialog::GroveTerms {
            title,
            body,
            links,
            options,
            ..
        } => {
            let mut content_rows = wrapped_text_rows(title, content_width).saturating_add(1);
            for line in body {
                content_rows = content_rows.saturating_add(wrapped_text_rows(line, content_width));
            }
            if !links.is_empty() {
                content_rows = content_rows.saturating_add(2);
                for link in links {
                    content_rows = content_rows.saturating_add(wrapped_text_rows(
                        &format!("{} · {}", link.label, link.url),
                        content_width,
                    ));
                }
            }
            content_rows
                .saturating_add(1)
                .saturating_add(u16::from(!options.is_empty()))
        }
        RequestDialog::SetupInput(dialog) => {
            let mut content_rows =
                wrapped_text_rows(&dialog.title, content_width).saturating_add(1);
            for line in &dialog.body {
                content_rows = content_rows.saturating_add(wrapped_text_rows(line, content_width));
            }
            if let Some(error) = &dialog.validation_error {
                content_rows = content_rows.saturating_add(wrapped_text_rows(error, content_width));
            }
            content_rows
                .saturating_add(3) // bordered single-line editor
                .saturating_add(2) // inner vertical margin
        }
        RequestDialog::Setup(dialog) => {
            let mut content_rows =
                wrapped_text_rows(&dialog.title, content_width).saturating_add(1);
            for line in &dialog.body {
                content_rows = content_rows.saturating_add(wrapped_text_rows(line, content_width));
            }
            if matches!(
                dialog.kind,
                crate::tui_app::SetupDialogKind::OnboardingTheme { .. }
            ) {
                content_rows = content_rows.saturating_add(3);
            }
            if !dialog.links.is_empty() {
                content_rows = content_rows.saturating_add(1);
                for link in &dialog.links {
                    content_rows = content_rows.saturating_add(wrapped_text_rows(
                        &format!("{} · {}", link.label, link.url),
                        content_width,
                    ));
                }
            }
            content_rows
                .saturating_add(1)
                .saturating_add(u16::try_from(dialog.choices.len()).unwrap_or(u16::MAX))
        }
    };
    rows.saturating_add(2) // dialog border
}

fn wrapped_text_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    text.split('\n').fold(0_u16, |rows, line| {
        let line_rows = line.width().max(1).div_ceil(width);
        rows.saturating_add(u16::try_from(line_rows).unwrap_or(u16::MAX))
    })
}

fn percent_width(width: u16, percent: u16) -> u16 {
    width.saturating_mul(percent).saturating_div(100).max(1)
}

#[derive(Debug, Default)]
struct OAuthBrowserBanner {
    lines: Vec<Line<'static>>,
    url: Option<Arc<str>>,
    url_rows: Vec<(usize, u16)>,
}

fn oauth_browser_banner_content_width(width: u16) -> u16 {
    if width > 2 { width - 2 } else { width }
}

/// Parse the device-flow code carried by the existing authoritative setup URL.
///
/// This is the fixed renderer behavior from the Rust TUI lifecycle.  It does
/// not create a setup field or infer a code from unrelated `auth_status`
/// output: a code is shown only when the already-validated URL contains one
/// exact `user_code` query member with the fixed safe character grammar.
fn oauth_device_user_code(url: &str) -> Option<&str> {
    let code = url
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|member| member.strip_prefix("user_code="))?;
    (!code.is_empty()
        && code
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'))
    .then_some(code)
}

/// Project the historical direct-TUI OAuth presenter into product chrome.
///
/// The fixed upstream renderer keeps banners outside scrollback, and CrabCode's
/// historical Ink flow keeps this prompt non-modal.  Consequently this view
/// reserves its own rows and never re-enters the retired transcript painter.
fn oauth_browser_banner(
    app: &TuiApp,
    terminal_width: u16,
    theme: CrabCodeTheme,
) -> OAuthBrowserBanner {
    oauth_browser_banner_for_notice(
        app.oauth_browser_notice(),
        terminal_width,
        theme,
        app.ui_language(),
    )
}

fn oauth_browser_banner_for_notice(
    notice: Option<OAuthBrowserNotice<'_>>,
    terminal_width: u16,
    theme: CrabCodeTheme,
    language: UiLanguage,
) -> OAuthBrowserBanner {
    let width = usize::from(oauth_browser_banner_content_width(terminal_width).max(1));
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    match notice {
        None => OAuthBrowserBanner::default(),
        Some(OAuthBrowserNotice::Opening { spinner, message }) => {
            let message = sanitize_bounded_terminal_text(message).into_owned();
            OAuthBrowserBanner {
                lines: wrap_line(
                    &Line::from(vec![
                        Span::styled(
                            spinner,
                            Style::default()
                                .fg(theme.accent_running)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(message, dim),
                    ]),
                    width,
                ),
                ..OAuthBrowserBanner::default()
            }
        }
        Some(OAuthBrowserNotice::Manual {
            message,
            hint,
            copied,
            url,
        }) => {
            let message = sanitize_bounded_terminal_text(message).into_owned();
            let hint = sanitize_bounded_terminal_text(hint).into_owned();
            let safe_url =
                safe_standard_url_target(url).map(|url| Arc::<str>::from(url.into_owned()));
            let visible_url = sanitize_bounded_terminal_text(url).into_owned();
            let mut lines = wrap_line(
                &Line::from(vec![
                    Span::styled(message, dim),
                    Span::raw(" "),
                    Span::styled(
                        hint,
                        if copied {
                            Style::default().fg(theme.accent_success)
                        } else {
                            dim
                        },
                    ),
                ]),
                width,
            );
            let url_start = lines.len();
            let wrapped_url = wrap_line(&Line::styled(visible_url, dim), width);
            let url_rows = wrapped_url
                .iter()
                .enumerate()
                .filter_map(|(row, line)| {
                    let columns = line
                        .spans
                        .iter()
                        .map(|span| span.content.width())
                        .sum::<usize>()
                        .min(width);
                    (columns > 0).then(|| {
                        (
                            url_start.saturating_add(row),
                            u16::try_from(columns).unwrap_or(u16::MAX),
                        )
                    })
                })
                .collect();
            lines.extend(wrapped_url);
            if let Some(code) = oauth_device_user_code(url) {
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::styled(language.text("验证码：", "Code: "), dim),
                    Span::styled(
                        code.to_string(),
                        Style::default()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            OAuthBrowserBanner {
                lines,
                url: safe_url,
                url_rows,
            }
        }
    }
}

fn oauth_browser_banner_height(app: &TuiApp, terminal_width: u16) -> u16 {
    u16::try_from(
        oauth_browser_banner(app, terminal_width, app.renderer_theme())
            .lines
            .len(),
    )
    .unwrap_or(u16::MAX)
}

fn render_oauth_browser_banner(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    theme: CrabCodeTheme,
) -> Vec<LinkSpan> {
    if area.is_empty() {
        return Vec::new();
    }
    let horizontal_margin = u16::from(area.width > 2);
    let content = Rect::new(
        area.x.saturating_add(horizontal_margin),
        area.y,
        area.width
            .saturating_sub(horizontal_margin.saturating_mul(2)),
        area.height,
    );
    if content.is_empty() {
        return Vec::new();
    }
    let banner = oauth_browser_banner(app, area.width, theme);
    frame.render_widget(
        Paragraph::new(banner.lines).style(Style::default().bg(theme.bg_base)),
        content,
    );
    let Some(url) = banner.url else {
        return Vec::new();
    };
    banner
        .url_rows
        .into_iter()
        .filter_map(|(row, width)| {
            let row = u16::try_from(row).ok()?;
            (row < content.height && width > 0).then(|| LinkSpan {
                row: content.y.saturating_add(row),
                col_start: content.x,
                col_end: content.x.saturating_add(width.min(content.width)),
                url: Arc::clone(&url),
                id: None,
            })
        })
        .collect()
}

pub(crate) fn render_with_transcript<E>(
    frame: &mut Frame<'_>,
    app: &mut TuiApp,
    render_transcript_owner: impl FnOnce(
        &mut Frame<'_>,
        Rect,
        &mut TuiApp,
        CrabCodeTheme,
    ) -> Result<TranscriptRenderOutcome, E>,
) -> Result<RenderOutcome, E> {
    app.synchronize_modal_tree();
    let theme = app.renderer_theme();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_base)),
        area,
    );
    if app.setup_surface_exclusive() {
        app.composer_area = Rect::default();
        app.composer_attachment_hitboxes.clear();
        app.status_hit_areas.clear();
        app.mermaid_hitboxes.clear();
        crate::terminal_writer::stage_image_post_flush(
            crate::crabcode_image_overlay::clear_committed_image(),
        );
        let mut cursor = None;
        let mut hyperlinks = Vec::new();
        if app.fatal.is_some() {
            render_fatal(frame, area, app, theme);
        } else if app.dialog.is_some() {
            cursor = render_dialog(frame, area, app, theme);
        } else {
            hyperlinks = render_setup_lifecycle(frame, area, app, theme);
        }
        return Ok(RenderOutcome { cursor, hyperlinks });
    }
    let compact = area.height < 10;
    let header_height = if compact { 1 } else { 2 };
    let transcript_minimum = if compact { 3 } else { 4 };
    let requested_oauth_height = oauth_browser_banner_height(app, area.width);
    let oauth_height = requested_oauth_height.min(
        area.height
            .saturating_sub(
                header_height
                    + transcript_minimum
                    + 3 // irreducible composer
                    + 1 // footer
                    + goal_status_height(app),
            )
            .max(u16::from(requested_oauth_height > 0)),
    );
    let composer_height = composer_height(app, area.width).min(
        area.height
            .saturating_sub(header_height + transcript_minimum + oauth_height + 1)
            .max(3),
    );
    let work_panel = work_panel_state(app);
    let todo_height =
        work_panel_height(&work_panel, area.width).min(area.height.saturating_sub(
            header_height + transcript_minimum + oauth_height + composer_height + 1,
        ));
    let completion_height = completion_overlay_rows(app).min(area.height.saturating_sub(
        header_height + transcript_minimum + todo_height + oauth_height + composer_height + 1,
    ));
    let goal_status_height = goal_status_height(app);
    let goal_console_height = goal_console_height(app, area.width).min(area.height.saturating_sub(
        header_height
            + transcript_minimum
            + todo_height
            + completion_height
            + oauth_height
            + composer_height
            + 1
            + goal_status_height,
    ));
    let [
        header,
        transcript,
        todos,
        completion,
        goal_console,
        oauth_banner,
        composer,
        footer,
        goal_status,
    ] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(transcript_minimum),
        Constraint::Length(todo_height),
        Constraint::Length(completion_height),
        Constraint::Length(goal_console_height),
        Constraint::Length(oauth_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
        Constraint::Length(goal_status_height),
    ])
    .areas(area);
    render_header(frame, header, app, theme);
    let TranscriptRenderOutcome {
        mut hyperlinks,
        cursor: transcript_cursor,
        selected_artifact_preview,
    } = render_transcript_owner(frame, transcript, app, theme)?;
    hyperlinks.extend(render_oauth_browser_banner(frame, oauth_banner, app, theme));
    render_work_panel(frame, todos, &work_panel, app.ui_language(), theme);
    render_completion_overlay(frame, completion, app, theme);
    render_goal_console(frame, goal_console, app, theme);
    let mut cursor = render_composer(frame, composer, app, theme);
    if transcript_cursor.is_some() {
        cursor = transcript_cursor;
    }
    render_footer(frame, footer, app, theme);
    render_goal_status(frame, goal_status, app, theme);

    let modal_active = minimal_centered_surface_active(app);
    let transcript_search_active = app.transcript_search.is_some();
    let composer_preview = app
        .composer_image_preview
        .and_then(|index| app.attachments.get(index).map(|image| (index, image)))
        .map(|(index, image)| {
            crate::crabcode_image_overlay::CrabCodeImagePreview::without_path(
                image.preview_identity,
                index + 1,
                image.mime.clone(),
                Some((image.width, image.height)),
                image.encoded_bytes,
                image.terminal_preview.clone(),
            )
        });
    let artifact_preview =
        if !modal_active && !transcript_search_active && composer_preview.is_none() {
            selected_artifact_preview
        } else {
            None
        };
    let preview = composer_preview.as_ref().or(artifact_preview.as_deref());
    let image_escapes = if !modal_active && !transcript_search_active {
        match preview.and_then(|preview| {
            crate::crabcode_image_overlay::render_image_overlay(
                frame.buffer_mut(),
                transcript,
                preview,
                theme.bg_dark,
                theme.text_primary,
                theme.prompt_border_active,
                app.ui_language(),
            )
        }) {
            Some(rendered) => {
                hyperlinks.clear();
                if rendered.pixels_active {
                    rendered.escapes
                } else {
                    crate::crabcode_image_overlay::clear_committed_image()
                }
            }
            None => crate::crabcode_image_overlay::clear_committed_image(),
        }
    } else {
        crate::crabcode_image_overlay::clear_committed_image()
    };
    crate::terminal_writer::stage_image_post_flush(image_escapes);

    match app.active_modal() {
        Some(ActiveModal::HistorySearch) => {
            cursor = render_history_search(frame, area, app, theme);
            hyperlinks.clear();
        }
        Some(ActiveModal::WorkspaceSearch) => {
            cursor = render_workspace_search(frame, area, app, theme);
            hyperlinks.clear();
        }
        Some(ActiveModal::ModelPicker) => {
            render_model_picker(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        Some(ActiveModal::ModelManagement) => {
            render_model_management(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        Some(ActiveModal::UsagePluginManagement) => {
            render_usage_plugin_management(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        Some(ActiveModal::McpSettings) => {
            render_mcp_settings(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        Some(ActiveModal::Overlay) => {
            render_overlay(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        Some(ActiveModal::Request) => {
            cursor = render_dialog(frame, area, app, theme);
            hyperlinks.clear();
        }
        Some(ActiveModal::Fatal) => {
            render_fatal(frame, area, app, theme);
            cursor = None;
            hyperlinks.clear();
        }
        None => {}
    }
    Ok(RenderOutcome { cursor, hyperlinks })
}

fn render_setup_lifecycle(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    theme: CrabCodeTheme,
) -> Vec<LinkSpan> {
    let language = app.ui_language();
    let panel = centered(area, 84, 58);
    let block = setup_block_with_quit_confirmation(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.warning))
            .style(Style::default().bg(theme.bg_dark))
            .title(language.text(" CrabCode 设置 ", " CrabCode setup ")),
        app.setup_ctrl_c_confirmation_active(),
        language,
        theme,
    );
    let inner = block.inner(panel).inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    frame.render_widget(block, panel);
    if app.oauth_browser_notice().is_some() {
        return render_oauth_browser_banner(frame, inner, app, theme);
    }
    if let Some(notice) = app.passive_setup_notice() {
        let mut lines = vec![
            Line::styled(
                notice.title.clone(),
                Style::default()
                    .fg(theme.accent_error)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::default(),
        ];
        lines.extend(notice.body.iter().map(|line| {
            let is_error = line.starts_with("错误：") || line.starts_with("Error:");
            Line::styled(
                line.clone(),
                Style::default().fg(if is_error {
                    theme.accent_error
                } else {
                    theme.text_secondary
                }),
            )
        }));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return Vec::new();
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                language.text("正在准备直连运行环境…", "Preparing the direct runtime…"),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::default(),
            Line::styled(
                language.text(
                    "首次设置或初始化完成后才会进入主界面。",
                    "The main interface opens after setup and initialization complete.",
                ),
                Style::default().fg(theme.text_secondary),
            ),
            Line::default(),
            Line::styled(
                language.text("请稍候。", "Please wait."),
                Style::default().fg(theme.gray),
            ),
        ])
        .wrap(Wrap { trim: false }),
        inner,
    );
    Vec::new()
}

fn setup_block_with_quit_confirmation<'a>(
    block: Block<'a>,
    active: bool,
    language: UiLanguage,
    theme: CrabCodeTheme,
) -> Block<'a> {
    if active {
        block.title_bottom(
            Line::styled(
                language.text(" 再次按 Ctrl-C 即可退出 ", " Press Ctrl-C again to exit "),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )
            .right_aligned(),
        )
    } else {
        block
    }
}

/// Historical transcript entry point retained only for renderer regression
/// tests. Production dispatch is owned by `TerminalSession -> AppView`.
#[cfg(test)]
pub(crate) fn render(frame: &mut Frame<'_>, app: &mut TuiApp) -> RenderOutcome {
    render_with_transcript(frame, app, |frame, area, app, theme| {
        let (hyperlinks, cursor) = render_transcript(frame, area, app, theme);
        let selected_target = app
            .selected_visible_link()
            .map(|selected| selected.target.clone());
        let selected_artifact_preview = selected_target
            .as_ref()
            .and_then(|target| app.transcript_layout.artifact_image_preview(target));
        Ok::<_, std::convert::Infallible>(TranscriptRenderOutcome {
            cursor,
            hyperlinks,
            selected_artifact_preview,
        })
    })
    .expect("the legacy transcript callback is infallible")
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: CrabCodeTheme) {
    if area.is_empty() {
        return;
    }
    let language = app.ui_language();
    let width = usize::from(area.width);
    let state = header_state_label(app);
    let permission_mode = app.projection.permission_mode().unwrap_or("default");
    let permission = permission_mode_label(permission_mode, language, width < 72);
    let dangerous_permission = permission_mode == "bypassPermissions";
    let ribbon = Style::default().bg(theme.bg_dark);

    frame.render_widget(Block::default().style(ribbon), area);

    let mut title_spans = vec![Span::styled(
        " CrabCode ",
        Style::default()
            .fg(theme.bg_base)
            .bg(theme.accent_assistant)
            .add_modifier(Modifier::BOLD),
    )];
    if width >= 64 {
        title_spans.push(Span::styled(
            language.text("  原生 TUI", "  native TUI"),
            Style::default().fg(theme.text_primary).bg(theme.bg_dark),
        ));
    }
    title_spans.extend([
        Span::styled("  ", ribbon),
        Span::styled(
            "● ",
            Style::default()
                .fg(if app.busy() {
                    theme.accent_running
                } else {
                    theme.accent_success
                })
                .bg(theme.bg_dark),
        ),
        Span::styled(
            state,
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.bg_dark)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let permission_style = if dangerous_permission {
        Style::default()
            .fg(theme.bg_base)
            .bg(theme.accent_error)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.accent_system)
            .bg(theme.bg_highlight)
            .add_modifier(Modifier::BOLD)
    };
    let permission_spans = vec![
        Span::styled(
            format!(" {} ", language.text("审批", "approval")),
            Style::default().fg(theme.gray).bg(theme.bg_dark),
        ),
        Span::styled(format!(" {permission} "), permission_style),
        Span::styled(" ", ribbon),
    ];
    let title = header_row_with_priority(title_spans, permission_spans, width, 10, ribbon);

    let mut rows = vec![title];
    if area.height > 1 {
        let session = sanitize_bounded_terminal_text(
            app.projection
                .session_id()
                .map(short_id)
                .unwrap_or(language.text("新会话", "new")),
        );
        let model = sanitize_bounded_terminal_text(
            app.projection
                .model()
                .unwrap_or(language.text("模型待回报", "model pending")),
        );
        let metadata = if width >= 88 {
            vec![
                Span::styled(
                    format!(" {} {}", language.text("会话", "session"), session),
                    Style::default().fg(theme.gray).bg(theme.bg_dark),
                ),
                Span::styled(" │ ", Style::default().fg(theme.gray_dim).bg(theme.bg_dark)),
                Span::styled(
                    format!("{} {model}", language.text("模型", "model")),
                    Style::default().fg(theme.text_secondary).bg(theme.bg_dark),
                ),
            ]
        } else {
            vec![Span::styled(
                format!(" {} {model}", language.text("模型", "model")),
                Style::default().fg(theme.text_secondary).bg(theme.bg_dark),
            )]
        };
        let context = header_context_spans(app, theme);
        rows.push(header_row_with_priority(
            metadata, context, width, 0, ribbon,
        ));
    }
    frame.render_widget(Paragraph::new(rows).style(ribbon), area);
}

fn header_state_label(app: &TuiApp) -> &'static str {
    let language = app.ui_language();
    if app.busy() {
        return language.text("运行中", "running");
    }
    match app.projection.session_state() {
        Some("initializing") => language.text("初始化中", "initializing"),
        Some("requires_action") => language.text("等待确认", "action needed"),
        Some("running") => language.text("运行中", "running"),
        Some("ready" | "idle") | None => language.text("就绪", "ready"),
        Some(_) => language.text("状态未知", "unknown state"),
    }
}

fn permission_mode_label(mode: &str, language: UiLanguage, compact: bool) -> &'static str {
    if !compact {
        return localized_permission_mode_label(Some(mode), language);
    }
    match (language, mode, compact) {
        (UiLanguage::ZhCn, "default", true) => "标准",
        (UiLanguage::ZhCn, "acceptEdits", true) => "自动编辑",
        (UiLanguage::ZhCn, "bypassPermissions", true) => "常规免审",
        (UiLanguage::ZhCn, "plan", true) => "只规划",
        (UiLanguage::ZhCn, "dontAsk", true) => "已批准",
        (UiLanguage::ZhCn, "auto", true) => "自动",
        (UiLanguage::ZhCn, _, _) => "自定义",
        (UiLanguage::EnUs, "default", true) => "standard",
        (UiLanguage::EnUs, "acceptEdits", true) => "auto edits",
        (UiLanguage::EnUs, "bypassPermissions", true) => "routine auto",
        (UiLanguage::EnUs, "plan", true) => "plan only",
        (UiLanguage::EnUs, "dontAsk", true) => "approved",
        (UiLanguage::EnUs, "auto", true) => "auto",
        (UiLanguage::EnUs, _, _) => "custom",
    }
}

fn header_context_spans(app: &TuiApp, theme: CrabCodeTheme) -> Vec<Span<'static>> {
    let language = app.ui_language();
    let label = if app.context_usage_is_baseline() {
        language.text("基础上下文", "baseline context")
    } else {
        language.text("上下文", "context")
    };
    let (text, color) = if let Some(usage) = app.live_context_usage() {
        let color = if usage.percentage >= 80 {
            theme.accent_error
        } else if usage.percentage >= 60 {
            theme.warning
        } else {
            theme.accent_success
        };
        (
            format!(
                " {} {}/{} · {}% ",
                label,
                format_header_tokens(usage.total_tokens),
                format_header_tokens(usage.max_tokens),
                usage.percentage
            ),
            color,
        )
    } else {
        (
            format!(
                " {} {} ",
                label,
                if app.context_usage_refresh_pending() {
                    language.text("计算中…", "calculating…")
                } else {
                    language.text("待同步", "pending")
                }
            ),
            theme.gray,
        )
    };
    vec![
        Span::styled(text, Style::default().fg(color).bg(theme.bg_dark)),
        Span::styled(" ", Style::default().bg(theme.bg_dark)),
    ]
}

fn format_header_tokens(tokens: u64) -> String {
    let (value, suffix) = if tokens >= 1_000_000 {
        (tokens as f64 / 1_000_000.0, "m")
    } else if tokens >= 1_000 {
        (tokens as f64 / 1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    let mut formatted = format!("{value:.1}");
    if formatted.ends_with(".0") {
        formatted.truncate(formatted.len().saturating_sub(2));
    }
    format!("{formatted}{suffix}")
}

fn header_row_with_priority(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    minimum_left_width: usize,
    style: Style,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let right_width = right.iter().map(|span| span.content.width()).sum::<usize>();
    if right_width.saturating_add(minimum_left_width) > width {
        return fit_line_to_width(Line::from(left).style(style), width);
    }
    if right_width >= width {
        return fit_line_to_width(Line::from(right).style(style), width);
    }
    let mut line = fit_line_to_width(Line::from(left).style(style), width - right_width);
    line.spans.extend(right);
    line.style = style;
    line
}

#[cfg(test)]
fn render_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) -> (Vec<LinkSpan>, Option<(u16, u16)>) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(theme.prompt_border))
        .style(Style::default().bg(theme.bg_base));
    let inner = block.inner(area).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, area);

    app.synchronize_transcript_interactions();
    let committed = HashSet::new();
    let notices = transcript_notice_records(app);
    let presentation_verbose = app.presentation_verbose();
    let direct_hook_transcript_mode = !app.composer_focused();
    let syntax_theme = app_syntax_theme(app);
    let ui_language = app.ui_language();
    app.transcript_layout.set_ui_language(ui_language);
    let mut content_inner = inner;
    let search_reserved_rows = if app.transcript_search.is_some() {
        content_inner.height.min(2)
    } else {
        0
    };
    content_inner.height = content_inner.height.saturating_sub(search_reserved_rows);
    let viewport_height = usize::from(content_inner.height);
    let full_width = usize::from(content_inner.width.max(1));
    let previous_scrolling_layout = app.transcript_layout.width() == full_width.saturating_sub(1)
        && app.transcript_layout.total_lines() > viewport_height;
    let mut width = if previous_scrolling_layout && inner.width > 1 {
        content_inner.width = content_inner.width.saturating_sub(1);
        usize::from(content_inner.width.max(1))
    } else {
        full_width
    };
    app.transcript_layout.synchronize(
        app.projection.items(),
        app.projection.raw_envelopes(),
        &app.projection,
        app.projection.raw_envelope_count(),
        &committed,
        &app.transcript_item_interactions,
        app.selected_transcript_key.as_deref(),
        presentation_verbose,
        direct_hook_transcript_mode,
        app.transcript_presentation_generation,
        &notices,
        theme,
        syntax_theme,
        width,
        &mut app.scroll,
        viewport_height,
    );
    if app.transcript_layout.total_lines() > viewport_height
        && inner.width > 1
        && width == full_width
    {
        // Match the pinned upstream layout contract: one blank gutter column
        // plus one scrollbar column. The track already occupies the outer
        // right margin, so only the gutter must be removed from content.
        // Reflow once after reserving it so neither cell overwrites transcript.
        content_inner.width = content_inner.width.saturating_sub(1);
        width = usize::from(content_inner.width.max(1));
        app.transcript_layout.synchronize(
            app.projection.items(),
            app.projection.raw_envelopes(),
            &app.projection,
            app.projection.raw_envelope_count(),
            &committed,
            &app.transcript_item_interactions,
            app.selected_transcript_key.as_deref(),
            presentation_verbose,
            direct_hook_transcript_mode,
            app.transcript_presentation_generation,
            &notices,
            theme,
            syntax_theme,
            width,
            &mut app.scroll,
            viewport_height,
        );
    }
    let search_highlight = app
        .transcript_search
        .as_ref()
        .and_then(crate::transcript_search::TranscriptSearchState::highlight_regex);
    if let Some(matched) = app.take_transcript_search_reveal() {
        app.transcript_layout.reveal_search_match(
            &matched,
            search_highlight.as_ref(),
            &mut app.scroll,
            viewport_height,
        );
    }
    if let Some(direction) = app.take_transcript_selection_scroll()
        && let Some(key) = app.selected_transcript_key.as_deref()
        && let Some((start, height)) = app.transcript_layout.item_bounds(key)
    {
        let end = start.saturating_add(height);
        let offset = app.scroll.offset();
        let viewport_end = offset.saturating_add(viewport_height);
        let target = if height > viewport_height {
            Some(match direction {
                TranscriptSelectionDirection::Down => start,
                TranscriptSelectionDirection::Up => end.saturating_sub(viewport_height),
            })
        } else if start < offset {
            Some(start)
        } else if end > viewport_end {
            Some(end.saturating_sub(viewport_height))
        } else {
            None
        };
        if let Some(target) = target {
            app.scroll
                .set_offset(target, app.transcript_layout.total_lines(), viewport_height);
        }
    }
    let mut visible = app.transcript_layout.visible_lines(
        app.projection.items(),
        app.scroll.offset(),
        viewport_height,
        theme,
    );
    if visible.lines.is_empty() && (!app.minimal_mode() || app.minimal_welcome_pending()) {
        if app.minimal_mode() {
            // Minimal mode's print-once card is owned by terminal.rs. Keep
            // this pre-commit fallback byte-for-byte aligned with that
            // native-scrollback surface; the compact full-screen welcome is
            // deliberately scoped to the live application viewport.
            visible.lines.extend([
                Line::styled(
                    app.ui_language()
                        .text("CrabCode 已就绪。", "CrabCode is ready."),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    app.ui_language().text(
                        "请在下方输入提示词。使用 /help 查看 TUI 操作说明。",
                        "Type a prompt below. /help shows TUI controls.",
                    ),
                    Style::default().fg(theme.gray),
                ),
            ]);
        } else {
            visible.lines.extend(empty_transcript_welcome_lines(
                app,
                content_inner.width,
                theme,
            ));
        }
        visible.link_groups = visible_link_groups_from_soft_wrapped_lines(
            &visible.lines,
            &vec![SoftWrapJoiner::HardBreak; visible.lines.len()],
            &[],
            Vec::new(),
        );
    }
    if let Some(regex) = search_highlight.as_ref() {
        highlight_transcript_search_matches(&mut visible.lines, regex);
    }
    app.mermaid_hitboxes.clear();
    let mermaid_fragments = visible
        .link_groups
        .iter()
        .filter_map(|group| match &group.target {
            LinkTarget::Mermaid { action, source } => {
                Some((*action, Arc::clone(source), group.fragments.clone()))
            }
            LinkTarget::Url(_) | LinkTarget::File(_) => None,
        })
        .flat_map(|(action, source, fragments)| {
            fragments
                .into_iter()
                .map(move |fragment| (action, Arc::clone(&source), fragment))
        })
        .collect::<Vec<_>>();
    let mut mermaid_status_overlays = Vec::new();
    let hover_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mouse_position = app.last_mouse_position();
    for (action, source, fragment) in mermaid_fragments {
        let Ok(row) = u16::try_from(fragment.row) else {
            continue;
        };
        let Ok(start_column) = u16::try_from(fragment.start_column) else {
            continue;
        };
        let Ok(end_column) = u16::try_from(fragment.end_column) else {
            continue;
        };
        if row >= content_inner.height
            || start_column >= end_column
            || end_column > content_inner.width
        {
            continue;
        }
        let hitbox = Rect::new(
            content_inner.x.saturating_add(start_column),
            content_inner.y.saturating_add(row),
            end_column - start_column,
            1,
        );
        if mouse_position.is_some_and(|position| hitbox.contains(position.into())) {
            highlight_visible_link(&mut visible.lines, &fragment, hover_style);
        }
        app.mermaid_hitboxes
            .push((hitbox, action, Arc::clone(&source)));

        if action == MermaidAffordanceAction::CopySource
            && app.mermaid_is_rendering(&source)
            && let Some((status_column, status)) = mermaid_affordance_layout(true).status
        {
            let row_width = content_inner
                .width
                .saturating_sub(MERMAID_AFFORDANCE_BODY_INDENT);
            if segment_fits(status_column, status, row_width) {
                mermaid_status_overlays.push((
                    content_inner.x.saturating_add(
                        MERMAID_AFFORDANCE_BODY_INDENT.saturating_add(status_column),
                    ),
                    content_inner.y.saturating_add(row),
                    status,
                ));
            }
        }
    }
    let visible_links = visible
        .link_groups
        .iter()
        .filter_map(VisibleLinkGroup::primary)
        .cloned()
        .collect::<Vec<_>>();
    let hyperlinks = hyperlink_spans_for_visible_link_groups(&visible.link_groups, content_inner);
    app.links.refresh_visible(visible_links);
    if let Some(selected) = app.selected_visible_link().cloned()
        && let Some(group) = visible
            .link_groups
            .iter()
            .find(|group| group.primary() == Some(&selected))
    {
        let selection_style = Style::default()
            .fg(theme.bg_base)
            .bg(theme.link)
            .add_modifier(Modifier::BOLD);
        for fragment in &group.fragments {
            highlight_visible_link(&mut visible.lines, fragment, selection_style);
        }
    }
    app.transcript_line_count = app.transcript_layout.total_lines().max(visible.lines.len());
    app.transcript_viewport_height = viewport_height;
    let offset = app.scroll.offset();
    frame.render_widget(Paragraph::new(visible.lines), content_inner);
    for (x, y, status) in mermaid_status_overlays {
        frame
            .buffer_mut()
            .set_string(x, y, status, Style::default().fg(theme.gray_dim));
    }
    render_sticky_prompt_header(frame, content_inner, app, offset, theme);
    if let Some(key) = app.selected_transcript_key.as_deref()
        && let Some((start, height)) = app.transcript_layout.item_bounds(key)
    {
        let end = start.saturating_add(height);
        let visible_start = start.max(offset);
        let visible_end = end.min(offset.saturating_add(viewport_height));
        if visible_start < visible_end {
            let row = visible_start.saturating_sub(offset);
            let selection_area = Rect::new(
                content_inner.x,
                content_inner
                    .y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                content_inner.width,
                u16::try_from(visible_end - visible_start).unwrap_or(u16::MAX),
            );
            SelectionBox::new(selection_area, Style::default().fg(theme.accent_assistant))
                .with_top_clipped(start < offset)
                .with_bottom_clipped(end > offset.saturating_add(viewport_height))
                .render(frame.buffer_mut());
        }
    }

    if app.transcript_layout.total_lines() > viewport_height && area.width >= 3 {
        let scrollbar_area = Rect::new(
            area.right().saturating_sub(1),
            content_inner.y,
            1,
            content_inner.height,
        );
        let mut state = ScrollbarState::new(app.transcript_layout.total_lines())
            .position(offset)
            .viewport_content_length(viewport_height);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(if app.scroll.is_following() {
                    theme.scrollbar_fg
                } else {
                    theme.gray_bright
                }))
                .track_style(Style::default().fg(theme.scrollbar_bg))
                .begin_symbol(None)
                .end_symbol(None),
            scrollbar_area,
            &mut state,
        );
    }
    let search_cursor =
        render_transcript_search_bar(frame, inner, search_reserved_rows, app, theme);
    (hyperlinks, search_cursor)
}

/// Apply the fixed scrollback-search reverse-video highlight to every regex
/// match on the already-rendered visible rows.
#[cfg(test)]
fn highlight_transcript_search_matches(lines: &mut [Line<'static>], regex: &regex::Regex) {
    for line in lines {
        let plain = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let matches = regex
            .find_iter(&plain)
            .filter(|found| found.start() != found.end())
            .map(|found| found.range())
            .collect::<Vec<_>>();
        if matches.is_empty() {
            continue;
        }

        let old_spans = std::mem::take(&mut line.spans);
        let mut rebuilt = Vec::with_capacity(old_spans.len().saturating_add(matches.len() * 2));
        let mut global_start = 0usize;
        for span in old_spans {
            let content = span.content.into_owned();
            let global_end = global_start.saturating_add(content.len());
            let mut boundaries = vec![0usize, content.len()];
            for matched in &matches {
                let start = matched.start.max(global_start).saturating_sub(global_start);
                let end = matched.end.min(global_end).saturating_sub(global_start);
                if start < end {
                    boundaries.push(start);
                    boundaries.push(end);
                }
            }
            boundaries.sort_unstable();
            boundaries.dedup();
            for pair in boundaries.windows(2) {
                let local_start = pair[0];
                let local_end = pair[1];
                if local_start == local_end {
                    continue;
                }
                let absolute_start = global_start.saturating_add(local_start);
                let absolute_end = global_start.saturating_add(local_end);
                let highlighted = matches
                    .iter()
                    .any(|matched| matched.start < absolute_end && matched.end > absolute_start);
                let style = if highlighted {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                rebuilt.push(Span::styled(
                    content[local_start..local_end].to_string(),
                    style,
                ));
            }
            global_start = global_end;
        }
        line.spans = rebuilt;
    }
}

/// Render the fixed two-row search footer (divider plus query/counter row).
pub(crate) fn render_transcript_search_bar(
    frame: &mut Frame<'_>,
    inner: Rect,
    reserved_rows: u16,
    app: &TuiApp,
    theme: CrabCodeTheme,
) -> Option<(u16, u16)> {
    let search = app.transcript_search.as_ref()?;
    if reserved_rows == 0 || inner.width == 0 {
        return None;
    }
    let reserved_top = inner.bottom().saturating_sub(reserved_rows);
    let bar_y = inner.bottom().saturating_sub(1);
    if reserved_rows >= 2 {
        frame.render_widget(
            Paragraph::new("─".repeat(usize::from(inner.width)))
                .style(Style::default().fg(theme.prompt_border)),
            Rect::new(inner.x, reserved_top, inner.width, 1),
        );
    }

    let language = app.ui_language();
    let label = truncate_str(
        language.text(" 搜索：", " search: "),
        usize::from(inner.width),
    );
    const TRAILING_GAP: u16 = 1;
    let query = search.query();
    let counter = match search.current_index() {
        Some(index) => Some(format!("{}/{}", index + 1, search.match_count())),
        None if search.has_error() => Some(language.text("模式无效", "bad pattern").to_string()),
        None if !query.is_empty() => Some(language.text("无匹配项", "no matches").to_string()),
        None => None,
    };
    let counter_width: u16 = counter.as_deref().map_or(0, |text| text.width() as u16);
    let label_width = u16::try_from(label.width())
        .unwrap_or(u16::MAX)
        .min(inner.width);
    let available_input = inner.width.saturating_sub(label_width);
    let trailing_reserved = if counter_width > 0
        && available_input >= counter_width.saturating_add(TRAILING_GAP).saturating_add(1)
    {
        counter_width.saturating_add(TRAILING_GAP)
    } else {
        0
    };
    let render_width = inner.width.saturating_sub(trailing_reserved);
    let input_width = usize::from(render_width.saturating_sub(label_width));
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(theme.gray)),
        Rect::new(inner.x, bar_y, label_width, 1),
    );

    let viewport = search.query_viewport(input_width);
    let displayed = &query[viewport.visible_byte_range.clone()];
    if input_width > 0 {
        frame.render_widget(
            Paragraph::new(displayed.to_string()).style(Style::default().fg(theme.text_primary)),
            Rect::new(
                inner.x.saturating_add(label_width),
                bar_y,
                u16::try_from(input_width).unwrap_or(u16::MAX),
                1,
            ),
        );
    }
    if trailing_reserved > 0
        && let Some(counter) = counter
    {
        frame.render_widget(
            Paragraph::new(counter).style(Style::default().fg(theme.gray)),
            Rect::new(
                inner.right().saturating_sub(counter_width),
                bar_y,
                counter_width,
                1,
            ),
        );
    }

    (search.is_composing() && input_width > 0).then(|| {
        (
            inner.x.saturating_add(label_width).saturating_add(
                u16::try_from(
                    viewport
                        .cursor_display_column
                        .min(input_width.saturating_sub(1)),
                )
                .unwrap_or(u16::MAX),
            ),
            bar_y,
        )
    })
}

/// Collapse a sticky prompt one row for each row scrolled past, clamping to
/// the prompt's configured minimum and the current viewport.
#[cfg(test)]
pub(crate) fn gradual_sticky_height(
    full_height: u16,
    min_height: u16,
    scroll_past: usize,
    viewport_height: u16,
) -> u16 {
    let past = u16::try_from(scroll_past).unwrap_or(u16::MAX);
    let minimum = min_height.max(1).min(full_height.max(1));
    full_height
        .saturating_sub(past)
        .max(minimum)
        .min(viewport_height)
}

#[cfg(test)]
fn render_sticky_prompt_header(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    offset: usize,
    theme: CrabCodeTheme,
) {
    if offset == 0 || area.height == 0 || area.width == 0 {
        return;
    }
    let Some((item, start, full_height)) = app
        .projection
        .items()
        .iter()
        .filter(|item| item.kind == ProjectedKind::User)
        .filter_map(|item| {
            let (start, height) = app.transcript_layout.item_bounds(&item.key)?;
            (start < offset).then_some((item, start, height))
        })
        .max_by_key(|(_, start, _)| *start)
    else {
        return;
    };
    let full_height = u16::try_from(full_height).unwrap_or(u16::MAX);
    let render_height =
        gradual_sticky_height(full_height, 4, offset.saturating_sub(start), area.height);
    if render_height == 0 {
        return;
    }
    let display_mode = app
        .transcript_item_interactions
        .get(&item.key)
        .map_or(TranscriptDisplayMode::Expanded, |interaction| {
            interaction.mode
        });
    let mut markdown_stream = CrabCodeMarkdownStream::default();
    let mut rendered = render_projected_item_with_state(
        item,
        usize::from(area.width),
        theme,
        &[],
        display_mode,
        false,
        &mut markdown_stream,
        app_syntax_theme(app),
    );
    rendered.lines.truncate(usize::from(render_height));
    let sticky_area = Rect::new(area.x, area.y, area.width, render_height);
    frame.render_widget(
        Paragraph::new(rendered.lines).style(Style::default().bg(theme.bg_dark)),
        sticky_area,
    );
}

#[cfg(test)]
fn hyperlink_spans_for_visible_link_groups(
    groups: &[VisibleLinkGroup],
    area: Rect,
) -> Vec<LinkSpan> {
    let area_width = usize::from(area.width);
    let area_height = usize::from(area.height);
    groups
        .iter()
        .enumerate()
        .flat_map(|(group_index, group)| {
            let url = resolve_link_target(&group.target).and_then(|resolved| resolved.osc8_url);
            let id = u32::try_from(group_index.saturating_add(1)).ok();
            group.fragments.iter().filter_map(move |link| {
                if link.row >= area_height {
                    return None;
                }
                let start = link.start_column.min(area_width);
                let end = link.end_column.min(area_width);
                if start >= end {
                    return None;
                }
                Some(LinkSpan {
                    row: area.y.saturating_add(link.row as u16),
                    col_start: area.x.saturating_add(start as u16),
                    col_end: area.x.saturating_add(end as u16),
                    url: url.clone()?,
                    id,
                })
            })
        })
        .collect()
}

#[cfg(test)]
fn transcript_notice_records(app: &TuiApp) -> Vec<NoticeRecord> {
    let mut records = app
        .renderer_notices()
        .map(|notice| NoticeRecord {
            kind: NoticeKind::Startup,
            text: sanitize_bounded_terminal_text(notice.text(app.ui_language())).into_owned(),
        })
        .chain(app.stderr_notices().map(|notice| NoticeRecord {
            kind: NoticeKind::Stderr,
            text: sanitize_bounded_terminal_text(notice).into_owned(),
        }))
        .collect::<Vec<_>>();
    records.extend(
        app.oauth_browser_notice_lines()
            .into_iter()
            .map(|notice| NoticeRecord {
                kind: NoticeKind::Startup,
                text: sanitize_bounded_terminal_text(&notice).into_owned(),
            }),
    );
    records
}

#[cfg(test)]
fn render_notice_records(
    notices: &[NoticeRecord],
    width: usize,
    theme: CrabCodeTheme,
) -> RenderedTranscriptPart {
    let mut rendered = RenderedTranscriptPart::default();
    for notice in notices {
        let (label, label_style, body_style) = match notice.kind {
            NoticeKind::Startup => (
                "ⓘ startup · ",
                Style::default()
                    .fg(theme.accent_system)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text_secondary),
            ),
            NoticeKind::Stderr => (
                "! backend stderr · ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.warning),
            ),
        };
        for (line_index, line) in notice.text.split('\n').enumerate() {
            let prefix = if line_index == 0 {
                Span::styled(label, label_style)
            } else {
                Span::styled("  ", label_style)
            };
            let annotated = AnnotatedLine::plain(Line::from(vec![
                prefix,
                Span::styled(line.to_string(), body_style),
            ]));
            if append_wrapped_annotated(&mut rendered, annotated, width, theme, &[]) {
                return rendered;
            }
        }
    }
    rendered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
struct ProjectedItemRenderContext {
    show_nested_agent_prompt: bool,
    direct_hook_transcript_counts: DirectHookTranscriptCounts,
    raw: bool,
    syntax_theme: Option<CrabCodeSyntaxTheme>,
    ui_language: UiLanguage,
}

#[cfg(test)]
impl Default for ProjectedItemRenderContext {
    fn default() -> Self {
        Self {
            show_nested_agent_prompt: false,
            direct_hook_transcript_counts: DirectHookTranscriptCounts::default(),
            raw: false,
            syntax_theme: Some(CrabCodeSyntaxTheme::MonokaiExtended),
            ui_language: UiLanguage::ZhCn,
        }
    }
}

#[cfg(test)]
fn render_projected_item(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
) -> RenderedTranscriptPart {
    let mut markdown_stream = CrabCodeMarkdownStream::default();
    render_projected_item_with_state_mode(
        item,
        width,
        theme,
        media_paths,
        TranscriptDisplayMode::Expanded,
        false,
        &mut markdown_stream,
        false,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_projected_item_with_state(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    display_mode: TranscriptDisplayMode,
    selected: bool,
    markdown_stream: &mut CrabCodeMarkdownStream,
    syntax_theme: Option<CrabCodeSyntaxTheme>,
) -> RenderedTranscriptPart {
    render_projected_item_with_state_context(
        item,
        width,
        theme,
        media_paths,
        display_mode,
        selected,
        markdown_stream,
        ProjectedItemRenderContext {
            syntax_theme,
            ..ProjectedItemRenderContext::default()
        },
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn render_projected_item_with_state_context(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    display_mode: TranscriptDisplayMode,
    selected: bool,
    markdown_stream: &mut CrabCodeMarkdownStream,
    render_context: ProjectedItemRenderContext,
) -> RenderedTranscriptPart {
    render_projected_item_with_state_mode_context(
        item,
        width,
        theme,
        media_paths,
        display_mode,
        selected,
        markdown_stream,
        crate::terminal::active_mode_is_minimal(),
        render_context,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn render_projected_item_with_state_mode(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    display_mode: TranscriptDisplayMode,
    selected: bool,
    markdown_stream: &mut CrabCodeMarkdownStream,
    static_commit: bool,
) -> RenderedTranscriptPart {
    render_projected_item_with_state_mode_context(
        item,
        width,
        theme,
        media_paths,
        display_mode,
        selected,
        markdown_stream,
        static_commit,
        ProjectedItemRenderContext::default(),
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn render_projected_item_with_state_mode_context(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    display_mode: TranscriptDisplayMode,
    selected: bool,
    markdown_stream: &mut CrabCodeMarkdownStream,
    static_commit: bool,
    render_context: ProjectedItemRenderContext,
) -> RenderedTranscriptPart {
    let mut rendered = RenderedTranscriptPart::default();
    let _truncated = append_projected_item(
        &mut rendered,
        item,
        width,
        theme,
        media_paths,
        markdown_stream,
        static_commit,
        render_context,
    );
    if crate::tui_app::projected_item_is_collapsible(item) {
        match display_mode {
            TranscriptDisplayMode::Collapsed => {
                collapse_rendered_item(item, &mut rendered, theme, render_context.ui_language)
            }
            TranscriptDisplayMode::Truncated if item.kind == ProjectedKind::Thinking => {
                truncate_rendered_thinking(&mut rendered, theme)
            }
            TranscriptDisplayMode::Truncated | TranscriptDisplayMode::Expanded => {}
        }
    }
    if selected {
        let selected_style = Style::default().bg(theme.bg_highlight);
        for line in &mut rendered.lines {
            line.style = line.style.patch(selected_style);
            for span in &mut line.spans {
                span.style = span.style.patch(selected_style);
            }
        }
    }
    rendered
}

#[cfg(test)]
fn collapse_rendered_item(
    item: &ProjectedItem,
    rendered: &mut RenderedTranscriptPart,
    theme: CrabCodeTheme,
    language: UiLanguage,
) {
    if rendered.lines.is_empty() {
        return;
    }
    if matches!(
        item.presentation.advisor.as_ref(),
        Some(&AdvisorPresentation::Result(
            AdvisorResultPresentation::Feedback { .. }
        ))
    ) {
        remap_rendered_rows(
            rendered,
            vec![
                (
                    None,
                    Line::from(vec![
                        Span::styled("✓ ", Style::default().fg(theme.accent_success)),
                        Span::styled(
                            advisor_reviewed_message(language),
                            Style::default().fg(theme.gray),
                        ),
                    ]),
                ),
                (None, Line::default()),
            ],
        );
        return;
    }
    let title = rendered.lines[0].clone();
    remap_rendered_rows(rendered, vec![(Some(0), title), (None, Line::default())]);
}

#[cfg(test)]
fn truncate_rendered_thinking(rendered: &mut RenderedTranscriptPart, theme: CrabCodeTheme) {
    const TRUNCATED_THINKING_LINES: usize = 3;
    if rendered.lines.len() <= TRUNCATED_THINKING_LINES.saturating_add(2) {
        return;
    }
    let body_end = if rendered.lines.last().is_some_and(|line| line.width() == 0) {
        rendered.lines.len() - 1
    } else {
        rendered.lines.len()
    };
    let body_start = 1;
    if body_end.saturating_sub(body_start) <= TRUNCATED_THINKING_LINES {
        return;
    }
    let first_retained = body_end - TRUNCATED_THINKING_LINES;
    let mut rows = Vec::with_capacity(TRUNCATED_THINKING_LINES + 3);
    rows.push((Some(0), rendered.lines[0].clone()));
    rows.push((None, Line::styled("  …", Style::default().fg(theme.gray))));
    rows.extend(
        (first_retained..body_end).map(|index| (Some(index), rendered.lines[index].clone())),
    );
    rows.push((None, Line::default()));
    remap_rendered_rows(rendered, rows);
}

#[cfg(test)]
fn remap_rendered_rows(
    rendered: &mut RenderedTranscriptPart,
    rows: Vec<(Option<usize>, Line<'static>)>,
) {
    let row_map = rows
        .iter()
        .enumerate()
        .filter_map(|(new_row, (old_row, _))| old_row.map(|old_row| (old_row, new_row)))
        .collect::<HashMap<_, _>>();
    rendered.lines = rows.into_iter().map(|(_, line)| line).collect();
    for group in &mut rendered.link_groups {
        group.fragments.retain_mut(|fragment| {
            let Some(new_row) = row_map.get(&fragment.row).copied() else {
                return false;
            };
            fragment.row = new_row;
            true
        });
    }
    rendered
        .link_groups
        .retain(|group| !group.fragments.is_empty());
}

#[cfg(test)]
fn advisor_reviewed_message(language: UiLanguage) -> &'static str {
    language.text(ADVISOR_REVIEWED_MESSAGE_ZH, ADVISOR_REVIEWED_MESSAGE_EN)
}

#[cfg(test)]
fn append_advisor_item(
    rendered: &mut RenderedTranscriptPart,
    item: &ProjectedItem,
    advisor: &AdvisorPresentation,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    language: UiLanguage,
) -> bool {
    match advisor {
        AdvisorPresentation::Invocation { input, state } => {
            let (indicator, indicator_style) = match state {
                AdvisorInvocationState::InProgress => {
                    ("◌ ", Style::default().fg(theme.accent_running))
                }
                AdvisorInvocationState::Succeeded => {
                    ("✓ ", Style::default().fg(theme.accent_success))
                }
                AdvisorInvocationState::Failed => ("× ", Style::default().fg(theme.accent_error)),
            };
            let mut spans = vec![
                Span::styled(indicator, indicator_style),
                Span::styled(
                    language.text("顾问审阅", "Advising"),
                    Style::default()
                        .fg(theme.accent_tool)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if item.streaming {
                spans.push(Span::styled(
                    " …",
                    Style::default().fg(theme.accent_running),
                ));
            }
            if input.as_object().is_some_and(|object| !object.is_empty()) {
                spans.push(Span::styled(" · ", Style::default().fg(theme.gray_dim)));
                spans.push(Span::styled(
                    compact_json(input),
                    Style::default().fg(theme.gray),
                ));
            }
            if append_wrapped_annotated(
                rendered,
                AnnotatedLine::plain(Line::from(spans)),
                width.max(1),
                theme,
                media_paths,
            ) {
                return true;
            }
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Feedback { text }) => {
            if push_bounded(
                rendered,
                Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.accent_tool)),
                    Span::styled(
                        language.text("顾问反馈", "Advisor feedback"),
                        Style::default()
                            .fg(theme.accent_tool)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                theme,
            ) {
                return true;
            }
            for source_line in text.split('\n') {
                let safe = sanitize_bounded_terminal_text(source_line);
                if append_wrapped_annotated(
                    rendered,
                    AnnotatedLine::plain(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(safe.into_owned(), Style::default().fg(theme.gray)),
                    ])),
                    width.max(1),
                    theme,
                    media_paths,
                ) {
                    return true;
                }
            }
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Redacted) => {
            if append_wrapped_annotated(
                rendered,
                AnnotatedLine::plain(Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(theme.accent_success)),
                    Span::styled(
                        advisor_reviewed_message(language),
                        Style::default().fg(theme.gray),
                    ),
                ])),
                width.max(1),
                theme,
                media_paths,
            ) {
                return true;
            }
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Error { error_code }) => {
            let safe_error_code = sanitize_bounded_terminal_text(error_code);
            if append_wrapped_annotated(
                rendered,
                AnnotatedLine::plain(Line::from(vec![
                    Span::styled("× ", Style::default().fg(theme.accent_error)),
                    Span::styled(
                        format!(
                            "{} ({safe_error_code})",
                            language.text("顾问不可用", "Advisor unavailable")
                        ),
                        Style::default()
                            .fg(theme.accent_error)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])),
                width.max(1),
                theme,
                media_paths,
            ) {
                return true;
            }
        }
    }
    push_bounded(rendered, Line::default(), theme)
}

#[cfg(test)]
fn append_nested_agent_prompt(
    rendered: &mut RenderedTranscriptPart,
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    syntax_theme: Option<CrabCodeSyntaxTheme>,
    language: UiLanguage,
) -> bool {
    let Some(DirectProgressPresentation::Nested {
        progress_type,
        prompt,
        ..
    }) = item.presentation.direct_progress.as_ref()
    else {
        return false;
    };
    if progress_type != "agent_progress" || prompt.is_empty() {
        return false;
    }

    if push_scanned_unwrapped(
        rendered,
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                language.text("提示词：", "Prompt:"),
                Style::default()
                    .fg(theme.accent_success)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        theme,
        media_paths,
    ) {
        return true;
    }

    let body_width = width.saturating_sub(2).max(1);
    let safe_prompt = sanitize_bounded_terminal_text(prompt);
    let mut prompt_stream = CrabCodeMarkdownStream::default();
    let prompt_render = prompt_stream
        .synchronize(
            &safe_prompt,
            false,
            false,
            Style::default().fg(theme.text_primary),
            theme,
            syntax_theme,
            body_width,
            media_paths,
        )
        .clone();
    let mut markdown_link_groups = HashMap::new();
    for markdown in prompt_render.lines {
        let prefix_bytes = 2;
        let source_line = markdown.source_line;
        let mut links = markdown.links;
        for link in &mut links {
            link.start_byte = link.start_byte.saturating_add(prefix_bytes);
            link.end_byte = link.end_byte.saturating_add(prefix_bytes);
        }
        if append_wrapped_markdown_annotated(
            rendered,
            AnnotatedLine {
                line: Line::from(
                    std::iter::once(Span::raw("  "))
                        .chain(markdown.line.spans)
                        .collect::<Vec<_>>(),
                ),
                links,
                source_line,
            },
            width.max(1),
            theme,
            media_paths,
            &mut markdown_link_groups,
        ) {
            return true;
        }
    }
    push_bounded(rendered, Line::default(), theme)
}

#[cfg(test)]
fn append_indented_plain_text(
    rendered: &mut RenderedTranscriptPart,
    text: &str,
    width: usize,
    style: Style,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
) -> bool {
    for source_line in text.split('\n') {
        if append_wrapped_annotated(
            rendered,
            AnnotatedLine::plain(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    sanitize_bounded_terminal_text(source_line).into_owned(),
                    style,
                ),
            ])),
            width.max(1),
            theme,
            media_paths,
        ) {
            return true;
        }
    }
    false
}

#[cfg(test)]
fn direct_mcp_progress_bar(ratio: f64) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let whole = (ratio * DIRECT_MCP_PROGRESS_BAR_CELLS as f64).floor() as usize;
    let mut bar = DIRECT_MCP_PROGRESS_BLOCKS[DIRECT_MCP_PROGRESS_BLOCKS.len() - 1].repeat(whole);
    if whole < DIRECT_MCP_PROGRESS_BAR_CELLS {
        let remainder = ratio * DIRECT_MCP_PROGRESS_BAR_CELLS as f64 - whole as f64;
        let partial = (remainder * DIRECT_MCP_PROGRESS_BLOCKS.len() as f64).floor() as usize;
        bar.push_str(
            DIRECT_MCP_PROGRESS_BLOCKS
                [partial.min(DIRECT_MCP_PROGRESS_BLOCKS.len().saturating_sub(1))],
        );
        bar.push_str(
            &DIRECT_MCP_PROGRESS_BLOCKS[0]
                .repeat(DIRECT_MCP_PROGRESS_BAR_CELLS.saturating_sub(whole + 1)),
        );
    }
    bar
}

#[cfg(test)]
fn append_direct_mcp_progress_body(
    rendered: &mut RenderedTranscriptPart,
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    language: UiLanguage,
) -> Option<bool> {
    let DirectProgressPresentation::Mcp {
        progress,
        total,
        progress_message,
        percentage,
        ..
    } = item.presentation.direct_progress.as_ref()?
    else {
        return None;
    };
    let body_style = Style::default().fg(theme.gray);

    let truncated = if progress.is_none() {
        append_indented_plain_text(
            rendered,
            language.text("运行中…", "Running…"),
            width,
            body_style,
            theme,
            media_paths,
        )
    } else if let (Some(progress_value), Some(total_value), Some(percentage)) = (
        progress.as_ref().and_then(serde_json::Number::as_f64),
        total.as_ref().and_then(serde_json::Number::as_f64),
        percentage,
    ) {
        if total_value <= 0.0 {
            let text = progress_message.as_ref().map_or_else(
                || {
                    format!(
                        "{} {}",
                        language.text("处理中…", "Processing…"),
                        progress
                            .as_ref()
                            .expect("the branch established exact MCP progress")
                    )
                },
                Clone::clone,
            );
            append_indented_plain_text(rendered, &text, width, body_style, theme, media_paths)
        } else {
            if progress_message
                .as_ref()
                .is_some_and(|message| !message.is_empty())
                && append_indented_plain_text(
                    rendered,
                    progress_message
                        .as_deref()
                        .expect("the branch established a non-empty progress message"),
                    width,
                    body_style,
                    theme,
                    media_paths,
                )
            {
                return Some(true);
            }
            let bar = direct_mcp_progress_bar(progress_value / total_value);
            push_scanned_unwrapped(
                rendered,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(bar, Style::default().fg(theme.accent_running)),
                    Span::raw(" "),
                    Span::styled(format!("{percentage}%"), body_style),
                ]),
                theme,
                media_paths,
            )
        }
    } else {
        let text = progress_message.as_ref().map_or_else(
            || {
                format!(
                    "{} {}",
                    language.text("处理中…", "Processing…"),
                    progress
                        .as_ref()
                        .expect("the branch established exact MCP progress")
                )
            },
            Clone::clone,
        );
        append_indented_plain_text(rendered, &text, width, body_style, theme, media_paths)
    };
    if truncated {
        return Some(true);
    }
    Some(push_bounded(rendered, Line::default(), theme))
}

#[cfg(test)]
fn append_direct_hook_transcript_summary(
    rendered: &mut RenderedTranscriptPart,
    hook_event: &'static str,
    count: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    language: UiLanguage,
) -> bool {
    if count == 0 {
        return false;
    }
    push_scanned_unwrapped(
        rendered,
        Line::from(vec![
            Span::styled("  ⎿ \u{a0}", Style::default().fg(theme.gray)),
            Span::styled(format!("{count} "), Style::default().fg(theme.gray)),
            Span::styled(
                hook_event,
                Style::default().fg(theme.gray).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if matches!(language, UiLanguage::ZhCn) {
                    " Hook 已运行"
                } else if count == 1 {
                    " hook ran"
                } else {
                    " hooks ran"
                },
                Style::default().fg(theme.gray),
            ),
        ]),
        theme,
        media_paths,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn append_projected_item(
    rendered: &mut RenderedTranscriptPart,
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    markdown_stream: &mut CrabCodeMarkdownStream,
    static_commit: bool,
    render_context: ProjectedItemRenderContext,
) -> bool {
    if let Some(advisor) = item.presentation.advisor.as_ref() {
        return append_advisor_item(
            rendered,
            item,
            advisor,
            width,
            theme,
            media_paths,
            render_context.ui_language,
        );
    }
    if render_context.show_nested_agent_prompt
        && append_nested_agent_prompt(
            rendered,
            item,
            width,
            theme,
            media_paths,
            render_context.syntax_theme,
            render_context.ui_language,
        )
    {
        return true;
    }
    let (prefix, title_style, body_style) = match item.kind {
        ProjectedKind::User => (
            "◆",
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.text_primary),
        ),
        ProjectedKind::Assistant => (
            "●",
            Style::default()
                .fg(theme.accent_assistant)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.text_secondary),
        ),
        ProjectedKind::Thinking => (
            "◇",
            Style::default().fg(theme.accent_thinking),
            Style::default()
                .fg(theme.gray)
                .add_modifier(Modifier::ITALIC),
        ),
        ProjectedKind::ToolUse => (
            "▶",
            Style::default().fg(theme.accent_tool),
            Style::default().fg(theme.gray_bright),
        ),
        ProjectedKind::ToolResult | ProjectedKind::TerminalOutput => (
            "│",
            Style::default().fg(theme.accent_tool),
            Style::default().fg(theme.text_secondary),
        ),
        ProjectedKind::Progress => (
            "◌",
            Style::default().fg(theme.accent_running),
            Style::default().fg(theme.gray_bright),
        ),
        ProjectedKind::Warning => (
            "!",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.warning),
        ),
        ProjectedKind::Error => (
            "×",
            Style::default()
                .fg(theme.accent_error)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.accent_error),
        ),
        ProjectedKind::System => (
            "·",
            Style::default().fg(theme.accent_system),
            Style::default().fg(theme.gray),
        ),
    };
    let display_title = if item.kind == ProjectedKind::User && item.title == "You" {
        render_context.ui_language.text("你", "You")
    } else {
        item.title.as_str()
    };
    let streaming = if item.streaming { " …" } else { "" };
    if push_scanned_unwrapped(
        rendered,
        Line::from(vec![
            Span::styled(format!("{prefix} "), title_style),
            Span::styled(
                sanitize_bounded_terminal_text(display_title).into_owned(),
                title_style,
            ),
            Span::styled(streaming, Style::default().fg(theme.accent_running)),
        ]),
        theme,
        media_paths,
    ) {
        return true;
    }

    if append_direct_hook_transcript_summary(
        rendered,
        "PreToolUse",
        render_context.direct_hook_transcript_counts.pre_tool_use,
        theme,
        media_paths,
        render_context.ui_language,
    ) {
        return true;
    }

    if let Some(truncated) = append_direct_mcp_progress_body(
        rendered,
        item,
        width,
        theme,
        media_paths,
        render_context.ui_language,
    ) {
        return truncated;
    }

    let body_width = width.saturating_sub(2).max(1);
    if item.kind == ProjectedKind::TerminalOutput {
        let (source, omitted) = bounded_terminal_source(&item.text);
        for terminal_line in render_terminal_lines(source, body_style) {
            let safe = terminal_line
                .line
                .spans
                .into_iter()
                .map(|span| {
                    Span::styled(
                        sanitize_bounded_terminal_text(&span.content).into_owned(),
                        span.style,
                    )
                })
                .collect::<Vec<_>>();
            let line = Line::from(
                std::iter::once(Span::raw("  "))
                    .chain(safe)
                    .collect::<Vec<_>>(),
            );
            if append_wrapped_annotated(
                rendered,
                AnnotatedLine::plain(line),
                width,
                theme,
                media_paths,
            ) {
                return true;
            }
        }
        if let Some(omitted) = omitted
            && push_bounded(
                rendered,
                Line::styled(
                    format!(
                        "  ⟪render-only truncation: {omitted} UTF-8 byte(s) omitted; full value retained⟫"
                    ),
                    Style::default().fg(theme.warning),
                ),
                theme,
            )
        {
            return true;
        }
    } else {
        let safe_text = sanitize_bounded_terminal_text(&item.text);
        let markdown_render = markdown_stream.synchronize(
            &safe_text,
            item.streaming,
            render_context.raw
                && matches!(
                    item.kind,
                    ProjectedKind::Assistant | ProjectedKind::Thinking
                ),
            body_style,
            theme,
            render_context.syntax_theme,
            body_width,
            media_paths,
        );
        let mut mermaid_after_prewrap_line = HashMap::<usize, Vec<Arc<str>>>::new();
        if item.kind == ProjectedKind::Assistant
            && !item.streaming
            && !static_commit
            && !render_context.raw
        {
            for block in markdown_render
                .code_blocks
                .iter()
                .filter(|block| is_mermaid_info(&block.info) && !block.output_line_range.is_empty())
            {
                assert!(
                    block.output_line_range.end <= markdown_render.lines.len(),
                    "Markdown code-block output range stays within the rendered view"
                );
                mermaid_after_prewrap_line
                    .entry(block.output_line_range.end)
                    .or_default()
                    .push(Arc::from(block.body.as_str()));
            }
        }
        let mut markdown_link_groups = HashMap::new();
        for (prewrap_index, markdown) in markdown_render.lines.iter().cloned().enumerate() {
            let prefix_bytes = 2;
            let source_line = markdown.source_line;
            let mut links = markdown.links;
            for link in &mut links {
                link.start_byte = link.start_byte.saturating_add(prefix_bytes);
                link.end_byte = link.end_byte.saturating_add(prefix_bytes);
            }
            let prefixed = AnnotatedLine {
                line: Line::from(
                    std::iter::once(Span::raw("  "))
                        .chain(markdown.line.spans)
                        .collect::<Vec<_>>(),
                ),
                links,
                source_line,
            };
            if append_wrapped_markdown_annotated(
                rendered,
                prefixed,
                body_width.saturating_add(2),
                theme,
                media_paths,
                &mut markdown_link_groups,
            ) {
                return true;
            }
            if let Some(sources) = mermaid_after_prewrap_line.get(&(prewrap_index + 1)) {
                for source in sources {
                    if append_mermaid_affordance_row(rendered, width, theme, Arc::clone(source)) {
                        return true;
                    }
                }
            }
        }
    }
    if append_direct_hook_transcript_summary(
        rendered,
        "PostToolUse",
        render_context.direct_hook_transcript_counts.post_tool_use,
        theme,
        media_paths,
        render_context.ui_language,
    ) {
        return true;
    }
    push_bounded(rendered, Line::default(), theme)
}

#[cfg(test)]
fn append_mermaid_affordance_row(
    rendered: &mut RenderedTranscriptPart,
    width: usize,
    theme: CrabCodeTheme,
    source: Arc<str>,
) -> bool {
    let content_width =
        u16::try_from(width.saturating_sub(usize::from(MERMAID_AFFORDANCE_BODY_INDENT)))
            .unwrap_or(u16::MAX);
    let affordance = mermaid_affordance_layout(false);
    let fitting_buttons = affordance
        .buttons
        .into_iter()
        .filter(|button| segment_fits(button.column, button.label, content_width))
        .collect::<Vec<_>>();
    let mut spans = Vec::with_capacity(fitting_buttons.len().saturating_mul(2) + 2);
    let mut cursor = 0_u16;
    let mut append_segment = |column: u16, text: &'static str, style: Style| {
        if column > cursor {
            spans.push(Span::raw(" ".repeat(usize::from(column - cursor))));
        }
        spans.push(Span::styled(text, style));
        cursor = column.saturating_add(mermaid_display_width(text));
    };

    let (label_column, label) = affordance.label;
    if segment_fits(label_column, label, content_width) {
        append_segment(
            MERMAID_AFFORDANCE_BODY_INDENT.saturating_add(label_column),
            label,
            Style::default().fg(theme.gray_dim),
        );
    }
    for button in &fitting_buttons {
        append_segment(
            MERMAID_AFFORDANCE_BODY_INDENT.saturating_add(button.column),
            button.label,
            Style::default().fg(theme.gray),
        );
    }

    let row = rendered.lines.len();
    if push_bounded(rendered, Line::from(spans), theme) {
        return true;
    }
    for button in fitting_buttons {
        let start_column =
            usize::from(MERMAID_AFFORDANCE_BODY_INDENT.saturating_add(button.column));
        let end_column =
            start_column.saturating_add(usize::from(mermaid_display_width(button.label)));
        let target = LinkTarget::Mermaid {
            action: button.action,
            source: Arc::clone(&source),
        };
        rendered.link_groups.push(VisibleLinkGroup {
            target: target.clone(),
            fragments: vec![VisibleLink {
                target,
                row,
                start_column,
                end_column,
            }],
        });
    }
    false
}

#[cfg(test)]
fn safe_render_tool_result_with<T>(
    tool_name: &str,
    render: impl FnOnce() -> Option<T>,
    report: impl FnOnce(String),
) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(render)) {
        Ok(rendered) => rendered,
        Err(error) => {
            let detail = error
                .downcast_ref::<&str>()
                .copied()
                .map(str::to_string)
                .or_else(|| error.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            report(format!(
                "Error rendering tool result for {tool_name}: {detail}"
            ));
            None
        }
    }
}

#[cfg(test)]
pub(crate) fn terminal_lines_for_projected_item(
    item: &ProjectedItem,
    width: usize,
) -> Vec<Line<'static>> {
    terminal_lines_for_projected_item_with_theme(
        item,
        width,
        CrabCodeTheme::NIGHT,
        CrabCodeThemeKind::Dark,
        false,
    )
}

#[cfg(test)]
pub(crate) fn terminal_lines_for_projected_item_with_theme(
    item: &ProjectedItem,
    width: usize,
    theme: CrabCodeTheme,
    theme_kind: CrabCodeThemeKind,
    syntax_highlighting_disabled: bool,
) -> Vec<Line<'static>> {
    let mut markdown_stream = CrabCodeMarkdownStream::default();
    let mut rendered = RenderedTranscriptPart::default();
    let _truncated = append_projected_item(
        &mut rendered,
        item,
        width.max(1),
        theme,
        &[],
        &mut markdown_stream,
        true,
        ProjectedItemRenderContext {
            syntax_theme: crabcode_syntax_theme(theme_kind, syntax_highlighting_disabled),
            ..ProjectedItemRenderContext::default()
        },
    );
    rendered.lines
}

#[cfg(test)]
fn push_bounded(
    rendered: &mut RenderedTranscriptPart,
    line: Line<'static>,
    theme: CrabCodeTheme,
) -> bool {
    if rendered.lines.len() < MAX_RENDERED_LINES_PER_ITEM.saturating_sub(1) {
        rendered.lines.push(line);
        return false;
    }
    if rendered.lines.len() < MAX_RENDERED_LINES_PER_ITEM {
        rendered.lines.push(Line::styled(
            "⟪render-only item line budget reached; CrabCode's session transcript remains authoritative⟫",
            Style::default().fg(theme.warning),
        ));
    }
    true
}

#[cfg(test)]
fn push_scanned_unwrapped(
    rendered: &mut RenderedTranscriptPart,
    line: Line<'static>,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
) -> bool {
    let groups = visible_link_groups_from_soft_wrapped_lines(
        std::slice::from_ref(&line),
        &[SoftWrapJoiner::HardBreak],
        media_paths,
        Vec::new(),
    );
    let row = rendered.lines.len();
    if push_bounded(rendered, line, theme) {
        return true;
    }
    for mut group in groups {
        for fragment in &mut group.fragments {
            fragment.row = fragment.row.saturating_add(row);
        }
        rendered.link_groups.push(group);
    }
    false
}

#[cfg(test)]
fn append_wrapped_annotated(
    rendered: &mut RenderedTranscriptPart,
    annotated: AnnotatedLine,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
) -> bool {
    let wrapped = wrap_annotated_line(annotated, width, media_paths);
    let remaining = MAX_RENDERED_LINES_PER_ITEM
        .saturating_sub(1)
        .saturating_sub(rendered.lines.len());
    let retained = wrapped.lines.len().min(remaining);
    append_rendered_slice(rendered, &wrapped, 0, retained);
    if retained < wrapped.lines.len() {
        let _ = push_bounded(
            rendered,
            Line::styled(
                "⟪render-only item line budget reached; CrabCode's session transcript remains authoritative⟫",
                Style::default().fg(theme.warning),
            ),
            theme,
        );
        return true;
    }
    false
}

#[cfg(test)]
fn append_wrapped_markdown_annotated(
    rendered: &mut RenderedTranscriptPart,
    annotated: AnnotatedLine,
    width: usize,
    theme: CrabCodeTheme,
    media_paths: &[PathBuf],
    semantic_destinations: &mut HashMap<u32, usize>,
) -> bool {
    let wrapped = wrap_annotated_line_with_semantic_ids(annotated, width, media_paths);
    let remaining = MAX_RENDERED_LINES_PER_ITEM
        .saturating_sub(1)
        .saturating_sub(rendered.lines.len());
    let retained = wrapped.rendered.lines.len().min(remaining);
    let row_offset = rendered.lines.len();
    rendered
        .lines
        .extend(wrapped.rendered.lines[..retained].iter().cloned());

    for (group, semantic_id) in wrapped
        .rendered
        .link_groups
        .iter()
        .zip(wrapped.semantic_ids)
    {
        let fragments = group
            .fragments
            .iter()
            .filter(|fragment| fragment.row < retained)
            .map(|fragment| VisibleLink {
                target: fragment.target.clone(),
                row: row_offset.saturating_add(fragment.row),
                start_column: fragment.start_column,
                end_column: fragment.end_column,
            })
            .collect::<Vec<_>>();
        if fragments.is_empty() {
            continue;
        }
        if let Some(semantic_id) = semantic_id
            && let Some(index) = semantic_destinations.get(&semantic_id).copied()
        {
            let destination = rendered
                .link_groups
                .get_mut(index)
                .expect("Markdown semantic link group index remains valid");
            assert_eq!(
                destination.target, group.target,
                "one Markdown semantic link ID has exactly one target"
            );
            destination.fragments.extend(fragments);
            continue;
        }
        let index = rendered.link_groups.len();
        rendered.link_groups.push(VisibleLinkGroup {
            target: group.target.clone(),
            fragments,
        });
        if let Some(semantic_id) = semantic_id {
            semantic_destinations.insert(semantic_id, index);
        }
    }

    if retained < wrapped.rendered.lines.len() {
        let _ = push_bounded(
            rendered,
            Line::styled(
                "⟪render-only item line budget reached; CrabCode's session transcript remains authoritative⟫",
                Style::default().fg(theme.warning),
            ),
            theme,
        );
        return true;
    }
    false
}

#[cfg(test)]
fn append_rendered_slice(
    destination: &mut RenderedTranscriptPart,
    source: &RenderedTranscriptPart,
    start: usize,
    end: usize,
) {
    let row_offset = destination.lines.len();
    destination
        .lines
        .extend(source.lines[start..end].iter().cloned());
    for group in &source.link_groups {
        let fragments = group
            .fragments
            .iter()
            .filter(|fragment| fragment.row >= start && fragment.row < end)
            .map(|fragment| VisibleLink {
                target: fragment.target.clone(),
                row: row_offset.saturating_add(fragment.row.saturating_sub(start)),
                start_column: fragment.start_column,
                end_column: fragment.end_column,
            })
            .collect::<Vec<_>>();
        if !fragments.is_empty() {
            destination.link_groups.push(VisibleLinkGroup {
                target: group.target.clone(),
                fragments,
            });
        }
    }
}

#[derive(Debug)]
#[cfg(test)]
struct WrappedRowSource {
    start_byte: usize,
    end_byte: usize,
    repeated_prefix_width: usize,
}

#[cfg(test)]
fn wrap_annotated_line(
    annotated: AnnotatedLine,
    width: usize,
    media_paths: &[PathBuf],
) -> RenderedTranscriptPart {
    wrap_annotated_line_with_semantic_ids(annotated, width, media_paths).rendered
}

#[derive(Debug)]
#[cfg(test)]
struct WrappedAnnotatedLine {
    rendered: RenderedTranscriptPart,
    semantic_ids: Vec<Option<u32>>,
}

#[cfg(test)]
fn wrap_annotated_line_with_semantic_ids(
    annotated: AnnotatedLine,
    width: usize,
    media_paths: &[PathBuf],
) -> WrappedAnnotatedLine {
    let flat = annotated
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let lines = wrap_line(&annotated.line, width);
    let Some((row_sources, joiners)) = wrapped_row_sources(&flat, &lines) else {
        let joiners = vec![SoftWrapJoiner::HardBreak; lines.len()];
        let link_groups =
            visible_link_groups_from_soft_wrapped_lines(&lines, &joiners, media_paths, Vec::new());
        let semantic_ids = vec![None; link_groups.len()];
        return WrappedAnnotatedLine {
            rendered: RenderedTranscriptPart { lines, link_groups },
            semantic_ids,
        };
    };

    let mut semantic_groups = Vec::new();
    let mut semantic_ids = Vec::new();
    for link in annotated.links {
        let mut fragments = Vec::new();
        for (row, source) in row_sources.iter().enumerate() {
            let start = link.start_byte.max(source.start_byte);
            let end = link.end_byte.min(source.end_byte);
            if start >= end {
                continue;
            }
            let start_column = source
                .repeated_prefix_width
                .saturating_add(flat[source.start_byte..start].width());
            let end_column = start_column.saturating_add(flat[start..end].width());
            if start_column < end_column {
                fragments.push(VisibleLink {
                    target: link.target.clone(),
                    row,
                    start_column,
                    end_column,
                });
            }
        }
        if !fragments.is_empty() {
            semantic_groups.push(VisibleLinkGroup {
                target: link.target,
                fragments,
            });
            semantic_ids.push(link.semantic_id);
        }
    }
    let link_groups =
        visible_link_groups_from_soft_wrapped_lines(&lines, &joiners, media_paths, semantic_groups);
    semantic_ids.resize(link_groups.len(), None);
    WrappedAnnotatedLine {
        rendered: RenderedTranscriptPart { lines, link_groups },
        semantic_ids,
    }
}

#[cfg(test)]
fn wrapped_row_sources(
    flat: &str,
    lines: &[Line<'_>],
) -> Option<(Vec<WrappedRowSource>, Vec<SoftWrapJoiner>)> {
    if flat.contains('\n') {
        return None;
    }
    let graphemes = flat.graphemes(true).collect::<Vec<_>>();
    let mut prefix_graphemes = 0_usize;
    while prefix_graphemes + 1 < graphemes.len()
        && graphemes[prefix_graphemes] == "│"
        && graphemes[prefix_graphemes + 1] == " "
    {
        prefix_graphemes += 2;
    }
    if prefix_graphemes == graphemes.len() {
        prefix_graphemes = 0;
    }
    let prefix = graphemes[..prefix_graphemes].concat();
    let prefix_width = prefix.width();

    let mut cursor = 0_usize;
    let mut sources = Vec::with_capacity(lines.len());
    let mut joiners = Vec::with_capacity(lines.len());
    joiners.push(SoftWrapJoiner::HardBreak);
    for (row, line) in lines.iter().enumerate() {
        let painted = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let (body, repeated_prefix_width) = if row > 0 && !prefix.is_empty() {
            (painted.strip_prefix(&prefix)?, prefix_width)
        } else {
            (painted.as_str(), 0)
        };
        if !flat.get(cursor..)?.starts_with(body) {
            return None;
        }
        let start_byte = cursor;
        cursor = cursor.checked_add(body.len())?;
        sources.push(WrappedRowSource {
            start_byte,
            end_byte: cursor,
            repeated_prefix_width,
        });
        if row + 1 < lines.len() {
            let mut skipped_whitespace = false;
            while cursor < flat.len() {
                let grapheme = flat.get(cursor..)?.graphemes(true).next()?;
                if !grapheme.chars().all(char::is_whitespace) {
                    break;
                }
                cursor = cursor.checked_add(grapheme.len())?;
                skipped_whitespace = true;
            }
            joiners.push(if skipped_whitespace {
                SoftWrapJoiner::Space
            } else {
                SoftWrapJoiner::MidWord
            });
        }
    }
    if flat
        .get(cursor..)?
        .graphemes(true)
        .any(|grapheme| !grapheme.chars().all(char::is_whitespace))
    {
        return None;
    }
    Some((sources, joiners))
}

#[cfg(test)]
fn bounded_terminal_source(source: &str) -> (&str, Option<usize>) {
    if source.len() <= MAX_RENDER_FIELD_BYTES {
        return (source, None);
    }
    let mut boundary = MAX_RENDER_FIELD_BYTES;
    while !source.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&source[..boundary], Some(source.len() - boundary))
}

#[cfg(test)]
fn semantic_markdown_target(target: &str, media_paths: &[PathBuf]) -> Option<LinkTarget> {
    if safe_standard_url_target(target).is_some() {
        return Some(LinkTarget::Url(std::sync::Arc::from(target)));
    }
    local_link_to_file_target(target, media_paths)
}

#[cfg(test)]
mod legacy_markdown_test_support {
    use super::*;

    #[derive(Debug, Clone)]
    struct Fence {
        marker: char,
        length: usize,
        language: String,
        body: String,
        body_start_in_source: usize,
    }

    #[cfg(test)]
    pub(super) fn markdown_lines(
        source: &str,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
        media_paths: &[PathBuf],
    ) -> Vec<AnnotatedLine> {
        let normalized = crate::crabcode_markdown::normalize_latex_delimiters(source);
        let mut open_code = crate::crabcode_markdown::CrabCodeOpenCodeHighlighter::default();
        markdown_render_full(
            &normalized,
            default_style,
            theme,
            width,
            media_paths,
            0,
            &mut open_code,
        )
        .lines
    }

    pub(super) fn markdown_render_full(
        source: &str,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
        media_paths: &[PathBuf],
        source_byte_base: usize,
        open_code: &mut crate::crabcode_markdown::CrabCodeOpenCodeHighlighter,
    ) -> CrabCodeMarkdownRender {
        let references = crate::crabcode_markdown::reference_definitions(source);
        let lines = markdown_lines_untracked(
            source,
            default_style,
            theme,
            width,
            media_paths,
            &references,
            source_byte_base,
            open_code,
        );
        CrabCodeMarkdownRender::from_lines(source, lines, 0, 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn markdown_lines_untracked(
        source: &str,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
        media_paths: &[PathBuf],
        references: &crate::crabcode_markdown::ReferenceDefinitions,
        source_byte_base: usize,
        open_code: &mut crate::crabcode_markdown::CrabCodeOpenCodeHighlighter,
    ) -> Vec<AnnotatedLine> {
        let source_lines = source.split('\n').collect::<Vec<_>>();
        let source_line_starts = source
            .split_inclusive('\n')
            .scan(0_usize, |offset, line| {
                let start = *offset;
                *offset = offset.saturating_add(line.len());
                Some(start)
            })
            .chain(std::iter::once(source.len()))
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut index = 0;
        let mut fence: Option<Fence> = None;
        while index < source_lines.len() {
            let line = source_lines[index];
            if let Some(open) = fence.as_mut() {
                if is_closing_fence(line, open) {
                    output.push(
                        AnnotatedLine::plain(Line::styled(
                            "└─",
                            Style::default().fg(theme.gray_dim),
                        ))
                        .with_source_line(index),
                    );
                    fence = None;
                } else {
                    let line_start = source_line_starts
                        .get(index)
                        .copied()
                        .unwrap_or(source.len());
                    output.push(
                        AnnotatedLine::plain(code_line(
                            line,
                            open,
                            default_style,
                            theme,
                            source_byte_base.saturating_add(line_start),
                            index + 1 < source_lines.len(),
                            open_code,
                        ))
                        .with_source_line(index),
                    );
                }
                index += 1;
                continue;
            }

            if let Some(open) = opening_fence(line) {
                if is_mermaid_fence(&open)
                    && let Some(close_index) =
                        closed_fence_index(&source_lines, index.saturating_add(1), &open)
                {
                    let source = source_lines[index.saturating_add(1)..close_index].join("\n");
                    let styles = crate::crabcode_mermaid::MermaidStyles {
                        border: Style::default().fg(theme.gray_dim),
                        node_text: Style::default()
                            .fg(theme.markdown_code)
                            .add_modifier(Modifier::BOLD),
                        edge: Style::default().fg(theme.accent_assistant),
                        edge_label: Style::default().fg(theme.accent_system),
                        title: Style::default()
                            .fg(theme.markdown_code)
                            .add_modifier(Modifier::BOLD),
                    };
                    if let Some(art) =
                        crate::crabcode_mermaid::render(&source, &styles, Some(width))
                    {
                        let crate::crabcode_mermaid::MermaidArt {
                            styled_lines,
                            plain_lines,
                        } = art;
                        drop(plain_lines);
                        output.extend(
                            styled_lines
                                .into_iter()
                                .map(|line| AnnotatedLine::plain(line).with_source_line(index)),
                        );
                        index = close_index.saturating_add(1);
                        continue;
                    }
                }
                let label = if open.language.is_empty() {
                    "code"
                } else {
                    open.language.as_str()
                };
                output.push(
                    AnnotatedLine::plain(Line::from(vec![
                        Span::styled("┌─ ", Style::default().fg(theme.gray_dim)),
                        Span::styled(
                            label.to_string(),
                            Style::default()
                                .fg(theme.markdown_code)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]))
                    .with_source_line(index),
                );
                fence = Some(open);
                index += 1;
                continue;
            }

            if index + 1 < source_lines.len()
                && !line.trim().is_empty()
                && let Some(level) = setext_heading_level(source_lines[index + 1])
            {
                let resolved =
                    crate::crabcode_markdown::resolve_reference_links(line.trim(), references);
                output.push(
                    styled_inline_line(
                        &resolved,
                        heading_style(level, theme),
                        theme,
                        "",
                        None,
                        media_paths,
                    )
                    .with_source_line(index),
                );
                index += 2;
                continue;
            }

            if index + 1 < source_lines.len()
                && line.contains('|')
                && is_table_separator(source_lines[index + 1])
            {
                let table_start = index;
                let mut rows = vec![parse_table_row(line)];
                index += 2;
                while index < source_lines.len()
                    && source_lines[index].contains('|')
                    && !source_lines[index].trim().is_empty()
                {
                    rows.push(parse_table_row(source_lines[index]));
                    index += 1;
                }
                output.extend(render_table(
                    rows,
                    width,
                    default_style,
                    theme,
                    media_paths,
                    references,
                    table_start,
                ));
                continue;
            }

            if crate::crabcode_markdown::is_reference_definition(line) {
                output.push(AnnotatedLine::plain(Line::default()).with_source_line(index));
            } else {
                let resolved = crate::crabcode_markdown::resolve_reference_links(line, references);
                output.push(
                    markdown_block_line(&resolved, default_style, theme, width, media_paths)
                        .with_source_line(index),
                );
            }
            index += 1;
        }
        output
    }

    fn is_mermaid_fence(fence: &Fence) -> bool {
        fence
            .language
            .split_ascii_whitespace()
            .next()
            .is_some_and(|language| language.eq_ignore_ascii_case("mermaid"))
    }

    fn closed_fence_index(source_lines: &[&str], start: usize, fence: &Fence) -> Option<usize> {
        source_lines
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, line)| is_closing_fence(line, fence).then_some(index))
    }

    fn opening_fence(line: &str) -> Option<Fence> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next()?;
        if !matches!(marker, '`' | '~') {
            return None;
        }
        let length = trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count();
        if length < 3 {
            return None;
        }
        let marker_bytes = marker.len_utf8().saturating_mul(length);
        let language = trimmed.get(marker_bytes..)?.trim().to_string();
        if marker == '`' && language.contains('`') {
            return None;
        }
        Some(Fence {
            marker,
            length,
            language,
            body: String::new(),
            body_start_in_source: 0,
        })
    }

    fn is_closing_fence(line: &str, fence: &Fence) -> bool {
        let trimmed = line.trim();
        let count = trimmed
            .chars()
            .take_while(|character| *character == fence.marker)
            .count();
        count >= fence.length && trimmed.chars().skip(count).all(char::is_whitespace)
    }

    fn code_line(
        line: &str,
        fence: &mut Fence,
        default_style: Style,
        theme: CrabCodeTheme,
        source_start: usize,
        has_newline: bool,
        open_code: &mut crate::crabcode_markdown::CrabCodeOpenCodeHighlighter,
    ) -> Line<'static> {
        let language = fence
            .language
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if fence.body.is_empty() {
            fence.body_start_in_source = source_start;
        }
        fence.body.push_str(line);
        if has_newline {
            fence.body.push('\n');
        }
        let style = if matches!(language.as_str(), "diff" | "patch") {
            if line.starts_with('+') && !line.starts_with("+++") {
                Style::default()
                    .fg(theme.accent_success)
                    .bg(theme.diff_insert_bg)
            } else if line.starts_with('-') && !line.starts_with("---") {
                Style::default()
                    .fg(theme.accent_error)
                    .bg(theme.diff_delete_bg)
            } else if line.starts_with("@@") {
                Style::default()
                    .fg(theme.accent_system)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                Style::default()
                    .fg(theme.command)
                    .add_modifier(Modifier::BOLD)
            } else {
                default_style
            }
        } else {
            Style::default()
                .fg(theme.markdown_code)
                .bg(theme.markdown_code_bg)
        };
        if !matches!(language.as_str(), "diff" | "patch") {
            let highlighted =
                open_code.highlight(&fence.language, fence.body_start_in_source, &fence.body);
            if let Some(highlighted_line) = highlighted.last() {
                let color_level = crate::crabcode_markdown::detected_terminal_color_level();
                let mut spans = vec![Span::styled("│ ", Style::default().fg(theme.gray_dim))];
                spans.extend(highlighted_line.iter().map(|span| {
                    let token_style = match span.class {
                        crate::crabcode_markdown::CodeTokenClass::Plain => {
                            Style::default().fg(theme.markdown_code)
                        }
                        crate::crabcode_markdown::CodeTokenClass::Key => Style::default()
                            .fg(theme.command)
                            .add_modifier(Modifier::BOLD),
                        crate::crabcode_markdown::CodeTokenClass::String => {
                            Style::default().fg(theme.accent_success)
                        }
                        crate::crabcode_markdown::CodeTokenClass::Number => {
                            Style::default().fg(theme.accent_system)
                        }
                        crate::crabcode_markdown::CodeTokenClass::Comment => Style::default()
                            .fg(theme.gray)
                            .add_modifier(Modifier::ITALIC),
                        crate::crabcode_markdown::CodeTokenClass::Keyword => Style::default()
                            .fg(theme.accent_assistant)
                            .add_modifier(Modifier::BOLD),
                    };
                    let adapted = token_style.fg.and_then(|color| {
                        crate::crabcode_markdown::adapt_color(color, color_level)
                    });
                    let mut adapted_style = token_style.bg(theme.markdown_code_bg);
                    adapted_style.fg = adapted;
                    Span::styled(span.text.clone(), adapted_style)
                }));
                return Line::from(spans);
            }
        }
        let mut gutter_style = Style::default().fg(theme.gray_dim);
        if matches!(language.as_str(), "diff" | "patch")
            && (line.starts_with('+') && !line.starts_with("+++")
                || line.starts_with('-') && !line.starts_with("---"))
        {
            gutter_style.bg = style.bg;
        }
        Line::from(vec![
            Span::styled("│ ", gutter_style),
            Span::styled(line.to_string(), style),
        ])
    }

    fn setext_heading_level(line: &str) -> Option<usize> {
        let trimmed = line.trim();
        if trimmed.len() < 3 {
            return None;
        }
        if trimmed.chars().all(|character| character == '=') {
            Some(1)
        } else if trimmed.chars().all(|character| character == '-') {
            Some(2)
        } else {
            None
        }
    }

    pub(super) fn heading_style(level: usize, theme: CrabCodeTheme) -> Style {
        let color = match level {
            1 => theme.markdown_h1,
            2 => theme.markdown_h2,
            _ => theme.markdown_h3,
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }

    fn markdown_block_line(
        line: &str,
        default_style: Style,
        theme: CrabCodeTheme,
        width: usize,
        media_paths: &[PathBuf],
    ) -> AnnotatedLine {
        let trimmed_start = line.trim_start();
        let leading = &line[..line.len().saturating_sub(trimmed_start.len())];
        if let Some(math_source) = display_math_source(trimmed_start)
            && let Some(rendered) = crate::crabcode_markdown::latex_to_unicode_display(math_source)
            && !rendered.is_empty()
        {
            return AnnotatedLine::plain(Line::styled(
                format!("{leading}  {}", rendered.join("; ")),
                Style::default()
                    .fg(theme.accent_system)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        if let Some((level, content)) = atx_heading(trimmed_start) {
            return styled_inline_line(
                content,
                heading_style(level, theme),
                theme,
                leading,
                None,
                media_paths,
            );
        }
        if is_thematic_break(trimmed_start) {
            return AnnotatedLine::plain(Line::styled(
                format!(
                    "{leading}{}",
                    "─".repeat(width.saturating_sub(leading.width()).clamp(1, 48))
                ),
                Style::default().fg(theme.gray_dim),
            ));
        }

        let (quote_depth, quoted) = strip_blockquotes(trimmed_start);
        if quote_depth > 0 {
            let mut prefix = leading.to_string();
            prefix.push_str(&"▎ ".repeat(quote_depth));
            return styled_inline_line(
                quoted,
                Style::default()
                    .fg(theme.gray_bright)
                    .add_modifier(Modifier::ITALIC),
                theme,
                &prefix,
                Some(theme.accent_assistant),
                media_paths,
            );
        }

        if let Some((marker, content, marker_color)) = list_item(trimmed_start, theme) {
            let mut prefix = leading.to_string();
            prefix.push_str(&marker);
            return styled_inline_line(
                content,
                default_style,
                theme,
                &prefix,
                Some(marker_color),
                media_paths,
            );
        }

        if leading.width() >= 4 {
            return AnnotatedLine::plain(Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.gray_dim)),
                Span::styled(
                    trimmed_start.to_string(),
                    Style::default()
                        .fg(theme.markdown_code)
                        .bg(theme.markdown_code_bg),
                ),
            ]));
        }

        styled_inline_line(line, default_style, theme, "", None, media_paths)
    }

    fn display_math_source(line: &str) -> Option<&str> {
        let trimmed = line.trim();
        let inner = trimmed.strip_prefix("$$")?.strip_suffix("$$")?;
        (!inner.is_empty()).then_some(inner)
    }

    pub(super) fn atx_heading(line: &str) -> Option<(usize, &str)> {
        let level = line
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if !(1..=6).contains(&level) {
            return None;
        }
        let rest = line.get(level..)?;
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return None;
        }
        Some((level, rest.trim().trim_end_matches('#').trim_end()))
    }

    fn is_thematic_break(line: &str) -> bool {
        let compact = line
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if compact.len() < 3 {
            return false;
        }
        let Some(marker) = compact.chars().next() else {
            return false;
        };
        matches!(marker, '-' | '*' | '_') && compact.chars().all(|character| character == marker)
    }

    fn strip_blockquotes(mut line: &str) -> (usize, &str) {
        let mut depth = 0;
        while let Some(rest) = line.strip_prefix('>') {
            depth += 1;
            line = rest.strip_prefix(' ').unwrap_or(rest);
        }
        (depth, line)
    }

    fn list_item(line: &str, theme: CrabCodeTheme) -> Option<(String, &str, Color)> {
        for marker in ["- ", "* ", "+ "] {
            if let Some(mut rest) = line.strip_prefix(marker) {
                if let Some(task) = rest.get(..3) {
                    if task.eq_ignore_ascii_case("[x]") {
                        rest = rest.get(3..)?.strip_prefix(' ').unwrap_or(&rest[3..]);
                        return Some(("✓ ".to_string(), rest, theme.accent_success));
                    }
                    if task == "[ ]" {
                        rest = rest.get(3..)?.strip_prefix(' ').unwrap_or(&rest[3..]);
                        return Some(("○ ".to_string(), rest, theme.gray));
                    }
                }
                return Some(("• ".to_string(), rest, theme.accent_assistant));
            }
        }

        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            let suffix = line.get(digits..)?;
            if let Some(rest) = suffix
                .strip_prefix(". ")
                .or_else(|| suffix.strip_prefix(") "))
            {
                return Some((
                    format!("{} ", &line[..digits + 1]),
                    rest,
                    theme.accent_assistant,
                ));
            }
        }
        None
    }

    fn styled_inline_line(
        content: &str,
        style: Style,
        theme: CrabCodeTheme,
        prefix: &str,
        prefix_color: Option<Color>,
        media_paths: &[PathBuf],
    ) -> AnnotatedLine {
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(Span::styled(
                prefix.to_string(),
                Style::default().fg(prefix_color.unwrap_or(theme.gray)),
            ));
        }
        let prefix_bytes = prefix.len();
        let mut inline = inline_markdown_with_links(content, style, theme, media_paths);
        for link in &mut inline.links {
            link.start_byte = link.start_byte.saturating_add(prefix_bytes);
            link.end_byte = link.end_byte.saturating_add(prefix_bytes);
        }
        spans.append(&mut inline.spans);
        AnnotatedLine {
            line: Line::from(spans),
            links: inline.links,
            source_line: 0,
        }
    }

    fn is_table_separator(line: &str) -> bool {
        let cells = parse_table_row(line);
        !cells.is_empty()
            && cells.iter().all(|cell| {
                let trimmed = cell.trim().trim_matches(':');
                !trimmed.is_empty() && trimmed.chars().all(|character| character == '-')
            })
    }

    fn parse_table_row(line: &str) -> Vec<String> {
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        let mut cells = Vec::new();
        let mut cell = String::new();
        let mut escaped = false;
        let mut code_ticks = 0_usize;
        for character in trimmed.chars() {
            if escaped {
                cell.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '`' {
                code_ticks ^= 1;
                cell.push(character);
            } else if character == '|' && code_ticks == 0 {
                cells.push(cell.trim().to_string());
                cell.clear();
            } else {
                cell.push(character);
            }
        }
        if escaped {
            cell.push('\\');
        }
        cells.push(cell.trim().to_string());
        cells
    }

    fn render_table(
        mut rows: Vec<Vec<String>>,
        width: usize,
        default_style: Style,
        theme: CrabCodeTheme,
        media_paths: &[PathBuf],
        references: &crate::crabcode_markdown::ReferenceDefinitions,
        source_start_line: usize,
    ) -> Vec<AnnotatedLine> {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let table_overhead = columns.saturating_mul(3).saturating_add(1);
        if columns == 0 || columns > 64 || width <= table_overhead {
            return rows
                .into_iter()
                .enumerate()
                .map(|(row_index, row)| {
                    let source_line = source_start_line.saturating_add(if row_index == 0 {
                        0
                    } else {
                        row_index + 1
                    });
                    let resolved = crate::crabcode_markdown::resolve_reference_links(
                        &format!("| {} |", row.join(" | ")),
                        references,
                    );
                    styled_inline_line(&resolved, default_style, theme, "", None, media_paths)
                        .with_source_line(source_line)
                })
                .collect();
        }
        for row in &mut rows {
            row.resize(columns, String::new());
        }
        let mut column_widths = (0..columns)
            .map(|column| {
                rows.iter()
                    .map(|row| row[column].width())
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect::<Vec<_>>();
        let available = width.saturating_sub(table_overhead);
        while column_widths.iter().sum::<usize>() > available {
            let Some((largest, _)) = column_widths
                .iter()
                .enumerate()
                .filter(|(_, width)| **width > 1)
                .max_by_key(|(_, width)| **width)
            else {
                break;
            };
            column_widths[largest] -= 1;
        }

        let border_style = Style::default().fg(theme.gray_dim);
        let border = |left: char, middle: char, right: char, source_line: usize| {
            let mut text = String::new();
            text.push(left);
            for (index, width) in column_widths.iter().enumerate() {
                text.push_str(&"─".repeat(width.saturating_add(2)));
                text.push(if index + 1 == columns { right } else { middle });
            }
            AnnotatedLine::plain(Line::styled(text, border_style)).with_source_line(source_line)
        };
        let mut output = vec![border('┌', '┬', '┐', source_start_line)];
        let body_row_count = rows.len().saturating_sub(1);
        for (row_index, row) in rows.into_iter().enumerate() {
            let cell_style = if row_index == 0 {
                default_style.add_modifier(Modifier::BOLD)
            } else {
                default_style
            };
            let wrapped_cells = row
                .into_iter()
                .enumerate()
                .map(|(column, cell)| {
                    let resolved =
                        crate::crabcode_markdown::resolve_reference_links(&cell, references);
                    let inline =
                        inline_markdown_with_links(&resolved, cell_style, theme, media_paths);
                    wrap_annotated_line(
                        AnnotatedLine {
                            line: Line::from(inline.spans),
                            links: inline.links,
                            source_line: 0,
                        },
                        column_widths[column],
                        media_paths,
                    )
                })
                .collect::<Vec<_>>();
            let row_height = wrapped_cells
                .iter()
                .map(|cell| cell.lines.len())
                .max()
                .unwrap_or(1)
                .max(1);
            let source_line =
                source_start_line.saturating_add(if row_index == 0 { 0 } else { row_index + 1 });
            for visual_row in 0..row_height {
                let mut spans = vec![Span::styled("│", border_style)];
                let mut links = Vec::new();
                for (column, cell) in wrapped_cells.iter().enumerate() {
                    let cell_line = cell.lines.get(visual_row).cloned().unwrap_or_default();
                    let fitted = fit_line_to_width(cell_line, column_widths[column]);
                    let cell_text = fitted
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    spans.push(Span::raw(" "));
                    let cell_offset = spans.iter().map(|span| span.content.len()).sum::<usize>();
                    for group in &cell.link_groups {
                        for fragment in group
                            .fragments
                            .iter()
                            .filter(|fragment| fragment.row == visual_row)
                        {
                            let start =
                                byte_offset_at_display_column(&cell_text, fragment.start_column);
                            let end =
                                byte_offset_at_display_column(&cell_text, fragment.end_column);
                            if start < end {
                                links.push(LogicalLinkRange {
                                    target: group.target.clone(),
                                    start_byte: cell_offset.saturating_add(start),
                                    end_byte: cell_offset.saturating_add(end),
                                    semantic_id: None,
                                });
                            }
                        }
                    }
                    spans.extend(fitted.spans);
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled("│", border_style));
                }
                output.push(AnnotatedLine {
                    line: Line::from(spans),
                    links,
                    source_line,
                });
            }
            if row_index == 0 {
                output.push(border('├', '┼', '┤', source_start_line.saturating_add(1)));
            }
        }
        output.push(border(
            '└',
            '┴',
            '┘',
            source_start_line.saturating_add(body_row_count + 1),
        ));
        output
    }

    fn byte_offset_at_display_column(text: &str, target: usize) -> usize {
        let mut width = 0_usize;
        for (offset, grapheme) in text.grapheme_indices(true) {
            if width >= target {
                return offset;
            }
            width = width.saturating_add(grapheme.width());
            if width >= target {
                return offset.saturating_add(grapheme.len());
            }
        }
        text.len()
    }

    #[derive(Debug, Default)]
    pub(super) struct AnnotatedInline {
        pub(super) spans: Vec<Span<'static>>,
        pub(super) links: Vec<LogicalLinkRange>,
    }

    #[cfg(test)]
    pub(super) fn inline_markdown(
        input: &str,
        style: Style,
        theme: CrabCodeTheme,
    ) -> Vec<Span<'static>> {
        inline_markdown_with_links(input, style, theme, &[]).spans
    }

    pub(super) fn inline_markdown_with_links(
        input: &str,
        style: Style,
        theme: CrabCodeTheme,
        media_paths: &[PathBuf],
    ) -> AnnotatedInline {
        let mut spans = Vec::new();
        let mut links = Vec::new();
        parse_inline(input, style, theme, 0, media_paths, &mut spans, &mut links);
        if spans.is_empty() {
            spans.push(Span::styled(String::new(), style));
        }
        AnnotatedInline { spans, links }
    }

    fn parse_inline(
        input: &str,
        style: Style,
        theme: CrabCodeTheme,
        depth: usize,
        media_paths: &[PathBuf],
        spans: &mut Vec<Span<'static>>,
        links: &mut Vec<LogicalLinkRange>,
    ) {
        if depth >= 16 {
            push_span(spans, input, style);
            return;
        }
        let mut plain = String::new();
        let mut index = 0;
        while index < input.len() {
            let remainder = &input[index..];
            if let Some(after_escape) = remainder.strip_prefix('\\')
                && let Some(character) = after_escape.chars().next()
            {
                plain.push(character);
                index += 1 + character.len_utf8();
                continue;
            }

            if let Some((consumed, label, target, image)) = parse_link(remainder) {
                flush_plain(spans, &mut plain, style);
                let link_style = style
                    .fg(theme.link)
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
                if image {
                    push_span(spans, "🖼 ", Style::default().fg(theme.accent_system));
                }
                let link_start = rendered_span_bytes(spans);
                let nested_link_start = links.len();
                parse_inline(
                    label,
                    link_style,
                    theme,
                    depth + 1,
                    media_paths,
                    spans,
                    links,
                );
                // Pinned-upstream pretty rendering has two independent targets:
                // the parser-produced target covers only the label, while the
                // plain URL/path scan below discovers the displayed `(target)`
                // suffix. Keeping this boundary exact also preserves navigation
                // parity when the two targets share the same destination.
                let link_end = rendered_span_bytes(spans);
                if !target.is_empty() {
                    push_span(
                        spans,
                        &format!(" ({target})"),
                        Style::default()
                            .fg(theme.link)
                            .add_modifier(Modifier::UNDERLINED | Modifier::DIM),
                    );
                }
                if let Some(target) = semantic_markdown_target(target, media_paths)
                    && link_start < link_end
                {
                    links.truncate(nested_link_start);
                    links.push(LogicalLinkRange {
                        target,
                        start_byte: link_start,
                        end_byte: link_end,
                        semantic_id: None,
                    });
                }
                index += consumed;
                continue;
            }

            if remainder.starts_with('`') {
                let ticks = remainder.bytes().take_while(|byte| *byte == b'`').count();
                let delimiter = "`".repeat(ticks);
                if let Some(end) = remainder[ticks..].find(&delimiter) {
                    flush_plain(spans, &mut plain, style);
                    let content_start = ticks;
                    let content_end = ticks + end;
                    push_span(
                        spans,
                        &remainder[content_start..content_end],
                        Style::default()
                            .fg(theme.markdown_code)
                            .bg(theme.markdown_code_bg),
                    );
                    index += content_end + ticks;
                    continue;
                }
            }

            if remainder.starts_with('$')
                && !remainder.starts_with("$$")
                && let Some(end) = remainder[1..].find('$').map(|offset| offset + 1)
            {
                let math_source = &remainder[1..end];
                let valid_flanking = !math_source.is_empty()
                    && !math_source.chars().next().is_some_and(char::is_whitespace)
                    && !math_source
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace);
                if valid_flanking
                    && let Some(rendered) =
                        crate::crabcode_markdown::latex_to_unicode_inline(math_source)
                {
                    flush_plain(spans, &mut plain, style);
                    push_span(
                        spans,
                        &rendered,
                        Style::default()
                            .fg(theme.accent_system)
                            .add_modifier(Modifier::ITALIC),
                    );
                    index += end + 1;
                    continue;
                }
            }

            let mut matched_delimiter = false;
            for (delimiter, modifier) in [
                ("**", Modifier::BOLD),
                ("__", Modifier::BOLD),
                ("~~", Modifier::CROSSED_OUT),
                ("*", Modifier::ITALIC),
                ("_", Modifier::ITALIC),
            ] {
                let Some(after) = remainder.strip_prefix(delimiter) else {
                    continue;
                };
                let Some(end) = after.find(delimiter) else {
                    continue;
                };
                if end == 0 {
                    continue;
                }
                flush_plain(spans, &mut plain, style);
                parse_inline(
                    &after[..end],
                    style.add_modifier(modifier),
                    theme,
                    depth + 1,
                    media_paths,
                    spans,
                    links,
                );
                index += delimiter.len() + end + delimiter.len();
                matched_delimiter = true;
                break;
            }
            if matched_delimiter {
                continue;
            }

            if let Some((consumed, url)) = parse_autolink(remainder) {
                flush_plain(spans, &mut plain, style);
                let link_start = rendered_span_bytes(spans);
                push_span(
                    spans,
                    url,
                    style.fg(theme.link).add_modifier(Modifier::UNDERLINED),
                );
                let link_end = rendered_span_bytes(spans);
                if let Some(target) = semantic_markdown_target(url, media_paths) {
                    links.push(LogicalLinkRange {
                        target,
                        start_byte: link_start,
                        end_byte: link_end,
                        semantic_id: None,
                    });
                }
                index += consumed;
                continue;
            }

            if let Some(url) = bare_url_prefix(remainder) {
                flush_plain(spans, &mut plain, style);
                let link_start = rendered_span_bytes(spans);
                push_span(
                    spans,
                    url,
                    style.fg(theme.link).add_modifier(Modifier::UNDERLINED),
                );
                let link_end = rendered_span_bytes(spans);
                if let Some(target) = semantic_markdown_target(url, media_paths) {
                    links.push(LogicalLinkRange {
                        target,
                        start_byte: link_start,
                        end_byte: link_end,
                        semantic_id: None,
                    });
                }
                index += url.len();
                continue;
            }

            if let Some((consumed, decoded)) = decode_safe_named_entity(remainder) {
                plain.push(decoded);
                index += consumed;
                continue;
            }

            let character = remainder
                .chars()
                .next()
                .expect("index remains on a UTF-8 boundary");
            plain.push(character);
            index += character.len_utf8();
        }
        flush_plain(spans, &mut plain, style);
    }

    fn rendered_span_bytes(spans: &[Span<'_>]) -> usize {
        spans.iter().map(|span| span.content.len()).sum()
    }

    fn parse_link(input: &str) -> Option<(usize, &str, &str, bool)> {
        let (image, label_start) = if input.starts_with("![") {
            (true, 2)
        } else if input.starts_with('[') {
            (false, 1)
        } else {
            return None;
        };
        let label_end = input[label_start..].find("](")? + label_start;
        let target_start = label_end + 2;
        let target_end = matching_parenthesis(input, target_start)?;
        let raw_target = input[target_start..target_end].trim();
        let target = if let Some(angle) = raw_target.strip_prefix('<') {
            angle.strip_suffix('>')?
        } else {
            raw_target
                .split_ascii_whitespace()
                .next()
                .unwrap_or_default()
        };
        Some((
            target_end + 1,
            &input[label_start..label_end],
            target,
            image,
        ))
    }

    fn matching_parenthesis(input: &str, start: usize) -> Option<usize> {
        let mut depth = 1_usize;
        let mut escaped = false;
        for (offset, character) in input[start..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '(' => depth = depth.saturating_add(1),
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(start + offset);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn parse_autolink(input: &str) -> Option<(usize, &str)> {
        let inner = input.strip_prefix('<')?;
        let end = inner.find('>')?;
        let target = &inner[..end];
        is_allowed_link_scheme(target).then_some((end + 2, target))
    }

    fn bare_url_prefix(input: &str) -> Option<&str> {
        if !is_allowed_link_scheme(input) {
            return None;
        }
        let end = input
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '<' | '>' | '"')
            })
            .unwrap_or(input.len());
        let candidate = &input[..end];
        let trimmed = candidate.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        (!trimmed.is_empty()).then_some(trimmed)
    }

    fn is_allowed_link_scheme(value: &str) -> bool {
        ["https://", "http://", "file://", "mailto:"]
            .iter()
            .any(|scheme| value.starts_with(scheme))
    }

    fn decode_safe_named_entity(input: &str) -> Option<(usize, char)> {
        [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
            ("&#39;", '\''),
            ("&#x27;", '\''),
        ]
        .into_iter()
        .find_map(|(encoded, decoded)| {
            input
                .starts_with(encoded)
                .then_some((encoded.len(), decoded))
        })
    }

    fn flush_plain(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
        if plain.is_empty() {
            return;
        }
        let text = std::mem::take(plain);
        push_span(spans, &text, style);
    }

    fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
        } else {
            spans.push(Span::styled(text.to_string(), style));
        }
    }
}

#[cfg(test)]
use legacy_markdown_test_support::*;

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) -> Option<(u16, u16)> {
    let language = app.ui_language();
    let identity_color = app
        .retained_commands
        .color_value()
        .and_then(|color| color.background(app.renderer_theme_kind()))
        .map(color_support::quantize);
    let identity_border = app
        .retained_commands
        .banner_visible()
        .then_some(identity_color.unwrap_or(theme.background_accent));
    app.composer_attachment_hitboxes.clear();
    if !app.composer_enabled() {
        app.composer_area = Rect::default();
        frame.render_widget(
            Paragraph::new(Line::styled(
                if app.fatal.is_some() {
                    language.text(
                        " 输入框已锁定：协议已停止 ",
                        " Composer locked: protocol stopped ",
                    )
                } else {
                    language.text(
                        " 弹窗处理期间输入框已锁定 ",
                        " Composer locked while a modal request is active ",
                    )
                },
                Style::default().fg(theme.gray),
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.prompt_border)),
            ),
            area,
        );
        return None;
    }
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(identity_border.unwrap_or(theme.prompt_border_active)))
        .style(Style::default().bg(theme.bg_dark))
        .title(Span::styled(
            if crate::terminal_capabilities::terminal_context().shift_enter_unavailable() {
                if app.busy() {
                    language.text(
                        " 输入 · Enter 加入队列 · Alt-Enter 换行 ",
                        " Composer · Enter queue · Alt-Enter newline ",
                    )
                } else {
                    language.text(
                        " 输入 · Enter 发送 · Alt-Enter 换行 ",
                        " Composer · Enter send · Alt-Enter newline ",
                    )
                }
            } else if app.busy() {
                language.text(
                    " 输入 · Enter 加入队列 · Shift/Alt-Enter 换行 ",
                    " Composer · Enter queue · Shift/Alt-Enter newline ",
                )
            } else {
                language.text(
                    " 输入 · Enter 发送 · Shift/Alt-Enter 换行 ",
                    " Composer · Enter send · Shift/Alt-Enter newline ",
                )
            },
            Style::default().fg(theme.gray_bright),
        ));
    if let Some(name) = app.retained_commands.name() {
        let badge_color = identity_border.unwrap_or(theme.background_accent);
        let inverse =
            crate::retained_command_surface::historical_inverse_text(app.renderer_theme_kind())
                .map(color_support::quantize)
                .unwrap_or(theme.bg_base);
        block = block.title_bottom(Span::styled(
            format!(" {} ", sanitize_bounded_terminal_text(name)),
            Style::default()
                .fg(inverse)
                .bg(badge_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let inner = block.inner(area).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, area);
    let attachment_height = u16::from(!app.attachments.is_empty()).min(inner.height);
    let [attachments, editor] =
        Layout::vertical([Constraint::Length(attachment_height), Constraint::Min(1)]).areas(inner);
    if !app.attachments.is_empty() {
        let prefix = format!(
            "{}{}",
            app.attachments.len(),
            language.text(" 张图片： ", " image(s): ")
        );
        let mut column = prefix.width();
        let mut spans = vec![Span::styled(
            prefix,
            Style::default().fg(theme.accent_system),
        )];
        for (index, image) in app.attachments.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(", ", Style::default().fg(theme.accent_system)));
                column = column.saturating_add(2);
            }
            let filename = sanitize_bounded_terminal_text(&image.filename);
            let chip = format!(
                "[{} #{} {}]",
                language.text("图片", "Image"),
                index + 1,
                filename
            );
            let chip_width = chip.width();
            let clipped_start = column.min(usize::from(attachments.width));
            let clipped_end = column
                .saturating_add(chip_width)
                .min(usize::from(attachments.width));
            if clipped_start < clipped_end {
                app.composer_attachment_hitboxes.push((
                    Rect::new(
                        attachments.x.saturating_add(clipped_start as u16),
                        attachments.y,
                        (clipped_end - clipped_start) as u16,
                        1,
                    ),
                    index,
                ));
            }
            spans.push(Span::styled(
                chip,
                Style::default()
                    .fg(theme.accent_system)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            column = column.saturating_add(chip_width);
        }
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::from(spans),
                usize::from(attachments.width),
            )),
            attachments,
        );
    }
    app.composer_area = editor;
    frame.render_stateful_widget_ref(&app.composer, editor, &mut app.composer_state);
    app.composer_focused()
        .then(|| {
            app.composer
                .cursor_pos_with_state(editor, app.composer_state)
        })
        .flatten()
}

fn goal_status_height(app: &TuiApp) -> u16 {
    u16::from(app.active_goal().is_some())
        .saturating_add(u16::from(app.processing_background_task()))
}

fn goal_verdict_label(language: UiLanguage, verdict: GoalVerdict) -> &'static str {
    if matches!(language, UiLanguage::EnUs) {
        return verdict.label();
    }
    match verdict {
        GoalVerdict::Pass => "通过",
        GoalVerdict::Fail => "失败",
        GoalVerdict::Partial => "部分通过",
        GoalVerdict::Blocked => "受阻",
    }
}

fn goal_task_state_label(language: UiLanguage, state: GoalTaskState) -> &'static str {
    if matches!(language, UiLanguage::EnUs) {
        return state.label();
    }
    match state {
        GoalTaskState::Executing => "执行中",
        GoalTaskState::Completed => "已完成",
        GoalTaskState::Failed => "失败",
        GoalTaskState::Stopped => "已停止",
    }
}

fn goal_phase_label(language: UiLanguage, ordinal: usize) -> &'static str {
    match ordinal {
        1 => language.text("验收标准", "criteria"),
        2 => language.text("实施", "implement"),
        3 => language.text("验证", "verify"),
        4 => language.text("抽查", "spotcheck"),
        _ => "",
    }
}

fn render_goal_status(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: CrabCodeTheme) {
    if area.is_empty() {
        return;
    }
    let language = app.ui_language();
    let columns = usize::from(area.width);
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    let mut rows = Vec::new();
    if let Some(goal) = app.active_goal() {
        if goal.completed_sequence.is_some() {
            let verdict = goal
                .verdict
                .map(|verdict| goal_verdict_label(language, verdict));
            let budget = columns
                .saturating_sub(28)
                .clamp(8.min(columns), 48.min(columns));
            let summary = goal.summary.as_deref().unwrap_or(&goal.text);
            let summary = truncate_str(&sanitize_bounded_terminal_text(summary), budget);
            let color = match goal.verdict {
                Some(GoalVerdict::Fail | GoalVerdict::Blocked) => theme.accent_error,
                Some(GoalVerdict::Partial) => theme.warning,
                _ => theme.accent_success,
            };
            rows.push(Line::from(vec![
                Span::styled(
                    format!("  {} {} ", check_mark(), language.text("目标", "Goal")),
                    dim,
                ),
                Span::styled(
                    verdict.map_or_else(
                        || language.text("已完成：", "done: ").to_string(),
                        |value| format!("· {value}: "),
                    ),
                    dim,
                ),
                Span::styled(summary, Style::default().fg(color)),
            ]));
        } else {
            let phase = goal.phase.as_deref().and_then(|phase| {
                let label = phase.trim();
                (!label.is_empty()).then(|| match canonical_goal_phase_ordinal(phase) {
                    Some(ordinal) => {
                        format!(" · {ordinal}/4 {}", goal_phase_label(language, ordinal))
                    }
                    None => format!(" · {}", sanitize_bounded_terminal_text(label)),
                })
            });
            let phase = phase.unwrap_or_default();
            let budget = columns
                .saturating_sub(32)
                .saturating_sub(phase.width())
                .clamp(8.min(columns), 48.min(columns));
            let objective = truncate_str(&sanitize_bounded_terminal_text(&goal.text), budget);
            let verification = app.goal_verification();
            let suffix = match verification {
                GoalVerificationState::Verifying => {
                    format!(" · {}", language.text("正在验证…", "verifying…"))
                }
                GoalVerificationState::Verdict(_) => String::new(),
                GoalVerificationState::None => {
                    let executing = app
                        .goal_agent_tasks()
                        .into_iter()
                        .filter(|task| task.state == GoalTaskState::Executing)
                        .count();
                    if executing == 0 {
                        String::new()
                    } else if matches!(language, UiLanguage::ZhCn) {
                        format!(" · {executing} 个后台代理")
                    } else {
                        format!(
                            " · {executing} agent{} in background",
                            if executing == 1 { "" } else { "s" }
                        )
                    }
                }
            };
            let mut spans = vec![Span::styled(
                format!(
                    "  {} {}: {objective}{phase}{suffix}",
                    filled_dot(),
                    language.text("目标", "Goal")
                ),
                dim,
            )];
            if let GoalVerificationState::Verdict(verdict) = verification {
                spans.push(Span::styled(" · ", dim));
                spans.push(Span::styled(
                    format!(
                        "{}: {}",
                        language.text("结论", "VERDICT"),
                        goal_verdict_label(language, verdict)
                    ),
                    Style::default().fg(match verdict {
                        GoalVerdict::Pass => theme.accent_success,
                        GoalVerdict::Fail | GoalVerdict::Blocked => theme.accent_error,
                        GoalVerdict::Partial => theme.warning,
                    }),
                ));
            }
            rows.push(fit_line_to_width(Line::from(spans), columns));
        }
    }
    if app.processing_background_task() {
        rows.push(fit_line_to_width(
            Line::styled(
                language.text(
                    "  … 正在处理后台任务结果…",
                    "  … Processing background task results…",
                ),
                dim,
            ),
            columns,
        ));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

fn goal_console_height(app: &TuiApp, terminal_width: u16) -> u16 {
    if !app.goal_console_open() {
        return 0;
    }
    let width = terminal_width.saturating_sub(2).clamp(1, 100);
    u16::try_from(goal_console_lines(app, width.saturating_sub(2), app.renderer_theme()).len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

fn render_goal_console(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: CrabCodeTheme) {
    if area.is_empty() || !app.goal_console_open() {
        return;
    }
    let width = area.width.saturating_sub(2).clamp(1, 100);
    let panel = Rect::new(
        area.x.saturating_add(1),
        area.y,
        width.min(area.width.saturating_sub(1)),
        area.height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.gray_dim))
        .style(Style::default().bg(theme.bg_base));
    let inner = block.inner(panel);
    frame.render_widget(block, panel);
    frame.render_widget(
        Paragraph::new(goal_console_lines(app, inner.width, theme)),
        inner,
    );
}

fn goal_console_lines(app: &TuiApp, inner_width: u16, theme: CrabCodeTheme) -> Vec<Line<'static>> {
    let language = app.ui_language();
    let width = usize::from(inner_width.max(1));
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    let Some(goal) = app.active_goal() else {
        return vec![
            Line::styled(
                language.text("目标控制台", "Goal console"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                language.text(
                    "当前会话没有活动目标。运行 /goal <objective> 可开始目标。",
                    "No active goal in this session. Run /goal <objective> to start one.",
                ),
                dim,
            ),
            Line::styled(
                language.text("esc / ctrl+x ctrl+g 关闭", "esc / ctrl+x ctrl+g to close"),
                dim,
            ),
        ];
    };

    let mut lines = vec![
        Line::styled(
            format!(
                "{} {}: {}",
                chevron(),
                language.text("目标", "Goal"),
                truncate_str(
                    &sanitize_bounded_terminal_text(&goal.text),
                    width.saturating_sub(10).max(1)
                )
            ),
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Line::default(),
        Line::styled(language.text("阶段", "Phases"), dim),
    ];
    let current = goal
        .phase_history
        .iter()
        .filter_map(|entry| canonical_goal_phase_ordinal(&entry.phase))
        .max()
        .unwrap_or(0);
    for ordinal in 1..=4 {
        let done = goal.completed_sequence.is_some() || ordinal < current;
        let active = goal.completed_sequence.is_none() && ordinal == current;
        let marker = if done {
            check_mark()
        } else if active {
            filled_dot()
        } else {
            "○"
        };
        let color = if done {
            theme.accent_success
        } else if active {
            theme.accent_running
        } else {
            theme.text_secondary
        };
        let label = format!("{ordinal}/4 {}", goal_phase_label(language, ordinal));
        let detail = goal
            .phase_history
            .iter()
            .rev()
            .find(|entry| canonical_goal_phase_ordinal(&entry.phase) == Some(ordinal))
            .and_then(|entry| entry.detail.as_deref())
            .map(|detail| {
                format!(
                    " — {}",
                    truncate_str(
                        &sanitize_bounded_terminal_text(detail),
                        width.saturating_sub(label.width() + 5).max(1)
                    )
                )
            })
            .unwrap_or_default();
        lines.push(Line::styled(
            format!("{marker} {label}{detail}"),
            Style::default().fg(color).add_modifier(if done || active {
                Modifier::empty()
            } else {
                Modifier::DIM
            }),
        ));
    }

    let tasks = app.goal_agent_tasks();
    lines.push(Line::default());
    lines.push(Line::styled(
        if matches!(language, UiLanguage::ZhCn) {
            format!(
                "子代理 · {}",
                if tasks.is_empty() {
                    "无子代理".to_string()
                } else {
                    format!("{} 个子代理", tasks.len())
                }
            )
        } else {
            format!(
                "Subagents · {}",
                if tasks.is_empty() {
                    "no subagents".to_string()
                } else {
                    format!(
                        "{} subagent{}",
                        tasks.len(),
                        if tasks.len() == 1 { "" } else { "s" }
                    )
                }
            )
        },
        dim,
    ));
    if tasks.is_empty() {
        lines.push(Line::styled(
            language.text("● 无运行中的子代理", "● none running"),
            dim,
        ));
    } else {
        for task in tasks {
            let state = goal_task_state_label(language, task.state);
            let color = match task.state {
                GoalTaskState::Executing => theme.accent_running,
                GoalTaskState::Completed => theme.accent_success,
                GoalTaskState::Failed | GoalTaskState::Stopped => theme.accent_error,
            };
            let marker = if task.state == GoalTaskState::Executing {
                filled_dot()
            } else {
                check_mark()
            };
            let description = truncate_str(
                &sanitize_bounded_terminal_text(&task.description),
                width.saturating_sub(state.width() + 7).max(1),
            );
            lines.push(Line::styled(
                format!("{marker} {description} [{state}]"),
                Style::default().fg(color),
            ));
        }
    }

    lines.push(Line::default());
    if goal.completed_sequence.is_some() {
        lines.push(Line::styled(language.text("结果", "Result"), dim));
        let verdict = goal
            .verdict
            .map(|verdict| goal_verdict_label(language, verdict))
            .unwrap_or(language.text("已完成", "done"));
        let color = match goal.verdict {
            Some(GoalVerdict::Fail | GoalVerdict::Blocked) => theme.accent_error,
            Some(GoalVerdict::Partial) => theme.warning,
            _ => theme.accent_success,
        };
        let summary = goal
            .summary
            .as_deref()
            .map(|summary| {
                format!(
                    ": {}",
                    truncate_str(
                        &sanitize_bounded_terminal_text(summary),
                        width.saturating_sub(verdict.width() + 8).max(1)
                    )
                )
            })
            .unwrap_or_default();
        lines.push(Line::styled(
            format!(
                "{} {} {verdict}{summary}",
                check_mark(),
                language.text("目标", "Goal")
            ),
            Style::default().fg(color),
        ));
    } else {
        lines.push(Line::styled(language.text("验证", "Verification"), dim));
        let (text, color) = match app.goal_verification() {
            GoalVerificationState::Verifying => (
                format!(
                    "{} {}",
                    filled_dot(),
                    language.text("正在验证…", "verifying…")
                ),
                theme.accent_running,
            ),
            GoalVerificationState::Verdict(verdict) => (
                format!(
                    "{} {}: {}",
                    check_mark(),
                    language.text("结论", "VERDICT"),
                    goal_verdict_label(language, verdict)
                ),
                match verdict {
                    GoalVerdict::Pass => theme.accent_success,
                    GoalVerdict::Fail | GoalVerdict::Blocked => theme.accent_error,
                    GoalVerdict::Partial => theme.warning,
                },
            ),
            GoalVerificationState::None => (
                language.text("● 尚未开始", "● not started").to_string(),
                theme.text_secondary,
            ),
        };
        lines.push(Line::styled(text, Style::default().fg(color)));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        language.text("esc / ctrl+x ctrl+g 关闭", "esc / ctrl+x ctrl+g to close"),
        dim,
    ));
    lines
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp, theme: CrabCodeTheme) {
    let language = app.ui_language();
    let suggestion = app
        .projection
        .prompt_suggestion()
        .filter(|_| app.composer.is_empty())
        .map(|suggestion| {
            format!(
                "{}: {}",
                language.text("Tab 建议", "Tab"),
                sanitize_bounded_terminal_text(suggestion)
            )
        });
    let follow = if app.transcript_following {
        language.text("跟随", "follow")
    } else {
        language.text("已暂停", "paused")
    };
    let left = format!(
        " {} ",
        sanitize_bounded_terminal_text(&minimal_status_line(app))
    );
    let mut items = vec![StatusItem {
        id: "scroll",
        line: Line::styled(
            format!("{} {follow}", language.text("滚动", "scroll")),
            Style::default().fg(theme.gray_dim).bg(theme.bg_base),
        ),
    }];
    if let Some(suggestion) = suggestion {
        items.push(StatusItem {
            id: "suggestion",
            line: Line::styled(
                suggestion,
                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
            ),
        });
    }
    app.status_hit_areas = render_status_row(frame.buffer_mut(), area, theme, &left, items);
}

fn latest_unfinished_todos(app: &TuiApp) -> Vec<(&str, &str)> {
    let Some(todos) = app.projection.items().iter().rev().find_map(|item| {
        let tool = item.presentation.tool.as_ref()?;
        (tool.name.as_deref() == Some("TodoWrite"))
            .then(|| tool.input.as_ref()?.get("todos")?.as_array())
            .flatten()
    }) else {
        return Vec::new();
    };
    if todos.iter().all(|todo| {
        todo.get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| status == "completed")
    }) {
        return Vec::new();
    }
    todos
        .iter()
        .filter_map(|todo| {
            Some((
                todo.get("content")?.as_str()?,
                todo.get("status")?.as_str()?,
            ))
        })
        .collect()
}

enum WorkPanel {
    Tasks {
        snapshot: Arc<TaskPanelSnapshot>,
        degraded: bool,
    },
    Todos(Vec<(String, String)>),
    Hidden,
}

fn work_panel_state(app: &TuiApp) -> WorkPanel {
    let task_state = app.task_panel_projection_state();
    if task_state.degraded {
        return WorkPanel::Tasks {
            snapshot: task_state.snapshot.unwrap_or_default(),
            degraded: true,
        };
    }
    if let Some(snapshot) = task_state.snapshot {
        if !snapshot.is_empty() {
            return if snapshot.has_unfinished() {
                WorkPanel::Tasks {
                    snapshot,
                    degraded: false,
                }
            } else {
                WorkPanel::Hidden
            };
        }
    }
    let todos = latest_unfinished_todos(app)
        .into_iter()
        .map(|(content, status)| (content.to_string(), status.to_string()))
        .collect::<Vec<_>>();
    if todos.is_empty() {
        WorkPanel::Hidden
    } else {
        WorkPanel::Todos(todos)
    }
}

/// The input-adjacent work panel is absent when the latest authoritative
/// Task/Todo state is empty or fully completed.
#[cfg(test)]
pub(crate) fn todo_panel_visible(app: &TuiApp) -> bool {
    !matches!(work_panel_state(app), WorkPanel::Hidden)
}

fn work_panel_height(panel: &WorkPanel, width: u16) -> u16 {
    match panel {
        WorkPanel::Hidden => 0,
        WorkPanel::Todos(todos) => u16::try_from(todos.len().min(8)).unwrap_or(8),
        WorkPanel::Tasks { snapshot, degraded } => {
            let columns = usize::from(width >= 88) + 1;
            let visible_limit = if columns == 2 { 6 } else { 4 };
            let visible = snapshot.rows.len().min(visible_limit);
            let task_rows = visible.div_ceil(columns);
            let overflow = usize::from(snapshot.rows.len() > visible);
            let degraded = usize::from(*degraded);
            u16::try_from(2 + degraded + task_rows + overflow).unwrap_or(8)
        }
    }
}

fn render_work_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    panel: &WorkPanel,
    language: UiLanguage,
    theme: CrabCodeTheme,
) {
    if area.height == 0 {
        return;
    }
    match panel {
        WorkPanel::Hidden => {}
        WorkPanel::Todos(todos) => render_legacy_todos(frame, area, todos, theme),
        WorkPanel::Tasks { snapshot, degraded } => {
            render_task_cards(frame, area, snapshot, *degraded, language, theme);
        }
    }
}

fn render_legacy_todos(
    frame: &mut Frame<'_>,
    area: Rect,
    todos: &[(String, String)],
    theme: CrabCodeTheme,
) {
    let lines = todos
        .iter()
        .take(usize::from(area.height))
        .map(|(content, status)| {
            let (glyph, style) = match status.as_str() {
                "completed" => ("✓", Style::default().fg(theme.gray)),
                "in_progress" => ("◉", Style::default().fg(theme.accent_running)),
                _ => ("○", Style::default().fg(theme.gray_bright)),
            };
            Line::from(vec![
                Span::styled(format!(" {glyph} "), style),
                Span::styled(sanitize_bounded_terminal_text(content).into_owned(), style),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_task_cards(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &TaskPanelSnapshot,
    degraded: bool,
    language: UiLanguage,
    theme: CrabCodeTheme,
) {
    let counts = snapshot.counts;
    let mut title = match language {
        UiLanguage::ZhCn => format!(" 任务 · {}/{} 已完成", counts.completed, counts.total),
        UiLanguage::EnUs => format!(" Tasks · {}/{} completed", counts.completed, counts.total),
    };
    if counts.in_progress > 0 {
        match language {
            UiLanguage::ZhCn => title.push_str(&format!(" · {} 进行中", counts.in_progress)),
            UiLanguage::EnUs => title.push_str(&format!(" · {} active", counts.in_progress)),
        }
    }
    if counts.blocked > 0 {
        match language {
            UiLanguage::ZhCn => title.push_str(&format!(" · {} 阻塞", counts.blocked)),
            UiLanguage::EnUs => title.push_str(&format!(" · {} blocked", counts.blocked)),
        }
    }
    title.push(' ');
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.prompt_border))
        .style(Style::default().bg(theme.bg_dark))
        .title(Line::styled(
            title,
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.bg_dark)
                .add_modifier(Modifier::BOLD),
        ));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    if degraded {
        let warning = language.text(
            "  ! 任务状态部分不可用，显示最近有效状态",
            "  ! Task state is partially unavailable; showing last valid state",
        );
        frame.render_widget(
            Paragraph::new(Line::styled(
                truncate_str(warning, usize::from(inner.width)),
                Style::default().fg(theme.warning).bg(theme.bg_dark),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        inner.y = inner.y.saturating_add(1);
        inner.height = inner.height.saturating_sub(1);
    }
    if inner.is_empty() {
        return;
    }

    let mut display_rows = snapshot.rows.iter().collect::<Vec<_>>();
    display_rows.sort_by_key(|task| match task.status {
        TaskPanelStatus::InProgress => 0,
        TaskPanelStatus::Pending if task.blocked() => 1,
        TaskPanelStatus::Pending => 2,
        TaskPanelStatus::Completed => 3,
    });
    let columns = if inner.width >= 84 { 2_usize } else { 1_usize };
    let available_rows = usize::from(inner.height);
    let raw_capacity = available_rows.saturating_mul(columns);
    let reserve_overflow = display_rows.len() > raw_capacity;
    let task_row_capacity = available_rows.saturating_sub(usize::from(reserve_overflow));
    let item_capacity = task_row_capacity.saturating_mul(columns);
    let visible = display_rows.len().min(item_capacity);

    for row_index in 0..task_row_capacity {
        let row_area = Rect::new(
            inner.x,
            inner
                .y
                .saturating_add(u16::try_from(row_index).unwrap_or(u16::MAX)),
            inner.width,
            1,
        );
        if columns == 1 {
            if let Some(task) = display_rows.get(row_index) {
                frame.render_widget(
                    Paragraph::new(task_card_line(task, row_area.width, language, theme)),
                    row_area,
                );
            }
            continue;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(row_area);
        if let Some(task) = display_rows.get(row_index * 2) {
            frame.render_widget(
                Paragraph::new(task_card_line(task, left.width, language, theme)),
                left,
            );
        }
        if let Some(task) = display_rows.get(row_index * 2 + 1) {
            frame.render_widget(
                Paragraph::new(task_card_line(task, right.width, language, theme)),
                right,
            );
        }
    }

    let hidden = display_rows.len().saturating_sub(visible);
    if hidden > 0 && available_rows > 0 {
        let row = inner.y.saturating_add(inner.height.saturating_sub(1));
        let text = match language {
            UiLanguage::ZhCn => format!("  … 另有 {hidden} 项"),
            UiLanguage::EnUs => format!("  … {hidden} more"),
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                text,
                Style::default().fg(theme.gray).bg(theme.bg_dark),
            )),
            Rect::new(inner.x, row, inner.width, 1),
        );
    }
}

fn task_card_line(
    task: &TaskPanelRow,
    width: u16,
    language: UiLanguage,
    theme: CrabCodeTheme,
) -> Line<'static> {
    let (glyph, status, style) = if task.blocked() {
        (
            "!",
            language.text("阻塞", "blocked"),
            Style::default().fg(theme.warning).bg(theme.bg_dark),
        )
    } else {
        match task.status {
            TaskPanelStatus::Completed => (
                "✓",
                language.text("已完成", "done"),
                Style::default().fg(theme.accent_success).bg(theme.bg_dark),
            ),
            TaskPanelStatus::InProgress => (
                "◉",
                language.text("进行中", "active"),
                Style::default()
                    .fg(theme.accent_running)
                    .bg(theme.bg_dark)
                    .add_modifier(Modifier::BOLD),
            ),
            TaskPanelStatus::Pending => (
                "○",
                language.text("待处理", "pending"),
                Style::default().fg(theme.gray_bright).bg(theme.bg_dark),
            ),
        }
    };
    let show_status = width >= 28;
    let suffix = if show_status {
        format!("  {status} ")
    } else {
        " ".to_string()
    };
    let prefix_shell = format!(" {glyph} #");
    let id_width = usize::from(width)
        .saturating_sub(prefix_shell.width())
        .saturating_sub(suffix.width())
        .saturating_sub(1)
        .min(32);
    let id = task_card_field(&task.id, id_width);
    let prefix = format!("{prefix_shell}{id} ");
    let subject_width = usize::from(width)
        .saturating_sub(prefix.width())
        .saturating_sub(suffix.width());
    let subject = task_card_field(&task.subject, subject_width);
    let mut spans = vec![
        Span::styled(prefix, style),
        Span::styled(
            subject,
            Style::default().fg(theme.text_primary).bg(theme.bg_dark),
        ),
    ];
    let used = spans.iter().map(|span| span.content.width()).sum::<usize>();
    spans.push(Span::styled(
        " ".repeat(usize::from(width).saturating_sub(used + suffix.width())),
        Style::default().bg(theme.bg_dark),
    ));
    spans.push(Span::styled(suffix, style));
    fit_line_to_width(
        Line::from(spans).style(Style::default().bg(theme.bg_dark)),
        width.into(),
    )
}

fn task_card_field(raw: &str, max_width: usize) -> String {
    let sanitized = sanitize_bounded_terminal_text(raw);
    let single_line = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_str(&single_line, max_width)
}

/// Resolve the one-line live status from exact renderer state. Observed task
/// descriptions take precedence, then the validated session state; idle keeps
/// a discoverable local help hint.
pub(crate) fn minimal_status_line(app: &TuiApp) -> String {
    let language = app.ui_language();
    let turn = app.turn_status();
    if let Some(spinner) = turn.spinner() {
        let activity = if turn.state() == AgentState::Cancelling {
            language.text("正在取消", "Cancelling")
        } else {
            match turn.activity() {
                Some(TurnActivity::Requesting) => {
                    language.text("正在请求响应", "Requesting response")
                }
                Some(TurnActivity::Thinking) => language.text("正在思考", "Thinking"),
                Some(TurnActivity::Responding) => language.text("正在回复", "Responding"),
                Some(TurnActivity::ToolInput) => language.text("正在准备工具", "Preparing tool"),
                Some(TurnActivity::ToolUse) => language.text("正在运行工具", "Running tool"),
                Some(TurnActivity::Waiting(WaitingReason::Model)) => {
                    language.text("正在等待模型", "Waiting for model")
                }
                Some(TurnActivity::Waiting(WaitingReason::User)) => {
                    language.text("等待你的输入", "Waiting for your input")
                }
                Some(TurnActivity::Waiting(WaitingReason::Subagent)) => {
                    language.text("正在等待子代理", "Waiting for subagent")
                }
                Some(TurnActivity::Waiting(WaitingReason::Task)) => {
                    language.text("正在等待任务", "Waiting for task")
                }
                Some(TurnActivity::Retrying) => language.text("正在重试", "Retrying"),
                None => language.text("正在工作", "Working"),
            }
        };
        let now = Instant::now();
        let phase = turn
            .activity_started_at()
            .map(|started| compact_turn_duration(now.saturating_duration_since(started)))
            .unwrap_or_else(|| "0s".to_string());
        let total = turn
            .elapsed(now)
            .map(compact_turn_duration)
            .unwrap_or_else(|| "0s".to_string());
        return format!("{spinner} {activity} {phase}  ·  {total}");
    }
    if let Some(icon) = turn.watcher_icon() {
        return format!("{icon} {}", still_running_label(turn.watchers(), language));
    }
    if let Some((task_id, summary)) = app.active_tasks.iter().min_by_key(|(task_id, _)| *task_id) {
        return format!(
            "{} {task_id} · {summary}",
            language.text("活动任务", "Active")
        );
    }
    if app.stream_requesting() {
        return language
            .text("CrabCode 正在请求响应", "CrabCode is requesting a response")
            .to_string();
    }
    match app.projection.session_state() {
        Some("running") => app
            .projection
            .items()
            .iter()
            .rev()
            .find(|item| item.streaming && item.kind == ProjectedKind::Progress)
            .map_or_else(
                || {
                    language
                        .text("CrabCode 正在响应", "CrabCode is responding")
                        .to_string()
                },
                |item| {
                    if item.text.is_empty() {
                        format!("{} {}", language.text("运行", "Run"), item.title)
                    } else {
                        format!(
                            "{} {} · {}",
                            language.text("运行", "Run"),
                            item.title,
                            item.text
                        )
                    }
                },
            ),
        Some("requires_action") => language
            .text("等待你的输入", "Waiting for your input")
            .to_string(),
        _ if app.status.contains("/help") => app.status.clone(),
        _ => format!("{} · /help", app.status),
    }
}

fn still_running_label(watchers: Watchers, language: UiLanguage) -> String {
    let mut parts = Vec::new();
    let mut push = |count: usize, zh: &str, en: &str| {
        if count == 0 {
            return;
        }
        parts.push(match language {
            UiLanguage::ZhCn => format!("{count} 个{zh}"),
            UiLanguage::EnUs => format!("{count} {en}{}", if count == 1 { "" } else { "s" }),
        });
    };
    push(watchers.commands, "命令", "command");
    push(watchers.monitors, "监控", "monitor");
    push(watchers.loops, "循环", "loop");
    push(watchers.subagents, "子代理", "subagent");
    push(watchers.workflows, "工作流", "workflow");
    match language {
        UiLanguage::ZhCn => format!("{}仍在运行", parts.join("、")),
        UiLanguage::EnUs => format!("{} still running", parts.join(", ")),
    }
}

fn compact_turn_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// Rows reserved above the composer for slash-command completion: one row per
/// visible item, capped, plus the top and bottom borders.
pub(crate) fn completion_overlay_rows(app: &TuiApp) -> u16 {
    let count = app
        .visible_command_suggestions()
        .count()
        .min(usize::from(MAX_COMPLETION_VISIBLE_ROWS));
    u16::try_from(count)
        .ok()
        .filter(|count| *count > 0)
        .map_or(0, |count| count + 2)
}

fn render_completion_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let language = app.ui_language();
    let rows = app
        .visible_command_suggestions()
        .map(|(selected, command)| {
            let argument = if command.argument_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", command.argument_hint)
            };
            (
                format!("/{}{argument}", command.name),
                command.description.clone(),
                selected,
            )
        })
        .collect::<Vec<_>>();
    let entries = rows
        .iter()
        .map(|(label, description, selected)| {
            PickerEntry::Row(PickerRow::simple(label, description, *selected))
        })
        .collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.prompt_border_active))
        .style(Style::default().bg(theme.bg_dark))
        .title(language.text(
            " 命令 · ↑/↓ 选择 · Tab 补全 ",
            " Commands · ↑/↓ choose · Tab complete ",
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let picker = app.command_palette_picker_mut();
    let hits = render_picker_content(
        frame.buffer_mut(),
        inner,
        &theme,
        picker,
        &entries,
        &[],
        &[],
        Some(theme.bg_dark),
        false,
    );
    picker.hit_areas = Some(PickerHitAreas {
        item_rects: hits.item_rects,
        entry_indices: hits.entry_indices,
        ..empty_picker_hit_areas()
    });
}

fn picker_search_cursor(area: Rect, state: &PickerState, label: &str) -> Option<(u16, u16)> {
    // Shared picker viewport math is intentionally byte-based for ASCII
    // labels, so renderer-owned localized surfaces use a language-neutral
    // ASCII prompt in Chinese mode and mirror that denominator here.
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    if !state.search_active || area.height == 0 || area.width <= label_width {
        return None;
    }
    let input_width = usize::from(area.width - label_width);
    let viewport = state.query_viewport(input_width);
    let cursor_column = u16::try_from(viewport.cursor_display_column)
        .unwrap_or(u16::MAX)
        .min((area.width - label_width).saturating_sub(1));
    Some((area.x + label_width + cursor_column, area.y))
}

fn render_history_search(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) -> Option<(u16, u16)> {
    let language = app.ui_language();
    let search = app.history_search.as_mut()?;
    let preview_on_right = area.width >= 100;
    let desired_height = if preview_on_right { 14 } else { 22 };
    let geometry = centered_selection_geometry(area, 94, 140, desired_height, false, false);
    frame.render_widget(Clear, geometry.panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.prompt_border_active))
        .style(Style::default().bg(theme.bg_dark))
        .title(language.text(" 搜索历史提示词 ", " Search prompts "));
    let inner = block.inner(geometry.panel);
    frame.render_widget(block, geometry.panel);

    let footer_height = u16::from(inner.height > 0);
    let search_height = u16::from(inner.height > footer_height);
    let content_height = inner
        .height
        .saturating_sub(search_height.saturating_add(footer_height));
    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(footer_height),
        inner.width,
        footer_height,
    );
    let search_row = Rect::new(
        inner.x,
        footer.y.saturating_sub(search_height),
        inner.width,
        search_height,
    );
    let content = Rect::new(inner.x, inner.y, inner.width, content_height);

    let (list, preview) = if preview_on_right && content.width >= 5 {
        let list_width = content.width.saturating_sub(2) / 2;
        (
            Rect::new(content.x, content.y, list_width, content.height),
            Rect::new(
                content.x + list_width + 2,
                content.y,
                content.width.saturating_sub(list_width + 2),
                content.height,
            ),
        )
    } else {
        let preview_height = content.height.min(8);
        let list_height = content.height.saturating_sub(preview_height);
        (
            Rect::new(content.x, content.y, content.width, list_height),
            Rect::new(
                content.x,
                content.y + list_height,
                content.width,
                preview_height,
            ),
        )
    };

    let selected = search.lifecycle().selected_for(search.match_count());
    let mut rows = Vec::with_capacity(search.match_count());
    for row in 0..search.match_count() {
        if let Some((_, entry)) = search.entry_for_visible_row(row) {
            rows.push((
                format!(
                    "{} {}",
                    sanitize_bounded_terminal_text(&entry.age),
                    sanitize_bounded_terminal_text(&entry.first_line)
                ),
                selected == Some(row),
            ));
        }
    }
    let entries = rows
        .iter()
        .map(|(label, selected)| PickerEntry::Row(PickerRow::simple(label, "", *selected)))
        .collect::<Vec<_>>();
    let mut hits = if rows.is_empty() {
        let empty = if search.query().trim().is_empty() {
            language.text("暂无历史记录", "No history yet")
        } else {
            language.text("没有匹配的提示词", "No matching prompts")
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(theme.gray).bg(theme.bg_dark)),
            list,
        );
        PickerHitAreas {
            search_bar: search_row,
            item_rects: Vec::new(),
            entry_indices: Vec::new(),
            ..empty_picker_hit_areas()
        }
    } else {
        let content_hits = render_picker_content(
            frame.buffer_mut(),
            list,
            &theme,
            search.lifecycle_mut(),
            &entries,
            &[],
            &[],
            Some(theme.bg_dark),
            false,
        );
        PickerHitAreas {
            search_bar: search_row,
            item_rects: content_hits.item_rects,
            entry_indices: content_hits.entry_indices,
            ..empty_picker_hit_areas()
        }
    };

    if preview.width > 2
        && preview.height > 2
        && let Some(entry) = search.selected_entry()
    {
        let preview_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.gray))
            .style(Style::default().bg(theme.bg_dark));
        let preview_inner = preview_block.inner(preview);
        let mut lines = entry
            .display
            .split('\n')
            .flat_map(|line| {
                wrap_line(
                    &Line::styled(
                        sanitize_bounded_terminal_text(line).into_owned(),
                        Style::default().fg(theme.gray_bright),
                    ),
                    usize::from(preview_inner.width.max(1)),
                )
            })
            .filter(|line| line.width() > 0)
            .collect::<Vec<_>>();
        let available = usize::from(preview_inner.height);
        if lines.len() > available && available > 0 {
            let hidden = lines.len().saturating_sub(available.saturating_sub(1));
            lines.truncate(available.saturating_sub(1));
            lines.push(Line::styled(
                if matches!(language, UiLanguage::ZhCn) {
                    format!("… 另有 {hidden} 行")
                } else {
                    format!("… +{hidden} more lines")
                },
                Style::default().fg(theme.gray),
            ));
        } else {
            lines.truncate(available);
        }
        frame.render_widget(preview_block, preview);
        frame.render_widget(Paragraph::new(lines), preview_inner);
    }

    let history_search_label = language.text(" > ", " Filter history: ");
    render_picker_search_bar_with_label(
        frame.buffer_mut(),
        search_row.x,
        search_row.y,
        search_row.width,
        &theme,
        history_search_label,
        search.lifecycle(),
        search.lifecycle().search_active,
        true,
        Some(theme.bg_dark),
    );
    let cursor = picker_search_cursor(search_row, search.lifecycle(), history_search_label);
    if footer.height > 0 {
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(
                    language.text(
                        "↑/↓ 导航 · Enter/Tab 使用 · Esc 取消",
                        "↑/↓ navigate · Enter/Tab use · Esc cancel",
                    ),
                    Style::default().fg(theme.gray),
                ),
                usize::from(footer.width),
            )),
            footer,
        );
    }
    hits.close_button = geometry.close_button;
    hits.search_bar = search_row;
    search.lifecycle_mut().hit_areas = Some(hits);
    if geometry.close_button.width > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " × ",
                Style::default()
                    .fg(theme.prompt_border_active)
                    .bg(theme.bg_dark),
            )),
            geometry.close_button,
        );
    }
    cursor
}

fn render_workspace_search(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) -> Option<(u16, u16)> {
    let language = app.ui_language();
    let search = app.workspace_search.as_mut()?;
    let kind = search.kind();
    let preview_on_right = area.width
        >= match kind {
            WorkspaceSearchKind::QuickOpen => 120,
            WorkspaceSearchKind::GlobalSearch => 140,
        };
    let desired_height = if preview_on_right {
        u16::try_from(kind.visible_results())
            .unwrap_or(u16::MAX)
            .saturating_add(4)
    } else {
        match kind {
            WorkspaceSearchKind::QuickOpen => 32,
            WorkspaceSearchKind::GlobalSearch => 25,
        }
    };
    let geometry = centered_selection_geometry(area, 96, 160, desired_height, false, false);
    frame.render_widget(Clear, geometry.panel);
    let title = match kind {
        WorkspaceSearchKind::QuickOpen => language.text(" 快速打开 ", " Quick Open "),
        WorkspaceSearchKind::GlobalSearch => language.text(" 全局搜索 ", " Global Search "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.prompt_border_active))
        .style(Style::default().bg(theme.bg_dark))
        .title(title);
    let inner = block.inner(geometry.panel);
    frame.render_widget(block, geometry.panel);

    let footer_height = u16::from(inner.height > 0);
    let search_height = u16::from(inner.height > footer_height);
    let content_height = inner
        .height
        .saturating_sub(search_height.saturating_add(footer_height));
    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(footer_height),
        inner.width,
        footer_height,
    );
    let search_row = Rect::new(
        inner.x,
        footer.y.saturating_sub(search_height),
        inner.width,
        search_height,
    );
    let content = Rect::new(inner.x, inner.y, inner.width, content_height);

    let (mut list, preview) = if preview_on_right && content.width >= 5 {
        let list_width = match kind {
            WorkspaceSearchKind::QuickOpen => content.width.saturating_sub(2).saturating_mul(2) / 5,
            WorkspaceSearchKind::GlobalSearch => content.width.saturating_sub(2) / 2,
        };
        (
            Rect::new(content.x, content.y, list_width, content.height),
            Rect::new(
                content.x + list_width + 2,
                content.y,
                content.width.saturating_sub(list_width + 2),
                content.height,
            ),
        )
    } else {
        let preview_height = content.height.min(match kind {
            WorkspaceSearchKind::QuickOpen => 22,
            WorkspaceSearchKind::GlobalSearch => 11,
        });
        let list_height = content.height.saturating_sub(preview_height);
        (
            Rect::new(content.x, content.y, content.width, list_height),
            Rect::new(
                content.x,
                content.y + list_height,
                content.width,
                preview_height,
            ),
        )
    };
    let visible_rows = u16::try_from(kind.visible_results()).unwrap_or(u16::MAX);
    if list.height > visible_rows {
        list.y += list.height - visible_rows;
        list.height = visible_rows;
    }

    let selected = search.selected_index();
    let query = search.query().to_string();
    let list_width = usize::from(list.width.saturating_sub(3).max(1));
    let mut rows = Vec::with_capacity(search.entries().len());
    for (index, entry) in search.entries().iter().enumerate() {
        let is_selected = selected == Some(index);
        let label = match (kind, entry.line) {
            (WorkspaceSearchKind::QuickOpen, _) => {
                truncate_workspace_path_middle(&entry.path, list_width)
            }
            (WorkspaceSearchKind::GlobalSearch, Some(line_number)) => {
                let path_budget = list_width.saturating_mul(2) / 5;
                let path = truncate_workspace_path_middle(&entry.path, path_budget.max(1));
                let prefix = format!("{path}:{line_number} ");
                let text_budget = list_width.saturating_sub(prefix.width());
                format!(
                    "{prefix}{}",
                    truncate_str(entry.text.trim_start(), text_budget.max(1))
                )
            }
            (WorkspaceSearchKind::GlobalSearch, None) => {
                truncate_workspace_path_middle(&entry.path, list_width)
            }
        };
        rows.push((label, is_selected));
    }
    let entries = rows
        .iter()
        .map(|(label, selected)| PickerEntry::Row(PickerRow::simple(label, "", *selected)))
        .collect::<Vec<_>>();

    let mut hits = if rows.is_empty() {
        let empty = if let Some(error) = search.error() {
            sanitize_bounded_terminal_text(error).into_owned()
        } else if search.searching() {
            language.text("正在搜索…", "Searching…").to_string()
        } else if query.trim().is_empty() {
            match kind {
                WorkspaceSearchKind::QuickOpen => language
                    .text("输入内容以搜索…", "Start typing to search…")
                    .to_string(),
                WorkspaceSearchKind::GlobalSearch => language
                    .text(
                        "输入文本以搜索工作区…",
                        "Type text to search the workspace…",
                    )
                    .to_string(),
            }
        } else {
            match kind {
                WorkspaceSearchKind::QuickOpen => language
                    .text("没有匹配的文件", "No matching files")
                    .to_string(),
                WorkspaceSearchKind::GlobalSearch => {
                    language.text("没有匹配项", "No matches").to_string()
                }
            }
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(theme.gray).bg(theme.bg_dark)),
            list,
        );
        PickerHitAreas {
            search_bar: search_row,
            item_rects: Vec::new(),
            entry_indices: Vec::new(),
            ..empty_picker_hit_areas()
        }
    } else {
        let content_hits = render_picker_content(
            frame.buffer_mut(),
            list,
            &theme,
            search.lifecycle_mut(),
            &entries,
            &[],
            &[],
            Some(theme.bg_dark),
            false,
        );
        PickerHitAreas {
            search_bar: search_row,
            item_rects: content_hits.item_rects,
            entry_indices: content_hits.entry_indices,
            ..empty_picker_hit_areas()
        }
    };

    if preview.width > 2 && preview.height > 2 {
        let preview_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.gray))
            .style(Style::default().bg(theme.bg_dark));
        let preview_inner = preview_block.inner(preview);
        frame.render_widget(preview_block, preview);
        let preview_lines = if let Some(entry) = search.selected_entry() {
            let mut lines = vec![Line::styled(
                match entry.line {
                    Some(line) => format!(
                        "{}:{line}",
                        truncate_workspace_path_middle(
                            &entry.path,
                            usize::from(preview_inner.width.max(1))
                        )
                    ),
                    None => truncate_workspace_path_middle(
                        &entry.path,
                        usize::from(preview_inner.width.max(1)),
                    ),
                },
                Style::default().fg(theme.gray),
            )];
            if let Some(loaded) = search.preview() {
                lines.extend(loaded.content.split('\n').map(|line| {
                    workspace_highlighted_line(
                        &truncate_str(line, usize::from(preview_inner.width.max(1))),
                        &query,
                        Style::default().fg(theme.gray_bright),
                        Style::default()
                            .fg(theme.accent_assistant)
                            .add_modifier(Modifier::BOLD),
                    )
                }));
            } else {
                lines.push(Line::styled(
                    language.text("正在加载预览…", "Loading preview…"),
                    Style::default().fg(theme.gray),
                ));
            }
            lines.truncate(usize::from(preview_inner.height));
            lines
        } else {
            Vec::new()
        };
        frame.render_widget(Paragraph::new(preview_lines), preview_inner);
    }

    let search_label = match kind {
        WorkspaceSearchKind::QuickOpen | WorkspaceSearchKind::GlobalSearch
            if matches!(language, UiLanguage::ZhCn) =>
        {
            " > "
        }
        WorkspaceSearchKind::QuickOpen => " Search files: ",
        WorkspaceSearchKind::GlobalSearch => " Search workspace: ",
    };
    render_picker_search_bar_with_label(
        frame.buffer_mut(),
        search_row.x,
        search_row.y,
        search_row.width,
        &theme,
        search_label,
        search.lifecycle(),
        search.lifecycle().search_active,
        true,
        Some(theme.bg_dark),
    );
    let cursor = picker_search_cursor(search_row, search.lifecycle(), search_label);
    if footer.height > 0 {
        let count = search.entries().len();
        let count_label = match kind {
            WorkspaceSearchKind::QuickOpen => {
                if matches!(language, UiLanguage::ZhCn) {
                    format!("{count} 个文件")
                } else {
                    format!("{count} files")
                }
            }
            WorkspaceSearchKind::GlobalSearch => {
                if matches!(language, UiLanguage::ZhCn) {
                    format!(
                        "{count}{} 个匹配项{}",
                        if search.truncated() { "+" } else { "" },
                        if search.searching() { "…" } else { "" }
                    )
                } else {
                    format!(
                        "{count}{} matches{}",
                        if search.truncated() { "+" } else { "" },
                        if search.searching() { "…" } else { "" }
                    )
                }
            }
        };
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::from(vec![
                    Span::styled(
                        language.text(
                            "↑/↓ 导航 · Enter 打开 · Tab 引用 · Shift-Tab 路径 · Esc 取消",
                            "↑/↓ navigate · Enter open · Tab mention · Shift-Tab path · Esc cancel",
                        ),
                        Style::default().fg(theme.gray),
                    ),
                    Span::styled(format!(" · {count_label}"), Style::default().fg(theme.gray)),
                ]),
                usize::from(footer.width),
            )),
            footer,
        );
    }
    hits.close_button = geometry.close_button;
    hits.search_bar = search_row;
    search.lifecycle_mut().hit_areas = Some(hits);
    if geometry.close_button.width > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " × ",
                Style::default()
                    .fg(theme.prompt_border_active)
                    .bg(theme.bg_dark),
            )),
            geometry.close_button,
        );
    }
    cursor
}

/// Fixed historical QuickOpen/GlobalSearch path formatter.
///
/// It preserves the filename and as much directory prefix as the display
/// width permits. Every cut occurs on an extended-grapheme boundary so CJK
/// paths, combining marks, and emoji cannot be split into invalid terminal
/// cells.
fn truncate_workspace_path_middle(path: &str, max_width: usize) -> String {
    if path.width() <= max_width {
        return path.to_string();
    }
    if max_width == 0 {
        return "…".to_string();
    }
    if max_width < 5 {
        return truncate_str(path, max_width);
    }

    let last_slash = path.rfind('/');
    let filename = last_slash.map_or(path, |index| &path[index..]);
    let directory = last_slash.map_or("", |index| &path[..index]);
    let filename_width = filename.width();
    if filename_width >= max_width.saturating_sub(1) {
        return truncate_workspace_start(path, max_width);
    }

    let directory_budget = max_width.saturating_sub(1 + filename_width);
    if directory_budget == 0 {
        return truncate_workspace_start(filename, max_width);
    }
    let directory_end = directory
        .grapheme_indices(true)
        .scan(0_usize, |used, (index, grapheme)| {
            let next = used.saturating_add(grapheme.width());
            if next > directory_budget {
                None
            } else {
                *used = next;
                Some(index + grapheme.len())
            }
        })
        .last()
        .unwrap_or(0);
    format!("{}…{filename}", &directory[..directory_end])
}

fn truncate_workspace_start(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }

    let tail_budget = max_width - 1;
    let mut used = 0_usize;
    let mut start = value.len();
    for (index, grapheme) in value.grapheme_indices(true).rev() {
        let next = used.saturating_add(grapheme.width());
        if next > tail_budget {
            break;
        }
        used = next;
        start = index;
    }
    format!("…{}", &value[start..])
}

fn workspace_highlighted_line(
    text: &str,
    query: &str,
    normal: Style,
    highlighted: Style,
) -> Line<'static> {
    let safe = sanitize_bounded_terminal_text(text).into_owned();
    let query = query.trim();
    if query.is_empty() {
        return Line::styled(safe, normal);
    }
    let Ok(pattern) = regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
    else {
        return Line::styled(safe, normal);
    };
    let mut spans = Vec::new();
    let mut cursor = 0;
    for matched in pattern.find_iter(&safe) {
        if matched.start() > cursor {
            spans.push(Span::styled(
                safe[cursor..matched.start()].to_string(),
                normal,
            ));
        }
        spans.push(Span::styled(
            safe[matched.start()..matched.end()].to_string(),
            highlighted,
        ));
        cursor = matched.end();
    }
    if cursor < safe.len() {
        spans.push(Span::styled(safe[cursor..].to_string(), normal));
    }
    if spans.is_empty() {
        Line::styled(safe, normal)
    } else {
        Line::from(spans)
    }
}

fn render_model_picker(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp, theme: CrabCodeTheme) {
    let language = app.ui_language();
    let Some(picker) = app.model_picker.as_mut() else {
        return;
    };
    let desired_height = u16::try_from(
        picker
            .match_count()
            .clamp(1, crate::tui_app::MODEL_PICKER_VISIBLE_OPTIONS),
    )
    .unwrap_or(u16::MAX)
    .saturating_add(4);
    let geometry = centered_selection_geometry(area, 86, 82, desired_height, true, true);
    frame.render_widget(Clear, geometry.panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent_assistant))
        .style(Style::default().bg(theme.bg_dark))
        .title(language.text(" 选择模型 ", " Select model "));
    frame.render_widget(block, geometry.panel);

    let selected_visible = picker.lifecycle().selected_for(picker.match_count());
    let mut rows = Vec::with_capacity(picker.match_count());
    for visible_index in 0..picker.match_count() {
        if let Some((choice_index, choice)) = picker.choice_at_visible(visible_index) {
            let initial = picker.initial_model() == Some(choice.id.as_str());
            let label = format!(
                "{:>2}. {}{}",
                visible_index + 1,
                sanitize_bounded_terminal_text(&choice.label),
                if initial { " ✓" } else { "" }
            );
            let description = choice
                .description
                .as_deref()
                .filter(|description| !description.is_empty())
                .map(|description| sanitize_bounded_terminal_text(description).into_owned())
                .unwrap_or_default();
            let _ = choice_index;
            rows.push((label, description, selected_visible == Some(visible_index)));
        }
    }
    let entries = rows
        .iter()
        .map(|(label, description, selected)| {
            PickerEntry::Row(PickerRow::simple(label, description, *selected))
        })
        .collect::<Vec<_>>();

    if let Some(search) = geometry.search {
        render_picker_search_bar(
            frame.buffer_mut(),
            search.x,
            search.y,
            search.width,
            &theme,
            picker.lifecycle(),
            picker.lifecycle().search_active,
            true,
            Some(theme.bg_dark),
        );
    }
    let mut hits = if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(if picker.choices().is_empty() {
                language.text(
                    "  运行环境未返回模型目录。",
                    "  Runtime returned no model catalog.",
                )
            } else {
                language.text("  没有匹配的模型。", "  No matching models.")
            })
            .style(Style::default().fg(theme.warning).bg(theme.bg_dark)),
            geometry.list,
        );
        PickerHitAreas {
            search_bar: geometry.search.unwrap_or_default(),
            item_rects: Vec::new(),
            entry_indices: Vec::new(),
            ..empty_picker_hit_areas()
        }
    } else {
        let content_hits = render_picker_content(
            frame.buffer_mut(),
            geometry.list,
            &theme,
            picker.lifecycle_mut(),
            &entries,
            &[],
            &[],
            Some(theme.bg_dark),
            false,
        );
        PickerHitAreas {
            search_bar: geometry.search.unwrap_or_default(),
            item_rects: content_hits.item_rects,
            entry_indices: content_hits.entry_indices,
            ..empty_picker_hit_areas()
        }
    };
    hits.close_button = geometry.close_button;
    hits.search_bar = geometry.search.unwrap_or_default();
    picker.lifecycle_mut().hit_areas = Some(hits);

    if let Some(footer) = geometry.footer {
        let hidden = picker.hidden_count();
        let hint = if hidden > 0 {
            if matches!(language, UiLanguage::ZhCn) {
                format!("↑/↓ · PgUp/PgDn · Enter 确认 · Esc 取消 · 另有 {hidden} 项")
            } else {
                format!("↑/↓ · PgUp/PgDn · Enter confirm · Esc cancel · and {hidden} more")
            }
        } else {
            language
                .text(
                    "↑/↓ · PgUp/PgDn · Enter 确认 · Esc 取消",
                    "↑/↓ · PgUp/PgDn · Enter confirm · Esc cancel",
                )
                .to_string()
        };
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(hint, Style::default().fg(theme.gray)),
                usize::from(footer.width),
            )),
            footer,
        );
    }
}

fn render_model_management(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) {
    let language = app.ui_language();
    let Some(management) = app.model_management.as_ref() else {
        return;
    };
    let rows = management.rows(language);
    let details = management.details(language);
    let input = management.input(language);
    let notice = management.notice();
    let selected = management.selected().min(rows.len().saturating_sub(1));
    let visible_count = rows.len().min(18);
    let visible_from = selected
        .saturating_add(1)
        .saturating_sub(visible_count)
        .min(rows.len().saturating_sub(visible_count));
    let visible_to = visible_from.saturating_add(visible_count).min(rows.len());
    let desired_height = u16::try_from(
        visible_count
            .saturating_add(details.len().min(5))
            .saturating_add(6)
            .saturating_add(usize::from(input.is_some()))
            .saturating_add(usize::from(notice.is_some())),
    )
    .unwrap_or(u16::MAX);
    let geometry = centered_selection_geometry(area, 94, 94, desired_height, false, true);
    frame.render_widget(Clear, geometry.panel);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent_assistant))
            .style(Style::default().bg(theme.bg_dark))
            .title(format!(
                " {}{} ",
                sanitize_bounded_terminal_text(&management.title(language)),
                if management.is_busy() {
                    language.text(" · 处理中", " · working")
                } else {
                    ""
                }
            )),
        geometry.panel,
    );

    let inner = geometry.panel.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let detail_height = u16::try_from(details.len().min(5)).unwrap_or(5);
    let notice_height = u16::from(notice.is_some());
    let input_height = u16::from(input.is_some());
    let footer_height = 1;
    let list_height = inner
        .height
        .saturating_sub(detail_height)
        .saturating_sub(notice_height)
        .saturating_sub(input_height)
        .saturating_sub(footer_height);
    let chunks = Layout::vertical([
        Constraint::Length(detail_height),
        Constraint::Length(notice_height),
        Constraint::Length(list_height),
        Constraint::Length(input_height),
        Constraint::Length(footer_height),
    ])
    .split(inner);

    if detail_height > 0 {
        let lines = details
            .iter()
            .take(5)
            .map(|line| {
                Line::styled(
                    sanitize_bounded_terminal_text(line),
                    Style::default().fg(theme.gray),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), chunks[0]);
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(
                    sanitize_bounded_terminal_text(notice),
                    Style::default().fg(theme.warning),
                ),
                usize::from(chunks[1].width),
            )),
            chunks[1],
        );
    }
    let list_lines = rows[visible_from..visible_to]
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let index = visible_from + offset;
            let marker = if index == selected { "› " } else { "  " };
            let disabled = if row.disabled {
                language.text("（不可用）", " (unavailable)")
            } else {
                ""
            };
            let detail = row
                .detail
                .as_ref()
                .map(|detail| format!(" · {}", sanitize_bounded_terminal_text(detail)))
                .unwrap_or_default();
            let style = if row.disabled {
                Style::default().fg(theme.gray)
            } else if index == selected {
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_highlight)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_primary)
            };
            fit_line_to_width(
                Line::styled(
                    format!(
                        "{marker}{}{}{}",
                        sanitize_bounded_terminal_text(&row.label),
                        disabled,
                        detail
                    ),
                    style,
                ),
                usize::from(chunks[2].width),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(list_lines), chunks[2]);

    if let Some((label, value, secret)) = input {
        let rendered = if secret {
            format!(
                "{label}: {value} {}",
                language.text("（已隐藏）", "(hidden)")
            )
        } else {
            format!("{label}: {value}")
        };
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(rendered, Style::default().fg(theme.text_primary)),
                usize::from(chunks[3].width),
            )),
            chunks[3],
        );
    }
    frame.render_widget(
        Paragraph::new(fit_line_to_width(
            Line::styled(management.footer(language), Style::default().fg(theme.gray)),
            usize::from(chunks[4].width),
        )),
        chunks[4],
    );
}

fn usage_management_content_width(terminal_width: u16) -> u16 {
    terminal_width
        .saturating_mul(96)
        .saturating_div(100)
        .max(1)
        .min(96)
        .min(terminal_width)
        .saturating_sub(6)
        .max(1)
}

fn render_usage_plugin_management(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) {
    let language = app.ui_language();
    let Some(management) = app.usage_plugin_management.as_ref() else {
        return;
    };
    let tabs = management.tabs(language);
    let rows = management.rows(language);
    let details = management.detail_lines(language, usage_management_content_width(area.width));
    let detail_limit = management.detail_line_limit();
    let input = management.input(language);
    let notice = management.notice();
    let selected = management.selected().min(rows.len().saturating_sub(1));
    let visible_count = rows.len().min(18);
    let visible_from = selected
        .saturating_add(1)
        .saturating_sub(visible_count)
        .min(rows.len().saturating_sub(visible_count));
    let visible_to = visible_from.saturating_add(visible_count).min(rows.len());
    let tab_height = u16::from(!tabs.is_empty());
    let detail_height = u16::try_from(details.len().min(detail_limit)).unwrap_or(12);
    let notice_height = u16::from(notice.is_some());
    let input_height = u16::from(input.is_some());
    let desired_height = u16::try_from(
        visible_count
            .saturating_add(usize::from(tab_height))
            .saturating_add(usize::from(detail_height))
            .saturating_add(usize::from(notice_height))
            .saturating_add(usize::from(input_height))
            .saturating_add(5),
    )
    .unwrap_or(u16::MAX);
    let geometry = centered_selection_geometry(area, 96, 96, desired_height, false, true);
    frame.render_widget(Clear, geometry.panel);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent_assistant))
            .style(Style::default().bg(theme.bg_dark))
            .title(format!(
                " {}{} ",
                sanitize_bounded_terminal_text(&management.title(language)),
                if management.is_busy() {
                    language.text(" · 处理中", " · working")
                } else {
                    ""
                }
            )),
        geometry.panel,
    );

    let inner = geometry.panel.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let list_height = inner
        .height
        .saturating_sub(tab_height)
        .saturating_sub(detail_height)
        .saturating_sub(notice_height)
        .saturating_sub(input_height)
        .saturating_sub(1);
    let chunks = Layout::vertical([
        Constraint::Length(tab_height),
        Constraint::Length(detail_height),
        Constraint::Length(notice_height),
        Constraint::Length(list_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(inner);

    if tab_height > 0 {
        let mut spans = Vec::new();
        for (index, tab) in tabs.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(theme.gray)));
            }
            let badge = tab.badge.map_or(String::new(), |count| format!(" {count}"));
            spans.push(Span::styled(
                format!("{}{}", sanitize_bounded_terminal_text(&tab.label), badge),
                if tab.active {
                    Style::default()
                        .fg(theme.accent_assistant)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.gray)
                },
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);
    }

    if detail_height > 0 {
        frame.render_widget(
            Paragraph::new(
                details
                    .iter()
                    .take(detail_limit)
                    .map(|line| {
                        let style = match line.tone {
                            crate::usage_plugin_management::UsagePluginDetailTone::Section => {
                                Style::default()
                                    .fg(theme.accent_assistant)
                                    .add_modifier(Modifier::BOLD)
                            }
                            crate::usage_plugin_management::UsagePluginDetailTone::Metric => {
                                Style::default().fg(theme.text_primary)
                            }
                            crate::usage_plugin_management::UsagePluginDetailTone::Supporting => {
                                Style::default().fg(theme.gray)
                            }
                            crate::usage_plugin_management::UsagePluginDetailTone::Warning => {
                                Style::default()
                                    .fg(theme.warning)
                                    .add_modifier(Modifier::BOLD)
                            }
                        };
                        Line::styled(sanitize_bounded_terminal_text(&line.text), style)
                    })
                    .collect::<Vec<_>>(),
            ),
            chunks[1],
        );
    }
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(
                    sanitize_bounded_terminal_text(notice),
                    Style::default().fg(theme.warning),
                ),
                usize::from(chunks[2].width),
            )),
            chunks[2],
        );
    }

    let list_lines = rows[visible_from..visible_to]
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let index = visible_from + offset;
            let marker = if index == selected { "›" } else { " " };
            let marked = if row.marked { check_mark() } else { " " };
            let unavailable = if row.disabled {
                language.text("（不可用）", " (unavailable)")
            } else {
                ""
            };
            let detail = row
                .detail
                .as_ref()
                .map(|detail| format!(" · {}", sanitize_bounded_terminal_text(detail)))
                .unwrap_or_default();
            let style = if row.disabled {
                Style::default().fg(theme.gray)
            } else if index == selected {
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_highlight)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_primary)
            };
            fit_line_to_width(
                Line::styled(
                    format!(
                        "{marker}{marked} {}{unavailable}{detail}",
                        sanitize_bounded_terminal_text(&row.label)
                    ),
                    style,
                ),
                usize::from(chunks[3].width),
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(list_lines), chunks[3]);

    if let Some((label, value, secret)) = input {
        let rendered = if secret {
            format!(
                "{label}: {value} {}",
                language.text("（已隐藏）", "(hidden)")
            )
        } else {
            format!("{label}: {value}")
        };
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(
                    sanitize_bounded_terminal_text(&rendered),
                    Style::default().fg(theme.text_primary),
                ),
                usize::from(chunks[4].width),
            )),
            chunks[4],
        );
    }
    frame.render_widget(
        Paragraph::new(fit_line_to_width(
            Line::styled(management.footer(language), Style::default().fg(theme.gray)),
            usize::from(chunks[5].width),
        )),
        chunks[5],
    );
}

fn render_mcp_settings(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp, theme: CrabCodeTheme) {
    let language = app.ui_language();
    let Some(settings) = app.mcp_settings.as_mut() else {
        return;
    };
    let view = settings.view();
    let (title, row_count, fixed_detail_rows) = match view {
        McpSettingsView::List => (
            if matches!(language, UiLanguage::ZhCn) {
                format!(" 管理 MCP 服务器 · {} 个服务器 ", settings.servers().len())
            } else {
                format!(
                    " Manage MCP servers · {} servers ",
                    settings.servers().len()
                )
            },
            settings.servers().len(),
            0,
        ),
        McpSettingsView::Server { server_index } => {
            let server = settings.servers().get(server_index);
            (
                if matches!(language, UiLanguage::ZhCn) {
                    format!(
                        " {} MCP 服务器 ",
                        server.map_or("MCP", |server| server.name.as_str())
                    )
                } else {
                    format!(
                        " {} MCP Server ",
                        server.map_or("MCP", |server| server.name.as_str())
                    )
                },
                server.map_or(0, |server| settings.server_menu_options(server).len()),
                8,
            )
        }
        McpSettingsView::Tools { server_index } => {
            let server = settings.servers().get(server_index);
            (
                if matches!(language, UiLanguage::ZhCn) {
                    format!(
                        " {} 的工具 · {} 个工具 ",
                        server.map_or("MCP", |server| server.name.as_str()),
                        server.map_or(0, |server| server.tools.len())
                    )
                } else {
                    format!(
                        " Tools for {} · {} tools ",
                        server.map_or("MCP", |server| server.name.as_str()),
                        server.map_or(0, |server| server.tools.len())
                    )
                },
                server.map_or(0, |server| server.tools.len()),
                0,
            )
        }
        McpSettingsView::ToolDetail {
            server_index,
            tool_index,
        } => {
            let server = settings.servers().get(server_index);
            let tool = server.and_then(|server| server.tools.get(tool_index));
            (
                format!(
                    " {} · {} ",
                    server.map_or("MCP", |server| server.name.as_str()),
                    tool.map_or(language.text("工具", "tool"), |tool| tool.name.as_str())
                ),
                crate::tui_app::MCP_SETTINGS_VISIBLE_OPTIONS,
                0,
            )
        }
    };
    let visible_rows = row_count.clamp(1, crate::tui_app::MCP_SETTINGS_VISIBLE_OPTIONS);
    let desired_height = u16::try_from(visible_rows.saturating_add(fixed_detail_rows))
        .unwrap_or(u16::MAX)
        .saturating_add(3);
    let geometry = centered_selection_geometry(area, 92, 96, desired_height, false, true);
    frame.render_widget(Clear, geometry.panel);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent_assistant))
            .style(Style::default().bg(theme.bg_dark))
            .title(sanitize_bounded_terminal_text(&title)),
        geometry.panel,
    );

    match view {
        McpSettingsView::List => {
            let selected = settings.lifecycle().selected_for(settings.servers().len());
            let rows = settings
                .servers()
                .iter()
                .enumerate()
                .map(|(index, server)| {
                    let label = sanitize_bounded_terminal_text(&server.name).into_owned();
                    let scope = server
                        .scope
                        .as_deref()
                        .unwrap_or(language.text("范围不可用", "scope unavailable"));
                    let status = mcp_status_label(language, &server.status);
                    (
                        label,
                        format!("{} · {}", sanitize_bounded_terminal_text(scope), status),
                        selected == Some(index),
                    )
                })
                .collect::<Vec<_>>();
            let mut hits = render_mcp_picker_rows(
                frame,
                geometry.list,
                settings.lifecycle_mut(),
                &rows,
                theme,
                language.text("未配置 MCP 服务器。", "No MCP servers configured."),
            );
            hits.close_button = geometry.close_button;
            settings.lifecycle_mut().hit_areas = Some(hits);
        }
        McpSettingsView::Server { server_index } => {
            let Some(server) = settings.servers().get(server_index).cloned() else {
                return;
            };
            let options = settings.server_menu_options(&server);
            let selected = settings.lifecycle().selected_for(options.len());
            let detail_lines = mcp_server_detail_lines(language, &server);
            let detail_height = u16::try_from(detail_lines.len())
                .unwrap_or(u16::MAX)
                .min(geometry.list.height.saturating_sub(1));
            let [detail_area, menu_area] =
                Layout::vertical([Constraint::Length(detail_height), Constraint::Min(1)])
                    .areas(geometry.list);
            frame.render_widget(
                Paragraph::new(detail_lines)
                    .style(Style::default().fg(theme.text_primary).bg(theme.bg_dark))
                    .wrap(Wrap { trim: false }),
                detail_area,
            );
            let rows = options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    (
                        mcp_menu_action_label(language, option.action).to_string(),
                        String::new(),
                        selected == Some(index),
                    )
                })
                .collect::<Vec<_>>();
            let mut hits = render_mcp_picker_rows(
                frame,
                menu_area,
                settings.lifecycle_mut(),
                &rows,
                theme,
                language.text(
                    "当前服务器状态没有可用操作。",
                    "No action is available from the projected server state.",
                ),
            );
            hits.close_button = geometry.close_button;
            settings.lifecycle_mut().hit_areas = Some(hits);
        }
        McpSettingsView::Tools { server_index } => {
            let Some(server) = settings.servers().get(server_index).cloned() else {
                return;
            };
            let selected = settings.lifecycle().selected_for(server.tools.len());
            let rows = server
                .tools
                .iter()
                .enumerate()
                .map(|(index, tool)| {
                    (
                        sanitize_bounded_terminal_text(&tool.name).into_owned(),
                        mcp_tool_annotations(language, tool),
                        selected == Some(index),
                    )
                })
                .collect::<Vec<_>>();
            let mut hits = render_mcp_picker_rows(
                frame,
                geometry.list,
                settings.lifecycle_mut(),
                &rows,
                theme,
                language.text(
                    "当前服务器没有可显示的工具。",
                    "No tools are projected for this server.",
                ),
            );
            hits.close_button = geometry.close_button;
            settings.lifecycle_mut().hit_areas = Some(hits);
        }
        McpSettingsView::ToolDetail {
            server_index,
            tool_index,
        } => {
            let Some(tool) = settings
                .servers()
                .get(server_index)
                .and_then(|server| server.tools.get(tool_index))
                .cloned()
            else {
                return;
            };
            let mut body = vec![
                format!(
                    "{}: {}",
                    language.text("工具名称", "Tool name"),
                    sanitize_bounded_terminal_text(&tool.name)
                ),
                format!(
                    "{}: {}",
                    language.text("注解", "Annotations"),
                    mcp_tool_annotations(language, &tool)
                ),
            ];
            if let Some(description) = tool.description.as_deref() {
                body.push(String::new());
                body.push(language.text("描述：", "Description:").to_string());
                body.extend(
                    sanitize_bounded_terminal_text(description)
                        .lines()
                        .map(str::to_string),
                );
            }
            body.push(String::new());
            body.push(
                language
                    .text(
                        "现有 mcp_status 控制未投影参数 schema。",
                        "Parameter schema is not projected by the existing mcp_status control.",
                    )
                    .to_string(),
            );
            frame.render_widget(
                Paragraph::new(body.join("\n"))
                    .style(Style::default().fg(theme.text_primary).bg(theme.bg_dark))
                    .wrap(Wrap { trim: false })
                    .scroll((
                        u16::try_from(settings.detail_scroll()).unwrap_or(u16::MAX),
                        0,
                    )),
                geometry.list,
            );
            settings.lifecycle_mut().hit_areas = Some(PickerHitAreas {
                close_button: geometry.close_button,
                ..empty_picker_hit_areas()
            });
        }
    }

    if let Some(footer) = geometry.footer {
        let hint = match view {
            McpSettingsView::List => language.text(
                "↑/↓ 导航 · Enter 管理 · Esc 关闭",
                "↑/↓ navigate · Enter manage · Esc close",
            ),
            McpSettingsView::Server { .. } => language.text(
                "↑/↓ 导航 · Enter 选择 · Esc 返回",
                "↑/↓ navigate · Enter select · Esc back",
            ),
            McpSettingsView::Tools { .. } => language.text(
                "↑/↓ 导航 · Enter 查看详情 · Esc 返回",
                "↑/↓ navigate · Enter details · Esc back",
            ),
            McpSettingsView::ToolDetail { .. } => language.text(
                "↑/↓ 或 PgUp/PgDn 滚动 · Esc 返回",
                "↑/↓ or PgUp/PgDn scroll · Esc back",
            ),
        };
        frame.render_widget(
            Paragraph::new(fit_line_to_width(
                Line::styled(hint, Style::default().fg(theme.gray)),
                usize::from(footer.width),
            )),
            footer,
        );
    }
}

fn render_mcp_picker_rows(
    frame: &mut Frame<'_>,
    area: Rect,
    lifecycle: &mut PickerState,
    rows: &[(String, String, bool)],
    theme: CrabCodeTheme,
    empty_message: &str,
) -> PickerHitAreas {
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("  {empty_message}"))
                .style(Style::default().fg(theme.warning).bg(theme.bg_dark)),
            area,
        );
        return PickerHitAreas {
            item_rects: Vec::new(),
            entry_indices: Vec::new(),
            ..empty_picker_hit_areas()
        };
    }
    let entries = rows
        .iter()
        .map(|(label, right_label, selected)| {
            PickerEntry::Row(PickerRow::simple(label, right_label, *selected))
        })
        .collect::<Vec<_>>();
    let content_hits = render_picker_content(
        frame.buffer_mut(),
        area,
        &theme,
        lifecycle,
        &entries,
        &[],
        &[],
        Some(theme.bg_dark),
        false,
    );
    PickerHitAreas {
        item_rects: content_hits.item_rects,
        entry_indices: content_hits.entry_indices,
        ..empty_picker_hit_areas()
    }
}

fn mcp_menu_action_label(language: UiLanguage, action: McpMenuAction) -> &'static str {
    match action {
        McpMenuAction::ViewTools => language.text("查看工具", "View tools"),
        McpMenuAction::ClearAuthentication => language.text("清除身份验证", "Clear authentication"),
        McpMenuAction::Reconnect => language.text("重新连接", "Reconnect"),
        McpMenuAction::Enable => language.text("启用", "Enable"),
        McpMenuAction::Disable => language.text("停用", "Disable"),
    }
}

fn mcp_status_label(language: UiLanguage, status: &str) -> &'static str {
    match status {
        "connected" => language.text("✓ 已连接", "✓ connected"),
        "disabled" => language.text("○ 已停用", "○ disabled"),
        "pending" => language.text("○ 正在连接…", "○ connecting…"),
        "needs-auth" => language.text("△ 需要身份验证", "△ needs authentication"),
        "failed" => language.text("× 失败", "× failed"),
        _ => language.text("未知", "unknown"),
    }
}

fn mcp_server_detail_lines(
    language: UiLanguage,
    server: &crate::tui_app::McpServerState,
) -> Vec<Line<'static>> {
    let mut fields = vec![format!(
        "{}: {}",
        language.text("状态", "Status"),
        mcp_status_label(language, &server.status)
    )];
    if let Some(config) = server.config.as_ref() {
        fields.push(format!(
            "{}: {}",
            language.text("传输方式", "Transport"),
            config.transport.label()
        ));
        if let Some(url) = config.url.as_deref() {
            fields.push(format!("URL: {}", sanitize_bounded_terminal_text(url)));
        }
        if let Some(command) = config.command.as_deref() {
            fields.push(format!(
                "{}: {}",
                language.text("命令", "Command"),
                sanitize_bounded_terminal_text(command)
            ));
        }
        if !config.args.is_empty() {
            fields.push(format!(
                "{}: {}",
                language.text("参数", "Args"),
                sanitize_bounded_terminal_text(&config.args.join(" "))
            ));
        }
        if let Some(sdk_name) = config.sdk_name.as_deref() {
            fields.push(format!(
                "{}: {}",
                language.text("SDK 名称", "SDK name"),
                sanitize_bounded_terminal_text(sdk_name)
            ));
        }
    }
    if let Some(scope) = server.scope.as_deref() {
        fields.push(format!(
            "{}: {}",
            language.text("范围", "Scope"),
            sanitize_bounded_terminal_text(scope)
        ));
    }
    if let Some(info) = server.server_info.as_ref() {
        fields.push(format!(
            "{}: {} {}",
            language.text("服务器", "Server"),
            sanitize_bounded_terminal_text(&info.name),
            sanitize_bounded_terminal_text(&info.version)
        ));
    }
    if let Some(lifecycle) = server.lifecycle_status.as_deref() {
        fields.push(format!(
            "{}: {}",
            language.text("生命周期", "Lifecycle"),
            sanitize_bounded_terminal_text(lifecycle)
        ));
    }
    if let Some(reason) = server.lifecycle_reason.as_deref() {
        fields.push(format!(
            "{}: {}",
            language.text("原因", "Reason"),
            sanitize_bounded_terminal_text(reason)
        ));
    }
    if let Some(error) = server.error.as_deref() {
        fields.push(format!(
            "{}: {}",
            language.text("错误", "Error"),
            sanitize_bounded_terminal_text(error)
        ));
    }
    fields
        .into_iter()
        .map(|field| Line::styled(field, Style::default()))
        .collect()
}

fn mcp_tool_annotations(language: UiLanguage, tool: &crate::tui_app::McpToolState) -> String {
    let mut labels = Vec::new();
    if tool.annotations.read_only == Some(true) {
        labels.push(language.text("只读", "read-only"));
    }
    if tool.annotations.destructive == Some(true) {
        labels.push(language.text("破坏性", "destructive"));
    }
    if tool.annotations.open_world == Some(true) {
        labels.push(language.text("开放世界", "open-world"));
    }
    if labels.is_empty()
        && tool.annotations.read_only.is_none()
        && tool.annotations.destructive.is_none()
        && tool.annotations.open_world.is_none()
    {
        language
            .text("注解不可用", "annotations unavailable")
            .to_string()
    } else {
        labels.join(", ")
    }
}

fn render_overlay(frame: &mut Frame<'_>, area: Rect, app: &mut TuiApp, theme: CrabCodeTheme) {
    if app
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.kind == OverlayKind::GoalConsole)
    {
        return;
    }
    let language = app.ui_language();
    let theme_kind = app.renderer_theme_kind();
    let Some(overlay) = app.overlay.as_mut() else {
        return;
    };
    if !overlay.lifecycle.visible {
        return;
    }
    let host_area = if embedded() || overlay.lifecycle.fullscreen {
        area
    } else {
        centered(area, 86, 82)
    };
    let is_help =
        overlay.kind == crate::tui_app::OverlayKind::Help && !overlay.help_tabs.is_empty();
    let safe_title = sanitize_bounded_terminal_text(&overlay.title);
    let safe_tabs = overlay
        .help_tabs
        .iter()
        .map(|tab| sanitize_bounded_terminal_text(tab.label))
        .collect::<Vec<_>>();
    let tab_labels = safe_tabs
        .iter()
        .map(|label| label.as_ref())
        .collect::<Vec<_>>();
    let shortcuts = [
        Shortcut {
            label: language.text("↑/↓ 滚动", "↑/↓ scroll"),
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: language.text("Ctrl-F 全屏", "Ctrl-F fullscreen"),
            clickable: false,
            id: 1,
        },
        Shortcut {
            label: language.text("Esc 关闭", "Esc close"),
            clickable: false,
            id: 2,
        },
    ];
    // The product overlay already owns its normal/fullscreen host rectangle.
    // Exact-fill sizing lets the shared window lifecycle own all chrome inside
    // that rectangle without changing CrabCode's nested fullscreen semantics.
    let sizing = ModalSizing {
        width_pct: 1.0,
        max_width: host_area.width,
        min_width: host_area.width,
        v_margin: 0,
        ..ModalSizing::large()
    };
    let config = ModalWindowConfig {
        title: safe_title.as_ref(),
        tabs: is_help.then_some(tab_labels.as_slice()),
        shortcuts: &shortcuts,
        sizing,
        fold_info: None,
    };
    let Some(chrome) = render_modal_window(
        frame.buffer_mut(),
        host_area,
        &mut overlay.window,
        &config,
        &theme,
    ) else {
        // No content was painted, so scrolling must not retain stale viewport
        // authority from a larger prior frame.
        overlay.body_viewport_height = None;
        return;
    };

    // Shared chrome geometry is the sole viewport authority for product body
    // scrolling; the caller continues to own the body and its persistent
    // offset.
    overlay.set_body_viewport_height(chrome.content.height);
    let content_width = usize::from(chrome.content.width);
    let lines = if let Some(context) = &overlay.context_visualization {
        context
            .styled_lines(
                theme,
                theme_kind,
                color_support::detect(),
                renderer_language(language),
            )
            .into_iter()
            .skip(overlay.scroll)
            .map(|line| fit_line_to_width(line, content_width))
            .collect::<Vec<_>>()
    } else {
        overlay
            .active_body()
            .split('\n')
            .skip(overlay.scroll)
            .map(|line| {
                let safe = sanitize_bounded_terminal_text(line);
                Line::styled(
                    truncate_str(&safe, content_width),
                    Style::default().fg(theme.text_secondary),
                )
            })
            .collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(lines), chrome.content);
}

fn question_preview_lines(preview: &str, theme: CrabCodeTheme, width: usize) -> Vec<Line<'static>> {
    let safe_preview = sanitize_bounded_terminal_text(preview);
    let expanded = expand_pinned_markdown_tabs(&safe_preview);
    let mut renderer = crabcode_markdown_renderer::StreamingMarkdownRenderer::new(
        crabcode_markdown_style(theme),
        true,
    );
    renderer.push(&expanded);
    renderer.finish(Some(crabcode_markdown_syntax_highlighter(
        CrabCodeSyntaxTheme::MonokaiExtended,
    )));
    renderer
        .view()
        .lines
        .iter()
        .flat_map(|line| wrap_line(line, width.max(1)))
        .collect()
}

fn render_dialog(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut TuiApp,
    theme: CrabCodeTheme,
) -> Option<(u16, u16)> {
    let modal = centered(area, 90, 82);
    let language = app.ui_language();
    let response_inflight = app.dialog_response_inflight();
    let setup_ctrl_c_confirmation_active = app.setup_ctrl_c_confirmation_active();
    app.dialog_pointer.begin_frame();
    frame.render_widget(Clear, modal);
    match app.dialog.as_mut() {
        Some(RequestDialog::Permission {
            tool_name,
            display_name,
            description,
            input,
            choices,
            selected,
            ..
        }) => {
            let labels = choices
                .iter()
                .map(|choice| match choice {
                    crate::tui_app::PermissionChoice::AllowOnce => {
                        language.text("仅允许一次", "Allow once")
                    }
                    crate::tui_app::PermissionChoice::AllowSession => {
                        language.text("本次会话按规则允许", "Allow by rule for session")
                    }
                    crate::tui_app::PermissionChoice::AllowAlways => {
                        language.text("保存规则并允许", "Save rule and allow")
                    }
                    crate::tui_app::PermissionChoice::Deny => language.text("拒绝", "Deny"),
                })
                .collect::<Vec<_>>();
            let selected = *selected;
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .style(Style::default().bg(theme.bg_dark))
                .title(if response_inflight {
                    language.text(
                        " 权限 · 正在提交响应… ",
                        " Permission · delivering response… ",
                    )
                } else {
                    language.text(
                        " 权限 · ↑/↓ 选择 · Enter 确认 · 双击确认 · Esc 拒绝 ",
                        " Permission · ↑/↓ choose · Enter confirm · double-click confirm · Esc deny ",
                    )
                });
            let inner = block.inner(modal);
            let [body_area, choices_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let mut lines = vec![
                Line::styled(
                    sanitize_bounded_terminal_text(display_name.as_deref().unwrap_or(tool_name))
                        .into_owned(),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    sanitize_bounded_terminal_text(description.as_deref().unwrap_or(
                        language.text("此工具需要你的批准。", "This tool requires approval."),
                    ))
                    .into_owned(),
                    Style::default().fg(theme.gray_bright),
                ),
                Line::default(),
            ];
            let input_body = localized_plan_approval_body(language, tool_name, input)
                .unwrap_or_else(|| bounded_json(input));
            for line in input_body.split('\n') {
                lines.push(Line::styled(
                    sanitize_bounded_terminal_text(line).into_owned(),
                    Style::default().fg(theme.markdown_code),
                ));
            }
            frame.render_widget(block, modal);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
            frame.render_widget(
                Paragraph::new(Line::from(
                    labels
                        .iter()
                        .enumerate()
                        .flat_map(|(index, label)| {
                            [
                                Span::styled(
                                    if index == selected { " [" } else { "  " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                                Span::styled(
                                    (*label).to_string(),
                                    Style::default()
                                        .fg(if index == selected {
                                            theme.text_primary
                                        } else {
                                            theme.gray
                                        })
                                        .add_modifier(if index == selected {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                                Span::styled(
                                    if index == selected { "]" } else { " " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )),
                choices_area,
            );
            app.dialog_pointer
                .set_choice_areas(crate::dialog_interaction::inline_choice_areas(
                    choices_area,
                    &labels,
                ));
            None
        }
        Some(RequestDialog::Question(dialog)) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .style(Style::default().bg(theme.bg_dark))
                .title(if response_inflight {
                    language.text(
                        " 问题 · 正在提交回答… ",
                        " Question · delivering response… ",
                    )
                } else {
                    language.text(
                        " 问题 · ↑/↓ 选择 · Space 切换 · Tab 操作 · PgUp/PgDn 预览 · Esc 跳过 ",
                        " Question · ↑/↓ choose · Space toggle · Tab actions · PgUp/PgDn preview · Esc decline ",
                    )
                });
            let inner = block.inner(modal).inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            frame.render_widget(block, modal);

            let current = dialog.current;
            let focus = dialog.focus;
            let action_index = dialog.action_index;
            let question_count = dialog.questions.len();
            let actions = question_dialog_actions(dialog);
            let question = &dialog.questions[current];
            let answer = &mut dialog.answers[current];
            let other_height = if answer.other_selected { 4 } else { 0 };
            let error_height = u16::from(dialog.validation_error.is_some());
            let [
                heading_area,
                body_area,
                other_area,
                error_area,
                actions_area,
            ] = Layout::vertical([
                Constraint::Length(3.min(inner.height)),
                Constraint::Min(3),
                Constraint::Length(other_height),
                Constraint::Length(error_height),
                Constraint::Length(2.min(inner.height)),
            ])
            .areas(inner);

            let mut heading = vec![Line::from(vec![
                Span::styled(
                    format!(" {} ", sanitize_bounded_terminal_text(&question.header)),
                    Style::default()
                        .fg(theme.bg_base)
                        .bg(theme.accent_assistant)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}/{}", current + 1, question_count),
                    Style::default().fg(theme.gray),
                ),
            ])];
            if let Some(agent_id) = dialog.agent_id.as_deref() {
                heading[0].spans.push(Span::styled(
                    format!(
                        "  {}: {}",
                        language.text("来自", "from"),
                        sanitize_bounded_terminal_text(agent_id)
                    ),
                    Style::default().fg(theme.text_secondary),
                ));
            }
            heading.push(Line::styled(
                sanitize_bounded_terminal_text(&question.question).into_owned(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(
                Paragraph::new(heading).wrap(Wrap { trim: false }),
                heading_area,
            );

            let selected_preview = question
                .options
                .get(answer.cursor)
                .and_then(|option| option.preview.as_deref());
            let option_rows = u16::try_from(question.options.len().saturating_add(1))
                .unwrap_or(u16::MAX)
                .min(body_area.height);
            let (options_area, preview_area) = if selected_preview.is_some() {
                if body_area.width >= 72 {
                    let [options, _, preview] = Layout::horizontal([
                        Constraint::Percentage(43),
                        Constraint::Length(1),
                        Constraint::Min(20),
                    ])
                    .areas(body_area);
                    (options, Some(preview))
                } else {
                    let [options, preview] =
                        Layout::vertical([Constraint::Length(option_rows), Constraint::Min(2)])
                            .areas(body_area);
                    (options, Some(preview))
                }
            } else {
                (body_area, None)
            };

            let mut pointer_areas = Vec::new();
            for index in 0..question.options.len().saturating_add(1) {
                if index >= usize::from(options_area.height) {
                    break;
                }
                let row = Rect::new(
                    options_area.x,
                    options_area.y.saturating_add(index as u16),
                    options_area.width,
                    1,
                );
                pointer_areas.push(row);
                let (label, description, selected) =
                    if let Some(option) = question.options.get(index) {
                        let mut label = sanitize_bounded_terminal_text(&option.label).into_owned();
                        if option.recommended {
                            label.push_str(language.text(" · 推荐", " · Recommended"));
                        }
                        (
                            label,
                            sanitize_bounded_terminal_text(&option.description).into_owned(),
                            answer.selected.get(index).copied().unwrap_or(false),
                        )
                    } else {
                        (
                            language
                                .text("其他（自行输入）", "Other (type your own)")
                                .to_string(),
                            language
                                .text("输入上述选项之外的答案", "Provide a custom answer")
                                .to_string(),
                            answer.other_selected,
                        )
                    };
                let marker = if question.multi_select {
                    if selected { "[x]" } else { "[ ]" }
                } else if selected {
                    "(●)"
                } else {
                    "( )"
                };
                let active = focus == QuestionFocus::Options && answer.cursor == index;
                let line = Line::from(vec![
                    Span::styled(
                        if active { "> " } else { "  " },
                        Style::default().fg(theme.accent_assistant),
                    ),
                    Span::styled(
                        format!("{marker} {label}"),
                        Style::default()
                            .fg(if active {
                                theme.text_primary
                            } else {
                                theme.gray_bright
                            })
                            .add_modifier(if active {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(format!(" — {description}"), Style::default().fg(theme.gray)),
                ]);
                frame.render_widget(
                    Paragraph::new(fit_line_to_width(line, usize::from(row.width))),
                    row,
                );
            }

            if let (Some(preview), Some(preview_area)) = (selected_preview, preview_area) {
                let preview_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.gray_dim))
                    .title(language.text(" 预览 ", " Preview "));
                let preview_inner = preview_block.inner(preview_area).inner(Margin {
                    horizontal: 1,
                    vertical: 0,
                });
                frame.render_widget(preview_block, preview_area);
                let width = usize::from(preview_inner.width.max(1));
                let preview_lines = question_preview_lines(preview, theme, width);
                dialog.preview_page_rows = usize::from(preview_inner.height.max(1));
                dialog.preview_max_scroll =
                    preview_lines.len().saturating_sub(dialog.preview_page_rows);
                dialog.preview_scroll = dialog.preview_scroll.min(dialog.preview_max_scroll);
                let visible_preview_lines = preview_lines
                    .into_iter()
                    .skip(dialog.preview_scroll)
                    .take(dialog.preview_page_rows)
                    .collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(visible_preview_lines), preview_inner);
                app.dialog_pointer.set_preview_area(preview_inner);
            } else {
                dialog.preview_scroll = 0;
                dialog.preview_max_scroll = 0;
                dialog.preview_page_rows = 1;
            }

            let cursor = if answer.other_selected && other_area.area() > 0 {
                let other_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if focus == QuestionFocus::OtherInput {
                        theme.prompt_border_active
                    } else {
                        theme.gray_dim
                    }))
                    .title(language.text(" 其他答案 ", " Other answer "));
                let other_inner = other_block.inner(other_area).inner(Margin {
                    horizontal: 1,
                    vertical: 0,
                });
                frame.render_widget(other_block, other_area);
                app.dialog_pointer.set_input_area(other_inner);
                frame.render_stateful_widget_ref(
                    answer.other_input.as_ref(),
                    other_inner,
                    &mut answer.other_input_state,
                );
                (focus == QuestionFocus::OtherInput && !response_inflight)
                    .then(|| {
                        answer
                            .other_input
                            .cursor_pos_with_state(other_inner, answer.other_input_state)
                    })
                    .flatten()
            } else {
                None
            };

            if let Some(error) = dialog.validation_error.as_deref() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        sanitize_bounded_terminal_text(error).into_owned(),
                        Style::default().fg(theme.accent_error),
                    )),
                    error_area,
                );
            }

            let action_labels = actions
                .iter()
                .map(|action| match action {
                    QuestionDialogAction::Previous => language.text("上一题", "Previous"),
                    QuestionDialogAction::Next => language.text("下一题", "Next"),
                    QuestionDialogAction::Submit => language.text("提交", "Submit"),
                    QuestionDialogAction::Decline => language.text("跳过回答", "Decline"),
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(Line::from(
                    action_labels
                        .iter()
                        .enumerate()
                        .flat_map(|(index, label)| {
                            let active = focus == QuestionFocus::Actions && index == action_index;
                            [
                                Span::styled(
                                    if active { " [" } else { "  " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                                Span::styled(
                                    (*label).to_string(),
                                    Style::default()
                                        .fg(if active {
                                            theme.text_primary
                                        } else {
                                            theme.gray
                                        })
                                        .add_modifier(if active {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                                Span::styled(
                                    if active { "]" } else { " " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )),
                actions_area,
            );
            pointer_areas.extend(crate::dialog_interaction::inline_choice_areas(
                actions_area,
                &action_labels,
            ));
            app.dialog_pointer.set_choice_areas(pointer_areas);
            cursor
        }
        Some(RequestDialog::Elicitation {
            server_name,
            message,
            mode,
            url,
            schema,
            input,
            input_state,
            action_index,
            validation_error,
            ..
        }) => {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .style(Style::default().bg(theme.bg_dark))
                .title(if response_inflight {
                    language.text(
                        " MCP 输入 · 正在提交响应… ",
                        " MCP input · delivering response… ",
                    )
                } else {
                    language.text(
                        " MCP 输入 · Tab 切换操作 · Enter 确认 · Esc 取消 ",
                        " MCP input · Tab action · Enter confirm · Esc cancel ",
                    )
                });
            let inner = block.inner(modal).inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            frame.render_widget(block, modal);
            let editor_height = if mode == "url" {
                0
            } else {
                5.min(inner.height)
            };
            let [description_area, editor_area, choices_area] = Layout::vertical([
                Constraint::Min(2),
                Constraint::Length(editor_height),
                Constraint::Length(2),
            ])
            .areas(inner);
            let mut lines = vec![
                Line::styled(
                    sanitize_bounded_terminal_text(server_name).into_owned(),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    sanitize_bounded_terminal_text(message).into_owned(),
                    Style::default().fg(theme.text_secondary),
                ),
            ];
            if let Some(url) = url {
                lines.push(Line::styled(
                    sanitize_bounded_terminal_text(url).into_owned(),
                    Style::default()
                        .fg(theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ));
            }
            if let Some(schema) = schema {
                lines.push(Line::styled(
                    format!(
                        "{}: {}",
                        language.text("结构", "schema"),
                        compact_json(schema)
                    ),
                    Style::default().fg(theme.gray),
                ));
            }
            if let Some(error) = validation_error {
                lines.push(Line::styled(
                    sanitize_bounded_terminal_text(error).into_owned(),
                    Style::default().fg(theme.accent_error),
                ));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                description_area,
            );
            let cursor = if mode != "url" {
                frame.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.prompt_border_active)),
                    editor_area,
                );
                let edit_inner = editor_area.inner(Margin {
                    horizontal: 1,
                    vertical: 1,
                });
                app.dialog_pointer.set_input_area(edit_inner);
                frame.render_stateful_widget_ref(input.as_ref(), edit_inner, input_state);
                (!response_inflight)
                    .then(|| input.cursor_pos_with_state(edit_inner, *input_state))
                    .flatten()
            } else {
                None
            };
            let actions = [
                language.text("接受", "Accept"),
                language.text("拒绝", "Decline"),
                language.text("取消", "Cancel"),
            ];
            frame.render_widget(
                Paragraph::new(Line::from(
                    actions
                        .iter()
                        .enumerate()
                        .flat_map(|(index, action)| {
                            [
                                Span::styled(
                                    if index == *action_index { " [" } else { "  " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                                Span::styled(
                                    (*action).to_string(),
                                    Style::default()
                                        .fg(if index == *action_index {
                                            theme.text_primary
                                        } else {
                                            theme.gray
                                        })
                                        .add_modifier(if index == *action_index {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                                Span::styled(
                                    if index == *action_index { "]" } else { " " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )),
                choices_area,
            );
            app.dialog_pointer
                .set_choice_areas(crate::dialog_interaction::inline_choice_areas(
                    choices_area,
                    &actions,
                ));
            cursor
        }
        Some(RequestDialog::GroveTerms {
            title,
            body,
            links,
            options,
            selected,
            dismissable,
            ..
        }) => {
            let mut lines = vec![Line::styled(
                sanitize_bounded_terminal_text(title).into_owned(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            )];
            lines.push(Line::default());
            lines.extend(body.iter().map(|line| {
                Line::styled(
                    sanitize_bounded_terminal_text(line).into_owned(),
                    Style::default().fg(theme.text_secondary),
                )
            }));
            if !links.is_empty() {
                lines.push(Line::default());
                lines.push(Line::styled(
                    language.text("参考链接", "References"),
                    Style::default()
                        .fg(theme.gray_bright)
                        .add_modifier(Modifier::BOLD),
                ));
                for (index, link) in links.iter().enumerate() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}. ", index + 1),
                            Style::default().fg(theme.accent_assistant),
                        ),
                        Span::styled(
                            sanitize_bounded_terminal_text(&link.label).into_owned(),
                            Style::default()
                                .fg(theme.link)
                                .add_modifier(Modifier::UNDERLINED),
                        ),
                        Span::styled(" · ", Style::default().fg(theme.gray_dim)),
                        Span::styled(
                            sanitize_bounded_terminal_text(&link.url).into_owned(),
                            Style::default().fg(theme.gray),
                        ),
                    ]));
                }
            }
            let labels = options
                .iter()
                .map(|option| sanitize_bounded_terminal_text(&option.label).into_owned())
                .collect::<Vec<_>>();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .style(Style::default().bg(theme.bg_dark))
                .title(if response_inflight {
                    language
                        .text(
                            " 条款更新 · 正在提交响应… ",
                            " Terms update · delivering response… ",
                        )
                        .to_string()
                } else {
                    format!(
                        " {} · {} · {}{} ",
                        language.text("条款更新", "Terms update"),
                        language.text(
                            "↑/↓ 选择 · Enter/双击确认",
                            "↑/↓ choose · Enter/double-click confirm",
                        ),
                        if *dismissable {
                            language.text("Esc 稍后处理", "Esc defer")
                        } else {
                            language.text("Esc 退出", "Esc exit")
                        },
                        if links.is_empty() {
                            String::new()
                        } else {
                            language
                                .text(" · 1-8 打开参考链接", " · 1-8 open reference")
                                .to_string()
                        }
                    )
                });
            let inner = block.inner(modal);
            let [body_area, choices_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            frame.render_widget(block, modal);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
            frame.render_widget(
                Paragraph::new(Line::from(
                    labels
                        .iter()
                        .enumerate()
                        .flat_map(|(index, label)| {
                            [
                                Span::styled(
                                    if index == *selected { " [" } else { "  " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                                Span::styled(
                                    label.clone(),
                                    Style::default()
                                        .fg(if index == *selected {
                                            theme.text_primary
                                        } else {
                                            theme.gray
                                        })
                                        .add_modifier(if index == *selected {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                                Span::styled(
                                    if index == *selected { "]" } else { " " },
                                    Style::default().fg(theme.accent_assistant),
                                ),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )),
                choices_area,
            );
            let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
            app.dialog_pointer
                .set_choice_areas(crate::dialog_interaction::inline_choice_areas(
                    choices_area,
                    &label_refs,
                ));
            None
        }
        Some(RequestDialog::SetupInput(dialog)) => {
            let block = setup_block_with_quit_confirmation(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.warning))
                    .style(Style::default().bg(theme.bg_dark))
                    .title(if response_inflight {
                        language.text(
                            " CrabCode 设置 · 正在提交响应… ",
                            " CrabCode setup · delivering response… ",
                        )
                    } else {
                        language.text(
                            " CrabCode 设置 · Enter 提交 · Esc 返回 ",
                            " CrabCode setup · Enter submit · Esc back ",
                        )
                    }),
                setup_ctrl_c_confirmation_active,
                language,
                theme,
            );
            let inner = block.inner(modal).inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            frame.render_widget(block, modal);
            let [description_area, editor_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);
            let mut lines = vec![
                Line::styled(
                    sanitize_bounded_terminal_text(&dialog.title).into_owned(),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::default(),
            ];
            lines.extend(dialog.body.iter().map(|line| {
                Line::styled(
                    sanitize_bounded_terminal_text(line).into_owned(),
                    Style::default().fg(theme.text_secondary),
                )
            }));
            if let Some(error) = &dialog.validation_error {
                lines.push(Line::styled(
                    sanitize_bounded_terminal_text(error).into_owned(),
                    Style::default().fg(theme.accent_error),
                ));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                description_area,
            );
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.prompt_border_active)),
                editor_area,
            );
            let edit_inner = editor_area.inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            app.dialog_pointer.set_input_area(edit_inner);
            let raw = dialog.input.text();
            let raw_cursor = dialog.input.cursor().min(raw.len());
            let (display_text, display_cursor) = if dialog.masked {
                let display_text = "*".repeat(raw.graphemes(true).count());
                let display_cursor = raw[..raw_cursor].graphemes(true).count();
                (display_text, display_cursor)
            } else {
                let display_text = sanitize_terminal_text(raw).into_owned();
                let display_cursor = sanitize_terminal_text(&raw[..raw_cursor]).len();
                (display_text, display_cursor)
            };
            let mut display_input = crabcode_ratatui_textarea::TextArea::new();
            display_input.set_text(&display_text);
            display_input.set_cursor(display_cursor.min(display_text.len()));
            frame.render_stateful_widget_ref(&display_input, edit_inner, &mut dialog.input_state);
            (!response_inflight)
                .then(|| display_input.cursor_pos_with_state(edit_inner, dialog.input_state))
                .flatten()
        }
        Some(RequestDialog::Setup(dialog)) => {
            let content_height = usize::from(modal.height.saturating_sub(2).max(1));
            let mut fixed_lines = vec![Line::styled(
                sanitize_bounded_terminal_text(&dialog.title).into_owned(),
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            )];
            fixed_lines.push(Line::default());
            fixed_lines.extend(dialog.body.iter().map(|line| {
                Line::styled(
                    sanitize_bounded_terminal_text(line).into_owned(),
                    Style::default().fg(theme.text_secondary),
                )
            }));
            if let crate::tui_app::SetupDialogKind::OnboardingTheme {
                syntax_highlighting_disabled,
                syntax_toggle_enabled,
            } = &dialog.kind
            {
                fixed_lines.push(Line::default());
                fixed_lines.push(Line::from(vec![
                    Span::styled(
                        "fn ",
                        Style::default().fg(if *syntax_highlighting_disabled {
                            theme.text_secondary
                        } else {
                            theme.markdown_h2
                        }),
                    ),
                    Span::styled(
                        "crabcode",
                        Style::default().fg(if *syntax_highlighting_disabled {
                            theme.text_secondary
                        } else {
                            theme.accent_assistant
                        }),
                    ),
                    Span::styled("() { ", Style::default().fg(theme.text_secondary)),
                    Span::styled(
                        "\"safe preview\"",
                        Style::default().fg(if *syntax_highlighting_disabled {
                            theme.text_secondary
                        } else {
                            theme.accent_success
                        }),
                    ),
                    Span::styled(" }", Style::default().fg(theme.text_secondary)),
                ]));
                fixed_lines.push(Line::styled(
                    if *syntax_toggle_enabled {
                        format!(
                            "{}：{} · {}",
                            language.text("语法高亮", "Syntax highlighting"),
                            if *syntax_highlighting_disabled {
                                language.text("已关闭", "disabled")
                            } else {
                                language.text("已开启", "enabled")
                            },
                            language.text("Ctrl+T 切换预览", "Ctrl+T toggles preview")
                        )
                    } else {
                        format!(
                            "{}：{} · {}",
                            language.text("语法高亮", "Syntax highlighting"),
                            if *syntax_highlighting_disabled {
                                language.text("已关闭", "disabled")
                            } else {
                                language.text("已开启", "enabled")
                            },
                            language.text("由环境锁定", "locked by environment")
                        )
                    },
                    Style::default().fg(theme.gray_bright),
                ));
            }
            if !dialog.links.is_empty() {
                fixed_lines.push(Line::default());
                for (index, link) in dialog.links.iter().enumerate() {
                    fixed_lines.push(Line::from(vec![
                        Span::styled(
                            format!("{}. ", index + 1),
                            Style::default().fg(theme.accent_assistant),
                        ),
                        Span::styled(
                            sanitize_bounded_terminal_text(&link.label).into_owned(),
                            Style::default()
                                .fg(theme.link)
                                .add_modifier(Modifier::UNDERLINED),
                        ),
                        Span::styled(" · ", Style::default().fg(theme.gray_dim)),
                        Span::styled(
                            sanitize_bounded_terminal_text(&link.url).into_owned(),
                            Style::default().fg(theme.gray),
                        ),
                    ]));
                }
            }

            let instructions = if response_inflight {
                language
                    .text(
                        " CrabCode 设置 · 正在提交响应… ",
                        " CrabCode setup · delivering response… ",
                    )
                    .to_string()
            } else {
                let interaction = match &dialog.kind {
                    crate::tui_app::SetupDialogKind::McpMultiple { .. } => language.text(
                        "↑/↓ 选择 · Space 切换 · Enter 确认 · Esc 全部拒绝",
                        "↑/↓ choose · Space toggle · Enter confirm · Esc reject all",
                    ),
                    crate::tui_app::SetupDialogKind::OnboardingOAuthError => {
                        language.text("Enter 重试", "Enter retry")
                    }
                    crate::tui_app::SetupDialogKind::OnboardingOAuthCustomProvider => language
                        .text(
                            "↑/↓ 选择 · Enter 确认 · Esc 返回",
                            "↑/↓ choose · Enter confirm · Esc back",
                        ),
                    crate::tui_app::SetupDialogKind::OnboardingOAuthSelect => {
                        language.text("↑/↓ 选择 · Enter 确认", "↑/↓ choose · Enter confirm")
                    }
                    crate::tui_app::SetupDialogKind::OnboardingTheme {
                        syntax_toggle_enabled,
                        ..
                    } => {
                        if *syntax_toggle_enabled {
                            language.text(
                                "↑/↓ 选择 · Ctrl+T 语法高亮 · Enter 确认",
                                "↑/↓ choose · Ctrl+T syntax · Enter confirm",
                            )
                        } else {
                            language.text("↑/↓ 选择 · Enter 确认", "↑/↓ choose · Enter confirm")
                        }
                    }
                    _ if dialog.choices.len() > 1 => language.text(
                        "↑/↓ 选择 · Enter 确认 · Esc 使用安全退出选项",
                        "↑/↓ choose · Enter confirm · Esc applies the safe exit choice",
                    ),
                    _ => language.text(
                        "Enter 继续 · 1-9 打开参考链接",
                        "Enter continue · 1-9 open reference",
                    ),
                };
                format!(
                    " {} · {interaction} ",
                    language.text("CrabCode 设置", "CrabCode setup")
                )
            };
            let block = setup_block_with_quit_confirmation(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.warning))
                    .style(Style::default().bg(theme.bg_dark))
                    .title(instructions),
                setup_ctrl_c_confirmation_active,
                language,
                theme,
            );
            let inner = block.inner(modal);
            let choice_rows = if dialog.choices.is_empty() {
                0
            } else {
                u16::try_from(dialog.choices.len().min(content_height.clamp(1, 8)))
                    .unwrap_or(u16::MAX)
                    .min(inner.height)
            };
            let body_and_gap_rows = inner.height.saturating_sub(choice_rows);
            let body_height = body_and_gap_rows
                .saturating_sub(u16::from(choice_rows > 0 && body_and_gap_rows > 0));
            let body_area = Rect::new(inner.x, inner.y, inner.width, body_height);
            let mut wrapped_lines = fixed_lines
                .iter()
                .flat_map(|line| wrap_line(line, usize::from(inner.width.max(1))))
                .collect::<Vec<_>>();
            let body_limit = usize::from(body_area.height);
            if wrapped_lines.len() > body_limit {
                wrapped_lines.truncate(body_limit);
                if let Some(last) = wrapped_lines.last_mut() {
                    *last = fit_line_to_width(
                        Line::styled(
                            language.text(
                                "… 更多设置详情已折叠，选项保持可见 …",
                                "… additional setup details clipped; choices remain visible …",
                            ),
                            Style::default().fg(theme.gray),
                        ),
                        usize::from(body_area.width),
                    );
                }
            }
            frame.render_widget(block, modal);
            frame.render_widget(Paragraph::new(wrapped_lines), body_area);
            if !dialog.choices.is_empty() && inner.height > 0 {
                let choice_y = inner.bottom().saturating_sub(choice_rows);
                let choice_area = Rect::new(
                    inner.x,
                    choice_y,
                    inner.width,
                    inner.bottom().saturating_sub(choice_y),
                );
                let labels = dialog
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        let prefix = if dialog.multi_selected.is_empty() {
                            ""
                        } else if dialog.multi_selected.get(index).copied().unwrap_or(false) {
                            "[x] "
                        } else {
                            "[ ] "
                        };
                        format!("{prefix}{}", sanitize_bounded_terminal_text(&choice.label))
                    })
                    .collect::<Vec<_>>();
                let entries = labels
                    .iter()
                    .enumerate()
                    .map(|(index, label)| {
                        PickerEntry::Row(PickerRow::simple(label, "", index == dialog.selected))
                    })
                    .collect::<Vec<_>>();
                if app.setup_picker.selected != dialog.selected {
                    app.setup_picker
                        .set_selected(dialog.selected, dialog.choices.len());
                }
                let content_hits = render_picker_content(
                    frame.buffer_mut(),
                    choice_area,
                    &theme,
                    &mut app.setup_picker,
                    &entries,
                    &[],
                    &[],
                    Some(theme.bg_dark),
                    false,
                );
                app.setup_picker.hit_areas = Some(PickerHitAreas {
                    item_rects: content_hits.item_rects,
                    entry_indices: content_hits.entry_indices,
                    ..empty_picker_hit_areas()
                });
            }
            None
        }
        None => None,
    }
}

/// Build the plan-approval body only from the exact SDK-facing tool input.
/// Missing and whitespace-only content produce an explicit notice instead of
/// a blank permission surface.
pub(crate) fn plan_approval_body(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    localized_plan_approval_body(UiLanguage::EnUs, tool_name, input)
}

fn localized_plan_approval_body(
    language: UiLanguage,
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<String> {
    if tool_name != "ExitPlanMode" {
        return None;
    }
    Some(
        input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .filter(|plan| !plan.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                language
                    .text(
                        "尚未编写计划。\n\n批准后将退出计划模式并开始实施；如需继续规划，请请求修改；也可拒绝并放弃。",
                        "No plan written yet.\n\nApprove to leave plan mode and start implementing, \
                         request changes to return to planning, or deny to abandon.",
                    )
                    .to_string()
            }),
    )
}

fn render_fatal(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: CrabCodeTheme) {
    let Some(fatal) = app.fatal.as_ref() else {
        return;
    };
    let language = app.ui_language();
    let latest_stderr = app.stderr_notices().next_back();
    let modal = centered(area, 84, if latest_stderr.is_some() { 60 } else { 45 });
    let mut lines = vec![
        Line::styled(
            language.text(
                "为防止语义漂移，直连运行环境已停止。",
                "The direct runtime was stopped to prevent semantic drift.",
            ),
            Style::default()
                .fg(theme.accent_error)
                .add_modifier(Modifier::BOLD),
        ),
        Line::default(),
    ];
    if let Some(stderr) = latest_stderr {
        lines.push(Line::from(vec![
            Span::styled(
                language.text("最新运行环境 stderr：", "Latest runtime stderr: "),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                sanitize_bounded_terminal_text(stderr).into_owned(),
                Style::default().fg(theme.warning),
            ),
        ]));
        lines.push(Line::default());
    }
    lines.extend([
        Line::styled(
            sanitize_bounded_terminal_text(fatal).into_owned(),
            Style::default().fg(theme.text_secondary),
        ),
        Line::default(),
        Line::styled(
            language.text(
                "近期诊断信息已保留 · 按 Ctrl-Q 退出。",
                "Recent diagnostics retained · Press Ctrl-Q to exit.",
            ),
            Style::default().fg(theme.gray),
        ),
    ]);
    frame.render_widget(Clear, modal);
    let title = if app.setup_surface_exclusive() {
        language.text(
            " 设置无法继续 · 协议兼容性失败 ",
            " Setup cannot continue · protocol compatibility failure ",
        )
    } else {
        language.text(" 协议兼容性失败 ", " Protocol compatibility failure ")
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent_error))
                .style(Style::default().bg(theme.bg_dark))
                .title(title),
        ),
        modal,
    );
}

fn composer_height(app: &TuiApp, width: u16) -> u16 {
    if !app.composer_enabled() {
        return 3;
    }
    let desired = app.composer.desired_height(width.saturating_sub(4).max(1));
    desired
        .saturating_add(2)
        .saturating_add(u16::from(!app.attachments.is_empty()))
        .clamp(3, 10)
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn bounded_json(value: &serde_json::Value) -> String {
    let value =
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unrenderable JSON>".to_string());
    sanitize_bounded_terminal_text(&value).into_owned()
}

fn compact_json(value: &serde_json::Value) -> String {
    let value = serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable JSON>".to_string());
    sanitize_bounded_terminal_text(&value).into_owned()
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use serde_json::{Value, json};

    use super::*;
    use crate::sdk_runtime::{EnvelopeClass, RawEnvelope, RuntimeEvent, SystemSubtype};
    use crate::transcript_search::{TranscriptSearchDocument, TranscriptSearchState};
    use crate::tui_app::{HostAction, InitialSessionRequest};

    fn render_buffer(app: &mut TuiApp, width: u16, height: u16) -> String {
        render_test_buffer(app, width, height)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn render_test_buffer(app: &mut TuiApp, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let _cursor = render(frame, app);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn buffer_row_texts(buffer: &Buffer) -> Vec<String> {
        (buffer.area.y..buffer.area.bottom())
            .map(|row| {
                (buffer.area.x..buffer.area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect()
            })
            .collect()
    }

    fn switch_to_english_and_initialize(app: &mut TuiApp) {
        app.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
            request_id: "language".to_string(),
            title: "Language / 语言".to_string(),
            body: vec!["Choose".to_string()],
            kind: crate::tui_app::SetupDialogKind::OnboardingLanguage,
            choices: vec![
                crate::tui_app::SetupChoice {
                    value: "zh-CN".to_string(),
                    label: "中文".to_string(),
                },
                crate::tui_app::SetupChoice {
                    value: "en-US".to_string(),
                    label: "English".to_string(),
                },
            ],
            selected: 1,
            links: Vec::new(),
            multi_selected: Vec::new(),
        }));
        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert_eq!(actions.len(), 1);
        app.action_admitted(&actions[0]);
        app.action_succeeded(&actions[0]);
        app.release_startup_barrier_for_test();
        assert_eq!(app.ui_language(), UiLanguage::EnUs);
    }

    #[test]
    fn retained_identity_repaints_current_composer_border_and_name_badge() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let effect = app
            .retained_commands
            .identity_snapshot()
            .expect("identity snapshot request");
        app.retained_commands
            .apply_result(
                effect.purpose,
                "retained.identity.snapshot",
                &json!({
                    "result": {
                        "kind":"retained.identity.snapshot",
                        "name":"会话甲",
                        "color":"purple"
                    }
                }),
                UiLanguage::ZhCn,
            )
            .expect("authoritative identity result");

        let buffer = render_test_buffer(&mut app, 80, 24);
        let expected_background = color_support::quantize(Color::Rgb(147, 51, 234));
        let expected_text = color_support::quantize(Color::Black);
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.fg == expected_background && cell.symbol() == "│"),
            "the current composer border must repaint with the committed agent color"
        );
        let name_cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "会" && cell.bg == expected_background)
            .expect("committed name badge");
        assert_eq!(name_cell.fg, expected_text);
        assert!(name_cell.modifier.contains(Modifier::BOLD));
        assert!(!app.should_quit);
    }

    fn projected_item(
        key: impl Into<String>,
        text: impl Into<String>,
        sequence: u64,
    ) -> ProjectedItem {
        ProjectedItem {
            key: key.into(),
            kind: ProjectedKind::Assistant,
            title: "Assistant".to_string(),
            text: text.into(),
            streaming: false,
            raw_sequences: vec![sequence],
            tool_use_id: None,
            presentation: crate::sdk_projection::ProjectedPresentation::default(),
        }
    }

    #[test]
    fn oauth_browser_banner_shows_verification_url_and_device_code() {
        let url = "https://accounts.example/device?user_code=ABCD-EFGH";
        let banner = oauth_browser_banner_for_notice(
            Some(OAuthBrowserNotice::Manual {
                message: "Open this URL in your browser to approve:",
                hint: "(c to copy)",
                copied: false,
                url,
            }),
            80,
            CrabCodeTheme::default(),
            UiLanguage::EnUs,
        );
        let text = banner
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(url));
        assert!(text.contains("Code: ABCD-EFGH"));
        assert_eq!(banner.url.as_deref(), Some(url));
        assert_eq!(oauth_device_user_code("https://example.test/device"), None);
        assert_eq!(
            oauth_device_user_code("https://example.test/device?user_code=bad%20code"),
            None
        );
    }

    #[test]
    fn transcript_search_highlight_spans_style_boundaries_exactly() {
        let base = Style::default().fg(Color::Green);
        let mut lines = vec![Line::from(vec![
            Span::styled("alpha nee", base),
            Span::styled("dle omega", base.add_modifier(Modifier::BOLD)),
        ])];
        let regex = regex::Regex::new("needle").expect("regex");

        highlight_transcript_search_matches(&mut lines, &regex);

        let rebuilt = &lines[0].spans;
        assert_eq!(
            rebuilt
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "alpha needle omega"
        );
        let highlighted = rebuilt
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(highlighted, "needle");
        assert!(
            rebuilt
                .iter()
                .find(|span| span.content.as_ref() == "dle")
                .expect("second highlighted fragment")
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::REVERSED)
        );
    }

    #[test]
    fn transcript_search_production_render_shows_bar_and_match_highlight() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.projection
            .project_wire_fixtures(
                &[json!({
                    "type":"assistant",
                    "uuid":"search-render-assistant",
                    "session_id":"session",
                    "parent_tool_use_id":null,
                    "message":{
                        "id":"search-render-message",
                        "content":[{"type":"text","text":"alpha needle omega"}]
                    }
                })],
                1,
            )
            .expect("search render fixture");
        let documents = app
            .projection
            .items()
            .iter()
            .filter_map(|item| {
                projected_item_searchable_text(item).map(|text| TranscriptSearchDocument {
                    key: item.key.clone(),
                    text,
                })
            })
            .collect::<Vec<_>>();
        let mut search = TranscriptSearchState::open();
        search.set_query("needle");
        search.refresh_query(app.projection.raw_envelope_count(), || documents);
        for _ in 0..1_000 {
            if search.poll() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(search.match_count(), 1);
        app.transcript_search = Some(search);
        app.reveal_transcript_match();

        let buffer = render_test_buffer(&mut app, 80, 24);
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.replace(' ', "").contains("搜索：needle"), "{text}");
        let reversed = buffer
            .content
            .iter()
            .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
            .count();
        assert_eq!(
            reversed,
            "needle".len(),
            "only the transcript match receives reverse-video highlighting"
        );
    }

    #[test]
    fn transcript_search_fixed_chrome_localizes_invalid_and_empty_results() {
        for (language, invalid_label, empty_label) in [
            (UiLanguage::ZhCn, "模式无效", "无匹配项"),
            (UiLanguage::EnUs, "bad pattern", "no matches"),
        ] {
            let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
            if language == UiLanguage::EnUs {
                switch_to_english_and_initialize(&mut app);
            } else {
                app.release_startup_barrier_for_test();
            }

            let mut search = TranscriptSearchState::open();
            search.set_query("[invalid");
            search.refresh_query(0, Vec::new);
            app.transcript_search = Some(search);
            let invalid = render_buffer(&mut app, 80, 24);
            let invalid_compact = invalid.replace(' ', "");
            let expected_prefix = language.text("搜索：[invalid", "search:[invalid");
            assert!(invalid_compact.contains(expected_prefix), "{invalid}");
            assert!(
                invalid_compact.contains(&invalid_label.replace(' ', "")),
                "{invalid}"
            );

            let search = app.transcript_search.as_mut().expect("search remains open");
            search.set_query("needle");
            search.refresh_query(0, Vec::new);
            for _ in 0..1_000 {
                if search.poll() && !search.has_pending_work() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(!search.has_pending_work(), "search worker did not settle");
            let empty = render_buffer(&mut app, 80, 24);
            let empty_compact = empty.replace(' ', "");
            let expected_prefix = language.text("搜索：needle", "search:needle");
            assert!(empty_compact.contains(expected_prefix), "{empty}");
            assert!(
                empty_compact.contains(&empty_label.replace(' ', "")),
                "{empty}"
            );
        }
    }

    #[test]
    fn fixed_theme_settings_select_only_the_three_historical_syntax_palettes() {
        for kind in [CrabCodeThemeKind::Dark, CrabCodeThemeKind::DarkDaltonized] {
            assert_eq!(
                crabcode_syntax_theme(kind, false),
                Some(CrabCodeSyntaxTheme::MonokaiExtended)
            );
        }
        for kind in [CrabCodeThemeKind::Light, CrabCodeThemeKind::LightDaltonized] {
            assert_eq!(
                crabcode_syntax_theme(kind, false),
                Some(CrabCodeSyntaxTheme::Github)
            );
        }
        for kind in [CrabCodeThemeKind::LightAnsi, CrabCodeThemeKind::DarkAnsi] {
            assert_eq!(
                crabcode_syntax_theme(kind, false),
                Some(CrabCodeSyntaxTheme::Ansi)
            );
        }
        for kind in CrabCodeThemeKind::ALL {
            assert_eq!(crabcode_syntax_theme(*kind, true), None);
        }

        let ansi = crabcode_markdown_syntax_highlighter(CrabCodeSyntaxTheme::Ansi);
        let dark = crabcode_markdown_syntax_highlighter(CrabCodeSyntaxTheme::MonokaiExtended);
        let light = crabcode_markdown_syntax_highlighter(CrabCodeSyntaxTheme::Github);
        assert_eq!(ansi.theme.name.as_deref(), Some("ANSI"));
        assert_eq!(dark.theme.name.as_deref(), Some("Monokai Extended"));
        assert_eq!(light.theme.name.as_deref(), Some("GitHub"));
        assert_ne!(ansi.theme, dark.theme);
        assert_ne!(ansi.theme, light.theme);
        assert_ne!(dark.theme, light.theme);
    }

    #[test]
    fn live_welcome_stays_compact_without_a_pixel_wordmark() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let buffer = render_test_buffer(&mut app, 80, 30);
        let actual_rows = buffer_row_texts(&buffer);
        let compact = actual_rows
            .iter()
            .map(|row| row.replace(' ', ""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            compact.contains("CrabCode原生RustTUI"),
            "the live welcome must retain a compact product identity: {actual_rows:#?}"
        );
        assert!(
            compact.contains("快速开始"),
            "the live welcome must expose its primary action: {actual_rows:#?}"
        );
        assert!(
            !actual_rows.iter().any(|row| row.contains(['▀', '▄'])),
            "the live welcome must not paint the legacy pixel banner: {actual_rows:#?}"
        );
    }

    #[test]
    fn safe_tool_result_render_degrades_to_none_and_reports_the_tool_name() {
        let mut reports = Vec::new();
        assert_eq!(
            safe_render_tool_result_with(
                "Edit",
                || Some("rendered"),
                |message| { reports.push(message) }
            ),
            Some("rendered")
        );
        assert!(reports.is_empty());
        assert_eq!(
            safe_render_tool_result_with::<()>(
                "Edit",
                || None,
                |message| { reports.push(message) }
            ),
            None
        );
        assert!(reports.is_empty());
        assert_eq!(
            safe_render_tool_result_with::<()>(
                "MyTool",
                || panic!("boom from renderer"),
                |message| reports.push(message),
            ),
            None
        );
        assert_eq!(reports.len(), 1);
        assert!(reports[0].contains("Error rendering tool result for MyTool"));
        assert!(reports[0].contains("boom from renderer"));
    }

    #[test]
    fn goal_status_and_inline_console_render_the_four_historical_regions() {
        let mut app = TuiApp::new(
            &json!({
                "commands":[{
                    "name":"goal",
                    "description":"Run an acceptance goal",
                    "argumentHint":"<objective>"
                }]
            }),
            InitialSessionRequest::New,
            None,
        );
        app.release_startup_barrier_for_test();
        app.composer.set_text("/goal 支持 exact renderer");
        app.composer.set_cursor("/goal 支持 exact renderer".len());
        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert_eq!(actions.len(), 1);
        app.action_succeeded(&actions[0]);
        let status = render_buffer(&mut app, 100, 40);
        assert!(status.replace(' ', "").contains("目标:"), "{status}");
        assert!(status.contains("exact renderer"), "{status}");

        for key in ['x', 'g'] {
            let _ = app.handle_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(key),
                    crossterm::event::KeyModifiers::CONTROL,
                ),
            ));
        }
        let console = render_buffer(&mut app, 100, 40);
        let console_compact = console.replace(' ', "");
        for region in ["阶段", "子代理", "验证", "ctrl+x ctrl+g 关闭"] {
            assert!(
                console_compact.contains(&region.replace(' ', "")),
                "missing {region}: {console}"
            );
        }
    }

    #[test]
    fn block_copy_respects_per_entry_raw_markdown_mode() {
        let source = "**bold** and [link](https://example.test)";
        let item = projected_item("assistant", source, 1);
        assert_eq!(
            projected_item_copy_text(&item, true).as_deref(),
            Some(source),
            "raw copy is the exact Markdown source"
        );
        let pretty = projected_item_copy_text(&item, false).expect("pretty copy");
        assert!(pretty.contains("bold"), "{pretty}");
        assert!(pretty.contains("link"), "{pretty}");
        assert!(!pretty.contains("**"), "{pretty}");
        assert!(!pretty.contains("]("), "{pretty}");
    }

    #[test]
    fn raw_toggle_rebuilds_the_selected_wire_markdown_entry() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.projection
            .project_wire_fixtures(
                &[json!({
                    "type":"assistant","uuid":"assistant-raw","session_id":"session",
                    "parent_tool_use_id":null,
                    "message":{"id":"assistant-raw","content":[{
                        "type":"text","text":"**raw-marker-exact**"
                    }]}
                })],
                1,
            )
            .expect("wire fixture");
        let pretty = render_buffer(&mut app, 80, 20);
        assert!(pretty.contains("raw-marker-exact"), "{pretty}");
        assert!(!pretty.contains("**raw-marker-exact**"), "{pretty}");

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('r'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let raw = render_buffer(&mut app, 80, 20);
        assert!(raw.contains("**raw-marker-exact**"), "{raw}");
        assert_eq!(app.projection.raw_envelope_count(), 1);
    }

    fn file_edit_item(old_string: &str, new_string: &str) -> ProjectedItem {
        ProjectedItem {
            key: "edit".to_string(),
            kind: ProjectedKind::ToolUse,
            title: "Edit".to_string(),
            text: "{}".to_string(),
            streaming: false,
            raw_sequences: vec![1],
            tool_use_id: Some("edit-tool".to_string()),
            presentation: crate::sdk_projection::ProjectedPresentation {
                assistant_block: Some(crate::sdk_projection::AssistantBlockType::ToolUse),
                tool: Some(crate::sdk_projection::ToolPresentation {
                    name: Some("Edit".to_string()),
                    input: Some(json!({
                        "file_path": "/tmp/example.rs",
                        "old_string": old_string,
                        "new_string": new_string
                    })),
                    partial_input_json: None,
                    lifecycle_output: None,
                    result: None,
                    is_error: None,
                }),
                ..crate::sdk_projection::ProjectedPresentation::default()
            },
        }
    }

    fn fixed_scrollback_buffer_for_item(item: &ProjectedItem, width: u16, height: u16) -> Buffer {
        let mut adapter = crate::scrollback_projection::ProjectionScrollbackAdapter::default();
        let mut scrollback = crabcode_pager_render::scrollback::ScrollbackState::new();
        adapter
            .synchronize(&mut scrollback, std::slice::from_ref(item))
            .expect("test item enters the fixed ScrollbackEntry/RenderBlock model");
        scrollback.expand_all();
        scrollback.prepare_layout(width.max(1), height);
        scrollback.set_scroll_offset(0);

        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut scratch = crabcode_pager_render::scrollback::ScratchBuffer::new();
        crabcode_pager_render::scrollback::ScrollbackPane::new().render_with_scratch(
            area,
            &mut buffer,
            &scrollback,
            &mut scratch,
        );
        buffer
    }

    fn pinned_markdown_one_shot(
        source: &str,
        width: usize,
        media_paths: &[PathBuf],
    ) -> CrabCodeMarkdownRender {
        let mut renderer = crabcode_markdown_renderer::StreamingMarkdownRenderer::new(
            crabcode_markdown_style(CrabCodeTheme::NIGHT),
            true,
        );
        renderer.set_max_table_width(Some(width.max(1)));
        let expanded = expand_pinned_markdown_tabs(source);
        renderer.push(&expanded);
        let view = renderer.finish(Some(crabcode_markdown_syntax_highlighter(
            CrabCodeSyntaxTheme::MonokaiExtended,
        )));
        crabcode_markdown_render_from_view(view, media_paths)
    }

    fn assert_markdown_render_exact(
        actual: &CrabCodeMarkdownRender,
        expected: &CrabCodeMarkdownRender,
    ) {
        assert_eq!(actual.line_source_map, expected.line_source_map);
        assert_eq!(actual.code_blocks, expected.code_blocks);
        assert_eq!(actual.lines.len(), expected.lines.len());
        for (index, (actual, expected)) in actual.lines.iter().zip(&expected.lines).enumerate() {
            assert_eq!(actual.line, expected.line, "rendered line {index}");
            assert_eq!(
                actual.source_line, expected.source_line,
                "source line {index}"
            );
            assert_eq!(
                actual.links, expected.links,
                "logical links on line {index}"
            );
        }
    }

    fn ingest_assistant_tool(app: &mut TuiApp, sequence: u64, id: &str, name: &str, input: Value) {
        let effect = app.projection.ingest(RawEnvelope {
            sequence,
            encoded_len: 1,
            value: json!({
                "type": "assistant",
                "uuid": format!("assistant-{sequence}"),
                "session_id": "session",
                "parent_tool_use_id": null,
                "message": {
                    "id": format!("message-{sequence}"),
                    "content": [{
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input
                    }]
                }
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        });
        assert_eq!(effect, crate::sdk_projection::ProjectionEffect::None);
    }

    fn ingest_tool_result(app: &mut TuiApp, sequence: u64, tool_use_id: &str, result: Value) {
        let effect = app.projection.ingest(RawEnvelope {
            sequence,
            encoded_len: 1,
            value: json!({
                "type":"user",
                "uuid":format!("user-{sequence}"),
                "timestamp":"2026-08-10T00:00:00.000Z",
                "toolUseResult":result,
                "message":{
                    "role":"user",
                    "content":[{
                        "type":"tool_result",
                        "tool_use_id":tool_use_id,
                        "content":"ok",
                        "is_error":false
                    }]
                }
            }),
            classification: EnvelopeClass::User,
            correlation: None,
        });
        assert_eq!(effect, crate::sdk_projection::ProjectionEffect::None);
    }

    fn ingest_runtime_value(app: &mut TuiApp, sequence: u64, value: Value) {
        let classification =
            crate::sdk_runtime::classify_envelope(&value).expect("known test envelope");
        let encoded_len = serde_json::to_vec(&value)
            .expect("encode test envelope")
            .len();
        app.handle_runtime_event(crate::sdk_runtime::RuntimeEvent::Envelope(RawEnvelope {
            sequence,
            encoded_len,
            value,
            classification,
            correlation: None,
        }));
        assert!(app.fatal.is_none(), "projection failed: {:?}", app.fatal);
    }

    #[test]
    fn scrollback_diff_reflow_preserves_gutter_and_change_semantics() {
        let item = file_edit_item(
            "let unchanged = true;\nlet value = a_very_long_identifier_that_wraps;",
            "let unchanged = true;\nlet value = another_very_long_identifier_that_wraps;",
        );
        let width = 28_u16;
        let height = 20_u16;
        let buffer = fixed_scrollback_buffer_for_item(&item, width, height);
        let theme = crabcode_pager_render::theme::Theme::current();
        let changed = (0..height)
            .filter(|row| {
                (0..width).any(|column| {
                    let background = buffer[(column, *row)].bg;
                    background == theme.diff_insert_bg || background == theme.diff_delete_bg
                })
            })
            .collect::<Vec<_>>();
        assert!(
            changed.len() >= 4,
            "both long changed lines must reflow through the fixed edit block"
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == theme.diff_insert_bg)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == theme.diff_delete_bg)
        );
    }

    #[test]
    fn fixed_scrollback_edit_keeps_diff_line_backgrounds() {
        let item = file_edit_item("let value = 1;", "let value = 2;");
        let buffer = fixed_scrollback_buffer_for_item(&item, 80, 12);
        let theme = crabcode_pager_render::theme::Theme::current();
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == theme.diff_insert_bg)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == theme.diff_delete_bg)
        );
    }

    #[test]
    fn minimal_transcript_expands_streamed_thinking_body() {
        let item = ProjectedItem {
            key: "thinking".to_string(),
            kind: ProjectedKind::Thinking,
            title: "Thinking".to_string(),
            text: "REASONINGBODY pondering\nquietly about wraps".to_string(),
            streaming: true,
            raw_sequences: vec![1],
            tool_use_id: None,
            presentation: crate::sdk_projection::ProjectedPresentation::default(),
        };
        let text = terminal_lines_for_projected_item(&item, 40)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.split_whitespace().collect::<Vec<_>>().join(" "),
            "◇ Thinking … REASONINGBODY pondering quietly about wraps"
        );
    }

    #[test]
    fn minimal_status_shows_rich_activity_and_idle_hint() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        assert!(minimal_status_line(&app).contains("/help"));
        app.active_tasks.insert(
            "task-1".to_string(),
            "Running exact verification".to_string(),
        );
        assert_eq!(
            minimal_status_line(&app),
            "活动任务 task-1 · Running exact verification"
        );
        app.active_tasks.clear();
        ingest_runtime_value(
            &mut app,
            1,
            json!({"type":"stream_request_start","uuid":"request-1"}),
        );
        let status = minimal_status_line(&app);
        assert!(status.contains("正在请求响应"), "{status}");
        assert!(
            status.contains("·"),
            "turn phase and total clocks are visible: {status}"
        );
    }

    #[test]
    fn header_distinguishes_a_missing_model_from_runtime_initialization_in_both_locales() {
        let mut chinese = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        chinese.release_startup_barrier_for_test();
        let chinese_rendered = render_buffer(&mut chinese, 100, 20);
        let chinese_compact = chinese_rendered.replace(' ', "");
        assert!(chinese_compact.contains("模型待回报"), "{chinese_rendered}");
        assert!(
            !chinese_compact.contains("初始化中"),
            "a missing model is not evidence that the runtime is initializing: {chinese_rendered}"
        );

        let mut english = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        switch_to_english_and_initialize(&mut english);
        let english_rendered = render_buffer(&mut english, 100, 20);
        assert!(
            english_rendered.contains("model pending"),
            "{english_rendered}"
        );
        assert!(
            !english_rendered.contains("initializing"),
            "a missing model is not evidence that the runtime is initializing: {english_rendered}"
        );
    }

    #[test]
    fn usage_management_surface_defaults_to_chinese_and_supports_english() {
        let mut chinese = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        chinese.release_startup_barrier_for_test();
        let (state, _) = crate::usage_plugin_management::UsagePluginManagementState::open_usage();
        chinese.usage_plugin_management = Some(state);
        let chinese_rendered = render_buffer(&mut chinese, 108, 30);
        let chinese_compact = chinese_rendered.replace(' ', "");
        assert!(chinese_compact.contains("用量与额度"), "{chinese_rendered}");
        assert!(
            chinese_compact.contains("Esc只关闭面板"),
            "{chinese_rendered}"
        );
        assert!(
            !chinese_rendered.contains("Usage and limits"),
            "the first-render locale must remain Chinese: {chinese_rendered}"
        );

        let mut english = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        switch_to_english_and_initialize(&mut english);
        let (state, _) = crate::usage_plugin_management::UsagePluginManagementState::open_usage();
        english.usage_plugin_management = Some(state);
        let english_rendered = render_buffer(&mut english, 108, 30);
        assert!(
            english_rendered.contains("Usage and limits"),
            "{english_rendered}"
        );
        assert!(
            english_rendered.contains("Esc only closes this panel"),
            "{english_rendered}"
        );
        assert!(
            !english_rendered.contains("用量与额度"),
            "the optional English locale must project through the native panel: {english_rendered}"
        );
    }

    fn loaded_usage_management(
        language: UiLanguage,
    ) -> crate::usage_plugin_management::UsagePluginManagementState {
        let (mut state, effect) =
            crate::usage_plugin_management::UsagePluginManagementState::open_usage();
        let token = match effect {
            crate::usage_plugin_management::UsagePluginManagementEffect::Private {
                token, ..
            } => token,
            crate::usage_plugin_management::UsagePluginManagementEffect::Close => {
                panic!("usage open must issue a private read")
            }
        };
        state.apply_result(
            token,
            json!({
                "kind": "usage_snapshot",
                "utilization": {
                    "five_hour": {
                        "utilization": 25.0,
                        "resets_at": "2030-01-01T00:00:00Z",
                        "overridable": true
                    },
                    "seven_day": {
                        "utilization": 62.5,
                        "resets_at": "2030-01-07T08:30:00Z",
                        "overridable": false
                    },
                    "extra_usage": {
                        "is_enabled": true,
                        "monthly_limit": 1_000_000.0,
                        "used_credits": 250_000.0,
                        "utilization": 25.0
                    },
                    "five_hour_continue_enabled": false
                },
                "entitlement_balance": {
                    "total_token_quota": 987_654_321_000.0,
                    "total_token_used": 123_456_789_000.0,
                    "total_token_remaining": 864_197_532_000.0,
                    "total_call_quota": 10.0,
                    "total_call_used": 2.0,
                    "total_call_remaining": 8.0,
                    "active_entitlements": 12_345
                }
            }),
            language,
        );
        state
    }

    #[test]
    fn usage_management_is_hierarchical_and_responsive_at_supported_terminal_sizes() {
        for (width, height) in [(120, 30), (80, 24), (60, 20)] {
            let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
            app.release_startup_barrier_for_test();
            app.usage_plugin_management = Some(loaded_usage_management(UiLanguage::ZhCn));

            let buffer = render_test_buffer(&mut app, width, height);
            let rendered = buffer_row_texts(&buffer).join("\n");
            let compact = rendered.replace(' ', "");
            assert!(
                compact.contains("额度余额"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(
                compact.contains("有效额度12,345项"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(compact.contains("Token"), "{width}x{height}:\n{rendered}");
            assert!(compact.contains("≈123B"), "{width}x{height}:\n{rendered}");
            assert!(compact.contains("调用"), "{width}x{height}:\n{rendered}");
            assert!(
                compact.contains("当前会话"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(
                compact.contains("七天用量"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(
                compact.contains("25.0%已用"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(
                compact.contains("重置2030-01-01"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(
                compact.contains("重置2030-01-07"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(compact.contains("R刷新"), "{width}x{height}:\n{rendered}");
            assert!(
                compact.contains("Enter执行"),
                "{width}x{height}:\n{rendered}"
            );
            assert!(compact.contains("Esc关闭"), "{width}x{height}:\n{rendered}");
            assert!(
                !rendered.contains("123456789000"),
                "large backend values must never look like an unlabeled dump: {width}x{height}:\n{rendered}"
            );
        }
    }

    #[test]
    fn usage_management_keeps_the_same_information_architecture_in_english() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        switch_to_english_and_initialize(&mut app);
        app.usage_plugin_management = Some(loaded_usage_management(UiLanguage::EnUs));

        let rendered = render_buffer(&mut app, 80, 24);
        assert!(rendered.contains("Entitlement balance"), "{rendered}");
        assert!(rendered.contains("active entitlements"), "{rendered}");
        assert!(rendered.contains("Current session"), "{rendered}");
        assert!(rendered.contains("Seven-day usage"), "{rendered}");
        assert!(rendered.contains("R refresh"), "{rendered}");
        assert!(rendered.contains("Enter run"), "{rendered}");
        assert!(rendered.contains("Esc close"), "{rendered}");
    }

    #[test]
    fn production_footer_uses_shared_status_layout_and_publishes_bounded_hits() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_runtime_value(
            &mut app,
            0,
            json!({
                "type": "prompt_suggestion",
                "suggestion": "继续处理组合字符 e\u{301} 和宽字符界面",
                "uuid": "suggestion",
                "session_id": "session"
            }),
        );

        let rendered = render_buffer(&mut app, 72, 20);
        assert!(rendered.replace(' ', "").contains("滚动跟随"), "{rendered}");
        let scroll = app.status_hit_areas["scroll"];
        assert_eq!(scroll.y, 19);
        assert!(scroll.right() <= 72);
        if let Some(suggestion) = app.status_hit_areas.get("suggestion") {
            assert!(suggestion.x >= scroll.right());
            assert!(suggestion.right() <= 72);
        }
    }

    #[test]
    fn open_completion_dropdown_reports_border_and_capped_item_rows() {
        let commands = (0..12)
            .map(|index| {
                json!({
                    "name": format!("command-{index}"),
                    "description": format!("Description {index}"),
                    "argumentHint": ""
                })
            })
            .collect::<Vec<_>>();
        let mut app = TuiApp::new(
            &json!({"commands": commands}),
            InitialSessionRequest::New,
            None,
        );
        app.release_startup_barrier_for_test();
        app.handle_event(crossterm::event::Event::Paste("/".to_string()));
        assert_eq!(
            completion_overlay_rows(&app),
            MAX_COMPLETION_VISIBLE_ROWS + 2
        );
        let text = render_buffer(&mut app, 100, 32);
        assert!(text.replace(' ', "").contains("命令"), "{text}");
        assert!(
            text.contains("/help"),
            "fixed renderer-local commands precede the runtime catalog: {text}"
        );
    }

    #[test]
    fn narrow_completion_keeps_command_name_before_wide_description() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.handle_event(crossterm::event::Event::Paste("/reload".to_string()));
        assert_eq!(completion_overlay_rows(&app), 3);

        let text = render_buffer(&mut app, 40, 16);
        assert!(
            text.contains("/reload-plugins"),
            "the primary command identity must remain visible before its wide Chinese description: {text}"
        );
        assert!(
            text.contains('…'),
            "the secondary description must truncate inside the remaining columns: {text}"
        );
    }

    #[test]
    fn minimal_viewport_tracks_live_chrome_and_centered_surface_content() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        let committed = HashSet::new();
        let idle = minimal_viewport_height(&mut app, 100, 40, &committed);
        assert!(
            (MINIMAL_LAYOUT_FLOOR_ROWS..20).contains(&idle),
            "idle minimal viewport must be content-sized, not the old fixed 20 rows: {idle}"
        );

        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::Help,
            "Help",
            (0..14)
                .map(|index| format!("help row {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        assert!(minimal_centered_surface_active(&app));
        let overlay = minimal_viewport_height(&mut app, 100, 40, &committed);
        assert!(
            overlay > idle,
            "centered overlay content must grow the live viewport: idle={idle}, overlay={overlay}"
        );
        assert!(overlay <= 40);
        app.overlay.as_mut().expect("help overlay").scroll = 10;
        assert_eq!(
            minimal_viewport_height(&mut app, 100, 40, &committed),
            overlay,
            "scroll offset must not feed back into centered viewport geometry"
        );
    }

    #[test]
    fn historical_model_picker_renders_bounded_catalog_window_and_sanitized_metadata() {
        let models = (0..12)
            .map(|index| {
                json!({
                    "value": format!("model-{index}"),
                    "displayName": if index == 0 {
                        "Model 0\u{1b}]52;c;unsafe\u{7}".to_string()
                    } else {
                        format!("Model {index}")
                    },
                    "description": format!("Description {index}")
                })
            })
            .collect::<Vec<_>>();
        let mut app = TuiApp::new(&json!({"models": models}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.composer.set_text("/model");
        app.composer.set_cursor("/model".len());
        assert!(
            app.handle_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .is_empty()
        );
        assert!(minimal_centered_surface_active(&app));

        let rendered = render_buffer(&mut app, 120, 30);
        let rendered_compact = rendered.replace(' ', "");
        assert!(rendered_compact.contains("选择模型"), "{rendered}");
        assert!(rendered.contains("Model 0"), "{rendered}");
        assert!(rendered.contains("Model 9"), "{rendered}");
        assert!(!rendered.contains("Model 10"), "{rendered}");
        assert!(rendered_compact.contains("另有2项"), "{rendered}");
        assert!(rendered_compact.contains("Enter确认"), "{rendered}");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(rendered.contains('␛'), "{rendered:?}");

        for _ in 0..10 {
            app.handle_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        let scrolled = render_buffer(&mut app, 120, 30);
        assert!(scrolled.contains("Model 10"), "{scrolled}");
    }

    #[test]
    fn model_management_renders_chinese_by_default_and_never_paints_api_key() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.composer.set_text("/model manage");
        app.composer.set_cursor("/model manage".len());
        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let crate::tui_app::HostAction::SendPrivateRuntimeAction {
            request_id,
            purpose,
            ..
        } = &actions[0]
        else {
            panic!("model management must use the private direct-runtime lane");
        };
        app.record_private_runtime_action(request_id.clone(), purpose.clone());
        app.handle_runtime_event(crate::sdk_runtime::RuntimeEvent::Envelope(RawEnvelope {
            sequence: 1,
            encoded_len: 1,
            value: json!({
                "type":"crabcode_tui_runtime_result",
                "protocol_version":1,
                "request_id":request_id,
                "result":{"kind":"model.custom.list","entries":[]}
            }),
            classification: EnvelopeClass::PrivateRuntimeResult {
                request_id: Some(request_id.clone()),
                result_kind: Some("model.custom.list".to_string()),
                validation_error: None,
            },
            correlation: None,
        }));

        let press = |app: &mut TuiApp, code| {
            app.handle_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            ))
        };
        assert!(press(&mut app, crossterm::event::KeyCode::Enter).is_empty());
        assert!(press(&mut app, crossterm::event::KeyCode::Enter).is_empty());
        assert!(press(&mut app, crossterm::event::KeyCode::Enter).is_empty());
        app.handle_event(crossterm::event::Event::Paste(
            "https://models.example.invalid".to_string(),
        ));
        assert!(press(&mut app, crossterm::event::KeyCode::Enter).is_empty());
        app.handle_event(crossterm::event::Event::Paste("model-one".to_string()));
        assert!(press(&mut app, crossterm::event::KeyCode::Enter).is_empty());
        app.handle_event(crossterm::event::Event::Paste(
            "sk-never-render-this".to_string(),
        ));

        let rendered = render_buffer(&mut app, 120, 32);
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("添加自定义模型"), "{rendered}");
        assert!(compact.contains("APIKey"), "{rendered}");
        assert!(rendered.contains('•'), "{rendered}");
        assert!(compact.contains("已隐藏"), "{rendered}");
        assert!(!rendered.contains("sk-never-render-this"), "{rendered}");
    }

    #[test]
    fn model_management_uses_selected_english_ui_without_rewriting_dynamic_values() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        switch_to_english_and_initialize(&mut app);
        app.composer.set_text("/model manage");
        app.composer.set_cursor("/model manage".len());
        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(matches!(
            actions.as_slice(),
            [crate::tui_app::HostAction::SendPrivateRuntimeAction { .. }]
        ));
        let rendered = render_buffer(&mut app, 120, 28);
        assert!(rendered.contains("Model management"), "{rendered}");
        assert!(rendered.contains("direct runtime"), "{rendered}");
        assert!(!rendered.contains("模型管理"), "{rendered}");
    }

    #[test]
    fn minimal_welcome_is_absent_from_live_viewport_after_native_commit() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.set_minimal_mode(true);
        let pending = render_buffer(&mut app, 100, 20);
        let pending_compact = pending.replace(' ', "");
        assert!(pending_compact.contains("CrabCode已就绪。"), "{pending}");

        app.mark_minimal_welcome_committed();
        let committed = render_buffer(&mut app, 100, 20);
        let committed_compact = committed.replace(' ', "");
        assert!(
            !committed_compact.contains("CrabCode已就绪。"),
            "the print-once welcome must not be duplicated in the live viewport: {committed}"
        );
        assert!(
            !committed_compact.contains("请在下方输入提示词。使用/help查看TUI操作说明。"),
            "the print-once hint must not be duplicated in the live viewport: {committed}"
        );
    }

    #[test]
    fn setup_and_setup_fatal_exclude_the_main_shell_until_initialize_completes() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
            request_id: "setup".to_string(),
            title: "Choose a renderer theme".to_string(),
            body: vec!["Preview the exact terminal presentation.".to_string()],
            kind: crate::tui_app::SetupDialogKind::OnboardingTheme {
                syntax_highlighting_disabled: false,
                syntax_toggle_enabled: true,
            },
            choices: vec![
                crate::tui_app::SetupChoice {
                    value: "dark".to_string(),
                    label: "Dark".to_string(),
                },
                crate::tui_app::SetupChoice {
                    value: "light".to_string(),
                    label: "Light".to_string(),
                },
            ],
            selected: 1,
            links: Vec::new(),
            multi_selected: Vec::new(),
        }));
        let setup = render_buffer(&mut app, 100, 30);
        let setup_compact = setup.replace(' ', "");
        assert!(setup.contains("Choose a renderer theme"), "{setup}");
        assert!(setup_compact.contains("语法高亮：已开启"), "{setup}");
        assert!(setup_compact.contains("CrabCode设置"), "{setup}");
        for forbidden in ["原生TUI", "快速开始", "输入·", "SDK帧"] {
            assert!(
                !setup_compact.contains(forbidden),
                "setup must not paint main-shell marker {forbidden:?}: {setup}"
            );
        }

        app.fatal = Some("setup transport failed".to_string());
        let fatal = render_buffer(&mut app, 100, 30);
        let fatal_compact = fatal.replace(' ', "");
        assert!(fatal_compact.contains("设置无法继续"), "{fatal}");
        assert!(fatal_compact.contains("协议兼容性失败"), "{fatal}");
        assert!(fatal_compact.contains("为防止语义漂移"), "{fatal}");
        for forbidden in ["原生TUI", "快速开始", "输入·", "SDK帧"] {
            assert!(
                !fatal_compact.contains(forbidden),
                "setup fatal must not paint main-shell marker {forbidden:?}: {fatal}"
            );
        }

        app.fatal = None;
        app.dialog = None;
        app.release_startup_barrier_for_test();
        let initialized = render_buffer(&mut app, 100, 30);
        let initialized_compact = initialized.replace(' ', "");
        assert!(initialized_compact.contains("原生TUI"), "{initialized}");
        assert!(
            initialized_compact.contains("已就绪，可以开始"),
            "{initialized}"
        );
        assert!(initialized_compact.contains("快速开始"), "{initialized}");
        assert!(
            initialized_compact.contains("输入·Enter发送"),
            "{initialized}"
        );
        assert!(
            initialized_compact.contains("审批标准审批"),
            "{initialized}"
        );
        assert!(
            initialized_compact.contains("基础上下文待同步"),
            "{initialized}"
        );
        assert!(!initialized_compact.contains("SDK帧"), "{initialized}");
    }

    #[test]
    fn header_localizes_approval_and_shows_exact_live_baseline_without_sdk_frames() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let effect = app.projection.ingest(RawEnvelope {
            sequence: 0,
            encoded_len: 1,
            value: json!({
                "type":"system",
                "subtype":"init",
                "apiKeySource":"none",
                "crab_code_version":"1.0.0",
                "cwd":"/tmp",
                "tools":[],
                "mcp_servers":[],
                "model":"deepseek-v4-flash",
                "permissionMode":"bypassPermissions",
                "slash_commands":[],
                "output_style":"default",
                "skills":[],
                "plugins":[],
                "uuid":"init",
                "session_id":"147c669d-session"
            }),
            classification: EnvelopeClass::System(SystemSubtype::Init),
            correlation: None,
        });
        assert!(matches!(
            effect,
            crate::sdk_projection::ProjectionEffect::Initialized { .. }
        ));
        app.set_live_context_usage_for_test(12_400, 128_000, 10);

        let buffer = render_test_buffer(&mut app, 120, 30);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact = rendered.replace(' ', "");
        assert!(
            compact.contains("审批常规操作自动批准（敏感操作仍确认）"),
            "{rendered}"
        );
        assert!(compact.contains("基础上下文12.4k/128k·10%"), "{rendered}");
        assert!(compact.contains("模型deepseek-v4-flash"), "{rendered}");
        assert!(!rendered.contains("bypassPermissions"), "{rendered}");
        assert!(!rendered.contains("SDK"), "{rendered}");
        assert!(
            buffer.content.iter().any(|cell| {
                cell.symbol() == "常" && cell.bg == app.renderer_theme().accent_error
            }),
            "dangerous approval mode must use the error-color chip"
        );

        for width in [80, 60, 40, 20] {
            let narrow = render_buffer(&mut app, width, 8);
            assert!(!narrow.contains("bypassPermissions"), "{narrow}");
            assert!(!narrow.contains("SDK"), "{narrow}");
        }
    }

    #[test]
    fn context_header_tracks_new_turn_resume_and_clear_at_common_widths() {
        fn init_envelope(sequence: u64, session_id: &str) -> RuntimeEvent {
            RuntimeEvent::Envelope(RawEnvelope {
                sequence,
                encoded_len: 1,
                value: json!({
                    "type":"system",
                    "subtype":"init",
                    "apiKeySource":"none",
                    "crab_code_version":"1.0.0",
                    "cwd":"/tmp",
                    "tools":[],
                    "mcp_servers":[],
                    "model":"deepseek-v4-flash",
                    "permissionMode":"default",
                    "slash_commands":[],
                    "output_style":"default",
                    "skills":[],
                    "plugins":[],
                    "uuid":format!("init-{sequence}"),
                    "session_id":session_id
                }),
                classification: EnvelopeClass::System(SystemSubtype::Init),
                correlation: None,
            })
        }

        fn hidden_assistant_envelope(sequence: u64, marker: &str) -> RuntimeEvent {
            let mut value = json!({
                "type":"assistant",
                "uuid":format!("hidden-assistant-{sequence}"),
                "timestamp":"2026-08-10T00:00:00.000Z",
                "message":{
                    "id":format!("hidden-message-{sequence}"),
                    "model":"deepseek-v4-flash",
                    "content":[{"type":"text","text":"hidden initialization context"}]
                }
            });
            value[marker] = Value::Bool(true);
            RuntimeEvent::Envelope(RawEnvelope {
                sequence,
                encoded_len: 1,
                value,
                classification: EnvelopeClass::Assistant,
                correlation: None,
            })
        }

        fn assert_context_label(
            app: &mut TuiApp,
            expected: &str,
            baseline_marker: &str,
            expect_baseline: bool,
        ) {
            for width in [120, 80, 60] {
                let rendered = render_buffer(app, width, 30);
                let compact = rendered.replace(' ', "");
                assert!(compact.contains(expected), "width={width}: {rendered}");
                assert_eq!(
                    compact.contains(baseline_marker),
                    expect_baseline,
                    "width={width}: {rendered}"
                );
            }
        }

        let mut fresh = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        fresh.release_startup_barrier_for_test();
        fresh.handle_runtime_event(init_envelope(1, "session-before-clear"));
        fresh.set_live_context_usage_for_test(30_200, 1_000_000, 3);
        fresh.handle_runtime_event(hidden_assistant_envelope(2, "isSynthetic"));
        fresh.handle_runtime_event(hidden_assistant_envelope(3, "isReplay"));
        assert_context_label(&mut fresh, "基础上下文30.2k/1m·3%", "基础上下文", true);

        let first_send = HostAction::SendUser {
            content: Value::String("first real turn".to_string()),
            priority: None,
        };
        fresh.action_admitted(&first_send);
        assert_context_label(&mut fresh, "上下文30.2k/1m·3%", "基础上下文", false);
        fresh.action_failed(&first_send, "writer unavailable");
        assert_context_label(&mut fresh, "基础上下文30.2k/1m·3%", "基础上下文", true);

        fresh.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
            sequence: 4,
            encoded_len: 1,
            value: json!({
                "type":"user",
                "message":{"role":"user","content":"first real turn"},
                "uuid":"first-user",
                "timestamp":"2026-08-10T00:00:00.000Z"
            }),
            classification: EnvelopeClass::User,
            correlation: None,
        }));
        assert_context_label(&mut fresh, "上下文30.2k/1m·3%", "基础上下文", false);

        // The backend's validated session-id transition is the existing
        // `/clear` boundary. It must discard the old snapshot and return the
        // newly blank session to baseline semantics.
        fresh.handle_runtime_event(init_envelope(5, "session-after-clear"));
        assert!(fresh.live_context_usage().is_none());
        assert!(!fresh.context_usage_refresh_pending());
        fresh.set_live_context_usage_for_test(30_200, 1_000_000, 3);
        assert_context_label(&mut fresh, "基础上下文30.2k/1m·3%", "基础上下文", true);

        let delivered_send = HostAction::SendUser {
            content: Value::String("post-clear turn".to_string()),
            priority: None,
        };
        fresh.action_admitted(&delivered_send);
        assert_context_label(&mut fresh, "上下文30.2k/1m·3%", "基础上下文", false);
        fresh.action_succeeded(&delivered_send);
        assert!(!fresh.context_usage_is_baseline());

        let mut resumed = TuiApp::new(
            &json!({}),
            InitialSessionRequest::ResumeExact {
                session_id: "resumed-session".to_string(),
            },
            None,
        );
        resumed.release_startup_barrier_for_test();
        resumed.set_live_context_usage_for_test(30_200, 1_000_000, 3);
        assert_context_label(&mut resumed, "上下文30.2k/1m·3%", "基础上下文", false);

        let mut english = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        switch_to_english_and_initialize(&mut english);
        english.set_live_context_usage_for_test(30_200, 1_000_000, 3);
        assert_context_label(
            &mut english,
            "baselinecontext30.2k/1m·3%",
            "baselinecontext",
            true,
        );
    }

    #[test]
    fn approval_labels_cover_every_direct_runtime_mode() {
        let cases = [
            ("default", "标准审批"),
            ("acceptEdits", "自动接受编辑"),
            ("bypassPermissions", "常规操作自动批准（敏感操作仍确认）"),
            ("plan", "只规划"),
            ("dontAsk", "仅执行已批准"),
            ("auto", "自动模式"),
        ];
        for (mode, expected) in cases {
            assert_eq!(
                permission_mode_label(mode, UiLanguage::ZhCn, false),
                expected
            );
        }
        assert_eq!(
            permission_mode_label("future", UiLanguage::ZhCn, false),
            "自定义"
        );
        assert_eq!(format_header_tokens(999), "999");
        assert_eq!(format_header_tokens(1_000), "1k");
        assert_eq!(format_header_tokens(12_400), "12.4k");
        assert_eq!(format_header_tokens(1_200_000), "1.2m");
    }

    #[test]
    fn setup_chrome_defaults_to_chinese_and_language_confirmation_switches_to_english() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
            request_id: "language".to_string(),
            title: "Language / 语言".to_string(),
            body: vec!["Choose".to_string()],
            kind: crate::tui_app::SetupDialogKind::OnboardingLanguage,
            choices: vec![
                crate::tui_app::SetupChoice {
                    value: "zh-CN".to_string(),
                    label: "中文".to_string(),
                },
                crate::tui_app::SetupChoice {
                    value: "en-US".to_string(),
                    label: "English".to_string(),
                },
            ],
            selected: 1,
            links: Vec::new(),
            multi_selected: Vec::new(),
        }));
        let chinese = render_buffer(&mut app, 100, 24);
        let chinese_compact = chinese.replace(' ', "");
        assert!(chinese_compact.contains("CrabCode设置"), "{chinese}");
        assert!(!chinese.contains("CrabCode setup ·"), "{chinese}");

        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert_eq!(actions.len(), 1);
        assert_eq!(app.ui_language(), crate::tui_app::UiLanguage::EnUs);
        let english = render_buffer(&mut app, 100, 24);
        assert!(
            english.contains("CrabCode setup · delivering response"),
            "{english}"
        );
        assert!(!english.contains("CrabCode 设置 ·"), "{english}");

        app.action_admitted(&actions[0]);
        app.action_succeeded(&actions[0]);
        app.release_startup_barrier_for_test();
        app.dialog = Some(RequestDialog::Permission {
            request_id: "english-permission".to_string(),
            tool_name: "Write".to_string(),
            display_name: None,
            description: Some("Dynamic tool description".to_string()),
            input: json!({"file_path":"a"}),
            suggestions: None,
            choices: vec![
                crate::tui_app::PermissionChoice::AllowOnce,
                crate::tui_app::PermissionChoice::Deny,
            ],
            selected: 0,
        });
        let english_permission = render_buffer(&mut app, 100, 24);
        assert!(
            english_permission.contains("Permission ·"),
            "{english_permission}"
        );
        assert!(
            english_permission.contains("Allow once"),
            "{english_permission}"
        );
        assert!(english_permission.contains("Deny"), "{english_permission}");
        assert!(
            english_permission.contains("Dynamic tool description"),
            "{english_permission}"
        );
        assert!(
            !english_permission.contains("仅允许一次"),
            "{english_permission}"
        );
    }

    #[test]
    fn setup_choices_keep_bottom_owned_rows_across_narrow_viewports() {
        for width in [40, 60, 80, 100] {
            for height in [12, 16, 24, 30] {
                let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
                app.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
                    request_id: format!("layout-{width}-{height}"),
                    title: "First-run setup".to_string(),
                    body: vec![format!(
                        "BODYOVERLAP https://example.invalid/{}",
                        "very-long-segment-".repeat(20)
                    )],
                    kind: crate::tui_app::SetupDialogKind::OnboardingLanguage,
                    choices: vec![
                        crate::tui_app::SetupChoice {
                            value: "a".to_string(),
                            label: "CHOICE-A".to_string(),
                        },
                        crate::tui_app::SetupChoice {
                            value: "b".to_string(),
                            label: "CHOICE-B".to_string(),
                        },
                    ],
                    selected: 0,
                    links: Vec::new(),
                    multi_selected: Vec::new(),
                }));

                let buffer = render_test_buffer(&mut app, width, height);
                let rows = buffer_row_texts(&buffer);
                let choice_a = rows
                    .iter()
                    .position(|row| row.contains("CHOICE-A"))
                    .unwrap_or_else(|| panic!("CHOICE-A missing at {width}x{height}: {rows:?}"));
                let choice_b = rows
                    .iter()
                    .position(|row| row.contains("CHOICE-B"))
                    .unwrap_or_else(|| panic!("CHOICE-B missing at {width}x{height}: {rows:?}"));
                let modal = centered(Rect::new(0, 0, width, height), 90, 82);
                assert_eq!(
                    choice_b,
                    usize::from(modal.bottom().saturating_sub(2)),
                    "the final setup choice must own the final inner row at {width}x{height}: {rows:?}"
                );
                assert_eq!(
                    choice_a + 1,
                    choice_b,
                    "setup choices must remain contiguous at {width}x{height}: {rows:?}"
                );
                for row in [&rows[choice_a], &rows[choice_b]] {
                    assert!(
                        !row.contains("BODYOVERLAP") && !row.contains("example.invalid"),
                        "wrapped setup body leaked into a choice row at {width}x{height}: {row:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn first_setup_ctrl_c_confirmation_is_visible_on_every_setup_surface() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        let actions = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ));
        assert!(actions.is_empty());
        assert!(app.setup_ctrl_c_confirmation_active());

        let lifecycle = render_buffer(&mut app, 100, 24);
        assert!(
            lifecycle.replace(' ', "").contains("再次按Ctrl-C即可退出"),
            "{lifecycle}"
        );

        app.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
            request_id: "confirm-setup".to_string(),
            title: "Language / 语言".to_string(),
            body: vec!["Choose".to_string()],
            kind: crate::tui_app::SetupDialogKind::OnboardingLanguage,
            choices: vec![crate::tui_app::SetupChoice {
                value: "zh-CN".to_string(),
                label: "中文".to_string(),
            }],
            selected: 0,
            links: Vec::new(),
            multi_selected: Vec::new(),
        }));
        let setup = render_buffer(&mut app, 100, 24);
        assert!(
            setup.replace(' ', "").contains("再次按Ctrl-C即可退出"),
            "{setup}"
        );

        app.dialog = Some(RequestDialog::SetupInput(
            crate::tui_app::SetupInputDialog {
                request_id: "confirm-input".to_string(),
                title: "Custom model".to_string(),
                body: vec!["Enter model ID".to_string()],
                kind: crate::tui_app::SetupInputKind::CustomModelId,
                input: Box::new(crabcode_ratatui_textarea::TextArea::new()),
                input_state: crabcode_ratatui_textarea::TextAreaState::default(),
                validation_error: None,
                masked: false,
            },
        ));
        let setup_input = render_buffer(&mut app, 100, 24);
        assert!(
            setup_input
                .replace(' ', "")
                .contains("再次按Ctrl-C即可退出"),
            "{setup_input}"
        );
    }

    #[test]
    fn environment_locked_theme_never_advertises_an_inactive_escape_action() {
        let locked_theme_dialog = || {
            RequestDialog::Setup(crate::tui_app::SetupDialog {
                request_id: "locked-theme".to_string(),
                title: "Theme".to_string(),
                body: vec!["Preview".to_string()],
                kind: crate::tui_app::SetupDialogKind::OnboardingTheme {
                    syntax_highlighting_disabled: false,
                    syntax_toggle_enabled: false,
                },
                choices: vec![
                    crate::tui_app::SetupChoice {
                        value: "dark".to_string(),
                        label: "Dark".to_string(),
                    },
                    crate::tui_app::SetupChoice {
                        value: "light".to_string(),
                        label: "Light".to_string(),
                    },
                ],
                selected: 0,
                links: Vec::new(),
                multi_selected: Vec::new(),
            })
        };

        let mut chinese = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        chinese.dialog = Some(locked_theme_dialog());
        let chinese_render = render_buffer(&mut chinese, 100, 24);
        let chinese_compact = chinese_render.replace(' ', "");
        assert!(
            chinese_compact.contains("↑/↓选择·Enter确认"),
            "{chinese_render}"
        );
        assert!(!chinese_render.contains("Esc"), "{chinese_render}");
        assert!(!chinese_render.contains("Ctrl+T"), "{chinese_render}");

        let mut english = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        english.dialog = Some(RequestDialog::Setup(crate::tui_app::SetupDialog {
            request_id: "language".to_string(),
            title: "Language / 语言".to_string(),
            body: vec!["Choose".to_string()],
            kind: crate::tui_app::SetupDialogKind::OnboardingLanguage,
            choices: vec![
                crate::tui_app::SetupChoice {
                    value: "zh-CN".to_string(),
                    label: "中文".to_string(),
                },
                crate::tui_app::SetupChoice {
                    value: "en-US".to_string(),
                    label: "English".to_string(),
                },
            ],
            selected: 1,
            links: Vec::new(),
            multi_selected: Vec::new(),
        }));
        let language_actions = english.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert_eq!(language_actions.len(), 1);
        english.action_admitted(&language_actions[0]);
        english.action_succeeded(&language_actions[0]);
        english.dialog = Some(locked_theme_dialog());
        let english_render = render_buffer(&mut english, 100, 24);
        assert!(
            english_render.contains("↑/↓ choose · Enter confirm"),
            "{english_render}"
        );
        assert!(
            !english_render.contains("Esc applies") && !english_render.contains("Ctrl+T syntax"),
            "{english_render}"
        );
    }

    #[test]
    fn custom_api_key_modal_masks_every_grapheme_and_never_paints_plaintext() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        let idle = minimal_viewport_height(&mut app, 80, 40, &HashSet::new());
        let secret = "sk-private-密钥";
        let mut input = crabcode_ratatui_textarea::TextArea::new();
        input.set_text(secret);
        input.set_cursor(secret.len());
        app.dialog = Some(RequestDialog::SetupInput(
            crate::tui_app::SetupInputDialog {
                request_id: "setup-key".to_string(),
                title: "Enter API key".to_string(),
                body: vec!["The value is stored by the direct backend.".to_string()],
                kind: crate::tui_app::SetupInputKind::CustomApiKey,
                input: Box::new(input),
                input_state: crabcode_ratatui_textarea::TextAreaState::default(),
                validation_error: None,
                masked: true,
            },
        ));
        let setup = minimal_viewport_height(&mut app, 80, 40, &HashSet::new());
        assert!(setup > idle, "input modal must grow the compact viewport");
        let text = render_buffer(&mut app, 80, setup.max(12));
        assert!(text.contains("Enter API key"), "{text}");
        assert!(
            text.contains(&"*".repeat(secret.graphemes(true).count())),
            "{text}"
        );
        assert!(
            !text.contains(secret),
            "plaintext API key reached the Ratatui buffer: {text}"
        );
    }

    #[test]
    fn empty_plan_permission_uses_explicit_notice_not_silence() {
        let body = plan_approval_body("ExitPlanMode", &json!({})).expect("plan body");
        assert!(body.contains("No plan written yet"));
        assert!(body.contains("Approve"));

        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.dialog = Some(RequestDialog::Permission {
            request_id: "plan".to_string(),
            tool_name: "ExitPlanMode".to_string(),
            display_name: None,
            description: None,
            input: json!({}),
            suggestions: None,
            choices: vec![crate::tui_app::PermissionChoice::AllowOnce],
            selected: 0,
        });
        let text = render_buffer(&mut app, 100, 30);
        let text_compact = text.replace(' ', "");
        assert!(text_compact.contains("尚未编写计划"), "{text}");
        assert!(text_compact.contains("仅允许一次"), "{text}");
    }

    fn open_question_dialog_for_ui(app: &mut TuiApp, multi_select: bool) {
        let preview = (1..=40)
            .map(|line| format!("preview line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut first_option = json!({
            "label":"Native TUI",
            "description":"Render directly in Ratatui",
            "recommended":true
        });
        if !multi_select {
            first_option["preview"] =
                Value::String(format!("**Markdown preview**\n\n```text\n{preview}\n```"));
        }
        app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
            sequence: 1,
            encoded_len: 1,
            value: json!({
                "type":"control_request",
                "request_id":"question-ui",
                "request":{
                    "subtype":"can_use_tool",
                    "tool_name":"AskUserQuestion",
                    "tool_use_id":"question-tool-ui",
                    "agent_id":"worker-ui",
                    "input":{
                        "questions":[{
                            "question":"Which rendering approach should be used?",
                            "header":"Rendering",
                            "multiSelect":multi_select,
                            "options":[
                                first_option,
                                {
                                    "label":"Plain text",
                                    "description":"Use a simple fallback"
                                }
                            ]
                        }]
                    }
                }
            }),
            classification: EnvelopeClass::ControlRequest {
                request_id: "question-ui".to_string(),
                subtype: "can_use_tool".to_string(),
            },
            correlation: None,
        }));
        assert!(matches!(app.dialog, Some(RequestDialog::Question(_))));
    }

    #[test]
    fn question_dialog_renders_recommendation_without_preselection_and_scrollable_preview() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        open_question_dialog_for_ui(&mut app, false);

        let text = render_buffer(&mut app, 110, 32);
        let compact = text.replace(' ', "");
        assert!(text.contains("Rendering"), "{text}");
        assert!(text.contains("worker-ui"), "{text}");
        assert!(compact.contains("推荐"), "{text}");
        assert!(compact.contains("其他（自行输入）"), "{text}");
        assert!(compact.contains("预览"), "{text}");
        assert!(compact.contains("Markdownpreview"), "{text}");
        assert!(
            !text.contains("**Markdown preview**"),
            "the advertised markdown preview format must be consumed by the renderer: {text}"
        );
        assert!(compact.contains("提交"), "{text}");
        assert!(compact.contains("跳过回答"), "{text}");
        assert!(
            text.contains("( ) Native TUI"),
            "recommended metadata must not preselect an option: {text}"
        );
        assert!(app.dialog_pointer.preview_area().is_some());
        assert_eq!(app.dialog_pointer.choice_areas().len(), 5);

        let before = match app.dialog.as_ref() {
            Some(RequestDialog::Question(dialog)) => {
                assert!(dialog.preview_max_scroll > 0);
                dialog.preview_scroll
            }
            _ => unreachable!(),
        };
        let outcome = app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageDown,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(outcome.is_empty());
        assert!(matches!(
            app.dialog.as_ref(),
            Some(RequestDialog::Question(dialog)) if dialog.preview_scroll > before
        ));
    }

    #[test]
    fn question_dialog_pointer_double_click_uses_keyboard_toggle_authority() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        open_question_dialog_for_ui(&mut app, true);
        let _ = render_buffer(&mut app, 110, 32);
        let option_area = app.dialog_pointer.choice_areas()[0];
        let mut pointer = |at| {
            app.handle_event_at(
                crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: option_area.x,
                    row: option_area.y,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                }),
                at,
            )
        };
        let start = std::time::Instant::now();
        assert!(pointer(start).actions.is_empty());
        assert!(
            pointer(start + std::time::Duration::from_millis(100))
                .actions
                .is_empty()
        );
        assert!(matches!(
            app.dialog.as_ref(),
            Some(RequestDialog::Question(dialog))
                if dialog.current_answer().selected == vec![true, false]
        ));
    }

    #[test]
    fn request_dialog_pointer_hover_and_double_click_reuse_existing_response_authority() {
        let suggestions = json!([{
            "type": "addRules",
            "rules": [{"toolName": "Write"}],
            "behavior": "allow",
            "destination": "session"
        }]);
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.dialog = Some(RequestDialog::Permission {
            request_id: "permission-pointer".to_string(),
            tool_name: "Write".to_string(),
            display_name: None,
            description: None,
            input: json!({"file_path": "a"}),
            suggestions: Some(suggestions.clone()),
            choices: vec![
                crate::tui_app::PermissionChoice::AllowOnce,
                crate::tui_app::PermissionChoice::AllowSession,
                crate::tui_app::PermissionChoice::Deny,
            ],
            selected: 0,
        });

        let _ = render_buffer(&mut app, 100, 30);
        let always_area = app.dialog_pointer.choice_areas()[1];
        let pointer = |kind| {
            crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: always_area.x,
                row: always_area.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        let start = std::time::Instant::now();

        let hover = app.handle_event_at(pointer(crossterm::event::MouseEventKind::Moved), start);
        assert!(hover.needs_frame);
        assert!(hover.actions.is_empty());
        assert!(matches!(
            app.dialog,
            Some(RequestDialog::Permission { selected: 1, .. })
        ));

        let first = app.handle_event_at(
            pointer(crossterm::event::MouseEventKind::Down(
                crossterm::event::MouseButton::Left,
            )),
            start + std::time::Duration::from_millis(1),
        );
        assert!(first.needs_frame);
        assert!(first.actions.is_empty());

        let second = app.handle_event_at(
            pointer(crossterm::event::MouseEventKind::Down(
                crossterm::event::MouseButton::Left,
            )),
            start + std::time::Duration::from_millis(200),
        );
        assert!(second.needs_frame);
        let [
            crate::tui_app::HostAction::RespondPermission {
                request_id,
                response,
            },
        ] = second.actions.as_slice()
        else {
            panic!("double-click must enter the existing permission response path");
        };
        assert_eq!(request_id, "permission-pointer");
        assert_eq!(response["behavior"], "allow");
        assert_eq!(response["updatedInput"]["file_path"], "a");
        assert_eq!(response["decisionClassification"], "user_temporary");
        assert_eq!(response["updatedPermissions"], suggestions);
        assert!(app.dialog_response_inflight());
    }

    #[test]
    fn todo_panel_visibility_auto_hides_when_work_is_done() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        ingest_assistant_tool(
            &mut app,
            1,
            "todo-1",
            "TodoWrite",
            json!({"todos": [
                {"content": "done", "status": "completed", "activeForm": "done"},
                {"content": "verify", "status": "in_progress", "activeForm": "verifying"}
            ]}),
        );
        assert!(todo_panel_visible(&app));
        ingest_assistant_tool(
            &mut app,
            2,
            "todo-2",
            "TodoWrite",
            json!({"todos": [
                {"content": "done", "status": "completed", "activeForm": "done"},
                {"content": "verified", "status": "completed", "activeForm": "verified"}
            ]}),
        );
        assert!(!todo_panel_visible(&app));
    }

    #[test]
    fn task_tools_render_a_bordered_card_grid_above_the_composer() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let subjects = ["文件系统", "联网搜索", "记忆系统", "子代理并行", "最终验收"];
        let mut sequence = 1_u64;
        for (index, subject) in subjects.iter().enumerate() {
            let id = (index + 1).to_string();
            let tool_use_id = format!("create-{id}");
            ingest_assistant_tool(
                &mut app,
                sequence,
                &tool_use_id,
                "TaskCreate",
                json!({
                    "subject":subject,
                    "description":format!("完成{subject}能力验证")
                }),
            );
            sequence += 1;
            ingest_tool_result(
                &mut app,
                sequence,
                &tool_use_id,
                json!({"task":{"id":id,"subject":subject}}),
            );
            sequence += 1;
        }
        for id in 1..=4 {
            let tool_use_id = format!("update-{id}");
            ingest_assistant_tool(
                &mut app,
                sequence,
                &tool_use_id,
                "TaskUpdate",
                json!({"taskId":id.to_string(),"status":"in_progress"}),
            );
            sequence += 1;
            ingest_tool_result(
                &mut app,
                sequence,
                &tool_use_id,
                json!({
                    "success":true,
                    "taskId":id.to_string(),
                    "updatedFields":["status"],
                    "statusChange":{"from":"pending","to":"in_progress"}
                }),
            );
            sequence += 1;
        }

        assert!(todo_panel_visible(&app));
        let buffer = render_test_buffer(&mut app, 120, 30);
        let rows = buffer_row_texts(&buffer);
        let compact = rows
            .iter()
            .map(|row| row.replace(' ', ""))
            .collect::<Vec<_>>();
        let panel_row = compact
            .iter()
            .position(|row| row.contains("任务·0/5已完成·4进行中"))
            .expect("task panel title");
        let composer_row = compact
            .iter()
            .position(|row| row.contains("输入·"))
            .expect("composer title");
        assert!(panel_row < composer_row, "{rows:#?}");
        for subject in subjects {
            assert!(
                compact.iter().any(|row| row.contains(subject)),
                "missing task {subject}: {rows:#?}"
            );
        }
        assert!(
            rows.iter().any(|row| row.contains('╭')) && rows.iter().any(|row| row.contains('╰')),
            "task panel must own a rounded card boundary: {rows:#?}"
        );

        let narrow = render_buffer(&mut app, 60, 30);
        assert!(
            narrow.replace(' ', "").contains("任务·0/5已完成"),
            "{narrow}"
        );

        for id in 1..=5 {
            let tool_use_id = format!("complete-{id}");
            ingest_assistant_tool(
                &mut app,
                sequence,
                &tool_use_id,
                "TaskUpdate",
                json!({"taskId":id.to_string(),"status":"completed"}),
            );
            sequence += 1;
            ingest_tool_result(
                &mut app,
                sequence,
                &tool_use_id,
                json!({
                    "success":true,
                    "taskId":id.to_string(),
                    "updatedFields":["status"],
                    "statusChange":{
                        "from":if id == 5 {"pending"} else {"in_progress"},
                        "to":"completed"
                    }
                }),
            );
            sequence += 1;
        }
        assert!(!todo_panel_visible(&app));
    }

    #[test]
    fn responsive_core_surfaces_fit_standard_and_narrow_terminals() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let initialized = app.projection.ingest(RawEnvelope {
            sequence: 0,
            encoded_len: 1,
            value: json!({
                "type":"system",
                "subtype":"init",
                "apiKeySource":"none",
                "crab_code_version":"1.0.0",
                "cwd":"/tmp",
                "tools":[],
                "mcp_servers":[],
                "model":"deepseek-v4-flash",
                "permissionMode":"bypassPermissions",
                "slash_commands":[],
                "output_style":"default",
                "skills":[],
                "plugins":[],
                "uuid":"responsive-init",
                "session_id":"session"
            }),
            classification: EnvelopeClass::System(SystemSubtype::Init),
            correlation: None,
        });
        assert!(matches!(
            initialized,
            crate::sdk_projection::ProjectionEffect::Initialized { .. }
        ));
        app.set_live_context_usage_for_test(12_400, 128_000, 10);

        ingest_assistant_tool(
            &mut app,
            1,
            "responsive-task",
            "TaskCreate",
            json!({"subject":"联网验收","description":"验证搜索与抓取渲染"}),
        );
        ingest_tool_result(
            &mut app,
            2,
            "responsive-task",
            json!({"task":{"id":"1","subject":"联网验收"}}),
        );

        ingest_assistant_tool(
            &mut app,
            3,
            "responsive-search",
            "WebSearch",
            json!({"query":"OpenAI Codex CLI Rust TUI"}),
        );
        ingest_tool_result(
            &mut app,
            4,
            "responsive-search",
            json!({
                "query":"OpenAI Codex CLI Rust TUI",
                "results":[{
                    "tool_use_id":"server-search",
                    "content":[{
                        "title":"Codex CLI",
                        "url":"https://github.com/openai/codex",
                        "snippet":"Rust terminal coding agent"
                    }]
                }]
            }),
        );

        ingest_assistant_tool(
            &mut app,
            5,
            "responsive-fetch",
            "WebFetch",
            json!({"url":"https://example.com"}),
        );
        ingest_tool_result(
            &mut app,
            6,
            "responsive-fetch",
            json!({
                "bytes":559,
                "code":200,
                "codeText":"OK",
                "result":"Example Domain — 安全提取摘要",
                "url":"https://example.com"
            }),
        );

        let mut view = crate::app_view::AppView::new();
        for (width, height) in [(120, 30), (80, 24), (60, 20)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("responsive terminal");
            terminal
                .draw(|frame| {
                    view.draw(frame, &mut app)
                        .expect("production AppView must remain renderable");
                })
                .expect("responsive draw");
            let buffer = terminal.backend().buffer().clone();
            let rows = buffer_row_texts(&buffer);
            let compact = rows
                .iter()
                .map(|row| row.replace(' ', ""))
                .collect::<Vec<_>>();
            let diagnostic = format!("{width}x{height}: {rows:#?}");

            assert_eq!(rows.len(), usize::from(height), "{diagnostic}");
            for row in 0..height {
                for column in 0..width {
                    let symbol_width =
                        unicode_width::UnicodeWidthStr::width(buffer[(column, row)].symbol());
                    assert!(
                        symbol_width <= usize::from(width - column),
                        "a wide glyph starts outside the remaining terminal cells at ({column},{row}): {diagnostic}"
                    );
                }
            }
            assert_eq!(
                buffer[(width - 1, 0)].bg,
                app.renderer_theme().bg_dark,
                "the header ribbon must paint through the right edge: {diagnostic}"
            );
            assert_eq!(
                buffer[(width - 1, 1)].bg,
                app.renderer_theme().bg_dark,
                "the metadata ribbon must paint through the right edge: {diagnostic}"
            );
            assert!(compact[0].contains("CrabCode"), "{diagnostic}");
            assert!(compact[0].contains("审批"), "{diagnostic}");
            assert!(compact[1].contains("上下文"), "{diagnostic}");
            assert!(
                !compact.join("").contains("bypassPermissions"),
                "{diagnostic}"
            );
            assert!(!compact.join("").contains("SDK帧"), "{diagnostic}");

            let task_row = compact
                .iter()
                .position(|row| row.contains("任务·0/1已完成"))
                .unwrap_or_else(|| panic!("missing task card title: {diagnostic}"));
            let composer_row = compact
                .iter()
                .position(|row| row.contains("输入·"))
                .unwrap_or_else(|| panic!("missing composer: {diagnostic}"));
            assert!(task_row < composer_row, "{diagnostic}");
            assert!(
                compact.iter().any(|row| row.contains("联网验收")),
                "{diagnostic}"
            );

            if width == 120 {
                assert!(
                    compact.iter().any(|row| row.contains("联网搜索")),
                    "{diagnostic}"
                );
            }
            if width >= 80 {
                assert!(
                    compact.iter().any(|row| row.contains("CodexCLI")),
                    "{diagnostic}"
                );
            }
            assert!(
                compact.iter().any(|row| row.contains("抓取网页")),
                "{diagnostic}"
            );
            assert!(
                compact.iter().any(|row| row.contains("ExampleDomain")),
                "{diagnostic}"
            );

            for hit_area in app.status_hit_areas.values() {
                assert!(hit_area.right() <= width, "{diagnostic}");
                assert!(hit_area.bottom() <= height, "{diagnostic}");
            }
        }

        // At 60x20 the latest WebFetch, task card, and composer deliberately anchor the
        // bottom of the viewport, so the preceding WebSearch cannot also fit. Render it
        // as the latest tool to prove its default result preview remains visible at the
        // same narrow size instead of weakening the narrow-screen acceptance criterion.
        let mut narrow_search_app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        narrow_search_app.release_startup_barrier_for_test();
        ingest_assistant_tool(
            &mut narrow_search_app,
            1,
            "narrow-search",
            "WebSearch",
            json!({"query":"OpenAI Codex CLI Rust TUI"}),
        );
        ingest_tool_result(
            &mut narrow_search_app,
            2,
            "narrow-search",
            json!({
                "query":"OpenAI Codex CLI Rust TUI",
                "results":[{
                    "tool_use_id":"server-search",
                    "content":[{
                        "title":"Codex CLI",
                        "url":"https://github.com/openai/codex",
                        "snippet":"Rust terminal coding agent"
                    }]
                }]
            }),
        );
        let mut narrow_search_view = crate::app_view::AppView::new();
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("narrow search terminal");
        terminal
            .draw(|frame| {
                narrow_search_view
                    .draw(frame, &mut narrow_search_app)
                    .expect("narrow WebSearch AppView must remain renderable");
            })
            .expect("narrow search draw");
        let rows = buffer_row_texts(terminal.backend().buffer());
        let compact = rows
            .iter()
            .map(|row| row.replace(' ', ""))
            .collect::<Vec<_>>();
        let diagnostic = format!("60x20 latest WebSearch: {rows:#?}");
        assert!(
            compact.iter().any(|row| row.contains("联网搜索")),
            "{diagnostic}"
        );
        assert!(
            compact.iter().any(|row| row.contains("CodexCLI")),
            "{diagnostic}"
        );
        for row in 0..20 {
            for column in 0..60 {
                let symbol_width = unicode_width::UnicodeWidthStr::width(
                    terminal.backend().buffer()[(column, row)].symbol(),
                );
                assert!(
                    symbol_width <= usize::from(60 - column),
                    "a wide glyph starts outside the narrow WebSearch buffer at ({column},{row}): {diagnostic}"
                );
            }
        }
    }

    #[test]
    fn task_card_dynamic_fields_are_single_line_terminal_safe_and_bounded() {
        let id = format!("id\u{1b}[31m\u{202e}{}", "x".repeat(20_000));
        let subject = format!(
            "subject\nforged\u{1b}]52;c;payload\u{7}\u{2066}{}",
            "y".repeat(20_000)
        );
        let task = TaskPanelRow {
            id: id.clone(),
            subject: subject.clone(),
            description: None,
            active_form: None,
            owner: None,
            status: TaskPanelStatus::InProgress,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            last_updated_sequence: 1,
        };

        for width in [120_u16, 80, 60] {
            let line = task_card_line(&task, width, UiLanguage::ZhCn, CrabCodeTheme::default());
            let rendered = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            for control in ['\n', '\r', '\u{1b}', '\u{7}', '\u{202e}', '\u{2066}'] {
                assert!(
                    !rendered.contains(control),
                    "unsafe {control:?} at width {width}: {rendered:?}"
                );
            }
            assert!(rendered.contains("␛[31m"), "{width}: {rendered:?}");
            assert!(rendered.contains("⟪U+202E⟫"), "{width}: {rendered:?}");
            assert!(rendered.contains("subject"), "{width}: {rendered:?}");
            if width == 120 {
                assert!(rendered.contains("forged␛]52;c;payload"), "{rendered:?}");
                assert!(rendered.contains("⟪U+2066⟫"), "{rendered:?}");
            }
            assert!(
                rendered.width() <= usize::from(width),
                "{width}: {rendered:?}"
            );

            let backend = TestBackend::new(width, 1);
            let mut terminal = Terminal::new(backend).expect("task safety terminal");
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new(line.clone()), Rect::new(0, 0, width, 1));
                })
                .expect("task safety draw");
            assert_eq!(terminal.backend().buffer().area.height, 1);
        }

        assert_eq!(task.id, id);
        assert_eq!(task.subject, subject);
    }

    #[test]
    fn task_panel_rerender_reuses_the_raw_projection_generation() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_assistant_tool(
            &mut app,
            1,
            "create-cache",
            "TaskCreate",
            json!({"subject":"缓存任务","description":"只投影一次"}),
        );
        ingest_tool_result(
            &mut app,
            2,
            "create-cache",
            json!({"task":{"id":"1","subject":"缓存任务"}}),
        );

        assert_eq!(app.task_panel_projection_rebuild_count(), 0);
        let first = render_buffer(&mut app, 120, 30);
        assert!(first.replace(' ', "").contains("缓存任务"), "{first}");
        let after_first = app.task_panel_projection_rebuild_count();
        assert_eq!(after_first, 1);

        let second = render_buffer(&mut app, 120, 30);
        assert!(second.replace(' ', "").contains("缓存任务"), "{second}");
        assert_eq!(app.task_panel_projection_rebuild_count(), after_first);
    }

    #[test]
    fn task_panel_overflow_keeps_active_work_visible_wide_and_narrow() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut sequence = 1_u64;
        for id in 1..=9 {
            let subject = if id == 9 {
                "活动任务".to_string()
            } else {
                format!("已完成任务{id}")
            };
            let tool_use_id = format!("create-priority-{id}");
            ingest_assistant_tool(
                &mut app,
                sequence,
                &tool_use_id,
                "TaskCreate",
                json!({"subject":subject,"description":"显示优先级测试"}),
            );
            sequence += 1;
            ingest_tool_result(
                &mut app,
                sequence,
                &tool_use_id,
                json!({"task":{"id":id.to_string(),"subject":subject}}),
            );
            sequence += 1;
        }
        for id in 1..=9 {
            let status = if id == 9 { "in_progress" } else { "completed" };
            let tool_use_id = format!("update-priority-{id}");
            ingest_assistant_tool(
                &mut app,
                sequence,
                &tool_use_id,
                "TaskUpdate",
                json!({"taskId":id.to_string(),"status":status}),
            );
            sequence += 1;
            ingest_tool_result(
                &mut app,
                sequence,
                &tool_use_id,
                json!({
                    "success":true,
                    "taskId":id.to_string(),
                    "updatedFields":["status"],
                    "statusChange":{"from":"pending","to":status}
                }),
            );
            sequence += 1;
        }

        for width in [120, 60] {
            let buffer = render_test_buffer(&mut app, width, 30);
            let rows = buffer_row_texts(&buffer)
                .into_iter()
                .map(|row| row.replace(' ', ""))
                .collect::<Vec<_>>();
            let panel_row = rows
                .iter()
                .position(|row| row.contains("任务·8/9已完成·1进行中"))
                .expect("task panel title");
            assert!(
                rows.get(panel_row + 1)
                    .is_some_and(|row| row.contains("活动任务")),
                "width {width}: {rows:#?}"
            );
            assert!(
                rows.iter()
                    .skip(panel_row)
                    .take_while(|row| !row.contains("输入·"))
                    .any(|row| row.contains("另有")),
                "width {width}: {rows:#?}"
            );
        }
    }

    #[test]
    fn malformed_task_after_valid_snapshot_keeps_card_with_controlled_warning() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_assistant_tool(
            &mut app,
            1,
            "create-valid",
            "TaskCreate",
            json!({"subject":"保留的任务","description":"最近有效状态"}),
        );
        ingest_tool_result(
            &mut app,
            2,
            "create-valid",
            json!({"task":{"id":"1","subject":"保留的任务"}}),
        );
        let valid = render_buffer(&mut app, 120, 30);
        assert!(valid.replace(' ', "").contains("保留的任务"), "{valid}");

        ingest_assistant_tool(
            &mut app,
            3,
            "malformed-update",
            "TaskUpdate",
            json!({
                "taskId":"1",
                "status":"completed",
                "unexpected":"must not reach the terminal"
            }),
        );
        let degraded = render_buffer(&mut app, 120, 30);
        let compact = degraded.replace(' ', "");
        assert!(compact.contains("保留的任务"), "{degraded}");
        assert!(compact.contains("任务状态部分不可用"), "{degraded}");
    }

    #[test]
    fn auth_status_stays_source_null_instead_of_entering_the_legacy_renderer() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        let value = json!({
            "type": "auth_status",
            "isAuthenticating": true,
            "output": [
                "Open https://auth.acosmi.example/device",
                "Code: ABCD-EFGH"
            ],
            "uuid": "auth",
            "session_id": "session"
        });
        let effect = app.projection.ingest(RawEnvelope {
            sequence: 1,
            encoded_len: 1,
            value: value.clone(),
            classification: EnvelopeClass::AuthStatus,
            correlation: None,
        });
        assert_eq!(effect, crate::sdk_projection::ProjectionEffect::None);
        assert!(app.projection.items().is_empty());
        assert_eq!(app.projection.raw_envelopes()[0].value, value);
    }

    #[test]
    fn sticky_prompt_gradually_collapses_to_minimum_height() {
        assert_eq!(gradual_sticky_height(8, 4, 1, 24), 7);
        assert_eq!(gradual_sticky_height(8, 4, 3, 24), 5);
        assert_eq!(gradual_sticky_height(8, 4, 6, 24), 4);
        assert_eq!(gradual_sticky_height(8, 4, 20, 24), 4);
    }

    #[test]
    fn selection_box_top_clipped_uses_dashed_sides_without_top_corners() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        SelectionBox::new(Rect::new(0, 2, 10, 4), Style::default())
            .with_top_clipped(true)
            .render(&mut buffer);
        assert_ne!(buffer[(0, 1)].symbol(), "┌");
        assert_ne!(buffer[(9, 1)].symbol(), "┐");
        assert_eq!(buffer[(0, 2)].symbol(), "┆");
        assert_eq!(buffer[(9, 2)].symbol(), "┆");
        for y in 3..=5 {
            assert_eq!(buffer[(0, y)].symbol(), "│");
            assert_eq!(buffer[(9, y)].symbol(), "│");
        }
        assert_eq!(buffer[(0, 6)].symbol(), "└");
        assert_eq!(buffer[(9, 6)].symbol(), "┘");
    }

    #[test]
    fn production_renderer_paints_composer_preview_and_attachment_hitbox() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.attachments
            .push(crate::composer_image::LoadedComposerImage {
                filename: "preview.png".to_string(),
                data_url: "data:image/png;base64,AA==".to_string(),
                mime: "image/png".to_string(),
                width: 12,
                height: 8,
                encoded_bytes: 1,
                preview_identity: crate::crabcode_image_overlay::next_image_identity(),
                terminal_preview: None,
            });
        app.composer_image_preview = Some(0);

        let text = render_buffer(&mut app, 100, 30);
        assert!(text.replace(' ', "").contains("图片#1"), "{text}");
        assert!(text.replace(' ', "").contains("格式：PNG"), "{text}");
        assert!(text.replace(' ', "").contains("预览不可用"), "{text}");
        assert_eq!(app.composer_attachment_hitboxes.len(), 1);
        assert_eq!(app.composer_attachment_hitboxes[0].1, 0);
    }

    fn populate_direct_nested_progress(app: &mut TuiApp, progress_type: &str) {
        if progress_type == "agent_progress" {
            ingest_runtime_value(
                app,
                0,
                json!({
                    "type":"progress",
                    "data":{
                        "type":"agent_progress",
                        "prompt":"EXACT-NESTED-AGENT-PROMPT",
                        "agentId":"agent-1",
                        "message":{
                            "type":"user",
                            "uuid":"nested-user",
                            "timestamp":"2026-07-27T00:00:00.000Z",
                            "message":{
                                "role":"user",
                                "content":"EXTERNAL-AGENT-USER-MUST-STAY-HIDDEN"
                            }
                        }
                    },
                    "toolUseID":"nested-progress",
                    "parentToolUseID":"nested-parent",
                    "uuid":"nested-progress-user",
                    "timestamp":"2026-07-27T00:00:00.000Z"
                }),
            );
        }

        let groups = [
            vec!["NESTED-GROUP-ONE"],
            vec!["NESTED-GROUP-TWO"],
            vec!["NESTED-GROUP-THREE"],
            vec![
                "NESTED-GROUP-FOUR-A",
                "NESTED-GROUP-FOUR-B",
                "NESTED-GROUP-FOUR-C",
                "NESTED-GROUP-FOUR-D",
            ],
            vec!["NESTED-GROUP-FIVE"],
        ];
        for (offset, labels) in groups.into_iter().enumerate() {
            let sequence = offset as u64 + u64::from(progress_type == "agent_progress");
            let mut content = labels
                .into_iter()
                .map(|label| json!({"type":"text","text":label}))
                .collect::<Vec<_>>();
            content.push(json!({
                "type":"tool_use",
                "id":format!("nested-tool-{offset}"),
                "name":"FileRead",
                "input":{"file_path":format!("/workspace/{offset}.rs")}
            }));
            ingest_runtime_value(
                app,
                sequence,
                json!({
                    "type":"progress",
                    "data":{
                        "type":progress_type,
                        "prompt":if progress_type == "agent_progress" {
                            ""
                        } else {
                            "EXACT-SKILL-CONTENT"
                        },
                        "agentId":"agent-1",
                        "message":{
                            "type":"assistant",
                            "uuid":format!("nested-assistant-{offset}"),
                            "timestamp":"2026-07-27T00:00:01.000Z",
                            "message":{
                                "id":format!("nested-message-{offset}"),
                                "role":"assistant",
                                "content":content
                            }
                        }
                    },
                    "toolUseID":"nested-progress",
                    "parentToolUseID":"nested-parent",
                    "uuid":format!("nested-progress-{offset}"),
                    "timestamp":"2026-07-27T00:00:01.000Z"
                }),
            );
        }
    }

    #[test]
    fn production_nested_progress_slices_last_three_envelopes_not_projected_blocks() {
        for progress_type in ["agent_progress", "skill_progress"] {
            let mut live =
                TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, false);
            live.release_startup_barrier_for_test();
            populate_direct_nested_progress(&mut live, progress_type);

            let live_frame = render_buffer(&mut live, 120, 64);
            assert!(!live_frame.contains("NESTED-GROUP-ONE"), "{live_frame}");
            assert!(!live_frame.contains("NESTED-GROUP-TWO"), "{live_frame}");
            assert!(live_frame.contains("NESTED-GROUP-THREE"), "{live_frame}");
            for label in [
                "NESTED-GROUP-FOUR-A",
                "NESTED-GROUP-FOUR-B",
                "NESTED-GROUP-FOUR-C",
                "NESTED-GROUP-FOUR-D",
                "NESTED-GROUP-FIVE",
            ] {
                assert!(
                    live_frame.contains(label),
                    "{progress_type} sliced projected blocks instead of outer progress messages: {live_frame}"
                );
            }
            assert!(!live_frame.contains("EXACT-NESTED-AGENT-PROMPT"));
            assert!(!live_frame.contains("EXTERNAL-AGENT-USER-MUST-STAY-HIDDEN"));

            let mut transcript =
                TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, true);
            transcript.release_startup_barrier_for_test();
            populate_direct_nested_progress(&mut transcript, progress_type);

            let transcript_frame = render_buffer(&mut transcript, 120, 96);
            assert!(
                transcript_frame.contains("NESTED-GROUP-ONE"),
                "{transcript_frame}"
            );
            assert!(
                transcript_frame.contains("NESTED-GROUP-TWO"),
                "{transcript_frame}"
            );
            if progress_type == "agent_progress" {
                assert!(
                    transcript_frame.replace(' ', "").contains("提示词："),
                    "{transcript_frame}"
                );
                assert!(
                    transcript_frame.contains("EXACT-NESTED-AGENT-PROMPT"),
                    "{transcript_frame}"
                );
                assert!(
                    !transcript_frame.contains("EXTERNAL-AGENT-USER-MUST-STAY-HIDDEN"),
                    "{transcript_frame}"
                );
            } else {
                assert!(
                    !transcript_frame.replace(' ', "").contains("提示词："),
                    "fixed Skill progress has no prompt panel: {transcript_frame}"
                );
            }
        }
    }

    #[test]
    fn production_mcp_progress_uses_exact_twenty_cell_bar_and_raw_number() {
        let mut bar_app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        bar_app.release_startup_barrier_for_test();
        ingest_runtime_value(
            &mut bar_app,
            0,
            json!({
                "type":"progress",
                "data":{
                    "type":"mcp_progress",
                    "status":"progress",
                    "serverName":"filesystem",
                    "toolName":"read_file",
                    "progress":3,
                    "total":4,
                    "progressMessage":"Reading exact bytes"
                },
                "toolUseID":"mcp-progress",
                "parentToolUseID":"mcp-parent",
                "uuid":"mcp-progress-0",
                "timestamp":"2026-07-27T00:00:00.000Z"
            }),
        );
        let bar = direct_mcp_progress_bar(0.75);
        assert_eq!(bar.width(), DIRECT_MCP_PROGRESS_BAR_CELLS);
        assert_eq!(bar, "███████████████     ");
        let bar_frame = render_buffer(&mut bar_app, 100, 24);
        assert!(bar_frame.contains("Reading exact bytes"), "{bar_frame}");
        assert!(
            bar_frame.contains("███████████████      75%"),
            "{bar_frame}"
        );

        let mut raw_app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        raw_app.release_startup_barrier_for_test();
        ingest_runtime_value(
            &mut raw_app,
            0,
            json!({
                "type":"progress",
                "data":{
                    "type":"mcp_progress",
                    "status":"progress",
                    "serverName":"filesystem",
                    "toolName":"read_file",
                    "progress":7.25
                },
                "toolUseID":"mcp-progress",
                "parentToolUseID":"mcp-parent",
                "uuid":"mcp-progress-raw",
                "timestamp":"2026-07-27T00:00:00.000Z"
            }),
        );
        let raw_frame = render_buffer(&mut raw_app, 100, 24);
        assert!(
            raw_frame.replace(' ', "").contains("处理中…7.25"),
            "{raw_frame}"
        );
        assert!(!raw_frame.contains("75%"), "{raw_frame}");
    }

    #[test]
    fn production_pre_and_post_tool_hook_progress_have_no_live_rows() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for (sequence, hook_event) in ["PreToolUse", "PostToolUse"].into_iter().enumerate() {
            ingest_runtime_value(
                &mut app,
                sequence as u64,
                json!({
                    "type":"progress",
                    "data":{
                        "type":"hook_progress",
                        "hookEvent":hook_event,
                        "hookName":format!("hook-{sequence}"),
                        "command":"check"
                    },
                    "toolUseID":format!("hook-progress-{sequence}"),
                    "parentToolUseID":"hook-parent",
                    "uuid":format!("hook-progress-{sequence}"),
                    "timestamp":"2026-07-27T00:00:00.000Z"
                }),
            );
        }
        assert!(
            app.projection.items().is_empty(),
            "live UI cannot manufacture the fixed transcript count from absent projected state"
        );
        assert_eq!(app.projection.raw_envelope_count(), 2);
        let frame = render_buffer(&mut app, 100, 24);
        assert!(!frame.contains("PreToolUse"), "{frame}");
        assert!(!frame.contains("PostToolUse"), "{frame}");
    }

    #[test]
    fn production_stop_hook_summary_removes_the_live_same_name_batch_row() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for sequence in 0..2_u64 {
            ingest_runtime_value(
                &mut app,
                sequence,
                json!({
                    "type":"progress",
                    "data":{
                        "type":"hook_progress",
                        "hookEvent":"Stop",
                        "hookName":"Stop",
                        "command":format!("run-{sequence}")
                    },
                    "toolUseID":format!("stop-progress-{sequence}"),
                    "parentToolUseID":"stop-batch",
                    "uuid":format!("stop-progress-{sequence}"),
                    "timestamp":"2026-07-27T00:00:00.000Z"
                }),
            );
        }
        let running = render_buffer(&mut app, 100, 24);
        let running_compact = running.replace(' ', "");
        let running_marker = if running_compact.contains("正在运行Stop钩子…") {
            "正在运行Stop钩子…"
        } else {
            "RunningStophooks…"
        };
        assert!(running_compact.contains(running_marker), "{running}");

        ingest_runtime_value(
            &mut app,
            2,
            json!({
                "type":"attachment",
                "attachment":{
                    "type":"hook_success",
                    "content":"",
                    "hookName":"Stop",
                    "toolUseID":"stop-batch",
                    "hookEvent":"Stop"
                },
                "uuid":"one-stop-terminal",
                "timestamp":"2026-07-27T00:00:01.000Z"
            }),
        );
        let partially_resolved = render_buffer(&mut app, 100, 24);
        assert!(
            partially_resolved.replace(' ', "").contains(running_marker),
            "{partially_resolved}"
        );

        ingest_runtime_value(
            &mut app,
            3,
            json!({
                "type":"system",
                "subtype":"stop_hook_summary",
                "hookCount":2,
                "hookInfos":[
                    {"hookName":"run-0","durationMs":10},
                    {"hookName":"run-1","durationMs":11}
                ],
                "hookErrors":[],
                "preventedContinuation":false,
                "stopReason":"",
                "hasOutput":false,
                "level":"suggestion",
                "toolUseID":"stop-batch",
                "uuid":"stop-summary",
                "timestamp":"2026-07-27T00:00:02.000Z"
            }),
        );
        let completed = render_buffer(&mut app, 100, 24);
        assert!(
            !completed.replace(' ', "").contains(running_marker),
            "{completed}"
        );
    }

    #[test]
    fn production_hook_transcript_summaries_match_fixed_direct_tui() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_assistant_tool(
            &mut app,
            0,
            "hook-parent",
            "Bash",
            json!({"command":"printf HOOK-TOOL-BODY"}),
        );
        ingest_runtime_value(
            &mut app,
            1,
            json!({
                "type":"progress",
                "data":{
                    "type":"hook_progress",
                    "hookEvent":"PreToolUse",
                    "hookName":"pre-one",
                    "command":"check"
                },
                "toolUseID":"hook-progress-pre-one",
                "parentToolUseID":"hook-parent",
                "uuid":"hook-progress-pre-one",
                "timestamp":"2026-07-27T00:00:00.000Z"
            }),
        );
        let pre = app
            .projection
            .direct_hook_progress_presentation("hook-parent", "PreToolUse")
            .expect("retained PreToolUse count");
        assert_eq!(pre.in_progress_count, 1);
        assert_eq!(
            pre.resolved_count, 0,
            "fixed transcript summary is based on started count, not completion"
        );

        let live = render_buffer(&mut app, 120, 40);
        assert!(
            !live.replace(' ', "").contains("1PreToolUseHook已运行"),
            "{live}"
        );
        assert!(!live.contains("PostToolUse"), "{live}");

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(!app.composer_focused());
        let singular = render_buffer(&mut app, 120, 40);
        assert!(
            singular.replace(' ', "").contains("1PreToolUseHook已运行"),
            "{singular}"
        );

        ingest_runtime_value(
            &mut app,
            2,
            json!({
                "type":"progress",
                "data":{
                    "type":"hook_progress",
                    "hookEvent":"PreToolUse",
                    "hookName":"pre-two",
                    "command":"check"
                },
                "toolUseID":"hook-progress-pre-two",
                "parentToolUseID":"hook-parent",
                "uuid":"hook-progress-pre-two",
                "timestamp":"2026-07-27T00:00:01.000Z"
            }),
        );
        let pluralized = render_buffer(&mut app, 120, 40);
        assert!(
            pluralized
                .replace(' ', "")
                .contains("2PreToolUseHook已运行"),
            "{pluralized}"
        );
        assert!(
            !pluralized
                .replace(' ', "")
                .contains("1PreToolUseHook已运行"),
            "{pluralized}"
        );

        ingest_runtime_value(
            &mut app,
            3,
            json!({
                "type":"progress",
                "data":{
                    "type":"bash_progress",
                    "output":"HOOK-SHELL-LIVE",
                    "fullOutput":"HOOK-SHELL-LIVE",
                    "elapsedTimeSeconds":1,
                    "totalLines":1
                },
                "toolUseID":"shell-progress",
                "parentToolUseID":"hook-parent",
                "uuid":"shell-progress",
                "timestamp":"2026-07-27T00:00:02.000Z"
            }),
        );
        for (sequence, hook_name) in [(4, "post-one"), (5, "post-two")] {
            ingest_runtime_value(
                &mut app,
                sequence,
                json!({
                    "type":"progress",
                    "data":{
                        "type":"hook_progress",
                        "hookEvent":"PostToolUse",
                        "hookName":hook_name,
                        "command":"check"
                    },
                    "toolUseID":format!("hook-progress-{hook_name}"),
                    "parentToolUseID":"hook-parent",
                    "uuid":format!("hook-progress-{hook_name}"),
                    "timestamp":"2026-07-27T00:00:03.000Z"
                }),
            );
        }
        ingest_runtime_value(
            &mut app,
            6,
            json!({
                "type":"user",
                "uuid":"hook-result",
                "session_id":"session",
                "parent_tool_use_id":"hook-parent",
                "message":{"content":[{
                    "type":"tool_result",
                    "tool_use_id":"hook-parent",
                    "content":"HOOK-RESULT-BODY",
                    "is_error":false
                }]}
            }),
        );
        let post = app
            .projection
            .direct_hook_progress_presentation("hook-parent", "PostToolUse")
            .expect("retained PostToolUse count");
        assert_eq!(post.in_progress_count, 2);
        assert_eq!(post.resolved_count, 0);

        let transcript = render_buffer(&mut app, 120, 40);
        let transcript_compact = transcript.replace(' ', "");
        assert!(
            transcript_compact.contains("2PreToolUseHook已运行"),
            "{transcript}"
        );
        assert!(
            transcript_compact.contains("2PostToolUseHook已运行"),
            "{transcript}"
        );
        assert_eq!(
            transcript_compact.matches("2PreToolUseHook已运行").count(),
            1,
            "the same parent id on shell progress must not duplicate the tool summary: {transcript}"
        );
        assert_eq!(
            transcript_compact.matches("2PostToolUseHook已运行").count(),
            1,
            "PostToolUse belongs to the final success result, not shell progress: {transcript}"
        );
        let pre_index = transcript_compact
            .find("2PreToolUseHook已运行")
            .expect("PreToolUse summary");
        let tool_body_index = transcript_compact
            .find("HOOK-TOOL-BODY")
            .expect("tool body");
        let result_body_index = transcript_compact
            .find("HOOK-RESULT-BODY")
            .expect("result body");
        let post_index = transcript_compact
            .find("2PostToolUseHook已运行")
            .expect("PostToolUse summary");
        assert!(
            pre_index < tool_body_index,
            "PreToolUse summary precedes tool progress/body: {transcript}"
        );
        assert!(
            result_body_index < post_index,
            "PostToolUse summary follows the successful result: {transcript}"
        );

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(app.composer_focused());
        let live_again = render_buffer(&mut app, 120, 40);
        assert!(!live_again.contains("PreToolUse"), "{live_again}");
        assert!(!live_again.contains("PostToolUse"), "{live_again}");
    }

    #[test]
    fn production_post_tool_hook_transcript_summary_excludes_error_result() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_assistant_tool(
            &mut app,
            0,
            "failed-hook-parent",
            "FileRead",
            json!({"file_path":"/tmp/missing"}),
        );
        ingest_runtime_value(
            &mut app,
            1,
            json!({
                "type":"progress",
                "data":{
                    "type":"hook_progress",
                    "hookEvent":"PostToolUse",
                    "hookName":"post-error",
                    "command":"check"
                },
                "toolUseID":"hook-progress-post-error",
                "parentToolUseID":"failed-hook-parent",
                "uuid":"hook-progress-post-error",
                "timestamp":"2026-07-27T00:00:00.000Z"
            }),
        );
        ingest_runtime_value(
            &mut app,
            2,
            json!({
                "type":"user",
                "uuid":"failed-hook-result",
                "session_id":"session",
                "parent_tool_use_id":"failed-hook-parent",
                "message":{"content":[{
                    "type":"tool_result",
                    "tool_use_id":"failed-hook-parent",
                    "content":"HOOK-ERROR-BODY",
                    "is_error":true
                }]}
            }),
        );
        assert_eq!(
            app.projection
                .direct_hook_progress_presentation("failed-hook-parent", "PostToolUse")
                .expect("retained PostToolUse count")
                .in_progress_count,
            1
        );
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let transcript = render_buffer(&mut app, 120, 40);
        assert!(transcript.contains("HOOK-ERROR-BODY"), "{transcript}");
        assert!(
            !transcript.contains("PostToolUse"),
            "fixed UserToolErrorMessage does not append the success-only hook summary: {transcript}"
        );
    }

    #[test]
    fn production_projected_item_renderer_closes_fixed_bash_progress_append_chunks() {
        let fragments = [
            ("Com", "Com"),
            ("pili", "Compili"),
            ("ng ", "Compiling "),
            ("crat", "Compiling crat"),
            ("e\n", "Compiling crate\n"),
        ];
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for (sequence, (output, full_output)) in fragments.into_iter().enumerate() {
            ingest_runtime_value(
                &mut app,
                sequence as u64,
                json!({
                    "type":"progress",
                    "data":{
                        "type":"bash_progress",
                        "output":output,
                        "fullOutput":full_output,
                        "elapsedTimeSeconds":sequence,
                        "totalLines":1
                    },
                    "toolUseID":format!("bash-progress-{sequence}"),
                    "parentToolUseID":"shell-tool-1",
                    "uuid":format!("progress-{sequence}"),
                    "timestamp":"2026-07-27T00:00:00.000Z"
                }),
            );
        }
        let terminal_items = app
            .projection
            .items()
            .iter()
            .filter(|item| item.kind == ProjectedKind::TerminalOutput)
            .collect::<Vec<_>>();
        assert_eq!(
            terminal_items.len(),
            1,
            "all five cumulative updates must retain one terminal-output item"
        );
        let item = terminal_items[0];
        assert_eq!(item.text, "Compiling crate\n");
        assert_eq!(item.raw_sequences, [0, 1, 2, 3, 4]);

        let rendered_lines = terminal_lines_for_projected_item(item, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(
            rendered_lines
                .iter()
                .any(|line| line == "  Compiling crate"),
            "production projected-item renderer changed shell output: {rendered_lines:?}"
        );
        let frame = render_buffer(&mut app, 100, 24);
        assert!(frame.contains("Compiling crate"), "{frame}");
    }

    #[test]
    fn renders_unicode_markdown_terminal_output_and_status_without_panic() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for (sequence, value, class) in [
            (
                0,
                json!({
                    "type":"system",
                    "subtype":"init",
                    "apiKeySource":"none",
                    "crab_code_version":"1.0.0",
                    "cwd":"/tmp",
                    "tools":[],
                    "mcp_servers":[],
                    "model":"m",
                    "permissionMode":"default",
                    "slash_commands":[],
                    "output_style":"default",
                    "skills":[],
                    "plugins":[],
                    "uuid":"init",
                    "session_id":"s"
                }),
                EnvelopeClass::System(SystemSubtype::Init),
            ),
            (
                1,
                json!({"type":"assistant","uuid":"a","session_id":"s","parent_tool_use_id":null,"message":{"id":"m","content":[{"type":"text","text":"# 标题\nhello `code` 🌍"}]}}),
                EnvelopeClass::Assistant,
            ),
            (
                2,
                json!({"type":"system","subtype":"local_command_output","content":"\u{1b}[32mok\u{1b}[0m\rgo","uuid":"local","session_id":"s"}),
                EnvelopeClass::System(SystemSubtype::LocalCommandOutput),
            ),
        ] {
            app.projection.ingest(RawEnvelope {
                sequence,
                encoded_len: 1,
                value,
                classification: class,
                correlation: None,
            });
        }
        let rendered = render_buffer(&mut app, 80, 40);
        assert!(rendered.contains("CrabCode"), "{rendered}");
        assert!(
            rendered.contains('标') && rendered.contains('题'),
            "{rendered}"
        );
        assert!(rendered.contains("code"), "{rendered}");
        assert!(rendered.contains("go"), "{rendered}");
    }

    #[test]
    fn renderer_verbose_and_selected_fold_control_thinking_and_info_rows() {
        fn populate(app: &mut TuiApp) {
            app.handle_runtime_event(crate::sdk_runtime::RuntimeEvent::Envelope(RawEnvelope {
                sequence: 0,
                encoded_len: 1,
                value: json!({
                    "type":"assistant",
                    "uuid":"assistant",
                    "session_id":"session",
                    "parent_tool_use_id":null,
                    "message":{
                        "id":"message",
                        "content":[{
                            "type":"thinking",
                            "thinking":"REASON-ONE\nREASON-TWO\nREASON-THREE\nREASON-FOUR",
                            "signature":"signature"
                        }]
                    }
                }),
                classification: EnvelopeClass::Assistant,
                correlation: None,
            }));
            app.projection
                .project_wire_fixtures(
                    &[json!({
                        "type":"system",
                        "subtype":"informational",
                        "level":"info",
                        "uuid":"quiet-info",
                        "session_id":"session",
                        "timestamp":"2026-07-27T00:00:00.000Z",
                        "content":"QUIET-INFO-BODY"
                    })],
                    1,
                )
                .expect("historical info");
            app.synchronize_transcript_interactions();
        }

        let mut quiet =
            TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, false);
        quiet.release_startup_barrier_for_test();
        populate(&mut quiet);
        let collapsed = render_buffer(&mut quiet, 90, 30);
        assert!(!collapsed.contains("REASON-FOUR"), "{collapsed}");
        assert!(!collapsed.contains("QUIET-INFO-BODY"), "{collapsed}");

        quiet.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        quiet.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('l'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let expanded = render_buffer(&mut quiet, 90, 30);
        assert!(expanded.contains("REASON-FOUR"), "{expanded}");
        assert!(!expanded.contains("QUIET-INFO-BODY"), "{expanded}");

        let mut verbose =
            TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, true);
        verbose.release_startup_barrier_for_test();
        populate(&mut verbose);
        let verbose_render = render_buffer(&mut verbose, 90, 30);
        assert!(verbose_render.contains("REASON-FOUR"), "{verbose_render}");
        assert!(
            verbose_render.contains("QUIET-INFO-BODY"),
            "{verbose_render}"
        );
    }

    #[test]
    fn fixed_transcript_status_chrome_is_bilingual_and_preserves_dynamic_values() {
        fn render_item_text(
            item: &ProjectedItem,
            render_context: ProjectedItemRenderContext,
        ) -> String {
            let mut markdown_stream = CrabCodeMarkdownStream::default();
            render_projected_item_with_state_context(
                item,
                100,
                CrabCodeTheme::NIGHT,
                &[],
                TranscriptDisplayMode::Expanded,
                false,
                &mut markdown_stream,
                render_context,
            )
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
        }

        for (
            language,
            advising,
            unavailable,
            reviewed,
            prompt_label,
            running,
            processing,
            hook_suffix,
        ) in [
            (
                UiLanguage::ZhCn,
                "顾问审阅",
                "顾问不可用",
                ADVISOR_REVIEWED_MESSAGE_ZH,
                "提示词：",
                "运行中…",
                "处理中…",
                "Hook 已运行",
            ),
            (
                UiLanguage::EnUs,
                "Advising",
                "Advisor unavailable",
                ADVISOR_REVIEWED_MESSAGE_EN,
                "Prompt:",
                "Running…",
                "Processing…",
                "hook ran",
            ),
        ] {
            let context = ProjectedItemRenderContext {
                ui_language: language,
                ..ProjectedItemRenderContext::default()
            };

            let mut advisor = projected_item("advisor", "", 1);
            advisor.presentation.advisor = Some(AdvisorPresentation::Invocation {
                input: json!({"focus":"EXACT-DYNAMIC-FOCUS"}),
                state: AdvisorInvocationState::InProgress,
            });
            let advisor_text = render_item_text(&advisor, context);
            assert!(advisor_text.contains(advising), "{advisor_text}");
            assert!(
                advisor_text.contains("EXACT-DYNAMIC-FOCUS"),
                "{advisor_text}"
            );

            advisor.presentation.advisor = Some(AdvisorPresentation::Result(
                AdvisorResultPresentation::Error {
                    error_code: "exact_backend_code".to_string(),
                },
            ));
            let advisor_error = render_item_text(&advisor, context);
            assert!(advisor_error.contains(unavailable), "{advisor_error}");
            assert!(
                advisor_error.contains("exact_backend_code"),
                "{advisor_error}"
            );
            assert_eq!(advisor_reviewed_message(language), reviewed);

            let mut nested = projected_item("nested", "", 2);
            nested.kind = ProjectedKind::Progress;
            nested.presentation.direct_progress = Some(DirectProgressPresentation::Nested {
                progress_type: "agent_progress".to_string(),
                parent_tool_use_id: "parent".to_string(),
                progress_tool_use_id: "progress".to_string(),
                prompt: "EXACT-DYNAMIC-PROMPT".to_string(),
                agent_id: "agent".to_string(),
                message_kind: crate::sdk_projection::DirectNestedMessageKind::Assistant,
                usage: None,
            });
            let nested_text = render_item_text(
                &nested,
                ProjectedItemRenderContext {
                    show_nested_agent_prompt: true,
                    ..context
                },
            );
            assert!(nested_text.contains(prompt_label), "{nested_text}");
            assert!(
                nested_text.contains("EXACT-DYNAMIC-PROMPT"),
                "{nested_text}"
            );

            let mut mcp = projected_item("mcp", "", 3);
            mcp.kind = ProjectedKind::Progress;
            mcp.presentation.direct_progress = Some(DirectProgressPresentation::Mcp {
                status: "progress".to_string(),
                server_name: "EXACT-SERVER".to_string(),
                tool_name: "EXACT-TOOL".to_string(),
                progress: None,
                total: None,
                elapsed_time_ms: None,
                progress_message: None,
                percentage: None,
            });
            let mcp_running = render_item_text(&mcp, context);
            assert!(mcp_running.contains(running), "{mcp_running}");

            if let Some(DirectProgressPresentation::Mcp { progress, .. }) =
                mcp.presentation.direct_progress.as_mut()
            {
                *progress = Some(
                    serde_json::Number::from_f64(7.25).expect("finite exact progress fixture"),
                );
            }
            let mcp_processing = render_item_text(&mcp, context);
            assert!(mcp_processing.contains(processing), "{mcp_processing}");
            assert!(mcp_processing.contains("7.25"), "{mcp_processing}");

            let mut hook = RenderedTranscriptPart::default();
            append_direct_hook_transcript_summary(
                &mut hook,
                "PreToolUse",
                1,
                CrabCodeTheme::NIGHT,
                &[],
                language,
            );
            let hook_text = hook
                .lines
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(hook_text.contains("PreToolUse"), "{hook_text}");
            assert!(hook_text.contains(hook_suffix), "{hook_text}");
        }
    }

    #[test]
    fn advisor_feedback_defaults_to_reviewed_hint_and_expands_to_exact_feedback() {
        let exact_feedback = "Keep StructuredIO direct and preserve every backend discriminator.";
        let mut app =
            TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, false);
        app.release_startup_barrier_for_test();
        ingest_runtime_value(
            &mut app,
            0,
            json!({
                "type":"assistant","uuid":"advisor-result","session_id":"session",
                "parent_tool_use_id":null,
                "message":{"id":"message","content":[
                    {
                        "type":"server_tool_use","id":"advisor-1","name":"advisor",
                        "input":{"focus":"direct backend fidelity"}
                    },
                    {
                        "type":"advisor_tool_result","tool_use_id":"advisor-1",
                        "content":{"type":"advisor_result","text":exact_feedback}
                    }
                ]}
            }),
        );

        let collapsed = render_buffer(&mut app, 100, 20);
        let key = app.projection.items()[1].key.clone();
        assert_eq!(
            app.transcript_item_interactions[&key].mode,
            TranscriptDisplayMode::Collapsed
        );
        let collapsed_compact = collapsed.replace(' ', "");
        assert!(collapsed_compact.contains("✓顾问审阅"), "{collapsed}");
        assert!(
            collapsed.contains(r#"{"focus":"direct backend fidelity"}"#),
            "{collapsed}"
        );
        assert!(
            collapsed_compact.contains(ADVISOR_REVIEWED_MESSAGE_ZH),
            "{collapsed}"
        );
        assert!(!collapsed.contains(exact_feedback), "{collapsed}");

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('e'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let expanded = render_buffer(&mut app, 100, 20);
        assert_eq!(
            app.transcript_item_interactions[&key].mode,
            TranscriptDisplayMode::Expanded
        );
        assert!(expanded.contains(exact_feedback), "{expanded}");
    }

    #[test]
    fn advisor_stream_invocation_renders_input_and_exact_error_state() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for (sequence, value) in [
            json!({
                "type":"stream_event","uuid":"0","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"message_start","message":{"id":"message"}}
            }),
            json!({
                "type":"stream_event","uuid":"1","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_start","index":0,"content_block":{
                    "type":"server_tool_use","id":"advisor-1","name":"advisor",
                    "input":{"focus":"stream fidelity"}
                }}
            }),
        ]
        .into_iter()
        .enumerate()
        {
            ingest_runtime_value(&mut app, sequence as u64, value);
        }
        let running = render_buffer(&mut app, 100, 20);
        assert!(running.replace(' ', "").contains("顾问审阅"), "{running}");
        assert!(
            running.contains(r#"{"focus":"stream fidelity"}"#),
            "{running}"
        );
        assert!(running.contains('…'), "{running}");

        for (sequence, value) in [
            json!({
                "type":"stream_event","uuid":"2","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_stop","index":0}
            }),
            json!({
                "type":"stream_event","uuid":"3","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_start","index":1,"content_block":{
                    "type":"advisor_tool_result","tool_use_id":"advisor-1",
                    "content":{"type":"advisor_tool_result_error","error_code":"capacity_exhausted"}
                }}
            }),
            json!({
                "type":"stream_event","uuid":"4","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_stop","index":1}
            }),
            json!({
                "type":"stream_event","uuid":"5","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"message_stop"}
            }),
        ]
        .into_iter()
        .enumerate()
        {
            ingest_runtime_value(&mut app, sequence as u64 + 2, value);
        }
        let failed = render_buffer(&mut app, 100, 24);
        let failed_compact = failed.replace(' ', "");
        assert!(failed_compact.contains("×顾问审阅"), "{failed}");
        assert!(
            failed_compact.contains("顾问不可用(capacity_exhausted)"),
            "{failed}"
        );
    }

    #[test]
    fn advisor_redacted_renderer_never_exposes_ciphertext() {
        let encrypted = "never-render-this-advisor-ciphertext";
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        ingest_runtime_value(
            &mut app,
            0,
            json!({
                "type":"assistant","uuid":"advisor-results","session_id":"session",
                "parent_tool_use_id":null,
                "message":{"id":"message","content":[
                    {
                        "type":"advisor_tool_result","tool_use_id":"advisor-1",
                        "content":{
                            "type":"advisor_redacted_result",
                            "encrypted_content":encrypted
                        }
                    },
                    {
                        "type":"advisor_tool_result","tool_use_id":"advisor-2",
                        "content":{
                            "type":"advisor_tool_result_error",
                            "error_code":"policy_denied"
                        }
                    }
                ]}
            }),
        );

        let rendered = render_buffer(&mut app, 100, 24);
        let rendered_compact = rendered.replace(' ', "");
        assert!(
            rendered_compact.contains(ADVISOR_REVIEWED_MESSAGE_ZH),
            "{rendered}"
        );
        assert!(
            rendered_compact.contains("顾问不可用(policy_denied)"),
            "{rendered}"
        );
        assert!(!rendered.contains(encrypted), "{rendered}");
    }

    #[test]
    fn truncated_thinking_keeps_only_the_pinned_upstream_three_line_tail() {
        let mut item = projected_item(
            "thinking",
            "old-one  \nold-two  \nkeep-one  \nkeep-two  \nkeep-three",
            1,
        );
        item.kind = ProjectedKind::Thinking;
        item.title = "Thinking".to_string();
        let mut markdown_stream = CrabCodeMarkdownStream::default();
        let rendered = render_projected_item_with_state(
            &item,
            80,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Truncated,
            false,
            &mut markdown_stream,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
        );
        let text = rendered.lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(text.iter().any(|line| line.trim() == "…"), "{text:?}");
        assert!(
            !text.iter().any(|line| line.contains("old-one")),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|line| line.contains("old-two")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("keep-one")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("keep-two")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("keep-three")),
            "{text:?}"
        );
    }

    #[test]
    fn selected_key_and_fold_survive_real_history_relayout_and_resize() {
        let mut app =
            TuiApp::new_with_presentation(&json!({}), InitialSessionRequest::New, None, false);
        app.release_startup_barrier_for_test();
        app.handle_runtime_event(crate::sdk_runtime::RuntimeEvent::Envelope(RawEnvelope {
            sequence: 0,
            encoded_len: 1,
            value: json!({
                "type":"assistant",
                "uuid":"assistant",
                "session_id":"session",
                "parent_tool_use_id":null,
                "message":{
                    "id":"message",
                    "content":[{
                        "type":"thinking",
                        "thinking":"stable selected reasoning",
                        "signature":"signature"
                    }]
                }
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        }));
        let _ = render_buffer(&mut app, 80, 20);
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('l'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let selected = app
            .selected_transcript_key
            .clone()
            .expect("thinking selected");
        assert_eq!(
            app.transcript_item_interactions[&selected].mode,
            TranscriptDisplayMode::Expanded
        );

        app.projection
            .project_wire_fixtures(
                &[json!({
                    "type":"user",
                    "uuid":"older",
                    "session_id":"session",
                    "parent_tool_use_id":null,
                    "message":{"content":"history inserted before selection"}
                })],
                1,
            )
            .expect("history");
        app.synchronize_transcript_interactions();
        let narrow = render_buffer(&mut app, 28, 16);
        assert!(narrow.contains("stable"), "{narrow}");
        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(selected.as_str())
        );
        assert_eq!(
            app.transcript_item_interactions[&selected].mode,
            TranscriptDisplayMode::Expanded
        );
    }

    #[test]
    fn backend_metadata_and_overlay_chrome_cannot_inject_terminal_controls() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for (sequence, value, class) in [
            (
                0,
                json!({
                    "type":"system",
                    "subtype":"init",
                    "apiKeySource":"none",
                    "crab_code_version":"1.0.0",
                    "cwd":"/tmp/\u{1b}]52;c;cwd\u{7}",
                    "model":"model\u{1b}]8;;https://evil.test\u{7}",
                    "permissionMode":"default",
                    "tools":[],
                    "mcp_servers":[],
                    "slash_commands":[],
                    "output_style":"default",
                    "skills":[],
                    "plugins":[],
                    "uuid":"init\u{202e}exe",
                    "session_id":"session\u{1b}[31m"
                }),
                EnvelopeClass::System(SystemSubtype::Init),
            ),
            (
                1,
                json!({
                    "type":"system",
                    "subtype":"session_state_changed",
                    "state":"idle",
                    "uuid":"state\u{1b}[2J",
                    "session_id":"session"
                }),
                EnvelopeClass::System(SystemSubtype::SessionStateChanged),
            ),
            (
                2,
                json!({
                    "type":"prompt_suggestion",
                    "suggestion":"next\u{1b}]52;c;suggestion\u{7}",
                    "uuid":"prompt",
                    "session_id":"session"
                }),
                EnvelopeClass::PromptSuggestion,
            ),
        ] {
            let _effect = app.projection.ingest(RawEnvelope {
                sequence,
                encoded_len: 1,
                value,
                classification: class,
                correlation: None,
            });
        }

        let rendered = render_buffer(&mut app, 120, 24);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(rendered.contains('␛'), "{rendered:?}");

        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::Json,
            "title\u{1b}]8;;https://evil.test\u{7}",
            "body\u{1b}[2J\u{202e}exe",
        ));
        let rendered = render_buffer(&mut app, 120, 24);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(rendered.contains('␛'), "{rendered:?}");
    }

    #[test]
    fn production_preview_overlay_truncates_a_long_line_inside_its_bounds() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::Json,
            "Preview",
            "a".repeat(100),
        ));

        let rendered = render_buffer(&mut app, 40, 16);
        assert!(
            rendered.contains('…'),
            "the visible preview line must end with an ellipsis: {rendered:?}"
        );
        let longest_body_run = rendered
            .split(|character| character != 'a')
            .map(str::len)
            .max()
            .unwrap_or_default();
        assert_eq!(
            longest_body_run, 27,
            "the 34-column modal has a 32-column inner box and the fixed shared chrome applies two columns of padding on each side: 27 cells plus one ellipsis, with no wrapped remainder"
        );
    }

    #[test]
    fn production_context_overlay_paints_product_grid_instead_of_generic_json() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let context = crate::context_visualization::ContextVisualization::from_control_response(
            &crate::context_visualization::minimal_test_control_response(),
        )
        .expect("valid context response");
        app.overlay = Some(crate::tui_app::Overlay::context(context, app.ui_language()));

        let buffer = render_test_buffer(&mut app, 100, 24);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let rendered_compact = rendered.replace(' ', "");
        assert!(rendered_compact.contains("上下文用量"), "{rendered}");
        assert!(rendered_compact.contains("⛁⛶"), "{rendered}");
        assert!(
            rendered_compact.contains("model-from-sdk·20k/100k词元（20%）"),
            "{rendered}"
        );
        assert!(!rendered.contains("\"categories\""), "{rendered}");

        let expected_cyan =
            color_support::quantize_color(Color::Rgb(8, 145, 178), color_support::detect());
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| { cell.symbol() == "⛁" && cell.fg == expected_cyan })
        );
    }

    #[test]
    fn context_overlay_narrow_pixels_remain_visible_and_scroll_to_the_measured_end() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut payload = crate::context_visualization::minimal_test_control_response();
        payload["response"]["memoryFiles"] = json!(
            (0..14)
                .map(|index| json!({
                    "path":format!("memory-{index:02}.md"),
                    "type":"project",
                    "tokens":100
                }))
                .collect::<Vec<_>>()
        );
        let context =
            crate::context_visualization::ContextVisualization::from_control_response(&payload)
                .expect("valid long context response");
        app.overlay = Some(crate::tui_app::Overlay::context(context, app.ui_language()));

        let first = render_test_buffer(&mut app, 56, 18);
        let first_rows = buffer_row_texts(&first);
        let first_pixels = first_rows.join("\n");
        let first_compact = first_pixels.replace(' ', "");
        assert!(first_compact.contains("上下文用量"), "{first_rows:#?}");
        assert!(first_compact.contains("⛁⛶"), "{first_rows:#?}");
        assert!(
            first_compact.contains("model-from-sdk·20k/100k词元（20%"),
            "{first_rows:#?}"
        );
        assert!(!first_pixels.contains("\"categories\""), "{first_rows:#?}");

        let (viewport_height, total_lines) = {
            let overlay = app.overlay.as_ref().expect("context overlay");
            (
                overlay
                    .body_viewport_height
                    .expect("production renderer measured body rows"),
                overlay
                    .context_visualization
                    .as_ref()
                    .expect("typed context projection")
                    .line_count(),
            )
        };
        assert!(total_lines > usize::from(viewport_height));
        app.overlay.as_mut().expect("context overlay").scroll = usize::MAX;

        let last = render_test_buffer(&mut app, 56, 18);
        let last_rows = buffer_row_texts(&last);
        let last_pixels = last_rows.join("\n");
        assert!(last_pixels.contains("memory-13.md"), "{last_rows:#?}");
        assert!(last_pixels.replace(' ', "").contains("上下文用量"));
        assert_eq!(
            app.overlay.as_ref().expect("context overlay").scroll,
            total_lines.saturating_sub(usize::from(viewport_height)),
            "scroll must clamp against the same measured pixel viewport used for painting"
        );
        for row in 0..last.area.height {
            for column in 0..last.area.width {
                let symbol_width =
                    unicode_width::UnicodeWidthStr::width(last[(column, row)].symbol());
                assert!(
                    symbol_width <= usize::from(last.area.width - column),
                    "wide context glyph starts outside the remaining terminal cells at ({column},{row})"
                );
            }
        }
    }

    #[test]
    fn overlay_render_measures_body_rows_and_clamps_before_painting() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let body = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut overlay = crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::BlockViewer,
            "Measured viewer",
            body,
        );
        overlay.scroll = usize::MAX;
        app.overlay = Some(overlay);

        let terminal = Rect::new(0, 0, 40, 16);
        let host_area = centered(terminal, 86, 82);
        let sizing = ModalSizing {
            width_pct: 1.0,
            max_width: host_area.width,
            min_width: host_area.width,
            v_margin: 0,
            ..ModalSizing::large()
        };
        let shortcuts = [
            Shortcut {
                label: "↑/↓ 滚动",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Ctrl-F 全屏",
                clickable: false,
                id: 1,
            },
            Shortcut {
                label: "Esc 关闭",
                clickable: false,
                id: 2,
            },
        ];
        let config = ModalWindowConfig {
            title: "Measured viewer",
            tabs: None,
            shortcuts: &shortcuts,
            sizing,
            fold_info: None,
        };
        let mut expected_buffer = Buffer::empty(host_area);
        let mut expected_state = crabcode_pager_render::modal_window::ModalWindowState::new();
        let expected_body_rows = render_modal_window(
            &mut expected_buffer,
            host_area,
            &mut expected_state,
            &config,
            &CrabCodeTheme::current(),
        )
        .expect("shared modal chrome fits the test viewport")
        .content
        .height;
        let _rendered = render_buffer(&mut app, terminal.width, terminal.height);
        let overlay = app.overlay.as_ref().expect("block viewer");
        assert_eq!(
            overlay.body_viewport_height,
            Some(expected_body_rows),
            "the renderer must publish the actual post-chrome body height"
        );
        assert_eq!(
            overlay.scroll,
            20_usize.saturating_sub(usize::from(expected_body_rows)),
            "the same measured layout must clamp the offset before painting"
        );
    }

    #[test]
    fn production_resize_recomputes_overlay_geometry_and_scroll_bounds() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let body = (0..60)
            .map(|index| format!("resize line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::BlockViewer,
            "Resize viewer",
            body,
        ));

        let _wide = render_buffer(&mut app, 100, 30);
        let (wide_popup, wide_body_height) = {
            let overlay = app.overlay.as_ref().expect("wide overlay");
            (
                overlay.window.popup_area.expect("wide popup geometry"),
                overlay
                    .body_viewport_height
                    .expect("wide body viewport height"),
            )
        };
        app.overlay.as_mut().expect("resize overlay").scroll = usize::MAX;

        let _narrow = render_buffer(&mut app, 40, 16);
        let overlay = app.overlay.as_ref().expect("narrow overlay");
        let narrow_popup = overlay.window.popup_area.expect("narrow popup geometry");
        let narrow_body_height = overlay
            .body_viewport_height
            .expect("narrow body viewport height");

        assert_ne!(narrow_popup, wide_popup);
        assert!(narrow_popup.right() <= 40);
        assert!(narrow_popup.bottom() <= 16);
        assert_ne!(narrow_body_height, wide_body_height);
        assert_eq!(
            overlay.scroll,
            60_usize.saturating_sub(usize::from(narrow_body_height)),
            "resize must clamp against the newly painted overlay body, not stale wide geometry"
        );
    }

    #[test]
    fn production_history_search_paints_fixed_dialog_rows_preview_and_search_caret() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        for prompt in ["deploy staging", "debug parser\nwith details"] {
            app.composer.set_text(prompt);
            app.composer.set_cursor(prompt.len());
            let actions = app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )));
            let [action] = actions.as_slice() else {
                panic!("one prompt action expected")
            };
            app.action_succeeded(action);
        }
        app.composer.set_text("");
        app.composer.set_cursor(0);
        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )))
            .is_empty()
        );

        let rendered = render_buffer(&mut app, 120, 30);
        let rendered_compact = rendered.replace(' ', "");
        assert!(rendered_compact.contains("搜索历史提示词"), "{rendered}");
        assert!(rendered_compact.contains("↑/↓导航"), "{rendered}");
        assert!(rendered.contains("debug parser"), "{rendered}");
        assert!(rendered.contains("with details"), "{rendered}");
        assert!(
            app.history_search
                .as_ref()
                .is_some_and(|search| search.lifecycle().query().is_empty())
        );
    }

    #[test]
    fn production_quick_open_paints_direction_up_results_preview_and_query_caret() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src")).expect("mkdir");
        std::fs::write(
            workspace.path().join("src/main.rs"),
            "fn main() {\n    println!(\"ready\");\n}\n",
        )
        .expect("write");
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.configure_pre_initialize_setup(workspace.path().to_path_buf())
            .expect("configure workspace");
        app.release_startup_barrier_for_test();
        app.open_workspace_search_for_test(WorkspaceSearchKind::QuickOpen);
        for character in "main".chars() {
            assert!(
                app.handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )))
                .is_empty()
            );
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.has_deferred_input_work() && std::time::Instant::now() < deadline {
            let _ = app.poll_deferred_input_with_progress();
            std::thread::yield_now();
        }
        assert!(!app.has_deferred_input_work());

        let rendered = render_buffer(&mut app, 130, 30);
        let rendered_compact = rendered.replace(' ', "");
        assert!(rendered_compact.contains("快速打开"), "{rendered}");
        assert!(rendered_compact.contains("↑/↓导航"), "{rendered}");
        assert!(rendered.contains("src/main.rs"), "{rendered}");
        assert!(rendered.contains("fn main()"), "{rendered}");
        assert!(rendered_compact.contains("Tab引用"), "{rendered}");
        assert!(rendered.contains("main"), "{rendered}");
    }

    #[test]
    fn workspace_search_middle_truncation_keeps_directory_context_and_filename() {
        assert_eq!(
            truncate_workspace_path_middle("src/components/deep/folder/PromptInput.tsx", 30),
            "src/component…/PromptInput.tsx"
        );
        assert_eq!(
            truncate_workspace_path_middle("目录/很深/Prompt输入.tsx", 16),
            "…/Prompt输入.tsx"
        );
        assert_eq!(
            truncate_workspace_path_middle("directory/very-long-filename.rs", 12),
            "…filename.rs"
        );
        for width in 1..=30 {
            assert!(
                truncate_workspace_path_middle("目录/components/👩🏽‍💻-prompt/PromptInput.tsx", width)
                    .width()
                    <= width
            );
        }
    }

    #[test]
    fn production_help_tabs_publish_click_geometry_and_switch_the_rendered_document() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let mut app = TuiApp::new(
            &json!({
                "commands":[{
                    "name":"project-workflow",
                    "description":"Project-local workflow",
                    "argumentHint":""
                }]
            }),
            InitialSessionRequest::New,
            None,
        );
        app.release_startup_barrier_for_test();
        app.composer.set_text("/help");
        app.composer.set_cursor(5);
        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .is_empty()
        );
        let general = render_buffer(&mut app, 100, 28);
        let general_compact = general.replace(' ', "");
        assert!(general_compact.contains("常规"), "{general}");
        assert!(general_compact.contains("命令"), "{general}");
        assert!(general_compact.contains("自定义命令"), "{general}");
        assert!(general_compact.contains("快捷键"), "{general}");
        let window = &app.overlay.as_ref().expect("painted help overlay").window;
        assert_eq!(
            window.shortcut_hits.len(),
            3,
            "the production overlay must publish shared footer geometry"
        );
        assert!(
            window.shortcut_hits.iter().all(|hit| !hit.clickable),
            "product-only shortcut hints cannot widen the modal action surface"
        );
        assert_eq!(
            window.close_button_rect.map(|rect| rect.width),
            Some(5),
            "the production overlay must use the shared close-button chrome"
        );

        let commands_tab = app
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.window.tab_rects.get(1))
            .copied()
            .flatten()
            .expect("painted commands tab");
        assert!(
            app.handle_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: commands_tab.x,
                row: commands_tab.y,
                modifiers: KeyModifiers::NONE,
            }))
            .is_empty()
        );
        assert_eq!(
            app.overlay
                .as_ref()
                .map(|overlay| overlay.window.active_tab),
            Some(1)
        );
        let commands = render_buffer(&mut app, 100, 28);
        assert!(
            commands.replace(' ', "").contains("浏览默认命令："),
            "{commands}"
        );
    }

    #[test]
    fn production_overlay_shared_chrome_close_hitbox_closes_existing_product_surface() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::Json,
            "JSON",
            "{\"existing\":\"product payload\"}",
        ));

        let rendered = render_buffer(&mut app, 80, 24);
        assert!(rendered.contains("Ctrl-F"), "{rendered}");
        assert!(rendered.replace(' ', "").contains("Esc关闭"), "{rendered}");
        let close = app
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.window.close_button_rect)
            .expect("shared chrome close hitbox");
        assert_eq!(close.width, 5);
        assert!(
            app.handle_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: close.x + close.width / 2,
                row: close.y,
                modifiers: KeyModifiers::NONE,
            }))
            .is_empty()
        );
        assert!(
            app.overlay.is_none(),
            "shared chrome outcomes must close the existing CrabCode overlay only"
        );
    }

    #[test]
    fn production_overlay_fullscreen_escape_and_mouse_close_share_painted_geometry() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };

        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.overlay = Some(crate::tui_app::Overlay::new(
            crate::tui_app::OverlayKind::Help,
            "Help",
            "body",
        ));

        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            )))
            .is_empty()
        );
        let _rendered = render_buffer(&mut app, 40, 16);
        let overlay = app.overlay.as_ref().expect("fullscreen overlay");
        assert!(overlay.lifecycle.fullscreen);
        assert_eq!(overlay.window.popup_area, Some(Rect::new(0, 0, 40, 16)));

        assert!(
            app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE,)))
                .is_empty()
        );
        assert!(
            app.overlay
                .as_ref()
                .is_some_and(|overlay| !overlay.lifecycle.fullscreen),
            "the first Escape exits only the fullscreen nesting level"
        );
        let _rendered = render_buffer(&mut app, 40, 16);
        let popup = app
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.window.popup_area)
            .expect("centered popup geometry");
        assert_eq!(popup, centered(Rect::new(0, 0, 40, 16), 86, 82));

        let outside = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.handle_event(outside).is_empty());
        assert!(app.overlay.is_none());
    }

    #[test]
    fn tiny_supported_viewport_keeps_one_live_transcript_row_and_composer() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.projection.ingest(RawEnvelope {
            sequence: 0,
            encoded_len: 1,
            value: json!({
                "type":"assistant",
                "uuid":"a",
                "session_id":"s",
                "parent_tool_use_id":null,
                "message":{"id":"m","content":[{"type":"text","text":"visible"}]}
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        });
        let rendered = render_buffer(&mut app, 20, 8);
        assert!(rendered.contains("CrabCode"));
        assert!(rendered.contains("visible"), "{rendered}");
        assert!(
            rendered.replace(' ', "").contains("输入·Enter发送"),
            "{rendered}"
        );
    }

    #[test]
    fn scrollback_user_prompt_wraps_across_visible_rows() {
        let mut item = projected_item(
            "long-user-prompt",
            "alpha beta gamma delta epsilon zeta eta theta",
            1,
        );
        item.kind = ProjectedKind::User;
        item.title = "You".to_string();
        let rendered = render_projected_item(&item, 14, CrabCodeTheme::NIGHT, &[]);
        let body = &rendered.lines[1..rendered.lines.len().saturating_sub(1)];
        assert!(
            body.len() >= 4,
            "a long prompt must occupy multiple visible rows: {body:?}"
        );
        assert!(
            body.iter().all(|line| line.width() <= 14),
            "wrapped prompt rows must stay inside the content width: {body:?}"
        );
    }

    #[test]
    fn user_prompt_title_follows_selected_interface_language() {
        let mut item = projected_item("localized-user-prompt", "hello", 1);
        item.kind = ProjectedKind::User;
        item.title = "You".to_string();

        let zh = render_projected_item(&item, 40, CrabCodeTheme::NIGHT, &[]);
        let zh_title = zh.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(zh_title, "◆ 你");

        let mut markdown_stream = CrabCodeMarkdownStream::default();
        let en = render_projected_item_with_state_context(
            &item,
            40,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut markdown_stream,
            ProjectedItemRenderContext {
                ui_language: UiLanguage::EnUs,
                ..ProjectedItemRenderContext::default()
            },
        );
        let en_title = en.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(en_title, "◆ You");
    }

    #[test]
    fn scrollback_prompt_fold_cycle_collapsed_to_expanded_on_live_key_path() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.projection
            .project_wire_fixtures(
                &[json!({
                    "type":"user",
                    "uuid":"foldable-prompt",
                    "session_id":"session",
                    "parent_tool_use_id":null,
                    "message":{"content":"one\ntwo\nthree\nfour"}
                })],
                1,
            )
            .expect("history projection");

        let collapsed = render_buffer(&mut app, 80, 20);
        let prompt_key = app.projection.items()[0].key.clone();
        assert_eq!(
            app.transcript_item_interactions[&prompt_key].mode,
            TranscriptDisplayMode::Collapsed
        );
        assert!(
            !collapsed.contains("four"),
            "collapsed prompt body must not be painted: {collapsed}"
        );

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('e'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let expanded = render_buffer(&mut app, 80, 20);

        assert_eq!(
            app.transcript_item_interactions[&prompt_key].mode,
            TranscriptDisplayMode::Expanded
        );
        assert!(
            expanded.contains("four"),
            "expanded prompt body must be painted: {expanded}"
        );
    }

    #[test]
    fn response_navigation_walks_rendered_anchor_offsets_and_skips_tool_only_turn() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let tall = |prefix: &str| {
            (0..30)
                .map(|line| format!("{prefix}-{line}"))
                .collect::<Vec<_>>()
                .join("  \n")
        };
        app.projection
            .project_wire_fixtures(
                &[
                    json!({
                        "type":"user","uuid":"q1","session_id":"session",
                        "parent_tool_use_id":null,"message":{"content":"Q1"}
                    }),
                    json!({
                        "type":"assistant","uuid":"a1","session_id":"session",
                        "parent_tool_use_id":null,
                        "message":{"id":"a1","content":[{"type":"text","text":tall("A1")}]}
                    }),
                    json!({
                        "type":"user","uuid":"q2","session_id":"session",
                        "parent_tool_use_id":null,"message":{"content":"Q2"}
                    }),
                    json!({
                        "type":"assistant","uuid":"tool","session_id":"session",
                        "parent_tool_use_id":null,
                        "message":{"id":"tool","content":[{
                            "type":"tool_use","id":"tool-2","name":"Bash",
                            "input":{"command":"printf tool-only"}
                        }]}
                    }),
                    json!({
                        "type":"user","uuid":"tool-result","session_id":"session",
                        "parent_tool_use_id":"tool-2",
                        "message":{"content":[{
                            "type":"tool_result","tool_use_id":"tool-2","content":"tool-only"
                        }]}
                    }),
                    json!({
                        "type":"user","uuid":"q3","session_id":"session",
                        "parent_tool_use_id":null,"message":{"content":"Q3"}
                    }),
                    json!({
                        "type":"assistant","uuid":"a3","session_id":"session",
                        "parent_tool_use_id":null,
                        "message":{"id":"a3","content":[{"type":"text","text":tall("A3")}]}
                    }),
                ],
                1,
            )
            .expect("history projection");
        let first_response = app
            .projection
            .items()
            .iter()
            .find(|item| item.text.starts_with("A1-0"))
            .expect("first response")
            .key
            .clone();
        let last_response = app
            .projection
            .items()
            .iter()
            .find(|item| item.text.starts_with("A3-0"))
            .expect("last response")
            .key
            .clone();

        let _ = render_buffer(&mut app, 80, 20);
        let first_offset = app
            .transcript_layout
            .item_bounds(&first_response)
            .expect("first response layout")
            .0;
        let last_offset = app
            .transcript_layout
            .item_bounds(&last_response)
            .expect("last response layout")
            .0;
        assert!(app.scroll.offset() > last_offset, "setup must follow below");

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let previous_response = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('K'),
            crossterm::event::KeyModifiers::SHIFT,
        ));
        app.handle_event(previous_response.clone());
        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(last_response.as_str())
        );
        assert_eq!(app.scroll.offset(), last_offset);
        assert!(!app.scroll.is_following());

        app.handle_event(previous_response.clone());
        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(first_response.as_str())
        );
        assert_eq!(app.scroll.offset(), first_offset);

        app.handle_event(previous_response);
        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(first_response.as_str()),
            "there is no response anchor above the first response"
        );
        assert_eq!(app.scroll.offset(), first_offset);

        app.handle_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('J'),
                crossterm::event::KeyModifiers::SHIFT,
            ),
        ));
        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(last_response.as_str())
        );
        assert_eq!(app.scroll.offset(), last_offset);
    }

    #[test]
    fn transcript_scrollbar_thumb_tracks_the_scroll_offset() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let _effect = app.projection.ingest(RawEnvelope {
            sequence: 1,
            encoded_len: 1,
            value: json!({
                "type":"assistant",
                "uuid":"assistant-1",
                "session_id":"session-1",
                "parent_tool_use_id":null,
                "message":{
                    "id":"message-1",
                    "content":[{"type":"text","text":(0..80).map(|line| format!("line-{line}")).collect::<Vec<_>>().join("\n")}]
                }
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        });

        let bottom = render_test_buffer(&mut app, 40, 18);
        let bottom_thumb = scrollbar_thumb_rows(&bottom);
        assert!(!bottom_thumb.is_empty(), "overflow must paint a thumb");

        app.scroll
            .set_offset(0, app.transcript_line_count, app.transcript_viewport_height);
        let top = render_test_buffer(&mut app, 40, 18);
        let top_thumb = scrollbar_thumb_rows(&top);
        assert!(!top_thumb.is_empty(), "overflow must paint a thumb");
        assert_ne!(
            top_thumb, bottom_thumb,
            "the thumb cells must move when the scroll offset changes"
        );
        assert!(
            top_thumb.iter().min() < bottom_thumb.iter().min(),
            "top offset must paint above the bottom offset"
        );
    }

    #[test]
    fn transcript_scrollbar_paints_no_thumb_without_overflow() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let buffer = render_test_buffer(&mut app, 40, 18);
        assert!(
            scrollbar_thumb_rows(&buffer).is_empty(),
            "content that fits the viewport must not paint a scrollbar thumb"
        );
    }

    #[test]
    fn inline_math_is_recognized_on_the_production_transcript_path() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "mass is $E=mc^2$ ok\n",
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
        );
        assert_eq!(line_text(&lines[0].line), "mass is E=mc² ok");
        let math = lines[0]
            .line
            .spans
            .iter()
            .find(|span| span.content == "E=mc²")
            .expect("inline math span");
        assert!(math.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn display_math_is_recognized_on_the_production_transcript_path() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "$$\\int x dx$$\n",
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
        );
        assert_eq!(lines.len(), 2);
        let rendered = line_text(&lines[0].line);
        assert!(rendered.contains("int x dx"), "{rendered:?}");
        assert!(!rendered.contains('$'), "{rendered:?}");
        assert!(
            lines[0].line.style.add_modifier.contains(Modifier::ITALIC),
            "display math must use the production math presentation"
        );
        assert_eq!(line_text(&lines[1].line), "");
    }

    #[test]
    fn production_mermaid_flowchart_fence_renders_terminal_box_art() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "```mermaid\ngraph TD\n A[Start] --> B[End]\n```\n",
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
        );
        let rendered = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Start"), "{rendered}");
        assert!(rendered.contains("End"), "{rendered}");
        assert!(rendered.contains('┌'), "{rendered}");
        assert!(rendered.contains('▼'), "{rendered}");
        assert!(!rendered.contains("graph TD"), "{rendered}");
    }

    #[test]
    fn completed_assistant_mermaid_adds_exact_lazy_actions_after_terminal_art() {
        let source = "```mermaid\ngraph TD\nA[Start] --> B[End]\n```\n";
        let item = projected_item("mermaid-actions", source, 1);
        let mut markdown_stream = CrabCodeMarkdownStream::default();
        let rendered = render_projected_item_with_state_mode(
            &item,
            90,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut markdown_stream,
            false,
        );
        let text = rendered.lines.iter().map(line_text).collect::<Vec<_>>();
        let affordance_row = text
            .iter()
            .position(|line| line.contains("◇ mermaid"))
            .expect("finished assistant diagram has an affordance row");
        assert!(
            text[..affordance_row]
                .iter()
                .any(|line| line.contains("Start"))
        );
        assert!(
            text[..affordance_row]
                .iter()
                .any(|line| line.contains("End"))
        );
        assert_eq!(
            text[affordance_row].trim_end(),
            "  ◇ mermaid   [Open Image]   [Copy Image Path]   [Copy Source]"
        );
        assert!(
            text.get(affordance_row.saturating_sub(1))
                .is_some_and(|line| line.contains('└')),
            "affordance follows the diagram's last rendered row: {text:?}"
        );

        let actions = rendered
            .link_groups
            .iter()
            .filter_map(|group| match &group.target {
                LinkTarget::Mermaid { action, source } => {
                    Some((*action, source.as_ref(), group.primary()?.row))
                }
                LinkTarget::Url(_) | LinkTarget::File(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                (
                    MermaidAffordanceAction::Open,
                    "graph TD\nA[Start] --> B[End]\n",
                    affordance_row
                ),
                (
                    MermaidAffordanceAction::CopyPath,
                    "graph TD\nA[Start] --> B[End]\n",
                    affordance_row
                ),
                (
                    MermaidAffordanceAction::CopySource,
                    "graph TD\nA[Start] --> B[End]\n",
                    affordance_row
                ),
            ]
        );
    }

    #[test]
    fn mermaid_actions_do_not_wrap_and_static_or_streaming_output_has_no_dead_row() {
        let source = "```mermaid\ngraph TD\nA-->B\n```\n";
        let item = projected_item("mermaid-width", source, 1);
        let mut narrow_stream = CrabCodeMarkdownStream::default();
        let narrow = render_projected_item_with_state_mode(
            &item,
            30,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut narrow_stream,
            false,
        );
        let narrow_text = narrow.lines.iter().map(line_text).collect::<Vec<_>>();
        let row = narrow_text
            .iter()
            .find(|line| line.contains("◇ mermaid"))
            .expect("label fits");
        assert!(row.contains("[Open Image]"), "{narrow_text:?}");
        assert!(!row.contains("[Copy Image Path]"), "{narrow_text:?}");
        assert!(!row.contains("[Copy Source]"), "{narrow_text:?}");
        assert_eq!(
            narrow
                .link_groups
                .iter()
                .filter(|group| matches!(&group.target, LinkTarget::Mermaid { .. }))
                .count(),
            1
        );

        let mut static_stream = CrabCodeMarkdownStream::default();
        let static_render = render_projected_item_with_state_mode(
            &item,
            90,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut static_stream,
            true,
        );
        let static_text = static_render
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(static_text.contains('┌'), "{static_text}");
        assert!(!static_text.contains("[Open Image]"), "{static_text}");
        assert!(
            static_render
                .link_groups
                .iter()
                .all(|group| !matches!(&group.target, LinkTarget::Mermaid { .. }))
        );
        let committed = terminal_lines_for_projected_item(&item, 90)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!committed.contains("[Open Image]"), "{committed}");

        let mut streaming_item = item;
        streaming_item.streaming = true;
        let mut streaming = CrabCodeMarkdownStream::default();
        let streaming_render = render_projected_item_with_state_mode(
            &streaming_item,
            90,
            CrabCodeTheme::NIGHT,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut streaming,
            false,
        );
        assert!(
            streaming_render
                .lines
                .iter()
                .map(line_text)
                .all(|line| !line.contains("[Open Image]"))
        );
    }

    #[test]
    fn visible_mermaid_buttons_hover_click_render_and_complete_without_inline_png() {
        let source = "flowchart LR\nA[CrabCode]-->B[TUI]\n";
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        app.handle_runtime_event(crate::sdk_runtime::RuntimeEvent::Envelope(RawEnvelope {
            sequence: 1,
            encoded_len: 1,
            value: json!({
                "type":"assistant",
                "uuid":"assistant-mermaid",
                "session_id":"session",
                "parent_tool_use_id":null,
                "message":{
                    "id":"message-mermaid",
                    "content":[{
                        "type":"text",
                        "text":format!("```mermaid\n{source}```\n")
                    }]
                }
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        }));

        let first = render_buffer(&mut app, 140, 40);
        assert!(first.contains("◇ mermaid"), "{first}");
        assert!(first.contains("[Open Image]"), "{first}");
        assert!(!first.contains("\u{1b}_G"), "PNG must never be inline");
        assert_eq!(app.mermaid_hitboxes.len(), 3);
        let hitbox = |action| {
            app.mermaid_hitboxes
                .iter()
                .find(|(_, candidate, _)| *candidate == action)
                .map(|(area, _, source)| (*area, Arc::clone(source)))
                .expect("action hitbox")
        };
        let (open_area, open_source) = hitbox(MermaidAffordanceAction::Open);
        let (copy_path_area, _) = hitbox(MermaidAffordanceAction::CopyPath);
        let (copy_source_area, copy_source) = hitbox(MermaidAffordanceAction::CopySource);
        assert_eq!(open_source.as_ref(), source);
        assert_eq!(copy_source.as_ref(), source);

        let _ = app.handle_event(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: open_area.x,
                row: open_area.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        let hovered = render_test_buffer(&mut app, 140, 40);
        let hover_cell = &hovered[(open_area.x, open_area.y)];
        assert!(hover_cell.modifier.contains(Modifier::BOLD));
        assert!(hover_cell.modifier.contains(Modifier::UNDERLINED));

        let _ = app.handle_event(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: copy_source_area.x,
                row: copy_source_area.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        assert_eq!(app.status, "已复制 Mermaid 源码");

        let cache = tempfile::tempdir().expect("cache root");
        crate::crabcode_mermaid_worker::with_test_cache_root(cache.path(), || {
            let _ = app.handle_event(crossterm::event::Event::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: open_area.x,
                    row: open_area.y,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
            ));
            let busy = render_buffer(&mut app, 140, 40);
            assert!(busy.contains("rendering diagram…"), "{busy}");

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let png_path = loop {
                let _ = app.poll_deferred_input();
                if let Some(LinkTarget::File(path)) = app.take_link_open_request() {
                    break path;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "lazy Mermaid render did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let png = std::fs::read(png_path.as_ref()).expect("rendered PNG");
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            image::load_from_memory(&png).expect("decodable PNG");

            let _ = app.handle_event(crossterm::event::Event::Mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: copy_path_area.x,
                    row: copy_path_area.y,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
            ));
            assert!(app.status.starts_with("已复制 Mermaid 图片路径："));
        });
    }

    #[test]
    fn autolink_and_reference_link_both_reach_production_link_rendering() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "<https://x.com>\n[a][b]\n\n[b]: https://x.com",
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
        );
        let text = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();
        assert_eq!(text[0], "https://x.com");
        assert_eq!(text[1], "a (https://x.com)");
        assert!(
            text.iter().all(|line| !line.contains("[b]:")),
            "reference definitions are parser metadata, not visible transcript text: {text:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.line.spans)
                .filter(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
                .count()
                >= 2,
            "both pinned link forms must be rendered as links"
        );
        assert_eq!(
            lines[0].links.len(),
            1,
            "the autolink must produce one parser-owned logical target"
        );
        assert_eq!(
            lines[1].links.len(),
            1,
            "the resolved reference must produce one parser-owned logical target"
        );
    }

    #[test]
    fn streaming_finish_matches_a_fresh_full_production_render() {
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let chunks = ["# Heading\n\n", "Some **bold** text.\n\n", "> Quote\n\n"];
        let source = chunks.concat();
        let fresh = pinned_markdown_one_shot(&source, 80, &[]);
        let mut stream = CrabCodeMarkdownStream::default();
        for chunk in chunks {
            let _ = stream.push_and_render(chunk, style, theme, 80);
        }
        let finished = stream.finish(style, theme, 80).clone();

        assert_markdown_render_exact(&finished, &fresh);
        assert_eq!(stream.frozen_source_bytes, source.len());
    }

    #[test]
    fn production_stream_matches_pinned_renderer_for_full_markdown_denominator() {
        const SECTIONS: &[&str] = &[
            "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n\n",
            "> quote **strong** and *emphasis*\n>\n> - nested\n\n",
            "- [x] checked\n- [ ] unchecked\n1. ordered\n\n",
            "soft\nbreak  \nhard break\n\n",
            "Entities: &amp; &lt; &gt;. Inline \\(\\alpha + x^2\\).\n\n",
            "$$\\int_0^1 x^2 dx$$\n\n",
            "| Name | Status |\n| --- | :---: |\n| 世界 | `ready` |\n\n",
            "[link\ntext](https://example.com/path) and <https://example.org/a>.\n\n",
            "```rust\nfn main() {\n\tprintln!(\"hello\");\n}\n```\n\n",
            "```mermaid\nflowchart TD\nA --> B\n```\n\n",
            "```mermaid\nclassDiagram\nA <|-- B\n```\n\n",
            "```mermaid\nstateDiagram-v2\n[*] --> Ready\n```\n\n",
            "```mermaid\nsequenceDiagram\nAlice->>Bob: Hello\n```\n\n",
            "```mermaid\nerDiagram\nCUSTOMER ||--o{ ORDER : places\n```\n",
        ];
        let source = SECTIONS.concat();
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut stream = CrabCodeMarkdownStream::default();
        for section in SECTIONS {
            let _ = stream.push_and_render(section, style, theme, 52);
        }
        let actual = stream.finish(style, theme, 52).clone();
        let expected = pinned_markdown_one_shot(&source, 52, &[]);

        assert_markdown_render_exact(&actual, &expected);
        assert_eq!(
            actual
                .code_blocks
                .iter()
                .map(|block| block.info.as_str())
                .collect::<Vec<_>>(),
            [
                "rust", "mermaid", "mermaid", "mermaid", "mermaid", "mermaid"
            ]
        );
        assert_eq!(stream.raw_source, source);
        assert_eq!(stream.expanded_source.matches('\t').count(), 0);
        assert!(stream.expanded_source.contains("    println!"));
        assert_eq!(stream.normalized_source, stream.renderer.source());
    }

    #[test]
    fn production_projected_item_uses_pinned_markdown_and_preserves_link_ids() {
        let source = "foo [link\ntext](https://example.com) bar";
        let item = projected_item("markdown", source, 1);
        let theme = CrabCodeTheme::NIGHT;
        let mut stream = CrabCodeMarkdownStream::default();
        let rendered = render_projected_item_with_state(
            &item,
            80,
            theme,
            &[],
            TranscriptDisplayMode::Expanded,
            false,
            &mut stream,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
        );
        let expected = pinned_markdown_one_shot(source, 78, &[]);
        assert_markdown_render_exact(&stream.output, &expected);

        let parser_group = rendered
            .link_groups
            .iter()
            .find(|group| {
                group
                    .fragments
                    .iter()
                    .map(|fragment| ascii_fragment_text(&rendered.lines, fragment))
                    .collect::<String>()
                    == "link text"
            })
            .expect("parser link fragments remain one production navigation group");
        assert!(
            parser_group.fragments.len() >= 2,
            "the renderer-assigned logical ID must merge split link fragments: {parser_group:?}"
        );
        assert_eq!(
            parser_group.target,
            LinkTarget::Url(Arc::from("https://example.com"))
        );
    }

    #[test]
    fn source_replacement_width_reset_and_finish_match_pinned_renderer() {
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut stream = CrabCodeMarkdownStream::default();
        let _ = stream.push_and_render("# discarded\n\nold", style, theme, 80);
        assert!(stream.frozen_source_bytes > 0);

        let replacement = "| A | B |\n|---|---|\n| long long long | value |\n";
        let _ = stream.synchronize(
            replacement,
            true,
            false,
            style,
            theme,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            24,
            &[],
        );
        assert_eq!(stream.last_render_source_start, 0);
        assert_eq!(stream.raw_source, replacement);
        assert!(!stream.normalized_source.contains("discarded"));
        let actual = stream.finish(style, theme, 24).clone();
        let expected = pinned_markdown_one_shot(replacement, 24, &[]);
        assert_markdown_render_exact(&actual, &expected);
    }

    #[test]
    fn nested_mermaid_span_keeps_clean_body_and_raw_source_range() {
        let source = "> ```mermaid\n> flowchart TD\n>   A --> B\n> ```\n";
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut stream = CrabCodeMarkdownStream::default();
        let actual = stream
            .synchronize(
                source,
                false,
                false,
                style,
                theme,
                Some(CrabCodeSyntaxTheme::MonokaiExtended),
                60,
                &[],
            )
            .clone();
        let expected = pinned_markdown_one_shot(source, 60, &[]);
        assert_markdown_render_exact(&actual, &expected);
        assert_eq!(actual.code_blocks.len(), 1);
        let block = &actual.code_blocks[0];
        assert_eq!(block.info, "mermaid");
        assert_eq!(block.body, "flowchart TD\n  A --> B\n");
        assert_eq!(
            &stream.normalized_source[block.source_byte_range.clone()],
            "flowchart TD\n>   A --> B\n"
        );
        assert!(
            !actual.lines[block.output_line_range.clone()].is_empty(),
            "the exact pre-wrap body range remains available to Mermaid affordances"
        );
    }

    #[test]
    fn streaming_freezes_a_stable_prefix_and_reparses_only_the_tail() {
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut stream = CrabCodeMarkdownStream::default();
        let _ = stream.push_and_render("# Heading\n\n", style, theme, 80);
        let frozen_bytes = stream.frozen_source_bytes;
        let frozen_lines = stream
            .frozen
            .lines
            .iter()
            .map(|line| line.line.clone())
            .collect::<Vec<_>>();
        assert!(frozen_bytes > 0);
        assert!(!frozen_lines.is_empty());

        let _ = stream.push_and_render("unstable tail", style, theme, 80);
        assert_eq!(stream.last_render_source_start, frozen_bytes);
        assert_eq!(
            stream.output.lines[..frozen_lines.len()]
                .iter()
                .map(|line| line.line.clone())
                .collect::<Vec<_>>(),
            frozen_lines
        );
    }

    #[test]
    fn rgb_syntax_colors_downgrade_to_the_pinned_ansi256_palette() {
        use crate::crabcode_markdown::{TerminalColorLevel, adapt_color};

        assert_eq!(
            adapt_color(Color::Rgb(255, 0, 0), TerminalColorLevel::Ansi256),
            Some(Color::Indexed(196))
        );
        assert_eq!(
            adapt_color(Color::Rgb(0, 255, 0), TerminalColorLevel::Ansi256),
            Some(Color::Indexed(46))
        );
        assert_eq!(
            adapt_color(Color::Rgb(0, 0, 255), TerminalColorLevel::Ansi256),
            Some(Color::Indexed(21))
        );
    }

    #[test]
    fn inline_latex_superscripts_render_as_unicode_in_the_transcript() {
        let theme = CrabCodeTheme::NIGHT;
        let cases = [
            ("$E = mc^2$", "E = mc²"),
            ("$x^{10}$", "x¹⁰"),
            ("$e^{-x}$", "e⁻ˣ"),
            ("$x^T$", "xᵀ"),
        ];
        for (source, expected) in cases {
            let lines = markdown_lines(
                source,
                Style::default().fg(theme.markdown_text),
                theme,
                80,
                &[],
            );
            assert_eq!(line_text(&lines[0].line), expected, "source={source:?}");
        }
    }

    #[test]
    fn streamed_latex_delimiters_match_oneshot_normalization_at_every_split() {
        const RICH_DOC: &str = concat!(
            "Inline \\(a+b\\), dollar $c+d$, display \\[e=mc^2\\].\n\n",
            "Padded \\( x + y \\) and \\( \\alpha + \\beta \\) spans.\n\n",
            "| Col | Math |\n|---|---|\n| x | \\(\\alpha\\) | $\\beta$ |\n\n",
            "Code `\\(not math\\)` stays raw.\n\n",
            "```latex\n\\(also not\\)\n\\[block\\]\n```\n\n",
            "Env \\begin{equation} x=1 \\end{equation} done.\n",
            "Escaped \\\\(literal\\\\), price $5 and $10.\n",
            "List:\n- item \\(p\\to q\\)\n- plain\n\n",
            "> quote \\[E=mc^2\\]\n\n",
            "## Heading \\(h=x^3\\)\n",
        );
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut one_shot = crabcode_markdown_renderer::StreamingMarkdownRenderer::new(
            crabcode_markdown_style(theme),
            true,
        );
        one_shot.push(RICH_DOC);
        one_shot.finish(Some(crabcode_markdown_syntax_highlighter(
            CrabCodeSyntaxTheme::MonokaiExtended,
        )));
        let expected = one_shot.source().to_string();
        for split in 0..=RICH_DOC.len() {
            if !RICH_DOC.is_char_boundary(split) {
                continue;
            }
            let mut stream = CrabCodeMarkdownStream::default();
            let _ = stream.push_and_render(&RICH_DOC[..split], style, theme, 80);
            let _ = stream.push_and_render(&RICH_DOC[split..], style, theme, 80);
            let _ = stream.finish(style, theme, 80);
            assert_eq!(
                stream.normalized_source, expected,
                "normalization diverged at byte split {split}"
            );
        }
    }

    #[test]
    fn open_code_incremental_highlight_matches_a_fresh_full_highlight() {
        let full = "foo: 1\nbar:\n  - a\n  - b\nbaz: true\n";
        let mut incremental = crate::crabcode_markdown::CrabCodeOpenCodeHighlighter::default();
        for end in 1..=full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            assert_eq!(
                incremental.highlight("yaml", 0, &full[..end]),
                crate::crabcode_markdown::highlight_code_batch("yaml", &full[..end]),
                "prefix length {end}"
            );
        }
    }

    #[test]
    fn closed_fence_reports_body_output_and_exact_source_byte_range() {
        let source = "```text\nflowchart TD\n  A --> B\n```\n";
        let theme = CrabCodeTheme::NIGHT;
        let mut highlighter = crate::crabcode_markdown::CrabCodeOpenCodeHighlighter::default();
        let rendered = markdown_render_full(
            source,
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
            0,
            &mut highlighter,
        );
        assert_eq!(rendered.code_blocks.len(), 1);
        let block = &rendered.code_blocks[0];
        assert_eq!(block.info, "text");
        assert_eq!(block.body, "flowchart TD\n  A --> B\n");
        assert_eq!(
            &source[block.source_byte_range.clone()],
            "flowchart TD\n  A --> B\n"
        );
        assert_eq!(
            rendered.lines[block.output_line_range.clone()]
                .iter()
                .map(|line| line_text(&line.line).trim_start_matches("│ ").to_string())
                .collect::<Vec<_>>(),
            ["flowchart TD", "  A --> B"]
        );
    }

    #[test]
    fn markdown_blocks_preserve_structure_styles_and_diff_semantics() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "# Heading\n\
             > quoted **strong**\n\
             - [x] shipped\n\
             2. ordered\n\
             [docs](https://example.test/a)\n\
             ```diff\n\
             -old\n\
             +new\n\
             @@ -1 +1 @@\n\
             ```",
            Style::default().fg(theme.markdown_text),
            theme,
            60,
            &[],
        );
        let plain = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();
        assert!(plain.iter().any(|line| line == "Heading"), "{plain:?}");
        assert!(
            plain.iter().any(|line| line.contains("▎ quoted strong")),
            "{plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.contains("✓ shipped")),
            "{plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|line| line.contains("docs (https://example.test/a)")),
            "{plain:?}"
        );
        assert!(!plain.iter().any(|line| line.contains("```")), "{plain:?}");
        let deletion = lines
            .iter()
            .flat_map(|line| &line.line.spans)
            .find(|span| span.content == "-old")
            .expect("diff deletion");
        assert_eq!(deletion.style.fg, Some(theme.accent_error));
        let addition = lines
            .iter()
            .flat_map(|line| &line.line.spans)
            .find(|span| span.content == "+new")
            .expect("diff addition");
        assert_eq!(addition.style.fg, Some(theme.accent_success));
        assert!(lines.iter().flat_map(|line| &line.line.spans).any(|span| {
            span.content == "strong" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn setext_equals_underline_is_an_isolated_level_one_heading() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "Title\n=====\n\nbody",
            Style::default().fg(theme.markdown_text),
            theme,
            40,
            &[],
        );
        let title = lines.first().expect("Setext title line");

        assert_eq!(line_text(&title.line), "Title");
        assert!(
            title
                .line
                .spans
                .iter()
                .all(|span| span.style.fg == Some(theme.markdown_h1)
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "the isolated equals-underlined title must use only the level-one heading style"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line_text(&line.line).contains("=====")),
            "the Setext underline is syntax, not a visible row"
        );
        assert!(
            lines.iter().any(|line| line_text(&line.line) == "body"),
            "the following body remains ordinary document content"
        );
    }

    #[test]
    fn single_dash_gfm_delimiters_render_a_table_in_the_production_markdown_path() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "a | b\n- | -\nc | d",
            Style::default().fg(theme.markdown_text),
            theme,
            40,
            &[],
        );
        let plain = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();

        assert!(
            plain
                .first()
                .is_some_and(|line| line.starts_with('┌') && line.ends_with('┐')),
            "a single-dash GFM delimiter row must enter the table renderer: {plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|line| line.contains('a') && line.contains('b'))
        );
        assert!(
            plain
                .iter()
                .any(|line| line.contains('c') && line.contains('d'))
        );
        assert!(
            plain.iter().all(|line| !line.contains("- | -")),
            "the delimiter is parser syntax rather than a visible prose row: {plain:?}"
        );
    }

    #[test]
    fn atx_heading_levels_two_through_six_reach_the_production_heading_path() {
        let theme = CrabCodeTheme::NIGHT;
        for level in 2..=6 {
            let source = format!("{} h{level}", "#".repeat(level));
            let expected = format!("h{level}");
            assert_eq!(atx_heading(&source), Some((level, expected.as_str())),);
            let lines = markdown_lines(
                &source,
                Style::default().fg(theme.markdown_text),
                theme,
                40,
                &[],
            );
            assert_eq!(lines.len(), 1, "level {level}: {lines:?}");
            assert_eq!(line_text(&lines[0].line), expected);
            assert!(
                lines[0].line.spans.iter().all(|span| {
                    span.style.fg == heading_style(level, theme).fg
                        && span.style.add_modifier.contains(Modifier::BOLD)
                }),
                "level {level} must retain the production heading classification"
            );
        }
    }

    #[test]
    fn fenced_code_info_string_labels_the_production_code_block() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "```rust\nx\n```\n",
            Style::default().fg(theme.markdown_text),
            theme,
            60,
            &[],
        );
        let plain = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();

        assert_eq!(&plain[..3], ["┌─ rust", "│ x", "└─"]);
        assert_eq!(plain.get(3).map(String::as_str), Some(""));
        let label = lines[0]
            .line
            .spans
            .iter()
            .find(|span| span.content == "rust")
            .expect("fence info label");
        assert_eq!(label.style.fg, Some(theme.markdown_code));
        assert!(label.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn four_space_indent_renders_as_indented_code_without_a_fence() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "    code\nbody",
            Style::default().fg(theme.markdown_text),
            theme,
            40,
            &[],
        );

        assert_eq!(line_text(&lines[0].line), "│ code");
        let code = lines[0]
            .line
            .spans
            .iter()
            .find(|span| span.content == "code")
            .expect("indented code payload");
        assert_eq!(code.style.fg, Some(theme.markdown_code));
        assert_eq!(code.style.bg, Some(theme.markdown_code_bg));
        assert_eq!(line_text(&lines[1].line), "body");
        assert!(
            lines
                .iter()
                .all(|line| !line_text(&line.line).contains("┌─")),
            "indented code must not be reclassified as a fenced block"
        );
    }

    #[test]
    fn thematic_break_is_painted_as_a_rule_in_the_production_markdown_path() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "---",
            Style::default().fg(theme.markdown_text),
            theme,
            32,
            &[],
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0].line), "─".repeat(32));
        assert_eq!(lines[0].line.style.fg, Some(theme.gray_dim));
    }

    #[test]
    fn task_list_items_remain_a_subset_of_production_list_items() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "- [ ] a\n- [x] b\n- c",
            Style::default().fg(theme.markdown_text),
            theme,
            40,
            &[],
        );
        let plain = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();

        assert_eq!(plain, ["○ a", "✓ b", "• c"]);
        assert_eq!(
            plain
                .iter()
                .filter(|line| line.starts_with('○') || line.starts_with('✓'))
                .count(),
            2
        );
        assert_eq!(plain.len(), 3);
    }

    #[test]
    fn local_markdown_image_is_not_emitted_as_an_ordinary_link() {
        let theme = CrabCodeTheme::NIGHT;
        let rendered = inline_markdown_with_links(
            "![a](x.png)",
            Style::default().fg(theme.markdown_text),
            theme,
            &[],
        );

        assert_eq!(
            rendered
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "🖼 a (x.png)"
        );
        assert!(
            rendered.links.is_empty(),
            "an image event must not also create an ordinary link event"
        );
    }

    #[test]
    fn markdown_tables_fit_display_width_and_keep_inline_styles() {
        let theme = CrabCodeTheme::NIGHT;
        let lines = markdown_lines(
            "| Name | Status |\n| --- | --- |\n| 世界 | **ready** |\n| long-value-that-must-shrink | `code` |",
            Style::default().fg(theme.markdown_text),
            theme,
            24,
            &[],
        );
        assert!(
            lines.iter().all(|line| line.line.width() <= 24),
            "{lines:?}"
        );
        let plain = lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();
        assert!(
            plain
                .first()
                .is_some_and(|line| line.starts_with('┌') && line.ends_with('┐'))
        );
        assert!(plain.iter().any(|line| line.contains("世界")));
        assert!(
            plain.iter().all(|line| !line.contains('…')),
            "constrained cells wrap instead of discarding source text: {plain:?}"
        );
        assert!(lines.iter().flat_map(|line| &line.line.spans).any(|span| {
            span.content.contains("ready") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn constrained_table_wraps_without_loss_and_keeps_source_line_mapping() {
        let source = concat!(
            "| Column A | Column B | Column C |\n",
            "|----------|----------|----------|\n",
            "| value 1  | value 2  | value 3  |\n\n",
        );
        let theme = CrabCodeTheme::NIGHT;
        let style = Style::default().fg(theme.markdown_text);
        let mut full_highlighter = crate::crabcode_markdown::CrabCodeOpenCodeHighlighter::default();
        let full = markdown_render_full(source, style, theme, 80, &[], 0, &mut full_highlighter);
        assert!(
            full.lines.iter().map(|line| line.line.width()).max() > Some(30),
            "the exact upstream fixture must be naturally wider than the constraint"
        );
        let mut highlighter = crate::crabcode_markdown::CrabCodeOpenCodeHighlighter::default();
        let rendered = markdown_render_full(source, style, theme, 30, &[], 0, &mut highlighter);
        let text = rendered
            .lines
            .iter()
            .map(|line| line_text(&line.line))
            .collect::<Vec<_>>();
        assert!(
            text.iter().all(|line| line.width() <= 30),
            "table exceeded the configured width: {text:?}"
        );
        let mut rendered_content = text
            .concat()
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<Vec<_>>();
        rendered_content.sort_unstable();
        let mut expected_content = "ColumnAColumnBColumnCvalue1value2value3"
            .chars()
            .collect::<Vec<_>>();
        expected_content.sort_unstable();
        assert_eq!(
            rendered_content, expected_content,
            "wrapping must preserve every table content character exactly once: {text:?}"
        );
        assert_eq!(rendered.line_source_map.len(), rendered.lines.len());
        assert!(
            rendered
                .line_source_map
                .iter()
                .filter(|source_line| **source_line == 0)
                .count()
                >= 1,
            "header rows must map to source line zero: {:?}",
            rendered.line_source_map
        );
        assert!(
            rendered.line_source_map.contains(&1) && rendered.line_source_map.contains(&2),
            "separator and body rows retain their source mapping: {:?}",
            rendered.line_source_map
        );
    }

    #[test]
    fn inline_markdown_is_fail_soft_and_never_interprets_code_contents() {
        let theme = CrabCodeTheme::NIGHT;
        let spans = inline_markdown(
            "plain `**literal**` ~~gone~~ <https://example.test> unmatched **",
            Style::default(),
            theme,
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "plain **literal** gone https://example.test unmatched **"
        );
        let literal = spans
            .iter()
            .find(|span| span.content == "**literal**")
            .expect("inline code");
        assert_eq!(literal.style.bg, Some(theme.markdown_code_bg));
        let gone = spans
            .iter()
            .find(|span| span.content == "gone")
            .expect("strikethrough");
        assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
        let link = spans
            .iter()
            .find(|span| span.content == "https://example.test")
            .expect("autolink");
        assert_eq!(link.style.fg, Some(Color::Rgb(122, 166, 218)));
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn only_balanced_double_tilde_receives_strikethrough() {
        let theme = CrabCodeTheme::NIGHT;
        let spans = inline_markdown(
            "keep ~~deleted~~ but ~single~ and ~10%",
            Style::default(),
            theme,
        );
        let plain = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(plain, "keep deleted but ~single~ and ~10%");
        let crossed = spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(crossed, "deleted");
        assert!(
            spans
                .iter()
                .filter(|span| span.content.contains("~single~") || span.content.contains("~10%"))
                .all(|span| !span.style.add_modifier.contains(Modifier::CROSSED_OUT)),
            "single tilde pairs and percentages must remain literal"
        );
    }

    #[test]
    fn bare_url_trailing_period_stays_outside_the_production_link_span() {
        let theme = CrabCodeTheme::NIGHT;
        let spans = inline_markdown(
            "See https://example.test/path.",
            Style::default().fg(theme.markdown_text),
            theme,
        );
        let link = spans
            .iter()
            .find(|span| span.content == "https://example.test/path")
            .expect("URL span");

        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(
            spans.iter().any(|span| {
                span.content.ends_with('.')
                    && !span.style.add_modifier.contains(Modifier::UNDERLINED)
            }),
            "sentence punctuation must remain literal prose outside the URL style"
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "See https://example.test/path."
        );
    }

    #[test]
    fn pretty_markdown_link_keeps_upstream_label_and_suffix_navigation_groups() {
        let theme = CrabCodeTheme::NIGHT;
        let mut lines = markdown_lines(
            "Here is a [link](https://example.com) in text.\n",
            Style::default().fg(theme.markdown_text),
            theme,
            80,
            &[],
        );
        assert!(!lines.is_empty());
        let rendered = wrap_annotated_line(lines.remove(0), 80, &[]);
        assert_eq!(
            line_text(&rendered.lines[0]),
            "Here is a link (https://example.com) in text."
        );
        let matching = rendered
            .link_groups
            .iter()
            .filter(|group| {
                group.target == LinkTarget::Url(std::sync::Arc::from("https://example.com"))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            2,
            "pinned upstream emits one label group and one displayed-URL group"
        );
        assert_eq!(
            ascii_fragment_text(&rendered.lines, &matching[0].fragments[0]),
            "link"
        );
        assert_eq!(
            ascii_fragment_text(&rendered.lines, &matching[1].fragments[0]),
            "https://example.com"
        );
    }

    #[test]
    fn soft_wrapped_url_is_one_navigation_group_with_all_visual_fragments() {
        let url = "https://example.test/a/very/long/generated/resource.png";
        let rendered = wrap_annotated_line(
            AnnotatedLine::plain(Line::raw(format!("open {url}"))),
            13,
            &[],
        );
        let matching = rendered
            .link_groups
            .iter()
            .filter(|group| group.target == LinkTarget::Url(std::sync::Arc::from(url)))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert!(
            matching[0].fragments.len() > 1,
            "the URL must retain every painted wrap fragment: {matching:?}"
        );
        assert_eq!(
            matching[0]
                .fragments
                .iter()
                .map(|fragment| fragment.row)
                .collect::<HashSet<_>>()
                .len(),
            matching[0].fragments.len()
        );
        let spans = hyperlink_spans_for_visible_link_groups(
            &rendered.link_groups,
            Rect::new(0, 0, 13, rendered.lines.len() as u16),
        );
        assert_eq!(spans.len(), matching[0].fragments.len());
        assert!(
            spans
                .iter()
                .all(|span| span.id.is_some() && span.id == spans[0].id),
            "all fragments of one wrapped link must share one OSC 8 id: {spans:?}"
        );
    }

    #[test]
    fn generated_media_links_use_only_exact_tool_artifact_provenance() {
        let directory = tempfile::tempdir().expect("tempdir");
        let unique_path = directory.path().join("one/images/result.png");
        std::fs::create_dir_all(unique_path.parent().expect("unique parent"))
            .expect("create unique parent");
        std::fs::write(&unique_path, b"image").expect("write unique image");

        let envelope = RawEnvelope {
            sequence: 7,
            encoded_len: 1,
            value: json!({
                "type": "user",
                "tool_artifacts": [{
                    "producerToolUseId": "tool-a",
                    "kind": "image",
                    "location": {
                        "type": "runtimePath",
                        "path": unique_path
                    }
                }]
            }),
            classification: EnvelopeClass::User,
            correlation: None,
        };
        let mut provenance = ArtifactProvenance::default();
        provenance.ingest_unseen(&[envelope]);

        let mut item = projected_item("media", "[preview](images/result.png)", 7);
        item.tool_use_id = Some("tool-a".to_string());
        let paths = provenance.media_paths_for(&item);
        assert_eq!(paths, std::slice::from_ref(&unique_path));
        let rendered = render_projected_item(&item, 80, CrabCodeTheme::NIGHT, paths);
        assert_eq!(
            rendered
                .link_groups
                .iter()
                .filter(|group| {
                    group.target == LinkTarget::File(std::sync::Arc::from(unique_path.as_path()))
                })
                .count(),
            2,
            "pretty Markdown emits a label group and a displayed-path group"
        );

        item.tool_use_id = Some("tool-b".to_string());
        assert!(provenance.media_paths_for(&item).is_empty());
        assert!(
            render_projected_item(&item, 80, CrabCodeTheme::NIGHT, &[])
                .link_groups
                .is_empty(),
            "an unrelated tool must not inherit generated-media provenance"
        );

        let second_path = directory.path().join("two/images/result.png");
        std::fs::create_dir_all(second_path.parent().expect("second parent"))
            .expect("create second parent");
        std::fs::write(&second_path, b"image").expect("write second image");
        let ambiguous = [unique_path, second_path];
        assert!(
            render_projected_item(&item, 80, CrabCodeTheme::NIGHT, &ambiguous)
                .link_groups
                .is_empty(),
            "a relative suffix matching multiple authoritative artifacts must fail closed"
        );
    }

    #[test]
    fn image_preview_accepts_only_exact_admitted_image_runtime_paths() {
        let directory = tempfile::tempdir().expect("tempdir");
        let image_path = directory.path().join("result.png");
        let video_path = directory.path().join("result.mp4");
        let forged_path = directory.path().join("forged.png");
        std::fs::write(&image_path, b"not-decodable-but-authoritative")
            .expect("write authoritative image path");
        std::fs::write(&video_path, b"video").expect("write video path");
        std::fs::write(&forged_path, b"forged").expect("write forged path");
        let envelope = RawEnvelope {
            sequence: 90,
            encoded_len: 1,
            value: json!({
                "type": "user",
                "tool_artifacts": [
                    {
                        "producerToolUseId": "tool-image",
                        "kind": "image",
                        "location": {"type": "runtimePath", "path": image_path}
                    },
                    {
                        "producerToolUseId": "tool-video",
                        "kind": "video",
                        "location": {"type": "runtimePath", "path": video_path}
                    },
                    {
                        "producerToolUseId": "tool-relative",
                        "kind": "image",
                        "location": {"type": "runtimePath", "path": "relative.png"}
                    }
                ]
            }),
            classification: EnvelopeClass::User,
            correlation: None,
        };
        let mut cache = TranscriptLayoutCache::default();
        cache.ingest_artifact_envelope(&envelope);

        let admitted = LinkTarget::File(Arc::from(image_path.as_path()));
        let preview = cache
            .artifact_image_preview(&admitted)
            .expect("exact authoritative image path");
        assert_eq!(preview.display_path.as_deref(), Some(image_path.as_path()));
        assert_eq!(preview.display_number, 1);
        assert!(
            preview.preview_failed,
            "invalid bytes must fail to metadata"
        );

        assert!(
            cache
                .artifact_image_preview(&LinkTarget::File(Arc::from(forged_path.as_path())))
                .is_none(),
            "an existing path not present in ArtifactProvenance must not be guessed"
        );
        assert!(
            cache
                .artifact_image_preview(&LinkTarget::File(Arc::from(video_path.as_path())))
                .is_none(),
            "video provenance must not be promoted to an image preview"
        );
        assert!(
            cache
                .artifact_image_preview(&LinkTarget::Url(Arc::from("https://example.test/a.png")))
                .is_none()
        );
    }

    #[test]
    fn pre_eviction_artifact_ingest_survives_an_empty_retained_raw_slice() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("generated/preview.png");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(&path, b"image").expect("write image");
        let envelope = RawEnvelope {
            sequence: 41,
            encoded_len: 1,
            value: json!({
                "type": "user",
                "tool_artifacts": [{
                    "producerToolUseId": "tool-41",
                    "kind": "image",
                    "location": {"type": "runtimePath", "path": path}
                }]
            }),
            classification: EnvelopeClass::User,
            correlation: None,
        };
        let mut item = projected_item("media-41", "[preview](generated/preview.png)", 41);
        item.tool_use_id = Some("tool-41".to_string());
        let mut forged_item = projected_item("media-forged", "[forged](generated/preview.png)", 42);
        forged_item.tool_use_id = Some("tool-forged".to_string());
        let items = vec![item, forged_item];
        let committed = HashSet::new();
        let projection = Projection::default();
        let mut cache = TranscriptLayoutCache::default();

        cache.ingest_artifact_envelope(&envelope);
        cache.ingest_artifact_envelope(&RawEnvelope {
            sequence: 42,
            encoded_len: 1,
            value: json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "text",
                        "tool_artifacts": [{
                            "producerToolUseId": "tool-forged",
                            "kind": "image",
                            "location": {"type": "runtimePath", "path": path}
                        }]
                    }]
                }
            }),
            classification: EnvelopeClass::Assistant,
            correlation: None,
        });
        let mut scroll = crate::tui_render::ScrollState::default();
        cache.synchronize(
            &items,
            &[],
            &projection,
            1,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            80,
            &mut scroll,
            20,
        );
        let visible = cache.visible_lines(&items, 0, 20, CrabCodeTheme::NIGHT);
        assert_eq!(
            visible
                .link_groups
                .iter()
                .filter(|group| {
                    group.target == LinkTarget::File(std::sync::Arc::from(path.as_path()))
                })
                .count(),
            2,
            "provenance must not depend on the raw envelope still being retained at render time"
        );
    }

    #[test]
    fn transcript_notices_are_sanitized_wrapped_counted_and_link_offset_aware() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.push_startup_notice(
            "boot https://startup.example/path\nsecond\u{1b}]52;c;payload\u{7}".to_string(),
        );
        app.push_stderr(b"stderr https://stderr.example/path\ncontinued", false);
        let notices = transcript_notice_records(&app);
        assert_eq!(notices.len(), 2);
        assert!(
            notices
                .iter()
                .all(|notice| !notice.text.contains(['\u{1b}', '\u{7}']))
        );

        let item = projected_item("assistant", "body https://assistant.example/path", 1);
        let items = vec![item];
        let committed = HashSet::new();
        let projection = Projection::default();
        let theme = CrabCodeTheme::NIGHT;
        let width = 32;
        let notice_render = render_notice_records(&notices, width, theme);
        assert!(notice_render.lines.len() >= 4);
        let item_render = render_projected_item(&items[0], width, theme, &[]);
        let expected_total = notice_render
            .lines
            .len()
            .saturating_add(item_render.lines.len().saturating_sub(1));

        let mut cache = TranscriptLayoutCache::default();
        let mut scroll = crate::tui_render::ScrollState::default();
        cache.synchronize(
            &items,
            &[],
            &projection,
            1,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &notices,
            theme,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            width,
            &mut scroll,
            expected_total,
        );
        assert_eq!(cache.total_lines(), expected_total);
        let visible = cache.visible_lines(&items, 0, expected_total, theme);
        let text = visible.lines.iter().map(line_text).collect::<String>();
        assert!(text.contains("startup"), "{text:?}");
        assert!(text.contains("backend stderr"), "{text:?}");
        assert!(text.contains('␛'), "{text:?}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{7}'), "{text:?}");

        let startup = visible
            .link_groups
            .iter()
            .find(|group| group.target.display_text() == "https://startup.example/path")
            .and_then(VisibleLinkGroup::primary)
            .expect("startup link");
        let assistant = visible
            .link_groups
            .iter()
            .find(|group| group.target.display_text() == "https://assistant.example/path")
            .and_then(VisibleLinkGroup::primary)
            .expect("assistant link");
        assert!(startup.row < notice_render.lines.len());
        assert!(assistant.row >= notice_render.lines.len());
    }

    #[test]
    fn clipping_top_of_wrapped_link_retains_one_visible_navigation_group() {
        let rendered = wrap_annotated_line(
            AnnotatedLine::plain(Line::raw(
                "https://example.test/long/path/that/spans/many/rows",
            )),
            10,
            &[],
        );
        assert!(rendered.lines.len() > 2);
        assert_eq!(rendered.link_groups.len(), 1);
        let mut visible = VisibleTranscript::default();
        append_visible_slice(&rendered, 1, rendered.lines.len(), &mut visible);
        assert_eq!(visible.link_groups.len(), 1);
        assert_eq!(
            visible.link_groups[0].target,
            LinkTarget::Url(std::sync::Arc::from(
                "https://example.test/long/path/that/spans/many/rows"
            ))
        );
        assert_eq!(
            visible.link_groups[0].primary().map(|link| link.row),
            Some(0)
        );
    }

    #[test]
    fn terminal_render_source_budget_is_utf8_safe_and_explicit() {
        let input = "界".repeat(MAX_RENDER_FIELD_BYTES);
        let (prefix, omitted) = bounded_terminal_source(&input);
        assert!(prefix.is_char_boundary(prefix.len()));
        assert!(omitted.is_some_and(|bytes| bytes > 0));
        assert_eq!(prefix.len() + omitted.unwrap_or_default(), input.len());
    }

    #[test]
    fn transcript_layout_is_incremental_and_follow_live_survives_more_than_old_global_budget() {
        let mut items = (0_u64..1_001)
            .map(|sequence| {
                projected_item(
                    format!("item-{sequence}"),
                    if sequence == 1_000 {
                        "LATEST".to_string()
                    } else {
                        "layout-line  \n".repeat(100)
                    },
                    sequence,
                )
            })
            .collect::<Vec<_>>();
        let committed = HashSet::new();
        let projection = Projection::default();
        let mut cache = TranscriptLayoutCache::default();
        let mut scroll = crate::tui_render::ScrollState::default();
        cache.synchronize(
            &items,
            &[],
            &projection,
            1_001,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            40,
            &mut scroll,
            5,
        );
        assert!(cache.total_lines() > 100_000);
        let initial_passes = cache.render_passes();
        let visible = cache.visible_lines(&items, scroll.offset(), 5, CrabCodeTheme::NIGHT);
        assert!(
            visible
                .lines
                .iter()
                .any(|line| line_text(line).contains("LATEST")),
            "{visible:?}"
        );

        cache.synchronize(
            &items,
            &[],
            &projection,
            1_001,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            40,
            &mut scroll,
            5,
        );
        assert_eq!(cache.render_passes(), initial_passes);

        items.push(projected_item("new-tail", "NEWEST", 1_001));
        cache.synchronize(
            &items,
            &[],
            &projection,
            1_002,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            40,
            &mut scroll,
            5,
        );
        assert_eq!(cache.render_passes(), initial_passes + 1);
        let visible = cache.visible_lines(&items, scroll.offset(), 5, CrabCodeTheme::NIGHT);
        assert!(
            visible
                .lines
                .iter()
                .any(|line| line_text(line).contains("NEWEST")),
            "{visible:?}"
        );
    }

    #[test]
    fn transcript_layout_rebuilds_for_palette_and_syntax_setting_changes() {
        let items = vec![projected_item(
            "theme-sensitive",
            "```rust\nfn main() { let value = 1; }\n```",
            1,
        )];
        let projection = Projection::default();
        let committed = HashSet::new();
        let mut cache = TranscriptLayoutCache::default();
        let mut scroll = crate::tui_render::ScrollState::default();
        let synchronize = |cache: &mut TranscriptLayoutCache,
                           scroll: &mut crate::tui_render::ScrollState,
                           theme: CrabCodeTheme,
                           syntax_theme: Option<CrabCodeSyntaxTheme>| {
            cache.synchronize(
                &items,
                &[],
                &projection,
                1,
                &committed,
                &HashMap::new(),
                None,
                false,
                false,
                0,
                &[],
                theme,
                syntax_theme,
                80,
                scroll,
                20,
            );
        };

        synchronize(
            &mut cache,
            &mut scroll,
            CrabCodeTheme::dark(),
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
        );
        let dark_passes = cache.render_passes();
        let dark = cache.visible_lines(&items, 0, 20, CrabCodeTheme::dark());

        synchronize(
            &mut cache,
            &mut scroll,
            CrabCodeTheme::light(),
            Some(CrabCodeSyntaxTheme::Github),
        );
        assert_eq!(cache.render_passes(), dark_passes + 1);
        let light = cache.visible_lines(&items, 0, 20, CrabCodeTheme::light());
        assert_ne!(
            dark.lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
                .collect::<Vec<_>>(),
            light
                .lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
                .collect::<Vec<_>>(),
        );

        let light_passes = cache.render_passes();
        synchronize(&mut cache, &mut scroll, CrabCodeTheme::light(), None);
        assert_eq!(
            cache.render_passes(),
            light_passes + 1,
            "syntax disable must invalidate an otherwise identical layout"
        );
    }

    #[test]
    fn transcript_layout_preserves_item_anchor_across_history_insert_and_resize() {
        let mut items = vec![
            projected_item("a", "alpha ".repeat(20), 1),
            projected_item("b", "bravo ".repeat(20), 2),
            projected_item("c", "charlie ".repeat(20), 3),
        ];
        let committed = HashSet::new();
        let projection = Projection::default();
        let mut cache = TranscriptLayoutCache::default();
        let mut scroll = crate::tui_render::ScrollState::default();
        cache.synchronize(
            &items,
            &[],
            &projection,
            3,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            20,
            &mut scroll,
            4,
        );
        let (b_start, _) = cache.entry_start_and_value("b").expect("b layout");
        scroll.set_offset(b_start, cache.total_lines(), 4);
        assert!(!scroll.is_following());

        items.insert(0, projected_item("history", "older history", 4));
        cache.synchronize(
            &items,
            &[],
            &projection,
            4,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            20,
            &mut scroll,
            4,
        );
        assert_eq!(
            cache.anchor_at(scroll.offset()).and_then(anchor_item_key),
            Some("b".to_string())
        );

        cache.synchronize(
            &items,
            &[],
            &projection,
            4,
            &committed,
            &HashMap::new(),
            None,
            false,
            false,
            0,
            &[],
            CrabCodeTheme::NIGHT,
            Some(CrabCodeSyntaxTheme::MonokaiExtended),
            9,
            &mut scroll,
            4,
        );
        assert_eq!(
            cache.anchor_at(scroll.offset()).and_then(anchor_item_key),
            Some("b".to_string())
        );
    }

    #[test]
    fn visible_links_map_to_clipped_absolute_hyperlink_spans() {
        let links = vec![
            VisibleLink {
                target: crate::tui_links::LinkTarget::Url(std::sync::Arc::from(
                    "https://example.test/docs",
                )),
                row: 0,
                start_column: 1,
                end_column: 20,
            },
            VisibleLink {
                target: crate::tui_links::LinkTarget::File(std::sync::Arc::from(
                    std::path::Path::new("/tmp/report final.txt"),
                )),
                row: 1,
                start_column: 0,
                end_column: 4,
            },
            VisibleLink {
                target: crate::tui_links::LinkTarget::Url(std::sync::Arc::from(
                    "https://outside.test",
                )),
                row: 2,
                start_column: 0,
                end_column: 3,
            },
        ];
        let groups = links
            .into_iter()
            .map(|link| VisibleLinkGroup {
                target: link.target.clone(),
                fragments: vec![link],
            })
            .collect::<Vec<_>>();
        let spans = hyperlink_spans_for_visible_link_groups(&groups, Rect::new(5, 7, 10, 2));

        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].row, spans[0].col_start, spans[0].col_end),
            (7, 6, 15)
        );
        assert_eq!(spans[0].url.as_ref(), "https://example.test/docs");
        assert_eq!(spans[0].id, Some(1));
        assert_eq!(
            (spans[1].row, spans[1].col_start, spans[1].col_end),
            (8, 5, 9)
        );
        assert_eq!(spans[1].url.as_ref(), "file:///tmp/report%20final.txt");
        assert_eq!(spans[1].id, Some(2));
    }

    fn scrollbar_thumb_rows(buffer: &Buffer) -> Vec<u16> {
        let Some(column) = buffer.area.right().checked_sub(1) else {
            return Vec::new();
        };
        (buffer.area.y..buffer.area.bottom())
            .filter(|row| buffer[(column, *row)].symbol() == "█")
            .collect()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn ascii_fragment_text(lines: &[Line<'_>], fragment: &VisibleLink) -> String {
        line_text(&lines[fragment.row])
            .chars()
            .skip(fragment.start_column)
            .take(fragment.end_column.saturating_sub(fragment.start_column))
            .collect()
    }

    fn anchor_item_key(anchor: LayoutAnchor) -> Option<String> {
        match anchor {
            LayoutAnchor::Item { key, .. } => Some(key),
            LayoutAnchor::Notices { .. } => None,
        }
    }
}
