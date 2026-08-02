//! Historical direct-TUI local-command output, hosted by the native pager lifecycle.
//!
//! Presentation source of truth:
//! `/private/tmp/crabcode-direct-pin-20260728/src/components/messages/UserLocalCommandOutputMessage.tsx`.
//! The block owns presentation only: it parses the two already-existing display
//! tags and never changes the backend payload or protocol.

use std::collections::HashMap;

use crabcode_markdown_renderer::HyperlinkTarget;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;

use crate::audited_render::wrapping::word_wrap_lines_with_joiners;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockContext, BlockLine, BlockOutput, shift_selection_metadata_for_prefix,
};
use crate::theme::Theme;

use super::markdown_content::MarkdownContent;

const STDOUT_TAG: &str = "local-command-stdout";
const STDERR_TAG: &str = "local-command-stderr";
const PREFIX: &str = "  \u{23bf}  ";
const PREFIX_WIDTH: usize = 5;
const NO_CONTENT_MESSAGE: &str = "(no content)";
const DIAMOND_OPEN_PREFIX: &str = "\u{25c7} ";
const DIAMOND_FILLED_PREFIX: &str = "\u{25c6} ";
const CLOUD_HEADER_SEPARATOR: &str = " \u{00b7} ";

#[derive(Debug, Clone)]
enum LocalCommandSection {
    Markdown {
        source: String,
        content: Box<MarkdownContent>,
    },
    CloudLaunch {
        source: String,
        diamond: char,
        label: String,
        suffix: String,
        rest: String,
    },
}

impl LocalCommandSection {
    fn from_extracted(content: &str) -> Option<Self> {
        let source = content.trim().to_string();
        if source.is_empty() {
            return None;
        }

        let cloud_tail = source
            .strip_prefix(DIAMOND_OPEN_PREFIX)
            .or_else(|| source.strip_prefix(DIAMOND_FILLED_PREFIX));
        if let Some(tail) = cloud_tail {
            let diamond = source.chars().next().expect("checked diamond prefix");
            let (header, rest) = tail
                .split_once('\n')
                .map_or((tail, ""), |(header, rest)| (header, rest.trim()));
            let (label, suffix) = header
                .find(CLOUD_HEADER_SEPARATOR)
                .map_or((header, ""), |separator| {
                    (&header[..separator], &header[separator..])
                });
            let label = label.to_string();
            let suffix = suffix.to_string();
            let rest = rest.to_string();
            return Some(Self::CloudLaunch {
                source,
                diamond,
                label,
                suffix,
                rest,
            });
        }

        Some(Self::Markdown {
            content: Box::new(MarkdownContent::new_source_faithful(source.clone(), None)),
            source,
        })
    }

    fn source(&self) -> &str {
        match self {
            Self::Markdown { source, .. } | Self::CloudLaunch { source, .. } => source,
        }
    }

    fn pre_wrap_line_count(&self) -> usize {
        match self {
            Self::Markdown { content, .. } => content.pre_wrap_lines().len(),
            Self::CloudLaunch { rest, .. } => 1 + usize::from(!rest.is_empty()),
        }
    }

    fn with_hyperlinks(&self, operation: impl FnOnce(&[HyperlinkTarget])) {
        match self {
            Self::Markdown { content, .. } => content.with_hyperlinks(operation),
            Self::CloudLaunch { .. } => operation(&[]),
        }
    }

    fn evict_render_cache(&self) {
        if let Self::Markdown { content, .. } = self {
            content.evict_wrap_cache();
        }
    }
}

/// Native block for `<local-command-stdout>` / `<local-command-stderr>` rows.
///
/// `stdout` is deliberately stored before `stderr` and rendering never follows
/// XML occurrence order. This is the historical direct-TUI contract.
#[derive(Debug, Clone)]
pub struct LocalCommandOutputBlock {
    stdout: Option<LocalCommandSection>,
    stderr: Option<LocalCommandSection>,
    had_extracted_tag: bool,
    hyperlinks: Vec<HyperlinkTarget>,
}

impl LocalCommandOutputBlock {
    #[must_use]
    pub fn new(content: impl Into<String>) -> Self {
        let content = content.into();
        let stdout_extracted = extract_tag(&content, STDOUT_TAG);
        let stderr_extracted = extract_tag(&content, STDERR_TAG);
        let had_extracted_tag = stdout_extracted.is_some() || stderr_extracted.is_some();
        let stdout = stdout_extracted
            .as_deref()
            .and_then(LocalCommandSection::from_extracted);
        let stderr = stderr_extracted
            .as_deref()
            .and_then(LocalCommandSection::from_extracted);
        let hyperlinks = combined_hyperlinks([stdout.as_ref(), stderr.as_ref()]);

        Self {
            stdout,
            stderr,
            had_extracted_tag,
            hyperlinks,
        }
    }

    /// Pure stdout/stderr bodies in historical render order, without XML.
    #[must_use]
    pub fn copy_text(&self) -> Option<String> {
        joined_source([self.stdout.as_ref(), self.stderr.as_ref()])
    }

    /// Search indexes the same pure bodies that whole-block copy returns.
    #[must_use]
    pub fn searchable_text(&self) -> Option<String> {
        self.copy_text()
    }

    pub fn with_hyperlinks<R>(&self, operation: impl FnOnce(&[HyperlinkTarget]) -> R) -> R {
        operation(&self.hyperlinks)
    }

    pub fn evict_render_caches(&self) {
        for section in [self.stdout.as_ref(), self.stderr.as_ref()]
            .into_iter()
            .flatten()
        {
            section.evict_render_cache();
        }
    }
}

impl BlockContent for LocalCommandOutputBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let mut lines = Vec::new();
        for (range_id, section) in [self.stdout.as_ref(), self.stderr.as_ref()]
            .into_iter()
            .flatten()
            .enumerate()
        {
            render_section(section, range_id as u16, ctx, &mut lines);
        }

        if lines.is_empty() && !self.had_extracted_tag {
            render_prefixed_plain(
                NO_CONTENT_MESSAGE,
                Theme::current().muted(),
                0,
                ctx.width,
                &mut lines,
            );
        }

        BlockOutput { lines }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn is_groupable(&self) -> bool {
        true
    }
}

fn extract_tag(content: &str, tag: &str) -> Option<String> {
    let escaped = regex::escape(tag);
    let pattern = format!(r"(?is)<{escaped}(?:\s+[^>]*)?>(.*?)</{escaped}>");
    let regex = Regex::new(&pattern).expect("fixed local-command tag regex");
    let captured = regex.captures(content)?.get(1)?.as_str();
    (!captured.is_empty()).then(|| captured.to_string())
}

fn joined_source<'a>(
    sections: impl IntoIterator<Item = Option<&'a LocalCommandSection>>,
) -> Option<String> {
    let joined = sections
        .into_iter()
        .flatten()
        .map(LocalCommandSection::source)
        .filter(|source| !source.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn combined_hyperlinks<'a>(
    sections: impl IntoIterator<Item = Option<&'a LocalCommandSection>>,
) -> Vec<HyperlinkTarget> {
    let mut combined = Vec::new();
    let mut pre_wrap_offset = 0usize;
    let mut next_link_id = 0u32;

    for section in sections.into_iter().flatten() {
        section.with_hyperlinks(|links| {
            let mut ids = HashMap::new();
            for link in links {
                let id = *ids.entry(link.id).or_insert_with(|| {
                    let id = next_link_id;
                    next_link_id = next_link_id.saturating_add(1);
                    id
                });
                combined.push(HyperlinkTarget {
                    line_index: pre_wrap_offset + link.line_index,
                    column_range: link.column_range.clone(),
                    url: link.url.clone(),
                    id,
                });
            }
        });
        pre_wrap_offset += section.pre_wrap_line_count();
    }

    combined
}

fn render_section(
    section: &LocalCommandSection,
    range_id: u16,
    ctx: &BlockContext,
    lines: &mut Vec<BlockLine>,
) {
    match section {
        LocalCommandSection::Markdown { content, .. } => {
            let body_width = usize::from(ctx.width).saturating_sub(PREFIX_WIDTH).max(1);
            let mut output = content.output(body_width);
            for (line_index, line) in output.lines.iter_mut().enumerate() {
                prepend_prefix(line, line_index == 0);
                line.hyperlink_prefix_width = PREFIX_WIDTH as u16;
                line.selection_range = Some(range_id);
            }
            lines.extend(output.lines);
        }
        LocalCommandSection::CloudLaunch {
            diamond,
            label,
            suffix,
            rest,
            ..
        } => {
            let theme = Theme::current();
            let header = Line::from(vec![
                Span::styled(
                    format!("{diamond} "),
                    Style::default().fg(theme.background_accent),
                ),
                Span::styled(label.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(suffix.clone(), theme.muted()),
            ]);
            let (wrapped, joiners) =
                word_wrap_lines_with_joiners([header], usize::from(ctx.width).max(1));
            lines.extend(wrapped.into_iter().zip(joiners).map(|(line, joiner)| {
                BlockLine::styled(line)
                    .with_selection_range(Some(range_id))
                    .with_joiner(joiner)
            }));
            if !rest.is_empty() {
                render_prefixed_plain(rest, theme.muted(), range_id, ctx.width, lines);
            }
        }
    }
}

fn render_prefixed_plain(
    text: &str,
    style: Style,
    range_id: u16,
    width: u16,
    lines: &mut Vec<BlockLine>,
) {
    let body_width = usize::from(width).saturating_sub(PREFIX_WIDTH).max(1);
    let source_lines = text
        .split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), style)));
    let (wrapped, joiners) = word_wrap_lines_with_joiners(source_lines, body_width);
    for (line_index, (line, joiner)) in wrapped.into_iter().zip(joiners).enumerate() {
        let mut line = BlockLine::styled(line)
            .with_selection_range(Some(range_id))
            .with_joiner(joiner);
        prepend_prefix(&mut line, line_index == 0);
        lines.push(line);
    }
}

fn prepend_prefix(line: &mut BlockLine, show_glyph: bool) {
    let prefix = if show_glyph {
        PREFIX.to_string()
    } else {
        " ".repeat(PREFIX_WIDTH)
    };
    line.content
        .spans
        .insert(0, Span::styled(prefix, Theme::current().muted()));
    shift_selection_metadata_for_prefix(line, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;
    use crate::render::osc8::LinkOverlay;
    use crate::scrollback::RenderBlock;
    use crate::scrollback::render::map_hyperlinks_to_overlay;
    use crate::scrollback::types::{DisplayMode, derive_selection_text};
    use pretty_assertions::assert_eq;

    fn context(width: u16) -> BlockContext {
        BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn text_lines(output: &BlockOutput) -> Vec<String> {
        output
            .lines
            .iter()
            .map(|line| line.content.to_string())
            .collect()
    }

    #[test]
    fn ordinary_output_uses_markdown_prefix_multiline_and_link_lifecycle() {
        let block = LocalCommandOutputBlock::new(concat!(
            "<local-command-stdout source=\"slash\">\n",
            "**完成**\n[文档](https://example.com/docs)\n",
            "</local-command-stdout>",
        ));
        let output = block.output(&context(40));
        let visible = text_lines(&output).join("\n");
        assert!(visible.starts_with(PREFIX));
        assert!(visible.contains("完成"));
        assert!(visible.contains("文档"));
        assert!(output.lines.len() >= 2, "physical newlines must survive");
        assert!(
            output.lines[0].content.spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(
            derive_selection_text(&output.lines[0]).trim(),
            "完成",
            "the prefix is decoration, not copy text",
        );
        assert!(
            output
                .lines
                .iter()
                .all(|line| line.hyperlink_prefix_width == PREFIX_WIDTH as u16),
            "every markdown wrap row records the repeated visual prefix",
        );

        block.with_hyperlinks(|links| {
            assert!(!links.is_empty());
            assert!(
                links
                    .iter()
                    .all(|link| link.url == "https://example.com/docs"),
            );
            assert!(
                links
                    .iter()
                    .any(|link| link.line_index == 1 && link.column_range.start == 0),
                "explicit hyperlinks stay in markdown source coordinates",
            );
        });
    }

    #[test]
    fn stdout_then_stderr_are_independent_even_when_xml_order_is_reversed() {
        let block = LocalCommandOutputBlock::new(concat!(
            "<local-command-stderr>second</local-command-stderr>",
            "<local-command-stdout>first</local-command-stdout>",
        ));
        let output = block.output(&context(40));
        assert_eq!(text_lines(&output), vec!["  ⎿  first", "  ⎿  second"]);
        assert_eq!(output.lines[0].selection_range, Some(0));
        assert_eq!(output.lines[1].selection_range, Some(1));
        assert_eq!(block.copy_text().as_deref(), Some("first\nsecond"));
        assert_eq!(block.searchable_text().as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn cloud_launch_preserves_diamond_label_suffix_header_and_dim_rest() {
        let block = LocalCommandOutputBlock::new(concat!(
            "<local-command-stdout>",
            "◇ 云端任务 · 正在启动\n第一行\n第二行",
            "</local-command-stdout>",
        ));
        let output = block.output(&context(40));
        assert_eq!(output.lines[0].content.spans[0].content, "◇ ");
        assert_eq!(
            output.lines[0].content.spans[0].style.fg,
            Some(Theme::current().background_accent),
        );
        assert_eq!(output.lines[0].content.spans[1].content, "云端任务");
        assert!(
            output.lines[0].content.spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
        );
        assert_eq!(output.lines[0].content.spans[2].content, " · 正在启动");
        assert!(
            output.lines[0].content.spans[2]
                .style
                .add_modifier
                .contains(Modifier::DIM),
        );
        assert_eq!(text_lines(&output)[1..], ["  ⎿  第一行", "     第二行"]);
        assert_eq!(
            block.copy_text().as_deref(),
            Some("◇ 云端任务 · 正在启动\n第一行\n第二行"),
        );
    }

    #[test]
    fn no_tag_uses_fixed_historical_no_content_row() {
        let block = LocalCommandOutputBlock::new("not a local command tag");
        assert_eq!(
            text_lines(&block.output(&context(40))),
            vec!["  ⎿  (no content)"],
        );
        assert_eq!(block.copy_text(), None);
        assert_eq!(block.searchable_text(), None);
    }

    #[test]
    fn narrow_width_wraps_without_losing_cjk_or_exceeding_the_budget() {
        let block = LocalCommandOutputBlock::new(
            "<local-command-stdout>中文内容abcdefghij</local-command-stdout>",
        );
        let output = block.output(&context(10));
        assert!(output.lines.len() > 1);
        assert!(output.lines.iter().all(|line| line.content.width() <= 10),);
        let copied =
            output
                .lines
                .iter()
                .enumerate()
                .fold(String::new(), |mut text, (index, line)| {
                    if index > 0 {
                        text.push_str(line.joiner.as_deref().unwrap_or("\n"));
                    }
                    text.push_str(&derive_selection_text(line));
                    text
                });
        assert_eq!(copied, "中文内容abcdefghij");
    }

    #[test]
    fn attributes_multiline_search_and_copy_never_expose_xml() {
        let block = LocalCommandOutputBlock::new(concat!(
            "<local-command-stdout kind=\"one\">\nalpha\nbeta\n</local-command-stdout>",
            "<local-command-stderr code=\"2\">\ngamma\n</local-command-stderr>",
        ));
        let expected = "alpha\nbeta\ngamma";
        assert_eq!(block.copy_text().as_deref(), Some(expected));
        assert_eq!(block.searchable_text().as_deref(), Some(expected));
        assert!(!block.copy_text().unwrap().contains("local-command-"));
    }

    #[test]
    fn render_block_delegates_copy_search_cache_and_hyperlinks() {
        let block = RenderBlock::local_command_output(
            "<local-command-stdout>[文档](https://example.com)</local-command-stdout>",
        );
        assert!(block.supports_copy());
        assert_eq!(
            block.copy_text(false).as_deref(),
            Some("[文档](https://example.com)"),
        );
        assert_eq!(
            block.searchable_text().as_deref(),
            Some("[文档](https://example.com)"),
        );
        block.with_hyperlinks(|links| {
            assert!(!links.is_empty());
            assert!(links.iter().all(|link| link.url == "https://example.com"),);
        });
        let before = block.output(&context(12));
        block.evict_render_caches();
        let after = block.output(&context(12));
        assert_eq!(text_lines(&before), text_lines(&after));
        assert_eq!(block.accent_color(), None);
        assert!(!block.supports_fullscreen());

        let no_content = RenderBlock::local_command_output("not tagged");
        assert!(!no_content.supports_copy());
    }

    #[test]
    fn prefixed_markdown_link_maps_to_the_visible_overlay_columns() {
        let block = RenderBlock::local_command_output(
            "<local-command-stdout>[文档](https://example.com)</local-command-stdout>",
        );
        let output = block.output(&context(20));
        let mut overlay = LinkOverlay::new();
        block.with_hyperlinks(|links| {
            map_hyperlinks_to_overlay(links, &output, 0, 3, 20, 2, 0, &[], &mut overlay);
        });
        assert!(!overlay.is_empty());
        assert!(
            overlay
                .links()
                .iter()
                .all(|link| link.screen_row >= 3 && link.col_start >= 2 + PREFIX_WIDTH as u16),
            "the dim prefix must shift, not cover, the clickable markdown label: {:?}",
            (text_lines(&output), overlay.links()),
        );
    }
}
