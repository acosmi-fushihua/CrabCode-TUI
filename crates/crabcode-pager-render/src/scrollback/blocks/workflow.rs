//! Workflow lifecycle block.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/scrollback/blocks/workflow.rs`, SHA-256
//! `235b29e196fcdabe7422911950a360b0df31812bdf1ce1eab261e9ed538a27cd`.
//! No backend or protocol type is referenced; the fixed renderer consumes
//! renderer-owned values supplied by a future read-only projection.

use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use crate::render::color::blend_color;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{AccentStyle, BlockContext, BlockOutput, DisplayMode};
use crate::theme::Theme;
use crate::util::format_duration;

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowBlockStatus {
    Running,
    Done { elapsed: Duration },
    Failed { elapsed: Duration },
    Cancelled { elapsed: Duration },
    Paused { elapsed: Duration },
}

#[derive(Debug, Clone)]
pub struct WorkflowBlockPhase {
    pub title: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowBlock {
    pub run_id: String,
    pub name: String,
    pub objective: String,
    pub status: WorkflowBlockStatus,
    pub phases: Vec<WorkflowBlockPhase>,
    pub current_phase: Option<String>,
    pub active_agents: u32,
    pub elapsed: Duration,
}

impl WorkflowBlock {
    pub fn started(
        run_id: impl Into<String>,
        name: impl Into<String>,
        objective: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            name: name.into(),
            objective: objective.into(),
            status: WorkflowBlockStatus::Running,
            phases: Vec::new(),
            current_phase: None,
            active_agents: 0,
            elapsed: Duration::ZERO,
        }
    }

    fn phase_trail(&self) -> Option<String> {
        if self.phases.is_empty() {
            return self.current_phase.clone();
        }
        Some(
            self.phases
                .iter()
                .map(|phase| {
                    let mark = match phase.state.as_str() {
                        "done" => "✓",
                        "active" => "●",
                        _ => "○",
                    };
                    format!("{} {mark}", phase.title)
                })
                .collect::<Vec<_>>()
                .join(" · "),
        )
    }
}

impl BlockContent for WorkflowBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let bold = if ctx.is_selected {
            theme.primary().add_modifier(Modifier::BOLD)
        } else {
            theme.muted().add_modifier(Modifier::BOLD)
        };
        let muted = theme.muted();

        let mut spans = vec![Span::styled("Workflow ", bold)];
        let verb = match &self.status {
            WorkflowBlockStatus::Running => format!("{}: ", self.name),
            WorkflowBlockStatus::Done { elapsed } => {
                format!("{} done in {}: ", self.name, format_duration(*elapsed))
            }
            WorkflowBlockStatus::Failed { elapsed } => {
                format!("{} failed in {}: ", self.name, format_duration(*elapsed))
            }
            WorkflowBlockStatus::Cancelled { elapsed } => {
                format!(
                    "{} ◌ cancelled after {}: ",
                    self.name,
                    format_duration(*elapsed)
                )
            }
            WorkflowBlockStatus::Paused { elapsed } => {
                format!("{} paused at {}: ", self.name, format_duration(*elapsed))
            }
        };
        let text_style = if matches!(self.status, WorkflowBlockStatus::Cancelled { .. }) {
            theme.dim()
        } else {
            muted
        };
        spans.push(Span::styled(verb, text_style));
        spans.push(Span::styled(self.objective.replace('\n', " "), text_style));
        if let Some(trail) = self.phase_trail()
            && !trail.is_empty()
        {
            spans.push(Span::styled(format!("  [{trail}]"), text_style));
        }
        if matches!(self.status, WorkflowBlockStatus::Running) && self.active_agents > 0 {
            spans.push(Span::styled(
                format!("  ({} agents)", self.active_agents),
                muted,
            ));
        }

        BlockOutput {
            lines: vec![Line::from(spans).into()],
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.status {
            WorkflowBlockStatus::Running if ctx.is_running => {
                Some(AccentStyle::static_color(theme.accent_running))
            }
            _ => None,
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.status {
            WorkflowBlockStatus::Running if ctx.is_running => {
                let dim = ctx.appearance.scrollback.display.dim_accent;
                let dimmed = blend_color(theme.bg_base, theme.accent_running, dim)
                    .unwrap_or(theme.accent_running);
                Some(AccentStyle::animated(dimmed))
            }
            WorkflowBlockStatus::Running => None,
            WorkflowBlockStatus::Done { .. } => {
                Some(AccentStyle::static_color(theme.accent_success))
            }
            WorkflowBlockStatus::Failed { .. } => {
                Some(AccentStyle::static_color(theme.accent_error))
            }
            WorkflowBlockStatus::Cancelled { .. } => {
                Some(AccentStyle::static_color(theme.gray_dim))
            }
            WorkflowBlockStatus::Paused { .. } => Some(AccentStyle::static_color(theme.warning)),
        }
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

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn is_selectable(&self) -> bool {
        true
    }

    fn has_bullet(&self, _ctx: &BlockContext) -> bool {
        true
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, _ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        let mut lines = vec![
            Line::from(Span::styled(self.objective.clone(), theme.primary())),
            Line::from(""),
        ];
        for phase in &self.phases {
            let mark = match phase.state.as_str() {
                "done" => "✓",
                "active" => "●",
                _ => "○",
            };
            lines.push(Line::from(Span::styled(
                format!("  {mark} {}", phase.title),
                theme.muted(),
            )));
        }
        Some(Text::from(lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;

    fn context(running: bool) -> BlockContext {
        BlockContext {
            mode: DisplayMode::Collapsed,
            is_running: running,
            width: 120,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn line_text(block: &WorkflowBlock) -> String {
        block.output(&context(false)).lines[0]
            .content
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn running_line_preserves_phase_and_agent_projection() {
        let mut block = WorkflowBlock::started("wf_1", "research", "compare A and B");
        block.phases = vec![
            WorkflowBlockPhase {
                title: "Plan".into(),
                state: "done".into(),
            },
            WorkflowBlockPhase {
                title: "Research".into(),
                state: "active".into(),
            },
        ];
        block.active_agents = 3;
        let text = line_text(&block);
        assert!(text.contains("Plan ✓"));
        assert!(text.contains("Research ●"));
        assert!(text.contains("(3 agents)"));
    }

    #[test]
    fn terminal_states_preserve_fixed_duration_and_bullet_semantics() {
        let mut block = WorkflowBlock::started("wf_1", "research", "q");
        block.status = WorkflowBlockStatus::Done {
            elapsed: Duration::from_secs(90),
        };
        assert!(line_text(&block).contains("done in 1m30s"));
        assert_eq!(
            block.bullet(&context(false)),
            Some(AccentStyle::static_color(Theme::current().accent_success))
        );

        block.status = WorkflowBlockStatus::Cancelled {
            elapsed: Duration::from_secs(45),
        };
        assert!(line_text(&block).contains("◌ cancelled after 45s"));
        assert_eq!(
            block.bullet(&context(false)),
            Some(AccentStyle::static_color(Theme::current().gray_dim))
        );
    }

    #[test]
    fn running_animation_depends_on_entry_lifecycle_flag() {
        let block = WorkflowBlock::started("wf_1", "research", "q");
        assert!(block.accent(&context(false)).is_none());
        assert!(block.accent(&context(true)).is_some());
        assert!(
            block
                .bullet(&context(true))
                .is_some_and(|style| style.animated)
        );
    }
}
