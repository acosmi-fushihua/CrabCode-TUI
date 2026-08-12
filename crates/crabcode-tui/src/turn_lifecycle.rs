//! Grok-style turn lifecycle owned by the renderer.
//!
//! CrabCode wire events are normalized into this small state machine before
//! presentation. The backend remains authoritative for capabilities and
//! safety/control decisions; this module owns only renderer timing, activity,
//! waiting, and terminal convergence.

use std::time::{Duration, Instant};

use crate::sdk_projection::{DirectStreamActivityPhase, DirectStreamActivityState};

const ANIMATION_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
#[cfg(test)]
const SPINNER_DIVISOR: u64 = 4;
#[cfg(test)]
const WATCHER_PULSE_DIVISOR: u64 = SPINNER_DIVISOR * 2;
#[cfg(test)]
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Idle-surviving work that can wake the agent after the foreground turn has
/// completed. This mirrors Grok's watcher ownership instead of treating a
/// foreground `result` as the visual terminal state of every background job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Watchers {
    pub(crate) commands: usize,
    pub(crate) monitors: usize,
    pub(crate) loops: usize,
    pub(crate) subagents: usize,
    pub(crate) workflows: usize,
}

impl Watchers {
    pub(crate) fn total(self) -> usize {
        self.commands
            .saturating_add(self.monitors)
            .saturating_add(self.loops)
            .saturating_add(self.subagents)
            .saturating_add(self.workflows)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentState {
    #[default]
    Idle,
    TurnRunning,
    Cancelling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitingReason {
    Model,
    User,
    Subagent,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnActivity {
    Requesting,
    Thinking,
    Responding,
    ToolInput,
    ToolUse,
    Waiting(WaitingReason),
    Retrying,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnOutcome {
    Complete,
    Failed,
    Cancelled,
    RuntimeStopped,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnStatus {
    state: AgentState,
    activity: Option<TurnActivity>,
    generation: u64,
    started_at: Option<Instant>,
    activity_started_at: Option<Instant>,
    last_outcome: Option<TurnOutcome>,
    last_elapsed: Option<Duration>,
    animation_tick: u64,
    next_animation_at: Option<Instant>,
    watchers: Watchers,
}

impl Default for TurnStatus {
    fn default() -> Self {
        Self {
            state: AgentState::Idle,
            activity: None,
            generation: 0,
            started_at: None,
            activity_started_at: None,
            last_outcome: None,
            last_elapsed: None,
            animation_tick: 0,
            next_animation_at: None,
            watchers: Watchers::default(),
        }
    }
}

impl TurnStatus {
    pub(crate) fn state(&self) -> AgentState {
        self.state
    }

    pub(crate) fn activity(&self) -> Option<TurnActivity> {
        self.activity
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn activity_started_at(&self) -> Option<Instant> {
        self.activity_started_at
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Option<Duration> {
        self.started_at
            .map(|started| now.saturating_duration_since(started))
    }

    pub(crate) fn last_outcome(&self) -> Option<TurnOutcome> {
        self.last_outcome
    }

    pub(crate) fn last_elapsed(&self) -> Option<Duration> {
        self.last_elapsed
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state, AgentState::TurnRunning | AgentState::Cancelling)
    }

    pub(crate) fn watchers(&self) -> Watchers {
        self.watchers
    }

    fn needs_animation(&self) -> bool {
        self.is_running() || self.watchers.total() > 0
    }

    pub(crate) fn set_watchers(&mut self, watchers: Watchers, now: Instant) {
        self.watchers = watchers;
        if self.needs_animation() {
            self.next_animation_at
                .get_or_insert(now + ANIMATION_INTERVAL);
        } else {
            self.next_animation_at = None;
        }
    }

    pub(crate) fn begin(&mut self, generation: u64, now: Instant) {
        if self.is_running() {
            // QueryEngine emits stream_request_start once per inference loop,
            // including the continuation after a tool. Advance the request
            // generation without resetting the enclosing user-turn clock.
            self.generation = self.generation.max(generation);
            self.next_animation_at
                .get_or_insert(now + ANIMATION_INTERVAL);
            return;
        }
        self.state = AgentState::TurnRunning;
        self.activity = None;
        self.generation = generation;
        self.started_at = Some(now);
        self.activity_started_at = None;
        self.last_outcome = None;
        self.last_elapsed = None;
        self.animation_tick = 0;
        self.next_animation_at = Some(now + ANIMATION_INTERVAL);
    }

    pub(crate) fn begin_if_idle(&mut self, now: Instant) {
        if !self.is_running() {
            self.begin(self.generation.saturating_add(1), now);
        }
    }

    pub(crate) fn set_activity(&mut self, activity: TurnActivity, now: Instant) {
        self.begin_if_idle(now);
        self.state = AgentState::TurnRunning;
        if self.activity != Some(activity) {
            self.activity = Some(activity);
            self.activity_started_at = Some(now);
        }
        self.next_animation_at
            .get_or_insert(now + ANIMATION_INTERVAL);
    }

    pub(crate) fn observe_direct_stream(
        &mut self,
        activity: &DirectStreamActivityState,
        now: Instant,
    ) {
        if activity.phase == DirectStreamActivityPhase::Idle {
            return;
        }
        if !self.is_running() || activity.turn_generation > self.generation {
            let generation = if activity.turn_generation == 0 {
                self.generation.saturating_add(1)
            } else {
                activity.turn_generation
            };
            self.begin(generation, now);
        }
        let activity = match activity.phase {
            DirectStreamActivityPhase::Idle => return,
            DirectStreamActivityPhase::Requesting => TurnActivity::Requesting,
            DirectStreamActivityPhase::Responding => TurnActivity::Responding,
            DirectStreamActivityPhase::Thinking => TurnActivity::Thinking,
            DirectStreamActivityPhase::ToolInput => TurnActivity::ToolInput,
            DirectStreamActivityPhase::ToolUse => TurnActivity::ToolUse,
        };
        self.set_activity(activity, now);
    }

    pub(crate) fn wait(&mut self, reason: WaitingReason, now: Instant) {
        self.set_activity(TurnActivity::Waiting(reason), now);
    }

    pub(crate) fn cancel(&mut self, now: Instant) {
        self.begin_if_idle(now);
        self.state = AgentState::Cancelling;
        // Cancelling is its own visible phase. Do not let the header report
        // the elapsed time of whichever activity happened to precede it.
        self.activity_started_at = Some(now);
        self.next_animation_at
            .get_or_insert(now + ANIMATION_INTERVAL);
    }

    pub(crate) fn finish(&mut self, outcome: TurnOutcome) {
        self.finish_at(outcome, Instant::now());
    }

    fn finish_at(&mut self, outcome: TurnOutcome, now: Instant) {
        if self.started_at.is_none()
            && !self.is_running()
            && matches!(outcome, TurnOutcome::Complete | TurnOutcome::Cancelled)
        {
            // Initial/duplicate idle convergence is not a completed user
            // turn. Recording it would make a fresh welcome claim that a
            // prior turn finished when no turn clock ever opened.
            return;
        }
        self.last_elapsed = self
            .started_at
            .map(|started| now.saturating_duration_since(started));
        self.last_outcome = Some(outcome);
        self.state = AgentState::Idle;
        self.activity = None;
        self.started_at = None;
        self.activity_started_at = None;
        self.next_animation_at = (self.watchers.total() > 0).then(|| now + ANIMATION_INTERVAL);
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        self.next_animation_at
    }

    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.next_animation_at else {
            return false;
        };
        if deadline > now || !self.needs_animation() {
            return false;
        }
        self.animation_tick = self.animation_tick.wrapping_add(1);
        self.next_animation_at = Some(now + ANIMATION_INTERVAL);
        true
    }

    #[cfg(test)]
    pub(crate) fn spinner(&self) -> Option<&'static str> {
        self.is_running().then(|| {
            let frame = (self.animation_tick / SPINNER_DIVISOR) as usize % SPINNER_FRAMES.len();
            SPINNER_FRAMES[frame]
        })
    }

    #[cfg(test)]
    pub(crate) fn watcher_icon(&self) -> Option<&'static str> {
        if self.is_running() || self.watchers.total() == 0 {
            return None;
        }
        let frames = crabcode_pager_render::audited_glyphs::monitor_icon_frames();
        let frame = (self.animation_tick / WATCHER_PULSE_DIVISOR) as usize % frames.len().max(1);
        frames.get(frame).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_status_ticks_only_while_running_and_parks_on_every_terminal_outcome() {
        let start = Instant::now();
        let mut status = TurnStatus::default();
        status.begin(7, start);
        status.set_activity(TurnActivity::Thinking, start);
        let deadline = status.animation_deadline().expect("running deadline");
        assert!(!status.tick(deadline - Duration::from_nanos(1)));
        assert!(status.tick(deadline));
        assert_eq!(status.spinner(), Some("⠋"));

        for outcome in [
            TurnOutcome::Complete,
            TurnOutcome::Failed,
            TurnOutcome::Cancelled,
            TurnOutcome::RuntimeStopped,
        ] {
            status.finish(outcome);
            assert_eq!(status.state(), AgentState::Idle);
            assert_eq!(status.animation_deadline(), None);
            assert_eq!(status.spinner(), None);
            status.begin(8, start);
        }
    }

    #[test]
    fn direct_phase_changes_reset_only_the_activity_timer() {
        let start = Instant::now();
        let later = start + Duration::from_secs(1);
        let mut status = TurnStatus::default();
        status.observe_direct_stream(
            &DirectStreamActivityState {
                phase: DirectStreamActivityPhase::Requesting,
                turn_generation: 4,
                ..DirectStreamActivityState::default()
            },
            start,
        );
        assert_eq!(status.generation(), 4);
        assert_eq!(status.activity(), Some(TurnActivity::Requesting));
        status.observe_direct_stream(
            &DirectStreamActivityState {
                phase: DirectStreamActivityPhase::Thinking,
                turn_generation: 4,
                ..DirectStreamActivityState::default()
            },
            later,
        );
        assert_eq!(status.activity(), Some(TurnActivity::Thinking));
        assert_eq!(status.activity_started_at(), Some(later));
        assert_eq!(status.elapsed(later), Some(Duration::from_secs(1)));
    }

    #[test]
    fn foreground_finish_preserves_idle_watcher_animation_until_exact_terminal() {
        let start = Instant::now();
        let mut status = TurnStatus::default();
        status.begin(1, start);
        status.set_watchers(
            Watchers {
                subagents: 1,
                ..Watchers::default()
            },
            start,
        );
        status.finish(TurnOutcome::Complete);

        assert_eq!(status.state(), AgentState::Idle);
        assert!(status.spinner().is_none());
        assert!(status.watcher_icon().is_some());
        assert!(status.animation_deadline().is_some());

        status.set_watchers(Watchers::default(), start + Duration::from_secs(1));
        assert!(status.watcher_icon().is_none());
        assert_eq!(status.animation_deadline(), None);
    }

    #[test]
    fn terminal_outcome_and_elapsed_are_frozen_until_the_next_turn() {
        let start = Instant::now();
        let finish = start + Duration::from_secs(48);
        let mut status = TurnStatus::default();
        status.begin(1, start);
        status.set_activity(TurnActivity::Responding, start + Duration::from_secs(2));
        status.finish_at(TurnOutcome::Failed, finish);

        assert_eq!(status.last_outcome(), Some(TurnOutcome::Failed));
        assert_eq!(status.last_elapsed(), Some(Duration::from_secs(48)));
        assert_eq!(status.elapsed(finish + Duration::from_secs(30)), None);

        status.begin(2, finish + Duration::from_secs(30));
        assert_eq!(status.last_outcome(), None);
        assert_eq!(status.last_elapsed(), None);
    }

    #[test]
    fn idle_convergence_does_not_invent_a_previous_turn() {
        let mut status = TurnStatus::default();
        status.finish_at(TurnOutcome::Complete, Instant::now());
        assert_eq!(status.last_outcome(), None);
        assert_eq!(status.last_elapsed(), None);
    }

    #[test]
    fn cancelling_starts_a_distinct_phase_clock() {
        let start = Instant::now();
        let cancelling = start + Duration::from_secs(9);
        let mut status = TurnStatus::default();
        status.begin(1, start);
        status.set_activity(TurnActivity::Thinking, start + Duration::from_secs(1));
        status.cancel(cancelling);

        assert_eq!(status.state(), AgentState::Cancelling);
        assert_eq!(status.activity_started_at(), Some(cancelling));
        assert_eq!(status.elapsed(cancelling), Some(Duration::from_secs(9)));
    }
}
