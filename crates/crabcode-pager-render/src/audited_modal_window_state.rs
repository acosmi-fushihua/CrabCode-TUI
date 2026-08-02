//! Pure modal-window chrome state shared by native terminal views.

use ratatui::layout::Rect;

/// Persistent hit-test and focus state for modal-window chrome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalWindowState {
    pub close_hovered: bool,
    pub close_button_rect: Option<Rect>,
    pub popup_area: Option<Rect>,
    pub active_tab: usize,
    pub tab_count: usize,
    pub tab_rects: Vec<Option<Rect>>,
    pub tabs_focused: bool,
    pub shortcut_hits: Vec<ShortcutHitArea>,
    pub hovered_shortcut: Option<usize>,
}

impl ModalWindowState {
    pub fn new() -> Self {
        Self {
            close_hovered: false,
            close_button_rect: None,
            popup_area: None,
            active_tab: 0,
            tab_count: 0,
            tab_rects: Vec::new(),
            tabs_focused: false,
            shortcut_hits: Vec::new(),
            hovered_shortcut: None,
        }
    }

    pub fn with_tabs(tab_count: usize) -> Self {
        Self {
            tab_count,
            tab_rects: vec![None; tab_count],
            ..Self::new()
        }
    }
}

impl Default for ModalWindowState {
    fn default() -> Self {
        Self::new()
    }
}

/// Hit-test area for one rendered modal shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutHitArea {
    pub rect: Rect,
    pub id: usize,
    pub shortcuts_idx: usize,
    pub clickable: bool,
}
