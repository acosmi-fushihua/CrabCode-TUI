//! Narrow CrabCode adapters shared by generic picker product surfaces.
//!
//! The full fixed-source picker lifecycle lives in `picker_surface`; this
//! module contains only the canonical single-line editor and product layout /
//! filtering adapters that are not part of the upstream picker state machine.

use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionGeometry {
    pub(crate) panel: Rect,
    pub(crate) search: Option<Rect>,
    pub(crate) list: Rect,
    pub(crate) footer: Option<Rect>,
    pub(crate) close_button: Rect,
}

/// Resolve a centered product surface without re-inflating past the terminal.
pub(crate) fn centered_selection_geometry(
    area: Rect,
    width_percent: u16,
    max_width: u16,
    desired_height: u16,
    search: bool,
    footer: bool,
) -> SelectionGeometry {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(1)
        .min(max_width)
        .min(area.width);
    let height = desired_height.max(1).min(area.height);
    let panel = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    let search_rows = u16::from(search && inner.height > 0);
    let footer_rows = u16::from(footer && inner.height > search_rows);
    let search_area = (search_rows == 1).then(|| Rect::new(inner.x, inner.y, inner.width, 1));
    let footer_area = (footer_rows == 1).then(|| {
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        )
    });
    let list = Rect::new(
        inner.x,
        inner.y.saturating_add(search_rows),
        inner.width,
        inner
            .height
            .saturating_sub(search_rows.saturating_add(footer_rows)),
    );
    let close_button = Rect::new(
        panel.x + panel.width.saturating_sub(4),
        panel.y,
        3.min(panel.width),
        u16::from(panel.height > 0),
    );
    SelectionGeometry {
        panel,
        search: search_area,
        list,
        footer: footer_area,
        close_button,
    }
}

pub(crate) fn filter_indices<'a>(
    query: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> Vec<usize> {
    let query = query.to_lowercase();
    values
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| value.to_lowercase().contains(&query).then_some(index))
        .collect()
}

#[cfg(test)]
mod tests {
    use crabcode_pager_render::picker_line_editor::{LineEditOutcome, LineEditor};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn query_editor_paste_and_viewport_preserve_graphemes() {
        let grapheme = "👩🏽\u{200d}💻";
        let mut editor = LineEditor::default();
        editor.set_text(format!("a{grapheme}b"));
        assert_eq!(
            editor.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            LineEditOutcome::CursorChanged
        );
        let viewport = editor.viewport(3);
        assert_eq!(
            &editor.text()[viewport.visible_byte_range],
            format!("{grapheme}b")
        );
        assert_eq!(viewport.cursor_display_column, 2);
        assert_eq!(
            editor.insert_paste("\r\n中\n"),
            LineEditOutcome::TextChanged
        );
        assert!(!editor.text().contains(['\r', '\n']));
    }

    #[test]
    fn geometry_clamps_to_tiny_terminal() {
        let terminal = Rect::new(7, 3, 8, 4);
        let geometry = centered_selection_geometry(terminal, 86, 82, 20, true, true);
        assert!(terminal.contains(geometry.panel.as_position()));
        assert!(geometry.panel.width <= terminal.width);
        assert!(geometry.panel.height <= terminal.height);
    }

    #[test]
    fn filter_is_unicode_case_insensitive_without_byte_slicing() {
        let values = ["Alpha", "中 文", "CrabCode"];
        assert_eq!(filter_indices("alpha", values), vec![0]);
        assert_eq!(filter_indices("中", values), vec![1]);
    }
}
