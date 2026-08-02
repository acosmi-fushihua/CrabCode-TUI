//! Narrow application adapter for renderer-owned mouse-scroll state.
//!
//! The fixed renderer owns gesture classification, pacing, acceleration, and
//! stream finalization. The application only selects a viewport and applies
//! the returned line delta; no backend or wire-protocol message is involved.

pub(crate) use crabcode_pager_render::input::mouse::{
    MouseScrollState, ScrollConfig, ScrollDirection,
};
