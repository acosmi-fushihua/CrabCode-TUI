//! Renderer-local syntax-highlighting lifecycle.
//!
//! The fixed renderer's dark/light TextMate themes and polarity-safe minimal
//! mode are preserved. Theme selection consumes only the already-resolved
//! renderer theme kind; it performs no config, backend, or protocol access.

use std::sync::OnceLock;

pub use crabcode_markdown_renderer::Syntect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::theme::ThemeKind;

static SYNTECT_DARK: OnceLock<Syntect> = OnceLock::new();
static SYNTECT_LIGHT: OnceLock<Syntect> = OnceLock::new();

/// Convert a syntect style to a ratatui foreground-only style.
#[must_use]
pub fn syntect_to_ratatui_fg(style: syntect::highlighting::Style) -> Style {
    let foreground = syntect_rgb_to_fg(style.foreground.r, style.foreground.g, style.foreground.b);
    let mut output = Style::default().fg(foreground);
    use syntect::highlighting::FontStyle;
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        output = output.add_modifier(Modifier::UNDERLINED);
    }
    output
}

/// Map a syntect RGB token to the active terminal color representation.
#[must_use]
pub fn syntect_rgb_to_fg(red: u8, green: u8, blue: u8) -> Color {
    if crate::theme::cache::terminal_native_locked() {
        polarity_safe_syntax_fg(red, green, blue)
    } else {
        crate::audited_theme::color_support::quantize(Color::Rgb(red, green, blue))
    }
}

/// Map syntax colors to a palette readable on both dark and light terminal
/// profiles when minimal mode leaves the terminal canvas transparent.
///
/// Near-grays inherit the terminal foreground. Chromatic tokens use only base
/// ANSI accents and never black, white, or bright variants.
#[must_use]
pub fn polarity_safe_syntax_fg(red: u8, green: u8, blue: u8) -> Color {
    let maximum = red.max(green).max(blue) as i32;
    let minimum = red.min(green).min(blue) as i32;
    let chroma = maximum - minimum;
    if chroma < 40 {
        return Color::Reset;
    }

    let (red, green, blue) = (red as i32, green as i32, blue as i32);
    let hue = if maximum == red {
        let mut hue = (green - blue) * 60 / chroma;
        if hue < 0 {
            hue += 360;
        }
        hue
    } else if maximum == green {
        (blue - red) * 60 / chroma + 120
    } else {
        (red - green) * 60 / chroma + 240
    };

    match hue {
        0..30 | 330..=360 => Color::Red,
        30..90 => Color::Yellow,
        90..150 => Color::Green,
        150..210 => Color::Cyan,
        210..255 => Color::Blue,
        _ => Color::Magenta,
    }
}

/// Highlight one source line, preserving a plain styled fallback.
#[must_use]
pub fn highlight_line(
    text: &str,
    highlighter: &mut Option<syntect::easy::HighlightLines<'_>>,
    syntect: &Syntect,
    fallback: Style,
) -> Vec<Span<'static>> {
    if let Some(highlighter) = highlighter.as_mut()
        && let Ok(ranges) = highlighter.highlight_line(&format!("{text}\n"), &syntect.syntax_set)
    {
        let mut spans = Vec::new();
        for (style, segment) in ranges {
            let mut segment = segment.to_owned();
            while segment.ends_with('\n') || segment.ends_with('\r') {
                segment.pop();
            }
            if !segment.is_empty() {
                spans.push(Span::styled(segment, syntect_to_ratatui_fg(style)));
            }
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(text.to_owned(), fallback)]
}

fn syntect_for_kind(kind: ThemeKind) -> &'static Syntect {
    match kind {
        ThemeKind::Dark | ThemeKind::DarkDaltonized | ThemeKind::DarkAnsi | ThemeKind::Auto => {
            SYNTECT_DARK
                .get_or_init(|| Syntect::new(include_bytes!("../assets/crabcode-dark.tmTheme")))
        }
        ThemeKind::Light | ThemeKind::LightDaltonized | ThemeKind::LightAnsi => SYNTECT_LIGHT
            .get_or_init(|| Syntect::new(include_bytes!("../assets/crabcode-light.tmTheme"))),
    }
}

/// Return the syntax highlighter matching the active renderer theme.
#[must_use]
pub fn get_syntect() -> &'static Syntect {
    syntect_for_kind(crate::theme::Theme::current_kind())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::cache as theme_cache;

    fn with_theme_state<T>(test: impl FnOnce() -> T) -> T {
        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        theme_cache::reset_for_test();
        let output = test();
        theme_cache::reset_for_test();
        output
    }

    #[test]
    fn fixed_theme_kind_denominator_maps_to_dark_or_light_asset() {
        for kind in [
            ThemeKind::Dark,
            ThemeKind::DarkDaltonized,
            ThemeKind::DarkAnsi,
            ThemeKind::Auto,
        ] {
            assert_eq!(
                syntect_for_kind(kind).theme.name.as_deref(),
                Some("CrabCode Dark"),
            );
        }
        for kind in [
            ThemeKind::Light,
            ThemeKind::LightDaltonized,
            ThemeKind::LightAnsi,
        ] {
            assert_eq!(
                syntect_for_kind(kind).theme.name.as_deref(),
                Some("CrabCode Light"),
            );
        }
    }

    #[test]
    fn polarity_safe_grays_inherit_terminal_foreground() {
        assert_eq!(polarity_safe_syntax_fg(0xc8, 0xc8, 0xc8), Color::Reset);
        assert_eq!(polarity_safe_syntax_fg(0x6c, 0x6c, 0x6c), Color::Reset);
    }

    #[test]
    fn polarity_safe_projection_never_emits_polarity_specific_slots() {
        let samples = [
            (0xbb, 0x9a, 0xf7),
            (0x7d, 0xcf, 0xff),
            (0x7a, 0xa2, 0xf7),
            (0xff, 0x9e, 0x64),
            (0xf7, 0x76, 0x8e),
            (0xe0, 0xaf, 0x68),
            (0x9e, 0xce, 0x6a),
            (0xc8, 0xc8, 0xc8),
        ];
        for (red, green, blue) in samples {
            let color = polarity_safe_syntax_fg(red, green, blue);
            assert!(
                !matches!(
                    color,
                    Color::White
                        | Color::Black
                        | Color::Gray
                        | Color::DarkGray
                        | Color::LightRed
                        | Color::LightGreen
                        | Color::LightYellow
                        | Color::LightBlue
                        | Color::LightMagenta
                        | Color::LightCyan
                ),
                "unsafe projection for #{red:02x}{green:02x}{blue:02x}: {color:?}",
            );
        }
    }

    #[test]
    fn terminal_native_lock_selects_dark_asset_and_safe_token_projection() {
        with_theme_state(|| {
            theme_cache::apply_setting(ThemeKind::Light);
            assert_eq!(get_syntect().theme.name.as_deref(), Some("CrabCode Light"));

            theme_cache::set_terminal_native_lock(true);
            assert_eq!(get_syntect().theme.name.as_deref(), Some("CrabCode Dark"));
            assert_eq!(syntect_rgb_to_fg(0xc8, 0xc8, 0xc8), Color::Reset);
            assert_eq!(syntect_rgb_to_fg(0xbb, 0x9a, 0xf7), Color::Magenta);
        });
    }

    #[test]
    fn highlight_line_preserves_fallback_and_syntax_modifiers() {
        let syntect = syntect_for_kind(ThemeKind::Dark);
        let fallback = Style::default().fg(Color::Reset);
        let mut absent = None;
        let fallback_spans = highlight_line("fn main() {}", &mut absent, syntect, fallback);
        assert_eq!(fallback_spans, vec![Span::styled("fn main() {}", fallback)]);

        let mut rust = syntect.highlight_lines_for_token("rust");
        let highlighted = highlight_line("fn main() {}", &mut rust, syntect, fallback);
        assert!(!highlighted.is_empty());
        assert_eq!(
            highlighted
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "fn main() {}",
        );
    }
}
