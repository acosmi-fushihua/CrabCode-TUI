//! CrabCode product adapters for the complete shared picker mother.
//!
//! Generic state, input and rendering live in
//! `crabcode_pager_render::picker`. This module only supplies conveniences
//! derived from existing direct-TUI product state; it owns no backend fields,
//! protocol messages, session payloads or clipboard readers.

use ratatui::layout::Rect;

pub(crate) use crabcode_pager_render::picker::{
    PickerConfig, PickerEntry, PickerHitAreas, PickerOutcome, PickerRow, PickerState,
    clamp_picker_selection, handle_picker_input, render_divider, render_picker_content,
    render_picker_search_bar, render_picker_search_bar_with_label,
};

/// Existing CrabCode list-state conveniences around the fixed picker state.
pub(crate) trait PickerStateProductExt {
    fn input_with_query(query: impl Into<String>) -> Self;
    fn selected_for(&self, entry_count: usize) -> Option<usize>;
    fn set_selected(&mut self, selected: usize, entry_count: usize);
    fn move_next(&mut self, entry_count: usize, non_selectable: &[bool], wrap: bool) -> bool;
    fn move_previous(&mut self, entry_count: usize, non_selectable: &[bool], wrap: bool) -> bool;
    fn move_page(&mut self, entry_count: usize, page_rows: usize, forward: bool) -> bool;
}

impl PickerStateProductExt for PickerState {
    fn input_with_query(query: impl Into<String>) -> Self {
        let mut state = Self::input_active();
        state.set_query(query);
        state
    }

    fn selected_for(&self, entry_count: usize) -> Option<usize> {
        (entry_count > 0).then(|| self.selected.min(entry_count - 1))
    }

    fn set_selected(&mut self, selected: usize, entry_count: usize) {
        self.selected = if entry_count == 0 {
            0
        } else {
            selected.min(entry_count - 1)
        };
        self.selection_hidden = false;
        self.hovered = None;
        self.scroll_offset = None;
    }

    fn move_next(&mut self, entry_count: usize, non_selectable: &[bool], wrap: bool) -> bool {
        navigate(self, entry_count, non_selectable, true, wrap)
    }

    fn move_previous(&mut self, entry_count: usize, non_selectable: &[bool], wrap: bool) -> bool {
        navigate(self, entry_count, non_selectable, false, wrap)
    }

    fn move_page(&mut self, entry_count: usize, page_rows: usize, forward: bool) -> bool {
        if entry_count == 0 {
            return false;
        }
        let before = self.selected.min(entry_count - 1);
        self.selected = if forward {
            before.saturating_add(page_rows.max(1)).min(entry_count - 1)
        } else {
            before.saturating_sub(page_rows.max(1))
        };
        self.selection_hidden = false;
        self.hovered = None;
        self.scroll_offset = None;
        self.selected != before
    }
}

fn navigate(
    state: &mut PickerState,
    entry_count: usize,
    non_selectable: &[bool],
    forward: bool,
    wrap: bool,
) -> bool {
    if entry_count == 0 {
        return false;
    }
    let before = state.selected.min(entry_count - 1);
    for step in 1..=entry_count {
        let candidate = if forward {
            if wrap {
                (before + step) % entry_count
            } else {
                before.saturating_add(step).min(entry_count - 1)
            }
        } else if wrap {
            (before + entry_count - (step % entry_count)) % entry_count
        } else {
            before.saturating_sub(step)
        };
        if !non_selectable.get(candidate).copied().unwrap_or(false) {
            state.selected = candidate;
            break;
        }
        if !wrap && ((forward && candidate == entry_count - 1) || (!forward && candidate == 0)) {
            break;
        }
    }
    state.selection_hidden = false;
    state.hovered = None;
    state.scroll_offset = None;
    state.selected != before
}

/// Product rows without expansion metadata.
pub(crate) trait PickerRowProductExt<'a> {
    fn simple(label: &'a str, right_label: &'a str, selected: bool) -> Self;
}

impl<'a> PickerRowProductExt<'a> for PickerRow<'a> {
    fn simple(label: &'a str, right_label: &'a str, selected: bool) -> Self {
        Self {
            label,
            right_label,
            selected,
            expanded: false,
            fields: &[],
            description_lines: &[],
            summary_lines: &[],
            dimmed: false,
            indent: 0,
            badge: "",
            badge_color: None,
            collapsible: false,
            underline_last_desc: false,
        }
    }
}

#[must_use]
pub(crate) fn picker_config_default<'a>() -> PickerConfig<'a> {
    PickerConfig {
        title: None,
        show_search_hint: false,
        expandable: false,
        esc_clears_query: true,
        shortcuts: None,
        pending_hint: None,
        shortcuts_area: None,
        non_selectable: &[],
        non_selectable_clickable: &[],
        tabs: None,
        active_tab: 0,
        filter_label: None,
        filter_key_hint: None,
        filter_active: false,
        action_keys: &[],
        disable_search: false,
        compact_bottom_bar: false,
        search_only_on_slash: false,
        vim_normal_first: false,
    }
}

#[must_use]
pub(crate) fn empty_picker_hit_areas() -> PickerHitAreas {
    PickerHitAreas {
        close_button: Rect::default(),
        search_bar: Rect::default(),
        item_rects: Vec::new(),
        entry_indices: Vec::new(),
        tab_rects: Vec::new(),
        filter_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crabcode_pager_render::audited_theme::CrabCodeTheme;
    use crabcode_pager_render::picker::{
        PickerField, PickerMode, compute_scroll_offset, picker_shortcuts, render_picker,
        render_search_bar_with_viewport, search_bar_layout,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use unicode_width::UnicodeWidthStr as _;

    use super::*;

    fn config(show_search_hint: bool, vim_normal_first: bool) -> PickerConfig<'static> {
        PickerConfig {
            show_search_hint,
            vim_normal_first,
            ..picker_config_default()
        }
    }

    fn press(character: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn fixed_scroll_offset_centers_and_clamps() {
        assert_eq!(compute_scroll_offset(0, 3, 4, 0), 0);
        assert_eq!(compute_scroll_offset(8, 12, 5, 0), 6);
        assert_eq!(compute_scroll_offset(11, 12, 5, 0), 7);
        assert_eq!(compute_scroll_offset(9, 10, 5, 2), 6);
    }

    #[test]
    fn fixed_always_active_and_hint_input_paths_are_retained() {
        let mut always = PickerState::default();
        assert!(matches!(
            handle_picker_input(&press('a'), &mut always, 3, &config(false, false)),
            PickerOutcome::QueryChanged
        ));
        assert_eq!(always.query(), "a");
        assert!(!always.search_active);

        let mut hinted = PickerState::default();
        assert!(matches!(
            handle_picker_input(&press('a'), &mut hinted, 3, &config(true, false)),
            PickerOutcome::QueryChanged
        ));
        assert!(hinted.search_active);
        assert_eq!(hinted.query(), "a");
    }

    #[test]
    fn fixed_slash_focus_and_literal_query_paths_are_retained() {
        let mut unfocused = PickerState::default();
        assert!(matches!(
            handle_picker_input(&press('/'), &mut unfocused, 3, &config(false, false)),
            PickerOutcome::Changed
        ));
        assert!(unfocused.search_active);
        assert!(unfocused.query().is_empty());

        let mut focused = PickerState::input_active();
        assert!(matches!(
            handle_picker_input(&press('/'), &mut focused, 3, &config(false, false)),
            PickerOutcome::QueryChanged
        ));
        assert_eq!(focused.query(), "/");

        let mut path = PickerState::default();
        for character in ['a', 'b', '/'] {
            let _ = handle_picker_input(&press(character), &mut path, 3, &config(false, false));
        }
        assert_eq!(path.query(), "ab/");
    }

    #[test]
    fn fixed_vim_search_navigation_and_paste_paths_are_retained() {
        let config = config(true, true);
        let mut state = PickerState::default();
        assert!(matches!(
            handle_picker_input(&press('a'), &mut state, 3, &config),
            PickerOutcome::Unchanged
        ));
        assert!(state.query().is_empty());
        assert!(matches!(
            handle_picker_input(&press('i'), &mut state, 3, &config),
            PickerOutcome::Changed
        ));
        assert!(state.search_active);
        assert!(matches!(
            handle_picker_input(
                &Event::Paste("hello\r\nworld".to_string()),
                &mut state,
                3,
                &config,
            ),
            PickerOutcome::QueryChanged
        ));
        assert_eq!(state.query(), "helloworld");
        assert!(matches!(
            handle_picker_input(
                &key(KeyCode::Esc, KeyModifiers::NONE),
                &mut state,
                3,
                &config,
            ),
            PickerOutcome::QueryChanged
        ));
        assert!(!state.search_active);
        assert!(state.query().is_empty());
        assert!(matches!(
            handle_picker_input(&press('j'), &mut state, 3, &config),
            PickerOutcome::Changed
        ));
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn fixed_query_cursor_edits_preserve_list_state_and_text_edits_reset_once() {
        let config = config(false, false);
        let mut state = PickerState::default();
        state.set_query("alpha-beta");
        state.selected = 2;
        state.expanded.insert(1);
        state.scroll_offset = Some(4);
        assert!(matches!(
            handle_picker_input(
                &key(KeyCode::Left, KeyModifiers::ALT),
                &mut state,
                3,
                &config,
            ),
            PickerOutcome::Changed
        ));
        assert_eq!(state.query_cursor(), "alpha-".len());
        assert_eq!(state.selected, 2);
        assert_eq!(state.expanded, HashSet::from([1]));
        assert_eq!(state.scroll_offset, Some(4));

        assert!(matches!(
            handle_picker_input(
                &key(KeyCode::Backspace, KeyModifiers::ALT),
                &mut state,
                3,
                &config,
            ),
            PickerOutcome::QueryChanged
        ));
        assert_eq!(state.query(), "alphabeta");
        assert_eq!(state.selected, 0);
        assert!(state.expanded.is_empty());
        assert_eq!(state.scroll_offset, None);
    }

    #[test]
    fn fixed_tabs_filter_actions_and_submit_query_outcomes_are_retained() {
        let tabs = ["one", "two"];
        let mut tab_config = config(false, false);
        tab_config.tabs = Some(&tabs);
        assert!(matches!(
            handle_picker_input(
                &key(KeyCode::Tab, KeyModifiers::NONE),
                &mut PickerState::default(),
                2,
                &tab_config,
            ),
            PickerOutcome::TabChanged(1)
        ));

        let mut action_config = config(false, false);
        action_config.action_keys = &[('r', "reload")];
        assert!(matches!(
            handle_picker_input(&press('r'), &mut PickerState::default(), 2, &action_config,),
            PickerOutcome::Action('r')
        ));

        let mut filter_config = config(false, false);
        filter_config.filter_label = Some("Enabled");
        assert!(matches!(
            handle_picker_input(&press('f'), &mut PickerState::default(), 2, &filter_config,),
            PickerOutcome::FilterCycled
        ));

        let mut empty = PickerState::input_active();
        empty.set_query("direct-id");
        assert!(matches!(
            handle_picker_input(
                &key(KeyCode::Enter, KeyModifiers::NONE),
                &mut empty,
                0,
                &config(false, false),
            ),
            PickerOutcome::SubmitQuery
        ));
    }

    #[test]
    fn fixed_mouse_hit_areas_select_and_route_clickable_headers() {
        let row_rect = Rect::new(3, 4, 20, 1);
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });

        let mut state = PickerState::default();
        state.hit_areas = Some(PickerHitAreas {
            item_rects: vec![row_rect],
            entry_indices: vec![2],
            ..empty_picker_hit_areas()
        });
        assert!(matches!(
            handle_picker_input(&click, &mut state, 4, &config(false, false)),
            PickerOutcome::Selected(2)
        ));

        let mut state = PickerState::default();
        state.hit_areas = Some(PickerHitAreas {
            item_rects: vec![row_rect],
            entry_indices: vec![1],
            ..empty_picker_hit_areas()
        });
        let mut clickable = config(false, false);
        clickable.non_selectable = &[false, true];
        clickable.non_selectable_clickable = &[false, true];
        assert!(matches!(
            handle_picker_input(&click, &mut state, 2, &clickable),
            PickerOutcome::NonSelectableClick(1)
        ));
    }

    #[test]
    fn fixed_content_renderer_tracks_visual_rows_scrollbar_and_link_band() {
        let theme = CrabCodeTheme::NIGHT;
        let area = Rect::new(0, 0, 40, 5);
        let descriptions = ["instruction", "[https://example.invalid]"];
        let fields = [PickerField {
            label: "Status",
            value: "a value long enough to wrap across the available field width",
        }];
        let rows = [
            PickerEntry::Header { label: "Section" },
            PickerEntry::Row(PickerRow {
                label: "Expandable",
                right_label: "meta",
                selected: true,
                expanded: true,
                fields: &fields,
                description_lines: &descriptions,
                summary_lines: &[],
                dimmed: false,
                indent: 0,
                badge: "[active]",
                badge_color: Some(theme.accent_success),
                collapsible: true,
                underline_last_desc: true,
            }),
            PickerEntry::Row(PickerRow::simple("Second", "", false)),
        ];
        let mut state = PickerState::default();
        state.selected = 1;
        let mut buffer = Buffer::empty(area);
        let hits = render_picker_content(
            &mut buffer,
            area,
            &theme,
            &mut state,
            &rows,
            &[true, false, false],
            &[],
            Some(theme.bg_base),
            false,
        );
        assert_eq!(hits.entry_indices, vec![1]);
        assert_eq!(hits.item_rects.len(), 1);
        assert!(state.scroll_offset.is_some());
        assert!(state.link_band.is_some());
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn fixed_full_picker_renders_frame_search_content_and_mouse_geometry() {
        let theme = CrabCodeTheme::NIGHT;
        let area = Rect::new(0, 0, 70, 18);
        let rows = [
            PickerEntry::Row(PickerRow::simple("First", "one", true)),
            PickerEntry::Row(PickerRow::simple("Second", "two", false)),
        ];
        let mut state = PickerState::with_mode(PickerMode::FullScreen);
        state.search_active = true;
        let mut buffer = Buffer::empty(area);
        let config = PickerConfig {
            title: Some("Choose"),
            shortcuts: Some(picker_shortcuts()),
            ..picker_config_default()
        };
        let hits = render_picker(&mut buffer, area, &theme, &mut state, &rows, &config, false);
        assert_eq!(hits.entry_indices, vec![0, 1]);
        assert_eq!(hits.item_rects.len(), 2);
        assert!(hits.search_bar.width > 0);
        assert!(hits.close_button.width > 0);
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.bg == theme.text_primary)
        );
    }

    #[test]
    fn fixed_search_counter_reservation_never_overwrites_trailing_cells() {
        let theme = CrabCodeTheme::NIGHT;
        let width = 20;
        let counter_width = "12/34".width() as u16;
        let layout = search_bar_layout(width, counter_width);
        assert_eq!(layout.input_width(), 5);
        assert_eq!(layout.trailing_width(), counter_width);
        let mut state = PickerState::default();
        state.set_query("123456789中e\u{301}👩🏽\u{200d}💻z");
        let viewport = state.query_viewport(layout.input_width());
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
        buffer.set_string(0, 0, "#".repeat(width as usize), Style::default());
        render_search_bar_with_viewport(
            &mut buffer,
            0,
            0,
            layout,
            &theme,
            state.query(),
            true,
            false,
            None,
            viewport,
        );
        let render_width = " search: ".len() as u16 + layout.input_width() as u16;
        for x in render_width..width {
            assert_eq!(buffer[(x, 0)].symbol(), "#");
        }
    }
}
