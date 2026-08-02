//! SessionEventBlock — typed session-level events displayed in scrollback.
//!
//! Unlike [`super::SystemMessageBlock`] (which renders arbitrary text),
//! `SessionEventBlock` uses a [`SessionEvent`] enum so each event variant
//! carries structured display data such as elapsed time, errors, and counts.

use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::tool::HookRunEntry;
use crate::render::wrapping::word_wrap_lines;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};
use crate::theme::Theme;
use crate::util::format_duration;

/// Shared text-selection range id for recap body lines (header is excluded).
const RECAP_BODY_RANGE: u16 = 0;

/// A session-level event with structured display data.
///
/// This is an internal renderer value model. It is not a backend or wire
/// protocol and carries no session-control authority.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Agent turn completed normally.
    TurnCompleted {
        /// Wall-clock elapsed time for the turn. `None` when unknown.
        elapsed: Option<Duration>,
    },
    /// Agent turn was cancelled by the user.
    TurnCancelled {
        /// Wall-clock elapsed time before cancellation.
        elapsed: Duration,
    },
    /// Agent turn was halted by the system.
    TurnHalted {
        /// Wall-clock elapsed time before the turn was halted.
        elapsed: Duration,
    },
    /// Agent turn failed with an error.
    TurnFailed {
        /// Error description.
        error: String,
        /// Elapsed time, if available.
        elapsed: Option<Duration>,
    },
    /// Auto-compaction started.
    CompactionStarted {
        /// Percentage of context window used.
        percentage: u8,
    },
    /// Auto-compaction completed successfully.
    CompactionCompleted {
        /// Tokens used before compaction (`None` from older backends).
        tokens_before: Option<u64>,
        /// Tokens used after compaction.
        tokens_after: u64,
        /// How long compaction took (milliseconds).
        elapsed_ms: Option<i64>,
    },
    /// Auto-compaction failed.
    CompactionFailed {
        /// Error description.
        error: String,
    },
    /// Auto-compaction was cancelled.
    CompactionCancelled,
    /// Retry failed — all retries exhausted or a non-retryable error.
    RetryFailed {
        /// Human-readable error description.
        error: String,
        /// Structured error category.
        error_type: Option<String>,
    },
    /// Authentication recovery was exhausted.
    ReAuthRequired,
    /// Terminal context overflow.
    ContextTooLarge,
    /// Manual compact command completed.
    CompactCompleted {
        /// Wall-clock elapsed time for the command.
        elapsed: Duration,
    },
    /// Hook annotation displayed inline after a tool call.
    HookAnnotation {
        /// The hook message.
        message: String,
    },
    /// The session's persisted model is no longer available.
    ModelUnavailable {
        previous_model_id: String,
        new_model_id: String,
        reason: String,
    },
    /// Memory was saved.
    MemorySaved {
        /// File path that was written.
        path: String,
        /// What triggered the save.
        trigger: String,
    },
    /// A goal finished.
    GoalCompleted {
        /// Goal end-to-end elapsed time.
        elapsed: Duration,
    },
    /// A session recap.
    Recap {
        /// The one-line recap text.
        summary: String,
        /// Whether this is an automatic return-from-away recap.
        auto: bool,
    },
}

impl SessionEvent {
    /// Format the event as a human-readable string.
    pub fn message(&self) -> String {
        match self {
            // Deliberately period-less — don't re-punctuate.
            SessionEvent::TurnCompleted {
                elapsed: Some(elapsed),
            } => {
                format!("Worked for {}", format_duration(*elapsed))
            }
            SessionEvent::TurnCompleted { elapsed: None } => "Turn completed.".to_string(),
            SessionEvent::TurnCancelled { elapsed } => {
                format!("Turn cancelled by user in {}.", format_duration(*elapsed))
            }
            SessionEvent::TurnHalted { elapsed } => {
                format!(
                    "Agent was unable to make progress \u{2014} turn ended in {}.",
                    format_duration(*elapsed)
                )
            }
            SessionEvent::TurnFailed {
                error,
                elapsed: Some(elapsed),
            } => {
                format!("Turn failed in {}: {error}", format_duration(*elapsed))
            }
            SessionEvent::TurnFailed {
                error,
                elapsed: None,
            } => {
                format!("Turn failed: {error}")
            }
            SessionEvent::CompactionStarted { percentage } => {
                format!("Context {percentage}% full. Compacting…")
            }
            SessionEvent::CompactionCompleted {
                tokens_before,
                tokens_after,
                elapsed_ms,
            } => {
                let after = format_tokens(*tokens_after);
                let body = match tokens_before {
                    Some(before) if *before > 0 => {
                        format!(
                            "Context compacted: {} → {after} tokens",
                            format_tokens(*before)
                        )
                    }
                    _ => format!("Context compacted → {after} tokens"),
                };
                if let Some(ms) = elapsed_ms {
                    let secs = *ms as f64 / 1000.0;
                    format!("{body} ({secs:.1}s)")
                } else {
                    body
                }
            }
            SessionEvent::CompactionFailed { error } => {
                if error.trim().is_empty() {
                    "Compaction failed.".to_string()
                } else {
                    format!("Compaction failed: {error}")
                }
            }
            SessionEvent::CompactionCancelled => "Compaction cancelled.".to_string(),
            SessionEvent::RetryFailed { error, error_type } => {
                if error_type.as_deref() == Some("encrypted_content_mismatch") {
                    "This session's conversation history is incompatible with the \
                     current model. Please start a new session."
                        .to_string()
                } else {
                    format!("Retry failed: {error}")
                }
            }
            SessionEvent::ReAuthRequired => {
                "Authentication required \u{2014} your session has expired or your \
                 credentials were rejected. Run /login to re-authenticate, then resend \
                 your message."
                    .to_string()
            }
            SessionEvent::ContextTooLarge => {
                "This conversation is too large for the model's context window. \
                 Use /new to start a new session."
                    .to_string()
            }
            SessionEvent::CompactCompleted { elapsed } => {
                format!("Compaction completed in {}.", format_duration(*elapsed))
            }
            SessionEvent::HookAnnotation { message } => message.clone(),
            SessionEvent::ModelUnavailable {
                new_model_id,
                reason,
                ..
            } => {
                if new_model_id.is_empty() {
                    reason.clone()
                } else {
                    format!("{reason} Switched to \"{new_model_id}\".")
                }
            }
            SessionEvent::MemorySaved { path, trigger } => {
                let short_path = crate::util::abbreviate_path(path);
                format!("Memory saved ({trigger}) \u{2192} {short_path}  \u{00b7}  /memory to view")
            }
            SessionEvent::GoalCompleted { elapsed } => {
                format!(
                    "Goal complete \u{2014} {} end-to-end.",
                    format_duration(*elapsed)
                )
            }
            SessionEvent::Recap { summary, auto: _ } => {
                format!("Recap \u{2014} {summary}")
            }
        }
    }

    /// The recap summary text when this is a recap event.
    fn recap_summary(&self) -> Option<&str> {
        match self {
            SessionEvent::Recap { summary, .. } => Some(summary.as_str()),
            _ => None,
        }
    }

    /// Whether this event marks the end of an agent turn.
    pub fn is_turn_terminal(&self) -> bool {
        matches!(
            self,
            SessionEvent::TurnCompleted { .. }
                | SessionEvent::TurnCancelled { .. }
                | SessionEvent::TurnHalted { .. }
                | SessionEvent::TurnFailed { .. }
        )
    }
}

/// Format a token count with "k" suffix for thousands.
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// Block that renders a [`SessionEvent`] in scrollback.
#[derive(Debug, Clone)]
pub struct SessionEventBlock {
    /// The typed event data.
    pub event: SessionEvent,
    /// Stop/stop_failure hook runs folded into a turn-terminal marker.
    pub stop_hooks: Vec<(String, Vec<HookRunEntry>)>,
    /// The prompt turn a terminal marker belongs to, when known.
    pub prompt_id: Option<String>,
    /// Whether the marker was pushed while the turn was still running.
    pub parked: bool,
}

impl SessionEventBlock {
    /// Create a new session event block.
    pub fn new(event: SessionEvent) -> Self {
        Self {
            event,
            stop_hooks: Vec::new(),
            prompt_id: None,
            parked: false,
        }
    }

    /// A turn-terminal marker carrying the turn's stop-hook runs and prompt id.
    pub fn with_stop_hooks(
        event: SessionEvent,
        stop_hooks: Vec<(String, Vec<HookRunEntry>)>,
        prompt_id: Option<String>,
    ) -> Self {
        debug_assert!(stop_hooks.is_empty() || event.is_turn_terminal());
        Self {
            event,
            stop_hooks,
            prompt_id,
            parked: false,
        }
    }

    /// Whether this marker may carry or accept stop-hook runs.
    pub fn accepts_stop_hooks(&self) -> bool {
        self.event.is_turn_terminal() && !self.parked
    }

    /// Whether any attached stop hook actually ran.
    pub fn has_stop_hook_content(&self) -> bool {
        self.stop_hooks.iter().any(|(_, runs)| {
            runs.iter()
                .any(|r| !matches!(r.status, super::tool::HookRunStatus::Skipped))
        })
    }

    /// A recap with real body content.
    fn recap_has_body(&self) -> bool {
        self.event
            .recap_summary()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Merge stop-hook summaries and expanded detail into marker output.
    fn append_stop_hooks(&self, lines: &mut Vec<BlockLine>, ctx: &BlockContext) {
        use super::tool::hook::{render_hooks_for_mode, render_stop_hooks_summary};

        if !self.has_stop_hook_content() {
            return;
        }
        let Some(summary) = render_stop_hooks_summary(&self.stop_hooks) else {
            return;
        };

        let avail = ctx.width as usize;
        let summary_width: usize = summary
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let single_line = lines.len() == 1;
        if let Some(first) = lines.first_mut() {
            let text = crate::scrollback::types::line_plain_text(&first.content);
            let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
            let text_spans = first.content.spans.len();
            if single_line && text_width + 2 + summary_width <= avail {
                let pad = avail - text_width - summary_width;
                first.content.spans.push(Span::raw(" ".repeat(pad)));
                first.content.spans.extend(summary);
                first.selectable = Selectable::Spans(0..text_spans);
                first.selection_text = Some(text);
            } else {
                let pad = avail.saturating_sub(summary_width);
                let mut spans = vec![Span::raw(" ".repeat(pad))];
                spans.extend(summary);
                lines.push(BlockLine::separator(Line::from(spans)));
            }
        }

        if !matches!(ctx.mode, DisplayMode::Collapsed) {
            let multiple = self.stop_hooks.len() > 1;
            for (event_name, runs) in &self.stop_hooks {
                let detail = if multiple {
                    render_hooks_for_mode(event_name, runs, ctx.mode)
                } else {
                    super::tool::hook::render_hooks_detail(runs, ctx.mode)
                };
                lines.extend(detail);
            }
        }
    }

    /// Render a recap event in the tool-call visual style.
    fn recap_output(&self, ctx: &BlockContext, summary: &str) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        let header_text_style = if muted_collapsed {
            theme.muted()
        } else {
            theme.primary()
        };
        let header_style = header_text_style.add_modifier(Modifier::BOLD);
        let header_line =
            || BlockLine::separator(Line::from(Span::styled("Recap".to_string(), header_style)));

        if ctx.is_running {
            return BlockOutput {
                lines: vec![header_line()],
            };
        }

        match ctx.mode {
            DisplayMode::Collapsed => {
                let mut spans = vec![Span::styled("Recap".to_string(), header_style)];
                let preview = summary.lines().next().unwrap_or(summary).trim();
                if !preview.is_empty() {
                    spans.push(Span::styled(format!("  {preview}"), theme.muted()));
                }
                let line = crate::render::line_utils::truncate_line(
                    Line::from(spans),
                    ctx.content_width(),
                );
                let selectable = if preview.is_empty() {
                    Selectable::None
                } else {
                    Selectable::Spans(1..2)
                };
                BlockOutput {
                    lines: vec![BlockLine {
                        content: line,
                        selectable,
                        selection_range: (!preview.is_empty()).then_some(RECAP_BODY_RANGE),
                        selection_text: (!preview.is_empty()).then(|| preview.to_string()),
                        ..Default::default()
                    }],
                }
            }
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let mut lines: Vec<BlockLine> = vec![header_line()];
                lines.push(BlockLine::separator(Line::from("")));

                let styled_lines = summary
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_string(), theme.muted())));
                let wrapped =
                    word_wrap_lines(styled_lines, (ctx.width as usize).saturating_sub(2).max(20));
                for wrapped_line in wrapped {
                    lines.push(
                        BlockLine::styled(wrapped_line)
                            .with_selection_range(Some(RECAP_BODY_RANGE)),
                    );
                }

                BlockOutput { lines }
            }
        }
    }
}

impl BlockContent for SessionEventBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        if let Some(summary) = self.event.recap_summary() {
            return self.recap_output(ctx, summary);
        }

        let theme = Theme::current();
        let style = if matches!(
            self.event,
            SessionEvent::ReAuthRequired
                | SessionEvent::ContextTooLarge
                | SessionEvent::CompactionFailed { .. }
        ) {
            ratatui::style::Style::default().fg(theme.warning)
        } else {
            theme.muted()
        };

        let text = self.event.message();
        let wrapped = if text.contains('\n') {
            let input_lines = text
                .split('\n')
                .map(|s| Line::from(Span::styled(s.to_owned(), style)));
            word_wrap_lines(input_lines, ctx.width as usize)
        } else {
            word_wrap_lines(
                std::iter::once(Line::from(Span::styled(text, style))),
                ctx.width as usize,
            )
        };
        let mut lines: Vec<BlockLine> = wrapped
            .into_iter()
            .map(|line| BlockLine::styled(line).with_selection_range(Some(0)))
            .collect();

        if lines.is_empty() {
            lines.push(BlockLine::styled(Line::from("")).with_selection_range(Some(0)));
        }
        self.append_stop_hooks(&mut lines, ctx);
        BlockOutput { lines }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        if self.event.recap_summary().is_some() {
            if ctx.is_running {
                return Some(AccentStyle::animated(theme.gray));
            }
            return (ctx.mode != DisplayMode::Collapsed)
                .then(|| AccentStyle::static_color(theme.accent_tool));
        }
        if matches!(
            self.event,
            SessionEvent::ReAuthRequired
                | SessionEvent::ContextTooLarge
                | SessionEvent::CompactionFailed { .. }
        ) {
            Some(AccentStyle::static_color(theme.warning))
        } else {
            None
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if self.event.recap_summary().is_some()
            && !ctx.is_running
            && ctx.mode == DisplayMode::Collapsed
        {
            return None;
        }
        self.accent(ctx)
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        self.recap_has_body() || self.has_stop_hook_content()
    }

    fn is_selectable(&self) -> bool {
        self.recap_has_body() || self.has_stop_hook_content()
    }

    fn default_display_mode(&self) -> DisplayMode {
        if self.has_stop_hook_content() {
            DisplayMode::Collapsed
        } else {
            DisplayMode::Expanded
        }
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        self.event.recap_summary().is_some()
            && ctx
                .appearance
                .scrollback
                .blocks
                .tool
                .bullet
                .char()
                .is_some()
    }

    fn is_groupable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::blocks::tool::HookRunStatus;

    fn ctx(mode: DisplayMode) -> BlockContext {
        BlockContext {
            mode,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: crate::appearance::AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn plain(line: &BlockLine) -> String {
        crate::scrollback::types::line_plain_text(&line.content)
    }

    fn stop_group(name: &str) -> (String, Vec<HookRunEntry>) {
        (
            name.to_string(),
            vec![HookRunEntry {
                name: "global/notify".into(),
                status: HookRunStatus::Success {
                    elapsed: Duration::from_millis(12),
                },
                output: None,
            }],
        )
    }

    #[test]
    fn terminal_event_messages_preserve_fixed_duration_contract() {
        assert_eq!(
            SessionEvent::TurnCompleted {
                elapsed: Some(Duration::from_secs(125))
            }
            .message(),
            "Worked for 2m5s"
        );
        assert_eq!(
            SessionEvent::TurnCompleted { elapsed: None }.message(),
            "Turn completed."
        );
        assert_eq!(
            SessionEvent::TurnCancelled {
                elapsed: Duration::from_secs(10)
            }
            .message(),
            "Turn cancelled by user in 10s."
        );
        assert_eq!(
            SessionEvent::TurnHalted {
                elapsed: Duration::from_secs(45)
            }
            .message(),
            "Agent was unable to make progress \u{2014} turn ended in 45s."
        );
        assert_eq!(
            SessionEvent::TurnFailed {
                error: "connection reset".into(),
                elapsed: Some(Duration::from_secs(3))
            }
            .message(),
            "Turn failed in 3.0s: connection reset"
        );
    }

    #[test]
    fn compaction_messages_cover_before_after_legacy_and_failure() {
        assert_eq!(
            SessionEvent::CompactionCompleted {
                tokens_before: Some(48_800),
                tokens_after: 27_100,
                elapsed_ms: Some(21_000)
            }
            .message(),
            "Context compacted: 48.8k → 27.1k tokens (21.0s)"
        );
        assert_eq!(
            SessionEvent::CompactionCompleted {
                tokens_before: None,
                tokens_after: 27_100,
                elapsed_ms: None
            }
            .message(),
            "Context compacted → 27.1k tokens"
        );
        assert_eq!(
            SessionEvent::CompactionFailed {
                error: String::new()
            }
            .message(),
            "Compaction failed."
        );
    }

    #[test]
    fn action_messages_keep_recovery_commands() {
        assert!(SessionEvent::ReAuthRequired.message().contains("/login"));
        assert!(SessionEvent::ContextTooLarge.message().contains("/new"));
        assert_eq!(
            SessionEvent::RetryFailed {
                error: "raw error".into(),
                error_type: Some("encrypted_content_mismatch".into())
            }
            .message(),
            "This session's conversation history is incompatible with the current model. Please start a new session."
        );
    }

    #[test]
    fn remaining_structured_variants_have_stable_messages() {
        assert_eq!(
            SessionEvent::CompactionStarted { percentage: 85 }.message(),
            "Context 85% full. Compacting…"
        );
        assert_eq!(
            SessionEvent::CompactCompleted {
                elapsed: Duration::from_secs(4)
            }
            .message(),
            "Compaction completed in 4.0s."
        );
        assert_eq!(
            SessionEvent::HookAnnotation {
                message: "annotation".into()
            }
            .message(),
            "annotation"
        );
        assert_eq!(
            SessionEvent::GoalCompleted {
                elapsed: Duration::from_secs(619)
            }
            .message(),
            "Goal complete \u{2014} 10m19s end-to-end."
        );
        assert_eq!(
            SessionEvent::Recap {
                summary: "fixed parser".into(),
                auto: true
            }
            .message(),
            "Recap \u{2014} fixed parser"
        );
    }

    #[test]
    fn recap_expanded_and_collapsed_keep_selection_contract() {
        let block = SessionEventBlock::new(SessionEvent::Recap {
            summary: "First line.\nSecond line.".into(),
            auto: false,
        });
        let expanded = block.output(&ctx(DisplayMode::Expanded));
        assert_eq!(plain(&expanded.lines[0]), "Recap");
        assert!(matches!(expanded.lines[0].selectable, Selectable::None));
        assert!(matches!(expanded.lines[1].selectable, Selectable::None));
        assert_eq!(expanded.lines[2].selection_range, Some(RECAP_BODY_RANGE));

        let collapsed = block.output(&ctx(DisplayMode::Collapsed));
        assert_eq!(collapsed.lines.len(), 1);
        assert!(plain(&collapsed.lines[0]).contains("First line."));
        assert!(matches!(
            &collapsed.lines[0].selectable,
            Selectable::Spans(range) if *range == (1..2)
        ));
        assert_eq!(
            collapsed.lines[0].selection_text.as_deref(),
            Some("First line.")
        );
    }

    #[test]
    fn recap_loading_and_empty_are_not_interactive() {
        let block = SessionEventBlock::new(SessionEvent::Recap {
            summary: String::new(),
            auto: false,
        });
        assert!(!block.is_foldable());
        assert!(!block.is_selectable());
        let mut running = ctx(DisplayMode::Expanded);
        running.is_running = true;
        let out = block.output(&running);
        assert_eq!(out.lines.len(), 1);
        assert_eq!(plain(&out.lines[0]), "Recap");
        assert!(block.accent(&running).is_some_and(|accent| accent.animated));
    }

    #[test]
    fn stop_hooks_summary_and_detail_preserve_fold_contract() {
        let block = SessionEventBlock::with_stop_hooks(
            SessionEvent::TurnCompleted {
                elapsed: Some(Duration::from_secs(5)),
            },
            vec![stop_group("stop")],
            Some("prompt-1".into()),
        );
        assert!(block.accepts_stop_hooks());
        assert!(block.has_stop_hook_content());
        assert!(block.is_foldable());
        assert!(block.is_selectable());
        assert_eq!(block.default_display_mode(), DisplayMode::Collapsed);

        let collapsed = block.output(&ctx(DisplayMode::Collapsed));
        assert_eq!(collapsed.lines.len(), 1);
        let collapsed_text = plain(&collapsed.lines[0]);
        assert!(collapsed_text.starts_with("Worked for 5.0s"));
        assert!(collapsed_text.ends_with("stop  [hooks: 1]"));
        assert_eq!(
            collapsed.lines[0].selection_text.as_deref(),
            Some("Worked for 5.0s")
        );

        let expanded = block.output(&ctx(DisplayMode::Expanded));
        let text = expanded.iter_plain();
        assert!(text.contains("global/notify (12ms)"));
    }

    #[test]
    fn parked_and_nonterminal_events_reject_stop_hooks() {
        let parked = SessionEventBlock {
            event: SessionEvent::TurnCompleted {
                elapsed: Some(Duration::from_secs(24)),
            },
            stop_hooks: Vec::new(),
            prompt_id: None,
            parked: true,
        };
        assert!(!parked.accepts_stop_hooks());
        assert!(!SessionEventBlock::new(SessionEvent::CompactionCancelled).accepts_stop_hooks());
    }

    trait PlainOutput {
        fn iter_plain(&self) -> String;
    }

    impl PlainOutput for BlockOutput {
        fn iter_plain(&self) -> String {
            self.lines.iter().map(plain).collect::<Vec<_>>().join("\n")
        }
    }
}
