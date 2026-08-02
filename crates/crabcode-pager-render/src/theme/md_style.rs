//! Theme-aware Markdown rendering style.
//!
//! This is the fixed renderer's Markdown-style bridge adapted to the audited
//! CrabCode palette. It is presentation-only: no config, backend, or protocol
//! authority is introduced here.

use anstyle::{Ansi256Color, AnsiColor, Color, Style};
use crabcode_markdown_renderer::MarkdownStyle;

use super::Theme;

/// Convert a ratatui color to the equivalent anstyle color.
///
/// The current [`Theme`] is already quantized for the terminal. `Reset`
/// intentionally becomes an unset color so the terminal retains authority
/// over its default foreground/background.
fn to_anstyle(color: ratatui::style::Color) -> Option<Color> {
    Some(match color {
        ratatui::style::Color::Reset => return None,
        ratatui::style::Color::Rgb(red, green, blue) => {
            Color::Rgb(anstyle::RgbColor(red, green, blue))
        }
        ratatui::style::Color::Indexed(index) => Color::Ansi256(Ansi256Color(index)),
        ratatui::style::Color::Black => Color::Ansi(AnsiColor::Black),
        ratatui::style::Color::Red => Color::Ansi(AnsiColor::Red),
        ratatui::style::Color::Green => Color::Ansi(AnsiColor::Green),
        ratatui::style::Color::Yellow => Color::Ansi(AnsiColor::Yellow),
        ratatui::style::Color::Blue => Color::Ansi(AnsiColor::Blue),
        ratatui::style::Color::Magenta => Color::Ansi(AnsiColor::Magenta),
        ratatui::style::Color::Cyan => Color::Ansi(AnsiColor::Cyan),
        ratatui::style::Color::Gray => Color::Ansi(AnsiColor::White),
        ratatui::style::Color::DarkGray => Color::Ansi(AnsiColor::BrightBlack),
        ratatui::style::Color::LightRed => Color::Ansi(AnsiColor::BrightRed),
        ratatui::style::Color::LightGreen => Color::Ansi(AnsiColor::BrightGreen),
        ratatui::style::Color::LightYellow => Color::Ansi(AnsiColor::BrightYellow),
        ratatui::style::Color::LightBlue => Color::Ansi(AnsiColor::BrightBlue),
        ratatui::style::Color::LightMagenta => Color::Ansi(AnsiColor::BrightMagenta),
        ratatui::style::Color::LightCyan => Color::Ansi(AnsiColor::BrightCyan),
        ratatui::style::Color::White => Color::Ansi(AnsiColor::BrightWhite),
    })
}

fn foreground(color: ratatui::style::Color) -> Style {
    Style::new().fg_color(to_anstyle(color))
}

fn background(color: ratatui::style::Color) -> Style {
    Style::new().bg_color(to_anstyle(color))
}

fn modifier_to_anstyle(modifier: ratatui::style::Modifier) -> Style {
    let mut style = Style::new();
    if modifier.contains(ratatui::style::Modifier::BOLD) {
        style = style.bold();
    }
    if modifier.contains(ratatui::style::Modifier::ITALIC) {
        style = style.italic();
    }
    if modifier.contains(ratatui::style::Modifier::UNDERLINED) {
        style = style.underline();
    }
    if modifier.contains(ratatui::style::Modifier::DIM) {
        style = style.dimmed();
    }
    if modifier.contains(ratatui::style::Modifier::HIDDEN) {
        style = style.hidden();
    }
    if modifier.contains(ratatui::style::Modifier::CROSSED_OUT) {
        style = style.strikethrough();
    }
    style
}

fn heading_inner_styles(
    colors: [ratatui::style::Color; 6],
    modifiers: [ratatui::style::Modifier; 6],
) -> [Style; 6] {
    std::array::from_fn(|index| {
        let mut style = foreground(colors[index]);
        let effects = modifier_to_anstyle(modifiers[index]).get_effects();
        if !effects.is_plain() {
            style = style.effects(style.get_effects() | effects);
        }
        style
    })
}

fn heading_outer_styles(colors: [ratatui::style::Color; 6]) -> [Style; 6] {
    colors.map(|color| foreground(color).dimmed().hidden())
}

/// Build the Markdown style for the active renderer theme.
#[must_use]
pub fn style() -> MarkdownStyle {
    build_style(Theme::current())
}

fn build_style(theme: Theme) -> MarkdownStyle {
    let heading_colors = [
        theme.markdown_h1,
        theme.markdown_h2,
        theme.markdown_h3,
        theme.markdown_h4,
        theme.markdown_h5,
        theme.markdown_h6,
    ];
    let heading_modifiers = [
        theme.markdown_h1_mod,
        theme.markdown_h2_mod,
        theme.markdown_h3_mod,
        theme.markdown_h4_mod,
        theme.markdown_h5_mod,
        theme.markdown_h6_mod,
    ];

    MarkdownStyle {
        heading_inner: heading_inner_styles(heading_colors, heading_modifiers),
        heading_outer: heading_outer_styles(heading_colors),
        strong_inner: foreground(theme.markdown_text).bold(),
        strong_outer: Style::new().dimmed().hidden(),
        emphasis_inner: foreground(theme.markdown_text).italic(),
        emphasis_outer: Style::new().dimmed().hidden(),
        strikethrough_inner: foreground(theme.markdown_text).strikethrough(),
        strikethrough_outer: Style::new().dimmed().hidden(),
        inline_code_inner: foreground(theme.markdown_code).bold(),
        inline_code_outer: foreground(theme.markdown_code).dimmed().hidden(),
        blockquote_outer: foreground(theme.markdown_muted).dimmed(),
        task_checked: foreground(theme.markdown_task_checked),
        task_unchecked: foreground(theme.markdown_task_unchecked).dimmed(),
        list_item: foreground(theme.markdown_muted),
        rule: foreground(theme.markdown_muted),
        link_outer: foreground(theme.markdown_muted),
        link_text: foreground(theme.link).underline(),
        link_url: foreground(theme.markdown_muted),
        link_title: foreground(theme.markdown_h5),
        code_outer: foreground(theme.markdown_code).dimmed().hidden(),
        code_language: foreground(theme.markdown_h3).hidden(),
        code_untagged: foreground(theme.markdown_text),
        code_background: background(theme.markdown_code_bg),
        table_outer: foreground(theme.markdown_h2).hidden(),
        text: foreground(theme.markdown_text),
        math: foreground(theme.markdown_text).italic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anstyle::Effects;
    use ratatui::style::{Color as RatatuiColor, Modifier};

    #[test]
    fn reset_maps_to_terminal_default() {
        assert_eq!(to_anstyle(RatatuiColor::Reset), None);
        assert_eq!(foreground(RatatuiColor::Reset).get_fg_color(), None);
        assert_eq!(background(RatatuiColor::Reset).get_bg_color(), None);
    }

    #[test]
    fn named_colors_map_without_gray_brightness_inversion() {
        assert_eq!(
            to_anstyle(RatatuiColor::DarkGray),
            Some(Color::Ansi(AnsiColor::BrightBlack)),
        );
        assert_eq!(
            to_anstyle(RatatuiColor::Gray),
            Some(Color::Ansi(AnsiColor::White)),
        );
        assert_eq!(
            to_anstyle(RatatuiColor::Red),
            Some(Color::Ansi(AnsiColor::Red)),
        );
    }

    #[test]
    fn every_markdown_theme_role_maps_to_its_fixed_style_slot() {
        let mut theme = Theme::dark();
        theme.markdown_h1 = RatatuiColor::Red;
        theme.markdown_h2 = RatatuiColor::Green;
        theme.markdown_h3 = RatatuiColor::Yellow;
        theme.markdown_h4 = RatatuiColor::Blue;
        theme.markdown_h5 = RatatuiColor::Magenta;
        theme.markdown_h6 = RatatuiColor::Cyan;
        theme.markdown_h1_mod = Modifier::BOLD;
        theme.markdown_h2_mod = Modifier::ITALIC;
        theme.markdown_h3_mod = Modifier::UNDERLINED;
        theme.markdown_h4_mod = Modifier::DIM;
        theme.markdown_h5_mod = Modifier::CROSSED_OUT;
        theme.markdown_h6_mod = Modifier::empty();
        theme.markdown_code = RatatuiColor::LightBlue;
        theme.markdown_task_checked = RatatuiColor::LightGreen;
        theme.markdown_task_unchecked = RatatuiColor::LightRed;
        theme.markdown_muted = RatatuiColor::DarkGray;
        theme.markdown_code_bg = RatatuiColor::Indexed(236);
        theme.markdown_text = RatatuiColor::White;
        theme.link = RatatuiColor::LightCyan;

        let mapped = build_style(theme);

        assert_eq!(
            mapped.heading_inner.map(|style| style.get_fg_color()),
            [
                Some(Color::Ansi(AnsiColor::Red)),
                Some(Color::Ansi(AnsiColor::Green)),
                Some(Color::Ansi(AnsiColor::Yellow)),
                Some(Color::Ansi(AnsiColor::Blue)),
                Some(Color::Ansi(AnsiColor::Magenta)),
                Some(Color::Ansi(AnsiColor::Cyan)),
            ],
        );
        assert!(
            mapped.heading_inner[0]
                .get_effects()
                .contains(Effects::BOLD)
        );
        assert!(
            mapped.heading_inner[1]
                .get_effects()
                .contains(Effects::ITALIC)
        );
        assert!(
            mapped.heading_inner[2]
                .get_effects()
                .contains(Effects::UNDERLINE)
        );
        assert!(
            mapped.heading_inner[3]
                .get_effects()
                .contains(Effects::DIMMED)
        );
        assert!(
            mapped.heading_inner[4]
                .get_effects()
                .contains(Effects::STRIKETHROUGH)
        );
        assert_eq!(
            mapped.inline_code_inner.get_fg_color(),
            to_anstyle(theme.markdown_code)
        );
        assert_eq!(
            mapped.task_checked.get_fg_color(),
            to_anstyle(theme.markdown_task_checked),
        );
        assert_eq!(
            mapped.task_unchecked.get_fg_color(),
            to_anstyle(theme.markdown_task_unchecked),
        );
        assert_eq!(
            mapped.blockquote_outer.get_fg_color(),
            to_anstyle(theme.markdown_muted),
        );
        assert_eq!(
            mapped.code_background.get_bg_color(),
            to_anstyle(theme.markdown_code_bg),
        );
        assert_eq!(mapped.text.get_fg_color(), to_anstyle(theme.markdown_text));
        assert_eq!(mapped.link_text.get_fg_color(), to_anstyle(theme.link));
    }
}
