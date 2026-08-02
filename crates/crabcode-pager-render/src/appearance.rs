//! Renderer-local appearance compatibility values.
//!
//! Disk/config ownership remains in the direct host. This module stores only
//! values already injected into the renderer lifecycle.

use std::sync::atomic::{AtomicU8, Ordering};

pub use crate::audited_appearance::render_mermaid::RenderMermaid;
pub use crate::audited_appearance::{
    AppearanceConfig, ExecuteHeaderStyle, RendererLanguage, ToolBullet,
};

static TAB_WIDTH: AtomicU8 = AtomicU8::new(4);

/// Renderer-local values injected by the direct TUI owner.
///
/// Persistence and configuration authority stay outside this crate. The
/// render path reads this thread-local value, matching the fixed upstream
/// single-main-thread lifecycle without performing disk I/O.
pub mod cache {
    use std::cell::Cell;

    use super::RenderMermaid;

    thread_local! {
        static RENDER_MERMAID: Cell<RenderMermaid> =
            const { Cell::new(RenderMermaid::Auto) };
        static COLLAPSED_EDIT_BLOCKS: Cell<bool> = const { Cell::new(false) };
        static SHOW_THINKING_BLOCKS: Cell<bool> = const { Cell::new(true) };
        static GROUP_TOOL_VERBS: Cell<bool> = const { Cell::new(true) };
    }

    #[must_use]
    pub fn load_render_mermaid() -> RenderMermaid {
        RENDER_MERMAID.get()
    }

    pub fn set_render_mermaid(value: RenderMermaid) {
        RENDER_MERMAID.set(value);
    }

    /// Read the renderer-local edit folding preference.
    ///
    /// CrabCode's direct backend does not expose the fixed shell setting.
    /// The fixed default is therefore retained (`false`) without reading
    /// configuration or expanding the direct protocol.
    #[must_use]
    pub fn load_collapsed_edit_blocks() -> bool {
        COLLAPSED_EDIT_BLOCKS.get()
    }

    /// Update the renderer-local edit folding preference.
    ///
    /// This is a presentation value only; persistence remains outside the
    /// renderer and no caller may treat it as backend configuration.
    pub fn set_collapsed_edit_blocks(enabled: bool) {
        COLLAPSED_EDIT_BLOCKS.set(enabled);
    }

    /// Read the renderer-local thinking visibility preference.
    ///
    /// The fixed default is on. The direct TUI may inject a presentation
    /// preference without granting this crate persistence or protocol
    /// authority.
    #[must_use]
    pub fn load_show_thinking_blocks() -> bool {
        SHOW_THINKING_BLOCKS.get()
    }

    pub fn set_show_thinking_blocks(enabled: bool) {
        SHOW_THINKING_BLOCKS.set(enabled);
    }

    /// Read fixed verb grouping, which defaults on.
    #[must_use]
    pub fn load_group_tool_verbs() -> bool {
        GROUP_TOOL_VERBS.get()
    }

    pub fn set_group_tool_verbs(enabled: bool) {
        GROUP_TOOL_VERBS.set(enabled);
    }
}

/// Current tab expansion width in spaces.
#[must_use]
pub fn tab_width() -> u8 {
    TAB_WIDTH.load(Ordering::Relaxed)
}

/// Update the renderer-local tab expansion width.
pub fn set_tab_width(width: u8) {
    TAB_WIDTH.store(width, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_width_defaults_to_four() {
        assert_eq!(tab_width(), 4);
    }

    #[test]
    fn collapsed_edit_blocks_retains_fixed_default() {
        cache::set_collapsed_edit_blocks(false);
        assert!(!cache::load_collapsed_edit_blocks());
        cache::set_collapsed_edit_blocks(true);
        assert!(cache::load_collapsed_edit_blocks());
        cache::set_collapsed_edit_blocks(false);
    }

    #[test]
    fn lifecycle_visibility_values_retain_fixed_defaults_and_round_trip() {
        assert!(cache::load_show_thinking_blocks());
        assert!(cache::load_group_tool_verbs());
        cache::set_show_thinking_blocks(false);
        cache::set_group_tool_verbs(false);
        assert!(!cache::load_show_thinking_blocks());
        assert!(!cache::load_group_tool_verbs());
        cache::set_show_thinking_blocks(true);
        cache::set_group_tool_verbs(true);
    }
}
