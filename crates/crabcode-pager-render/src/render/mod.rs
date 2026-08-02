//! Fixed-upstream image-overlay rendering plus existing audited primitives.

pub mod highlight;
pub mod image_overlay;
pub mod osc8;

pub mod color {
    pub use crate::audited_render::color::*;
}

pub mod line_utils {
    pub use crate::audited_render::line_utils::*;
}

pub mod terminal_output {
    pub use crate::terminal_output::*;
}

pub mod tool_paths {
    pub use crate::audited_render::tool_paths::*;
}

pub mod wrapping {
    pub use crate::audited_render::wrapping::*;
}

pub use crate::audited_render::renderable::{Renderable, RenderableItem};
pub use crate::audited_render::safe_buf::SafeBuf;
pub use image_overlay::render_image_overlay;
