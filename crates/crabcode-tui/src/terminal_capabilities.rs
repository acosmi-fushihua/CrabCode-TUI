//! Narrow CrabCode adapters around the fixed upstream terminal matrix.
//!
//! Terminal detection, keyboard delivery, clipboard support, mouse safety,
//! multiplexer policy, and modifier fate are owned by
//! `crabcode_pager_render::audited_terminal`. Keeping a second table in the
//! application crate made the renderer capable of disagreeing with its own
//! terminal engine. This module now contains only the two CrabCode call-shape
//! adapters that are not part of that upstream matrix.

#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
use crabcode_pager_render::audited_host::HostOs;
use crabcode_pager_render::audited_terminal::Osc8Support;

#[cfg(test)]
pub(crate) use crabcode_pager_render::audited_terminal::ModifierFate;
pub(crate) use crabcode_pager_render::audited_terminal::{
    ModifierDelivery, MultiplexerKind, TerminalContext, TerminalName,
};

/// OSC 8 emission decision consumed by the CrabCode inline terminal backend.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HyperlinkRoute {
    pub(crate) emit_osc8: bool,
    pub(crate) emit_id: bool,
    pub(crate) skip_reason: Option<&'static str>,
}

/// Preserve the existing application call shape while delegating every
/// capability fact to the fixed upstream terminal matrix.
pub(crate) trait TerminalContextExt {
    fn hyperlink_route(&self) -> HyperlinkRoute;
}

impl TerminalContextExt for TerminalContext {
    fn hyperlink_route(&self) -> HyperlinkRoute {
        let capabilities = self.hyperlink_capabilities();
        let skip_reason = self.hyperlink_skip_reason();
        let emit_osc8 = capabilities.osc8 == Osc8Support::Native && skip_reason.is_none();
        HyperlinkRoute {
            emit_osc8,
            emit_id: emit_osc8 && capabilities.id_param,
            skip_reason,
        }
    }
}

pub(crate) fn terminal_context() -> &'static TerminalContext {
    crabcode_pager_render::audited_terminal::terminal_context()
}

#[cfg(test)]
pub(crate) fn terminal_context_from_env_for_test(entries: &[(&str, &str)]) -> TerminalContext {
    let env = entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<HashMap<_, _>>();
    crabcode_pager_render::audited_terminal::terminal_context_from_env_for_test(
        &env,
        HostOs::current(),
    )
}

/// True when `Ctrl+.` cannot be used as the advertised shortcuts key.
///
/// The fixed terminal route is one half of the policy. Native Windows and WSL
/// also pass modified keys through a console pipeline where `Ctrl+.` is not a
/// reliable distinct event.
pub(crate) fn ctrl_dot_shortcut_unreliable() -> bool {
    terminal_context().ctrl_dot_unreliable()
        || cfg!(target_os = "windows")
        || crabcode_pager_render::audited_host::is_wsl()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperlink_route_is_derived_from_the_fixed_matrix() {
        let context = terminal_context_from_env_for_test(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5.12"),
        ]);
        assert_eq!(
            context.hyperlink_route(),
            HyperlinkRoute {
                emit_osc8: true,
                emit_id: true,
                skip_reason: None,
            }
        );

        let hostile = terminal_context_from_env_for_test(&[("TERM_PROGRAM", "Apple_Terminal")]);
        assert_eq!(
            hostile.hyperlink_route().skip_reason,
            Some("apple_terminal")
        );
        assert!(!hostile.hyperlink_route().emit_osc8);
    }
}
