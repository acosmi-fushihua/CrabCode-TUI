//! Shared semantic model and geometry for CrabCode's new-session welcome.
//!
//! Full-screen/inline rendering and terminal-native scrollback insertion have
//! different owners, but they consume this exact content plan. No transcript
//! occupancy, terminal I/O, or backend mutation is consulted here.

use crabcode_pager_render::audited_glyphs::{is_legacy_windows_console, record_dot};
use crabcode_pager_render::audited_theme::CrabCodeTheme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget as _};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::text_safety::sanitize_bounded_terminal_text;
use crate::tui_app::{TuiApp, UiLanguage};
use crate::tui_render::fit_line_to_width;

pub(crate) const CRAB_WORDMARK_WIDTH: usize = 48;
pub(crate) const CRAB_WORDMARK_STACKED_WIDTH: usize = 24;

const WORDMARK_EMPTY: u8 = 0;
const WORDMARK_FACE: u8 = 1;
const WORDMARK_SHADOW: u8 = 2;
const WORDMARK_GLYPH_HEIGHT: usize = 5;
const WORDMARK_LETTER_GAP: usize = 1;
const WORDMARK_SHADOW_OFFSET: usize = 1;
const WORDMARK_STACK_GAP: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WelcomeLayout {
    Wide,
    Standard,
    Compact,
    BestEffort,
}

impl WelcomeLayout {
    /// Ordered breakpoint predicate. Width and available content height must
    /// both match; this prevents a wide-but-short terminal from choosing a
    /// surface it cannot display.
    pub(crate) const fn choose(width: u16, height: u16) -> Self {
        if width >= 100 && height >= 16 {
            Self::Wide
        } else if width >= 72 && height >= 12 {
            Self::Standard
        } else if width >= 60 && height >= 10 {
            Self::Compact
        } else {
            Self::BestEffort
        }
    }

    const fn surface_height(self) -> u16 {
        match self {
            // Native scrollback reserves 16 rows for Wide: a 15-row card
            // and one separator. Matching that geometry here prevents the
            // native adapter from creating a second, accidental blank row.
            Self::Wide => 15,
            Self::Standard => 11,
            Self::Compact => 9,
            Self::BestEffort => 3,
        }
    }

    const fn native_commit_height(self) -> u16 {
        match self {
            // One trailing row separates the committed card from the first
            // conversation record in native scrollback.
            Self::Wide => 16,
            Self::Standard => 12,
            Self::Compact => 10,
            Self::BestEffort => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WelcomeCommand {
    pub(crate) name: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WelcomeViewModel {
    pub(crate) language: UiLanguage,
    pub(crate) preparing: bool,
    pub(crate) model: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) commands: Vec<WelcomeCommand>,
}

impl WelcomeViewModel {
    pub(crate) fn from_app(app: &TuiApp) -> Self {
        let language = app.ui_language();
        let model = app.projection.model().map(|active| {
            app.models
                .iter()
                .find(|choice| choice.id == active)
                .map(|choice| choice.label.as_str())
                .filter(|label| !label.trim().is_empty())
                .unwrap_or(active)
                .to_string()
        });
        let cwd = app
            .workspace_cwd()
            .map(|path| path.to_string_lossy().into_owned());
        let commands = ["help", "resume", "model", "release-notes"]
            .into_iter()
            .filter_map(|name| {
                app.welcome_command(name).map(|command| WelcomeCommand {
                    name: format!("/{}", sanitize_bounded_terminal_text(&command.name)),
                    // Availability comes from the authoritative completion
                    // catalog. Presentation for this closed, known command
                    // set is localized here so a runtime catalog that emits
                    // English descriptions cannot create a mixed-language
                    // Chinese welcome surface.
                    description: welcome_command_description(language, name).to_string(),
                })
            })
            .collect();
        Self {
            language,
            preparing: app.busy()
                || matches!(
                    app.projection.session_state(),
                    Some("initializing" | "running")
                ),
            model,
            cwd,
            commands,
        }
    }
}

fn welcome_command_description(language: UiLanguage, name: &str) -> &'static str {
    match name {
        "help" => language.text("显示帮助和可用命令", "Show help and available commands"),
        "resume" => language.text("恢复历史会话", "Resume a previous session"),
        "model" => language.text("切换当前模型", "Switch the active model"),
        "release-notes" => language.text("查看版本说明", "View release notes"),
        _ => unreachable!("welcome command list is closed"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrabWordmarkLayout {
    Single,
    Stacked,
}

const fn crab_wordmark_glyph(character: char) -> Option<[&'static str; WORDMARK_GLYPH_HEIGHT]> {
    match character {
        'C' => Some(["01110", "10001", "10000", "10001", "01110"]),
        'R' => Some(["11110", "10001", "11110", "10010", "10001"]),
        'A' => Some(["01110", "10001", "11111", "10001", "10001"]),
        'B' => Some(["11110", "10001", "11110", "10001", "11110"]),
        'O' => Some(["01110", "10001", "10001", "10001", "01110"]),
        'D' => Some(["11100", "10010", "10001", "10010", "11100"]),
        'E' => Some(["11111", "10000", "11110", "10000", "11111"]),
        _ => None,
    }
}

fn build_wordmark_grid(word: &str) -> Vec<Vec<u8>> {
    let mut face_rows = vec![Vec::new(); WORDMARK_GLYPH_HEIGHT];
    for character in word.chars() {
        let glyph = crab_wordmark_glyph(character)
            .expect("CrabCode wordmarks contain only the closed glyph set");
        for (row_index, row) in glyph.into_iter().enumerate() {
            face_rows[row_index].extend(
                row.bytes()
                    .map(|pixel| u8::from(pixel == b'1') * WORDMARK_FACE),
            );
            face_rows[row_index].extend(std::iter::repeat_n(WORDMARK_EMPTY, WORDMARK_LETTER_GAP));
        }
    }
    let width = face_rows.first().map_or(0, Vec::len);
    let mut grid =
        vec![vec![WORDMARK_EMPTY; width]; WORDMARK_GLYPH_HEIGHT + WORDMARK_SHADOW_OFFSET];
    for (target, source) in grid.iter_mut().zip(&face_rows) {
        target.copy_from_slice(source);
    }
    for column in 0..width {
        if let Some(row) = (0..WORDMARK_GLYPH_HEIGHT)
            .rev()
            .find(|row| face_rows[*row][column] == WORDMARK_FACE)
        {
            for displacement in 1..=WORDMARK_SHADOW_OFFSET {
                if grid[row + displacement][column] == WORDMARK_EMPTY {
                    grid[row + displacement][column] = WORDMARK_SHADOW;
                }
            }
        }
    }
    grid
}

fn build_stacked_wordmark_grid() -> Vec<Vec<u8>> {
    let mut top = build_wordmark_grid("CRAB");
    let mut bottom = build_wordmark_grid("CODE");
    let width = top
        .first()
        .map_or(0, Vec::len)
        .max(bottom.first().map_or(0, Vec::len));
    for row in top.iter_mut().chain(&mut bottom) {
        row.resize(width, WORDMARK_EMPTY);
    }
    top.extend(std::iter::repeat_n(
        vec![WORDMARK_EMPTY; width],
        WORDMARK_STACK_GAP,
    ));
    top.extend(bottom);
    top
}

/// Historical half-block pixel geometry with current semantic theme colors.
pub(crate) fn crab_wordmark_lines(
    layout: CrabWordmarkLayout,
    theme: CrabCodeTheme,
) -> Vec<Line<'static>> {
    let grid = match layout {
        CrabWordmarkLayout::Single => build_wordmark_grid("CRABCODE"),
        CrabWordmarkLayout::Stacked => build_stacked_wordmark_grid(),
    };
    debug_assert_eq!(
        grid.first().map_or(0, Vec::len),
        match layout {
            CrabWordmarkLayout::Single => CRAB_WORDMARK_WIDTH,
            CrabWordmarkLayout::Stacked => CRAB_WORDMARK_STACKED_WIDTH,
        }
    );
    let face = theme.accent_assistant;
    let shadow = wordmark_shadow(face, theme.gray_dim);
    grid.chunks(2)
        .map(|rows| {
            let top = &rows[0];
            let bottom = rows.get(1);
            Line::from(
                top.iter()
                    .enumerate()
                    .map(|(column, top)| {
                        let bottom = bottom
                            .and_then(|row| row.get(column))
                            .copied()
                            .unwrap_or(WORDMARK_EMPTY);
                        match (*top, bottom) {
                            (WORDMARK_EMPTY, WORDMARK_EMPTY) => Span::raw(" "),
                            (top, WORDMARK_EMPTY) => Span::styled(
                                "▀",
                                Style::default().fg(if top == WORDMARK_FACE {
                                    face
                                } else {
                                    shadow
                                }),
                            ),
                            (WORDMARK_EMPTY, bottom) => Span::styled(
                                "▄",
                                Style::default().fg(if bottom == WORDMARK_FACE {
                                    face
                                } else {
                                    shadow
                                }),
                            ),
                            (top, bottom) if top == bottom => Span::styled(
                                "█",
                                Style::default().fg(if top == WORDMARK_FACE {
                                    face
                                } else {
                                    shadow
                                }),
                            ),
                            (top, bottom) => Span::styled(
                                "▀",
                                Style::default()
                                    .fg(if top == WORDMARK_FACE { face } else { shadow })
                                    .bg(if bottom == WORDMARK_FACE {
                                        face
                                    } else {
                                        shadow
                                    }),
                            ),
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn wordmark_shadow(face: Color, fallback: Color) -> Color {
    match face {
        Color::Rgb(red, green, blue) => Color::Rgb(
            ((u16::from(red) * 45) / 100) as u8,
            ((u16::from(green) * 45) / 100) as u8,
            ((u16::from(blue) * 45) / 100) as u8,
        ),
        Color::LightRed => Color::Red,
        Color::LightMagenta => Color::Magenta,
        Color::LightBlue => Color::Blue,
        Color::LightCyan => Color::Cyan,
        Color::LightGreen => Color::Green,
        Color::LightYellow => Color::Yellow,
        Color::White => Color::Gray,
        _ => fallback,
    }
}

/// Height used by the native-scrollback adapter. The chosen virtual height
/// deliberately satisfies the same ordered breakpoint predicate as live UI.
pub(crate) fn native_welcome_height(width: u16) -> u16 {
    let virtual_height = if width >= 100 {
        16
    } else if width >= 72 {
        12
    } else if width >= 60 {
        10
    } else {
        3
    };
    WelcomeLayout::choose(width, virtual_height).native_commit_height()
}

/// Render the shared welcome content and return the rows reserved at the top
/// of the supplied area, including a trailing separation row when possible.
pub(crate) fn render_welcome_surface(
    buffer: &mut Buffer,
    area: Rect,
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
) -> u16 {
    render_welcome_surface_for_width(buffer, area, area.width, model, theme)
}

/// Live shell adapters may reserve horizontal transcript gutters. Pass the
/// pre-gutter terminal width so a formally supported 60-column terminal still
/// selects Compact rather than falling through to BestEffort at 58 cells.
pub(crate) fn render_welcome_surface_for_width(
    buffer: &mut Buffer,
    area: Rect,
    breakpoint_width: u16,
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
) -> u16 {
    if area.is_empty() {
        return 0;
    }
    let layout = WelcomeLayout::choose(breakpoint_width, area.height);
    let surface_height = layout.surface_height().min(area.height);
    let surface = Rect {
        height: surface_height,
        ..area
    };
    if layout == WelcomeLayout::BestEffort {
        Paragraph::new(best_effort_lines(model, theme, usize::from(surface.width)))
            .style(Style::default().bg(theme.bg_base))
            .render(surface, buffer);
        return surface_height;
    }

    let wordmark_supported = !is_legacy_windows_console();
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(if wordmark_supported {
            BorderType::Rounded
        } else {
            // Plain box drawing is covered by the legacy CP437 console font;
            // rounded corners are not. Keep the fallback surface tofu-free,
            // not just its wordmark.
            BorderType::Plain
        })
        .border_style(Style::default().fg(theme.gray_dim))
        .style(Style::default().bg(theme.bg_base));
    let inner = block.inner(surface).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    block.render(surface, buffer);
    let lines = match layout {
        WelcomeLayout::Wide => {
            wide_lines(model, theme, usize::from(inner.width), wordmark_supported)
        }
        WelcomeLayout::Standard => {
            standard_lines(model, theme, usize::from(inner.width), wordmark_supported)
        }
        WelcomeLayout::Compact => compact_lines(model, theme, usize::from(inner.width)),
        WelcomeLayout::BestEffort => unreachable!("handled above"),
    };
    Paragraph::new(lines)
        .style(Style::default().bg(theme.bg_base))
        .render(inner, buffer);
    surface_height.saturating_add(u16::from(surface_height < area.height))
}

fn surface_wordmark_lines(
    layout: CrabWordmarkLayout,
    theme: CrabCodeTheme,
    supported: bool,
) -> Vec<Line<'static>> {
    if supported {
        crab_wordmark_lines(layout, theme)
    } else {
        vec![Line::styled(
            "CrabCode",
            Style::default()
                .fg(theme.accent_assistant)
                .add_modifier(Modifier::BOLD),
        )]
    }
}

fn wide_lines(
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
    width: usize,
    wordmark_supported: bool,
) -> Vec<Line<'static>> {
    let left_width = width.saturating_mul(2).saturating_div(5).clamp(25, 38);
    let right_width = width.saturating_sub(left_width.saturating_add(3));
    let mut left = surface_wordmark_lines(CrabWordmarkLayout::Stacked, theme, wordmark_supported);
    left.push(Line::styled(
        model_line(model, left_width),
        Style::default().fg(theme.text_secondary),
    ));
    left.push(Line::styled(
        cwd_line(model, left_width),
        Style::default().fg(theme.gray),
    ));

    let mut right = vec![
        Line::styled(
            model.language.text("使用帮助", "Getting started"),
            Style::default()
                .fg(theme.accent_assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            model
                .language
                .text("输入任务描述即可开始", "Describe a task to get started"),
            Style::default().fg(theme.text_secondary),
        ),
        Line::default(),
    ];
    let mut release_notes = None;
    for command in &model.commands {
        if command.name == "/release-notes" {
            release_notes = Some(command_line(command, theme, right_width));
        } else if right.len() < 6 {
            right.push(command_line(command, theme, right_width));
        }
    }
    if let Some(line) = release_notes {
        right.push(Line::styled(
            "─".repeat(right_width),
            Style::default().fg(theme.gray_dim),
        ));
        right.push(line);
    }
    let rows = left.len().max(right.len());
    (0..rows)
        .map(|row| {
            combine_columns(
                left.get(row).cloned().unwrap_or_default(),
                right.get(row).cloned().unwrap_or_default(),
                left_width,
                right_width,
                theme,
            )
        })
        .collect()
}

fn standard_lines(
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
    width: usize,
    wordmark_supported: bool,
) -> Vec<Line<'static>> {
    let mut lines = surface_wordmark_lines(CrabWordmarkLayout::Single, theme, wordmark_supported);
    lines.push(Line::from(vec![
        Span::styled(
            model.language.text("开始", "Start"),
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            model.language.text(
                "  输入任务描述，或选择命令",
                "  Describe a task or choose a command",
            ),
            Style::default().fg(theme.text_secondary),
        ),
    ]));
    for command in model.commands.iter().take(3) {
        lines.push(command_line(command, theme, width));
    }
    let metadata_width = width.saturating_sub(5);
    let model_width = metadata_width / 2;
    let cwd_width = metadata_width.saturating_sub(model_width);
    lines.push(Line::styled(
        format!(
            "{}  ·  {}",
            model_line(model, model_width),
            cwd_line(model, cwd_width)
        ),
        Style::default().fg(theme.gray),
    ));
    lines
        .into_iter()
        .map(|line| fit_line_to_width(line, width))
        .collect()
}

fn compact_lines(
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![brand_line(model, theme), readiness_line(model, theme)];
    lines.push(Line::styled(
        model
            .language
            .text("输入任务描述即可开始", "Describe a task to get started"),
        Style::default().fg(theme.text_secondary),
    ));
    for command in model.commands.iter().take(2) {
        lines.push(command_line(command, theme, width));
    }
    lines.push(Line::styled(
        model_line(model, width),
        Style::default().fg(theme.gray),
    ));
    lines
        .into_iter()
        .map(|line| fit_line_to_width(line, width))
        .collect()
}

fn best_effort_lines(
    model: &WelcomeViewModel,
    theme: CrabCodeTheme,
    width: usize,
) -> Vec<Line<'static>> {
    let help_available = model.commands.iter().any(|command| command.name == "/help");
    [
        brand_line(model, theme),
        readiness_line(model, theme),
        Line::styled(
            match (model.language, help_available) {
                (UiLanguage::ZhCn, true) => "输入任务开始 · /help",
                (UiLanguage::EnUs, true) => "Describe a task · /help",
                (UiLanguage::ZhCn, false) => "输入任务即可开始",
                (UiLanguage::EnUs, false) => "Describe a task to start",
            },
            Style::default().fg(theme.text_secondary),
        ),
    ]
    .into_iter()
    .map(|line| fit_line_to_width(line, width))
    .collect()
}

fn brand_line(_model: &WelcomeViewModel, theme: CrabCodeTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "CrabCode",
            Style::default()
                .fg(theme.accent_assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.gray),
        ),
    ])
}

fn readiness_line(model: &WelcomeViewModel, theme: CrabCodeTheme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if model.preparing {
                format!("{} ", record_dot(true))
            } else {
                "● ".to_string()
            },
            Style::default().fg(if model.preparing {
                theme.accent_running
            } else {
                theme.accent_success
            }),
        ),
        Span::styled(
            if model.preparing {
                model.language.text("正在准备会话", "Preparing the session")
            } else {
                model.language.text("已就绪，可以开始", "Ready to start")
            },
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn model_line(model: &WelcomeViewModel, width: usize) -> String {
    let fallback = model.language.text("模型待同步", "Model pending");
    middle_ellipsis(
        &sanitize_bounded_terminal_text(model.model.as_deref().unwrap_or(fallback)),
        width,
    )
}

fn cwd_line(model: &WelcomeViewModel, width: usize) -> String {
    let fallback = model.language.text("工作区待同步", "Workspace pending");
    middle_ellipsis(
        &sanitize_bounded_terminal_text(model.cwd.as_deref().unwrap_or(fallback)),
        width,
    )
}

fn command_line(command: &WelcomeCommand, theme: CrabCodeTheme, width: usize) -> Line<'static> {
    let name_width = command.name.width().min(width);
    let description_width = width.saturating_sub(name_width.saturating_add(2));
    Line::from(vec![
        Span::styled(
            command.name.clone(),
            Style::default()
                .fg(theme.accent_system)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            middle_ellipsis(&command.description, description_width),
            Style::default().fg(theme.gray),
        ),
    ])
}

fn combine_columns(
    left: Line<'static>,
    right: Line<'static>,
    left_width: usize,
    right_width: usize,
    theme: CrabCodeTheme,
) -> Line<'static> {
    let mut line = fit_line_to_width(left, left_width);
    line.spans
        .push(Span::styled(" │ ", Style::default().fg(theme.gray_dim)));
    line.spans
        .extend(fit_line_to_width(right, right_width).spans);
    line
}

pub(crate) fn middle_ellipsis(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let left_budget = (width - 1).div_ceil(2);
    let right_budget = width - 1 - left_budget;
    let graphemes = value.graphemes(true).collect::<Vec<_>>();
    let mut left = String::new();
    let mut used: usize = 0;
    for grapheme in &graphemes {
        let next = grapheme.width();
        if used.saturating_add(next) > left_budget {
            break;
        }
        left.push_str(grapheme);
        used = used.saturating_add(next);
    }
    let mut right = String::new();
    used = 0;
    for grapheme in graphemes.iter().rev() {
        let next = grapheme.width();
        if used.saturating_add(next) > right_budget {
            break;
        }
        right.insert_str(0, grapheme);
        used = used.saturating_add(next);
    }
    format!("{left}…{right}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_require_both_width_and_height_in_order() {
        assert_eq!(WelcomeLayout::choose(120, 30), WelcomeLayout::Wide);
        assert_eq!(WelcomeLayout::choose(120, 10), WelcomeLayout::Compact);
        assert_eq!(WelcomeLayout::choose(80, 24), WelcomeLayout::Standard);
        assert_eq!(WelcomeLayout::choose(60, 20), WelcomeLayout::Compact);
        assert_eq!(WelcomeLayout::choose(59, 30), WelcomeLayout::BestEffort);
        assert_eq!(WelcomeLayout::choose(120, 9), WelcomeLayout::BestEffort);
    }

    #[test]
    fn historical_wordmarks_keep_their_exact_display_geometry() {
        let single = crab_wordmark_lines(CrabWordmarkLayout::Single, CrabCodeTheme::dark());
        assert_eq!(single.len(), 3);
        assert!(
            single
                .iter()
                .all(|line| line.width() == CRAB_WORDMARK_WIDTH)
        );
        let stacked = crab_wordmark_lines(CrabWordmarkLayout::Stacked, CrabCodeTheme::light());
        assert_eq!(stacked.len(), 7);
        assert!(
            stacked
                .iter()
                .all(|line| line.width() == CRAB_WORDMARK_STACKED_WIDTH)
        );
        assert_eq!(wordmark_shadow(Color::LightRed, Color::Gray), Color::Red);
        assert_eq!(
            wordmark_shadow(Color::Rgb(200, 100, 50), Color::Gray),
            Color::Rgb(90, 45, 22)
        );
    }

    #[test]
    fn legacy_console_capability_falls_back_to_plain_brand_text() {
        for layout in [CrabWordmarkLayout::Single, CrabWordmarkLayout::Stacked] {
            let lines = surface_wordmark_lines(layout, CrabCodeTheme::dark(), false);
            let rendered = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(rendered, "CrabCode");
            assert!(!rendered.contains(['▀', '▄', '█']));
        }
    }

    #[test]
    fn middle_ellipsis_is_unicode_width_bounded_and_keeps_both_ends() {
        let clipped = middle_ellipsis("DeepSeek-超长模型-Flash", 14);
        assert!(clipped.width() <= 14);
        assert!(clipped.starts_with("Deep"));
        assert!(clipped.ends_with("Flash"));
        assert!(clipped.contains('…'));
    }

    #[test]
    fn best_effort_does_not_invent_help_when_slash_commands_are_disabled() {
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        let model = WelcomeViewModel {
            language: UiLanguage::ZhCn,
            preparing: false,
            model: None,
            cwd: None,
            commands: Vec::new(),
        };
        render_welcome_surface(&mut buffer, area, &model, CrabCodeTheme::dark());
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("/help"), "{rendered}");
    }

    #[test]
    fn known_welcome_command_copy_is_localized_independently_of_runtime_copy() {
        assert_eq!(
            welcome_command_description(UiLanguage::ZhCn, "help"),
            "显示帮助和可用命令"
        );
        assert_eq!(
            welcome_command_description(UiLanguage::EnUs, "release-notes"),
            "View release notes"
        );
    }

    #[test]
    fn every_layout_stays_inside_its_buffer_for_both_languages_and_themes() {
        for language in [UiLanguage::ZhCn, UiLanguage::EnUs] {
            for theme in [
                CrabCodeTheme::dark(),
                CrabCodeTheme::light(),
                CrabCodeTheme::dark_daltonized(),
                CrabCodeTheme::light_daltonized(),
                CrabCodeTheme::dark_ansi(),
                CrabCodeTheme::light_ansi(),
            ] {
                for (width, height) in [(120, 20), (80, 14), (60, 10), (40, 3)] {
                    let area = Rect::new(0, 0, width, height);
                    let mut buffer = Buffer::empty(area);
                    let model = WelcomeViewModel {
                        language,
                        preparing: false,
                        model: Some("DeepSeek-v4-Flash-非常长的模型名称".to_string()),
                        cwd: Some("/workspace/一个很长的目录/CrabCode-TUI".to_string()),
                        commands: vec![WelcomeCommand {
                            name: "/help".to_string(),
                            description: language
                                .text("查看全部命令", "View every available command")
                                .to_string(),
                        }],
                    };
                    let used = render_welcome_surface(&mut buffer, area, &model, theme);
                    assert!(used <= height);
                    for row in 0..height {
                        for column in 0..width {
                            let symbol_width = buffer[(column, row)].symbol().width();
                            assert!(symbol_width <= usize::from(width - column));
                        }
                    }
                }
            }
        }
    }
}
