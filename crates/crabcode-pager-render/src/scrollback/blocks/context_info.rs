//! Fixed-lifecycle context block over CrabCode's unique direct-mode payload.
//!
//! Fixed-source lifecycle: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/scrollback/blocks/context_info.rs`.
//!
//! The fixed producer fetches a dedicated session-info snapshot and pushes it
//! into scrollback. CrabCode's unchanged direct backend has no equivalent
//! event: `/context` receives the existing `get_context_usage` control
//! response and opens the existing modal. Consequently the production mapping
//! for this scrollback producer is deliberately **RED / unreachable**. The
//! literal block variant is retained as part of the fixed renderer denominator
//! and consumes the very same [`ContextVisualization`] type as that modal; it
//! does not add a request, response field, DTO, or second command owner.

use ratatui::text::{Line, Span};

use crate::audited_theme::{CrabCodeTheme, color_support};
use crate::context_visualization::ContextVisualization;
use crate::render::wrapping::word_wrap_lines;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockLine, BlockOutput, Selectable};

/// Literal fixed-denominator context block.
#[derive(Debug, Clone)]
pub struct ContextInfoBlock {
    /// The unique parsed projection also used by the existing `/context` modal.
    pub visualization: ContextVisualization,
}

impl ContextInfoBlock {
    pub fn new(visualization: ContextVisualization) -> Self {
        Self { visualization }
    }

    pub fn model(&self) -> &str {
        self.visualization.model()
    }
}

impl BlockContent for ContextInfoBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = CrabCodeTheme::current();
        let styled_lines = self.visualization.styled_lines(
            theme,
            CrabCodeTheme::current_kind(),
            color_support::detect(),
            ctx.appearance.language,
        );
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
                    last.content
                        .spans
                        .push(Span::styled(" …".to_string(), theme.muted()));
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
    use crate::audited_appearance::{AppearanceConfig, RendererLanguage};
    use crate::context_visualization::minimal_test_control_response;
    use crate::scrollback::types::DisplayMode;

    fn context(width: u16, max_lines: Option<u16>) -> BlockContext {
        let appearance = AppearanceConfig {
            language: RendererLanguage::EnUs,
            ..AppearanceConfig::default()
        };
        BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width,
            raw: false,
            max_lines,
            appearance,
            is_selected: false,
            cwd: None,
        }
    }

    fn block() -> ContextInfoBlock {
        let visualization =
            ContextVisualization::from_control_response(&minimal_test_control_response())
                .expect("fixed direct-mode fixture");
        ContextInfoBlock::new(visualization)
    }

    #[test]
    fn renders_the_unique_direct_mode_projection() {
        let block = block();
        let output = block.output(&context(100, None));
        let text = output
            .lines
            .iter()
            .flat_map(|line| line.content.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(block.model(), "model-from-sdk");
        assert!(text.contains("model-from-sdk · 20k/100k tokens (20%)"));
        assert!(text.contains("⛁"));
        assert!(text.contains("⛶"));
    }

    #[test]
    fn preserves_fixed_truncation_contract() {
        let output = block().output(&context(30, Some(2)));
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
    fn preserves_fixed_navigation_contract() {
        let block = block();
        let ctx = context(80, None);
        assert!(block.accent(&ctx).is_none());
        assert!(!block.has_vpad(&ctx));
        assert!(!block.has_raw_mode());
        assert!(!block.is_foldable());
        assert!(!block.is_selectable());
        assert!(block.is_groupable());
    }
}
