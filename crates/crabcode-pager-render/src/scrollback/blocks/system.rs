//! Compact system-message block.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/scrollback/blocks/system.rs`, SHA-256
//! `858c000d2acabfd65b78f2db173ab29a52c27a2b9c24bc1e6c5e091fe1e1f56e`.
//! Imports bind to CrabCode's renderer-owned wrapping and theme modules;
//! behavior is unchanged.

use ratatui::text::{Line, Span};

use crate::render::wrapping::word_wrap_lines;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockLine, BlockOutput, Selectable};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct SystemMessageBlock {
    pub text: String,
}

impl SystemMessageBlock {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl BlockContent for SystemMessageBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let style = Theme::current().muted();
        let styled_lines: Vec<Line<'static>> = self
            .text
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect();
        let wrapped = word_wrap_lines(styled_lines, usize::from(ctx.width));
        let all_lines: Vec<BlockLine> = wrapped
            .into_iter()
            .map(|line| BlockLine::styled(line).with_selection_range(Some(0)))
            .collect();

        let lines = if let Some(max_lines) = ctx.max_lines {
            let max_lines = usize::from(max_lines);
            if all_lines.len() > max_lines && max_lines > 0 {
                let take_count = if max_lines > 1 { max_lines - 1 } else { 1 };
                let mut truncated: Vec<BlockLine> =
                    all_lines.into_iter().take(take_count).collect();
                if let Some(last) = truncated.last_mut() {
                    let content_end = last.content.spans.len();
                    last.content.spans.push(Span::styled(" …", style));
                    last.selectable = Selectable::Spans(0..content_end);
                }
                truncated
            } else {
                all_lines
            }
        } else {
            all_lines
        };

        if lines.is_empty() {
            BlockOutput {
                lines: vec![BlockLine::styled(Line::from("")).with_selection_range(Some(0))],
            }
        } else {
            BlockOutput { lines }
        }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn is_selectable(&self) -> bool {
        false
    }

    fn is_groupable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;
    use crate::scrollback::types::DisplayMode;

    fn context(width: u16, max_lines: Option<u16>) -> BlockContext {
        BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width,
            raw: false,
            max_lines,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    #[test]
    fn wraps_and_truncates_with_non_selectable_ellipsis() {
        let block = SystemMessageBlock::new("alpha beta gamma delta");
        let output = block.output(&context(6, Some(2)));
        assert_eq!(output.lines.len(), 1);
        assert!(matches!(output.lines[0].selectable, Selectable::Spans(_)));
        let text = output.lines[0]
            .content
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.ends_with(" …"));
    }

    #[test]
    fn empty_message_retains_one_layout_row() {
        let block = SystemMessageBlock::new("");
        assert_eq!(block.output(&context(80, None)).lines.len(), 1);
    }

    #[test]
    fn fixed_navigation_contract_is_preserved() {
        let block = SystemMessageBlock::new("notice");
        assert!(!block.is_selectable());
        assert!(!block.is_foldable());
        assert!(block.is_groupable());
        assert!(!block.has_vpad(&context(80, None)));
    }
}
