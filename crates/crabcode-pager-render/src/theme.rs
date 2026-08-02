//! Renderer-local theme compatibility surface.

pub mod md_style;

pub use crate::audited_theme::CrabCodeTheme as Theme;
pub use crate::audited_theme::CrabCodeThemeKind as ThemeKind;
pub use crate::audited_theme::color_support::quantize;
pub use crate::audited_theme::{cache, color_support};

/// Compute animated brightness for a traveling wave effect.
///
/// Exact fixed renderer algorithm: each row has a spatial phase and frame
/// ticks advance the temporal phase; `sin²` keeps the result in `[0, 1]`.
pub fn wave_brightness(tick: u64, row: u16, wave_rows: u16, speed: f32) -> f32 {
    use std::f32::consts::PI;

    let rows_per_wave = wave_rows.max(1) as f32;
    let phase = (row as f32 / rows_per_wave) * 2.0 * PI;
    let time = tick as f32 * speed;
    let value = (time + phase).sin();
    value * value
}
