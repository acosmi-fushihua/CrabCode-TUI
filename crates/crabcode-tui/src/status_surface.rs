//! Shared one-row status rendering for the direct terminal application.
//!
//! The two widget implementations are source ports from the fixed Rust
//! renderer.  The adapter below them only clips read-only CrabCode
//! presentation strings to the available terminal cells.

use std::cell::Cell;
use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::tui_render::{CrabCodeTheme, fit_line_to_width, truncate_str};

const SEPARATOR: &str = "│";

/// The fixed widgets only consume these three palette values.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    bg_base: Color,
    gray: Color,
    gray_dim: Color,
}

impl From<CrabCodeTheme> for Theme {
    fn from(theme: CrabCodeTheme) -> Self {
        Self {
            bg_base: theme.bg_base,
            gray: theme.gray,
            gray_dim: theme.gray_dim,
        }
    }
}

impl Theme {
    fn current() -> Self {
        STATUS_THEME_OVERRIDE
            .with(Cell::get)
            .unwrap_or_else(|| CrabCodeTheme::current().into())
    }
}

thread_local! {
    /// Adapter-only override for the duration of one status-row paint.
    ///
    /// The fixed widget body remains byte-for-byte source-identical to its
    /// mother implementation (`Theme::current()`); the CrabCode render tree
    /// still supplies the concrete palette selected for this frame.
    static STATUS_THEME_OVERRIDE: Cell<Option<Theme>> = const { Cell::new(None) };
}

fn with_status_theme<T>(theme: Theme, paint: impl FnOnce() -> T) -> T {
    STATUS_THEME_OVERRIDE.with(|slot| {
        let previous = slot.replace(Some(theme));
        let output = paint();
        slot.set(previous);
        output
    })
}

/// Status bar showing context information.
///
/// Displays: token count, current turn, view mode, etc.
/// Respects layout: first 3 cols and last 2 cols are empty.
pub struct StatusBar<'a> {
    /// Left-aligned content (e.g., "Context: 5.2k tokens")
    pub left: &'a str,
    /// Center content (e.g., "Turn 2/3")
    pub center: Option<&'a str>,
    /// Right-aligned content (e.g., view mode indicator)
    pub right: Option<&'a str>,
}

impl<'a> StatusBar<'a> {
    /// Create a new status bar with left content.
    pub fn new(left: &'a str) -> Self {
        Self {
            left,
            center: None,
            right: None,
        }
    }

    /// Add center content.
    #[allow(dead_code)]
    pub fn center(mut self, text: &'a str) -> Self {
        self.center = Some(text);
        self
    }

    /// Add right content.
    #[allow(dead_code)]
    pub fn right(mut self, text: &'a str) -> Self {
        self.right = Some(text);
        self
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let theme = Theme::current();

        // Layout: outer block already has 2-char horizontal padding
        // No additional margins needed
        let left_margin = 0u16;
        let right_margin = 0u16;
        let content_x = area.x + left_margin;
        let content_width = area.width.saturating_sub(left_margin + right_margin);

        if content_width < 10 {
            return;
        }

        let style = Style::default().fg(theme.gray).bg(theme.bg_base);

        // Fill background (the whole row)
        buf.set_style(area, Style::default().bg(theme.bg_base));

        // Left content
        let left_span = Span::styled(self.left, style);
        buf.set_span(content_x, area.y, &left_span, content_width);

        // Center content (if fits)
        if let Some(center) = self.center {
            let center_width = center.len() as u16;
            let center_x = content_x + (content_width.saturating_sub(center_width)) / 2;
            if center_x > content_x + self.left.len() as u16 + 2 {
                let center_span = Span::styled(center, style);
                buf.set_span(center_x, area.y, &center_span, center_width);
            }
        }

        // Right content
        if let Some(right) = self.right {
            let right_width = right.len() as u16;
            let right_x = content_x + content_width.saturating_sub(right_width);
            let right_span = Span::styled(right, style);
            buf.set_span(right_x, area.y, &right_span, right_width);
        }
    }
}

/// A named status bar item.
struct StatusEntry {
    /// Identifier for hit-test lookup (e.g., "context", "badge").
    id: &'static str,
    /// Pre-built styled content.
    line: Line<'static>,
    /// Display width in columns.
    width: u16,
}

/// Builder for the agent status bar.
///
/// Collect items with [`push`], then call [`render`] to lay them out
/// right-aligned with separators and get back hit-test areas.
pub struct AgentStatusBar<'a> {
    items: Vec<StatusEntry>,
    theme: &'a Theme,
    /// Padding from the right edge of the status bar area.
    right_pad: u16,
}

impl<'a> AgentStatusBar<'a> {
    /// Create a new empty status bar.
    pub fn new(theme: &'a Theme) -> Self {
        Self {
            items: Vec::new(),
            theme,
            right_pad: 0,
        }
    }

    /// Add an item to the status bar.
    ///
    /// Items are rendered left-to-right in push order, but the entire
    /// group is right-aligned within the status bar area.
    pub fn push(&mut self, id: &'static str, line: Line<'static>) {
        let width = line.width() as u16;
        self.items.push(StatusEntry { id, line, width });
    }

    /// Build a separator span: ` │ ` in dim color.
    fn separator(&self) -> Span<'static> {
        Span::styled(
            format!(" {SEPARATOR} "),
            Style::default()
                .fg(self.theme.gray_dim)
                .bg(self.theme.bg_base),
        )
    }

    /// Render all items right-aligned into the given area.
    ///
    /// Layout: `··· item0 │ item1 │ item2` — separators appear only *between*
    /// items, never before the first or after the last.
    ///
    /// Returns a map of item ID → screen `Rect` for hit-testing.
    pub fn render(self, buf: &mut Buffer, area: Rect) -> HashMap<&'static str, Rect> {
        if area.height == 0 || area.width == 0 || self.items.is_empty() {
            return HashMap::new();
        }

        // Fill background
        buf.set_style(area, Style::default().bg(self.theme.bg_base));

        let sep = self.separator();
        let sep_w = sep.width() as u16; // 3

        // Total width: items plus the separators *between* them only — no
        // leading separator before the first item or trailing one after the
        // last.
        let items_width: u16 = self.items.iter().map(|e| e.width).sum();
        let num_seps = (self.items.len() as u16).saturating_sub(1);
        let total_width = items_width + num_seps * sep_w;

        // Right-align: compute starting x
        let start_x = area
            .x
            .saturating_add(area.width.saturating_sub(self.right_pad + total_width));

        let mut x = start_x;
        let mut areas = HashMap::new();

        for (i, entry) in self.items.iter().enumerate() {
            // Separator before every item except the first.
            if i > 0 {
                buf.set_span(x, area.y, &sep, sep_w);
                x += sep_w;
            }

            // Render item
            buf.set_line(x, area.y, &entry.line, entry.width);
            areas.insert(
                entry.id,
                Rect {
                    x,
                    y: area.y,
                    width: entry.width,
                    height: 1,
                },
            );
            x += entry.width;
        }

        areas
    }
}

/// One read-only item supplied by the CrabCode presentation adapter.
pub(crate) struct StatusItem {
    pub(crate) id: &'static str,
    pub(crate) line: Line<'static>,
}

fn group_width(items: &[StatusItem]) -> u16 {
    let item_width = items
        .iter()
        .map(|item| u16::try_from(item.line.width()).unwrap_or(u16::MAX))
        .fold(0_u16, u16::saturating_add);
    let separators = u16::try_from(items.len().saturating_sub(1))
        .unwrap_or(u16::MAX)
        .saturating_mul(3);
    item_width.saturating_add(separators)
}

fn fit_items(items: Vec<StatusItem>, budget: u16) -> Vec<StatusItem> {
    let mut remaining = budget;
    let mut fitted = Vec::new();
    for item in items {
        let separator = u16::from(!fitted.is_empty()).saturating_mul(3);
        if remaining <= separator {
            break;
        }
        remaining = remaining.saturating_sub(separator);
        let width = u16::try_from(item.line.width())
            .unwrap_or(u16::MAX)
            .min(remaining);
        if width == 0 {
            break;
        }
        let line = if item.line.width() > usize::from(width) {
            fit_line_to_width(item.line, usize::from(width))
        } else {
            item.line
        };
        fitted.push(StatusItem { id: item.id, line });
        remaining = remaining.saturating_sub(width);
    }
    fitted
}

/// Render one production status row without allowing either group to cross
/// the terminal rectangle. The fixed widgets still own paint and hit areas;
/// this adapter only applies a grapheme-safe width budget beforehand.
pub(crate) fn render_status_row(
    buf: &mut Buffer,
    area: Rect,
    theme: CrabCodeTheme,
    left: &str,
    items: Vec<StatusItem>,
) -> HashMap<&'static str, Rect> {
    if area.height == 0 || area.width == 0 {
        return HashMap::new();
    }
    buf.set_style(area, Style::default().bg(theme.bg_base));

    let desired_right = group_width(&items);
    let minimum_left = if area.width >= 10 {
        10.min(area.width)
    } else {
        0
    };
    let gap = u16::from(desired_right > 0 && minimum_left > 0).saturating_mul(2);
    let right_budget = if desired_right
        .saturating_add(gap)
        .saturating_add(minimum_left)
        <= area.width
    {
        desired_right
    } else {
        area.width.saturating_sub(minimum_left + gap)
    };
    let fitted = fit_items(items, right_budget);
    let fitted_width = group_width(&fitted);
    let left_budget = area
        .width
        .saturating_sub(fitted_width)
        .saturating_sub(u16::from(fitted_width > 0).saturating_mul(2));
    let left = truncate_str(left, usize::from(left_budget));
    let palette = Theme::from(theme);
    with_status_theme(palette, || StatusBar::new(&left).render(area, buf));

    let mut status = AgentStatusBar::new(&palette);
    for item in fitted {
        status.push(item.id, item.line);
    }
    status.render(buf, area)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(buf: &Buffer, area: Rect) -> String {
        (area.x..area.right())
            .map(|x| buf[(x, area.y)].symbol())
            .collect()
    }

    #[test]
    fn status_bar_separators_only_between_items() {
        let theme = Theme::current();
        let mut bar = AgentStatusBar::new(&theme);
        bar.push("a", Line::from("AA"));
        bar.push("b", Line::from("BB"));
        bar.push("c", Line::from("CC"));

        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let areas = bar.render(&mut buf, area);
        let trimmed = row(&buf, area).trim().to_string();

        assert_eq!(trimmed, format!("AA {SEPARATOR} BB {SEPARATOR} CC"));
        assert_eq!(areas["a"], Rect::new(28, 0, 2, 1));
        assert_eq!(areas["c"], Rect::new(38, 0, 2, 1));
    }

    #[test]
    fn status_bar_single_item_has_no_separators() {
        let theme = Theme::current();
        let mut bar = AgentStatusBar::new(&theme);
        bar.push("only", Line::from("XX"));

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        bar.render(&mut buf, area);

        let rendered = row(&buf, area);
        assert_eq!(rendered.trim(), "XX");
        assert!(!rendered.contains(SEPARATOR));
    }

    #[test]
    fn production_adapter_clips_unicode_and_hit_areas_to_the_row() {
        let area = Rect::new(5, 3, 24, 1);
        let mut buf = Buffer::empty(area);
        let items = vec![
            StatusItem {
                id: "scroll",
                line: Line::from("scroll paused"),
            },
            StatusItem {
                id: "suggestion",
                line: Line::from("Tab: 👩‍💻e\u{301}非常长的建议"),
            },
        ];
        let areas = render_status_row(
            &mut buf,
            area,
            CrabCodeTheme::NIGHT,
            "处理中：组合字符e\u{301}与宽字符",
            items,
        );

        assert!(row(&buf, area).contains("scroll"));
        assert!(
            areas
                .values()
                .all(|hit| hit.x >= area.x && hit.right() <= area.right())
        );
        assert_eq!(
            row(&buf, area)
                .chars()
                .filter(|ch| *ch == '\u{301}')
                .count(),
            0
        );
    }

    #[test]
    fn zero_sized_rows_do_not_paint_or_publish_hits() {
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(area);
        assert!(
            render_status_row(&mut buf, area, CrabCodeTheme::NIGHT, "status", Vec::new(),)
                .is_empty()
        );
    }
}
