//! Direct-TUI adapter for the pager-owned terminal-text safety boundary.

pub use crabcode_pager_render::text_safety::{
    MAX_RENDER_FIELD_BYTES, sanitize_bounded_terminal_text, sanitize_terminal_text,
};

pub(crate) use crabcode_pager_render::text_safety::{
    is_ecmascript_whitespace, trim_ecmascript_whitespace,
};
