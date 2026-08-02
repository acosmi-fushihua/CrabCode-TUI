//! Shared modal and overlay lifecycle for CrabCode's terminal renderer.
//!
//! The reusable visibility/focus/fullscreen state and modal-window hit-test
//! state are source ports from the fixed Rust upstream.  `ActiveModalTree` is
//! the narrow CrabCode adapter: it names only product surfaces that already
//! exist and carries no backend payload or protocol behavior.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub use crabcode_pager_render::modal_window::{
    ModalWindowOutcome, ModalWindowState, handle_modal_mouse,
};

/// What the caller should do after an overlay state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    /// No state change — key not consumed.
    Ignored,
    /// State changed, redraw needed.
    Changed,
    /// Unfocused → move focus to scrollback.
    FocusScrollback,
    /// Unfocused → move focus to prompt.
    FocusPrompt,
}

impl OverlayAction {
    /// Whether this action represents a consumed key event.
    pub fn consumed(self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

/// Shared visibility / focus / fullscreen state for overlay panes.
///
/// Embedded in each toggleable pane.  The pane's shortcut handler calls
/// [`toggle()`], and the shared [`handle_overlay_key()`] handles
/// Tab/Esc/q/Space/Ctrl-F.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayState {
    pub visible: bool,
    pub focused: bool,
    pub fullscreen: bool,
}

impl OverlayState {
    /// Start visible but not focused (e.g. a pane that shows when items arrive).
    #[allow(dead_code)]
    pub fn visible() -> Self {
        Self {
            visible: true,
            focused: false,
            fullscreen: false,
        }
    }

    /// Start hidden.
    pub fn hidden() -> Self {
        Self::default()
    }

    /// Pane shortcut: three-state toggle.
    ///
    /// Hidden → show + focus.
    /// Visible + unfocused → focus.
    /// Visible + focused → hide.
    pub fn toggle(&mut self) -> OverlayAction {
        if !self.visible {
            self.visible = true;
            self.focused = true;
        } else if !self.focused {
            self.focused = true;
        } else {
            self.visible = false;
            self.fullscreen = false;
            self.focused = false;
        }
        OverlayAction::Changed
    }

    /// Tab: exit fullscreen if active, unfocus, keep visible → scrollback.
    pub fn tab_out(&mut self) -> OverlayAction {
        self.fullscreen = false;
        self.focused = false;
        OverlayAction::FocusScrollback
    }

    /// Esc / q: exit one nesting level.
    ///
    /// Fullscreen → exit fullscreen (stay visible + focused).
    /// Non-fullscreen → hide entirely.
    pub fn escape(&mut self) -> OverlayAction {
        if self.fullscreen {
            self.fullscreen = false;
        } else {
            self.visible = false;
            self.focused = false;
        }
        OverlayAction::Changed
    }

    /// Space: exit fullscreen if active, unfocus, keep visible → prompt.
    pub fn space(&mut self) -> OverlayAction {
        self.fullscreen = false;
        self.focused = false;
        OverlayAction::FocusPrompt
    }

    /// Ctrl-F: toggle fullscreen.
    pub fn toggle_fullscreen(&mut self) -> OverlayAction {
        self.fullscreen = !self.fullscreen;
        OverlayAction::Changed
    }

    /// Hide entirely. Used by external callers.
    #[allow(dead_code)]
    pub fn hide(&mut self) -> OverlayAction {
        self.visible = false;
        self.fullscreen = false;
        self.focused = false;
        OverlayAction::Changed
    }

    /// Show without changing focus.
    pub fn show(&mut self) {
        self.visible = true;
    }
}

/// Handle structural keys for any focused overlay pane.
pub fn handle_overlay_key(state: &mut OverlayState, key: &KeyEvent) -> Option<OverlayAction> {
    // Ctrl-F: toggle fullscreen (works even with input bar open).
    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(state.toggle_fullscreen());
    }

    None
}

/// Handle structural keys that should only fire when no input bar is open.
pub fn handle_overlay_nav_key(state: &mut OverlayState, key: &KeyEvent) -> Option<OverlayAction> {
    match key.code {
        KeyCode::Tab => Some(state.tab_out()),
        KeyCode::Esc => Some(state.escape()),
        // Plain 'q' only (Ctrl-Q is the app-level quit shortcut).
        KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => Some(state.escape()),
        KeyCode::Char(' ') => Some(state.space()),
        _ => None,
    }
}

/// Existing CrabCode terminal surfaces in paint/input priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveModal {
    HistorySearch,
    WorkspaceSearch,
    ModelPicker,
    ModelManagement,
    UsagePluginManagement,
    McpSettings,
    Overlay,
    Request,
    Fatal,
}

/// Product-only adapter that resolves the single active modal without owning
/// any modal payload or backend response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ActiveModalTree {
    active: Option<ActiveModal>,
}

impl ActiveModalTree {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn synchronize(
        &mut self,
        history_search: bool,
        workspace_search: bool,
        model_picker: bool,
        model_management: bool,
        usage_plugin_management: bool,
        mcp_settings: bool,
        overlay: bool,
        request: bool,
        fatal: bool,
    ) {
        self.active = if fatal {
            Some(ActiveModal::Fatal)
        } else if request {
            Some(ActiveModal::Request)
        } else if history_search {
            Some(ActiveModal::HistorySearch)
        } else if workspace_search {
            Some(ActiveModal::WorkspaceSearch)
        } else if model_management {
            Some(ActiveModal::ModelManagement)
        } else if usage_plugin_management {
            Some(ActiveModal::UsagePluginManagement)
        } else if overlay {
            Some(ActiveModal::Overlay)
        } else if mcp_settings {
            Some(ActiveModal::McpSettings)
        } else if model_picker {
            Some(ActiveModal::ModelPicker)
        } else {
            None
        };
    }

    pub(crate) fn active(&self) -> Option<ActiveModal> {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    #[test]
    fn overlay_lifecycle_keeps_fullscreen_as_a_nested_escape_level() {
        let mut state = OverlayState::hidden();
        assert_eq!(state.toggle(), OverlayAction::Changed);
        assert!(state.visible && state.focused);
        assert_eq!(state.toggle_fullscreen(), OverlayAction::Changed);
        assert!(state.fullscreen);
        assert_eq!(state.escape(), OverlayAction::Changed);
        assert!(state.visible && state.focused && !state.fullscreen);
        assert_eq!(state.escape(), OverlayAction::Changed);
        assert!(!state.visible && !state.focused);
    }

    #[test]
    fn structural_keys_preserve_plain_q_and_ctrl_f_boundaries() {
        let mut state = OverlayState {
            visible: true,
            focused: true,
            fullscreen: false,
        };
        assert_eq!(
            handle_overlay_key(
                &mut state,
                &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            ),
            Some(OverlayAction::Changed)
        );
        assert!(state.fullscreen);
        assert_eq!(
            handle_overlay_nav_key(
                &mut state,
                &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            ),
            None
        );
        assert_eq!(
            handle_overlay_nav_key(
                &mut state,
                &KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            ),
            Some(OverlayAction::Changed)
        );
        assert!(state.visible, "plain q first exits fullscreen");
    }

    #[test]
    fn modal_mouse_uses_current_geometry_and_clears_hover_on_exit() {
        let mut state = ModalWindowState::new();
        state.popup_area = Some(Rect::new(10, 5, 40, 20));
        state.close_button_rect = Some(Rect::new(45, 5, 3, 1));
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Moved, 46, 5),
            ModalWindowOutcome::Handled
        );
        assert!(state.close_hovered);
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Moved, 20, 10),
            ModalWindowOutcome::Handled
        );
        assert!(!state.close_hovered);
        assert_eq!(
            handle_modal_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 2, 2,),
            ModalWindowOutcome::CloseRequested
        );
    }

    #[test]
    fn modal_tree_uses_existing_paint_priority_without_payload_duplication() {
        let mut tree = ActiveModalTree::default();
        tree.synchronize(false, false, true, false, false, false, true, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::Overlay));
        tree.synchronize(false, false, true, false, false, false, true, true, false);
        assert_eq!(tree.active(), Some(ActiveModal::Request));
        tree.synchronize(false, false, true, false, false, false, true, true, true);
        assert_eq!(tree.active(), Some(ActiveModal::Fatal));
        tree.synchronize(true, true, true, true, true, true, true, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::HistorySearch));
        tree.synchronize(false, true, true, true, true, true, true, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::WorkspaceSearch));
        tree.synchronize(false, false, true, false, false, true, false, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::McpSettings));
        tree.synchronize(false, false, true, true, true, true, true, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::ModelManagement));
        tree.synchronize(false, false, true, false, true, true, true, false, false);
        assert_eq!(tree.active(), Some(ActiveModal::UsagePluginManagement));
    }
}
