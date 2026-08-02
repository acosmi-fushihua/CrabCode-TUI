//! Backend-independent terminal appearance primitives.

pub mod cache;
pub mod color_support;
mod engine;
pub mod osc11;
pub mod system_appearance;

pub use engine::{CrabCodeTheme, CrabCodeThemeKind, PRODUCT_ROLE_AUDIT, ProductRoleSource};
