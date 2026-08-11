//! Renderer-private pointer lifecycle for blocking request dialogs.
//!
//! The fixed Rust TUI keeps permission/question hit geometry, hover
//! selection, and double-click activation in its interaction owner. CrabCode
//! retains that terminal mechanic here while the existing `RequestDialog`
//! and `HostAction` path remain the only product/backend authorities.
//!
//! This state is intentionally process-local. It contains no request id,
//! response value, transport payload, or serializable protocol shape.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

/// Fixed-source multi-click window used by permission/question rows.
const MULTI_CLICK_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogPointerOutcome {
    Unchanged,
    Selected(usize),
    Activated(usize),
    Input,
}

#[derive(Debug, Default)]
pub(crate) struct DialogPointerState {
    choice_areas: Vec<Rect>,
    input_area: Option<Rect>,
    preview_area: Option<Rect>,
    hovered_choice: Option<usize>,
    last_click: Option<(Instant, usize)>,
}

impl DialogPointerState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Begin a new paint generation. Geometry is rebuilt by the active dialog
    /// branch before input can observe it.
    pub(crate) fn begin_frame(&mut self) {
        self.choice_areas.clear();
        self.input_area = None;
        self.preview_area = None;
        self.hovered_choice = None;
    }

    pub(crate) fn set_choice_areas(&mut self, areas: Vec<Rect>) {
        self.choice_areas = areas;
        if self
            .hovered_choice
            .is_some_and(|index| index >= self.choice_areas.len())
        {
            self.hovered_choice = None;
        }
        if self
            .last_click
            .is_some_and(|(_, index)| index >= self.choice_areas.len())
        {
            self.last_click = None;
        }
    }

    pub(crate) fn set_input_area(&mut self, area: Rect) {
        self.input_area = (area.area() != 0).then_some(area);
    }

    pub(crate) fn input_area(&self) -> Option<Rect> {
        self.input_area
    }

    pub(crate) fn set_preview_area(&mut self, area: Rect) {
        self.preview_area = (area.area() != 0).then_some(area);
    }

    pub(crate) fn preview_area(&self) -> Option<Rect> {
        self.preview_area
    }

    #[cfg(test)]
    pub(crate) fn choice_areas(&self) -> &[Rect] {
        &self.choice_areas
    }

    fn choice_at(&self, column: u16, row: u16) -> Option<usize> {
        self.choice_areas
            .iter()
            .position(|area| area.contains((column, row).into()))
    }

    pub(crate) fn handle_mouse(
        &mut self,
        mouse: &MouseEvent,
        arrived_at: Instant,
    ) -> DialogPointerOutcome {
        if self
            .input_area
            .is_some_and(|area| area.contains((mouse.column, mouse.row).into()))
        {
            self.last_click = None;
            return DialogPointerOutcome::Input;
        }

        let hit = self.choice_at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved => {
                if self.hovered_choice == hit {
                    DialogPointerOutcome::Unchanged
                } else {
                    self.hovered_choice = hit;
                    hit.map_or(
                        DialogPointerOutcome::Unchanged,
                        DialogPointerOutcome::Selected,
                    )
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(index) = hit else {
                    self.last_click = None;
                    return DialogPointerOutcome::Unchanged;
                };
                self.hovered_choice = Some(index);
                let activate = self
                    .last_click
                    .is_some_and(|(previous_at, previous_index)| {
                        previous_index == index
                            && arrived_at.saturating_duration_since(previous_at)
                                < MULTI_CLICK_TIMEOUT
                    });
                self.last_click = (!activate).then_some((arrived_at, index));
                if activate {
                    DialogPointerOutcome::Activated(index)
                } else {
                    DialogPointerOutcome::Selected(index)
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => DialogPointerOutcome::Unchanged,
        }
    }
}

/// Build exact one-row hit rectangles for the same `"  label "` segments
/// painted by the dialog action row.
pub(crate) fn inline_choice_areas(area: Rect, labels: &[&str]) -> Vec<Rect> {
    use unicode_width::UnicodeWidthStr as _;

    if area.area() == 0 {
        return Vec::new();
    }
    let mut x = area.x;
    let right = area.right();
    let mut output = Vec::with_capacity(labels.len());
    for label in labels {
        let width = u16::try_from(label.width())
            .unwrap_or(u16::MAX)
            .saturating_add(3);
        let available = right.saturating_sub(x);
        let painted = width.min(available);
        output.push(Rect::new(x, area.y, painted, 1));
        x = x.saturating_add(painted);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn mouse(kind: MouseEventKind, column: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row: 4,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn permission_style_pointer_selects_then_double_clicks_only_the_same_row() {
        let mut state = DialogPointerState::default();
        state.set_choice_areas(vec![Rect::new(2, 4, 8, 1), Rect::new(10, 4, 8, 1)]);
        let start = Instant::now();

        assert_eq!(
            state.handle_mouse(&mouse(MouseEventKind::Moved, 11), start),
            DialogPointerOutcome::Selected(1)
        );
        assert_eq!(
            state.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 11), start,),
            DialogPointerOutcome::Selected(1)
        );
        assert_eq!(
            state.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 11),
                start + Duration::from_millis(299),
            ),
            DialogPointerOutcome::Activated(1)
        );

        assert_eq!(
            state.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 3),
                start + Duration::from_millis(300),
            ),
            DialogPointerOutcome::Selected(0)
        );
        assert_eq!(
            state.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 3),
                start + Duration::from_millis(600),
            ),
            DialogPointerOutcome::Selected(0),
            "the fixed window is strict and an expired click must re-arm"
        );
    }

    #[test]
    fn input_geometry_preempts_choice_activation_and_retires_click_pairing() {
        let mut state = DialogPointerState::default();
        state.set_choice_areas(vec![Rect::new(2, 4, 8, 1)]);
        state.set_input_area(Rect::new(2, 2, 20, 1));
        let start = Instant::now();
        assert_eq!(
            state.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 3), start,),
            DialogPointerOutcome::Selected(0)
        );
        let input = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            state.handle_mouse(&input, start + Duration::from_millis(10)),
            DialogPointerOutcome::Input
        );
        assert_eq!(
            state.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 3),
                start + Duration::from_millis(20),
            ),
            DialogPointerOutcome::Selected(0)
        );
    }
}
