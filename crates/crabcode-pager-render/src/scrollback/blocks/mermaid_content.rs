//! Mermaid diagram detection and renderer affordance rows.
//!
//! This module is presentation-only. It detects closed Mermaid fences in the
//! Markdown render view, derives stable renderer cache keys, and inserts the
//! non-selectable row painted by the interactive affordance lifecycle. Actual
//! PNG rendering remains outside this value layer.

use std::ops::Range;

use crabcode_markdown_renderer::{CodeBlockSpan, MarkdownRenderView};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::appearance::{RenderMermaid, RendererLanguage};
use crate::scrollback::types::{BlockLine, BlockOutput};
use crate::theme::ThemeKind;

pub const MERMAID_INFO: &str = "mermaid";

const MERMAID_LABEL: &str = "\u{25c7} mermaid";
const MERMAID_RENDERING_ZH: &str = "正在渲染图表\u{2026}";
const MERMAID_RENDERING_EN: &str = "rendering diagram\u{2026}";
const AFFORDANCE_OPEN_ZH: &str = "[打开图片]";
const AFFORDANCE_OPEN_EN: &str = "[Open Image]";
const AFFORDANCE_COPY_PATH_ZH: &str = "[复制图片路径]";
const AFFORDANCE_COPY_PATH_EN: &str = "[Copy Image Path]";
const AFFORDANCE_COPY_SOURCE_ZH: &str = "[复制源代码]";
const AFFORDANCE_COPY_SOURCE_EN: &str = "[Copy Source]";
const AFFORDANCE_GAP: u16 = 3;
const MERMAID_WIDTH_BUCKET: u16 = 8;
const OPEN_QUALITY_WIDTH_BUCKET: u16 = u16::MAX;
const RENDER_REVISION: u8 = 3;

fn width_bucket(target_width_cols: u16) -> u16 {
    target_width_cols / MERMAID_WIDTH_BUCKET
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MermaidRenderQuality {
    #[default]
    Terminal,
    Open,
}

pub(crate) fn hash_source(source: &str) -> [u8; 32] {
    *blake3::hash(source.as_bytes()).as_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidBlock {
    pub source: String,
    pub prewrap_line_range: Range<usize>,
}

fn is_mermaid_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case(MERMAID_INFO))
}

fn mermaid_spans<'a>(view: &'a MarkdownRenderView<'a>) -> impl Iterator<Item = &'a CodeBlockSpan> {
    view.code_blocks
        .iter()
        .filter(|span| is_mermaid_info(&span.info))
}

#[must_use]
pub fn mermaid_blocks(view: &MarkdownRenderView<'_>) -> Vec<MermaidBlock> {
    mermaid_spans(view)
        .map(|span| MermaidBlock {
            source: span.body.clone(),
            prewrap_line_range: span.output_line_range.clone(),
        })
        .collect()
}

#[must_use]
pub fn mermaid_block_ranges(view: &MarkdownRenderView<'_>) -> Vec<Range<usize>> {
    mermaid_spans(view)
        .map(|span| span.output_line_range.clone())
        .collect()
}

#[must_use]
pub const fn theme_is_dark(theme: ThemeKind) -> bool {
    match theme {
        ThemeKind::Dark | ThemeKind::DarkDaltonized | ThemeKind::DarkAnsi | ThemeKind::Auto => true,
        ThemeKind::Light | ThemeKind::LightDaltonized | ThemeKind::LightAnsi => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MermaidCacheKey {
    pub source_hash: [u8; 32],
    pub theme: ThemeKind,
    pub width_bucket: u16,
    pub quality: MermaidRenderQuality,
}

impl MermaidCacheKey {
    #[must_use]
    pub fn derive(
        source: &str,
        theme: ThemeKind,
        target_width_cols: u16,
        quality: MermaidRenderQuality,
    ) -> Self {
        let width_bucket = match quality {
            MermaidRenderQuality::Terminal => width_bucket(target_width_cols),
            MermaidRenderQuality::Open => OPEN_QUALITY_WIDTH_BUCKET,
        };
        Self {
            source_hash: hash_source(source),
            theme,
            width_bucket,
            quality,
        }
    }

    #[must_use]
    pub fn cache_filename(&self) -> String {
        use std::fmt::Write as _;

        let mut name = String::with_capacity(88);
        for byte in self.source_hash {
            let _ = write!(name, "{byte:02x}");
        }
        let quality_tag = match self.quality {
            MermaidRenderQuality::Terminal => "t",
            MermaidRenderQuality::Open => "o",
        };
        let _ = write!(
            name,
            "-{}-{}-{}-r{RENDER_REVISION}.png",
            self.theme as u8, self.width_bucket, quality_tag,
        );
        name
    }
}

#[derive(Debug, Clone, Default)]
pub struct MermaidContent {
    blocks: Vec<MermaidBlock>,
}

impl MermaidContent {
    #[must_use]
    pub fn from_view(view: &MarkdownRenderView<'_>) -> Self {
        Self {
            blocks: mermaid_blocks(view),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[must_use]
    pub fn source(&self, index: usize) -> Option<&str> {
        self.blocks.get(index).map(|block| block.source.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDisplay {
    SourceOnly,
    Affordances,
}

#[must_use]
pub const fn mermaid_display(setting: RenderMermaid) -> MermaidDisplay {
    match setting {
        RenderMermaid::Off => MermaidDisplay::SourceOnly,
        RenderMermaid::Auto | RenderMermaid::On => MermaidDisplay::Affordances,
    }
}

#[must_use]
pub const fn mermaid_display_static(setting: RenderMermaid, static_commit: bool) -> MermaidDisplay {
    if static_commit {
        MermaidDisplay::SourceOnly
    } else {
        mermaid_display(setting)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffordanceKind {
    Open,
    CopyPath,
    CopySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffordanceButton {
    pub label: &'static str,
    pub kind: AffordanceKind,
    pub col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffordanceRow {
    pub label: (u16, &'static str),
    pub buttons: [AffordanceButton; 3],
    pub status: Option<(u16, &'static str)>,
}

fn affordance_buttons(start_col: u16, language: RendererLanguage) -> [AffordanceButton; 3] {
    let specs = [
        (
            language.text(AFFORDANCE_OPEN_ZH, AFFORDANCE_OPEN_EN),
            AffordanceKind::Open,
        ),
        (
            language.text(AFFORDANCE_COPY_PATH_ZH, AFFORDANCE_COPY_PATH_EN),
            AffordanceKind::CopyPath,
        ),
        (
            language.text(AFFORDANCE_COPY_SOURCE_ZH, AFFORDANCE_COPY_SOURCE_EN),
            AffordanceKind::CopySource,
        ),
    ];
    let mut column = start_col;
    specs.map(|(label, kind)| {
        let button = AffordanceButton {
            label,
            kind,
            col: column,
        };
        column += UnicodeWidthStr::width(label) as u16 + AFFORDANCE_GAP;
        button
    })
}

pub fn affordance_row(rendering: bool) -> AffordanceRow {
    affordance_row_for_language(rendering, RendererLanguage::default())
}

pub fn affordance_row_for_language(rendering: bool, language: RendererLanguage) -> AffordanceRow {
    let buttons_start = UnicodeWidthStr::width(MERMAID_LABEL) as u16 + AFFORDANCE_GAP;
    let buttons = affordance_buttons(buttons_start, language);
    let status = rendering.then(|| {
        let last = &buttons[buttons.len() - 1];
        let after = last.col + UnicodeWidthStr::width(last.label) as u16 + AFFORDANCE_GAP;
        (
            after,
            language.text(MERMAID_RENDERING_ZH, MERMAID_RENDERING_EN),
        )
    });
    AffordanceRow {
        label: (0, MERMAID_LABEL),
        buttons,
        status,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramAffordance {
    pub row_offset: u16,
    pub source: String,
}

fn prewrap_end_rows(lines: &[BlockLine]) -> Vec<usize> {
    let mut ends = Vec::new();
    for (row, prewrap) in crate::scrollback::types::prewrap_index_per_row(lines)
        .into_iter()
        .enumerate()
    {
        if prewrap < ends.len() {
            ends[prewrap] = row + 1;
        } else {
            ends.push(row + 1);
        }
    }
    ends
}

fn diagram_insert_rows(lines: &[BlockLine], ranges: &[Range<usize>]) -> Vec<(usize, usize)> {
    let ends = prewrap_end_rows(lines);
    ranges
        .iter()
        .enumerate()
        .filter(|(_, range)| !range.is_empty())
        .filter_map(|(index, range)| ends.get(range.end - 1).map(|&insert_at| (insert_at, index)))
        .collect()
}

fn continuation_row(line: Line<'static>) -> BlockLine {
    BlockLine::separator(line).with_joiner(Some(String::new()))
}

pub fn apply_affordance_rows(
    output: &mut BlockOutput,
    prewrap_ranges: &[Range<usize>],
    mut source_for: impl FnMut(usize) -> String,
) -> Vec<DiagramAffordance> {
    let inserts = diagram_insert_rows(&output.lines, prewrap_ranges);
    let affordances = inserts
        .iter()
        .enumerate()
        .map(|(offset, &(insert_at, index))| DiagramAffordance {
            row_offset: (insert_at + offset) as u16,
            source: source_for(index),
        })
        .collect();
    for &(insert_at, _) in inserts.iter().rev() {
        output
            .lines
            .insert(insert_at, continuation_row(Line::from(String::new())));
    }
    affordances
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::Selectable;
    use crate::syntax::get_syntect;
    use crate::theme::md_style;
    use crabcode_markdown_renderer::StreamingMarkdownRenderer;

    fn detect(source: &str, pretty: bool) -> Vec<MermaidBlock> {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), pretty);
        renderer.push(source);
        let view = renderer.finish(Some(get_syntect()));
        mermaid_blocks(&view)
    }

    #[test]
    fn detects_only_mermaid_fences_and_preserves_clean_source_order() {
        let source = concat!(
            "```rust\nfn main() {}\n```\n\n",
            "```Mermaid theme=base\nflowchart TD\n A --> B\n```\n\n",
            "```mermaid\nsequenceDiagram\n A->>B: hi\n```\n",
        );
        for pretty in [true, false] {
            let blocks = detect(source, pretty);
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].source, "flowchart TD\n A --> B\n");
            assert_eq!(blocks[1].source, "sequenceDiagram\n A->>B: hi\n");
            assert!(
                blocks
                    .windows(2)
                    .all(|pair| pair[0].prewrap_line_range.start
                        < pair[1].prewrap_line_range.start),
            );
        }
    }

    #[test]
    fn theme_and_quality_are_part_of_stable_cache_identity() {
        let terminal = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::Dark,
            81,
            MermaidRenderQuality::Terminal,
        );
        let nearby = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::Dark,
            87,
            MermaidRenderQuality::Terminal,
        );
        let light = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::Light,
            81,
            MermaidRenderQuality::Terminal,
        );
        let open = MermaidCacheKey::derive(
            "flowchart TD\nA-->B",
            ThemeKind::Dark,
            81,
            MermaidRenderQuality::Open,
        );
        assert_eq!(terminal, nearby);
        assert_ne!(terminal, light);
        assert_ne!(terminal, open);
        assert!(terminal.cache_filename().ends_with("-0-10-t-r3.png"));
        assert!(open.cache_filename().ends_with("-0-65535-o-r3.png"));
    }

    #[test]
    fn theme_denominator_classifies_all_concrete_product_kinds() {
        for theme in [
            ThemeKind::Dark,
            ThemeKind::DarkDaltonized,
            ThemeKind::DarkAnsi,
            ThemeKind::Auto,
        ] {
            assert!(theme_is_dark(theme));
        }
        for theme in [
            ThemeKind::Light,
            ThemeKind::LightDaltonized,
            ThemeKind::LightAnsi,
        ] {
            assert!(!theme_is_dark(theme));
        }
    }

    #[test]
    fn static_commit_suppresses_inert_affordance_rows() {
        for setting in [RenderMermaid::Auto, RenderMermaid::On] {
            assert_eq!(
                mermaid_display_static(setting, false),
                MermaidDisplay::Affordances,
            );
            assert_eq!(
                mermaid_display_static(setting, true),
                MermaidDisplay::SourceOnly,
            );
        }
        assert_eq!(
            mermaid_display_static(RenderMermaid::Off, false),
            MermaidDisplay::SourceOnly,
        );
    }

    #[test]
    fn affordance_rows_follow_wrapped_prewrap_ends_and_stay_non_selectable() {
        let mut output = BlockOutput {
            lines: vec![
                BlockLine::styled(Line::raw("diagram row 1")),
                BlockLine::styled(Line::raw("wrapped continuation"))
                    .with_joiner(Some(String::new())),
                BlockLine::styled(Line::raw("diagram row 2")),
                BlockLine::styled(Line::raw("following text")),
            ],
        };
        let diagram_range = 0..2;
        let affordances =
            apply_affordance_rows(&mut output, std::slice::from_ref(&diagram_range), |_| {
                "flowchart TD\nA-->B\n".to_owned()
            });
        assert_eq!(
            affordances,
            vec![DiagramAffordance {
                row_offset: 3,
                source: "flowchart TD\nA-->B\n".to_owned(),
            }],
        );
        assert_eq!(output.lines.len(), 5);
        assert!(matches!(output.lines[3].selectable, Selectable::None));
        assert_eq!(output.lines[3].joiner.as_deref(), Some(""));
    }

    #[test]
    fn affordance_paint_and_hit_test_layout_share_button_columns() {
        let row = affordance_row(true);
        assert_eq!(row.label, (0, MERMAID_LABEL));
        assert_eq!(
            row.buttons.map(|button| (button.label, button.kind)),
            [
                (AFFORDANCE_OPEN_ZH, AffordanceKind::Open),
                (AFFORDANCE_COPY_PATH_ZH, AffordanceKind::CopyPath),
                (AFFORDANCE_COPY_SOURCE_ZH, AffordanceKind::CopySource),
            ],
        );
        assert!(row.buttons.windows(2).all(|pair| pair[1].col
            == pair[0].col + UnicodeWidthStr::width(pair[0].label) as u16 + AFFORDANCE_GAP),);
        assert_eq!(row.status.map(|(_, text)| text), Some(MERMAID_RENDERING_ZH));
        assert!(affordance_row(false).status.is_none());

        let english = affordance_row_for_language(true, RendererLanguage::EnUs);
        assert_eq!(
            english.buttons.map(|button| (button.label, button.kind)),
            [
                (AFFORDANCE_OPEN_EN, AffordanceKind::Open),
                (AFFORDANCE_COPY_PATH_EN, AffordanceKind::CopyPath),
                (AFFORDANCE_COPY_SOURCE_EN, AffordanceKind::CopySource),
            ],
        );
        assert_eq!(
            english.status.map(|(_, text)| text),
            Some(MERMAID_RENDERING_EN)
        );
        assert!(english.buttons.windows(2).all(|pair| pair[1].col
            == pair[0].col + UnicodeWidthStr::width(pair[0].label) as u16 + AFFORDANCE_GAP),);
    }
}
