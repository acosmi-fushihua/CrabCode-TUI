#![allow(clippy::uninlined_format_args)]

//! Backend-independent terminal presentation primitives used by CrabCode.
//!
//! This crate owns only deterministic rendering and terminal-interaction
//! algorithms. It does not own sessions, tools, permissions, telemetry,
//! configuration authority, remote transport, or renderer behavior.

pub mod appearance;
pub mod audited_appearance;
pub mod audited_glyphs;
pub mod audited_host;
pub mod audited_modal_window_state;
pub mod audited_render;
pub mod audited_terminal;
pub mod audited_theme;
pub mod context_visualization;
pub mod diff;
pub mod input;
pub mod glyphs {
    pub use crate::audited_glyphs::*;
}
pub mod inline_media;
pub mod link_opener;
pub mod mcp_display;
pub mod modal_window;
pub mod picker;
pub mod picker_line_editor;
pub mod picker_scrollbar;
pub mod picker_shortcuts;
pub mod prompt_images;
pub mod render;
pub mod scrollback;
pub mod search;
mod shell_command;
pub mod side_question_panel;
pub mod syntax;
pub mod terminal;
pub mod terminal_output;
pub mod text_safety;
pub mod theme;
pub mod timeline;
pub mod tui_render;
pub mod util;
