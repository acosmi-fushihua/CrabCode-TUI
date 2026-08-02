//! Backend-independent scrollback rendering foundations.

pub mod block;
pub mod blocks;
pub mod entry;
pub mod export;
pub mod layout;
pub mod link_map;
pub mod minimal;
pub mod render;
pub mod scrollback_pane;
pub mod search;
pub mod selection;
pub mod state;
pub mod sticky;
pub mod table_geometry;
pub mod text_selection;
pub mod types;
pub mod wrappers;

pub use block::{AnchoredMedia, BlockContent, RenderBlock, StubBlock};
pub use blocks::ToolCallBlock;
pub use entry::{EntryId, ScrollbackEntry};
pub use render::{
    DiagramAffordancePlacement, InlineMediaPlacement, ScratchBuffer, ScrollRenderResult,
    SelectedEntryArea,
};
pub use scrollback_pane::ScrollbackPane;
pub use selection::{RenderOutput, ScrollInfo, SelectionBox};
pub use state::ScrollbackState;
pub use types::*;
