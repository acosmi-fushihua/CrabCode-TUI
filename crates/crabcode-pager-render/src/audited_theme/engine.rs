//! Backend-independent theme model and product palette adapter.
//!
//! The semantic role set, day/night mother palettes, quantization lifecycle,
//! and automatic-appearance behavior are adapted from the fixed xAI Rust TUI
//! source pin. Product colors are then applied only where the historical
//! CrabCode direct TUI has an exact semantic source-field match.
//!
//! Historical product palette anchor:
//! - commit: `2358212c2df2018816058c8a03b1ac3d324e74e0`
//! - path: `src/utils/theme.ts`
//! - SHA-256: `275bc5c675525525ba981b59948b264c11f2243203fc72e7aa8d4203b4b36a36`
//!
//! No configuration or backend authority lives here. The direct renderer
//! injects one of the seven [`CrabCodeThemeKind`] values explicitly.

use ratatui::style::{Color, Modifier, Style};

use super::{cache, color_support};

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

/// Complete semantic palette consumed by the Rust TUI renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrabCodeTheme {
    // Backgrounds.
    pub bg_base: Color,
    pub bg_light: Color,
    pub bg_dark: Color,
    pub bg_highlight: Color,
    pub bg_hover: Color,
    pub bg_terminal: Color,

    // Content/state accents.
    pub accent_user: Color,
    pub accent_assistant: Color,
    pub accent_thinking: Color,
    pub accent_tool: Color,
    pub accent_system: Color,
    pub accent_error: Color,
    pub accent_success: Color,
    pub accent_running: Color,
    pub accent_skill: Color,
    /// Historical direct-TUI `background` foreground role (cyan status glyphs).
    pub background_accent: Color,

    // Text hierarchy.
    pub text_primary: Color,
    pub text_secondary: Color,
    pub gray_dim: Color,
    pub gray: Color,
    pub gray_bright: Color,

    // Semantic foregrounds.
    pub command: Color,
    pub path: Color,
    pub running: Color,
    pub warning: Color,
    pub fuzzy_accent: Color,
    pub accent_plan: Color,
    pub accent_verify: Color,
    pub accent_feedback: Color,
    pub accent_remember: Color,

    // Selection and prompt chrome.
    pub selection_border: Color,
    pub hover_border: Color,
    pub prompt_border: Color,
    pub prompt_border_active: Color,
    pub accent_model: Color,
    pub scrollbar_bg: Color,
    pub scrollbar_fg: Color,

    // Diffs.
    pub diff_delete_bg: Color,
    pub diff_delete_fg: Color,
    pub diff_insert_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_equal_fg: Color,
    pub diff_gutter_fg: Color,

    // Selection and paste surfaces.
    pub bg_visual: Color,
    pub paste_bg: Color,
    pub paste_fg: Color,
    pub paste_dim: Color,

    // Markdown.
    pub markdown_h1: Color,
    pub markdown_h1_mod: Modifier,
    pub markdown_h2: Color,
    pub markdown_h2_mod: Modifier,
    pub markdown_h3: Color,
    pub markdown_h3_mod: Modifier,
    pub markdown_h4: Color,
    pub markdown_h4_mod: Modifier,
    pub markdown_h5: Color,
    pub markdown_h5_mod: Modifier,
    pub markdown_h6: Color,
    pub markdown_h6_mod: Modifier,
    pub markdown_code: Color,
    pub markdown_task_checked: Color,
    pub markdown_task_unchecked: Color,
    pub markdown_muted: Color,
    pub markdown_code_bg: Color,
    pub markdown_text: Color,
    pub link: Color,
}

/// Renderer-facing theme setting.
///
/// The canonical spellings are exactly the historical direct-TUI setting
/// denominator. `Auto` is a meta-setting and is never returned by
/// [`CrabCodeTheme::current_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CrabCodeThemeKind {
    Dark = 0,
    Light = 1,
    LightDaltonized = 2,
    DarkDaltonized = 3,
    LightAnsi = 4,
    DarkAnsi = 5,
    Auto = 6,
}

impl CrabCodeThemeKind {
    pub const ALL: &[Self] = &[
        Self::Dark,
        Self::Light,
        Self::LightDaltonized,
        Self::DarkDaltonized,
        Self::LightAnsi,
        Self::DarkAnsi,
    ];

    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::LightDaltonized => "light-daltonized",
            Self::DarkDaltonized => "dark-daltonized",
            Self::LightAnsi => "light-ansi",
            Self::DarkAnsi => "dark-ansi",
            Self::Auto => "auto",
        }
    }

    /// Parse only the seven protocol-authoritative spellings.
    ///
    /// Case folding matches the fixed Rust TUI parser lifecycle, while
    /// intentionally adding no aliases and therefore no new protocol surface.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "light-daltonized" => Some(Self::LightDaltonized),
            "dark-daltonized" => Some(Self::DarkDaltonized),
            "light-ansi" => Some(Self::LightAnsi),
            "dark-ansi" => Some(Self::DarkAnsi),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    #[must_use]
    pub const fn is_ansi(self) -> bool {
        matches!(self, Self::LightAnsi | Self::DarkAnsi)
    }

    #[must_use]
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark | Self::DarkDaltonized | Self::DarkAnsi)
    }
}

impl std::str::FromStr for CrabCodeThemeKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_name(value).ok_or(())
    }
}

impl std::fmt::Display for CrabCodeThemeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.canonical_name())
    }
}

/// Provenance classification for a product palette role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductRoleSource {
    /// Same semantic role exists in the historical direct-TUI `Theme`.
    DirectHistorical(&'static str),
    /// No exact historical role exists; retain the fixed Rust TUI mother value.
    FixedMother,
}

/// Auditable denominator covering every field in [`CrabCodeTheme`].
///
/// Modifier entries are included even though they are not colors. This makes a
/// future field addition fail the denominator test until its provenance is
/// classified explicitly.
pub const PRODUCT_ROLE_AUDIT: &[(&str, ProductRoleSource)] = &[
    ("bg_base", ProductRoleSource::FixedMother),
    ("bg_light", ProductRoleSource::FixedMother),
    ("bg_dark", ProductRoleSource::FixedMother),
    ("bg_highlight", ProductRoleSource::FixedMother),
    ("bg_hover", ProductRoleSource::FixedMother),
    ("bg_terminal", ProductRoleSource::FixedMother),
    ("accent_user", ProductRoleSource::FixedMother),
    (
        "accent_assistant",
        ProductRoleSource::DirectHistorical("crabCode"),
    ),
    ("accent_thinking", ProductRoleSource::FixedMother),
    ("accent_tool", ProductRoleSource::FixedMother),
    (
        "accent_system",
        ProductRoleSource::DirectHistorical("crabCodeBlue_FOR_SYSTEM_SPINNER"),
    ),
    ("accent_error", ProductRoleSource::DirectHistorical("error")),
    (
        "accent_success",
        ProductRoleSource::DirectHistorical("success"),
    ),
    ("accent_running", ProductRoleSource::FixedMother),
    ("accent_skill", ProductRoleSource::FixedMother),
    (
        "background_accent",
        ProductRoleSource::DirectHistorical("background"),
    ),
    ("text_primary", ProductRoleSource::DirectHistorical("text")),
    (
        "text_secondary",
        ProductRoleSource::DirectHistorical("inactive"),
    ),
    ("gray_dim", ProductRoleSource::DirectHistorical("subtle")),
    ("gray", ProductRoleSource::FixedMother),
    ("gray_bright", ProductRoleSource::FixedMother),
    ("command", ProductRoleSource::FixedMother),
    ("path", ProductRoleSource::FixedMother),
    ("running", ProductRoleSource::FixedMother),
    ("warning", ProductRoleSource::DirectHistorical("warning")),
    ("fuzzy_accent", ProductRoleSource::FixedMother),
    ("accent_plan", ProductRoleSource::FixedMother),
    ("accent_verify", ProductRoleSource::FixedMother),
    ("accent_feedback", ProductRoleSource::FixedMother),
    ("accent_remember", ProductRoleSource::FixedMother),
    ("selection_border", ProductRoleSource::FixedMother),
    ("hover_border", ProductRoleSource::FixedMother),
    (
        "prompt_border",
        ProductRoleSource::DirectHistorical("promptBorder"),
    ),
    (
        "prompt_border_active",
        ProductRoleSource::DirectHistorical("promptBorderShimmer"),
    ),
    ("accent_model", ProductRoleSource::FixedMother),
    ("scrollbar_bg", ProductRoleSource::FixedMother),
    ("scrollbar_fg", ProductRoleSource::FixedMother),
    (
        "diff_delete_bg",
        ProductRoleSource::DirectHistorical("diffRemoved"),
    ),
    ("diff_delete_fg", ProductRoleSource::FixedMother),
    (
        "diff_insert_bg",
        ProductRoleSource::DirectHistorical("diffAdded"),
    ),
    ("diff_insert_fg", ProductRoleSource::FixedMother),
    ("diff_equal_fg", ProductRoleSource::FixedMother),
    ("diff_gutter_fg", ProductRoleSource::FixedMother),
    ("bg_visual", ProductRoleSource::FixedMother),
    ("paste_bg", ProductRoleSource::FixedMother),
    ("paste_fg", ProductRoleSource::FixedMother),
    ("paste_dim", ProductRoleSource::FixedMother),
    ("markdown_h1", ProductRoleSource::FixedMother),
    ("markdown_h1_mod", ProductRoleSource::FixedMother),
    ("markdown_h2", ProductRoleSource::FixedMother),
    ("markdown_h2_mod", ProductRoleSource::FixedMother),
    ("markdown_h3", ProductRoleSource::FixedMother),
    ("markdown_h3_mod", ProductRoleSource::FixedMother),
    ("markdown_h4", ProductRoleSource::FixedMother),
    ("markdown_h4_mod", ProductRoleSource::FixedMother),
    ("markdown_h5", ProductRoleSource::FixedMother),
    ("markdown_h5_mod", ProductRoleSource::FixedMother),
    ("markdown_h6", ProductRoleSource::FixedMother),
    ("markdown_h6_mod", ProductRoleSource::FixedMother),
    ("markdown_code", ProductRoleSource::FixedMother),
    ("markdown_task_checked", ProductRoleSource::FixedMother),
    ("markdown_task_unchecked", ProductRoleSource::FixedMother),
    ("markdown_muted", ProductRoleSource::FixedMother),
    ("markdown_code_bg", ProductRoleSource::FixedMother),
    ("markdown_text", ProductRoleSource::FixedMother),
    ("link", ProductRoleSource::FixedMother),
];

#[derive(Debug, Clone, Copy)]
struct ProductPalette {
    text_primary: Color,
    text_secondary: Color,
    gray_dim: Color,
    accent_assistant: Color,
    accent_system: Color,
    accent_error: Color,
    accent_success: Color,
    background_accent: Color,
    warning: Color,
    prompt_border: Color,
    prompt_border_active: Color,
    diff_insert_bg: Color,
    diff_delete_bg: Color,
}

impl CrabCodeTheme {
    /// Terminal-native palette used by minimal mode.
    ///
    /// Every color is either `Reset` or a named ANSI-16 accent, so the
    /// terminal's own foreground/background remain authoritative regardless
    /// of host-profile polarity.
    pub const fn terminal_default() -> Self {
        const MUTED: Color = Color::Reset;

        Self {
            bg_base: Color::Reset,
            bg_light: Color::Reset,
            bg_dark: Color::Reset,
            bg_highlight: Color::Reset,
            bg_hover: Color::Reset,
            bg_terminal: Color::Reset,
            accent_user: Color::Reset,
            accent_assistant: Color::Magenta,
            accent_thinking: MUTED,
            accent_tool: MUTED,
            accent_system: Color::Blue,
            accent_error: Color::Red,
            accent_success: Color::Green,
            accent_running: Color::Magenta,
            accent_skill: Color::Blue,
            background_accent: Color::Cyan,
            text_primary: Color::Reset,
            text_secondary: MUTED,
            gray_dim: MUTED,
            gray: MUTED,
            gray_bright: Color::Reset,
            command: Color::Yellow,
            path: Color::Cyan,
            running: Color::Cyan,
            warning: Color::Yellow,
            fuzzy_accent: Color::Cyan,
            accent_plan: Color::Yellow,
            accent_verify: Color::Magenta,
            accent_feedback: Color::Cyan,
            accent_remember: Color::Green,
            selection_border: MUTED,
            hover_border: MUTED,
            prompt_border: MUTED,
            prompt_border_active: Color::Reset,
            accent_model: Color::Cyan,
            scrollbar_bg: Color::Reset,
            scrollbar_fg: MUTED,
            diff_delete_bg: Color::Reset,
            diff_delete_fg: Color::Red,
            diff_insert_bg: Color::Reset,
            diff_insert_fg: Color::Green,
            diff_equal_fg: MUTED,
            diff_gutter_fg: MUTED,
            bg_visual: Color::Reset,
            paste_bg: Color::Reset,
            paste_fg: MUTED,
            paste_dim: MUTED,
            markdown_h1: Color::Reset,
            markdown_h1_mod: Modifier::BOLD.union(Modifier::UNDERLINED),
            markdown_h2: Color::Reset,
            markdown_h2_mod: Modifier::BOLD,
            markdown_h3: Color::Reset,
            markdown_h3_mod: Modifier::BOLD,
            markdown_h4: Color::Reset,
            markdown_h4_mod: Modifier::BOLD,
            markdown_h5: Color::Reset,
            markdown_h5_mod: Modifier::BOLD,
            markdown_h6: MUTED,
            markdown_h6_mod: Modifier::BOLD,
            markdown_code: Color::Cyan,
            markdown_task_checked: Color::Green,
            markdown_task_unchecked: MUTED,
            markdown_muted: MUTED,
            markdown_code_bg: Color::Reset,
            markdown_text: Color::Reset,
            link: Color::Blue,
        }
    }

    /// Backward-compatible constant while the application finishes injecting
    /// [`Self::current`] into every render path.
    pub const NIGHT: Self = Self::dark();
    pub const DAY: Self = Self::light();

    /// Fixed upstream neutral dark mother palette.
    #[must_use]
    pub const fn fixed_night_mother() -> Self {
        Self {
            bg_base: rgb(20, 20, 20),
            bg_light: rgb(36, 36, 36),
            bg_dark: rgb(28, 28, 28),
            bg_highlight: rgb(36, 36, 36),
            bg_hover: rgb(44, 44, 44),
            bg_terminal: rgb(10, 10, 10),

            accent_user: rgb(200, 200, 200),
            accent_assistant: rgb(187, 154, 247),
            accent_thinking: rgb(187, 154, 247),
            accent_tool: rgb(120, 120, 120),
            accent_system: rgb(122, 162, 247),
            accent_error: rgb(247, 118, 142),
            accent_success: rgb(158, 206, 106),
            accent_running: rgb(187, 154, 247),
            accent_skill: rgb(122, 162, 247),
            background_accent: Color::Reset,

            text_primary: rgb(225, 225, 225),
            text_secondary: rgb(200, 200, 200),
            gray_dim: rgb(88, 88, 88),
            gray: rgb(108, 108, 108),
            gray_bright: rgb(120, 120, 120),

            command: rgb(224, 175, 104),
            path: rgb(255, 158, 100),
            running: rgb(125, 207, 255),
            warning: rgb(224, 175, 104),
            fuzzy_accent: rgb(122, 162, 247),
            accent_plan: rgb(255, 219, 141),
            accent_verify: rgb(187, 154, 247),
            accent_feedback: rgb(115, 218, 202),
            accent_remember: rgb(139, 195, 74),

            selection_border: rgb(60, 60, 65),
            hover_border: rgb(30, 30, 34),
            prompt_border: rgb(50, 50, 55),
            prompt_border_active: rgb(80, 80, 88),
            accent_model: rgb(26, 188, 156),
            scrollbar_bg: rgb(17, 17, 17),
            scrollbar_fg: rgb(36, 36, 36),

            diff_delete_bg: rgb(66, 14, 20),
            diff_delete_fg: rgb(247, 118, 142),
            diff_insert_bg: rgb(6, 56, 6),
            diff_insert_fg: rgb(158, 206, 106),
            diff_equal_fg: rgb(108, 108, 108),
            diff_gutter_fg: rgb(108, 108, 108),

            bg_visual: rgb(54, 54, 54),
            paste_bg: rgb(17, 17, 17),
            paste_fg: rgb(200, 200, 200),
            paste_dim: rgb(65, 65, 65),

            markdown_h1: rgb(26, 188, 156),
            markdown_h1_mod: Modifier::BOLD,
            markdown_h2: rgb(122, 162, 247),
            markdown_h2_mod: Modifier::BOLD,
            markdown_h3: rgb(157, 124, 216),
            markdown_h3_mod: Modifier::BOLD,
            markdown_h4: rgb(120, 120, 120),
            markdown_h4_mod: Modifier::BOLD,
            markdown_h5: rgb(108, 108, 108),
            markdown_h5_mod: Modifier::BOLD,
            markdown_h6: rgb(90, 90, 90),
            markdown_h6_mod: Modifier::empty(),
            markdown_code: rgb(58, 149, 171),
            markdown_task_checked: rgb(158, 206, 106),
            markdown_task_unchecked: rgb(200, 200, 200),
            markdown_muted: rgb(108, 108, 108),
            markdown_code_bg: rgb(28, 28, 28),
            markdown_text: rgb(200, 200, 200),
            link: rgb(122, 166, 218),
        }
    }

    /// Fixed upstream neutral light mother palette.
    #[must_use]
    pub const fn fixed_day_mother() -> Self {
        Self {
            bg_base: rgb(238, 238, 238),
            bg_light: rgb(222, 222, 222),
            bg_dark: rgb(228, 228, 228),
            bg_highlight: rgb(222, 222, 222),
            bg_hover: rgb(208, 208, 208),
            bg_terminal: rgb(245, 245, 245),

            accent_user: rgb(68, 68, 68),
            accent_assistant: rgb(125, 75, 198),
            accent_thinking: rgb(125, 75, 198),
            accent_tool: rgb(98, 98, 98),
            accent_system: rgb(47, 100, 210),
            accent_error: rgb(205, 48, 72),
            accent_success: rgb(55, 142, 35),
            accent_running: rgb(125, 75, 198),
            accent_skill: rgb(47, 100, 210),
            background_accent: Color::Reset,

            text_primary: rgb(38, 38, 38),
            text_secondary: rgb(68, 68, 68),
            gray_dim: rgb(165, 165, 165),
            gray: rgb(118, 118, 118),
            gray_bright: rgb(98, 98, 98),

            command: rgb(162, 118, 18),
            path: rgb(195, 105, 30),
            running: rgb(0, 130, 170),
            warning: rgb(162, 118, 18),
            fuzzy_accent: rgb(47, 100, 210),
            accent_plan: rgb(168, 120, 10),
            accent_verify: rgb(120, 80, 160),
            accent_feedback: rgb(12, 148, 124),
            accent_remember: rgb(76, 175, 80),

            selection_border: rgb(185, 185, 190),
            hover_border: rgb(212, 212, 216),
            prompt_border: rgb(200, 200, 205),
            prompt_border_active: rgb(165, 165, 175),
            accent_model: rgb(10, 142, 112),
            scrollbar_bg: rgb(234, 234, 234),
            scrollbar_fg: rgb(222, 222, 222),

            diff_delete_bg: rgb(245, 218, 222),
            diff_delete_fg: rgb(205, 48, 72),
            diff_insert_bg: rgb(218, 242, 220),
            diff_insert_fg: rgb(55, 142, 35),
            diff_equal_fg: rgb(118, 118, 118),
            diff_gutter_fg: rgb(118, 118, 118),

            bg_visual: rgb(198, 198, 198),
            paste_bg: rgb(222, 222, 222),
            paste_fg: rgb(68, 68, 68),
            paste_dim: rgb(178, 178, 178),

            markdown_h1: rgb(10, 142, 112),
            markdown_h1_mod: Modifier::BOLD,
            markdown_h2: rgb(47, 100, 210),
            markdown_h2_mod: Modifier::BOLD,
            markdown_h3: rgb(108, 62, 178),
            markdown_h3_mod: Modifier::BOLD,
            markdown_h4: rgb(98, 98, 98),
            markdown_h4_mod: Modifier::BOLD,
            markdown_h5: rgb(118, 118, 118),
            markdown_h5_mod: Modifier::BOLD,
            markdown_h6: rgb(142, 142, 142),
            markdown_h6_mod: Modifier::empty(),
            markdown_code: rgb(15, 135, 162),
            markdown_task_checked: rgb(55, 142, 35),
            markdown_task_unchecked: rgb(68, 68, 68),
            markdown_muted: rgb(118, 118, 118),
            markdown_code_bg: rgb(228, 228, 228),
            markdown_text: rgb(68, 68, 68),
            link: rgb(47, 100, 210),
        }
    }

    #[must_use]
    const fn with_product_palette(self, palette: ProductPalette) -> Self {
        Self {
            text_primary: palette.text_primary,
            text_secondary: palette.text_secondary,
            gray_dim: palette.gray_dim,
            accent_assistant: palette.accent_assistant,
            accent_system: palette.accent_system,
            accent_error: palette.accent_error,
            accent_success: palette.accent_success,
            background_accent: palette.background_accent,
            warning: palette.warning,
            prompt_border: palette.prompt_border,
            prompt_border_active: palette.prompt_border_active,
            diff_insert_bg: palette.diff_insert_bg,
            diff_delete_bg: palette.diff_delete_bg,
            ..self
        }
    }

    #[must_use]
    pub const fn dark() -> Self {
        Self::fixed_night_mother().with_product_palette(ProductPalette {
            text_primary: rgb(255, 255, 255),
            text_secondary: rgb(153, 153, 153),
            gray_dim: rgb(80, 80, 80),
            accent_assistant: rgb(215, 119, 87),
            accent_system: rgb(147, 165, 255),
            accent_error: rgb(255, 107, 128),
            accent_success: rgb(78, 186, 101),
            background_accent: rgb(0, 204, 204),
            warning: rgb(255, 193, 7),
            prompt_border: rgb(136, 136, 136),
            prompt_border_active: rgb(166, 166, 166),
            diff_insert_bg: rgb(34, 92, 43),
            diff_delete_bg: rgb(122, 41, 54),
        })
    }

    #[must_use]
    pub const fn light() -> Self {
        Self::fixed_day_mother().with_product_palette(ProductPalette {
            text_primary: rgb(0, 0, 0),
            text_secondary: rgb(102, 102, 102),
            gray_dim: rgb(175, 175, 175),
            accent_assistant: rgb(215, 119, 87),
            accent_system: rgb(87, 105, 247),
            accent_error: rgb(171, 43, 63),
            accent_success: rgb(44, 122, 57),
            background_accent: rgb(0, 153, 153),
            warning: rgb(150, 108, 30),
            prompt_border: rgb(153, 153, 153),
            prompt_border_active: rgb(183, 183, 183),
            diff_insert_bg: rgb(105, 219, 124),
            diff_delete_bg: rgb(255, 168, 180),
        })
    }

    #[must_use]
    pub const fn light_daltonized() -> Self {
        Self::fixed_day_mother().with_product_palette(ProductPalette {
            text_primary: rgb(0, 0, 0),
            text_secondary: rgb(102, 102, 102),
            gray_dim: rgb(175, 175, 175),
            accent_assistant: rgb(255, 153, 51),
            accent_system: rgb(51, 102, 255),
            accent_error: rgb(204, 0, 0),
            accent_success: rgb(0, 102, 153),
            background_accent: rgb(0, 153, 153),
            warning: rgb(255, 153, 0),
            prompt_border: rgb(153, 153, 153),
            prompt_border_active: rgb(183, 183, 183),
            diff_insert_bg: rgb(153, 204, 255),
            diff_delete_bg: rgb(255, 204, 204),
        })
    }

    #[must_use]
    pub const fn dark_daltonized() -> Self {
        Self::fixed_night_mother().with_product_palette(ProductPalette {
            text_primary: rgb(255, 255, 255),
            text_secondary: rgb(153, 153, 153),
            gray_dim: rgb(80, 80, 80),
            accent_assistant: rgb(255, 153, 51),
            accent_system: rgb(153, 204, 255),
            accent_error: rgb(255, 102, 102),
            accent_success: rgb(51, 153, 255),
            background_accent: rgb(0, 204, 204),
            warning: rgb(255, 204, 0),
            prompt_border: rgb(136, 136, 136),
            prompt_border_active: rgb(166, 166, 166),
            diff_insert_bg: rgb(0, 68, 102),
            diff_delete_bg: rgb(102, 0, 0),
        })
    }

    #[must_use]
    pub const fn light_ansi() -> Self {
        Self::fixed_day_mother().with_product_palette(ProductPalette {
            text_primary: Color::Black,
            text_secondary: Color::DarkGray,
            gray_dim: Color::DarkGray,
            accent_assistant: Color::LightRed,
            accent_system: Color::Blue,
            accent_error: Color::Red,
            accent_success: Color::Green,
            background_accent: Color::Cyan,
            warning: Color::Yellow,
            prompt_border: Color::Gray,
            prompt_border_active: Color::White,
            diff_insert_bg: Color::Green,
            diff_delete_bg: Color::Red,
        })
    }

    #[must_use]
    pub const fn dark_ansi() -> Self {
        Self::fixed_night_mother().with_product_palette(ProductPalette {
            text_primary: Color::White,
            text_secondary: Color::Gray,
            gray_dim: Color::Gray,
            accent_assistant: Color::LightRed,
            accent_system: Color::LightBlue,
            accent_error: Color::LightRed,
            accent_success: Color::LightGreen,
            background_accent: Color::LightCyan,
            warning: Color::LightYellow,
            prompt_border: Color::Gray,
            prompt_border_active: Color::White,
            diff_insert_bg: Color::Green,
            diff_delete_bg: Color::Red,
        })
    }

    /// Build a raw palette. Quantization is deliberately separate so tests and
    /// adapters can distinguish source palette fidelity from terminal loss.
    #[must_use]
    pub const fn for_kind(kind: CrabCodeThemeKind) -> Self {
        match kind {
            CrabCodeThemeKind::Dark => Self::dark(),
            CrabCodeThemeKind::Light => Self::light(),
            CrabCodeThemeKind::LightDaltonized => Self::light_daltonized(),
            CrabCodeThemeKind::DarkDaltonized => Self::dark_daltonized(),
            CrabCodeThemeKind::LightAnsi => Self::light_ansi(),
            CrabCodeThemeKind::DarkAnsi => Self::dark_ansi(),
            // `Auto` is never a render palette. The deterministic fallback is
            // the fixed dark product palette.
            CrabCodeThemeKind::Auto => Self::dark(),
        }
    }

    /// Quantize every color role while preserving markdown modifiers.
    #[must_use]
    pub fn quantized(self, level: color_support::ColorLevel) -> Self {
        let q = |color| color_support::quantize_color(color, level);
        Self {
            bg_base: q(self.bg_base),
            bg_light: q(self.bg_light),
            bg_dark: q(self.bg_dark),
            bg_highlight: q(self.bg_highlight),
            bg_hover: q(self.bg_hover),
            bg_terminal: q(self.bg_terminal),
            accent_user: q(self.accent_user),
            accent_assistant: q(self.accent_assistant),
            accent_thinking: q(self.accent_thinking),
            accent_tool: q(self.accent_tool),
            accent_system: q(self.accent_system),
            accent_error: q(self.accent_error),
            accent_success: q(self.accent_success),
            accent_running: q(self.accent_running),
            accent_skill: q(self.accent_skill),
            background_accent: q(self.background_accent),
            text_primary: q(self.text_primary),
            text_secondary: q(self.text_secondary),
            gray_dim: q(self.gray_dim),
            gray: q(self.gray),
            gray_bright: q(self.gray_bright),
            command: q(self.command),
            path: q(self.path),
            running: q(self.running),
            warning: q(self.warning),
            fuzzy_accent: q(self.fuzzy_accent),
            accent_plan: q(self.accent_plan),
            accent_verify: q(self.accent_verify),
            accent_feedback: q(self.accent_feedback),
            accent_remember: q(self.accent_remember),
            selection_border: q(self.selection_border),
            hover_border: q(self.hover_border),
            prompt_border: q(self.prompt_border),
            prompt_border_active: q(self.prompt_border_active),
            accent_model: q(self.accent_model),
            scrollbar_bg: q(self.scrollbar_bg),
            scrollbar_fg: q(self.scrollbar_fg),
            diff_delete_bg: q(self.diff_delete_bg),
            diff_delete_fg: q(self.diff_delete_fg),
            diff_insert_bg: q(self.diff_insert_bg),
            diff_insert_fg: q(self.diff_insert_fg),
            diff_equal_fg: q(self.diff_equal_fg),
            diff_gutter_fg: q(self.diff_gutter_fg),
            bg_visual: q(self.bg_visual),
            paste_bg: q(self.paste_bg),
            paste_fg: q(self.paste_fg),
            paste_dim: q(self.paste_dim),
            markdown_h1: q(self.markdown_h1),
            markdown_h1_mod: self.markdown_h1_mod,
            markdown_h2: q(self.markdown_h2),
            markdown_h2_mod: self.markdown_h2_mod,
            markdown_h3: q(self.markdown_h3),
            markdown_h3_mod: self.markdown_h3_mod,
            markdown_h4: q(self.markdown_h4),
            markdown_h4_mod: self.markdown_h4_mod,
            markdown_h5: q(self.markdown_h5),
            markdown_h5_mod: self.markdown_h5_mod,
            markdown_h6: q(self.markdown_h6),
            markdown_h6_mod: self.markdown_h6_mod,
            markdown_code: q(self.markdown_code),
            markdown_task_checked: q(self.markdown_task_checked),
            markdown_task_unchecked: q(self.markdown_task_unchecked),
            markdown_muted: q(self.markdown_muted),
            markdown_code_bg: q(self.markdown_code_bg),
            markdown_text: q(self.markdown_text),
            link: q(self.link),
        }
    }

    /// Current concrete palette, adapted to the detected terminal color level.
    #[must_use]
    pub fn current() -> Self {
        let kind = cache::current_kind();
        let level = effective_level_for_kind(kind, color_support::detect());
        if cache::terminal_native_locked() {
            return Self::terminal_default().quantized(level);
        }
        Self::for_kind(kind).quantized(level)
    }

    /// Current concrete kind. This never returns [`CrabCodeThemeKind::Auto`].
    #[must_use]
    pub fn current_kind() -> CrabCodeThemeKind {
        cache::current_kind()
    }

    /// Whether this palette paints no diff row bands, in which case changed
    /// diff lines use a whole-line foreground instead of syntax highlighting
    /// over a colored background.
    #[must_use]
    pub fn diff_uses_line_fg(&self) -> bool {
        self.diff_delete_bg == Color::Reset && self.diff_insert_bg == Color::Reset
    }

    /// Apply a renderer-injected setting using runtime-safe appearance
    /// detection. This never performs an OSC 11 stdin probe.
    pub fn apply_kind(kind: CrabCodeThemeKind) -> CrabCodeThemeKind {
        if cache::terminal_native_locked() {
            return cache::current_kind();
        }
        cache::apply_setting(kind)
    }

    /// Apply a setting before any terminal input reader exists.
    ///
    /// Unlike [`Self::apply_kind`], `Auto` may use the startup-only OSC 11
    /// fallback. Calling this after crossterm input starts violates its stated
    /// precondition.
    pub fn apply_initial_kind(kind: CrabCodeThemeKind) -> CrabCodeThemeKind {
        if cache::terminal_native_locked() {
            return cache::current_kind();
        }
        cache::apply_setting_at_startup(kind)
    }

    /// Get a style with the given foreground color.
    #[must_use]
    pub const fn fg(&self, color: Color) -> Style {
        Style::new().fg(color)
    }

    #[must_use]
    pub const fn primary(&self) -> Style {
        Style::new().fg(self.text_primary)
    }

    #[must_use]
    pub const fn muted(&self) -> Style {
        match self.gray {
            Color::Reset => Style::new().add_modifier(Modifier::DIM),
            color => Style::new().fg(color),
        }
    }

    #[must_use]
    pub const fn dim(&self) -> Style {
        match self.gray_dim {
            Color::Reset => Style::new().add_modifier(Modifier::DIM),
            color => Style::new().fg(color),
        }
    }

    #[must_use]
    pub const fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    #[must_use]
    pub fn link_style(&self) -> Style {
        Style::new()
            .fg(self.link)
            .add_modifier(Modifier::UNDERLINED)
    }
}

impl Default for CrabCodeTheme {
    fn default() -> Self {
        Self::dark()
    }
}

#[must_use]
pub(crate) fn effective_level_for_kind(
    kind: CrabCodeThemeKind,
    detected: color_support::ColorLevel,
) -> color_support::ColorLevel {
    if kind.is_ansi() && detected > color_support::ColorLevel::Basic {
        color_support::ColorLevel::Basic
    } else {
        detected
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    macro_rules! assert_colors {
        ($theme:expr, $($field:ident => $expected:expr),+ $(,)?) => {{
            let theme = $theme;
            $(
                assert_eq!(
                    theme.$field,
                    $expected,
                    "source-parity mismatch for {}",
                    stringify!($field),
                );
            )+
        }};
    }

    #[test]
    fn canonical_setting_denominator_is_exact_and_alias_free() {
        let expected = [
            "dark",
            "light",
            "light-daltonized",
            "dark-daltonized",
            "light-ansi",
            "dark-ansi",
            "auto",
        ];
        for name in expected {
            let kind = CrabCodeThemeKind::from_name(name).expect("canonical setting");
            assert_eq!(kind.canonical_name(), name);
            assert_eq!(
                CrabCodeThemeKind::from_name(&name.to_ascii_uppercase()),
                Some(kind),
            );
        }
        for alias in ["system", "day", "night", "crabcode-dark", "default", ""] {
            assert_eq!(CrabCodeThemeKind::from_name(alias), None, "{alias}");
        }
        assert_eq!(CrabCodeThemeKind::ALL.len(), 6);
        assert!(!CrabCodeThemeKind::ALL.contains(&CrabCodeThemeKind::Auto));
    }

    #[test]
    fn product_role_audit_is_complete_unique_and_contains_no_derived_mapping() {
        assert_eq!(PRODUCT_ROLE_AUDIT.len(), 66);
        let unique = PRODUCT_ROLE_AUDIT
            .iter()
            .map(|(role, _)| *role)
            .collect::<HashSet<_>>();
        assert_eq!(unique.len(), PRODUCT_ROLE_AUDIT.len());
        let direct = PRODUCT_ROLE_AUDIT
            .iter()
            .filter(|(_, source)| matches!(source, ProductRoleSource::DirectHistorical(_)))
            .count();
        assert_eq!(direct, 13);
    }

    #[test]
    fn fixed_night_mother_matches_every_upstream_color_role() {
        assert_colors!(
            CrabCodeTheme::fixed_night_mother(),
            bg_base => rgb(20, 20, 20),
            bg_light => rgb(36, 36, 36),
            bg_dark => rgb(28, 28, 28),
            bg_highlight => rgb(36, 36, 36),
            bg_hover => rgb(44, 44, 44),
            bg_terminal => rgb(10, 10, 10),
            accent_user => rgb(200, 200, 200),
            accent_assistant => rgb(187, 154, 247),
            accent_thinking => rgb(187, 154, 247),
            accent_tool => rgb(120, 120, 120),
            accent_system => rgb(122, 162, 247),
            accent_error => rgb(247, 118, 142),
            accent_success => rgb(158, 206, 106),
            accent_running => rgb(187, 154, 247),
            accent_skill => rgb(122, 162, 247),
            text_primary => rgb(225, 225, 225),
            text_secondary => rgb(200, 200, 200),
            gray_dim => rgb(88, 88, 88),
            gray => rgb(108, 108, 108),
            gray_bright => rgb(120, 120, 120),
            command => rgb(224, 175, 104),
            path => rgb(255, 158, 100),
            running => rgb(125, 207, 255),
            warning => rgb(224, 175, 104),
            fuzzy_accent => rgb(122, 162, 247),
            accent_plan => rgb(255, 219, 141),
            accent_verify => rgb(187, 154, 247),
            accent_feedback => rgb(115, 218, 202),
            accent_remember => rgb(139, 195, 74),
            selection_border => rgb(60, 60, 65),
            hover_border => rgb(30, 30, 34),
            prompt_border => rgb(50, 50, 55),
            prompt_border_active => rgb(80, 80, 88),
            accent_model => rgb(26, 188, 156),
            scrollbar_bg => rgb(17, 17, 17),
            scrollbar_fg => rgb(36, 36, 36),
            diff_delete_bg => rgb(66, 14, 20),
            diff_delete_fg => rgb(247, 118, 142),
            diff_insert_bg => rgb(6, 56, 6),
            diff_insert_fg => rgb(158, 206, 106),
            diff_equal_fg => rgb(108, 108, 108),
            diff_gutter_fg => rgb(108, 108, 108),
            bg_visual => rgb(54, 54, 54),
            paste_bg => rgb(17, 17, 17),
            paste_fg => rgb(200, 200, 200),
            paste_dim => rgb(65, 65, 65),
            markdown_h1 => rgb(26, 188, 156),
            markdown_h2 => rgb(122, 162, 247),
            markdown_h3 => rgb(157, 124, 216),
            markdown_h4 => rgb(120, 120, 120),
            markdown_h5 => rgb(108, 108, 108),
            markdown_h6 => rgb(90, 90, 90),
            markdown_code => rgb(58, 149, 171),
            markdown_task_checked => rgb(158, 206, 106),
            markdown_task_unchecked => rgb(200, 200, 200),
            markdown_muted => rgb(108, 108, 108),
            markdown_code_bg => rgb(28, 28, 28),
            markdown_text => rgb(200, 200, 200),
            link => rgb(122, 166, 218),
        );
        let theme = CrabCodeTheme::fixed_night_mother();
        assert_eq!(theme.markdown_h1_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h2_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h3_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h4_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h5_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h6_mod, Modifier::empty());
    }

    #[test]
    fn fixed_day_mother_matches_every_upstream_color_role() {
        assert_colors!(
            CrabCodeTheme::fixed_day_mother(),
            bg_base => rgb(238, 238, 238),
            bg_light => rgb(222, 222, 222),
            bg_dark => rgb(228, 228, 228),
            bg_highlight => rgb(222, 222, 222),
            bg_hover => rgb(208, 208, 208),
            bg_terminal => rgb(245, 245, 245),
            accent_user => rgb(68, 68, 68),
            accent_assistant => rgb(125, 75, 198),
            accent_thinking => rgb(125, 75, 198),
            accent_tool => rgb(98, 98, 98),
            accent_system => rgb(47, 100, 210),
            accent_error => rgb(205, 48, 72),
            accent_success => rgb(55, 142, 35),
            accent_running => rgb(125, 75, 198),
            accent_skill => rgb(47, 100, 210),
            text_primary => rgb(38, 38, 38),
            text_secondary => rgb(68, 68, 68),
            gray_dim => rgb(165, 165, 165),
            gray => rgb(118, 118, 118),
            gray_bright => rgb(98, 98, 98),
            command => rgb(162, 118, 18),
            path => rgb(195, 105, 30),
            running => rgb(0, 130, 170),
            warning => rgb(162, 118, 18),
            fuzzy_accent => rgb(47, 100, 210),
            accent_plan => rgb(168, 120, 10),
            accent_verify => rgb(120, 80, 160),
            accent_feedback => rgb(12, 148, 124),
            accent_remember => rgb(76, 175, 80),
            selection_border => rgb(185, 185, 190),
            hover_border => rgb(212, 212, 216),
            prompt_border => rgb(200, 200, 205),
            prompt_border_active => rgb(165, 165, 175),
            accent_model => rgb(10, 142, 112),
            scrollbar_bg => rgb(234, 234, 234),
            scrollbar_fg => rgb(222, 222, 222),
            diff_delete_bg => rgb(245, 218, 222),
            diff_delete_fg => rgb(205, 48, 72),
            diff_insert_bg => rgb(218, 242, 220),
            diff_insert_fg => rgb(55, 142, 35),
            diff_equal_fg => rgb(118, 118, 118),
            diff_gutter_fg => rgb(118, 118, 118),
            bg_visual => rgb(198, 198, 198),
            paste_bg => rgb(222, 222, 222),
            paste_fg => rgb(68, 68, 68),
            paste_dim => rgb(178, 178, 178),
            markdown_h1 => rgb(10, 142, 112),
            markdown_h2 => rgb(47, 100, 210),
            markdown_h3 => rgb(108, 62, 178),
            markdown_h4 => rgb(98, 98, 98),
            markdown_h5 => rgb(118, 118, 118),
            markdown_h6 => rgb(142, 142, 142),
            markdown_code => rgb(15, 135, 162),
            markdown_task_checked => rgb(55, 142, 35),
            markdown_task_unchecked => rgb(68, 68, 68),
            markdown_muted => rgb(118, 118, 118),
            markdown_code_bg => rgb(228, 228, 228),
            markdown_text => rgb(68, 68, 68),
            link => rgb(47, 100, 210),
        );
        let theme = CrabCodeTheme::fixed_day_mother();
        assert_eq!(theme.markdown_h1_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h2_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h3_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h4_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h5_mod, Modifier::BOLD);
        assert_eq!(theme.markdown_h6_mod, Modifier::empty());
    }

    #[test]
    fn six_product_palettes_match_all_direct_historical_roles() {
        assert_colors!(
            CrabCodeTheme::light(),
            text_primary => rgb(0, 0, 0),
            text_secondary => rgb(102, 102, 102),
            gray_dim => rgb(175, 175, 175),
            accent_assistant => rgb(215, 119, 87),
            accent_system => rgb(87, 105, 247),
            accent_error => rgb(171, 43, 63),
            accent_success => rgb(44, 122, 57),
            background_accent => rgb(0, 153, 153),
            warning => rgb(150, 108, 30),
            prompt_border => rgb(153, 153, 153),
            prompt_border_active => rgb(183, 183, 183),
            diff_insert_bg => rgb(105, 219, 124),
            diff_delete_bg => rgb(255, 168, 180),
        );
        assert_colors!(
            CrabCodeTheme::dark(),
            text_primary => rgb(255, 255, 255),
            text_secondary => rgb(153, 153, 153),
            gray_dim => rgb(80, 80, 80),
            accent_assistant => rgb(215, 119, 87),
            accent_system => rgb(147, 165, 255),
            accent_error => rgb(255, 107, 128),
            accent_success => rgb(78, 186, 101),
            background_accent => rgb(0, 204, 204),
            warning => rgb(255, 193, 7),
            prompt_border => rgb(136, 136, 136),
            prompt_border_active => rgb(166, 166, 166),
            diff_insert_bg => rgb(34, 92, 43),
            diff_delete_bg => rgb(122, 41, 54),
        );
        assert_colors!(
            CrabCodeTheme::light_daltonized(),
            text_primary => rgb(0, 0, 0),
            text_secondary => rgb(102, 102, 102),
            gray_dim => rgb(175, 175, 175),
            accent_assistant => rgb(255, 153, 51),
            accent_system => rgb(51, 102, 255),
            accent_error => rgb(204, 0, 0),
            accent_success => rgb(0, 102, 153),
            background_accent => rgb(0, 153, 153),
            warning => rgb(255, 153, 0),
            prompt_border => rgb(153, 153, 153),
            prompt_border_active => rgb(183, 183, 183),
            diff_insert_bg => rgb(153, 204, 255),
            diff_delete_bg => rgb(255, 204, 204),
        );
        assert_colors!(
            CrabCodeTheme::dark_daltonized(),
            text_primary => rgb(255, 255, 255),
            text_secondary => rgb(153, 153, 153),
            gray_dim => rgb(80, 80, 80),
            accent_assistant => rgb(255, 153, 51),
            accent_system => rgb(153, 204, 255),
            accent_error => rgb(255, 102, 102),
            accent_success => rgb(51, 153, 255),
            background_accent => rgb(0, 204, 204),
            warning => rgb(255, 204, 0),
            prompt_border => rgb(136, 136, 136),
            prompt_border_active => rgb(166, 166, 166),
            diff_insert_bg => rgb(0, 68, 102),
            diff_delete_bg => rgb(102, 0, 0),
        );
        assert_colors!(
            CrabCodeTheme::light_ansi(),
            text_primary => Color::Black,
            text_secondary => Color::DarkGray,
            gray_dim => Color::DarkGray,
            accent_assistant => Color::LightRed,
            accent_system => Color::Blue,
            accent_error => Color::Red,
            accent_success => Color::Green,
            background_accent => Color::Cyan,
            warning => Color::Yellow,
            prompt_border => Color::Gray,
            prompt_border_active => Color::White,
            diff_insert_bg => Color::Green,
            diff_delete_bg => Color::Red,
        );
        assert_colors!(
            CrabCodeTheme::dark_ansi(),
            text_primary => Color::White,
            text_secondary => Color::Gray,
            gray_dim => Color::Gray,
            accent_assistant => Color::LightRed,
            accent_system => Color::LightBlue,
            accent_error => Color::LightRed,
            accent_success => Color::LightGreen,
            background_accent => Color::LightCyan,
            warning => Color::LightYellow,
            prompt_border => Color::Gray,
            prompt_border_active => Color::White,
            diff_insert_bg => Color::Green,
            diff_delete_bg => Color::Red,
        );
    }

    #[test]
    fn unproven_product_roles_remain_byte_for_byte_mother_values() {
        let dark = CrabCodeTheme::dark();
        let dark_mother = CrabCodeTheme::fixed_night_mother();
        let light = CrabCodeTheme::light();
        let light_mother = CrabCodeTheme::fixed_day_mother();
        for (adapted, mother) in [(dark, dark_mother), (light, light_mother)] {
            assert_eq!(adapted.bg_base, mother.bg_base);
            assert_eq!(adapted.bg_dark, mother.bg_dark);
            assert_eq!(adapted.bg_highlight, mother.bg_highlight);
            assert_eq!(adapted.bg_terminal, mother.bg_terminal);
            assert_eq!(adapted.accent_user, mother.accent_user);
            assert_eq!(adapted.accent_thinking, mother.accent_thinking);
            assert_eq!(adapted.accent_tool, mother.accent_tool);
            assert_eq!(adapted.markdown_h1, mother.markdown_h1);
            assert_eq!(adapted.markdown_h2, mother.markdown_h2);
            assert_eq!(adapted.markdown_h3, mother.markdown_h3);
            assert_eq!(adapted.markdown_code, mother.markdown_code);
            assert_eq!(adapted.markdown_code_bg, mother.markdown_code_bg);
            assert_eq!(adapted.markdown_text, mother.markdown_text);
        }
    }

    #[test]
    fn quantization_covers_all_color_roles_and_preserves_modifiers() {
        let no_color = CrabCodeTheme::dark().quantized(color_support::ColorLevel::None);
        let colors = [
            no_color.bg_base,
            no_color.bg_light,
            no_color.bg_dark,
            no_color.bg_highlight,
            no_color.bg_hover,
            no_color.bg_terminal,
            no_color.accent_user,
            no_color.accent_assistant,
            no_color.accent_thinking,
            no_color.accent_tool,
            no_color.accent_system,
            no_color.accent_error,
            no_color.accent_success,
            no_color.accent_running,
            no_color.accent_skill,
            no_color.background_accent,
            no_color.text_primary,
            no_color.text_secondary,
            no_color.gray_dim,
            no_color.gray,
            no_color.gray_bright,
            no_color.command,
            no_color.path,
            no_color.running,
            no_color.warning,
            no_color.fuzzy_accent,
            no_color.accent_plan,
            no_color.accent_verify,
            no_color.accent_feedback,
            no_color.accent_remember,
            no_color.selection_border,
            no_color.hover_border,
            no_color.prompt_border,
            no_color.prompt_border_active,
            no_color.accent_model,
            no_color.scrollbar_bg,
            no_color.scrollbar_fg,
            no_color.diff_delete_bg,
            no_color.diff_delete_fg,
            no_color.diff_insert_bg,
            no_color.diff_insert_fg,
            no_color.diff_equal_fg,
            no_color.diff_gutter_fg,
            no_color.bg_visual,
            no_color.paste_bg,
            no_color.paste_fg,
            no_color.paste_dim,
            no_color.markdown_h1,
            no_color.markdown_h2,
            no_color.markdown_h3,
            no_color.markdown_h4,
            no_color.markdown_h5,
            no_color.markdown_h6,
            no_color.markdown_code,
            no_color.markdown_task_checked,
            no_color.markdown_task_unchecked,
            no_color.markdown_muted,
            no_color.markdown_code_bg,
            no_color.markdown_text,
            no_color.link,
        ];
        assert_eq!(colors.len(), 60);
        assert!(colors.into_iter().all(|color| color == Color::Reset));
        assert_eq!(no_color.markdown_h1_mod, Modifier::BOLD);
        assert_eq!(no_color.markdown_h6_mod, Modifier::empty());
    }

    #[test]
    fn terminal_default_uses_only_reset_or_named_ansi_colors() {
        let theme = CrabCodeTheme::terminal_default();
        let colors = [
            theme.bg_base,
            theme.bg_light,
            theme.bg_dark,
            theme.bg_highlight,
            theme.bg_hover,
            theme.bg_terminal,
            theme.accent_user,
            theme.accent_assistant,
            theme.accent_thinking,
            theme.accent_tool,
            theme.accent_system,
            theme.accent_error,
            theme.accent_success,
            theme.accent_running,
            theme.accent_skill,
            theme.background_accent,
            theme.text_primary,
            theme.text_secondary,
            theme.gray_dim,
            theme.gray,
            theme.gray_bright,
            theme.command,
            theme.path,
            theme.running,
            theme.warning,
            theme.fuzzy_accent,
            theme.accent_plan,
            theme.accent_verify,
            theme.accent_feedback,
            theme.accent_remember,
            theme.selection_border,
            theme.hover_border,
            theme.prompt_border,
            theme.prompt_border_active,
            theme.accent_model,
            theme.scrollbar_bg,
            theme.scrollbar_fg,
            theme.diff_delete_bg,
            theme.diff_delete_fg,
            theme.diff_insert_bg,
            theme.diff_insert_fg,
            theme.diff_equal_fg,
            theme.diff_gutter_fg,
            theme.bg_visual,
            theme.paste_bg,
            theme.paste_fg,
            theme.paste_dim,
            theme.markdown_h1,
            theme.markdown_h2,
            theme.markdown_h3,
            theme.markdown_h4,
            theme.markdown_h5,
            theme.markdown_h6,
            theme.markdown_code,
            theme.markdown_task_checked,
            theme.markdown_task_unchecked,
            theme.markdown_muted,
            theme.markdown_code_bg,
            theme.markdown_text,
            theme.link,
        ];
        assert_eq!(colors.len(), 60);
        assert!(
            colors
                .into_iter()
                .all(|color| !matches!(color, Color::Rgb(..) | Color::Indexed(_))),
        );
    }

    #[test]
    fn terminal_default_backgrounds_are_transparent_and_secondary_text_dims() {
        let theme = CrabCodeTheme::terminal_default();
        for color in [
            theme.bg_base,
            theme.bg_light,
            theme.bg_dark,
            theme.bg_terminal,
            theme.markdown_code_bg,
            theme.diff_delete_bg,
            theme.diff_insert_bg,
            theme.paste_bg,
        ] {
            assert_eq!(color, Color::Reset);
        }
        assert_eq!(theme.primary().fg, Some(Color::Reset));
        assert_eq!(theme.muted().fg, None);
        assert!(theme.muted().add_modifier.contains(Modifier::DIM));
        assert_eq!(theme.dim().fg, None);
        assert!(theme.dim().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn terminal_native_lock_serves_default_without_losing_selected_theme() {
        let _guard = cache::test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cache::reset_for_test();
        cache::apply_setting(CrabCodeThemeKind::LightDaltonized);
        cache::set_terminal_native_lock(true);

        let expected = CrabCodeTheme::terminal_default().quantized(effective_level_for_kind(
            CrabCodeThemeKind::Dark,
            color_support::detect(),
        ));
        assert_eq!(CrabCodeTheme::current(), expected);
        assert_eq!(
            CrabCodeTheme::apply_kind(CrabCodeThemeKind::DarkAnsi),
            CrabCodeThemeKind::Dark,
        );

        cache::set_terminal_native_lock(false);
        assert_eq!(cache::current_kind(), CrabCodeThemeKind::LightDaltonized);
        cache::reset_for_test();
    }

    #[test]
    fn ansi_settings_cap_at_basic_but_never_override_no_color() {
        use color_support::ColorLevel;
        assert_eq!(
            effective_level_for_kind(CrabCodeThemeKind::DarkAnsi, ColorLevel::TrueColor),
            ColorLevel::Basic,
        );
        assert_eq!(
            effective_level_for_kind(CrabCodeThemeKind::LightAnsi, ColorLevel::Ansi256),
            ColorLevel::Basic,
        );
        assert_eq!(
            effective_level_for_kind(CrabCodeThemeKind::DarkAnsi, ColorLevel::None),
            ColorLevel::None,
        );
        assert_eq!(
            effective_level_for_kind(CrabCodeThemeKind::Dark, ColorLevel::TrueColor),
            ColorLevel::TrueColor,
        );
    }
}
