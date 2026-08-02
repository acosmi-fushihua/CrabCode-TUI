//! Direct-TUI adapter for the unique context-usage renderer owner.
//!
//! Parsing and painting live in `crabcode-pager-render`, so the existing
//! `/context` modal and the literal scrollback denominator cannot diverge.

pub(crate) use crabcode_pager_render::context_visualization::ContextVisualization;

#[cfg(test)]
pub(crate) use crabcode_pager_render::context_visualization::minimal_test_control_response;
