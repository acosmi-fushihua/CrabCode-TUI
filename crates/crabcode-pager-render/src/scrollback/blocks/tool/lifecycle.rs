//! Standalone lifecycle-event presentation block.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/scrollback/blocks/tool/lifecycle.rs`,
//! SHA-256
//! `17da34da7a5c2054881b7c478d651d29d412a9e3c54f2755bae7ba7ba3e6e7d2`.
//! This leaf carries no tool execution or protocol authority.

use ratatui::text::{Line, Span};

use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockOutput, DisplayMode};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct LifecycleEventBlock {
    pub name: String,
}

impl LifecycleEventBlock {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl BlockContent for LifecycleEventBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);
        let style = if matches!(ctx.mode, DisplayMode::Collapsed) && muted_collapsed {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold = style.add_modifier(ratatui::style::Modifier::BOLD);
        BlockOutput {
            lines: vec![Line::from(vec![Span::styled(self.name.clone(), bold)]).into()],
        }
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn is_foldable(&self) -> bool {
        true
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn is_groupable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;

    fn context(mode: DisplayMode, selected: bool) -> BlockContext {
        BlockContext {
            mode,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: selected,
            cwd: None,
        }
    }

    #[test]
    fn event_name_is_the_only_rendered_payload() {
        let block = LifecycleEventBlock::new("session_start");
        let output = block.output(&context(DisplayMode::Collapsed, false));
        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].content.spans[0].content, "session_start");
    }

    #[test]
    fn lifecycle_block_keeps_fixed_group_and_fold_contract() {
        let block = LifecycleEventBlock::new("session_end");
        assert!(block.is_foldable());
        assert!(block.is_groupable());
        assert!(!block.has_vpad(&context(DisplayMode::Expanded, true)));
        assert_eq!(block.default_display_mode(), DisplayMode::Collapsed);
    }
}
