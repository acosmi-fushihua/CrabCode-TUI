//! Pure layout contract for Mermaid actions shown below terminal box art.
//!
//! The PNG is never painted inline. These labels reserve one terminal row for
//! opening the lazily rendered PNG, copying its path, or copying the original
//! diagram source. Painting and hit-testing both consume this layout so their
//! columns cannot drift.

use unicode_width::UnicodeWidthStr as _;

use crate::tui_links::MermaidAffordanceAction;

pub(crate) const MERMAID_INFO: &str = "mermaid";
pub(crate) const MERMAID_LABEL: &str = "◇ mermaid";
pub(crate) const MERMAID_RENDERING: &str = "rendering diagram…";
pub(crate) const AFFORDANCE_GAP: u16 = 3;

const AFFORDANCE_OPEN: &str = "[Open Image]";
const AFFORDANCE_COPY_PATH: &str = "[Copy Image Path]";
const AFFORDANCE_COPY_SOURCE: &str = "[Copy Source]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MermaidAffordanceButton {
    pub(crate) label: &'static str,
    pub(crate) action: MermaidAffordanceAction,
    pub(crate) column: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MermaidAffordanceLayout {
    pub(crate) label: (u16, &'static str),
    pub(crate) buttons: [MermaidAffordanceButton; 3],
    pub(crate) status: Option<(u16, &'static str)>,
}

pub(crate) fn is_mermaid_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case(MERMAID_INFO))
}

pub(crate) fn layout(rendering: bool) -> MermaidAffordanceLayout {
    let buttons_start = display_width(MERMAID_LABEL).saturating_add(AFFORDANCE_GAP);
    let specs = [
        (AFFORDANCE_OPEN, MermaidAffordanceAction::Open),
        (AFFORDANCE_COPY_PATH, MermaidAffordanceAction::CopyPath),
        (AFFORDANCE_COPY_SOURCE, MermaidAffordanceAction::CopySource),
    ];
    let mut column = buttons_start;
    let buttons = specs.map(|(label, action)| {
        let button = MermaidAffordanceButton {
            label,
            action,
            column,
        };
        column = column
            .saturating_add(display_width(label))
            .saturating_add(AFFORDANCE_GAP);
        button
    });
    let status = rendering.then_some((column, MERMAID_RENDERING));
    MermaidAffordanceLayout {
        label: (0, MERMAID_LABEL),
        buttons,
        status,
    }
}

pub(crate) fn segment_fits(column: u16, label: &str, row_width: u16) -> bool {
    column
        .checked_add(display_width(label))
        .is_some_and(|end| end <= row_width)
}

pub(crate) fn display_width(text: &str) -> u16 {
    u16::try_from(text.width()).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_first_token_matches_case_insensitively() {
        for info in ["mermaid", "Mermaid", "MERMAID", "mermaid theme=base"] {
            assert!(is_mermaid_info(info), "{info:?}");
        }
        for info in ["", "mermaidx", "rust mermaid"] {
            assert!(!is_mermaid_info(info), "{info:?}");
        }
    }

    #[test]
    fn labels_columns_and_gaps_are_one_layout_contract() {
        let idle = layout(false);
        assert_eq!(idle.label, (0, "◇ mermaid"));
        assert_eq!(
            idle.buttons.map(|button| (button.label, button.action)),
            [
                ("[Open Image]", MermaidAffordanceAction::Open),
                ("[Copy Image Path]", MermaidAffordanceAction::CopyPath),
                ("[Copy Source]", MermaidAffordanceAction::CopySource),
            ]
        );
        assert_eq!(idle.buttons.map(|button| button.column), [12, 27, 47]);
        for pair in idle.buttons.windows(2) {
            assert_eq!(
                pair[1].column,
                pair[0]
                    .column
                    .saturating_add(display_width(pair[0].label))
                    .saturating_add(AFFORDANCE_GAP)
            );
        }
        assert_eq!(idle.status, None);
    }

    #[test]
    fn rendering_hint_follows_the_last_button_without_moving_buttons() {
        let idle = layout(false);
        let busy = layout(true);
        assert_eq!(busy.label, idle.label);
        assert_eq!(busy.buttons, idle.buttons);
        assert_eq!(busy.status, Some((63, "rendering diagram…")));
    }

    #[test]
    fn a_segment_is_drawn_only_when_its_complete_label_fits() {
        assert!(segment_fits(12, "[Open Image]", 24));
        assert!(!segment_fits(12, "[Open Image]", 23));
        assert!(!segment_fits(u16::MAX, "x", u16::MAX));
    }
}
