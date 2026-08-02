//! Fixed-upstream synchronized frame transaction, adapted to CrabCode's
//! existing ordered terminal writer.
//!
//! Source lineage:
//! - repository commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - monorepo source revision: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//! - source owner anchor:
//!   `render-owner-transplant-closure-report.json` owner-chain order 2
//! - source SHA-256:
//!   `6963f8d4c18dca4d9103966fd99a185b35877d85b928443d3344989a2baf057e`
//!
//! The mother implementation owns the order
//! synchronized-begin -> resize -> frame composition -> link installation ->
//! diff flush -> buffer swap -> post-flush/cursor -> synchronized-end.
//! This adapter preserves that order while retaining CrabCode's established
//! fallible ordered-writer boundary. It owns no backend/session/protocol state.

use std::io;

use crabcode_ratatui_inline::{LinkSpan, Terminal};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};

use crate::terminal::{SynchronizedFrameBackend, TerminalMode};
use crate::terminal_capabilities::HyperlinkRoute;
use crate::terminal_writer::lock_terminal_output_for_active_write;

/// Last cursor state successfully committed by the ordered writer.
///
/// This is the fixed source's `CursorState` with one CrabCode-owned extension:
/// `disturbed_outside_frame` forces a reposition after an inline insertion or
/// resize has moved the physical cursor outside Ratatui's cell diff.
#[derive(Debug, Default)]
pub(crate) struct CursorState {
    pub(crate) last_position: Option<(u16, u16)>,
    pub(crate) disturbed_outside_frame: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorAction {
    None,
    Reposition(u16, u16),
    Show(u16, u16),
    Hide,
}

impl CursorState {
    pub(crate) fn action(&self, desired: Option<(u16, u16)>, frame_wrote: bool) -> CursorAction {
        let terminal_wrote = frame_wrote || self.disturbed_outside_frame;
        if desired == self.last_position {
            return match desired {
                Some((x, y)) if terminal_wrote => CursorAction::Reposition(x, y),
                _ => CursorAction::None,
            };
        }
        match (desired, self.last_position) {
            (Some((x, y)), Some(_)) => CursorAction::Reposition(x, y),
            (Some((x, y)), None) => CursorAction::Show(x, y),
            (None, Some(_)) => CursorAction::Hide,
            (None, None) => CursorAction::None,
        }
    }

    fn settle(&mut self, desired: Option<(u16, u16)>) {
        self.last_position = desired;
        self.disturbed_outside_frame = false;
    }

    pub(crate) fn mark_disturbed(&mut self) {
        self.disturbed_outside_frame = true;
    }
}

/// Draw one frame through the sole production terminal route.
///
/// `compose` is the narrow read-only renderer adapter: it projects the current
/// `TuiApp` into Ratatui cells and returns cursor/link presentation metadata.
/// All terminal mutations, including minimal-mode native-scrollback insertion,
/// remain inside this one fallible synchronized transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_frame<B: SynchronizedFrameBackend>(
    terminal: &mut Terminal<B>,
    cursor_state: &mut CursorState,
    last_rendered_frame: &mut Option<Buffer>,
    last_rendered_hyperlinks: &mut Option<Vec<LinkSpan>>,
    backend_size: &mut Size,
    resize_requested: bool,
    mode: TerminalMode,
    hyperlink_route: HyperlinkRoute,
    compose: impl FnOnce(&mut Terminal<B>) -> io::Result<(crate::tui_ui::RenderOutcome, bool)>,
) -> io::Result<()> {
    terminal
        .backend_mut()
        .frame_writer_mut()
        .begin_synchronized_frame()?;

    let mut buffers_swapped = false;
    let draw_result = (|| {
        let observed_backend_size = terminal.backend().size()?;
        let resized = resize_requested || observed_backend_size != *backend_size;
        if resized {
            {
                // Inline resize can issue a direct cursor-position query.
                // Serialize that query with panic/signal restoration; frame
                // bytes remain staged in the ordered writer transaction.
                let _output_guard = lock_terminal_output_for_active_write()?;
                terminal.resize(Rect::new(
                    0,
                    0,
                    observed_backend_size.width,
                    observed_backend_size.height,
                ))?;
            }
            match mode {
                TerminalMode::Fullscreen => {}
                TerminalMode::Inline => {
                    terminal.set_viewport_height(observed_backend_size.height)?;
                }
                // Minimal resolves its post-commit content height inside the
                // compose pass on every frame. Resizing to a fixed live height
                // here would race that authoritative measurement.
                TerminalMode::Minimal => {}
            }
        }
        let (mut render_outcome, mutated_before_render) = compose(terminal)?;
        if !hyperlink_route.emit_osc8 {
            render_outcome.hyperlinks.clear();
        } else if !hyperlink_route.emit_id {
            for hyperlink in &mut render_outcome.hyperlinks {
                hyperlink.id = None;
            }
        }
        if resized || mutated_before_render {
            // Inline insertion clears the live viewport and Ratatui's inactive
            // diff buffer; resize does the same even when the live viewport
            // keeps identical geometry. The live region therefore requires a
            // full redraw within this same synchronized transaction.
            *last_rendered_frame = None;
            *last_rendered_hyperlinks = None;
            cursor_state.mark_disturbed();
        }
        terminal.set_frame_links(&render_outcome.hyperlinks);
        let rendered_frame = terminal.current_buffer_mut().clone();
        let frame_changed = last_rendered_frame
            .as_ref()
            .is_none_or(|previous| previous != &rendered_frame)
            || last_rendered_hyperlinks
                .as_ref()
                .is_none_or(|previous| previous != &render_outcome.hyperlinks);
        if frame_changed {
            let _changed = terminal.flush_with_links()?;
        }
        terminal.swap_buffers();
        buffers_swapped = true;

        let frame_wrote = terminal
            .backend_mut()
            .frame_writer_mut()
            .synchronized_frame_has_content()?;
        let cursor_action = cursor_state.action(render_outcome.cursor, frame_wrote);
        apply_cursor_action(terminal, cursor_action)?;

        Ok::<_, io::Error>((
            render_outcome.cursor,
            rendered_frame,
            render_outcome.hyperlinks,
            cursor_action,
            observed_backend_size,
        ))
    })();

    let (desired_cursor, rendered_frame, rendered_hyperlinks, cursor_action, observed_backend_size) =
        match draw_result {
            Ok(result) => result,
            Err(error) => {
                if buffers_swapped {
                    // The ordered writer discards this transaction, so Ratatui
                    // must discard the matching "previous frame" too. A retry
                    // must diff against cells that actually reached the tty.
                    terminal.swap_buffers();
                }
                terminal
                    .backend_mut()
                    .frame_writer_mut()
                    .abort_synchronized_frame();
                return Err(error);
            }
        };

    let frame_has_content = terminal
        .backend_mut()
        .frame_writer_mut()
        .synchronized_frame_has_content()?;
    if !frame_has_content && cursor_action == CursorAction::None {
        terminal
            .backend_mut()
            .frame_writer_mut()
            .abort_synchronized_frame();
    } else if let Err(error) = terminal
        .backend_mut()
        .frame_writer_mut()
        .finish_synchronized_frame()
    {
        terminal.swap_buffers();
        terminal
            .backend_mut()
            .frame_writer_mut()
            .abort_synchronized_frame();
        return Err(error);
    }

    cursor_state.settle(desired_cursor);
    *last_rendered_frame = Some(rendered_frame);
    *last_rendered_hyperlinks = Some(rendered_hyperlinks);
    *backend_size = observed_backend_size;
    Ok(())
}

fn apply_cursor_action<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    action: CursorAction,
) -> io::Result<()> {
    match action {
        CursorAction::None => Ok(()),
        CursorAction::Reposition(x, y) => terminal.set_cursor_position((x, y)),
        CursorAction::Show(x, y) => {
            terminal.set_cursor_position((x, y))?;
            terminal.show_cursor()
        }
        CursorAction::Hide => terminal.hide_cursor(),
    }
}
