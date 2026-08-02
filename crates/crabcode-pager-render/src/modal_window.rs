//! Shared native modal-window chrome.
//!
//! This module owns only deterministic terminal rendering and chrome input
//! routing: popup geometry, border/title/close button, tabs, footer shortcuts,
//! fold affordances, and embedded rendering. Product modal payloads remain in
//! the caller.

use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use crate::audited_render::line_utils::{byte_offset_at_width, truncate_str};
use crate::audited_theme::CrabCodeTheme;

pub use crate::audited_modal_window_state::{ModalWindowState, ShortcutHitArea};

/// Process-wide rendering mode selected by the terminal owner.
///
/// Embedded mode removes the centered popup frame and lets modal content fill
/// the supplied area. It is renderer state, not application state.
static EMBEDDED: AtomicBool = AtomicBool::new(false);

pub fn set_embedded(on: bool) {
    EMBEDDED.store(on, Ordering::Relaxed);
}

#[must_use]
pub fn embedded() -> bool {
    EMBEDDED.load(Ordering::Relaxed)
}

/// Resolved list-row styling used by embedded modal content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbeddedRowStyle {
    pub bg: Color,
    pub selected: bool,
    selected_fg: Color,
}

impl EmbeddedRowStyle {
    #[must_use]
    pub fn fg(&self, normal: Color) -> Color {
        if self.selected {
            self.selected_fg
        } else {
            normal
        }
    }
}

#[must_use]
pub fn embedded_row_style(theme: &CrabCodeTheme, is_selected: bool) -> Option<EmbeddedRowStyle> {
    embedded().then_some(EmbeddedRowStyle {
        bg: Color::Reset,
        selected: is_selected,
        selected_fg: theme.fuzzy_accent,
    })
}

/// Per-frame configuration for modal chrome.
pub struct ModalWindowConfig<'a> {
    pub title: &'a str,
    pub tabs: Option<&'a [&'a str]>,
    pub shortcuts: &'a [Shortcut<'a>],
    pub sizing: ModalSizing,
    pub fold_info: Option<FoldInfo>,
}

/// Popup geometry and internal padding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModalSizing {
    pub width_pct: f32,
    pub max_width: u16,
    pub min_width: u16,
    pub v_margin: u16,
    pub h_pad: u16,
    pub v_pad: u16,
    pub footer_lines: u16,
}

impl Default for ModalSizing {
    fn default() -> Self {
        Self {
            width_pct: 0.9,
            max_width: 140,
            min_width: 60,
            v_margin: 7,
            h_pad: 2,
            v_pad: 2,
            footer_lines: 2,
        }
    }
}

impl ModalSizing {
    #[must_use]
    pub fn medium() -> Self {
        Self {
            width_pct: 0.60,
            max_width: 120,
            min_width: 44,
            v_margin: 4,
            h_pad: 2,
            v_pad: 1,
            footer_lines: 2,
        }
    }

    #[must_use]
    pub fn large() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_compact(mut self, compact: bool) -> Self {
        if compact {
            self.v_margin = 0;
            self.h_pad = 1;
            self.v_pad = 0;
        }
        self
    }
}

/// Fold state of the caller-owned focused entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldInfo {
    pub collapsible: bool,
    pub expanded: bool,
    pub has_details: bool,
    pub details_expanded: bool,
    pub parent_index: Option<usize>,
}

/// One footer shortcut or non-clickable hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut<'a> {
    pub label: &'a str,
    pub clickable: bool,
    pub id: usize,
}

/// Append the discoverability hint when the caller's authoritative picker
/// state is in vim navigation mode and search is not active.
///
/// Vim mode is explicit because this renderer crate does not own configuration.
pub fn push_vim_nav_search_hint<'a>(
    shortcuts: &mut Vec<Shortcut<'a>>,
    vim_mode: bool,
    search_active: bool,
) {
    if vim_mode && !search_active {
        shortcuts.push(Shortcut {
            label: "i search",
            clickable: false,
            id: 0,
        });
    }
}

/// Areas assigned to caller-owned modal content and shared footer chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModalContentArea {
    pub content: Rect,
    pub footer: Rect,
    pub inner_x: u16,
    pub inner_width: u16,
}

/// Result of generic modal chrome input handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalWindowOutcome {
    Handled,
    CloseRequested,
    TabChanged(usize),
    ShortcutActivated(usize),
    CollapseGroup,
    ExpandGroup,
    CollapseDetails,
    ExpandDetails,
    JumpToParent(usize),
    Unhandled,
}

/// Render modal chrome and return the caller's content/footer geometry.
///
/// Returns `None` when the supplied area cannot hold a meaningful modal. Hit
/// rectangles are cleared on that path so stale geometry cannot remain active.
pub fn render_modal_window(
    buf: &mut Buffer,
    area: Rect,
    state: &mut ModalWindowState,
    config: &ModalWindowConfig<'_>,
    theme: &CrabCodeTheme,
) -> Option<ModalContentArea> {
    let sizing = &config.sizing;
    let is_embedded = embedded();
    let (modal_width, modal_height) = if is_embedded {
        (area.width, area.height)
    } else {
        compute_modal_dims(area, sizing)
    };

    if modal_width < 20 || modal_height < 6 {
        clear_hit_geometry(state);
        return None;
    }

    let modal_area = if is_embedded {
        area
    } else {
        Rect {
            x: area.x + area.width.saturating_sub(modal_width) / 2,
            y: area.y + area.height.saturating_sub(modal_height) / 2,
            width: modal_width,
            height: modal_height,
        }
    };
    state.popup_area = Some(modal_area);
    Clear.render(modal_area, buf);

    let border_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
    let title_style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);

    let inner = if is_embedded {
        state.close_button_rect = None;
        if config.title.is_empty() {
            modal_area
        } else {
            let title = ratatui::text::Line::from(Span::styled(
                config.title,
                Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ));
            buf.set_line(
                modal_area.x + sizing.h_pad,
                modal_area.y,
                &title,
                modal_area.width.saturating_sub(sizing.h_pad),
            );
            Rect {
                x: modal_area.x,
                y: modal_area.y + 1,
                width: modal_area.width,
                height: modal_area.height.saturating_sub(1),
            }
        }
    } else {
        let mut block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().bg(theme.bg_base).fg(theme.text_primary))
            .border_style(border_style);
        if !config.title.is_empty() {
            block = block.title(ratatui::text::Line::from(vec![
                Span::styled("\u{2500} ", border_style),
                Span::styled(config.title, title_style),
                Span::styled(" \u{2500}", border_style),
            ]));
        }
        let inner = block.inner(modal_area);
        block.render(modal_area, buf);
        render_close_button(buf, modal_area, state, theme);
        inner
    };

    let (tab_bar_height, tab_divider_height) = if let Some(tabs) = config.tabs {
        let tab_bar_height = render_tab_bar(buf, inner, state, tabs, theme);
        let divider_y = inner.y + tab_bar_height;
        if divider_y < inner.y + inner.height {
            let divider_bg = if is_embedded {
                Color::Reset
            } else {
                theme.bg_base
            };
            let line: String = std::iter::repeat_n('\u{2500}', inner.width as usize).collect();
            buf.set_string(
                inner.x,
                divider_y,
                &line,
                Style::default().fg(theme.gray_dim).bg(divider_bg),
            );
        }
        (tab_bar_height, 1)
    } else {
        state.tab_count = 0;
        state.tab_rects.clear();
        (0, 0)
    };

    let effective_v_pad = if config.tabs.is_some() {
        0
    } else {
        sizing.v_pad
    };
    let footer_width = inner.width.saturating_sub(sizing.h_pad * 2);
    let footer_lines = sizing
        .footer_lines
        .max(shortcuts_rows_needed(config.shortcuts, footer_width));
    let content_top = inner.y + effective_v_pad + tab_bar_height + tab_divider_height;
    let content_height = inner
        .height
        .saturating_sub(effective_v_pad + tab_bar_height + tab_divider_height + footer_lines);
    let content = Rect {
        x: inner.x + sizing.h_pad,
        y: content_top,
        width: footer_width,
        height: content_height,
    };

    let footer_height = footer_lines.min(inner.height);
    let footer = Rect {
        x: inner.x + sizing.h_pad,
        y: inner.y + inner.height.saturating_sub(footer_height),
        width: footer_width,
        height: footer_height,
    };
    state.shortcut_hits =
        render_modal_shortcuts(buf, footer, config.shortcuts, state.hovered_shortcut, theme);

    Some(ModalContentArea {
        content,
        footer,
        inner_x: inner.x,
        inner_width: inner.width,
    })
}

fn clear_hit_geometry(state: &mut ModalWindowState) {
    state.popup_area = None;
    state.close_button_rect = None;
    state.shortcut_hits.clear();
    state.tab_rects.clear();
    state.tab_count = 0;
}

fn render_close_button(
    buf: &mut Buffer,
    modal_area: Rect,
    state: &mut ModalWindowState,
    theme: &CrabCodeTheme,
) {
    let cells = [" ", "[", crate::audited_glyphs::ballot_x(), "]", " "];
    let width = cells.len() as u16;
    let rect = Rect {
        x: modal_area.x + modal_area.width.saturating_sub(width + 2),
        y: modal_area.y,
        width,
        height: 1,
    };
    for (offset, symbol) in cells.iter().enumerate() {
        let column = rect.x + offset as u16;
        if let Some(cell) = buf.cell_mut((column, rect.y)) {
            cell.set_symbol(symbol);
            if !symbol.trim().is_empty() && state.close_hovered {
                let mut style = cell.style();
                style.fg = Some(theme.text_primary);
                cell.set_style(style.add_modifier(Modifier::BOLD));
            }
        }
    }
    state.close_button_rect = Some(rect);
}

fn render_tab_bar(
    buf: &mut Buffer,
    inner: Rect,
    state: &mut ModalWindowState,
    tabs: &[&str],
    theme: &CrabCodeTheme,
) -> u16 {
    state.tab_count = tabs.len();
    state.tab_rects = vec![None; tabs.len()];

    let left_margin = 2u16;
    let available = (inner.width as usize).saturating_sub(left_margin as usize);
    if available == 0 {
        return 0;
    }
    let separator = "  ";
    let separator_width = separator.width();
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for (index, label) in tabs.iter().enumerate() {
        let label_width = label.width();
        let needed = if rows.last().is_none_or(Vec::is_empty) {
            label_width
        } else {
            row_width + separator_width + label_width
        };
        if needed > available && rows.last().is_some_and(|row| !row.is_empty()) {
            rows.push(vec![index]);
            row_width = label_width;
        } else {
            rows.last_mut()
                .expect("tab rows are never empty")
                .push(index);
            row_width = needed;
        }
    }

    let right_edge = inner.x + inner.width;
    let row_count = rows.len() as u16;
    for (row_index, indices) in rows.iter().enumerate() {
        let y = inner.y + row_index as u16;
        if y >= inner.y + inner.height {
            for &tab_index in indices {
                state.tab_rects[tab_index] = None;
            }
            break;
        }
        let mut x = inner.x + left_margin;
        for (local_index, &tab_index) in indices.iter().enumerate() {
            let remaining = right_edge.saturating_sub(x) as usize;
            if remaining == 0 {
                state.tab_rects[tab_index] = None;
                continue;
            }
            let display = &tabs[tab_index][..byte_offset_at_width(tabs[tab_index], remaining)];
            let display_width = display.width();
            let active = tab_index == state.active_tab;
            let style = if active {
                if state.tabs_focused && !embedded() {
                    Style::default()
                        .fg(theme.text_primary)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default().fg(theme.gray)
            };
            buf.set_string(x, y, display, style);
            state.tab_rects[tab_index] = Some(Rect {
                x,
                y,
                width: display_width as u16,
                height: 1,
            });
            x += display_width as u16;
            if local_index + 1 < indices.len() {
                let remaining = right_edge.saturating_sub(x) as usize;
                if remaining > 0 {
                    let display = &separator[..byte_offset_at_width(separator, remaining)];
                    buf.set_string(x, y, display, Style::default().fg(theme.gray));
                    x += display.width() as u16;
                }
            }
        }
    }
    row_count
}

#[must_use]
pub fn predict_shortcut_rows(area: Rect, sizing: &ModalSizing, shortcuts: &[Shortcut<'_>]) -> u16 {
    let (modal_width, modal_height) = compute_modal_dims(area, sizing);
    if modal_width < 20 || modal_height < 6 {
        return 0;
    }
    let inner_width = modal_width.saturating_sub(2);
    let footer_width = inner_width.saturating_sub(sizing.h_pad * 2);
    shortcuts_rows_needed(shortcuts, footer_width)
}

fn compute_modal_dims(area: Rect, sizing: &ModalSizing) -> (u16, u16) {
    let max_width = area.width.saturating_sub(4).min(sizing.max_width);
    let preferred_width = (area.width as f32 * sizing.width_pct) as u16;
    let width = preferred_width
        .min(max_width)
        .max(sizing.min_width)
        .min(area.width);
    let height = area.height.saturating_sub(sizing.v_margin * 2);
    (width, height)
}

#[must_use]
pub fn shortcuts_rows_needed(shortcuts: &[Shortcut<'_>], width: u16) -> u16 {
    if width == 0 || shortcuts.is_empty() {
        return 0;
    }
    let available = width as usize;
    let separator_width = "  |  ".width();
    let mut rows = 1u16;
    let mut row_width = 0usize;
    for shortcut in shortcuts {
        let label_width = shortcut.label.width();
        let needed = if row_width == 0 {
            label_width
        } else {
            row_width + separator_width + label_width
        };
        if needed > available && row_width > 0 {
            rows += 1;
            row_width = label_width;
        } else {
            row_width = needed;
        }
    }
    rows
}

fn split_shortcut_label(label: &str) -> (&str, &str) {
    match label.find(' ') {
        Some(index) => label.split_at(index),
        None => (label, ""),
    }
}

pub fn render_modal_shortcuts(
    buf: &mut Buffer,
    area: Rect,
    shortcuts: &[Shortcut<'_>],
    hovered: Option<usize>,
    theme: &CrabCodeTheme,
) -> Vec<ShortcutHitArea> {
    if area.width == 0 || area.height == 0 || shortcuts.is_empty() {
        return Vec::new();
    }
    let available = area.width as usize;
    let separator = "  |  ";
    let separator_width = separator.width();
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut row_width = 0usize;
    for (index, shortcut) in shortcuts.iter().enumerate() {
        let label_width = shortcut.label.width();
        let needed = if rows.last().is_none_or(Vec::is_empty) {
            label_width
        } else {
            row_width + separator_width + label_width
        };
        if needed > available && rows.last().is_some_and(|row| !row.is_empty()) {
            rows.push(vec![index]);
            row_width = label_width;
        } else {
            rows.last_mut()
                .expect("shortcut rows are never empty")
                .push(index);
            row_width = needed;
        }
    }
    rows.truncate(area.height as usize);

    let row_count = rows.len() as u16;
    let row_end = area.x + area.width;
    let mut hits = Vec::new();
    for (row_index, indices) in rows.iter().enumerate() {
        let y = area.y + area.height - row_count + row_index as u16;
        let total_width = indices
            .iter()
            .map(|&index| shortcuts[index].label.width())
            .sum::<usize>()
            + separator_width * indices.len().saturating_sub(1);
        let mut x = if total_width > available {
            area.x
        } else {
            area.x + area.width.saturating_sub(total_width as u16) / 2
        };
        for (local_index, &shortcut_index) in indices.iter().enumerate() {
            let remaining = row_end.saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let shortcut = &shortcuts[shortcut_index];
            let display = &shortcut.label[..byte_offset_at_width(shortcut.label, remaining)];
            let visible_width = display.width() as u16;
            let is_hovered = hovered == Some(shortcut_index);
            if is_hovered {
                for column in x..x + visible_width {
                    if let Some(cell) = buf.cell_mut((column, y)) {
                        cell.set_style(Style::default().bg(theme.bg_highlight));
                    }
                }
            }

            let (key_part, label_part) = split_shortcut_label(display);
            let mut key_style = Style::default()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD);
            if is_hovered {
                key_style = key_style.bg(theme.bg_highlight);
            }
            buf.set_string(x, y, key_part, key_style);
            if !label_part.is_empty() {
                let mut label_style = Style::default().fg(theme.gray);
                if is_hovered {
                    label_style = label_style.bg(theme.bg_highlight);
                }
                buf.set_string(x + key_part.width() as u16, y, label_part, label_style);
            }
            hits.push(ShortcutHitArea {
                rect: Rect {
                    x,
                    y,
                    width: visible_width,
                    height: 1,
                },
                id: shortcut.id,
                shortcuts_idx: shortcut_index,
                clickable: shortcut.clickable,
            });
            x += visible_width;
            if local_index + 1 < indices.len() {
                let remaining = row_end.saturating_sub(x) as usize;
                if remaining == 0 {
                    break;
                }
                let display = &separator[..byte_offset_at_width(separator, remaining)];
                buf.set_string(x, y, display, Style::default().fg(theme.gray_dim));
                x += display.width() as u16;
            }
        }
    }
    hits
}

#[must_use]
pub fn fit_tip_line<'a>(candidates: &[&'a str], width: usize) -> std::borrow::Cow<'a, str> {
    if width == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    for &candidate in candidates {
        if candidate.width() <= width {
            return std::borrow::Cow::Borrowed(candidate);
        }
    }
    match candidates
        .last()
        .copied()
        .filter(|candidate| !candidate.is_empty())
    {
        Some(last) => std::borrow::Cow::Owned(truncate_str(last, width)),
        None => std::borrow::Cow::Borrowed(""),
    }
}

pub fn render_centered_tip_footer(buf: &mut Buffer, area: Rect, theme: &CrabCodeTheme, text: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default()
        .fg(theme.gray_dim)
        .bg(theme.bg_base)
        .add_modifier(Modifier::ITALIC);
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let rendered = if text.width() <= area.width as usize {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(truncate_str(text, area.width as usize))
    };
    let width = (rendered.width() as u16).min(area.width);
    let start_x = area.x + area.width.saturating_sub(width) / 2;
    buf.set_span(
        start_x,
        area.y,
        &Span::styled(rendered.as_ref(), style),
        width,
    );
}

#[must_use]
pub fn split_content_for_tip_footer(content: Rect) -> (Rect, Option<Rect>) {
    if content.height < 3 {
        return (content, None);
    }
    let gap = u16::from(content.height >= 6);
    let body = Rect {
        height: content.height - 1 - gap,
        ..content
    };
    let tip = Rect {
        y: content.y + content.height - 1,
        height: 1,
        ..content
    };
    (body, Some(tip))
}

#[must_use]
pub fn footer_lines_with_tip_gap(
    area: Rect,
    sizing: &ModalSizing,
    shortcuts: &[Shortcut<'_>],
) -> u16 {
    predict_shortcut_rows(area, sizing, shortcuts)
        .saturating_add(1)
        .max(2)
}

#[must_use]
pub fn fold_indicator_span(
    collapsed: bool,
    hovered: bool,
    bg: Option<Color>,
    theme: &CrabCodeTheme,
) -> Span<'static> {
    let glyph = if collapsed {
        format!("{} ", crate::audited_glyphs::chevron())
    } else {
        format!("{} ", crate::audited_glyphs::diamond_filled())
    };
    let foreground = if hovered {
        theme.text_primary
    } else {
        theme.gray_dim
    };
    let modifier = if hovered {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let mut style = Style::default().fg(foreground).add_modifier(modifier);
    if let Some(background) = bg {
        style = style.bg(background);
    }
    Span::styled(glyph, style)
}

pub fn render_fold_indicator(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    collapsed: bool,
    hovered: bool,
    bg: Option<Color>,
    theme: &CrabCodeTheme,
) -> u16 {
    let span = fold_indicator_span(collapsed, hovered, bg, theme);
    buf.set_span(x, y, &span, 2);
    2
}

#[must_use]
pub fn handle_modal_key(
    state: &mut ModalWindowState,
    key: &KeyEvent,
    config: &ModalWindowConfig<'_>,
) -> ModalWindowOutcome {
    match key.code {
        KeyCode::Esc => ModalWindowOutcome::CloseRequested,
        KeyCode::Left | KeyCode::Char('h') => {
            if state.tabs_focused {
                ModalWindowOutcome::Unhandled
            } else if let Some(fold) = config.fold_info {
                if fold.collapsible && fold.expanded {
                    ModalWindowOutcome::CollapseGroup
                } else if fold.has_details && fold.details_expanded {
                    ModalWindowOutcome::CollapseDetails
                } else if let Some(parent) = fold.parent_index {
                    ModalWindowOutcome::JumpToParent(parent)
                } else {
                    ModalWindowOutcome::Unhandled
                }
            } else {
                ModalWindowOutcome::Unhandled
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if state.tabs_focused {
                ModalWindowOutcome::Unhandled
            } else if let Some(fold) = config.fold_info {
                if fold.collapsible && !fold.expanded {
                    ModalWindowOutcome::ExpandGroup
                } else if fold.has_details && !fold.details_expanded {
                    ModalWindowOutcome::ExpandDetails
                } else {
                    ModalWindowOutcome::Unhandled
                }
            } else {
                ModalWindowOutcome::Unhandled
            }
        }
        _ => ModalWindowOutcome::Unhandled,
    }
}

#[must_use]
pub fn handle_modal_mouse(
    state: &mut ModalWindowState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> ModalWindowOutcome {
    let in_rect = |rect: Rect| {
        column >= rect.x
            && column < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height
    };
    let on_close = state.close_button_rect.is_some_and(in_rect);
    let on_tab = state
        .tab_rects
        .iter()
        .enumerate()
        .find_map(|(index, rect)| rect.filter(|rect| in_rect(*rect)).map(|_| index));
    let on_shortcut = state
        .shortcut_hits
        .iter()
        .find(|hit| hit.clickable && in_rect(hit.rect))
        .map(|hit| hit.id);
    let shortcut_hover = state
        .shortcut_hits
        .iter()
        .find(|hit| in_rect(hit.rect))
        .map(|hit| hit.shortcuts_idx);

    match kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if on_close {
                return ModalWindowOutcome::CloseRequested;
            }
            if let Some(tab_index) = on_tab {
                if tab_index != state.active_tab {
                    state.active_tab = tab_index;
                    return ModalWindowOutcome::TabChanged(tab_index);
                }
                return ModalWindowOutcome::Handled;
            }
            if let Some(id) = on_shortcut {
                return ModalWindowOutcome::ShortcutActivated(id);
            }
            if let Some(popup) = state.popup_area
                && !in_rect(popup)
            {
                return ModalWindowOutcome::CloseRequested;
            }
            ModalWindowOutcome::Unhandled
        }
        MouseEventKind::Moved => {
            let mut changed = false;
            if state.close_hovered != on_close {
                state.close_hovered = on_close;
                changed = true;
            }
            if state.hovered_shortcut != shortcut_hover {
                state.hovered_shortcut = shortcut_hover;
                changed = true;
            }
            if on_close || shortcut_hover.is_some() || on_tab.is_some() || changed {
                ModalWindowOutcome::Handled
            } else {
                ModalWindowOutcome::Unhandled
            }
        }
        _ => ModalWindowOutcome::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton};

    fn dummy_config<'a>() -> ModalWindowConfig<'a> {
        ModalWindowConfig {
            title: "Test",
            tabs: None,
            shortcuts: &[],
            sizing: ModalSizing::default(),
            fold_info: None,
        }
    }

    fn config_with_fold<'a>(fold_info: FoldInfo) -> ModalWindowConfig<'a> {
        ModalWindowConfig {
            fold_info: Some(fold_info),
            ..dummy_config()
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn popup_state() -> ModalWindowState {
        let mut state = ModalWindowState::new();
        state.popup_area = Some(Rect::new(10, 5, 80, 30));
        state
    }

    #[test]
    fn vim_nav_search_hint_only_in_vim_nav_mode() {
        let mut nav = Vec::new();
        push_vim_nav_search_hint(&mut nav, true, false);
        assert!(nav.iter().any(|shortcut| shortcut.label == "i search"));

        let mut searching = Vec::new();
        push_vim_nav_search_hint(&mut searching, true, true);
        assert!(searching.is_empty());

        let mut disabled = Vec::new();
        push_vim_nav_search_hint(&mut disabled, false, false);
        assert!(disabled.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn embedded_fills_area_without_centering() {
        let theme = CrabCodeTheme::current();
        let area = Rect::new(0, 0, 80, 24);
        let config = dummy_config();

        set_embedded(false);
        let mut buf = Buffer::empty(area);
        let mut state = ModalWindowState::new();
        let centered = render_modal_window(&mut buf, area, &mut state, &config, &theme)
            .expect("centered modal renders");
        assert_ne!(state.popup_area, Some(area));

        set_embedded(true);
        let mut buf = Buffer::empty(area);
        let mut state = ModalWindowState::new();
        let embedded_area = render_modal_window(&mut buf, area, &mut state, &config, &theme)
            .expect("embedded modal renders");
        assert_eq!(state.popup_area, Some(area));
        assert!(embedded_area.content.width > centered.content.width);
        assert_eq!(
            state.close_button_rect, None,
            "embedded chrome has no stale close hit target"
        );
        set_embedded(false);
    }

    #[test]
    fn centered_tip_footer_centers_and_clips() {
        let theme = CrabCodeTheme::current();
        const TEXT: &str = "Tip · Read the local documentation";
        let row_text = |width: u16| {
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            render_centered_tip_footer(&mut buf, area, &theme, TEXT);
            (0..width)
                .filter_map(|x| buf.cell((x, 0)).map(|cell| cell.symbol().to_string()))
                .collect::<String>()
        };

        let wide = row_text(80);
        let start = wide.find("Tip").expect("tip text");
        let trailing = wide
            .chars()
            .rev()
            .take_while(|character| *character == ' ')
            .count();
        assert!(start.abs_diff(trailing) <= 1);
        assert!(wide.contains("Read the local documentation"));

        let tiny = row_text(10);
        assert!(tiny.contains("Tip"));
        assert!(tiny.trim_end().chars().count() <= 10);
    }

    #[test]
    fn split_content_for_tip_footer_thresholds() {
        let (body, tip) = split_content_for_tip_footer(Rect::new(2, 3, 40, 8));
        assert_eq!(body.height, 6);
        assert_eq!(tip.expect("tip").y, 10);

        let (body, tip) = split_content_for_tip_footer(Rect::new(0, 0, 20, 4));
        assert_eq!(body.height, 3);
        assert!(tip.is_some());

        let tiny = Rect::new(0, 0, 20, 2);
        let (body, tip) = split_content_for_tip_footer(tiny);
        assert_eq!(body, tiny);
        assert!(tip.is_none());
    }

    #[test]
    fn fit_tip_line_picks_first_fit_then_truncates() {
        assert_eq!(fit_tip_line(&["abcdef", "xy"], 10).as_ref(), "abcdef");
        assert_eq!(fit_tip_line(&["abcdef", "xy"], 4).as_ref(), "xy");
        assert!(fit_tip_line(&["abcdef", "xy"], 1).as_ref().width() <= 1);
    }

    #[test]
    fn modal_sizing_with_compact_reduces_margins_aggressively() {
        let base = ModalSizing {
            width_pct: 0.8,
            max_width: 120,
            min_width: 50,
            v_margin: 5,
            h_pad: 3,
            v_pad: 2,
            footer_lines: 2,
        };
        let compact = base.with_compact(true);
        assert_eq!(compact.v_margin, 0);
        assert_eq!(compact.h_pad, 1);
        assert_eq!(compact.v_pad, 0);
        assert_eq!(compact.width_pct, 0.8);
        assert_eq!(compact.max_width, 120);

        let unchanged = base.with_compact(false);
        assert_eq!(unchanged.v_margin, 5);
        assert_eq!(unchanged.h_pad, 3);
        assert_eq!(unchanged.v_pad, 2);
    }

    #[test]
    fn modal_width_never_exceeds_narrow_terminal() {
        for sizing in [ModalSizing::medium(), ModalSizing::large()] {
            for width in 0..=70 {
                let (modal_width, _) = compute_modal_dims(Rect::new(0, 0, width, 85), &sizing);
                assert!(modal_width <= width);
            }
        }
    }

    #[test]
    fn new_defaults() {
        let state = ModalWindowState::new();
        assert!(!state.close_hovered);
        assert_eq!(state.close_button_rect, None);
        assert_eq!(state.popup_area, None);
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.tab_count, 0);
        assert!(state.tab_rects.is_empty());
        assert!(state.shortcut_hits.is_empty());
        assert_eq!(state.hovered_shortcut, None);
    }

    #[test]
    fn with_tabs_initialises_rects() {
        let state = ModalWindowState::with_tabs(3);
        assert_eq!(state.tab_count, 3);
        assert_eq!(state.tab_rects.len(), 3);
        assert!(state.tab_rects.iter().all(Option::is_none));
    }

    #[test]
    fn default_matches_new() {
        let new = ModalWindowState::new();
        let default = ModalWindowState::default();
        assert_eq!(new.tab_count, default.tab_count);
        assert_eq!(new.close_hovered, default.close_hovered);
    }

    #[test]
    fn key_esc_returns_close_requested() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Esc),
                &dummy_config()
            ),
            ModalWindowOutcome::CloseRequested
        );
    }

    #[test]
    fn key_other_returns_unhandled() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Char('j')),
                &dummy_config()
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn key_left_without_fold_info_returns_unhandled() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Left),
                &dummy_config()
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn key_h_without_fold_info_returns_unhandled() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Char('h')),
                &dummy_config()
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn key_right_without_fold_info_returns_unhandled() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Right),
                &dummy_config()
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn key_l_without_fold_info_returns_unhandled() {
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Char('l')),
                &dummy_config()
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn left_on_expanded_collapsible_returns_collapse_group() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::CollapseGroup
        );
    }

    #[test]
    fn right_on_collapsed_collapsible_returns_expand_group() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::ExpandGroup
        );
    }

    #[test]
    fn left_on_collapsed_collapsible_with_parent_returns_jump() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: Some(3),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::JumpToParent(3)
        );
    }

    #[test]
    fn left_on_collapsed_collapsible_without_parent_returns_unhandled() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn left_on_expanded_details_returns_collapse_details() {
        let config = config_with_fold(FoldInfo {
            collapsible: false,
            expanded: false,
            has_details: true,
            details_expanded: true,
            parent_index: Some(0),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::CollapseDetails
        );
    }

    #[test]
    fn right_on_collapsed_details_returns_expand_details() {
        let config = config_with_fold(FoldInfo {
            collapsible: false,
            expanded: false,
            has_details: true,
            details_expanded: false,
            parent_index: Some(0),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::ExpandDetails
        );
    }

    #[test]
    fn left_on_leaf_without_details_returns_jump_to_parent() {
        let config = config_with_fold(FoldInfo {
            collapsible: false,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: Some(5),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::JumpToParent(5)
        );
    }

    #[test]
    fn right_on_expanded_collapsible_returns_unhandled() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn h_key_uses_fold_info_same_as_left() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Char('h')),
                &config
            ),
            ModalWindowOutcome::CollapseGroup
        );
    }

    #[test]
    fn l_key_uses_fold_info_same_as_right() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(
                &mut ModalWindowState::new(),
                &key(KeyCode::Char('l')),
                &config
            ),
            ModalWindowOutcome::ExpandGroup
        );
    }

    #[test]
    fn left_collapse_group_wins_over_collapse_details() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: true,
            details_expanded: true,
            parent_index: Some(0),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::CollapseGroup
        );
    }

    #[test]
    fn right_expand_group_wins_over_expand_details() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: false,
            has_details: true,
            details_expanded: false,
            parent_index: Some(0),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::ExpandGroup
        );
    }

    #[test]
    fn right_on_expanded_collapsible_with_unexpanded_details_returns_expand_details() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: true,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::ExpandDetails
        );
    }

    #[test]
    fn left_on_bare_leaf_no_parent_returns_unhandled() {
        let config = config_with_fold(FoldInfo {
            collapsible: false,
            expanded: false,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Left), &config),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn right_on_fully_expanded_details_returns_unhandled() {
        let config = config_with_fold(FoldInfo {
            collapsible: false,
            expanded: false,
            has_details: true,
            details_expanded: true,
            parent_index: Some(0),
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Right), &config),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn esc_with_fold_info_returns_close_requested() {
        let config = config_with_fold(FoldInfo {
            collapsible: true,
            expanded: true,
            has_details: false,
            details_expanded: false,
            parent_index: None,
        });
        assert_eq!(
            handle_modal_key(&mut ModalWindowState::new(), &key(KeyCode::Esc), &config),
            ModalWindowOutcome::CloseRequested
        );
    }

    #[test]
    fn click_on_close_button_returns_close_requested() {
        let mut state = popup_state();
        state.close_button_rect = Some(Rect::new(85, 5, 5, 1));
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 87, 5),
            ModalWindowOutcome::CloseRequested
        );
    }

    #[test]
    fn click_outside_popup_returns_close_requested() {
        assert_eq!(
            handle_modal_mouse(
                &mut popup_state(),
                MouseEventKind::Down(MouseButton::Left),
                5,
                5
            ),
            ModalWindowOutcome::CloseRequested
        );
    }

    #[test]
    fn click_inside_popup_no_chrome_returns_unhandled() {
        assert_eq!(
            handle_modal_mouse(
                &mut popup_state(),
                MouseEventKind::Down(MouseButton::Left),
                50,
                20
            ),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn click_on_shortcut_returns_shortcut_activated() {
        let mut state = popup_state();
        state.shortcut_hits = vec![ShortcutHitArea {
            rect: Rect::new(30, 32, 10, 1),
            id: 42,
            shortcuts_idx: 3,
            clickable: true,
        }];
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 35, 32),
            ModalWindowOutcome::ShortcutActivated(42)
        );
    }

    #[test]
    fn hover_over_close_sets_hovered_and_returns_handled() {
        let mut state = popup_state();
        state.close_button_rect = Some(Rect::new(85, 5, 5, 1));
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Moved, 87, 5),
            ModalWindowOutcome::Handled
        );
        assert!(state.close_hovered);
    }

    #[test]
    fn hover_leaving_shortcut_returns_handled_for_redraw() {
        let mut state = popup_state();
        state.shortcut_hits = vec![ShortcutHitArea {
            rect: Rect::new(30, 32, 10, 1),
            id: 1,
            shortcuts_idx: 4,
            clickable: true,
        }];
        state.hovered_shortcut = Some(4);
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Moved, 50, 20),
            ModalWindowOutcome::Handled
        );
        assert_eq!(state.hovered_shortcut, None);
    }

    #[test]
    fn hover_no_change_returns_unhandled() {
        assert_eq!(
            handle_modal_mouse(&mut popup_state(), MouseEventKind::Moved, 50, 20),
            ModalWindowOutcome::Unhandled
        );
    }

    #[test]
    fn hover_shortcut_uses_shortcuts_idx_not_position_in_hits() {
        let mut state = popup_state();
        state.shortcut_hits = vec![ShortcutHitArea {
            rect: Rect::new(40, 32, 8, 1),
            id: 99,
            shortcuts_idx: 3,
            clickable: true,
        }];
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Moved, 44, 32),
            ModalWindowOutcome::Handled
        );
        assert_eq!(state.hovered_shortcut, Some(3));
    }

    #[test]
    fn modal_sizing_medium_has_expected_values() {
        let medium = ModalSizing::medium();
        assert_eq!(medium.width_pct, 0.60);
        assert_eq!(medium.max_width, 120);
        assert_eq!(medium.min_width, 44);
        assert_eq!(medium.v_margin, 4);
        assert_eq!(medium.h_pad, 2);
        assert_eq!(medium.v_pad, 1);
        assert_eq!(medium.footer_lines, 2);
    }

    #[test]
    fn modal_sizing_large_matches_default() {
        assert_eq!(ModalSizing::large(), ModalSizing::default());
    }

    #[test]
    fn split_shortcut_label_basic_ascii() {
        assert_eq!(split_shortcut_label("Esc cancel"), ("Esc", " cancel"));
        assert_eq!(split_shortcut_label("Enter select"), ("Enter", " select"));
    }

    #[test]
    fn split_shortcut_label_multi_word_label() {
        assert_eq!(
            split_shortcut_label("Enter import 3"),
            ("Enter", " import 3")
        );
        assert_eq!(
            split_shortcut_label("x confirm delete"),
            ("x", " confirm delete")
        );
    }

    #[test]
    fn split_shortcut_label_unicode_arrows() {
        assert_eq!(
            split_shortcut_label("\u{2191}/\u{2193} nav"),
            ("\u{2191}/\u{2193}", " nav")
        );
        assert_eq!(
            split_shortcut_label("\u{2191}\u{2193} nav"),
            ("\u{2191}\u{2193}", " nav")
        );
    }

    #[test]
    fn split_shortcut_label_no_whitespace_is_key_only() {
        assert_eq!(split_shortcut_label("Esc"), ("Esc", ""));
        assert_eq!(split_shortcut_label(""), ("", ""));
    }

    #[test]
    fn split_shortcut_label_only_splits_on_ascii_space() {
        assert_eq!(split_shortcut_label("Esc\tcancel"), ("Esc\tcancel", ""));
        assert_eq!(
            split_shortcut_label("Esc\u{00A0}cancel"),
            ("Esc\u{00A0}cancel", "")
        );
        assert_eq!(
            split_shortcut_label("Esc cancel\twith\ttabs"),
            ("Esc", " cancel\twith\ttabs")
        );
    }
}
