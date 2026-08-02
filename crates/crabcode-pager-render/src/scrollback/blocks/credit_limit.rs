//! Credit-limit presentation card.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/scrollback/blocks/credit_limit.rs`,
//! SHA-256
//! `882719185886b8a84ae153a9e87bb34c6b2adfb32518b78f212ea10595efef80`.
//! Product difference: fixed-source product-tier prose and destination
//! fixtures are neutralized. The block renders only values supplied by a
//! caller; no billing authority or protocol mapping is introduced.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockLine, BlockOutput, DisplayMode};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditLimitCardAction {
    EnablePayg,
    IncreasePaygLimit,
    PurchaseCredits,
}

#[derive(Debug, Clone)]
pub struct CreditLimitBlock {
    pub heading: String,
    pub action: CreditLimitCardAction,
    pub url: String,
}

impl CreditLimitBlock {
    pub fn new(
        heading: impl Into<String>,
        action: CreditLimitCardAction,
        url: impl Into<String>,
    ) -> Self {
        Self {
            heading: heading.into(),
            action,
            url: url.into(),
        }
    }
}

impl BlockContent for CreditLimitBlock {
    fn output(&self, _ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let heading_style = Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD);
        let heading = Line::from(Span::styled(self.heading.clone(), heading_style));

        let body = match self.action {
            CreditLimitCardAction::IncreasePaygLimit => {
                "You can continue by increasing your spending limit."
            }
            CreditLimitCardAction::EnablePayg => {
                "You can continue by enabling pay-as-you-go usage."
            }
            CreditLimitCardAction::PurchaseCredits => {
                "You can continue by purchasing more credits."
            }
        };
        let body_line = Line::from(Span::styled(body.to_string(), theme.muted()));
        let link_line = Line::from(Span::styled(self.url.clone(), theme.link_style()));

        BlockOutput {
            lines: vec![
                BlockLine::styled(heading).with_selection_range(Some(0)),
                BlockLine::separator(Line::from("")),
                BlockLine::styled(body_line).with_selection_range(Some(0)),
                BlockLine::styled(link_line).with_selection_range(Some(0)),
            ],
        }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        Some(AccentStyle::static_color(Theme::current().warning))
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        true
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Expanded
    }

    fn is_selectable(&self) -> bool {
        true
    }

    fn is_groupable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;

    fn context() -> BlockContext {
        BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn output_text(block: &CreditLimitBlock) -> String {
        block
            .output(&context())
            .lines
            .iter()
            .flat_map(|line| line.content.spans.iter().map(|span| span.content.as_ref()))
            .collect()
    }

    #[test]
    fn action_copy_is_selected_by_fixed_enum_variant() {
        let url = "https://example.invalid/usage";
        let enable = CreditLimitBlock::new("limit", CreditLimitCardAction::EnablePayg, url);
        assert!(output_text(&enable).contains("enabling pay-as-you-go"));
        assert!(output_text(&enable).contains(url));

        let increase =
            CreditLimitBlock::new("limit", CreditLimitCardAction::IncreasePaygLimit, url);
        assert!(output_text(&increase).contains("increasing your spending limit"));

        let purchase = CreditLimitBlock::new("limit", CreditLimitCardAction::PurchaseCredits, url);
        assert!(output_text(&purchase).contains("purchasing more credits"));
    }

    #[test]
    fn card_structure_and_navigation_contract_match_fixed_source() {
        let block = CreditLimitBlock::new(
            "limit",
            CreditLimitCardAction::PurchaseCredits,
            "https://example.invalid/usage",
        );
        let output = block.output(&context());
        assert_eq!(output.lines.len(), 4);
        assert!(
            output.lines[0]
                .content
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(!block.is_foldable());
        assert!(block.is_selectable());
        assert!(!block.is_groupable());
        assert!(block.has_vpad(&context()));
    }
}
