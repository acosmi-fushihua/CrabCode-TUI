//! Shared streaming Markdown content with generation-tracked wrap caching.

use std::borrow::Cow;
use std::cell::RefCell;

use crabcode_markdown_renderer::{HyperlinkTarget, StreamingMarkdownRenderer};
use ratatui::text::Line;

use crate::audited_render::wrapping::word_wrap_lines_with_joiners;
use crate::scrollback::types::{BlockLine, BlockOutput};
use crate::syntax::get_syntect;
use crate::theme::{ThemeKind, cache as theme_cache, md_style};

use super::quote_bar::QuoteBarStrip;

pub(crate) const MARKDOWN_BODY_RANGE: u16 = 0;

#[derive(Debug, Clone)]
struct RenderState {
    renderer: StreamingMarkdownRenderer,
    cache_width: usize,
    cache_generation: u64,
    cache_theme: ThemeKind,
    cache_lines: Vec<Line<'static>>,
    cache_joiners: Vec<Option<String>>,
    frozen_pre_wrap_count: usize,
    frozen_wrapped_count: usize,
}

#[derive(Debug, Clone)]
pub struct MarkdownContent {
    state: RefCell<RenderState>,
    current_raw: bool,
    generation: u64,
}

pub struct WrappedLines<'a> {
    pub lines: &'a [Line<'static>],
    pub joiners: &'a [Option<String>],
}

fn expand_tabs(text: &str) -> Cow<'_, str> {
    let tab_width = crate::appearance::tab_width();
    if tab_width == 0 || !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace('\t', &" ".repeat(tab_width as usize)))
}

impl MarkdownContent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self::new_with_table_width(text, None)
    }

    #[must_use]
    pub fn new_with_table_width(text: impl Into<String>, max_table_width: Option<usize>) -> Self {
        Self::new_inner(text, max_table_width, true)
    }

    #[must_use]
    pub fn new_source_faithful(text: impl Into<String>, max_table_width: Option<usize>) -> Self {
        Self::new_inner(text, max_table_width, false)
    }

    fn new_inner(
        text: impl Into<String>,
        max_table_width: Option<usize>,
        collapse_soft_breaks: bool,
    ) -> Self {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.set_max_table_width(max_table_width);
        renderer.set_collapse_soft_breaks(collapse_soft_breaks);
        let text = text.into();
        let expanded = expand_tabs(&text);
        renderer.push(&expanded);
        renderer.finish(Some(get_syntect()));
        Self {
            state: RefCell::new(RenderState {
                renderer,
                cache_width: 0,
                cache_generation: 0,
                cache_theme: theme_cache::current_kind(),
                cache_lines: Vec::new(),
                cache_joiners: Vec::new(),
                frozen_pre_wrap_count: 0,
                frozen_wrapped_count: 0,
            }),
            current_raw: false,
            generation: 1,
        }
    }

    #[must_use]
    pub fn streaming() -> Self {
        Self {
            state: RefCell::new(RenderState {
                renderer: StreamingMarkdownRenderer::new(md_style::style(), true),
                cache_width: 0,
                cache_generation: 0,
                cache_theme: theme_cache::current_kind(),
                cache_lines: Vec::new(),
                cache_joiners: Vec::new(),
                frozen_pre_wrap_count: 0,
                frozen_wrapped_count: 0,
            }),
            current_raw: false,
            generation: 0,
        }
    }

    pub fn push_chunk(&mut self, chunk: &str) {
        let expanded = expand_tabs(chunk);
        self.state
            .get_mut()
            .renderer
            .push_and_render(&expanded, Some(get_syntect()));
        self.generation += 1;
    }

    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        let expanded = expand_tabs(chunk);
        self.state.get_mut().renderer.push(&expanded);
        self.generation += 1;
    }

    pub fn finish(&mut self) {
        let state = self.state.get_mut();
        state.renderer.finish(Some(get_syntect()));
        state.frozen_pre_wrap_count = 0;
        state.frozen_wrapped_count = 0;
        self.generation += 1;
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.state.borrow().renderer.source().to_owned()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.borrow().renderer.source().is_empty()
    }

    #[must_use]
    pub fn rendered_plain_text(&self) -> String {
        let state = self.state.borrow();
        let view = state.renderer.view();
        let mut output = String::new();
        for (index, line) in view.lines.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            for span in &line.spans {
                output.push_str(&span.content);
            }
        }
        output
    }

    #[must_use]
    pub fn line_source_map(&self) -> Vec<usize> {
        self.state.borrow().renderer.view().line_source_map.to_vec()
    }

    #[must_use]
    pub fn pre_wrap_lines(&self) -> Vec<Line<'static>> {
        self.state.borrow().renderer.view().lines.to_vec()
    }

    pub fn with_hyperlinks<T>(&self, operation: impl FnOnce(&[HyperlinkTarget]) -> T) -> T {
        let state = self.state.borrow();
        operation(state.renderer.view().hyperlinks)
    }

    #[must_use]
    pub fn mermaid_block_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let state = self.state.borrow();
        super::mermaid_content::mermaid_block_ranges(&state.renderer.view())
    }

    #[must_use]
    pub fn mermaid_content(&self) -> super::mermaid_content::MermaidContent {
        let state = self.state.borrow();
        super::mermaid_content::MermaidContent::from_view(&state.renderer.view())
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn is_raw(&self) -> bool {
        self.current_raw
    }

    pub fn evict_wrap_cache(&self) {
        let mut state = self.state.borrow_mut();
        if state.cache_lines.is_empty() && state.cache_joiners.is_empty() {
            return;
        }
        state.cache_lines = Vec::new();
        state.cache_joiners = Vec::new();
        state.cache_generation = u64::MAX;
        state.frozen_pre_wrap_count = 0;
        state.frozen_wrapped_count = 0;
    }

    pub fn set_raw_mode(&mut self, raw: bool) {
        if self.current_raw == raw {
            return;
        }
        self.current_raw = raw;
        let state = self.state.get_mut();
        state.renderer.set_pretty(!raw);
        state.renderer.render(Some(get_syntect()));
        state.frozen_pre_wrap_count = 0;
        state.frozen_wrapped_count = 0;
        self.generation += 1;
    }

    fn ensure_wrapped(&self, width: usize) {
        let mut state = self.state.borrow_mut();
        let current_theme = theme_cache::current_kind();

        if state.cache_theme != current_theme {
            state.renderer.set_style(md_style::style());
            state.cache_theme = current_theme;
            state.cache_generation = u64::MAX;
            state.frozen_pre_wrap_count = 0;
            state.frozen_wrapped_count = 0;
        }

        if state.cache_width == width && state.cache_generation == self.generation {
            return;
        }

        if state.cache_width != width {
            state.frozen_pre_wrap_count = 0;
            state.frozen_wrapped_count = 0;
        }

        state.renderer.set_max_table_width(Some(width));
        state.renderer.render(Some(get_syntect()));
        let frozen_count = state.renderer.frozen_lines_count();

        let new_frozen_wrapped = if frozen_count > state.frozen_pre_wrap_count {
            let lines =
                state.renderer.view().lines[state.frozen_pre_wrap_count..frozen_count].to_vec();
            Some(word_wrap_lines_with_joiners(lines, width))
        } else {
            None
        };

        let total_lines = state.renderer.view().lines.len();
        let tail_wrapped = if frozen_count < total_lines {
            let lines = state.renderer.view().lines[frozen_count..].to_vec();
            Some(word_wrap_lines_with_joiners(lines, width))
        } else {
            None
        };

        let frozen_wrapped_count = state.frozen_wrapped_count;
        state.cache_lines.truncate(frozen_wrapped_count);
        state.cache_joiners.truncate(frozen_wrapped_count);

        if let Some((lines, joiners)) = new_frozen_wrapped {
            state.cache_lines.extend(lines);
            state.cache_joiners.extend(joiners);
            state.frozen_pre_wrap_count = frozen_count;
            state.frozen_wrapped_count = state.cache_lines.len();
        }

        if let Some((lines, joiners)) = tail_wrapped {
            state.cache_lines.extend(lines);
            state.cache_joiners.extend(joiners);
        }

        state.cache_width = width;
        state.cache_generation = self.generation;
    }

    pub fn with_wrapped_lines<T>(
        &self,
        width: usize,
        operation: impl FnOnce(WrappedLines<'_>) -> T,
    ) -> T {
        self.ensure_wrapped(width);
        let state = self.state.borrow();
        operation(WrappedLines {
            lines: &state.cache_lines,
            joiners: &state.cache_joiners,
        })
    }

    #[must_use]
    pub fn output(&self, width: usize) -> BlockOutput {
        let strip = QuoteBarStrip::new(!self.current_raw);
        self.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return BlockOutput {
                    lines: vec![Line::from("").into()],
                };
            }

            BlockOutput {
                lines: wrapped
                    .lines
                    .iter()
                    .zip(wrapped.joiners)
                    .map(|(line, joiner)| {
                        let mut content = line.clone();
                        let selectable = strip.selectable(&mut content);
                        let mut block_line = BlockLine::styled(content)
                            .with_selection_range(Some(MARKDOWN_BODY_RANGE))
                            .with_joiner(joiner.clone());
                        block_line.selectable = selectable;
                        if let Some(background) = line.style.bg {
                            block_line.with_background(background)
                        } else {
                            block_line
                        }
                    })
                    .collect(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::{Selectable, derive_selection_text};
    use unicode_width::UnicodeWidthStr;

    fn output_text(output: &BlockOutput) -> Vec<String> {
        output
            .lines
            .iter()
            .map(|line| line.content.to_string())
            .collect()
    }

    #[test]
    fn same_generation_and_width_reuse_wrap_cache() {
        let markdown = MarkdownContent::new("Hello world, this is a test line");
        let first = markdown.output(80);
        let second = markdown.output(80);
        assert_eq!(output_text(&first), output_text(&second));
        let state = markdown.state.borrow();
        assert_eq!(state.cache_generation, 1);
        assert_eq!(state.cache_width, 80);
    }

    #[test]
    fn streaming_incremental_wrap_matches_one_shot_render() {
        let chunks = [
            "First paragraph long enough to wrap around a narrow viewport.\n\n",
            "Second paragraph with **styled content** and a [link](https://example.com).\n\n",
            "- first item\n- second item with additional wrapping words\n",
        ];
        let mut streaming = MarkdownContent::streaming();
        for chunk in chunks {
            streaming.push_chunk(chunk);
            let _ = streaming.output(32);
        }
        let one_shot = MarkdownContent::new(chunks.concat());
        assert_eq!(
            output_text(&streaming.output(32)),
            output_text(&one_shot.output(32)),
        );
    }

    #[test]
    fn source_faithful_mode_preserves_soft_break_line_mapping() {
        let collapsed = MarkdownContent::new("alpha\nbeta\ngamma").pre_wrap_lines();
        let faithful =
            MarkdownContent::new_source_faithful("alpha\nbeta\ngamma", None).pre_wrap_lines();
        assert_eq!(collapsed.len(), 1);
        assert_eq!(faithful.len(), 3);
        assert_eq!(faithful[1].to_string(), "beta");
    }

    #[test]
    fn default_tab_width_is_consumed_from_renderer_local_appearance_state() {
        let markdown = MarkdownContent::new("a\tb");
        assert_eq!(markdown.rendered_plain_text(), "a    b");
    }

    #[test]
    fn table_rows_fill_requested_display_width_with_wide_glyphs() {
        let source = "| Status | Note |\n|---|---|\n| \u{26A0}\u{FE0F} warn | em \u{2014} dash |\n| \u{2705} ok | \u{2717} no |\n";
        let output = MarkdownContent::new(source).output(48);
        assert!(output.lines.len() >= 6);
        for line in &output.lines {
            assert_eq!(line.content.to_string().width(), 48);
        }
    }

    #[test]
    fn body_range_joiners_and_selection_survive_wrap() {
        let output = MarkdownContent::new("hello world this should wrap across lines").output(10);
        assert!(output.lines.len() > 1);
        assert!(
            output
                .lines
                .iter()
                .all(|line| line.selection_range == Some(MARKDOWN_BODY_RANGE)),
        );
        assert!(
            output
                .lines
                .iter()
                .all(|line| !matches!(line.selectable, Selectable::None)),
        );
        assert!(
            output
                .lines
                .iter()
                .skip(1)
                .any(|line| line.joiner.is_some()),
        );
        let mut selected = String::new();
        for (index, line) in output.lines.iter().enumerate() {
            if index > 0 {
                selected.push_str(line.joiner.as_deref().unwrap_or("\n"));
            }
            selected.push_str(&derive_selection_text(line));
        }
        assert_eq!(selected, "hello world this should wrap across lines");
    }

    #[test]
    fn raw_mode_and_cache_eviction_preserve_source_and_generation_contract() {
        let mut markdown = MarkdownContent::new("**bold** text");
        let _ = markdown.output(20);
        let before = markdown.generation();
        markdown.set_raw_mode(true);
        assert!(markdown.is_raw());
        assert_eq!(markdown.generation(), before + 1);
        assert_eq!(markdown.text(), "**bold** text");

        markdown.evict_wrap_cache();
        {
            let state = markdown.state.borrow();
            assert!(state.cache_lines.is_empty());
            assert!(state.cache_joiners.is_empty());
        }
        assert!(!markdown.output(20).lines.is_empty());
    }

    #[test]
    fn mermaid_detection_and_hyperlink_view_remain_available_without_reparse() {
        let markdown = MarkdownContent::new(concat!(
            "[docs](https://example.com)\n\n",
            "```mermaid\nflowchart TD\nA-->B\n```\n",
        ));
        assert_eq!(markdown.mermaid_content().len(), 1);
        assert_eq!(markdown.mermaid_block_ranges().len(), 1);
        markdown.with_hyperlinks(|links| {
            assert!(!links.is_empty());
            assert!(links.iter().all(|link| link.url == "https://example.com"),);
            assert!(links.iter().all(|link| !link.column_range.is_empty()),);
        });
    }
}
