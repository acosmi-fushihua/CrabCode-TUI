//! Viewport-clipped scrollback selection border.
//!
//! This contains the dependency-closed `SelectionBox`, `ScrollInfo`, and
//! `RenderOutput` value layer adapted from the fixed upstream module. The
//! complete `ScrollbackState`/`ScrollbackPane` graph is present in this crate;
//! production ownership remains open until that graph enters the sole
//! `AppView` → `AgentView` route and replaces the current transcript route
//! atomically.
//!
//! Source lineage:
//! - repository commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - monorepo source revision: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//! - source path:
//!   `crates/codegen/xai-grok-pager/src/scrollback/selection.rs`
//! - source SHA-256:
//!   `508c5788029c2d3bd735d7cd9f6e585355648a9bf5b0eb17138eea178f01ab57`

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::audited_theme::CrabCodeTheme;
use crate::render::SafeBuf;
use crate::render::osc8::LinkOverlay;
use crate::scrollback::render::{DiagramAffordancePlacement, InlineMediaPlacement};
use crate::scrollback::text_selection::ResolvedSelectionModel;

mod border_chars {
    pub const TOP_LEFT: char = '┌';
    pub const TOP_RIGHT: char = '┐';
    pub const BOTTOM_LEFT: char = '└';
    pub const BOTTOM_RIGHT: char = '┘';
    pub const VERTICAL: char = '│';
    pub const VERTICAL_DASHED: char = '┆';
}

/// A border drawn around the visible portion of one selected scrollback
/// entry. Clipped edges use dashed sides and omit their corners.
#[derive(Debug, Clone)]
pub struct SelectionBox {
    pub inner_area: Rect,
    pub top_clipped: bool,
    pub bottom_clipped: bool,
    pub style: Style,
    pub closable: bool,
    pub close_hovered: bool,
    pub close_label: Option<&'static str>,
}

/// Post-render output owned by the scrollback pane lifecycle.
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub selection_box: Option<SelectionBox>,
    pub scroll_info: Option<ScrollInfo>,
    pub selected_entry_area: Option<Rect>,
    pub selection_model: ResolvedSelectionModel,
    pub link_overlay: LinkOverlay,
    pub inline_media: Vec<InlineMediaPlacement>,
    pub diagram_affordances: Vec<DiagramAffordancePlacement>,
}

/// Scrollbar inputs produced by the fixed pane render pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollInfo {
    pub scroll_offset: usize,
    pub viewport_height: u16,
    pub total_height: usize,
}

impl RenderOutput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_selection_box(selection_box: SelectionBox) -> Self {
        Self {
            selection_box: Some(selection_box),
            scroll_info: None,
            selected_entry_area: None,
            selection_model: ResolvedSelectionModel::default(),
            link_overlay: LinkOverlay::default(),
            inline_media: Vec::new(),
            diagram_affordances: Vec::new(),
        }
    }

    pub fn with_scroll_info(mut self, scroll_info: ScrollInfo) -> Self {
        self.scroll_info = Some(scroll_info);
        self
    }
}

impl SelectionBox {
    pub fn new(inner_area: Rect, style: Style) -> Self {
        Self {
            inner_area,
            top_clipped: false,
            bottom_clipped: false,
            style,
            closable: false,
            close_hovered: false,
            close_label: None,
        }
    }

    pub fn with_top_clipped(mut self, clipped: bool) -> Self {
        self.top_clipped = clipped;
        self
    }

    pub fn with_bottom_clipped(mut self, clipped: bool) -> Self {
        self.bottom_clipped = clipped;
        self
    }

    pub fn with_closable(mut self, closable: bool, hovered: bool) -> Self {
        self.closable = closable;
        self.close_hovered = hovered;
        self
    }

    pub fn with_close_label(mut self, label: Option<&'static str>) -> Self {
        self.close_label = label;
        if label.is_some() {
            self.closable = true;
        }
        self
    }

    pub fn close_button_rect(&self) -> Option<Rect> {
        if !self.closable || self.top_clipped || self.inner_area.y == 0 {
            return None;
        }
        let label_width = self
            .close_label
            .map(|label| label.chars().count() as u16)
            .unwrap_or(1)
            .max(1);
        let right = self
            .inner_area
            .x
            .saturating_add(self.inner_area.width.saturating_sub(1));
        Some(Rect {
            x: right.saturating_sub(label_width.saturating_sub(1)),
            y: self.inner_area.y - 1,
            width: label_width,
            height: 1,
        })
    }

    pub fn render(&self, buffer: &mut Buffer) {
        let area = self.inner_area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let left = area.x;
        let right = area.x.saturating_add(area.width.saturating_sub(1));
        let top = area.y;
        let bottom = area.y.saturating_add(area.height.saturating_sub(1));

        for row in top..=bottom {
            let first = row == top;
            let last = row == bottom;
            let dashed = (first && self.top_clipped) || (last && self.bottom_clipped);
            let symbol = if dashed {
                border_chars::VERTICAL_DASHED
            } else {
                border_chars::VERTICAL
            };
            if let Some(cell) = buffer.cell_mut((left, row)) {
                cell.set_char(symbol).set_style(self.style);
            }
            if let Some(cell) = buffer.cell_mut((right, row)) {
                cell.set_char(symbol).set_style(self.style);
            }
        }

        if !self.top_clipped && top > 0 {
            let corner_row = top - 1;
            if let Some(cell) = buffer.cell_mut((left, corner_row)) {
                cell.set_char(border_chars::TOP_LEFT).set_style(self.style);
            }
            if let Some(close) = self.close_button_rect() {
                let style = if self.close_hovered {
                    Style::default().fg(CrabCodeTheme::current().text_primary)
                } else {
                    self.style
                };
                if let Some(label) = self.close_label {
                    buffer.set_string_safe(close.x, close.y, label, style);
                } else if let Some(cell) = buffer.cell_mut((close.x, close.y)) {
                    cell.set_symbol(crate::audited_glyphs::ballot_x())
                        .set_style(style);
                }
            } else if let Some(cell) = buffer.cell_mut((right, corner_row)) {
                cell.set_char(border_chars::TOP_RIGHT).set_style(self.style);
            }
        }

        if !self.bottom_clipped {
            let corner_row = bottom.saturating_add(1);
            if let Some(cell) = buffer.cell_mut((left, corner_row)) {
                cell.set_char(border_chars::BOTTOM_LEFT)
                    .set_style(self.style);
            }
            if let Some(cell) = buffer.cell_mut((right, corner_row)) {
                cell.set_char(border_chars::BOTTOM_RIGHT)
                    .set_style(self.style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_unclipped_sides_and_corners() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        SelectionBox::new(Rect::new(0, 2, 10, 4), Style::default()).render(&mut buffer);

        assert_eq!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(9, 1)].symbol(), "┐");
        for row in 2..=5 {
            assert_eq!(buffer[(0, row)].symbol(), "│");
            assert_eq!(buffer[(9, row)].symbol(), "│");
        }
        assert_eq!(buffer[(0, 6)].symbol(), "└");
        assert_eq!(buffer[(9, 6)].symbol(), "┘");
    }

    #[test]
    fn clipped_edges_use_dashed_sides_without_corners() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        SelectionBox::new(Rect::new(0, 2, 10, 4), Style::default())
            .with_top_clipped(true)
            .with_bottom_clipped(true)
            .render(&mut buffer);

        assert_ne!(buffer[(0, 1)].symbol(), "┌");
        assert_eq!(buffer[(0, 2)].symbol(), "┆");
        assert_eq!(buffer[(9, 2)].symbol(), "┆");
        assert_eq!(buffer[(0, 3)].symbol(), "│");
        assert_eq!(buffer[(9, 4)].symbol(), "│");
        assert_eq!(buffer[(0, 5)].symbol(), "┆");
        assert_eq!(buffer[(9, 5)].symbol(), "┆");
        assert_ne!(buffer[(0, 6)].symbol(), "└");
    }

    #[test]
    fn single_visible_row_honors_either_clipped_edge() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        SelectionBox::new(Rect::new(0, 3, 10, 1), Style::default())
            .with_top_clipped(true)
            .render(&mut buffer);

        assert_eq!(buffer[(0, 3)].symbol(), "┆");
        assert_eq!(buffer[(9, 3)].symbol(), "┆");
        assert_eq!(buffer[(0, 4)].symbol(), "└");
        assert_eq!(buffer[(9, 4)].symbol(), "┘");
    }

    #[test]
    fn render_output_builder_preserves_post_render_channels() {
        let selection = SelectionBox::new(Rect::new(1, 2, 8, 3), Style::default());
        let scroll = ScrollInfo {
            scroll_offset: usize::from(u16::MAX) + 9,
            viewport_height: 20,
            total_height: usize::from(u16::MAX) + 200,
        };
        let output = RenderOutput::with_selection_box(selection).with_scroll_info(scroll);

        assert!(output.selection_box.is_some());
        assert_eq!(output.scroll_info, Some(scroll));
        assert!(output.inline_media.is_empty());
        assert!(output.diagram_affordances.is_empty());
    }
}
