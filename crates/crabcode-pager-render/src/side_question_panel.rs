//! Renderer-owned `/btw` side-question panel.
//!
//! The panel is deliberately backend-independent. Its caller supplies only a
//! question and the already-returned response/error; request dispatch,
//! correlation, session state, and settings authority stay outside this
//! crate. The layout follows the pinned Rust TUI lifecycle: a compact panel in
//! normal flow below scrollback, bounded Markdown viewport, explicit close
//! geometry, and a link overlay merged after the scrollback links.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::appearance::RendererLanguage;
use crate::render::osc8::{LinkOverlay, scan_lines_for_url_overlays};
use crate::scrollback::blocks::MarkdownContent;
use crate::scrollback::render::map_hyperlinks_to_overlay;
use crate::scrollback::text_selection::{
    ResolvedSelectableLine, ResolvedSelectionModel, VisibleBlockGeometry,
};
use crate::theme::Theme;

/// Synthetic entry index reserved for the panel's text-selection surface.
pub const SIDE_QUESTION_PANEL_ENTRY_IDX: usize = usize::MAX;
const SIDE_QUESTION_PANEL_RANGE_ID: u16 = 0;

/// Maximum number of response rows painted at once.
pub const DONE_MAX_BODY_LINES: u16 = 12;

/// Renderer state for one side-question panel.
#[derive(Debug, Clone)]
pub enum SideQuestionPanelState {
    Loading {
        question: String,
    },
    Done {
        question: String,
        content: Box<MarkdownContent>,
        scroll_offset: usize,
    },
    Error {
        question: String,
        error: String,
    },
}

impl SideQuestionPanelState {
    pub fn done(question: String, response: String) -> Self {
        Self::Done {
            question,
            content: Box::new(MarkdownContent::new(response)),
            scroll_offset: 0,
        }
    }

    pub fn question(&self) -> &str {
        match self {
            Self::Loading { question }
            | Self::Done { question, .. }
            | Self::Error { question, .. } => question,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }

    pub fn scroll_up(&mut self, rows: usize) {
        if let Self::Done { scroll_offset, .. } = self {
            *scroll_offset = scroll_offset.saturating_sub(rows);
        }
    }

    pub fn scroll_down(&mut self, rows: usize, max_offset: usize) {
        if let Self::Done { scroll_offset, .. } = self {
            *scroll_offset = scroll_offset.saturating_add(rows).min(max_offset);
        }
    }

    pub fn max_scroll_offset(&self, content_width: usize, max_body_lines: usize) -> usize {
        match self {
            Self::Done { content, .. } if content_width > 0 => content
                .with_wrapped_lines(content_width, |wrapped| wrapped.lines.len())
                .saturating_sub(max_body_lines),
            _ => 0,
        }
    }

    /// Rebuild selection geometry for the complete response at the wrap width
    /// captured when a drag began. This keeps copy exact when the response is
    /// longer than the visible twelve-row panel.
    pub fn full_selection_model(&self, content_width: usize) -> ResolvedSelectionModel {
        let mut model = ResolvedSelectionModel::default();
        let Self::Done { content, .. } = self else {
            return model;
        };
        if content_width == 0 {
            return model;
        }
        content.with_wrapped_lines(content_width, |wrapped| {
            for (index, (line, joiner)) in
                wrapped.lines.iter().zip(wrapped.joiners.iter()).enumerate()
            {
                let text = line_plain_text(line);
                model.push_line(ResolvedSelectableLine {
                    entry_idx: SIDE_QUESTION_PANEL_ENTRY_IDX,
                    range_id: SIDE_QUESTION_PANEL_RANGE_ID,
                    block_line_idx: index,
                    screen_y: 0,
                    screen_x: 0,
                    selectable_cols: 0..u16::try_from(text.width()).unwrap_or(u16::MAX),
                    text,
                    joiner_to_previous: if index == 0 { None } else { joiner.clone() },
                });
            }
        });
        model
    }
}

/// Geometry and links published by one completed panel paint.
#[derive(Debug, Clone)]
pub struct SideQuestionPanelRender {
    pub close_rect: Rect,
    pub links: LinkOverlay,
    pub max_scroll_offset: usize,
    pub selection_model: ResolvedSelectionModel,
}

/// Desired panel height for the current width.
pub fn side_question_panel_height(state: Option<&SideQuestionPanelState>, panel_width: u16) -> u16 {
    match state {
        None => 0,
        Some(SideQuestionPanelState::Loading { .. } | SideQuestionPanelState::Error { .. }) => 3,
        Some(SideQuestionPanelState::Done { content, .. }) => {
            let content_width = panel_width.saturating_sub(4) as usize;
            let total = if content_width == 0 {
                1
            } else {
                content.with_wrapped_lines(content_width, |wrapped| wrapped.lines.len())
            };
            2 + u16::try_from(total.clamp(1, DONE_MAX_BODY_LINES as usize))
                .unwrap_or(DONE_MAX_BODY_LINES)
        }
    }
}

/// Paint one side-question panel.
///
/// Returns `None` when the supplied rectangle cannot hold the fixed minimum
/// surface. No stale hit geometry or links are returned on that path.
pub fn render_side_question_panel(
    buf: &mut Buffer,
    state: &SideQuestionPanelState,
    area: Rect,
    tick: u64,
    focused: bool,
    close_hovered: bool,
    media_paths: &[std::path::PathBuf],
) -> Option<SideQuestionPanelRender> {
    render_side_question_panel_for_language(
        buf,
        state,
        area,
        tick,
        focused,
        close_hovered,
        media_paths,
        RendererLanguage::default(),
    )
}

/// Paint one side-question panel using the direct TUI's selected language.
///
/// The question, response, error, URLs, and command/key markers are copied
/// verbatim; only renderer-owned fixed chrome is localized.
#[allow(clippy::too_many_arguments)]
pub fn render_side_question_panel_for_language(
    buf: &mut Buffer,
    state: &SideQuestionPanelState,
    area: Rect,
    tick: u64,
    focused: bool,
    close_hovered: bool,
    media_paths: &[std::path::PathBuf],
    language: RendererLanguage,
) -> Option<SideQuestionPanelRender> {
    if area.width < 12 || area.height < 3 {
        return None;
    }

    let theme = Theme::current();
    let background = theme.bg_base;
    let content_x = area.x.saturating_add(2);
    let content_width = area.width.saturating_sub(4) as usize;
    if content_width == 0 {
        return None;
    }
    let max_body = area.height.saturating_sub(2) as usize;
    let max_scroll_offset = state.max_scroll_offset(content_width, max_body);
    let focus_active = focused && max_scroll_offset > 0;
    let border_style = Style::default()
        .fg(if focus_active {
            theme.accent_user
        } else {
            theme.gray_dim
        })
        .bg(background);

    Clear.render(area, buf);
    buf.set_style(area, Style::default().bg(background));
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(Style::default().bg(background))
        .render(area, buf);

    let mut hint = match state {
        SideQuestionPanelState::Loading { .. } | SideQuestionPanelState::Error { .. } => {
            "[Esc]".to_string()
        }
        SideQuestionPanelState::Done {
            content,
            scroll_offset,
            ..
        } => {
            let total = content.with_wrapped_lines(content_width, |wrapped| wrapped.lines.len());
            if total > max_body {
                let offset = (*scroll_offset).min(total.saturating_sub(max_body));
                let first = offset.saturating_add(1);
                let last = offset.saturating_add(max_body).min(total);
                if focus_active {
                    format!("{first}-{last}/{total}  ↑↓  [Esc]")
                } else {
                    format!("{first}-{last}/{total}  [Esc]")
                }
            } else {
                "[Esc]".to_string()
            }
        }
    };
    hint = format!(" {hint} ");
    let title_x = area.x.saturating_add(2);
    let mut hint_width = u16::try_from(hint.width()).unwrap_or(u16::MAX);
    let mut hint_x = area
        .x
        .saturating_add(area.width)
        .saturating_sub(1_u16.saturating_add(hint_width));
    if hint_x < title_x {
        hint = " [Esc] ".to_string();
        hint_width = u16::try_from(hint.width()).unwrap_or(u16::MAX);
        hint_x = area
            .x
            .saturating_add(area.width)
            .saturating_sub(1_u16.saturating_add(hint_width));
    }

    let title = format!("/btw {}", state.question());
    let title_budget = hint_x.saturating_sub(title_x).saturating_sub(2) as usize;
    let title = truncate_with_ellipsis(&title, title_budget);
    let title = format!(" {title} ");
    let title_width = u16::try_from(title.width())
        .unwrap_or(u16::MAX)
        .min(hint_x.saturating_sub(title_x));
    buf.set_line(
        title_x,
        area.y,
        &Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.accent_user)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        )),
        title_width,
    );

    let close_rect = Rect::new(hint_x, area.y, hint_width, 1);
    buf.set_line(
        hint_x,
        area.y,
        &Line::from(Span::styled(
            hint,
            if close_hovered {
                Style::default()
                    .fg(theme.text_primary)
                    .bg(background)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.gray).bg(background)
            },
        )),
        hint_width,
    );

    let body_y = area.y.saturating_add(1);
    let mut links = LinkOverlay::new();
    let mut selection_model = ResolvedSelectionModel::default();
    match state {
        SideQuestionPanelState::Loading { .. } => {
            let frames = crate::glyphs::braille_spinner_frames();
            let spinner = frames[(tick as usize) % frames.len()];
            let style = Style::default().fg(theme.gray).bg(background);
            buf.set_line(
                content_x,
                body_y,
                &Line::from(vec![
                    Span::styled(format!("{spinner} "), style),
                    Span::styled(language.text("正在回答…", "Answering…"), style),
                ]),
                content_width as u16,
            );
        }
        SideQuestionPanelState::Done {
            content,
            scroll_offset,
            ..
        } => {
            let output = content.output(content_width);
            let total = output.lines.len();
            let skip = (*scroll_offset).min(total.saturating_sub(max_body));
            let end = skip.saturating_add(max_body).min(total);
            for (screen_offset, line_index) in (skip..end).enumerate() {
                let line = &output.lines[line_index];
                buf.set_line(
                    content_x,
                    body_y.saturating_add(screen_offset as u16),
                    &line.content,
                    content_width as u16,
                );
                let text = line_plain_text(&line.content);
                selection_model.push_line(ResolvedSelectableLine {
                    entry_idx: SIDE_QUESTION_PANEL_ENTRY_IDX,
                    range_id: SIDE_QUESTION_PANEL_RANGE_ID,
                    block_line_idx: line_index,
                    screen_y: body_y.saturating_add(screen_offset as u16),
                    screen_x: content_x,
                    selectable_cols: 0..u16::try_from(text.width()).unwrap_or(u16::MAX),
                    text,
                    joiner_to_previous: if line_index == 0 {
                        None
                    } else {
                        line.joiner.clone()
                    },
                });
            }
            let visible_count = end.saturating_sub(skip);
            if visible_count > 0 {
                let body_area = Rect::new(
                    content_x,
                    body_y,
                    u16::try_from(content_width).unwrap_or(u16::MAX),
                    u16::try_from(visible_count).unwrap_or(u16::MAX),
                );
                selection_model.content_area = body_area;
                selection_model.visible_blocks.push(VisibleBlockGeometry {
                    entry_idx: SIDE_QUESTION_PANEL_ENTRY_IDX,
                    area: body_area,
                    content_area: body_area,
                    selection_area: body_area,
                    content_width: u16::try_from(content_width).unwrap_or(u16::MAX),
                    top_clipped: false,
                    bottom_clipped: false,
                    drag_startable: true,
                });
            }

            let max_screen_y =
                body_y.saturating_add(u16::try_from(visible_count).unwrap_or(u16::MAX));
            content.with_hyperlinks(|hyperlinks| {
                if !hyperlinks.is_empty() {
                    map_hyperlinks_to_overlay(
                        hyperlinks,
                        &output,
                        skip,
                        body_y,
                        max_screen_y,
                        content_x,
                        0,
                        media_paths,
                        &mut links,
                    );
                }
            });
            let visible_lines = output
                .lines
                .iter()
                .enumerate()
                .skip(skip)
                .map(|(line_index, line)| {
                    (
                        body_y.saturating_add((line_index - skip) as u16),
                        &line.content,
                        line.joiner.as_deref(),
                    )
                })
                .take_while(|(screen_y, _, _)| *screen_y < max_screen_y);
            scan_lines_for_url_overlays(visible_lines, content_x, media_paths, &mut links);
        }
        SideQuestionPanelState::Error { error, .. } => {
            let text = truncate_with_ellipsis(error, content_width);
            buf.set_line(
                content_x,
                body_y,
                &Line::from(Span::styled(
                    text,
                    Style::default().fg(theme.accent_error).bg(background),
                )),
                content_width as u16,
            );
        }
    }

    Some(SideQuestionPanelRender {
        close_rect,
        links,
        max_scroll_offset,
        selection_model,
    })
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn truncate_with_ellipsis(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut width: usize = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if width.saturating_add(character_width).saturating_add(1) > max_width {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::osc8::resolve_link_target;

    fn row_text(buffer: &Buffer, width: u16, row: u16) -> String {
        (0..width)
            .filter_map(|column| {
                buffer
                    .cell((column, row))
                    .map(|cell| cell.symbol().to_string())
            })
            .collect()
    }

    #[test]
    fn loading_panel_keeps_close_affordance_and_spinner_inside_fixed_geometry() {
        let state = SideQuestionPanelState::Loading {
            question: "why?".to_string(),
        };
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        let rendered = render_side_question_panel(&mut buffer, &state, area, 0, false, false, &[])
            .expect("paintable panel");
        assert!(row_text(&buffer, 40, 0).contains("/btw why?"));
        assert!(row_text(&buffer, 40, 0).contains("[Esc]"));
        assert!(
            row_text(&buffer, 40, 1)
                .replace(' ', "")
                .contains("正在回答…")
        );
        assert!(area.contains(rendered.close_rect.as_position()));
        assert!(rendered.links.is_empty());
    }

    #[test]
    fn loading_panel_supports_english_without_rewriting_the_question() {
        let state = SideQuestionPanelState::Loading {
            question: "why-原始?".to_string(),
        };
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        render_side_question_panel_for_language(
            &mut buffer,
            &state,
            area,
            0,
            false,
            false,
            &[],
            RendererLanguage::EnUs,
        )
        .expect("paintable panel");

        assert!(
            row_text(&buffer, 40, 0)
                .replace(' ', "")
                .contains("/btwwhy-原始?")
        );
        assert!(row_text(&buffer, 40, 1).contains("Answering…"));
    }

    #[test]
    fn narrow_title_truncates_before_the_close_affordance() {
        let state = SideQuestionPanelState::Loading {
            question: "a deliberately long side question".to_string(),
        };
        let area = Rect::new(0, 0, 20, 3);
        let mut buffer = Buffer::empty(area);
        let rendered = render_side_question_panel(&mut buffer, &state, area, 0, false, false, &[])
            .expect("paintable panel");
        let top = row_text(&buffer, 20, 0);
        assert!(top.contains('…'), "{top:?}");
        assert!(top.contains("[Esc]"), "{top:?}");
        assert!(rendered.close_rect.right() <= area.right());
    }

    #[test]
    fn done_height_and_scroll_are_bounded_by_twelve_body_rows() {
        let response = (0..30)
            .map(|index| format!("paragraph {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut state = SideQuestionPanelState::done("q".to_string(), response);
        assert_eq!(side_question_panel_height(Some(&state), 40), 14);
        let max = state.max_scroll_offset(36, DONE_MAX_BODY_LINES as usize);
        assert!(max > 0);
        state.scroll_down(usize::MAX, max);
        assert_eq!(
            state.max_scroll_offset(36, DONE_MAX_BODY_LINES as usize),
            max
        );
        state.scroll_up(3);
        let SideQuestionPanelState::Done { scroll_offset, .. } = state else {
            unreachable!()
        };
        assert_eq!(scroll_offset, max.saturating_sub(3));
    }

    #[test]
    fn markdown_and_plain_urls_share_the_panel_link_overlay() {
        let state = SideQuestionPanelState::done(
            "links".to_string(),
            "[label](https://example.com/a)\nhttps://example.com/b".to_string(),
        );
        let area = Rect::new(0, 0, 60, 6);
        let mut buffer = Buffer::empty(area);
        let rendered = render_side_question_panel(&mut buffer, &state, area, 0, true, false, &[])
            .expect("paintable panel");
        let targets = rendered
            .links
            .links()
            .iter()
            .filter_map(|link| resolve_link_target(&link.target))
            .filter_map(|resolved| resolved.osc8_url)
            .collect::<Vec<_>>();
        assert!(
            targets
                .iter()
                .any(|url| url.as_ref() == "https://example.com/a")
        );
        assert!(
            targets
                .iter()
                .any(|url| url.as_ref() == "https://example.com/b")
        );
    }

    #[test]
    fn done_panel_publishes_visible_and_complete_selection_models() {
        let response = (0..20)
            .map(|index| format!("selectable row {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut state = SideQuestionPanelState::done("selection".to_string(), response);
        let max = state.max_scroll_offset(36, 4);
        state.scroll_down(3, max);

        let area = Rect::new(0, 0, 40, 6);
        let mut buffer = Buffer::empty(area);
        let rendered = render_side_question_panel(&mut buffer, &state, area, 0, true, false, &[])
            .expect("paintable panel");
        let visible_lines = rendered
            .selection_model
            .ranges
            .iter()
            .flat_map(|range| &range.lines)
            .collect::<Vec<_>>();
        assert_eq!(visible_lines.len(), 4);
        assert!(visible_lines.iter().all(|line| {
            line.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX
                && line.range_id == SIDE_QUESTION_PANEL_RANGE_ID
                && rendered
                    .selection_model
                    .content_area
                    .contains((line.screen_x, line.screen_y).into())
        }));
        assert_eq!(rendered.selection_model.visible_blocks.len(), 1);

        let full = state.full_selection_model(36);
        let full_lines = full
            .ranges
            .iter()
            .flat_map(|range| &range.lines)
            .collect::<Vec<_>>();
        assert!(
            full_lines.len() > visible_lines.len(),
            "copy reconstruction must include off-screen response rows"
        );
        assert_eq!(full_lines.first().map(|line| line.block_line_idx), Some(0));
    }

    #[test]
    fn unpaintable_panel_publishes_no_geometry() {
        let state = SideQuestionPanelState::Loading {
            question: "q".to_string(),
        };
        for area in [Rect::new(0, 0, 11, 3), Rect::new(0, 0, 12, 2)] {
            let mut buffer = Buffer::empty(area);
            assert!(
                render_side_question_panel(&mut buffer, &state, area, 0, false, false, &[])
                    .is_none()
            );
        }
    }
}
