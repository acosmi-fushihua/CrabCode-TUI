//! Fixed-lifecycle transcript view for CrabCode's direct TUI.
//!
//! The structure and render order are adapted from the pinned upstream Rust
//! TUI source
//! (`a5727c5960452e7527a154b25cb5bf00cda0545e`):
//! synchronize entries, prepare one [`ScrollbackState`], then paint through
//! [`ScrollbackPane::render_with_scratch`]. CrabCode's only product adapter is
//! the already-existing read-only [`ProjectedItem`](crate::sdk_projection::ProjectedItem)
//! projection. This module owns no server transport, graphical surface, or
//! backend DTO.
//!
//! Fixed-source anchors:
//! - repository commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - monorepo source revision: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//! - the fixed `AgentView` module (persistent view-model ownership)
//! - the fixed `AgentView::draw` implementation (set cwd → prepare_layout →
//!   `ScrollbackPane::render_with_scratch_and_selection_boundaries` →
//!   selection/scrollbar/link/media post-passes)
//! - the fixed `AgentView::paint_diagram_affordances` implementation
//!
//! Backend/session tracking, ACP routing, subagent dashboards, permissions,
//! voice, cloud, and model/account state are intentionally not recreated here:
//! they remain in CrabCode's existing direct backend/TuiApp boundary. This
//! adapted file closes only the renderer-owned transcript lifecycle and must
//! not be counted as a full line-for-line port of the upstream AgentView.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crabcode_pager_render::appearance::RendererLanguage;
use crabcode_pager_render::render::Renderable as _;
use crabcode_pager_render::render::SafeBuf as _;
use crabcode_pager_render::render::osc8::{
    LinkOverlay, LinkTarget as PagerLinkTarget, resolve_link_target_with_presentation,
};
use crabcode_pager_render::scrollback::blocks::mermaid_content::{
    AffordanceKind, affordance_row_for_language,
};
use crabcode_pager_render::scrollback::link_map::{VisibleLink, VisibleLinkMap};
use crabcode_pager_render::scrollback::minimal;
use crabcode_pager_render::scrollback::table_geometry::{CellRef, TableGeometry};
use crabcode_pager_render::scrollback::text_selection::{
    ActiveBlockDrag, ActiveTextDrag, DragAutoScrollState, PendingBlockDrag, PendingTextDrag,
    PersistentTextSelection, RangeHit, ResolvedSelectionBoundaries, ResolvedSelectionModel,
    SelectionEndpoint, SelectionKind, SelectionOrigin, TableSelectionGeometry,
    block_drag_threshold_exceeded, compute_autoscroll, configured_word_separators,
    drag_threshold_exceeded, reconstruct_selection_text, reconstruct_table_selection_text,
    render_active_selection_overlay, render_block_drag_overlay,
    render_persistent_selection_overlay, resolve_table_drag_kind, url_range_at_col,
    word_boundaries_at_col,
};
use crabcode_pager_render::scrollback::{
    DisplayMode, EntryId, RenderBlock, RenderOutput, ScratchBuffer, ScrollbackPane, ScrollbackState,
};
#[cfg(test)]
use crabcode_pager_render::side_question_panel::render_side_question_panel;
use crabcode_pager_render::side_question_panel::{
    SIDE_QUESTION_PANEL_ENTRY_IDX, SideQuestionPanelState, render_side_question_panel_for_language,
    side_question_panel_height,
};
use crabcode_pager_render::theme::Theme;
use crabcode_ratatui_inline::{LinkSpan, Terminal};
use ratatui::Frame;
use ratatui::backend::Backend;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthStr as _;

use crate::scrollback_projection::{
    ProjectionScrollbackAdapter, ProjectionScrollbackDelta, ProjectionScrollbackError,
    RendererNoticePlacement, RendererNoticeProjection, SynchronizationOptions,
    direct_api_error_retry_attempt,
};
use crate::sdk_projection::{DirectProgressPresentation, ProjectedItem, ProjectedKind};
use crate::task_panel::TaskPanelSnapshot;
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::tui_app::{TuiApp, TuiRendererNoticePlacement, UiLanguage, projected_item_is_visible};
use crate::tui_links::{LinkTarget, MermaidAffordanceAction};
use crate::tui_render::CrabCodeTheme;
use crate::tui_ui::{TranscriptRenderOutcome, empty_transcript_welcome_lines};

/// Fixed-source multi-click window for word/line selection.
const MULTI_CLICK_TIMEOUT: Duration = Duration::from_millis(300);
/// Fixed-source default flash lifetime when no product setting supplies a
/// hold mode.
const TEXT_SELECTION_FLASH_DURATION: Duration = Duration::from_millis(150);
/// Drag autoscroll participates in the fixed fast renderer cadence.
const TEXT_SELECTION_FAST_TICK: Duration = Duration::from_millis(16);

const fn renderer_language(language: UiLanguage) -> RendererLanguage {
    match language {
        UiLanguage::ZhCn => RendererLanguage::ZhCn,
        UiLanguage::EnUs => RendererLanguage::EnUs,
    }
}

fn selected_visible_link_status(language: UiLanguage, target: &LinkTarget) -> String {
    match language {
        UiLanguage::ZhCn => format!("已选择可见链接：{target}"),
        UiLanguage::EnUs => format!("Selected visible link: {target}"),
    }
}

fn copied_blocks_status(language: UiLanguage, count: usize) -> String {
    match language {
        UiLanguage::ZhCn => format!("已复制 {count} 个选中块"),
        UiLanguage::EnUs => format!("Copied {count} selected blocks"),
    }
}

fn block_copy_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::ZhCn => format!("复制所选块失败：{error}"),
        UiLanguage::EnUs => format!("Failed to copy block selection: {error}"),
    }
}

fn copied_characters_status(language: UiLanguage, count: usize) -> String {
    match language {
        UiLanguage::ZhCn => format!("已复制 {count} 个选中字符"),
        UiLanguage::EnUs => format!("Copied {count} selected characters"),
    }
}

fn text_copy_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::ZhCn => format!("复制文本选择失败：{error}"),
        UiLanguage::EnUs => format!("Failed to copy text selection: {error}"),
    }
}

#[derive(Debug, Clone, Copy)]
struct TextClickState {
    at: Instant,
    hit: RangeHit,
    count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApiRetryAnimationClock {
    mounted_at: Instant,
    stop_at: Instant,
    next_tick_at: Instant,
}

/// Renderer-private semantic actions that must cross the temporary TuiApp
/// boundary during the atomic owner cutover.
///
/// These are not backend messages and must never be serialized. They mirror
/// the fixed `AgentView` scrollback actions so the sole [`ScrollbackState`]
/// remains authoritative after production wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptViewAction {
    FocusChanged {
        scrollback: bool,
    },
    PointerMoved {
        pointer: Option<(u16, u16)>,
        arrived_at: Instant,
    },
    UpdateLinkHover {
        modifier_held: bool,
    },
    PointerDown {
        point: (u16, u16),
        link_modifier_held: bool,
    },
    PointerUp((u16, u16)),
    PointerDrag {
        point: (u16, u16),
    },
    CancelPointerGesture,
    CopyTextSelection,
    ClearTextSelection,
    ScrollUp(u16),
    ScrollDown(u16),
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Top,
    Bottom,
    SelectNext,
    SelectPrevious,
    CollapseSelected,
    ExpandSelected,
    ToggleFoldSelected,
    ToggleRawSelected,
    ToggleExpandAll,
    ToggleThinking,
    /// Minimal-mode Ctrl-E: re-print the latest immutable folded commit.
    MinimalExpandLast,
    SetThinkingExpanded(bool),
    NextTurn,
    PreviousTurn,
    NextResponse,
    PreviousResponse,
    RevealMatch {
        key: String,
        line_in_entry: usize,
    },
    CycleHighlightedLink {
        forward: bool,
    },
    ClearHighlightedLink {
        announce: bool,
    },
    OpenHighlightedLinkOrBlockViewer,
    StartSideQuestion {
        question: String,
    },
    FinishSideQuestion {
        question: String,
        result: Result<String, String>,
    },
    DismissSideQuestion,
    ScrollSideQuestion {
        rows: i32,
    },
    SideQuestionCloseHover {
        hovered: bool,
    },
}

impl TranscriptViewAction {
    /// Pointer/modifier actions are observations from one terminal ownership
    /// generation. They must not be replayed after suspend/resume even when a
    /// writer ACK delayed the presentation that would normally consume them.
    pub(crate) fn is_terminal_generation_scoped_pointer_action(&self) -> bool {
        matches!(
            self,
            Self::PointerMoved { .. }
                | Self::UpdateLinkHover { .. }
                | Self::PointerDown { .. }
                | Self::PointerUp(_)
                | Self::PointerDrag { .. }
                | Self::CancelPointerGesture
        )
    }
}

/// The single transcript view-model.
///
/// `projection` and `scrollback` are deliberately adjacent and private:
/// `ProjectionScrollbackAdapter` validates that no other owner inserted,
/// deleted, or reordered entries in `scrollback`.
#[derive(Debug)]
pub(crate) struct AgentView {
    projection: ProjectionScrollbackAdapter,
    scrollback: ScrollbackState,
    pending_actions: VecDeque<TranscriptViewAction>,
    /// Folded entries queued by minimal-mode Ctrl-E for an honest expanded
    /// re-print below immutable native scrollback.
    pending_minimal_expand: VecDeque<EntryId>,
    scrollback_focused: bool,
    pointer: Option<(u16, u16)>,
    last_mouse_moved_at: Option<Instant>,
    next_link_modifier_poll_at: Option<Instant>,
    side_question: Option<SideQuestionPanelState>,
    side_question_focused: bool,
    side_question_area: Rect,
    side_question_close_rect: Rect,
    side_question_close_hovered: bool,
    side_question_max_scroll_offset: usize,
    side_question_animation_tick: u64,
    next_side_question_animation_at: Option<Instant>,
    next_scrollback_animation_at: Option<Instant>,
    api_retry_animation_clock: Option<ApiRetryAnimationClock>,
    media_link_paths: Vec<PathBuf>,
    media_link_paths_gen: Option<u64>,
    pending_pointer_selection: Option<(u16, u16)>,
    pending_text_click: Option<(Instant, RangeHit)>,
    pending_text_drag: Option<PendingTextDrag>,
    active_text_drag: Option<ActiveTextDrag>,
    pending_block_drag: Option<PendingBlockDrag>,
    active_block_drag: Option<ActiveBlockDrag>,
    deferred_text_press: Option<(u16, u16)>,
    persistent_text_selection: Option<PersistentTextSelection>,
    table_selection_geometry: Option<TableSelectionGeometry>,
    selection_created_at: Option<Instant>,
    last_text_click: Option<TextClickState>,
    drag_autoscroll: Option<DragAutoScrollState>,
    next_selection_tick_at: Option<Instant>,
    last_drag_mouse: Option<(u16, u16)>,
    last_scrollback_selection_model: ResolvedSelectionModel,
    last_scrollback_selection_boundaries: ResolvedSelectionBoundaries,
    last_side_question_selection_model: ResolvedSelectionModel,
    visible_link_map: VisibleLinkMap,
    scrollback_visible_link_count: usize,
    highlighted_link_idx: Option<usize>,
    hovered_link_idx: Option<usize>,
    pending_link_click: Option<(u16, u16, PagerLinkTarget)>,
    left_mouse_down: bool,
    visible_links_invalidated: bool,
    last_delta: ProjectionScrollbackDelta,
}

/// Find renderer rows that are safely superseded by the task card.
///
/// A task call remains visible until its latest result is both structurally
/// accepted and applied to the current card snapshot. A later failed result
/// therefore restores the whole lifecycle in ordinary presentation, while
/// verbose mode bypasses compaction entirely. The returned stable keys are
/// renderer-local; the authoritative projection remains untouched.
fn confirmed_task_history_compaction_keys(
    items: &[ProjectedItem],
    snapshot: Option<&TaskPanelSnapshot>,
    degraded: bool,
    presentation_verbose: bool,
) -> HashSet<String> {
    if presentation_verbose || degraded {
        return HashSet::new();
    }
    let Some(snapshot) = snapshot else {
        return HashSet::new();
    };

    let task_tool_use_ids = items
        .iter()
        .filter_map(|item| {
            (item.kind == ProjectedKind::ToolUse
                && item
                    .presentation
                    .tool
                    .as_ref()
                    .and_then(|tool| tool.name.as_deref())
                    .is_some_and(|name| matches!(name, "TaskCreate" | "TaskUpdate")))
            .then(|| item.tool_use_id.as_deref())
            .flatten()
        })
        .collect::<HashSet<_>>();

    // Results can be replayed with the same tool_use_id. Only the most recent
    // terminal result may authorize compaction; an earlier success must never
    // hide a later failure or compatibility fallback.
    let mut latest_results = HashMap::<&str, (u64, usize)>::new();
    for (index, item) in items.iter().enumerate() {
        if item.kind != ProjectedKind::ToolResult {
            continue;
        }
        let Some(tool_use_id) = item.tool_use_id.as_deref() else {
            continue;
        };
        if !task_tool_use_ids.contains(tool_use_id) {
            continue;
        }
        let sequence = item.raw_sequences.iter().copied().max().unwrap_or_default();
        let replace = match latest_results.get(tool_use_id) {
            Some((previous_sequence, previous_index)) => {
                sequence > *previous_sequence
                    || (sequence == *previous_sequence && index > *previous_index)
            }
            None => true,
        };
        if replace {
            latest_results.insert(tool_use_id, (sequence, index));
        }
    }

    let confirmed_tool_use_ids = latest_results
        .iter()
        .filter_map(|(tool_use_id, (_, index))| {
            snapshot
                .mutation_result_succeeded(&items[*index].key)
                .then_some(*tool_use_id)
        })
        .collect::<HashSet<_>>();
    if confirmed_tool_use_ids.is_empty() {
        return HashSet::new();
    }

    items
        .iter()
        .filter_map(|item| {
            let tool_use_id = item.tool_use_id.as_deref()?;
            if !confirmed_tool_use_ids.contains(tool_use_id) {
                return None;
            }
            match item.kind {
                ProjectedKind::ToolUse
                    if item
                        .presentation
                        .tool
                        .as_ref()
                        .and_then(|tool| tool.name.as_deref())
                        .is_some_and(|name| matches!(name, "TaskCreate" | "TaskUpdate")) =>
                {
                    Some(item.key.clone())
                }
                ProjectedKind::ToolResult if snapshot.mutation_result_succeeded(&item.key) => {
                    Some(item.key.clone())
                }
                ProjectedKind::User
                | ProjectedKind::Assistant
                | ProjectedKind::Thinking
                | ProjectedKind::ToolUse
                | ProjectedKind::ToolResult
                | ProjectedKind::TerminalOutput
                | ProjectedKind::System
                | ProjectedKind::Progress
                | ProjectedKind::Warning
                | ProjectedKind::Error => None,
            }
        })
        .collect()
}

impl Default for AgentView {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentView {
    pub(crate) fn new() -> Self {
        Self {
            projection: ProjectionScrollbackAdapter::default(),
            scrollback: ScrollbackState::new(),
            pending_actions: VecDeque::new(),
            pending_minimal_expand: VecDeque::new(),
            // CrabCode starts with its composer focused.
            scrollback_focused: false,
            pointer: None,
            last_mouse_moved_at: None,
            next_link_modifier_poll_at: None,
            side_question: None,
            side_question_focused: false,
            side_question_area: Rect::default(),
            side_question_close_rect: Rect::default(),
            side_question_close_hovered: false,
            side_question_max_scroll_offset: 0,
            side_question_animation_tick: 0,
            next_side_question_animation_at: None,
            next_scrollback_animation_at: None,
            api_retry_animation_clock: None,
            media_link_paths: Vec::new(),
            media_link_paths_gen: None,
            pending_pointer_selection: None,
            pending_text_click: None,
            pending_text_drag: None,
            active_text_drag: None,
            pending_block_drag: None,
            active_block_drag: None,
            deferred_text_press: None,
            persistent_text_selection: None,
            table_selection_geometry: None,
            selection_created_at: None,
            last_text_click: None,
            drag_autoscroll: None,
            next_selection_tick_at: None,
            last_drag_mouse: None,
            last_scrollback_selection_model: ResolvedSelectionModel::default(),
            last_scrollback_selection_boundaries: ResolvedSelectionBoundaries::default(),
            last_side_question_selection_model: ResolvedSelectionModel::default(),
            visible_link_map: VisibleLinkMap::default(),
            scrollback_visible_link_count: 0,
            highlighted_link_idx: None,
            hovered_link_idx: None,
            pending_link_click: None,
            left_mouse_down: false,
            visible_links_invalidated: false,
            last_delta: ProjectionScrollbackDelta::default(),
        }
    }

    pub(crate) fn enqueue(&mut self, action: TranscriptViewAction) {
        self.pending_actions.push_back(action);
    }

    #[cfg(test)]
    pub(crate) fn scrollback(&self) -> &ScrollbackState {
        &self.scrollback
    }

    #[cfg(test)]
    pub(crate) fn scrollback_focused_for_test(&self) -> bool {
        self.scrollback_focused
    }

    #[cfg(test)]
    pub(crate) fn last_delta(&self) -> ProjectionScrollbackDelta {
        self.last_delta
    }

    /// Advance product projection and semantic input through the native
    /// scrollback lifecycle before any terminal side effect.
    ///
    /// Conversion is atomic: an unsupported projected variant returns before
    /// the prior frame's state is mutated.
    pub(crate) fn prepare(&mut self, app: &mut TuiApp) -> Result<(), ProjectionScrollbackError> {
        let presentation_verbose = app.presentation_verbose();
        let agent_transcript_mode = app.direct_agent_transcript_mode();
        let session_id = app.projection.session_id().map(str::to_string);
        let task_panel_state = app.task_panel_projection_state();
        let projected_items = app.projection.items();
        let compacted_task_history_keys = confirmed_task_history_compaction_keys(
            projected_items,
            task_panel_state.snapshot.as_deref(),
            task_panel_state.degraded,
            presentation_verbose,
        );
        let mut lifecycle_delta = if compacted_task_history_keys.is_empty() {
            ProjectionScrollbackDelta::default()
        } else {
            self.projection
                .retire_rendered_keys(&mut self.scrollback, &compacted_task_history_keys)?
        };
        let visible_items = projected_items
            .iter()
            .enumerate()
            .filter(|(index, item)| {
                if direct_api_error_retry_attempt(item).is_some()
                    && index + 1 != projected_items.len()
                {
                    return false;
                }
                let nested_message = matches!(
                    item.presentation.direct_progress.as_ref(),
                    Some(DirectProgressPresentation::Nested { .. })
                );
                // A fixed nested MessageComponent applies `verbose` only
                // after the outer Agent/Skill envelope has participated in
                // last-three grouping. Retain every typed nested item here;
                // the adapter performs that inner visibility decision.
                (nested_message || projected_item_is_visible(item, presentation_verbose))
                    && !compacted_task_history_keys.contains(&item.key)
            })
            .map(|(_, item)| item)
            .cloned()
            .collect::<Vec<_>>();
        let language = app.ui_language();
        let options = SynchronizationOptions {
            presentation_verbose,
            agent_transcript_mode,
        };
        let renderer_notices = if app.setup_surface_exclusive() {
            Vec::new()
        } else {
            app.renderer_notices()
                .map(|notice| RendererNoticeProjection {
                    id: notice.id(),
                    placement: match notice.placement() {
                        TuiRendererNoticePlacement::BootPrefix => {
                            RendererNoticePlacement::BootPrefix
                        }
                        TuiRendererNoticePlacement::AfterRawSequence(sequence) => {
                            RendererNoticePlacement::AfterRawSequence(sequence)
                        }
                    },
                    text: sanitize_bounded_terminal_text(notice.text(language)).into_owned(),
                })
                .collect::<Vec<_>>()
        };
        let item_removals = app.projection.item_removals().to_vec();
        let stream_activity = app.projection.direct_stream_activity().clone();
        let latest_sequence = app
            .projection
            .raw_envelopes()
            .last()
            .map_or(0, |envelope| envelope.sequence);
        lifecycle_delta += self.projection.advance_lifecycle_with_options_and_notices(
            &mut self.scrollback,
            &visible_items,
            &renderer_notices,
            options,
            &item_removals,
            session_id.as_deref(),
            stream_activity.turn_generation,
            app.turn_status().is_running(),
            stream_activity.request_started_sequence,
            latest_sequence,
        )?;
        self.last_delta = lifecycle_delta;
        let language = renderer_language(language);
        if self.scrollback.appearance().language != language {
            let mut appearance = self.scrollback.appearance().clone();
            appearance.language = language;
            self.scrollback.set_appearance(appearance);
        }
        self.scrollback
            .set_cwd(app.projection.cwd().map(PathBuf::from));
        self.synchronize_api_retry_animation_clock();
        self.apply_pending_actions(app);
        self.snapshot_selection(app);
        self.snapshot_highlighted_link(app);
        app.transcript_following = self.scrollback.is_follow_mode();
        Ok(())
    }

    /// Read the post-commit minimal tail from the same state used by draw.
    pub(crate) fn minimal_tail_rows(&self, width: u16, turn_running: bool) -> u16 {
        let appearance = minimal::committed_appearance(self.scrollback.appearance());
        minimal::tail_height(
            &self.scrollback,
            turn_running,
            width.max(1),
            &appearance,
            self.scrollback.cwd(),
        )
    }

    pub(crate) fn minimal_side_question_rows(&self, width: u16) -> u16 {
        side_question_panel_height(self.side_question.as_ref(), width)
    }

    pub(crate) fn minimal_will_commit(&self, turn_running: bool) -> bool {
        minimal::scan_frontier(&self.scrollback, turn_running).will_commit
    }

    /// Print the leading stable frontier into native scrollback.
    ///
    /// An entry is marked committed only after `insert_before` succeeds. A
    /// failed write remains retryable in this same `ScrollbackState`.
    pub(crate) fn commit_minimal<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        turn_running: bool,
        hold_native_commits: bool,
    ) -> std::io::Result<bool> {
        if hold_native_commits {
            return Ok(false);
        }
        let width = terminal.viewport_area().width;
        if width == 0 {
            return Ok(false);
        }
        let appearance = minimal::committed_appearance(self.scrollback.appearance());
        let max_rows = appearance.minimal_max_commit_rows;
        let cwd = self.scrollback.cwd().map(PathBuf::from);
        let theme = Theme::current();
        let footer_style = theme.dim();
        let expanded = self.expand_pending_minimal(
            terminal,
            width,
            &theme,
            appearance.clone(),
            cwd.as_deref(),
            footer_style,
        )?;
        let mut terminal_error = None;
        let committed =
            minimal::commit_leading_run(&mut self.scrollback, turn_running, |state, index| {
                minimal::prepare_commit_entry(state, index);
                let Some(entry) = state.get(index) else {
                    return true;
                };
                let renderer =
                    minimal::committed_renderer(entry, &theme, appearance.clone(), cwd.as_deref());
                let full_height = renderer.desired_height(width);
                let commit_height = if max_rows > 0 && full_height > max_rows {
                    max_rows
                } else {
                    full_height
                };
                if full_height > 0
                    && let Err(error) = terminal.insert_before(commit_height, move |buffer| {
                        minimal::paint_committed(
                            buffer,
                            renderer,
                            width,
                            full_height,
                            footer_style,
                        );
                    })
                {
                    terminal_error = Some(error);
                    return false;
                }
                minimal::record_folded_commit(state, index);
                true
            });
        minimal::stamp_live_tail(&mut self.scrollback);
        match terminal_error {
            Some(error) => Err(error),
            None => Ok(expanded || committed > 0),
        }
    }

    /// Re-print minimal-mode folded commits in the exact fixed-source order.
    ///
    /// Native scrollback is immutable, so expansion is an uncapped second
    /// insertion of the same stable entry. Missing entries are discarded
    /// (rewind/clear won), while a terminal failure keeps the failed ID and
    /// every later request queued for the next frame.
    fn expand_pending_minimal<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        width: u16,
        theme: &Theme,
        appearance: crabcode_pager_render::appearance::AppearanceConfig,
        cwd: Option<&std::path::Path>,
        footer_style: Style,
    ) -> std::io::Result<bool> {
        let mut pending = std::mem::take(&mut self.pending_minimal_expand);
        let mut expanded = false;
        while let Some(id) = pending.pop_front() {
            let Some(index) = self.scrollback.index_of_id(id) else {
                continue;
            };
            if let Some(entry) = self.scrollback.get_mut(index) {
                entry.set_display_mode(DisplayMode::Expanded);
            }
            let Some(entry) = self.scrollback.get(index) else {
                continue;
            };
            let renderer = minimal::committed_renderer(entry, theme, appearance.clone(), cwd);
            let full_height = renderer.desired_height(width);
            if full_height > 0
                && let Err(error) = terminal.insert_before(full_height, move |buffer| {
                    minimal::paint_committed(buffer, renderer, width, full_height, footer_style);
                })
            {
                pending.push_front(id);
                self.pending_minimal_expand = pending;
                return Err(error);
            }
            expanded = true;
        }
        Ok(expanded)
    }

    /// Render a frame that was already synchronized by [`Self::prepare`].
    pub(crate) fn draw_prepared(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        app: &mut TuiApp,
        theme: CrabCodeTheme,
        scratch: &mut ScratchBuffer,
    ) -> TranscriptRenderOutcome {
        self.scrollback.begin_frame();
        self.ensure_media_link_paths();
        if app.minimal_mode() {
            self.last_scrollback_selection_model = ResolvedSelectionModel::default();
            self.last_scrollback_selection_boundaries = ResolvedSelectionBoundaries::default();
            let (tail_area, panel_area) = self.side_question_layout(area, 0);
            let appearance = minimal::committed_appearance(self.scrollback.appearance());
            let turn_running = app.busy();
            let tail_rows = self.minimal_tail_rows(tail_area.width.max(1), turn_running);
            minimal::draw_tail(
                frame.buffer_mut(),
                tail_area,
                &self.scrollback,
                turn_running,
                &Theme::current(),
                &appearance,
                self.scrollback.cwd(),
                self.scrollback.animation_tick(),
            );
            let panel_links = self.paint_side_question_panel(frame, panel_area, app);
            let hyperlinks = self.synchronize_links(frame, app, theme, None, &panel_links);
            self.snapshot_highlighted_link(app);
            app.mermaid_hitboxes.clear();
            app.transcript_content_width = tail_area.width;
            app.transcript_line_count = usize::from(tail_rows);
            app.transcript_viewport_height = usize::from(tail_area.height);
            return TranscriptRenderOutcome {
                cursor: None,
                hyperlinks,
                selected_artifact_preview: None,
            };
        }

        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(theme.prompt_border))
            .style(Style::default().bg(theme.bg_base));
        let inner = block.inner(area).inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        frame.render_widget(block, area);

        let search_reserved_rows = if app.transcript_search.is_some() {
            inner.height.min(2)
        } else {
            0
        };
        let (mut content, panel_area) = self.side_question_layout(inner, search_reserved_rows);
        app.transcript_content_width = content.width;

        if self.scrollback.is_empty() {
            self.pending_pointer_selection = None;
            self.last_scrollback_selection_model = ResolvedSelectionModel::default();
            self.last_scrollback_selection_boundaries = ResolvedSelectionBoundaries::default();
            self.render_empty(frame, content, app, theme);
            let panel_links = self.paint_side_question_panel(frame, panel_area, app);
            let hyperlinks = self.synchronize_links(frame, app, theme, None, &panel_links);
            self.snapshot_highlighted_link(app);
            app.mermaid_hitboxes.clear();
            app.transcript_line_count = 0;
            app.transcript_viewport_height = usize::from(content.height);
            let cursor = crate::tui_ui::render_transcript_search_bar(
                frame,
                inner,
                search_reserved_rows,
                app,
                theme,
            );
            return TranscriptRenderOutcome {
                cursor,
                hyperlinks,
                selected_artifact_preview: None,
            };
        }

        self.prepare_layout_with_scrollbar_gutter(&mut content);
        if let Some(point) = self.pending_pointer_selection.take()
            && content.contains(point.into())
        {
            let selected = self.scrollback.entry_index_at_screen_row(point.1, content);
            self.scrollback.set_selected(selected);
            self.snapshot_selection(app);
        }
        app.transcript_content_width = content.width;
        let hovered_entry = self.pointer.and_then(|(x, y)| {
            content
                .contains((x, y).into())
                .then(|| self.scrollback.entry_index_at_screen_row(y, content))
                .flatten()
        });
        let search_highlight = app
            .transcript_search
            .as_ref()
            .and_then(crate::transcript_search::TranscriptSearchState::highlight_regex);
        let mut pane = ScrollbackPane::new()
            .active(self.scrollback_focused)
            .with_hovered_entry(hovered_entry)
            .with_search_highlight(search_highlight)
            .with_media_paths(self.media_link_paths.clone());
        if let Some(pointer) = self.pointer {
            pane = pane.with_mouse_pos(pointer);
        }
        let rendered = pane.render_with_scratch_and_selection_boundaries(
            content,
            frame.buffer_mut(),
            &self.scrollback,
            scratch,
        );
        let output = rendered.output;
        self.last_scrollback_selection_model = output.selection_model.clone();
        self.last_scrollback_selection_boundaries = rendered.selection_boundaries;
        self.reclamp_active_drag(false);
        self.paint_text_selection_overlay(frame.buffer_mut(), false);

        self.paint_post_scrollback(frame, area, content, app, theme, &output);
        let panel_links = self.paint_side_question_panel(frame, panel_area, app);
        let mut hyperlinks = self.synchronize_links(frame, app, theme, Some(&output), &panel_links);
        self.paint_diagram_affordances(frame, app, theme, output.diagram_affordances);
        drop_link_spans_intersecting(
            &mut hyperlinks,
            app.mermaid_hitboxes.iter().map(|(area, _, _)| *area),
        );

        let (_, viewport_height, total_height) = self.scrollback.scroll_info();
        app.transcript_line_count = total_height;
        app.transcript_viewport_height = usize::from(viewport_height);
        let cursor = crate::tui_ui::render_transcript_search_bar(
            frame,
            inner,
            search_reserved_rows,
            app,
            theme,
        );
        let selected_artifact_preview = self
            .highlighted_link_target()
            .map(product_link_target)
            .and_then(|target| app.selected_artifact_preview_for_target(&target));
        TranscriptRenderOutcome {
            cursor,
            hyperlinks,
            selected_artifact_preview,
        }
    }

    fn side_question_layout(&self, area: Rect, reserved_bottom_rows: u16) -> (Rect, Rect) {
        let mut content = area;
        content.height = content.height.saturating_sub(reserved_bottom_rows);
        let desired = side_question_panel_height(self.side_question.as_ref(), area.width);
        if desired == 0 || area.width < 12 || content.height < 3 {
            return (content, Rect::default());
        }
        let panel_height = desired.min(content.height);
        let panel = Rect::new(
            content.x,
            content.bottom().saturating_sub(panel_height),
            content.width,
            panel_height,
        );
        content.height = content.height.saturating_sub(panel_height);
        (content, panel)
    }

    fn paint_side_question_panel(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        app: &mut TuiApp,
    ) -> LinkOverlay {
        self.side_question_area = Rect::default();
        self.side_question_close_rect = Rect::default();
        self.side_question_max_scroll_offset = 0;
        let active = self.side_question.is_some();
        let loading = self
            .side_question
            .as_ref()
            .is_some_and(SideQuestionPanelState::is_loading);
        let rendered = self.side_question.as_ref().and_then(|state| {
            render_side_question_panel_for_language(
                frame.buffer_mut(),
                state,
                area,
                self.side_question_animation_tick,
                self.side_question_focused,
                self.side_question_close_hovered,
                &self.media_link_paths,
                renderer_language(app.ui_language()),
            )
        });
        let links = if let Some(rendered) = rendered {
            self.side_question_area = area;
            self.side_question_close_rect = rendered.close_rect;
            self.side_question_max_scroll_offset = rendered.max_scroll_offset;
            self.last_side_question_selection_model = rendered.selection_model;
            self.reclamp_active_drag(true);
            self.paint_text_selection_overlay(frame.buffer_mut(), true);
            rendered.links
        } else {
            self.side_question_close_hovered = false;
            self.last_side_question_selection_model = ResolvedSelectionModel::default();
            LinkOverlay::new()
        };
        app.set_renderer_side_question_snapshot(
            active,
            loading,
            self.side_question_focused,
            self.side_question_area,
            self.side_question_close_rect,
        );
        links
    }

    /// Refresh the fixed renderer's relative-media-link allowlist only when
    /// scrollback geometry/content changes. Every path originates in a typed
    /// media reference already owned by the sole transcript state.
    fn ensure_media_link_paths(&mut self) {
        let generation = self.scrollback.generation();
        if self.media_link_paths_gen == Some(generation) {
            return;
        }
        self.media_link_paths_gen = Some(generation);
        self.media_link_paths.clear();
        self.media_link_paths
            .extend(self.scrollback.generated_media_paths());
    }

    fn prepare_layout_with_scrollbar_gutter(&mut self, content: &mut Rect) {
        self.scrollback
            .prepare_layout(content.width.max(1), content.height);
        let (_, viewport_height, total_height) = self.scrollback.scroll_info();
        if total_height > usize::from(viewport_height) && content.width > 1 {
            // Fixed AgentView keeps a distinct scrollbar column. CrabCode's
            // bordered transcript puts that column in the outer right edge,
            // so reserve one content gutter and reflow before paint.
            content.width = content.width.saturating_sub(1);
            self.scrollback
                .prepare_layout(content.width.max(1), content.height);
        }
    }

    fn apply_pending_actions(&mut self, app: &mut TuiApp) {
        while let Some(action) = self.pending_actions.pop_front() {
            match action {
                TranscriptViewAction::FocusChanged { scrollback } => {
                    self.scrollback_focused = scrollback;
                    if scrollback {
                        self.scrollback.on_activate();
                    } else {
                        self.clear_text_selection();
                        self.highlighted_link_idx = None;
                    }
                }
                TranscriptViewAction::PointerMoved {
                    pointer,
                    arrived_at,
                } => {
                    self.observe_pointer(pointer, arrived_at);
                }
                TranscriptViewAction::UpdateLinkHover { modifier_held } => {
                    self.update_hovered_link(modifier_held, link_interaction_policy());
                }
                TranscriptViewAction::PointerDown {
                    point,
                    link_modifier_held,
                } => {
                    self.handle_pointer_down(point, link_modifier_held, link_interaction_policy());
                }
                TranscriptViewAction::PointerUp(point) => {
                    if let Some(target) = self.finish_pointer_up(point, app) {
                        app.open_renderer_link_target(product_link_target(&target));
                    }
                }
                TranscriptViewAction::PointerDrag { point } => {
                    self.pointer = Some(point);
                    self.handle_pointer_drag(Some(point));
                }
                TranscriptViewAction::CancelPointerGesture => {
                    self.cancel_pointer_gesture();
                }
                TranscriptViewAction::CopyTextSelection => {
                    self.copy_persistent_text_selection(app);
                }
                TranscriptViewAction::ClearTextSelection => {
                    self.clear_text_selection();
                }
                TranscriptViewAction::ScrollUp(rows) => {
                    self.clear_text_selection();
                    self.scrollback.scroll_up(rows);
                }
                TranscriptViewAction::ScrollDown(rows) => {
                    self.clear_text_selection();
                    self.scrollback.scroll_down(rows);
                }
                TranscriptViewAction::PageUp => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.page_up();
                }
                TranscriptViewAction::PageDown => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.page_down();
                }
                TranscriptViewAction::HalfPageUp => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.half_page_up();
                }
                TranscriptViewAction::HalfPageDown => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.half_page_down();
                }
                TranscriptViewAction::Top => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.goto_top();
                }
                TranscriptViewAction::Bottom => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.goto_bottom();
                }
                TranscriptViewAction::SelectNext => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.select_next();
                }
                TranscriptViewAction::SelectPrevious => {
                    self.clear_text_selection();
                    self.highlighted_link_idx = None;
                    self.scrollback.select_prev();
                }
                TranscriptViewAction::CollapseSelected => {
                    self.clear_text_selection();
                    self.scrollback.collapse_selected();
                }
                TranscriptViewAction::ExpandSelected => {
                    self.clear_text_selection();
                    self.scrollback.expand_selected();
                }
                TranscriptViewAction::ToggleFoldSelected => {
                    self.clear_text_selection();
                    self.scrollback.toggle_fold_selected();
                }
                TranscriptViewAction::ToggleRawSelected => {
                    self.clear_text_selection();
                    self.scrollback.toggle_raw_selected();
                }
                TranscriptViewAction::ToggleExpandAll => {
                    self.clear_text_selection();
                    self.scrollback.toggle_expand_all();
                }
                TranscriptViewAction::ToggleThinking => {
                    self.clear_text_selection();
                    self.scrollback.expand_all_thinking();
                }
                TranscriptViewAction::MinimalExpandLast => {
                    if let Some(id) = self.scrollback.take_expandable_committed() {
                        self.pending_minimal_expand.push_back(id);
                    }
                }
                TranscriptViewAction::SetThinkingExpanded(expanded) => {
                    self.clear_text_selection();
                    self.scrollback.set_thinking_expanded(expanded);
                }
                TranscriptViewAction::NextTurn => {
                    self.clear_text_selection();
                    self.scrollback.next_turn();
                }
                TranscriptViewAction::PreviousTurn => {
                    self.clear_text_selection();
                    self.scrollback.prev_turn();
                }
                TranscriptViewAction::NextResponse => {
                    self.clear_text_selection();
                    self.scrollback.next_response();
                }
                TranscriptViewAction::PreviousResponse => {
                    self.clear_text_selection();
                    self.scrollback.prev_response();
                }
                TranscriptViewAction::RevealMatch { key, line_in_entry } => {
                    if let Some(index) = self
                        .projection
                        .entry_id(&key)
                        .and_then(|id| self.scrollback.index_of_id(id))
                    {
                        self.scrollback.reveal_entry_line(index, line_in_entry);
                    }
                }
                TranscriptViewAction::CycleHighlightedLink { forward } => {
                    self.cycle_highlighted_link(forward);
                    let language = app.ui_language();
                    app.status = self.highlighted_link_target().map_or_else(
                        || {
                            language
                                .text(
                                    "没有可用的可见对话链接",
                                    "No visible transcript links available",
                                )
                                .to_string()
                        },
                        |target| {
                            selected_visible_link_status(language, &product_link_target(target))
                        },
                    );
                }
                TranscriptViewAction::ClearHighlightedLink { announce } => {
                    let changed = self.highlighted_link_idx.take().is_some();
                    if announce && changed {
                        app.status = app
                            .ui_language()
                            .text("已清除选中的对话链接", "Cleared selected transcript link")
                            .to_string();
                    }
                }
                TranscriptViewAction::OpenHighlightedLinkOrBlockViewer => {
                    if let Some(target) = self.take_highlighted_link_target() {
                        app.open_renderer_link_target(product_link_target(&target));
                    } else {
                        app.open_selected_block_viewer_without_link();
                    }
                }
                TranscriptViewAction::StartSideQuestion { question } => {
                    self.side_question = Some(SideQuestionPanelState::Loading { question });
                    self.side_question_focused = false;
                    self.side_question_animation_tick = 0;
                    self.next_side_question_animation_at = None;
                    self.visible_links_invalidated = true;
                }
                TranscriptViewAction::FinishSideQuestion { question, result } => {
                    let owns_response = self
                        .side_question
                        .as_ref()
                        .is_some_and(|state| state.question() == question);
                    if !owns_response {
                        continue;
                    }
                    self.side_question = Some(match result {
                        Ok(response) => SideQuestionPanelState::done(question, response),
                        Err(error) => SideQuestionPanelState::Error { question, error },
                    });
                    self.side_question_focused = matches!(
                        self.side_question,
                        Some(SideQuestionPanelState::Done { .. })
                    );
                    self.next_side_question_animation_at = None;
                    self.visible_links_invalidated = true;
                }
                TranscriptViewAction::DismissSideQuestion => {
                    if self.persistent_text_selection.is_some_and(|selection| {
                        selection.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX
                    }) {
                        self.clear_text_selection();
                    }
                    self.side_question = None;
                    self.side_question_focused = false;
                    self.side_question_area = Rect::default();
                    self.side_question_close_rect = Rect::default();
                    self.side_question_close_hovered = false;
                    self.side_question_max_scroll_offset = 0;
                    self.next_side_question_animation_at = None;
                    self.visible_links_invalidated = true;
                }
                TranscriptViewAction::ScrollSideQuestion { rows } => {
                    self.clear_text_selection();
                    if let Some(panel) = self.side_question.as_mut() {
                        if rows < 0 {
                            panel.scroll_up(rows.unsigned_abs() as usize);
                        } else {
                            panel.scroll_down(rows as usize, self.side_question_max_scroll_offset);
                        }
                    }
                }
                TranscriptViewAction::SideQuestionCloseHover { hovered } => {
                    self.side_question_close_hovered = hovered;
                }
            }
        }
    }

    fn cycle_highlighted_link(&mut self, forward: bool) {
        let count = self.visible_link_map.links().len();
        if count == 0 {
            self.highlighted_link_idx = None;
            return;
        }
        self.highlighted_link_idx = Some(match self.highlighted_link_idx {
            None if forward => 0,
            None => count - 1,
            Some(current) if forward => (current + 1) % count,
            Some(current) => (current + count - 1) % count,
        });
    }

    fn highlighted_link_target(&self) -> Option<&PagerLinkTarget> {
        self.highlighted_link_idx
            .and_then(|index| self.visible_link_map.links().get(index))
            .map(|link| &link.target)
    }

    fn take_highlighted_link_target(&mut self) -> Option<PagerLinkTarget> {
        let target = self.highlighted_link_target().cloned();
        self.highlighted_link_idx = None;
        target
    }

    fn selection_model_for_point(&self, point: (u16, u16)) -> &ResolvedSelectionModel {
        if self.side_question_area.contains(point.into()) {
            &self.last_side_question_selection_model
        } else {
            &self.last_scrollback_selection_model
        }
    }

    fn selection_model_for_hit(&self, hit: &RangeHit) -> &ResolvedSelectionModel {
        if hit.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX {
            &self.last_side_question_selection_model
        } else {
            &self.last_scrollback_selection_model
        }
    }

    fn absolute_entry_index(&self, relative_index: usize) -> Option<usize> {
        let absolute = self
            .scrollback
            .visible_entry_range()
            .start
            .checked_add(relative_index)?;
        (absolute < self.scrollback.len()).then_some(absolute)
    }

    fn with_entry_output_text_source<R>(
        &self,
        hit: &RangeHit,
        width_override: Option<u16>,
        operation: impl FnOnce(&dyn Fn(usize) -> Option<String>) -> R,
    ) -> Option<R> {
        let content_width = width_override.or_else(|| {
            self.last_scrollback_selection_model
                .visible_block_content_width(hit.entry_idx)
        })?;
        let entry = self
            .scrollback
            .get(self.absolute_entry_index(hit.entry_idx)?)?;
        let output = entry.effective_output(
            content_width,
            self.scrollback.appearance(),
            false,
            self.scrollback.cwd(),
        );
        let lines = &output.output().lines;
        let source = |index: usize| -> Option<String> {
            let line = lines.get(index)?;
            (line.selection_range == Some(hit.range_id))
                .then(|| crabcode_pager_render::scrollback::derive_selection_text(line))
        };
        Some(operation(&source))
    }

    fn compute_drag_table_geometry(&self, hit: &RangeHit) -> Option<TableGeometry> {
        if hit.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX {
            return None;
        }
        if let Some(line) = self.last_scrollback_selection_model.line_for_hit(hit)
            && !line.text.contains(['│', '┌', '├', '└'])
        {
            return None;
        }
        self.with_entry_output_text_source(hit, None, |source| {
            TableGeometry::detect(source, hit.block_line_idx)
        })
        .flatten()
    }

    fn table_geometry_for_selection(
        &self,
        entry_idx: usize,
        range_id: u16,
    ) -> Option<&TableGeometry> {
        self.table_selection_geometry
            .as_ref()
            .and_then(|table| table.for_selection(entry_idx, range_id))
    }

    fn resolve_drag_kind(
        &self,
        anchor: &RangeHit,
        head: &RangeHit,
        previous: SelectionKind,
    ) -> SelectionKind {
        resolve_table_drag_kind(
            self.table_geometry_for_selection(anchor.entry_idx, anchor.range_id),
            anchor,
            head,
            previous,
        )
    }

    fn clear_text_selection(&mut self) {
        self.persistent_text_selection = None;
        self.table_selection_geometry = None;
        self.selection_created_at = None;
        self.next_selection_tick_at = None;
    }

    fn cancel_pointer_gesture(&mut self) {
        self.pending_pointer_selection = None;
        self.pending_link_click = None;
        self.pending_text_click = None;
        self.pending_text_drag = None;
        self.active_text_drag = None;
        self.pending_block_drag = None;
        self.active_block_drag = None;
        self.deferred_text_press = None;
        self.drag_autoscroll = None;
        self.next_selection_tick_at = None;
        self.last_drag_mouse = None;
        self.left_mouse_down = false;
    }

    fn begin_pending_text_drag(&mut self, point: (u16, u16)) -> bool {
        let model = self.selection_model_for_point(point);
        let Some(anchor) = model.hit_test_selectable_range(point.0, point.1) else {
            return false;
        };
        let exact_hit = model
            .hit_test_text_exact(point.0, point.1)
            .map(|hit| (Instant::now(), hit));
        let anchor_content_width = model.visible_block_content_width(anchor.entry_idx);
        self.pending_text_click = exact_hit;
        self.pending_text_drag = Some(PendingTextDrag {
            anchor,
            start_col: point.0,
            start_row: point.1,
            anchor_content_width,
        });
        self.active_text_drag = None;
        true
    }

    fn begin_pending_block_drag(&mut self, point: (u16, u16)) -> bool {
        let Some(block) = self
            .last_scrollback_selection_model
            .hit_test_visible_block(point.0, point.1)
        else {
            return false;
        };
        if !block.drag_startable {
            return false;
        }
        self.pending_block_drag = Some(PendingBlockDrag {
            anchor_entry_idx: block.entry_idx,
            start_col: point.0,
            start_row: point.1,
        });
        self.active_text_drag = None;
        self.active_block_drag = None;
        true
    }

    fn arm_text_drag(&mut self, anchor: RangeHit, head: RangeHit, width: Option<u16>) {
        if anchor.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX {
            self.table_selection_geometry =
                self.compute_drag_table_geometry(&anchor)
                    .map(|geometry| TableSelectionGeometry {
                        entry_idx: anchor.entry_idx,
                        range_id: anchor.range_id,
                        geometry,
                    });
        }
        let kind = self.resolve_drag_kind(&anchor, &head, SelectionKind::Linear);
        self.active_text_drag = Some(ActiveTextDrag {
            anchor,
            head,
            kind,
            anchor_content_width: width,
        });
        self.pending_text_click = None;
    }

    fn convert_deferred_text_press(&mut self, point: (u16, u16)) -> bool {
        if self.deferred_text_press.is_none() {
            return false;
        }
        let Some(hit) = self
            .last_scrollback_selection_model
            .hit_test_selectable_range(point.0, point.1)
        else {
            return false;
        };
        self.deferred_text_press = None;
        self.pending_pointer_selection = None;
        self.pending_block_drag = None;
        self.active_block_drag = None;
        let width = self
            .last_scrollback_selection_model
            .visible_block_content_width(hit.entry_idx);
        self.arm_text_drag(hit, hit, width);
        self.last_drag_mouse = Some(point);
        true
    }

    fn update_active_block_drag(&mut self, point: (u16, u16)) {
        let Some(mut drag) = self.active_block_drag else {
            return;
        };
        if let Some(block) = self
            .last_scrollback_selection_model
            .hit_test_visible_block(point.0, point.1)
        {
            drag.head_entry_idx = block.entry_idx;
        }
        self.active_block_drag = Some(drag);
        self.last_drag_mouse = Some(point);
        self.drag_autoscroll =
            compute_autoscroll(point.1, self.last_scrollback_selection_model.content_area);
    }

    fn promote_pending_block_drag(&mut self, point: (u16, u16)) -> bool {
        let Some(pending) = self.pending_block_drag else {
            return false;
        };
        if !block_drag_threshold_exceeded(&pending, point.0, point.1) {
            return false;
        }
        let head_entry_idx = self
            .last_scrollback_selection_model
            .hit_test_visible_block(point.0, point.1)
            .map_or(pending.anchor_entry_idx, |block| block.entry_idx);
        self.pending_pointer_selection = None;
        self.pending_block_drag = None;
        self.active_block_drag = Some(ActiveBlockDrag {
            anchor_entry_idx: pending.anchor_entry_idx,
            head_entry_idx,
        });
        self.last_drag_mouse = Some(point);
        self.drag_autoscroll =
            compute_autoscroll(point.1, self.last_scrollback_selection_model.content_area);
        true
    }

    fn handle_pointer_drag(&mut self, point: Option<(u16, u16)>) {
        self.pending_link_click = None;
        let Some(point) = point else {
            return;
        };
        self.last_drag_mouse = Some(point);
        if let Some(mut drag) = self.active_text_drag {
            let model = self.selection_model_for_hit(&drag.anchor);
            if let Some(head) = model.hit_test_nearest_in_range(drag.anchor, point.0, point.1) {
                drag.head = head;
                drag.kind = self.resolve_drag_kind(&drag.anchor, &head, drag.kind);
                self.active_text_drag = Some(drag);
            }
        } else if self.convert_deferred_text_press(point) {
            // The first selectable cell entered by an anchor-less press owns
            // the rest of the gesture; block drag cannot resume afterward.
        } else if self.active_block_drag.is_some() {
            self.update_active_block_drag(point);
        } else if let Some(pending) = self.pending_text_drag
            && drag_threshold_exceeded(&pending, point.0, point.1)
        {
            let head = self
                .selection_model_for_hit(&pending.anchor)
                .hit_test_nearest_in_range(pending.anchor, point.0, point.1)
                .unwrap_or(pending.anchor);
            self.arm_text_drag(pending.anchor, head, pending.anchor_content_width);
        } else if self.pending_block_drag.is_some() {
            self.promote_pending_block_drag(point);
        }
        if self.active_text_drag.is_some() {
            self.drag_autoscroll = self.active_text_drag.and_then(|drag| {
                (drag.anchor.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX)
                    .then(|| {
                        compute_autoscroll(
                            point.1,
                            self.last_scrollback_selection_model.content_area,
                        )
                    })
                    .flatten()
            });
        }
        self.next_selection_tick_at = self
            .drag_autoscroll
            .map(|_| Instant::now() + TEXT_SELECTION_FAST_TICK);
    }

    fn finish_block_drag(&mut self, app: &mut TuiApp) {
        let Some(drag) = self.active_block_drag.take() else {
            return;
        };
        self.pending_block_drag = None;
        self.pending_pointer_selection = None;
        let start = drag.anchor_entry_idx.min(drag.head_entry_idx);
        let end = drag.anchor_entry_idx.max(drag.head_entry_idx);
        let mut parts = Vec::new();
        for relative_index in start..=end {
            let Some(absolute_index) = self.absolute_entry_index(relative_index) else {
                continue;
            };
            if self
                .scrollback
                .entry_content_hidden_by_group(absolute_index)
            {
                continue;
            }
            let Some(entry) = self.scrollback.get(absolute_index) else {
                continue;
            };
            if !entry.block.is_drag_block_selectable() {
                continue;
            }
            let width = self
                .last_scrollback_selection_model
                .visible_block_content_width(relative_index)
                .unwrap_or(80);
            let context = entry.context(width, self.scrollback.appearance(), self.scrollback.cwd());
            if let Some(text) = entry.block.copy_visible_text_in_state(&context)
                && !text.is_empty()
            {
                parts.push(text);
            }
        }
        let text = parts.join("\n\n");
        if text.is_empty() {
            return;
        }
        match crate::tui_clipboard::set_text(&text) {
            Ok(()) => {
                app.status = copied_blocks_status(app.ui_language(), parts.len());
            }
            Err(error) => {
                app.status = block_copy_failed_status(app.ui_language(), &error.to_string());
            }
        }
    }

    fn reconstruct_drag_copy(&self, drag: &ActiveTextDrag) -> Option<(String, SelectionKind)> {
        if drag.anchor.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX {
            let content_width = drag
                .anchor_content_width
                .map(usize::from)
                .unwrap_or_else(|| self.side_question_area.width.saturating_sub(4) as usize);
            if let Some(panel) = self.side_question.as_ref() {
                let full = panel.full_selection_model(content_width);
                if let Some(text) = reconstruct_selection_text(&full, drag) {
                    return Some((text, SelectionKind::Linear));
                }
            }
            return reconstruct_selection_text(&self.last_side_question_selection_model, drag)
                .map(|text| (text, SelectionKind::Linear));
        }

        if drag.kind != SelectionKind::Linear
            && let Some(geometry) =
                self.table_geometry_for_selection(drag.anchor.entry_idx, drag.anchor.range_id)
            && let Some(text) = self
                .with_entry_output_text_source(&drag.anchor, drag.anchor_content_width, |source| {
                    if TableGeometry::detect(source, drag.anchor.block_line_idx).as_ref()
                        != Some(geometry)
                    {
                        return None;
                    }
                    reconstruct_table_selection_text(geometry, drag, source)
                })
                .flatten()
        {
            return Some((text, drag.kind));
        }

        let width = drag.anchor_content_width.or_else(|| {
            self.last_scrollback_selection_model
                .visible_block_content_width(drag.anchor.entry_idx)
        });
        if let Some(content_width) = width
            && let Some(entry) = self
                .absolute_entry_index(drag.anchor.entry_idx)
                .and_then(|index| self.scrollback.get(index))
            && let Some(text) = entry.reconstruct_text_drag(
                content_width,
                self.scrollback.appearance(),
                self.scrollback.cwd(),
                drag,
            )
        {
            return Some((text, SelectionKind::Linear));
        }

        crabcode_pager_render::scrollback::text_selection::
            reconstruct_selection_text_with_boundaries(
                &self.last_scrollback_selection_model,
                &self.last_scrollback_selection_boundaries,
                drag,
            )
            .map(|text| (text, SelectionKind::Linear))
    }

    fn persist_drag_selection(&mut self, drag: &ActiveTextDrag, kind: SelectionKind) {
        if kind == SelectionKind::Linear && drag.kind != SelectionKind::Linear {
            self.table_selection_geometry = None;
        }
        self.persistent_text_selection = Some(PersistentTextSelection {
            entry_idx: drag.anchor.entry_idx,
            range_id: drag.anchor.range_id,
            anchor: SelectionEndpoint {
                block_line_idx: drag.anchor.block_line_idx,
                col_within_range: drag.anchor.col_within_range,
            },
            head: SelectionEndpoint {
                block_line_idx: drag.head.block_line_idx,
                col_within_range: drag.head.col_within_range,
            },
            origin: SelectionOrigin::Drag,
            kind,
        });
        self.selection_created_at = Some(Instant::now());
        self.next_selection_tick_at = Some(Instant::now() + TEXT_SELECTION_FLASH_DURATION);
    }

    fn persistent_as_drag(&self) -> Option<ActiveTextDrag> {
        let selection = self.persistent_text_selection?;
        Some(ActiveTextDrag {
            anchor: RangeHit {
                entry_idx: selection.entry_idx,
                range_id: selection.range_id,
                block_line_idx: selection.anchor.block_line_idx,
                col_within_range: selection.anchor.col_within_range,
            },
            head: RangeHit {
                entry_idx: selection.entry_idx,
                range_id: selection.range_id,
                block_line_idx: selection.head.block_line_idx,
                col_within_range: selection.head.col_within_range,
            },
            kind: selection.kind,
            anchor_content_width: self
                .selection_model_for_hit(&RangeHit {
                    entry_idx: selection.entry_idx,
                    range_id: selection.range_id,
                    block_line_idx: selection.anchor.block_line_idx,
                    col_within_range: selection.anchor.col_within_range,
                })
                .visible_block_content_width(selection.entry_idx),
        })
    }

    fn copy_persistent_text_selection(&mut self, app: &mut TuiApp) {
        let Some(drag) = self.persistent_as_drag() else {
            return;
        };
        let Some((text, _)) = self
            .reconstruct_drag_copy(&drag)
            .filter(|(text, _)| !text.is_empty())
        else {
            app.status = app
                .ui_language()
                .text(
                    "文本选择中没有可复制内容",
                    "Text selection has no copyable content",
                )
                .to_string();
            return;
        };
        match crate::tui_clipboard::set_text(&text) {
            Ok(()) => {
                app.status = copied_characters_status(app.ui_language(), text.chars().count());
                // Historical direct `selection:copy` clears; automatic
                // copy-on-select below deliberately preserves the flash.
                self.clear_text_selection();
            }
            Err(error) => {
                app.status = text_copy_failed_status(app.ui_language(), &error.to_string());
            }
        }
    }

    fn count_text_click(&self, now: Instant, hit: RangeHit) -> u8 {
        self.last_text_click
            .filter(|previous| {
                previous.hit.entry_idx == hit.entry_idx
                    && previous.hit.range_id == hit.range_id
                    && previous.hit.block_line_idx == hit.block_line_idx
                    && now.saturating_duration_since(previous.at) < MULTI_CLICK_TIMEOUT
            })
            .map_or(1, |previous| previous.count.saturating_add(1))
    }

    fn copy_semantic_selection(
        &mut self,
        app: &mut TuiApp,
        hit: RangeHit,
        range: std::ops::Range<u16>,
        text: String,
        origin: SelectionOrigin,
        kind: SelectionKind,
    ) {
        if range.is_empty() {
            return;
        }
        self.persistent_text_selection = Some(PersistentTextSelection {
            entry_idx: hit.entry_idx,
            range_id: hit.range_id,
            anchor: SelectionEndpoint {
                block_line_idx: hit.block_line_idx,
                col_within_range: range.start,
            },
            head: SelectionEndpoint {
                block_line_idx: hit.block_line_idx,
                col_within_range: range.end.saturating_sub(1),
            },
            origin,
            kind,
        });
        self.selection_created_at = Some(Instant::now());
        self.next_selection_tick_at = Some(Instant::now() + TEXT_SELECTION_FLASH_DURATION);
        if !text.trim().is_empty() {
            match crate::tui_clipboard::set_text(&text) {
                Ok(()) => {
                    app.status = copied_characters_status(app.ui_language(), text.chars().count());
                }
                Err(error) => {
                    app.status = text_copy_failed_status(app.ui_language(), &error.to_string());
                }
            }
        }
    }

    fn select_word_at(&mut self, app: &mut TuiApp, hit: RangeHit) {
        let Some(line) = self.selection_model_for_hit(&hit).line_for_hit(&hit) else {
            return;
        };
        let range = url_range_at_col(&line.text, hit.col_within_range).unwrap_or_else(|| {
            word_boundaries_at_col(
                &line.text,
                hit.col_within_range,
                configured_word_separators(),
            )
        });
        let text = crabcode_pager_render::scrollback::slice_display_cols(
            &line.text,
            range.start,
            range.end,
        );
        self.copy_semantic_selection(
            app,
            hit,
            range,
            text,
            SelectionOrigin::DoubleClick,
            SelectionKind::Linear,
        );
    }

    fn select_line_or_table_at(&mut self, app: &mut TuiApp, hit: RangeHit) {
        if hit.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX
            && let Some(geometry) = self.compute_drag_table_geometry(&hit)
        {
            if let Some(cell) = geometry.cell_at(hit.block_line_idx, hit.col_within_range) {
                let text = self
                    .with_entry_output_text_source(&hit, None, |source| {
                        geometry.cell_text(cell, source)
                    })
                    .unwrap_or_default();
                let lines = geometry.row_lines(cell.row);
                let band = geometry.band(cell.col);
                self.table_selection_geometry = Some(TableSelectionGeometry {
                    entry_idx: hit.entry_idx,
                    range_id: hit.range_id,
                    geometry,
                });
                self.persistent_text_selection = Some(PersistentTextSelection {
                    entry_idx: hit.entry_idx,
                    range_id: hit.range_id,
                    anchor: SelectionEndpoint {
                        block_line_idx: lines.start,
                        col_within_range: band.start,
                    },
                    head: SelectionEndpoint {
                        block_line_idx: lines.end.saturating_sub(1),
                        col_within_range: band.end.saturating_sub(1),
                    },
                    origin: SelectionOrigin::TripleClick,
                    kind: SelectionKind::TableCell,
                });
                self.selection_created_at = Some(Instant::now());
                self.next_selection_tick_at = Some(Instant::now() + TEXT_SELECTION_FLASH_DURATION);
                if !text.trim().is_empty() {
                    match crate::tui_clipboard::set_text(&text) {
                        Ok(()) => {
                            app.status =
                                copied_characters_status(app.ui_language(), text.chars().count());
                        }
                        Err(error) => {
                            app.status =
                                text_copy_failed_status(app.ui_language(), &error.to_string());
                        }
                    }
                }
                return;
            }

            let anchor = CellRef { row: 0, col: 0 };
            let head = CellRef {
                row: geometry.n_rows().saturating_sub(1),
                col: geometry.n_cols().saturating_sub(1),
            };
            if geometry.n_rows() > 0 && geometry.n_cols() > 0 {
                let text = self
                    .with_entry_output_text_source(&hit, None, |source| {
                        geometry.grid_tsv(anchor, head, source)
                    })
                    .unwrap_or_default();
                let first = geometry.row_lines(0);
                let last = geometry.row_lines(head.row);
                let start_col = geometry.band(0).start;
                let end_col = geometry.band(head.col).end.saturating_sub(1);
                self.table_selection_geometry = Some(TableSelectionGeometry {
                    entry_idx: hit.entry_idx,
                    range_id: hit.range_id,
                    geometry,
                });
                self.persistent_text_selection = Some(PersistentTextSelection {
                    entry_idx: hit.entry_idx,
                    range_id: hit.range_id,
                    anchor: SelectionEndpoint {
                        block_line_idx: first.start,
                        col_within_range: start_col,
                    },
                    head: SelectionEndpoint {
                        block_line_idx: last.end.saturating_sub(1),
                        col_within_range: end_col,
                    },
                    origin: SelectionOrigin::TripleClick,
                    kind: SelectionKind::TableGrid { anchor, head },
                });
                self.selection_created_at = Some(Instant::now());
                self.next_selection_tick_at = Some(Instant::now() + TEXT_SELECTION_FLASH_DURATION);
                if !text.trim().is_empty() {
                    match crate::tui_clipboard::set_text(&text) {
                        Ok(()) => {
                            app.status =
                                copied_characters_status(app.ui_language(), text.chars().count());
                        }
                        Err(error) => {
                            app.status =
                                text_copy_failed_status(app.ui_language(), &error.to_string());
                        }
                    }
                }
                return;
            }
        }

        let model = self.selection_model_for_hit(&hit);
        let Some(line) = model.line_for_hit(&hit) else {
            return;
        };
        let width = line
            .selectable_cols
            .end
            .saturating_sub(line.selectable_cols.start);
        if width == 0 {
            return;
        }
        let text = if hit.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX {
            line.text.clone()
        } else {
            self.last_scrollback_selection_boundaries
                .full_line_text(model, &hit)
                .unwrap_or_else(|| line.text.clone())
        };
        self.copy_semantic_selection(
            app,
            hit,
            0..width,
            text,
            SelectionOrigin::TripleClick,
            SelectionKind::Linear,
        );
    }

    fn finish_text_click(&mut self, app: &mut TuiApp, at: Instant, hit: RangeHit) {
        let count = self.count_text_click(at, hit);
        match count {
            1 => {
                self.clear_text_selection();
                if hit.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX
                    && let Some(index) = self.absolute_entry_index(hit.entry_idx)
                {
                    self.scrollback.set_selected(Some(index));
                }
            }
            2 => self.select_word_at(app, hit),
            3 => self.select_line_or_table_at(app, hit),
            _ => {
                self.last_text_click = None;
                return;
            }
        }
        self.last_text_click = (count < 3).then_some(TextClickState { at, hit, count });
    }

    fn begin_pointer_down(
        &mut self,
        point: (u16, u16),
        modifier_held: bool,
        policy: LinkInteractionPolicy,
    ) -> bool {
        self.left_mouse_down = true;
        self.pending_link_click = None;
        if policy.native_link_hover || !modifier_held {
            return false;
        }
        let Some(link) = self.visible_link_map.link_at(point.0, point.1) else {
            return false;
        };
        self.pending_link_click =
            app_should_open_link_on_click_with(policy.native_plain_url_open, link)
                .then(|| (point.0, point.1, link.target.clone()));
        // A delegated bare URL is still a link hit. The terminal owns its
        // activation and the transcript must not reinterpret the same press
        // as block selection.
        true
    }

    fn handle_pointer_down(
        &mut self,
        point: (u16, u16),
        modifier_held: bool,
        policy: LinkInteractionPolicy,
    ) {
        let in_side_question = self.side_question_area.contains(point.into());
        if !in_side_question {
            self.clear_text_selection();
        }
        if self.begin_pointer_down(point, modifier_held, policy) {
            return;
        }
        if self.begin_pending_text_drag(point) {
            return;
        }
        if !in_side_question {
            self.pending_pointer_selection = Some(point);
            self.begin_pending_block_drag(point);
            self.deferred_text_press = Some(point);
        }
    }

    fn finish_pointer_up(
        &mut self,
        point: (u16, u16),
        app: &mut TuiApp,
    ) -> Option<PagerLinkTarget> {
        self.left_mouse_down = false;
        self.drag_autoscroll = None;
        self.next_selection_tick_at = None;
        self.last_drag_mouse = None;
        self.deferred_text_press = None;
        if let Some(drag) = self.active_text_drag.take() {
            self.pending_text_drag = None;
            self.pending_text_click = None;
            if let Some((text, kind)) = self
                .reconstruct_drag_copy(&drag)
                .filter(|(text, _)| !text.is_empty())
            {
                match crate::tui_clipboard::set_text(&text) {
                    Ok(()) => {
                        self.persist_drag_selection(&drag, kind);
                        app.status =
                            copied_characters_status(app.ui_language(), text.chars().count());
                    }
                    Err(error) => {
                        app.status = text_copy_failed_status(app.ui_language(), &error.to_string());
                    }
                }
            }
            return None;
        }
        if self.active_block_drag.is_some() {
            self.finish_block_drag(app);
            self.pending_link_click = None;
            return None;
        }
        self.pending_text_drag = None;
        self.pending_block_drag = None;
        if let Some((at, hit)) = self.pending_text_click.take() {
            self.finish_text_click(app, at, hit);
            self.pending_link_click = None;
            return None;
        }
        let (column, row, target) = self.pending_link_click.take()?;
        (point == (column, row)).then_some(target)
    }

    fn update_hovered_link(&mut self, modifier_held: bool, policy: LinkInteractionPolicy) -> bool {
        if !modifier_held && self.hovered_link_idx.is_none() {
            return false;
        }
        let next = if modifier_held && !policy.native_link_hover {
            self.app_owned_link_index_at_pointer(policy)
        } else {
            None
        };
        if next == self.hovered_link_idx {
            false
        } else {
            self.hovered_link_idx = next;
            true
        }
    }

    fn app_owned_link_index_at_pointer(&self, policy: LinkInteractionPolicy) -> Option<usize> {
        if policy.native_link_hover {
            return None;
        }
        self.pointer
            .and_then(|(column, row)| self.visible_link_map.link_at(column, row))
            .filter(|link| app_should_open_link_on_click_with(policy.native_plain_url_open, link))
            .and_then(|hit| {
                self.visible_link_map
                    .links()
                    .iter()
                    .position(|link| std::ptr::eq(link, hit))
            })
    }

    fn retire_invalid_hovered_link(&mut self, policy: LinkInteractionPolicy) {
        if self.hovered_link_idx.is_some()
            && self.app_owned_link_index_at_pointer(policy) != self.hovered_link_idx
        {
            self.hovered_link_idx = None;
        }
    }

    /// Retire pointer/modifier state that cannot cross a terminal ownership
    /// generation.
    ///
    /// Job control can make the macOS physical-modifier side channel
    /// permanently unavailable. Keeping a hover or an in-progress click after
    /// that cutover would either strand visual state with no release event or
    /// let a pre-suspend gesture activate in the resumed generation.
    pub(crate) fn retire_input_side_channels_for_terminal_generation(&mut self) {
        self.pending_actions
            .retain(|action| !action.is_terminal_generation_scoped_pointer_action());
        self.cancel_pointer_gesture();
        self.pointer = None;
        self.last_mouse_moved_at = None;
        self.next_link_modifier_poll_at = None;
        self.hovered_link_idx = None;
    }

    /// How long recent pointer activity may keep the macOS Command-key poll
    /// armed when no link is highlighted. An active hover remains armed past
    /// this bound so a modifier release cannot strand its underline.
    #[cfg(any(target_os = "macos", test))]
    const LINK_MODIFIER_POLL_WINDOW: Duration = Duration::from_secs(3);

    /// Record one renderer-local pointer sample.
    ///
    /// The fixed mouse lifecycle refreshes the bounded polling window only
    /// when screen coordinates actually change. Repeated reports at the same
    /// cell therefore cannot keep the terminal wake loop alive.
    fn observe_pointer(&mut self, pointer: Option<(u16, u16)>, arrived_at: Instant) {
        let coordinates_changed = pointer.is_some() && pointer != self.pointer;
        self.pointer = pointer;
        if coordinates_changed {
            self.last_mouse_moved_at = Some(arrived_at);
            // Re-arm from the fresh state on the next owner-chain deadline
            // query. Keeping an absolute deadline after that query prevents
            // unrelated readiness from postponing the poll indefinitely.
            self.next_link_modifier_poll_at = None;
        }
        if pointer.is_none() {
            self.hovered_link_idx = None;
            self.next_link_modifier_poll_at = None;
        }
    }

    /// Whether the renderer has an application-owned link under the pointer
    /// that still needs the macOS modifier fallback.
    #[cfg(any(target_os = "macos", test))]
    fn needs_link_modifier_poll_at(&self, now: Instant, policy: LinkInteractionPolicy) -> bool {
        if policy.native_link_hover || self.visible_link_map.is_empty() {
            return false;
        }
        let app_owned_link_idx = self.app_owned_link_index_at_pointer(policy);
        if let Some(hovered_link_idx) = self.hovered_link_idx {
            // Continue past the ordinary activity window until the physical
            // modifier release is observed and clears the active highlight.
            return app_owned_link_idx == Some(hovered_link_idx);
        }
        let recently_moved = self.last_mouse_moved_at.is_some_and(|moved_at| {
            now.saturating_duration_since(moved_at) < Self::LINK_MODIFIER_POLL_WINDOW
        });
        if !recently_moved {
            return false;
        }
        app_owned_link_idx.is_some()
    }

    /// Return the persistent absolute deadline for the next modifier poll.
    ///
    /// `interval` is supplied by `AppView`, which owns the fixed 83ms slow
    /// cadence. The deadline is retained across unrelated event-loop wakes.
    #[cfg(any(target_os = "macos", test))]
    fn link_modifier_poll_deadline_at(
        &mut self,
        now: Instant,
        interval: Duration,
        policy: LinkInteractionPolicy,
    ) -> Option<Instant> {
        if !self.needs_link_modifier_poll_at(now, policy) {
            self.next_link_modifier_poll_at = None;
            return None;
        }
        let poll_at = *self
            .next_link_modifier_poll_at
            .get_or_insert_with(|| now + interval);
        if self.hovered_link_idx.is_some() {
            return Some(poll_at);
        }
        // Wake at the exact activity-window boundary when it precedes the
        // next poll. The wake parks the clock without querying CoreGraphics.
        let expires_at = self
            .last_mouse_moved_at
            .and_then(|moved_at| moved_at.checked_add(Self::LINK_MODIFIER_POLL_WINDOW));
        Some(expires_at.map_or(poll_at, |expires_at| poll_at.min(expires_at)))
    }

    /// Poll one due modifier deadline with an injected physical-state probe.
    ///
    /// The closure is invoked only when the deadline is due and the link is
    /// still eligible, making the no-poll branches directly testable without
    /// touching the operating system.
    #[cfg(any(target_os = "macos", test))]
    fn poll_link_modifier_if_due_with(
        &mut self,
        now: Instant,
        interval: Duration,
        policy: LinkInteractionPolicy,
        modifier_held: impl FnOnce() -> bool,
    ) -> bool {
        let Some(deadline) = self.link_modifier_poll_deadline_at(now, interval, policy) else {
            return false;
        };
        if deadline > now {
            return false;
        }
        self.next_link_modifier_poll_at = None;
        if !self.needs_link_modifier_poll_at(now, policy) {
            return false;
        }
        self.update_hovered_link(modifier_held(), policy)
    }

    /// Give a delivered-Super hover one bounded frame when the physical probe
    /// has been retired, then guarantee its removal even when the terminal
    /// emits no modifier-only release event.
    #[cfg(any(target_os = "macos", test))]
    fn retired_modifier_clear_deadline_at(
        &mut self,
        now: Instant,
        interval: Duration,
    ) -> Option<Instant> {
        if self.hovered_link_idx.is_none() {
            self.next_link_modifier_poll_at = None;
            return None;
        }
        Some(
            *self
                .next_link_modifier_poll_at
                .get_or_insert_with(|| now + interval),
        )
    }

    #[cfg(any(target_os = "macos", test))]
    fn clear_retired_modifier_hover_if_due(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.next_link_modifier_poll_at else {
            return false;
        };
        if deadline > now {
            return false;
        }
        self.next_link_modifier_poll_at = None;
        self.hovered_link_idx.take().is_some()
    }

    /// Production owner-chain deadline entrypoint. Non-macOS terminals
    /// receive modifier transitions through crossterm and never arm this
    /// polling clock.
    pub(crate) fn renderer_animation_deadline(
        &mut self,
        now: Instant,
        interval: Duration,
    ) -> Option<Instant> {
        let modifier_deadline = {
            #[cfg(target_os = "macos")]
            {
                if !crate::tui_input::physical_modifier_probe_available() {
                    self.retired_modifier_clear_deadline_at(now, interval)
                } else {
                    self.link_modifier_poll_deadline_at(now, interval, link_interaction_policy())
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        };
        let side_question_deadline = if self
            .side_question
            .as_ref()
            .is_some_and(SideQuestionPanelState::is_loading)
        {
            Some(
                *self
                    .next_side_question_animation_at
                    .get_or_insert_with(|| now + interval),
            )
        } else {
            self.next_side_question_animation_at = None;
            None
        };
        let selection_deadline =
            if self.drag_autoscroll.is_some() || self.selection_created_at.is_some() {
                Some(*self.next_selection_tick_at.get_or_insert_with(|| {
                    now + if self.drag_autoscroll.is_some() {
                        TEXT_SELECTION_FAST_TICK
                    } else {
                        TEXT_SELECTION_FLASH_DURATION
                    }
                }))
            } else {
                self.next_selection_tick_at = None;
                None
            };
        let api_retry_deadline = self
            .api_retry_animation_clock
            .map(|clock| clock.next_tick_at);
        // Grok's ScrollbackState is the animation owner for every running
        // native block (thinking, tools, subagents, background work). The
        // projection adapter may synchronize entries, but it must never park
        // the renderer clock while a visible entry still needs frames.
        let scrollback_deadline = if self.scrollback.needs_animation() {
            Some(
                *self
                    .next_scrollback_animation_at
                    .get_or_insert_with(|| now + interval),
            )
        } else {
            self.next_scrollback_animation_at = None;
            None
        };
        [
            modifier_deadline,
            side_question_deadline,
            selection_deadline,
            api_retry_deadline,
            scrollback_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Production owner-chain tick entrypoint.
    pub(crate) fn tick_renderer_animation(&mut self, now: Instant, interval: Duration) -> bool {
        let modifier_changed = {
            #[cfg(target_os = "macos")]
            {
                if !crate::tui_input::physical_modifier_probe_available() {
                    self.clear_retired_modifier_hover_if_due(now)
                } else {
                    self.poll_link_modifier_if_due_with(
                        now,
                        interval,
                        link_interaction_policy(),
                        || {
                            crate::tui_input::link_modifier_held(
                                crossterm::event::KeyModifiers::NONE,
                            )
                        },
                    )
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                false
            }
        };
        let side_question_changed = self
            .next_side_question_animation_at
            .filter(|deadline| *deadline <= now)
            .is_some_and(|_| {
                if self
                    .side_question
                    .as_ref()
                    .is_some_and(SideQuestionPanelState::is_loading)
                {
                    self.side_question_animation_tick =
                        self.side_question_animation_tick.wrapping_add(1);
                    self.next_side_question_animation_at = Some(now + interval);
                    true
                } else {
                    self.next_side_question_animation_at = None;
                    false
                }
            });
        let api_retry_changed = self.tick_api_retry_animation(now);
        let selection_changed = self.tick_text_selection(now);
        let scrollback_changed = self
            .next_scrollback_animation_at
            .filter(|deadline| *deadline <= now)
            .is_some_and(|_| {
                if self.scrollback.needs_animation() {
                    let changed = self.scrollback.tick();
                    self.next_scrollback_animation_at = Some(now + interval);
                    changed
                } else {
                    self.next_scrollback_animation_at = None;
                    false
                }
            });
        modifier_changed
            || side_question_changed
            || api_retry_changed
            || selection_changed
            || scrollback_changed
    }

    fn synchronize_api_retry_animation_clock(&mut self) {
        let window = self.tail_api_retry_animation_window();
        let Some((mounted_at, stop_at)) = window else {
            self.api_retry_animation_clock = None;
            return;
        };
        if self
            .api_retry_animation_clock
            .is_some_and(|clock| clock.mounted_at == mounted_at && clock.stop_at == stop_at)
        {
            return;
        }
        self.api_retry_animation_clock = mounted_at
            .checked_add(Duration::from_secs(1))
            .filter(|next_tick_at| *next_tick_at <= stop_at)
            .map(|next_tick_at| ApiRetryAnimationClock {
                mounted_at,
                stop_at,
                next_tick_at,
            });
    }

    fn tick_api_retry_animation(&mut self, now: Instant) -> bool {
        let Some(clock) = self.api_retry_animation_clock else {
            return false;
        };
        if now < clock.next_tick_at {
            return false;
        }
        let still_owned = self
            .tail_api_retry_animation_window()
            .is_some_and(|window| window == (clock.mounted_at, clock.stop_at));
        if !still_owned {
            self.api_retry_animation_clock = None;
            return false;
        }
        if let Some(entry) = self.scrollback.entries_mut().find(|entry| {
            entry.block.api_retry_animation_window() == Some((clock.mounted_at, clock.stop_at))
        }) {
            entry.invalidate_cache();
        }
        if now >= clock.stop_at {
            self.api_retry_animation_clock = None;
        } else {
            let elapsed_seconds = now.saturating_duration_since(clock.mounted_at).as_secs();
            self.api_retry_animation_clock = clock
                .mounted_at
                .checked_add(Duration::from_secs(elapsed_seconds.saturating_add(1)))
                .filter(|next_tick_at| *next_tick_at <= clock.stop_at)
                .map(|next_tick_at| ApiRetryAnimationClock {
                    next_tick_at,
                    ..clock
                });
        }
        true
    }

    fn tail_api_retry_animation_window(&self) -> Option<(Instant, Instant)> {
        self.scrollback
            .iter_entries()
            .filter(|&(_, entry)| matches!(entry.block, RenderBlock::CrabCodeProjection(_)))
            .map(|(_, entry)| entry.block.api_retry_animation_window())
            .last()
            .flatten()
    }

    fn tick_text_selection(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.next_selection_tick_at else {
            return false;
        };
        if deadline > now {
            return false;
        }

        if let Some(autoscroll) = self.drag_autoscroll {
            match autoscroll.direction {
                crabcode_pager_render::scrollback::text_selection::AutoScrollDirection::Up => {
                    self.scrollback.scroll_up(autoscroll.speed);
                }
                crabcode_pager_render::scrollback::text_selection::AutoScrollDirection::Down => {
                    self.scrollback.scroll_down(autoscroll.speed);
                }
            }
            if let (Some(point), Some(mut drag)) = (self.last_drag_mouse, self.active_text_drag)
                && drag.anchor.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX
                && let Some(head) = self
                    .last_scrollback_selection_model
                    .hit_test_nearest_in_range(drag.anchor, point.0, point.1)
            {
                drag.head = head;
                drag.kind = self.resolve_drag_kind(&drag.anchor, &head, drag.kind);
                self.active_text_drag = Some(drag);
            }
            if let Some(mut drag) = self.active_block_drag {
                let head_visible = self
                    .last_scrollback_selection_model
                    .visible_blocks
                    .iter()
                    .any(|block| block.entry_idx == drag.head_entry_idx);
                if !head_visible {
                    let replacement = match autoscroll.direction {
                        crabcode_pager_render::scrollback::text_selection::AutoScrollDirection::Down => {
                            self.last_scrollback_selection_model
                                .visible_blocks
                                .iter()
                                .find(|block| block.entry_idx > drag.head_entry_idx)
                        }
                        crabcode_pager_render::scrollback::text_selection::AutoScrollDirection::Up => {
                            self.last_scrollback_selection_model
                                .visible_blocks
                                .iter()
                                .rev()
                                .find(|block| block.entry_idx < drag.head_entry_idx)
                        }
                    };
                    if let Some(block) = replacement {
                        drag.head_entry_idx = block.entry_idx;
                        self.active_block_drag = Some(drag);
                    }
                }
            }
            self.next_selection_tick_at = Some(now + TEXT_SELECTION_FAST_TICK);
            return true;
        }

        if self.selection_created_at.is_some_and(|created| {
            now.saturating_duration_since(created) >= TEXT_SELECTION_FLASH_DURATION
        }) {
            self.clear_text_selection();
            return true;
        }
        self.next_selection_tick_at = self
            .selection_created_at
            .map(|created| created + TEXT_SELECTION_FLASH_DURATION);
        false
    }

    fn snapshot_highlighted_link(&self, app: &mut TuiApp) {
        app.set_renderer_link_highlighted(self.highlighted_link_idx.is_some());
    }

    fn snapshot_selection(&self, app: &mut TuiApp) {
        let selected_index = self.scrollback.selected();
        let selected_key = selected_index
            .and_then(|index| self.scrollback.entry(index))
            .and_then(|entry| self.projection.key_for_entry_id(entry.id))
            .map(str::to_string);
        let selected_raw = selected_index
            .and_then(|index| self.scrollback.entry(index))
            .is_some_and(|entry| entry.raw);
        app.set_renderer_selection_snapshot(selected_key, selected_raw);
        app.set_renderer_text_selection_active(self.persistent_text_selection.is_some());
    }

    fn render_empty(&self, frame: &mut Frame<'_>, area: Rect, app: &TuiApp, theme: CrabCodeTheme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(empty_transcript_welcome_lines(app, area.width, theme))
                .style(Style::default().bg(theme.bg_base)),
            area,
        );
    }

    fn reclamp_active_drag(&mut self, side_question_rebuilt: bool) {
        let Some(point) = self.last_drag_mouse else {
            return;
        };
        let Some(mut drag) = self.active_text_drag else {
            return;
        };
        if (drag.anchor.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX) != side_question_rebuilt {
            return;
        }
        let Some(head) = self
            .selection_model_for_hit(&drag.anchor)
            .hit_test_nearest_in_range(drag.anchor, point.0, point.1)
        else {
            return;
        };
        if head != drag.head {
            drag.head = head;
            drag.kind = self.resolve_drag_kind(&drag.anchor, &head, drag.kind);
            self.active_text_drag = Some(drag);
        }
    }

    fn paint_text_selection_overlay(
        &self,
        buffer: &mut ratatui::buffer::Buffer,
        side_question: bool,
    ) {
        let model = if side_question {
            &self.last_side_question_selection_model
        } else {
            &self.last_scrollback_selection_model
        };
        if let Some(drag) = self.active_text_drag
            && (drag.anchor.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX) == side_question
        {
            render_active_selection_overlay(
                model,
                &drag,
                self.table_geometry_for_selection(drag.anchor.entry_idx, drag.anchor.range_id),
                buffer,
            );
        } else if !side_question {
            if let Some(block_drag) = self.active_block_drag {
                render_block_drag_overlay(model, &block_drag, buffer);
            } else if let Some(selection) = self.persistent_text_selection
                && selection.entry_idx != SIDE_QUESTION_PANEL_ENTRY_IDX
            {
                render_persistent_selection_overlay(
                    model,
                    &selection,
                    self.table_geometry_for_selection(selection.entry_idx, selection.range_id),
                    buffer,
                );
            }
        } else if let Some(selection) = self.persistent_text_selection
            && selection.entry_idx == SIDE_QUESTION_PANEL_ENTRY_IDX
        {
            render_persistent_selection_overlay(
                model,
                &selection,
                self.table_geometry_for_selection(selection.entry_idx, selection.range_id),
                buffer,
            );
        }
    }

    fn paint_post_scrollback(
        &self,
        frame: &mut Frame<'_>,
        outer: Rect,
        content: Rect,
        _app: &TuiApp,
        theme: CrabCodeTheme,
        output: &RenderOutput,
    ) {
        if let Some(selection_box) = output.selection_box.as_ref() {
            selection_box.render(frame.buffer_mut());
        }
        let Some(scroll) = output.scroll_info else {
            return;
        };
        if scroll.total_height <= usize::from(scroll.viewport_height) || outer.width < 3 {
            return;
        }
        let scrollbar_area = Rect::new(
            outer.right().saturating_sub(1),
            content.y,
            1,
            content.height,
        );
        let mut state = ScrollbarState::new(scroll.total_height)
            .position(scroll.scroll_offset)
            .viewport_content_length(usize::from(scroll.viewport_height));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(if self.scrollback.is_follow_mode() {
                    theme.scrollbar_fg
                } else {
                    theme.gray_bright
                }))
                .track_style(Style::default().fg(theme.scrollbar_bg))
                .begin_symbol(None)
                .end_symbol(None),
            scrollbar_area,
            &mut state,
        );
    }

    fn synchronize_links(
        &mut self,
        frame: &mut Frame<'_>,
        app: &mut TuiApp,
        theme: CrabCodeTheme,
        output: Option<&RenderOutput>,
        panel_links: &LinkOverlay,
    ) -> Vec<LinkSpan> {
        let mut hyperlinks = Vec::new();
        for link in output
            .into_iter()
            .flat_map(|output| output.link_overlay.links())
            .chain(panel_links.links())
        {
            if let Some(url) =
                resolve_link_target_with_presentation(&link.target, link.presentation)
                    .and_then(|resolved| resolved.osc8_url)
            {
                hyperlinks.push(LinkSpan {
                    row: link.screen_row,
                    col_start: link.col_start,
                    col_end: link.col_end,
                    url,
                    id: link.id,
                });
            }
        }
        if self.visible_links_invalidated
            || self.visible_link_map.is_stale(self.scrollback.generation())
            || (output.is_none() && self.scrollback_visible_link_count != 0)
        {
            let citation_links = output.map_or_else(Vec::new, |output| {
                collect_citation_links(&self.scrollback, &output.selection_model)
            });
            let empty_overlay = LinkOverlay::new();
            self.visible_link_map.rebuild(
                self.scrollback.generation(),
                output.map_or(&empty_overlay, |output| &output.link_overlay),
                citation_links,
            );
            self.scrollback_visible_link_count = self.visible_link_map.len();
            self.visible_links_invalidated = false;
        } else {
            self.visible_link_map
                .truncate(self.scrollback_visible_link_count);
        }
        self.visible_link_map.append_from_overlay(panel_links);
        if let Some(index) = self.highlighted_link_idx {
            if self.visible_link_map.is_empty() {
                self.highlighted_link_idx = None;
            } else if index >= self.visible_link_map.links().len() {
                self.highlighted_link_idx = Some(self.visible_link_map.links().len() - 1);
            }
        }
        if self.hovered_link_idx.is_some_and(|index| {
            self.visible_link_map.is_empty() || index >= self.visible_link_map.links().len()
        }) {
            self.hovered_link_idx = None;
        }
        self.retire_invalid_hovered_link(link_interaction_policy());
        let style = Style::default()
            .fg(theme.bg_base)
            .bg(theme.link)
            .add_modifier(Modifier::BOLD);
        let mut paint = |index: usize| {
            if let Some(link) = self.visible_link_map.links().get(index) {
                for rect in &link.rects {
                    frame.buffer_mut().set_style(*rect, style);
                }
            }
        };
        if let Some(index) = self.highlighted_link_idx {
            paint(index);
        }
        if let Some(index) = self.hovered_link_idx
            && Some(index) != self.highlighted_link_idx
        {
            paint(index);
        }
        self.snapshot_highlighted_link(app);
        hyperlinks
    }

    /// Exact fixed affordance-row paint adapted only to CrabCode's existing
    /// Mermaid click-action enum and in-flight worker.
    fn paint_diagram_affordances(
        &self,
        frame: &mut Frame<'_>,
        app: &mut TuiApp,
        theme: CrabCodeTheme,
        placements: Vec<crabcode_pager_render::scrollback::DiagramAffordancePlacement>,
    ) {
        app.mermaid_hitboxes.clear();
        let pointer = self.pointer;
        for placement in placements {
            let rect = placement.screen_rect;
            let source: Arc<str> = Arc::from(placement.source);
            let row = affordance_row_for_language(
                app.mermaid_is_rendering(&source),
                renderer_language(app.ui_language()),
            );
            let fits = |column: u16, text: &str| {
                column
                    .checked_add(u16::try_from(text.width()).unwrap_or(u16::MAX))
                    .is_some_and(|end| end <= rect.width)
            };
            let (label_column, label) = row.label;
            if fits(label_column, label) {
                frame.buffer_mut().set_string_safe(
                    rect.x.saturating_add(label_column),
                    rect.y,
                    label,
                    Style::default().fg(theme.gray_dim),
                );
            }
            for button in row.buttons {
                if !fits(button.col, button.label) {
                    continue;
                }
                let width = u16::try_from(button.label.width()).unwrap_or(u16::MAX);
                let hitbox = Rect::new(rect.x.saturating_add(button.col), rect.y, width, 1);
                let style = if pointer.is_some_and(|point| hitbox.contains(point.into())) {
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(theme.gray)
                };
                frame
                    .buffer_mut()
                    .set_string_safe(hitbox.x, hitbox.y, button.label, style);
                app.mermaid_hitboxes.push((
                    hitbox,
                    product_mermaid_action(button.kind),
                    Arc::clone(&source),
                ));
            }
            if let Some((column, status)) = row.status
                && fits(column, status)
            {
                frame.buffer_mut().set_string_safe(
                    rect.x.saturating_add(column),
                    rect.y,
                    status,
                    Style::default().fg(theme.gray_dim),
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinkInteractionPolicy {
    native_link_hover: bool,
    native_plain_url_open: bool,
}

fn link_interaction_policy() -> LinkInteractionPolicy {
    let capabilities =
        crabcode_pager_render::audited_terminal::terminal_context().hyperlink_capabilities();
    LinkInteractionPolicy {
        native_link_hover: capabilities.native_link_hover,
        native_plain_url_open: capabilities.native_plain_url_open,
    }
}

/// Whether the application fallback should activate a modifier-clicked link.
///
/// Warp opens bare URL text itself even while mouse reporting is enabled.
/// Labels, citation blocks, and filesystem targets remain application-owned.
fn app_should_open_link_on_click_with(native_plain_url_open: bool, link: &VisibleLink) -> bool {
    if !native_plain_url_open {
        return true;
    }
    let Some(url) = crabcode_pager_render::render::osc8::resolve_link_target(&link.target)
        .and_then(|resolved| resolved.osc8_url)
    else {
        return true;
    };
    if !crabcode_pager_render::link_opener::is_safe_to_open(
        &url,
        crabcode_pager_render::audited_terminal::SchemeFilter::Standard,
    ) {
        return true;
    }
    !link.looks_like_bare_url_text()
}

/// Collect citation targets from visible WebSearch and WebFetch blocks.
///
/// This is the fixed AgentView post-render pass: only blocks represented by
/// the current selection model can contribute screen-space hit rectangles.
fn collect_citation_links(
    scrollback: &ScrollbackState,
    selection_model: &crabcode_pager_render::scrollback::text_selection::ResolvedSelectionModel,
) -> Vec<VisibleLink> {
    use crabcode_pager_render::scrollback::{RenderBlock, ToolCallBlock};

    let mut links = Vec::new();
    for block_geometry in &selection_model.visible_blocks {
        let Some(entry) = scrollback.entry(block_geometry.entry_idx) else {
            continue;
        };
        match &entry.block {
            RenderBlock::ToolCall(ToolCallBlock::WebSearch(search)) => {
                for url in &search.citations {
                    links.push(VisibleLink {
                        rects: vec![block_geometry.content_area],
                        target: PagerLinkTarget::Url(Arc::from(url.as_str())),
                        id: None,
                    });
                }
            }
            RenderBlock::ToolCall(ToolCallBlock::WebFetch(fetch)) if !fetch.url.is_empty() => {
                links.push(VisibleLink {
                    rects: vec![block_geometry.content_area],
                    target: PagerLinkTarget::Url(Arc::from(fetch.url.as_str())),
                    id: None,
                });
            }
            _ => {}
        }
    }
    links
}

fn product_link_target(target: &PagerLinkTarget) -> LinkTarget {
    match target {
        PagerLinkTarget::Url(url) => LinkTarget::Url(Arc::clone(url)),
        PagerLinkTarget::File(path) => LinkTarget::File(Arc::clone(path)),
    }
}

fn product_mermaid_action(kind: AffordanceKind) -> MermaidAffordanceAction {
    match kind {
        AffordanceKind::Open => MermaidAffordanceAction::Open,
        AffordanceKind::CopyPath => MermaidAffordanceAction::CopyPath,
        AffordanceKind::CopySource => MermaidAffordanceAction::CopySource,
    }
}

/// Drop terminal-owned OSC 8 spans covered by later-painted interaction
/// surfaces. Application mouse dispatch already routes those surfaces before
/// scrollback; this post-pass gives native terminal link activation the same
/// hit order.
fn drop_link_spans_intersecting(
    links: &mut Vec<LinkSpan>,
    occluders: impl IntoIterator<Item = Rect>,
) {
    let occluders = occluders.into_iter().collect::<Vec<_>>();
    links.retain(|link| {
        !occluders.iter().any(|occluder| {
            link.row >= occluder.y
                && link.row < occluder.bottom()
                && link.col_start < occluder.right()
                && occluder.x < link.col_end
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabcode_pager_render::render::osc8::{LinkPresentation, OverlayLink};
    use crabcode_pager_render::scrollback::ScrollbackEntry;
    use crabcode_pager_render::scrollback::blocks::tool::{OtherToolCallBlock, ToolCallBlock};
    use crabcode_pager_render::scrollback::blocks::{
        CrabCodeDirectApiError, CrabCodeDirectSystemBlock, CrabCodeProjectionBlock,
        CrabCodeProjectionKind,
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use serde_json::json;

    fn app() -> TuiApp {
        TuiApp::new(&json!({}), crate::tui_app::InitialSessionRequest::New, None)
    }

    #[test]
    fn renderer_owned_statuses_localize_without_rewriting_dynamic_values() {
        let target = LinkTarget::Url(Arc::from("https://example.com/原始?q=RAW"));
        let error = "backend::原始 error[42]";

        assert_eq!(
            selected_visible_link_status(UiLanguage::ZhCn, &target),
            "已选择可见链接：https://example.com/原始?q=RAW"
        );
        assert_eq!(
            selected_visible_link_status(UiLanguage::EnUs, &target),
            "Selected visible link: https://example.com/原始?q=RAW"
        );
        assert_eq!(
            block_copy_failed_status(UiLanguage::ZhCn, error),
            "复制所选块失败：backend::原始 error[42]"
        );
        assert_eq!(
            block_copy_failed_status(UiLanguage::EnUs, error),
            "Failed to copy block selection: backend::原始 error[42]"
        );
        assert_eq!(
            text_copy_failed_status(UiLanguage::ZhCn, error),
            "复制文本选择失败：backend::原始 error[42]"
        );
        assert_eq!(
            text_copy_failed_status(UiLanguage::EnUs, error),
            "Failed to copy text selection: backend::原始 error[42]"
        );
        assert_eq!(
            copied_blocks_status(UiLanguage::ZhCn, 3),
            "已复制 3 个选中块"
        );
        assert_eq!(
            copied_blocks_status(UiLanguage::EnUs, 3),
            "Copied 3 selected blocks"
        );
        assert_eq!(
            copied_characters_status(UiLanguage::ZhCn, 17),
            "已复制 17 个选中字符"
        );
        assert_eq!(
            copied_characters_status(UiLanguage::EnUs, 17),
            "Copied 17 selected characters"
        );
        assert_eq!(renderer_language(UiLanguage::ZhCn), RendererLanguage::ZhCn);
        assert_eq!(renderer_language(UiLanguage::EnUs), RendererLanguage::EnUs);
    }

    #[test]
    fn production_prepare_projects_sanitized_startup_notices_into_owned_scrollback() {
        let mut app = app();
        app.release_startup_barrier_for_test();
        app.push_startup_notice("启动提醒\u{1b}]8;;https://evil.invalid\u{7}".to_string());
        let mut view = AgentView::new();

        view.prepare(&mut app).expect("startup notice projection");

        let expected =
            sanitize_bounded_terminal_text("启动提醒\u{1b}]8;;https://evil.invalid\u{7}");
        assert!(matches!(
            view.scrollback().entry(0).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == expected
        ));
        assert_eq!(view.scrollback().len(), 1);
    }

    #[test]
    fn production_selection_uses_entry_identity_across_notices_and_filtered_rows() {
        let mut app = app();
        app.release_startup_barrier_for_test();
        let fixtures = [
            json!({
                "type":"system",
                "subtype":"informational",
                "level":"info",
                "uuid":"quiet-info",
                "session_id":"session",
                "timestamp":"2026-07-28T00:00:00.000Z",
                "content":"quiet"
            }),
            json!({
                "type":"user",
                "message":{"role":"user","content":"first visible"},
                "parent_tool_use_id":null,
                "uuid":"first-user",
                "session_id":"session"
            }),
            json!({
                "type":"user",
                "message":{"role":"user","content":"second visible"},
                "parent_tool_use_id":null,
                "uuid":"second-user",
                "session_id":"session"
            }),
        ];
        assert_eq!(app.projection.project_wire_fixtures(&fixtures, 1), Ok(3));
        let expected_key = app
            .projection
            .items()
            .iter()
            .find(|item| item.text == "first visible")
            .expect("first visible projected item")
            .key
            .clone();
        app.push_startup_notice("boot".to_string());
        let mut view = AgentView::new();
        view.prepare(&mut app)
            .expect("notice plus filtered projection");
        assert_eq!(view.scrollback().len(), 3);

        view.scrollback.set_selected(Some(1));
        view.snapshot_selection(&mut app);

        assert_eq!(
            app.selected_transcript_key.as_deref(),
            Some(expected_key.as_str())
        );
    }

    fn one_line_selection_model(
        text: &str,
        screen_x: u16,
        screen_y: u16,
    ) -> ResolvedSelectionModel {
        let width = u16::try_from(text.width()).expect("test text width");
        let mut model = ResolvedSelectionModel {
            content_area: Rect::new(screen_x, screen_y, width, 1),
            ..ResolvedSelectionModel::default()
        };
        model.push_line(
            crabcode_pager_render::scrollback::text_selection::ResolvedSelectableLine {
                entry_idx: 0,
                range_id: 0,
                block_line_idx: 0,
                screen_y,
                screen_x,
                selectable_cols: 0..width,
                text: text.to_string(),
                joiner_to_previous: None,
            },
        );
        model.visible_blocks.push(
            crabcode_pager_render::scrollback::text_selection::VisibleBlockGeometry {
                entry_idx: 0,
                area: Rect::new(screen_x, screen_y, width, 1),
                content_area: Rect::new(screen_x, screen_y, width, 1),
                selection_area: Rect::new(screen_x, screen_y, width, 1),
                content_width: width,
                top_clipped: false,
                bottom_clipped: false,
                drag_startable: true,
            },
        );
        model
    }

    #[test]
    fn action_surface_is_renderer_private_and_complete_for_fixed_scrollback_navigation() {
        let actions = [
            TranscriptViewAction::ScrollUp(1),
            TranscriptViewAction::ScrollDown(1),
            TranscriptViewAction::PageUp,
            TranscriptViewAction::PageDown,
            TranscriptViewAction::HalfPageUp,
            TranscriptViewAction::HalfPageDown,
            TranscriptViewAction::Top,
            TranscriptViewAction::Bottom,
            TranscriptViewAction::SelectNext,
            TranscriptViewAction::SelectPrevious,
            TranscriptViewAction::CollapseSelected,
            TranscriptViewAction::ExpandSelected,
            TranscriptViewAction::ToggleFoldSelected,
            TranscriptViewAction::ToggleRawSelected,
            TranscriptViewAction::NextTurn,
            TranscriptViewAction::PreviousTurn,
            TranscriptViewAction::NextResponse,
            TranscriptViewAction::PreviousResponse,
        ];
        assert_eq!(actions.len(), 18);
    }

    #[test]
    fn pager_link_targets_remain_path_native() {
        let path: Arc<std::path::Path> = Arc::from(std::path::Path::new("/tmp/a.rs"));
        assert_eq!(
            product_link_target(&PagerLinkTarget::File(Arc::clone(&path))),
            LinkTarget::File(path),
        );
    }

    #[test]
    fn semantic_actions_mutate_only_the_agent_owned_scrollback() {
        let mut view = AgentView::new();
        let mut app = app();
        view.scrollback.push(ScrollbackEntry::new(RenderBlock::stub(
            "first",
            Color::Blue,
        )));
        view.scrollback.push(ScrollbackEntry::new(RenderBlock::stub(
            "second",
            Color::Blue,
        )));

        view.enqueue(TranscriptViewAction::SelectNext);
        view.enqueue(TranscriptViewAction::ScrollUp(1));
        view.apply_pending_actions(&mut app);

        assert_eq!(view.scrollback.selected(), Some(0));
        assert!(view.pending_actions.is_empty());
    }

    #[test]
    fn presentation_setting_is_idempotent_not_a_toggle() {
        let mut view = AgentView::new();
        let mut app = app();
        let id = view
            .scrollback
            .push(ScrollbackEntry::new(RenderBlock::thinking("reason")));
        view.enqueue(TranscriptViewAction::SetThinkingExpanded(false));
        view.enqueue(TranscriptViewAction::SetThinkingExpanded(false));
        view.apply_pending_actions(&mut app);
        let index = view.scrollback.index_of_id(id).expect("thinking entry");
        assert_eq!(
            view.scrollback
                .entry(index)
                .expect("thinking entry")
                .display_mode(),
            crabcode_pager_render::scrollback::DisplayMode::Collapsed,
        );
    }

    #[test]
    fn presentation_filter_never_deletes_lifecycle_owned_rows_or_selection() {
        let mut view = AgentView::new();
        let mut app = app();
        let fixtures = [
            json!({
                "type":"system",
                "subtype":"informational",
                "level":"info",
                "uuid":"info",
                "session_id":"session",
                "timestamp":"2026-07-28T00:00:00.000Z",
                "content":"verbose information"
            }),
            json!({
                "type":"assistant",
                "uuid":"assistant",
                "session_id":"session",
                "parent_tool_use_id":null,
                "message":{
                    "id":"message",
                    "content":[{"type":"redacted_thinking","data":"ciphertext"}]
                }
            }),
        ];
        assert_eq!(app.projection.project_wire_fixtures(&fixtures, 1), Ok(2));

        view.prepare(&mut app).expect("quiet projection");
        assert!(view.scrollback().is_empty());

        app.set_presentation_verbose(true);
        view.prepare(&mut app).expect("verbose projection");
        assert_eq!(view.scrollback().len(), 2);
        view.scrollback.set_selected(Some(1));

        app.set_presentation_verbose(false);
        let quiet_delta = {
            view.prepare(&mut app)
                .expect("quiet projection after selection");
            view.last_delta()
        };
        assert_eq!(quiet_delta.removed, 0);
        assert_eq!(view.scrollback().len(), 2);
        assert_eq!(view.scrollback().selected(), Some(1));
    }

    #[test]
    fn production_nested_agent_ctrl_o_expands_outer_envelopes_without_implying_verbose() {
        let mut view = AgentView::new();
        let mut app = app();
        let mut fixtures = vec![json!({
            "type":"progress",
            "data":{
                "type":"agent_progress",
                "prompt":"inspect the repository",
                "agentId":"agent-1",
                "message":{
                    "type":"user",
                    "uuid":"nested-user",
                    "timestamp":"2026-07-28T00:00:00.000Z",
                    "message":{"role":"user","content":"inspect the repository"}
                }
            },
            "toolUseID":"progress-tool-0",
            "parentToolUseID":"agent-tool",
            "uuid":"progress-0",
            "timestamp":"2026-07-28T00:00:00.000Z"
        })];
        for (index, content) in [
            json!([{
                "type":"tool_use",
                "id":"nested-read",
                "name":"Read",
                "input":{"file_path":"README.md"}
            }]),
            json!([{"type":"text","text":"second"}]),
            json!([{"type":"text","text":"third"}]),
            json!([{
                "type":"thinking",
                "thinking":"private reasoning",
                "signature":"signature"
            }]),
            json!([{"type":"text","text":"fifth"}]),
        ]
        .into_iter()
        .enumerate()
        {
            let ordinal = index + 1;
            fixtures.push(json!({
                "type":"progress",
                "data":{
                    "type":"agent_progress",
                    "prompt":"",
                    "agentId":"agent-1",
                    "message":{
                        "type":"assistant",
                        "uuid":format!("nested-assistant-{ordinal}"),
                        "timestamp":format!("2026-07-28T00:00:0{ordinal}.000Z"),
                        "message":{
                            "id":format!("nested-message-{ordinal}"),
                            "role":"assistant",
                            "content":content
                        }
                    }
                },
                "toolUseID":format!("progress-tool-{ordinal}"),
                "parentToolUseID":"agent-tool",
                "uuid":format!("progress-{ordinal}"),
                "timestamp":format!("2026-07-28T00:00:0{ordinal}.000Z")
            }));
        }
        assert_eq!(
            app.projection.project_wire_fixtures(&fixtures, 1),
            Ok(fixtures.len())
        );

        view.prepare(&mut app)
            .expect("quiet nested Agent projection");
        assert_eq!(view.scrollback().len(), 6);
        assert!(matches!(
            view.scrollback().entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(_))
        ));

        assert!(
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL,
            )))
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("Ctrl-O nested Agent projection");
        assert_eq!(
            view.scrollback().len(),
            6,
            "Ctrl-O cannot duplicate or relocate the already-owned native child prompt"
        );
        assert!(matches!(
            view.scrollback().entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(_))
        ));

        app.set_presentation_verbose(true);
        view.prepare(&mut app)
            .expect("verbose transcript nested Agent projection");
        assert_eq!(
            view.scrollback().len(),
            6,
            "verbose keeps the same native child lifecycle"
        );

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )));
        view.prepare(&mut app)
            .expect("verbose quiet-slice nested Agent projection");
        assert_eq!(
            view.scrollback().len(),
            6,
            "closing transcript mode never deletes a lifecycle-owned child prompt"
        );
    }

    #[test]
    fn agent_view_owns_one_wrapped_link_and_opens_its_semantic_target() {
        let mut view = AgentView::new();
        let mut app = app();
        let target = Arc::<str>::from("https://example.com/wrapped");
        let mut overlay = LinkOverlay::new();
        for (screen_row, col_start, col_end) in [(2, 4, 12), (3, 4, 9)] {
            overlay.push(OverlayLink {
                screen_row,
                col_start,
                col_end,
                target: PagerLinkTarget::Url(Arc::clone(&target)),
                presentation: LinkPresentation::Opaque,
                id: Some(17),
            });
        }
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        view.scrollback_visible_link_count = view.visible_link_map.len();

        assert!(!view.visible_link_map.is_empty());
        assert_eq!(view.visible_link_map.links()[0].rects.len(), 2);

        view.enqueue(TranscriptViewAction::CycleHighlightedLink { forward: true });
        view.apply_pending_actions(&mut app);
        assert_eq!(view.highlighted_link_idx, Some(0));

        view.enqueue(TranscriptViewAction::OpenHighlightedLinkOrBlockViewer);
        view.apply_pending_actions(&mut app);
        assert_eq!(view.highlighted_link_idx, None);
        assert_eq!(app.take_link_open_request(), Some(LinkTarget::Url(target)),);
    }

    #[test]
    fn modifier_click_requires_same_cell_up_and_drag_cancels() {
        let mut view = AgentView::new();
        let mut app = app();
        let target = Arc::<str>::from("https://example.com/click");
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 5,
            col_start: 10,
            col_end: 18,
            target: PagerLinkTarget::Url(Arc::clone(&target)),
            presentation: LinkPresentation::Opaque,
            id: Some(19),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        let fallback = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };

        assert!(!view.begin_pointer_down((12, 5), false, fallback));
        assert!(view.pending_link_click.is_none());

        assert!(view.begin_pointer_down((12, 5), true, fallback));
        assert!(view.finish_pointer_up((13, 5), &mut app).is_none());

        assert!(view.begin_pointer_down((12, 5), true, fallback));
        view.enqueue(TranscriptViewAction::PointerDrag { point: (13, 5) });
        view.enqueue(TranscriptViewAction::PointerUp((12, 5)));
        view.apply_pending_actions(&mut app);
        assert!(app.take_link_open_request().is_none());

        assert!(view.begin_pointer_down((12, 5), true, fallback));
        view.enqueue(TranscriptViewAction::PointerUp((12, 5)));
        view.apply_pending_actions(&mut app);
        assert_eq!(app.take_link_open_request(), Some(LinkTarget::Url(target)));
    }

    #[test]
    fn text_press_drag_release_copies_and_persists_exact_linear_selection() {
        let mut view = AgentView::new();
        let mut app = app();
        view.last_scrollback_selection_model = one_line_selection_model("hello world", 2, 4);

        view.handle_pointer_down(
            (2, 4),
            false,
            LinkInteractionPolicy {
                native_link_hover: false,
                native_plain_url_open: false,
            },
        );
        assert!(view.pending_text_drag.is_some());
        view.handle_pointer_drag(Some((6, 4)));
        assert_eq!(
            view.active_text_drag.map(|drag| (drag.anchor, drag.head)),
            Some((
                RangeHit {
                    entry_idx: 0,
                    range_id: 0,
                    block_line_idx: 0,
                    col_within_range: 0,
                },
                RangeHit {
                    entry_idx: 0,
                    range_id: 0,
                    block_line_idx: 0,
                    col_within_range: 4,
                },
            )),
        );

        assert!(view.finish_pointer_up((6, 4), &mut app).is_none());
        let selection = view
            .persistent_text_selection
            .expect("mouse-up preserves the copied range for feedback");
        assert_eq!(selection.origin, SelectionOrigin::Drag);
        assert_eq!(selection.kind, SelectionKind::Linear);
        assert_eq!(selection.anchor.col_within_range, 0);
        assert_eq!(selection.head.col_within_range, 4);
        assert_eq!(app.status, "已复制 5 个选中字符");

        view.copy_persistent_text_selection(&mut app);
        assert!(
            view.persistent_text_selection.is_none(),
            "the explicit selection:copy action clears after confirmed delivery"
        );
    }

    #[test]
    fn anchorless_block_press_converts_once_when_drag_enters_text() {
        let mut view = AgentView::new();
        let mut model = one_line_selection_model("text", 5, 4);
        model.content_area = Rect::new(0, 3, 12, 3);
        model.visible_blocks[0].area = Rect::new(0, 3, 12, 3);
        model.visible_blocks[0].selection_area = Rect::new(0, 3, 12, 3);
        view.last_scrollback_selection_model = model;

        view.handle_pointer_down(
            (1, 3),
            false,
            LinkInteractionPolicy {
                native_link_hover: false,
                native_plain_url_open: false,
            },
        );
        assert_eq!(view.deferred_text_press, Some((1, 3)));
        assert!(view.pending_block_drag.is_some());
        assert!(view.active_text_drag.is_none());

        view.handle_pointer_drag(Some((5, 4)));
        let drag = view
            .active_text_drag
            .expect("first selectable cell converts the gesture");
        assert_eq!(drag.anchor, drag.head);
        assert_eq!(drag.anchor.col_within_range, 0);
        assert!(view.deferred_text_press.is_none());
        assert!(view.pending_pointer_selection.is_none());
        assert!(view.pending_block_drag.is_none());
        assert!(view.active_block_drag.is_none());
    }

    #[test]
    fn whole_block_drag_paints_and_copies_each_visible_block_once() {
        let mut view = AgentView::new();
        let mut app = app();
        for text in ["first block", "second block"] {
            view.scrollback
                .push(ScrollbackEntry::new(RenderBlock::agent_message(
                    text.to_string(),
                )));
        }
        let mut model = ResolvedSelectionModel {
            content_area: Rect::new(0, 3, 20, 5),
            ..ResolvedSelectionModel::default()
        };
        for (entry_idx, row) in [(0, 3), (1, 6)] {
            model.visible_blocks.push(
                crabcode_pager_render::scrollback::text_selection::VisibleBlockGeometry {
                    entry_idx,
                    area: Rect::new(0, row, 20, 2),
                    content_area: Rect::new(1, row, 18, 2),
                    selection_area: Rect::new(0, row, 20, 2),
                    content_width: 18,
                    top_clipped: false,
                    bottom_clipped: false,
                    drag_startable: true,
                },
            );
        }
        view.last_scrollback_selection_model = model;
        let policy = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };

        view.handle_pointer_down((2, 3), false, policy);
        view.handle_pointer_drag(Some((2, 6)));
        assert_eq!(
            view.active_block_drag,
            Some(ActiveBlockDrag {
                anchor_entry_idx: 0,
                head_entry_idx: 1,
            }),
        );
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 24, 10));
        let before = buffer.cell((2, 3)).expect("block cell").clone();
        view.paint_text_selection_overlay(&mut buffer, false);
        assert_ne!(buffer.cell((2, 3)).expect("first block cell"), &before);
        assert_ne!(buffer.cell((2, 6)).expect("second block cell"), &before);

        assert!(view.finish_pointer_up((2, 6), &mut app).is_none());
        assert_eq!(app.status, "已复制 2 个选中块");
        assert!(view.active_block_drag.is_none());
        assert!(view.pending_pointer_selection.is_none());
    }

    #[test]
    fn repeated_exact_clicks_select_word_then_complete_line() {
        let mut view = AgentView::new();
        let mut app = app();
        view.last_scrollback_selection_model = one_line_selection_model("hello world", 2, 4);
        let policy = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };

        for expected_count in 1..=3 {
            view.handle_pointer_down((3, 4), false, policy);
            assert!(view.finish_pointer_up((3, 4), &mut app).is_none());
            assert_eq!(
                view.last_text_click.map(|click| click.count),
                (expected_count < 3).then_some(expected_count),
            );
            match expected_count {
                1 => assert!(view.persistent_text_selection.is_none()),
                2 => {
                    let selection = view
                        .persistent_text_selection
                        .expect("double-click word selection");
                    assert_eq!(selection.origin, SelectionOrigin::DoubleClick);
                    assert_eq!(selection.anchor.col_within_range, 0);
                    assert_eq!(selection.head.col_within_range, 4);
                    assert_eq!(app.status, "已复制 5 个选中字符");
                }
                3 => {
                    let selection = view
                        .persistent_text_selection
                        .expect("triple-click line selection");
                    assert_eq!(selection.origin, SelectionOrigin::TripleClick);
                    assert_eq!(selection.anchor.col_within_range, 0);
                    assert_eq!(selection.head.col_within_range, 10);
                    assert_eq!(app.status, "已复制 11 个选中字符");
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn selection_overlay_flash_and_autoscroll_share_renderer_clock() {
        let mut view = AgentView::new();
        view.last_scrollback_selection_model = one_line_selection_model("hello", 2, 4);
        view.persistent_text_selection = Some(PersistentTextSelection {
            entry_idx: 0,
            range_id: 0,
            anchor: SelectionEndpoint {
                block_line_idx: 0,
                col_within_range: 1,
            },
            head: SelectionEndpoint {
                block_line_idx: 0,
                col_within_range: 3,
            },
            origin: SelectionOrigin::Drag,
            kind: SelectionKind::Linear,
        });
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 12, 8));
        let before = buffer.cell((3, 4)).expect("selected cell").clone();
        view.paint_text_selection_overlay(&mut buffer, false);
        assert_ne!(
            buffer.cell((3, 4)).expect("selected cell"),
            &before,
            "persistent selection must paint after the scrollback frame"
        );
        assert_eq!(
            buffer.cell((2, 4)).expect("unselected cell"),
            &ratatui::buffer::Cell::default(),
        );

        let created = Instant::now();
        view.selection_created_at = Some(created);
        view.next_selection_tick_at = Some(created + TEXT_SELECTION_FLASH_DURATION);
        assert!(view.tick_text_selection(created + TEXT_SELECTION_FLASH_DURATION));
        assert!(view.persistent_text_selection.is_none());

        for index in 0..24 {
            view.scrollback.push(ScrollbackEntry::new(RenderBlock::stub(
                format!("line {index}"),
                Color::Blue,
            )));
        }
        view.scrollback.prepare_layout(40, 5);
        view.scrollback.goto_bottom();
        let before_scroll = view.scrollback.scroll_info().0;
        assert!(before_scroll > 2, "test transcript must overflow");
        view.drag_autoscroll = Some(DragAutoScrollState {
            direction: crabcode_pager_render::scrollback::text_selection::AutoScrollDirection::Up,
            speed: 2,
        });
        view.next_selection_tick_at = Some(created);
        assert!(view.tick_text_selection(created));
        assert_eq!(view.scrollback.scroll_info().0, before_scroll - 2);
        assert_eq!(
            view.next_selection_tick_at,
            Some(created + TEXT_SELECTION_FAST_TICK),
        );
    }

    #[test]
    fn side_question_drag_reconstructs_across_rows_that_left_the_viewport() {
        let response = (0..18)
            .map(|index| format!("selectable side row {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut view = AgentView::new();
        view.side_question = Some(SideQuestionPanelState::done(
            "selection".to_string(),
            response,
        ));
        let area = Rect::new(0, 0, 40, 6);
        view.side_question_area = area;

        let first_render = {
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            render_side_question_panel(
                &mut buffer,
                view.side_question.as_ref().expect("panel"),
                area,
                0,
                true,
                false,
                &[],
            )
            .expect("first panel render")
        };
        view.side_question_max_scroll_offset = first_render.max_scroll_offset;
        view.last_side_question_selection_model = first_render.selection_model;
        let anchor_line = view.last_side_question_selection_model.ranges[0].lines[0].clone();
        let anchor_point = (
            anchor_line
                .screen_x
                .saturating_add(anchor_line.selectable_cols.start),
            anchor_line.screen_y,
        );
        view.handle_pointer_down(
            anchor_point,
            false,
            LinkInteractionPolicy {
                native_link_hover: false,
                native_plain_url_open: false,
            },
        );
        view.handle_pointer_drag(Some((anchor_point.0.saturating_add(1), anchor_point.1)));
        assert!(view.active_text_drag.is_some());

        view.side_question
            .as_mut()
            .expect("panel")
            .scroll_down(8, view.side_question_max_scroll_offset);
        let second_render = {
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            render_side_question_panel(
                &mut buffer,
                view.side_question.as_ref().expect("panel"),
                area,
                0,
                true,
                false,
                &[],
            )
            .expect("scrolled panel render")
        };
        view.last_side_question_selection_model = second_render.selection_model;
        let head_line = view.last_side_question_selection_model.ranges[0]
            .lines
            .last()
            .expect("visible head")
            .clone();
        let head_point = (
            head_line
                .screen_x
                .saturating_add(head_line.selectable_cols.end.saturating_sub(1)),
            head_line.screen_y,
        );
        view.handle_pointer_drag(Some(head_point));
        let drag = view.active_text_drag.expect("drag remains active");

        let visible_only =
            reconstruct_selection_text(&view.last_side_question_selection_model, &drag)
                .expect("visible fallback");
        let full_model = view
            .side_question
            .as_ref()
            .expect("panel")
            .full_selection_model(usize::from(
                drag.anchor_content_width.expect("captured panel width"),
            ));
        let expected =
            reconstruct_selection_text(&full_model, &drag).expect("complete panel reconstruction");
        let (actual, kind) = view
            .reconstruct_drag_copy(&drag)
            .expect("production side-panel copy reconstruction");
        assert_eq!(kind, SelectionKind::Linear);
        assert_eq!(actual, expected);
        assert_ne!(
            actual, visible_only,
            "the production path must not lose rows that scrolled out of the panel"
        );
        assert!(actual.contains(&anchor_line.text));
        assert!(actual.contains(&head_line.text));
    }

    #[test]
    fn native_terminal_delegation_keeps_only_non_bare_targets_app_owned() {
        let mut view = AgentView::new();
        let bare = Arc::<str>::from("https://example.com");
        let bare_width = u16::try_from(bare.as_ref().width()).expect("URL width");
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 4,
            col_start: 2,
            col_end: 2 + bare_width,
            target: PagerLinkTarget::Url(Arc::clone(&bare)),
            presentation: LinkPresentation::Opaque,
            id: Some(20),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());

        let native_plain = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: true,
        };
        assert!(
            view.begin_pointer_down((3, 4), true, native_plain),
            "the delegated link hit is consumed instead of selecting a block"
        );
        assert!(view.pending_link_click.is_none());

        let native_hover = LinkInteractionPolicy {
            native_link_hover: true,
            native_plain_url_open: false,
        };
        assert!(!view.begin_pointer_down((3, 4), true, native_hover));
        assert!(view.pending_link_click.is_none());

        let mut labeled_overlay = LinkOverlay::new();
        labeled_overlay.push(OverlayLink {
            screen_row: 4,
            col_start: 2,
            col_end: 6,
            target: PagerLinkTarget::Url(Arc::clone(&bare)),
            presentation: LinkPresentation::Opaque,
            id: Some(21),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &labeled_overlay, Vec::new());
        assert!(view.begin_pointer_down((3, 4), true, native_plain));
        assert!(view.pending_link_click.is_some());

        let path: Arc<std::path::Path> =
            Arc::from(std::path::Path::new("/tmp/crabcode-link-target.rs"));
        let mut file_overlay = LinkOverlay::new();
        file_overlay.push(OverlayLink {
            screen_row: 6,
            col_start: 2,
            col_end: 8,
            target: PagerLinkTarget::File(Arc::clone(&path)),
            presentation: LinkPresentation::Opaque,
            id: Some(23),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &file_overlay, Vec::new());
        assert!(view.begin_pointer_down((3, 6), true, native_plain));
        assert_eq!(
            view.pending_link_click
                .as_ref()
                .map(|(_, _, target)| target),
            Some(&PagerLinkTarget::File(path)),
        );
    }

    #[test]
    fn modifier_hover_is_agent_owned_and_obeys_terminal_delegation() {
        let mut view = AgentView::new();
        let target = Arc::<str>::from("https://example.com/label");
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 8,
            target: PagerLinkTarget::Url(target),
            presentation: LinkPresentation::Opaque,
            id: Some(22),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        view.pointer = Some((4, 7));

        let fallback = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };
        assert!(view.update_hovered_link(true, fallback));
        assert_eq!(view.hovered_link_idx, Some(0));
        assert!(view.update_hovered_link(false, fallback));
        assert_eq!(view.hovered_link_idx, None);

        let native_hover = LinkInteractionPolicy {
            native_link_hover: true,
            native_plain_url_open: false,
        };
        assert!(!view.update_hovered_link(true, native_hover));
        assert_eq!(view.hovered_link_idx, None);
    }

    #[test]
    fn pointer_activity_window_advances_only_when_coordinates_change() {
        let mut view = AgentView::new();
        let started_at = Instant::now();
        view.observe_pointer(Some((4, 7)), started_at);
        assert_eq!(view.last_mouse_moved_at, Some(started_at));

        let repeated_at = started_at + Duration::from_secs(2);
        view.observe_pointer(Some((4, 7)), repeated_at);
        assert_eq!(
            view.last_mouse_moved_at,
            Some(started_at),
            "a repeated report at the same cell must not extend polling"
        );

        let moved_at = repeated_at + Duration::from_millis(1);
        view.observe_pointer(Some((5, 7)), moved_at);
        assert_eq!(view.last_mouse_moved_at, Some(moved_at));

        view.observe_pointer(None, moved_at + Duration::from_secs(1));
        assert_eq!(
            view.last_mouse_moved_at,
            Some(moved_at),
            "focus loss is not pointer-coordinate movement"
        );
        assert_eq!(view.next_link_modifier_poll_at, None);
    }

    #[test]
    fn only_moved_mouse_events_enter_the_pointer_activity_path() {
        let mut app = app();
        let started_at = Instant::now();
        let event = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 4,
                row: 7,
                modifiers: KeyModifiers::NONE,
            })
        };

        let _ = app.handle_event_at(event(MouseEventKind::Down(MouseButton::Left)), started_at);
        assert!(
            app.take_transcript_view_actions()
                .iter()
                .all(|action| { !matches!(action, TranscriptViewAction::PointerMoved { .. }) })
        );

        let moved_at = started_at + Duration::from_millis(1);
        let _ = app.handle_event_at(event(MouseEventKind::Moved), moved_at);
        assert!(app.take_transcript_view_actions().iter().any(|action| {
            matches!(
                action,
                TranscriptViewAction::PointerMoved {
                    pointer: Some((4, 7)),
                    arrived_at,
                } if *arrived_at == moved_at
            )
        }));
    }

    #[test]
    fn bounded_link_poll_gates_native_empty_delegated_and_expired_states() {
        let mut view = AgentView::new();
        let target = Arc::<str>::from("https://example.com/label");
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 8,
            target: PagerLinkTarget::Url(target),
            presentation: LinkPresentation::Opaque,
            id: Some(24),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        let moved_at = Instant::now();
        view.observe_pointer(Some((4, 7)), moved_at);
        let fallback = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };

        assert!(view.needs_link_modifier_poll_at(moved_at, fallback));
        assert!(!view.needs_link_modifier_poll_at(
            moved_at + AgentView::LINK_MODIFIER_POLL_WINDOW,
            fallback,
        ));

        view.hovered_link_idx = Some(0);
        assert!(
            view.needs_link_modifier_poll_at(
                moved_at + AgentView::LINK_MODIFIER_POLL_WINDOW + Duration::from_secs(30),
                fallback,
            ),
            "an active hover must remain armed until modifier release"
        );
        view.hovered_link_idx = None;

        assert!(!view.needs_link_modifier_poll_at(
            moved_at,
            LinkInteractionPolicy {
                native_link_hover: true,
                native_plain_url_open: false,
            },
        ));

        view.visible_link_map.rebuild(
            view.scrollback.generation(),
            &LinkOverlay::new(),
            Vec::new(),
        );
        assert!(!view.needs_link_modifier_poll_at(moved_at, fallback));

        let bare = Arc::<str>::from("https://example.com");
        let bare_width = u16::try_from(bare.as_ref().width()).expect("URL width");
        let mut delegated = LinkOverlay::new();
        delegated.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 3 + bare_width,
            target: PagerLinkTarget::Url(bare),
            presentation: LinkPresentation::Opaque,
            id: Some(25),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &delegated, Vec::new());
        view.observe_pointer(Some((4, 7)), moved_at);
        assert!(
            !view.needs_link_modifier_poll_at(
                moved_at,
                LinkInteractionPolicy {
                    native_link_hover: false,
                    native_plain_url_open: true,
                },
            ),
            "a terminal-owned bare URL must not arm the application poll"
        );
        view.hovered_link_idx = Some(0);
        assert!(!view.needs_link_modifier_poll_at(
            moved_at,
            LinkInteractionPolicy {
                native_link_hover: false,
                native_plain_url_open: true,
            },
        ));
        assert_eq!(
            view.link_modifier_poll_deadline_at(
                moved_at,
                crate::app_view::SLOW_TICK_INTERVAL,
                LinkInteractionPolicy {
                    native_link_hover: false,
                    native_plain_url_open: true,
                },
            ),
            None,
        );
        view.retire_invalid_hovered_link(LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: true,
        });
        assert_eq!(
            view.hovered_link_idx, None,
            "an impossible stale hover on a delegated target must be retired"
        );
    }

    #[test]
    fn link_poll_keeps_absolute_83ms_deadline_and_observes_late_release() {
        let mut view = AgentView::new();
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 8,
            target: PagerLinkTarget::Url(Arc::from("https://example.com/label")),
            presentation: LinkPresentation::Opaque,
            id: Some(26),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        let moved_at = Instant::now();
        view.observe_pointer(Some((4, 7)), moved_at);
        let fallback = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };
        let interval = crate::app_view::SLOW_TICK_INTERVAL;
        let first_deadline = view
            .link_modifier_poll_deadline_at(moved_at, interval, fallback)
            .expect("recent openable link arms poll");
        assert_eq!(first_deadline, moved_at + Duration::from_millis(83));
        assert_eq!(
            view.link_modifier_poll_deadline_at(
                moved_at + Duration::from_millis(40),
                interval,
                fallback,
            ),
            Some(first_deadline),
            "unrelated wakes must not postpone an absolute poll deadline"
        );
        assert!(!view.poll_link_modifier_if_due_with(
            first_deadline - Duration::from_millis(1),
            interval,
            fallback,
            || panic!("an early deadline must not touch the OS probe"),
        ));
        assert!(view.poll_link_modifier_if_due_with(first_deadline, interval, fallback, || true,));
        assert_eq!(view.hovered_link_idx, Some(0));

        let after_window = moved_at + AgentView::LINK_MODIFIER_POLL_WINDOW + Duration::from_secs(1);
        let release_deadline = view
            .link_modifier_poll_deadline_at(after_window, interval, fallback)
            .expect("active hover remains armed after the activity window");
        assert!(
            view.poll_link_modifier_if_due_with(release_deadline, interval, fallback, || false,)
        );
        assert_eq!(view.hovered_link_idx, None);
        assert_eq!(
            view.link_modifier_poll_deadline_at(release_deadline, interval, fallback),
            None,
            "release after expiry parks the poll immediately"
        );
    }

    #[test]
    fn expired_inactive_poll_parks_without_querying_modifier_state() {
        let mut view = AgentView::new();
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 8,
            target: PagerLinkTarget::Url(Arc::from("https://example.com/label")),
            presentation: LinkPresentation::Opaque,
            id: Some(27),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        let moved_at = Instant::now();
        view.observe_pointer(Some((4, 7)), moved_at);
        let fallback = LinkInteractionPolicy {
            native_link_hover: false,
            native_plain_url_open: false,
        };
        assert!(!view.poll_link_modifier_if_due_with(
            moved_at + AgentView::LINK_MODIFIER_POLL_WINDOW,
            crate::app_view::SLOW_TICK_INTERVAL,
            fallback,
            || panic!("expired inactive state must not query CoreGraphics"),
        ));
        assert_eq!(view.next_link_modifier_poll_at, None);
    }

    #[test]
    fn retired_modifier_hover_has_one_absolute_bounded_clear_deadline() {
        let mut view = AgentView::new();
        let now = Instant::now();
        let interval = crate::app_view::SLOW_TICK_INTERVAL;
        view.hovered_link_idx = Some(3);

        let deadline = view
            .retired_modifier_clear_deadline_at(now, interval)
            .expect("delivered-Super hover needs one clear deadline");
        assert_eq!(deadline, now + interval);
        assert_eq!(
            view.retired_modifier_clear_deadline_at(now + Duration::from_millis(40), interval),
            Some(deadline),
            "unrelated wakes must not postpone the retired-probe clear"
        );
        assert!(!view.clear_retired_modifier_hover_if_due(deadline - Duration::from_millis(1)));
        assert_eq!(view.hovered_link_idx, Some(3));
        assert!(view.clear_retired_modifier_hover_if_due(deadline));
        assert_eq!(view.hovered_link_idx, None);
        assert_eq!(
            view.retired_modifier_clear_deadline_at(deadline, interval),
            None
        );
    }

    #[test]
    fn terminal_generation_retires_all_pointer_side_channels() {
        let mut view = AgentView::new();
        let mut app = app();
        let now = Instant::now();
        let target = Arc::<str>::from("https://example.com/stale-click");
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 8,
            target: PagerLinkTarget::Url(Arc::clone(&target)),
            presentation: LinkPresentation::Opaque,
            id: Some(31),
        });
        view.visible_link_map
            .rebuild(view.scrollback.generation(), &overlay, Vec::new());
        view.pointer = Some((4, 7));
        view.last_mouse_moved_at = Some(now);
        view.next_link_modifier_poll_at = Some(now + Duration::from_millis(83));
        view.pending_pointer_selection = Some((4, 7));
        view.hovered_link_idx = Some(0);
        view.pending_link_click =
            Some((4, 7, PagerLinkTarget::Url(Arc::from("https://example.com"))));
        view.left_mouse_down = true;
        view.enqueue(TranscriptViewAction::PointerDown {
            point: (4, 7),
            link_modifier_held: true,
        });
        view.enqueue(TranscriptViewAction::ScrollUp(1));

        view.retire_input_side_channels_for_terminal_generation();

        assert_eq!(view.pointer, None);
        assert_eq!(view.last_mouse_moved_at, None);
        assert_eq!(view.next_link_modifier_poll_at, None);
        assert_eq!(view.pending_pointer_selection, None);
        assert_eq!(view.hovered_link_idx, None);
        assert_eq!(view.pending_link_click, None);
        assert!(!view.left_mouse_down);
        assert_eq!(
            view.pending_actions,
            VecDeque::from([TranscriptViewAction::ScrollUp(1)]),
            "semantic scroll state remains queued, but old pointer input is retired"
        );

        // A release delivered by the resumed terminal generation cannot pair
        // with the retired pre-suspend press.
        view.enqueue(TranscriptViewAction::PointerUp((4, 7)));
        view.apply_pending_actions(&mut app);
        assert_eq!(app.take_link_open_request(), None);
    }

    #[test]
    fn side_question_state_ignores_stale_response_and_dismisses_locally() {
        let mut view = AgentView::new();
        let mut app = app();

        view.enqueue(TranscriptViewAction::StartSideQuestion {
            question: "first".to_string(),
        });
        view.enqueue(TranscriptViewAction::StartSideQuestion {
            question: "second".to_string(),
        });
        view.enqueue(TranscriptViewAction::FinishSideQuestion {
            question: "first".to_string(),
            result: Ok("stale".to_string()),
        });
        view.apply_pending_actions(&mut app);

        assert!(matches!(
            view.side_question,
            Some(SideQuestionPanelState::Loading { ref question }) if question == "second"
        ));
        assert!(!view.side_question_focused);

        view.enqueue(TranscriptViewAction::FinishSideQuestion {
            question: "second".to_string(),
            result: Ok("current".to_string()),
        });
        view.apply_pending_actions(&mut app);
        assert!(matches!(
            view.side_question,
            Some(SideQuestionPanelState::Done { ref question, .. }) if question == "second"
        ));
        assert!(view.side_question_focused);

        view.enqueue(TranscriptViewAction::DismissSideQuestion);
        view.apply_pending_actions(&mut app);
        assert!(view.side_question.is_none());
        assert!(!view.side_question_focused);
        assert_eq!(view.side_question_area, Rect::default());
        assert_eq!(view.side_question_close_rect, Rect::default());
    }

    #[test]
    fn side_question_loading_owns_the_renderer_animation_deadline() {
        let mut view = AgentView::new();
        let mut app = app();
        let started_at = Instant::now();
        let interval = Duration::from_millis(83);
        view.enqueue(TranscriptViewAction::StartSideQuestion {
            question: "why".to_string(),
        });
        view.apply_pending_actions(&mut app);

        let deadline = view
            .renderer_animation_deadline(started_at, interval)
            .expect("loading panel arms the renderer cadence");
        assert_eq!(deadline, started_at + interval);
        assert!(!view.tick_renderer_animation(started_at, interval));
        assert!(view.tick_renderer_animation(deadline, interval));
        assert_eq!(view.side_question_animation_tick, 1);
        assert_eq!(
            view.renderer_animation_deadline(deadline, interval),
            Some(deadline + interval)
        );

        view.enqueue(TranscriptViewAction::FinishSideQuestion {
            question: "why".to_string(),
            result: Err("stopped".to_string()),
        });
        view.apply_pending_actions(&mut app);
        assert_eq!(
            view.renderer_animation_deadline(deadline, interval),
            None,
            "a settled panel parks its renderer-only animation clock"
        );
    }

    #[test]
    fn running_native_scrollback_entry_owns_frames_until_finished() {
        let mut view = AgentView::new();
        let started_at = Instant::now();
        let interval = Duration::from_millis(33);
        let id = view
            .scrollback
            .push(ScrollbackEntry::running(RenderBlock::thinking("reason")));

        let first = view
            .renderer_animation_deadline(started_at, interval)
            .expect("running native entry arms upstream scrollback animation");
        assert_eq!(first, started_at + interval);
        assert!(!view.tick_renderer_animation(first - Duration::from_nanos(1), interval));
        assert!(view.tick_renderer_animation(first, interval));
        assert_eq!(view.scrollback.animation_tick(), 1);
        assert_eq!(
            view.renderer_animation_deadline(first, interval),
            Some(first + interval)
        );

        view.scrollback.finish_running(id);
        assert_eq!(
            view.renderer_animation_deadline(first, interval),
            None,
            "finish parks the running-entry cadence"
        );
    }

    #[test]
    fn api_retry_owns_stable_second_deadlines_and_parks_at_zero() {
        let mounted_at = Instant::now();
        let mut view = AgentView::new();
        view.scrollback
            .push(ScrollbackEntry::new(RenderBlock::CrabCodeProjection(
                CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
                    CrabCodeDirectSystemBlock::ApiError {
                        error: CrabCodeDirectApiError {
                            message: Some("temporary".to_string()),
                            status: None,
                            nested_message: None,
                            deeply_nested_message: None,
                            connection_code: None,
                        },
                        retry_in_ms: serde_json::Number::from_f64(2_500.0).expect("finite retry"),
                        retry_attempt: serde_json::Number::from(4),
                        max_retries: serde_json::Number::from(6),
                        mounted_at,
                    },
                )),
            )));
        view.synchronize_api_retry_animation_clock();
        let interval = Duration::from_millis(83);
        let first = mounted_at + Duration::from_secs(1);
        assert_eq!(
            view.renderer_animation_deadline(mounted_at, interval),
            Some(first)
        );
        assert_eq!(
            view.renderer_animation_deadline(mounted_at + Duration::from_millis(700), interval),
            Some(first),
            "deadline queries must not slide the absolute one-second boundary"
        );
        assert!(!view.tick_renderer_animation(first - Duration::from_nanos(1), interval));
        assert!(view.tick_renderer_animation(first, interval));
        assert_eq!(
            view.renderer_animation_deadline(first, interval),
            Some(mounted_at + Duration::from_secs(2))
        );
        assert!(view.tick_renderer_animation(mounted_at + Duration::from_secs(2), interval));
        assert_eq!(
            view.renderer_animation_deadline(mounted_at + Duration::from_secs(2), interval),
            Some(mounted_at + Duration::from_secs(3))
        );
        assert!(view.tick_renderer_animation(mounted_at + Duration::from_secs(3), interval));
        assert_eq!(
            view.renderer_animation_deadline(mounted_at + Duration::from_secs(3), interval),
            None,
            "the historical ceil(retryInMs / 1000) tick displays zero once and parks"
        );

        let mut ordinary = AgentView::new();
        ordinary
            .scrollback
            .push(ScrollbackEntry::new(RenderBlock::stub(
                "ordinary",
                Color::White,
            )));
        ordinary.synchronize_api_retry_animation_clock();
        assert_eq!(
            ordinary.renderer_animation_deadline(mounted_at, interval),
            None,
            "ordinary transcript rows do not arm a renderer wake"
        );
    }

    #[test]
    fn minimal_side_question_rebuilds_links_without_resetting_panel_highlight() {
        let mut view = AgentView::new();
        let mut app = app();
        app.set_minimal_mode(true);
        view.enqueue(TranscriptViewAction::StartSideQuestion {
            question: "links".to_string(),
        });
        view.enqueue(TranscriptViewAction::FinishSideQuestion {
            question: "links".to_string(),
            result: Ok("[answer](https://example.com/answer)".to_string()),
        });
        view.apply_pending_actions(&mut app);

        let mut scratch = ScratchBuffer::new();
        let backend = TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut first_hyperlinks = Vec::new();
        terminal
            .draw(|frame| {
                first_hyperlinks = view
                    .draw_prepared(
                        frame,
                        frame.area(),
                        &mut app,
                        CrabCodeTheme::NIGHT,
                        &mut scratch,
                    )
                    .hyperlinks;
            })
            .expect("first panel draw");

        assert!(view.side_question_area.area() > 0);
        assert!(view.side_question_close_rect.area() > 0);
        assert!(!view.visible_link_map.is_empty());
        assert!(first_hyperlinks.iter().any(|span| {
            span.url.as_ref() == "https://example.com/answer"
                && view
                    .side_question_area
                    .contains((span.col_start, span.row).into())
        }));

        view.highlighted_link_idx = Some(0);
        terminal
            .draw(|frame| {
                let outcome = view.draw_prepared(
                    frame,
                    frame.area(),
                    &mut app,
                    CrabCodeTheme::NIGHT,
                    &mut scratch,
                );
                assert!(!outcome.hyperlinks.is_empty());
            })
            .expect("second panel draw");
        assert_eq!(
            view.highlighted_link_idx,
            Some(0),
            "an unchanged minimal panel remains the same keyboard-link owner"
        );
    }

    #[test]
    fn alternate_surface_clear_forces_next_full_link_map_rebuild() {
        let mut view = AgentView::new();
        view.scrollback.push(ScrollbackEntry::new(RenderBlock::stub(
            "stable transcript row",
            Color::Blue,
        )));

        let mut stale_panel_overlay = LinkOverlay::new();
        stale_panel_overlay.push(OverlayLink {
            screen_row: 7,
            col_start: 3,
            col_end: 11,
            target: PagerLinkTarget::Url(Arc::from("https://example.com/stale-panel")),
            presentation: LinkPresentation::Opaque,
            id: Some(91),
        });
        view.visible_link_map.rebuild(
            view.scrollback.generation(),
            &stale_panel_overlay,
            Vec::new(),
        );
        view.scrollback_visible_link_count = view.visible_link_map.len();
        view.highlighted_link_idx = Some(0);

        let mut app = app();
        view.enqueue(TranscriptViewAction::DismissSideQuestion);
        view.apply_pending_actions(&mut app);
        assert!(
            view.visible_links_invalidated,
            "clearing an alternate surface must invalidate links even when the scrollback generation is unchanged"
        );

        let mut scratch = ScratchBuffer::new();
        let backend = TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let outcome = view.draw_prepared(
                    frame,
                    frame.area(),
                    &mut app,
                    CrabCodeTheme::NIGHT,
                    &mut scratch,
                );
                assert!(
                    outcome.hyperlinks.is_empty(),
                    "the next full transcript frame must not emit the stale alternate-surface link"
                );
            })
            .expect("full transcript redraw after alternate surface clear");

        assert!(!view.visible_links_invalidated);
        assert!(
            view.visible_link_map.is_empty(),
            "invalidation must rebuild from the current transcript overlay instead of truncating the stale map"
        );
        assert_eq!(
            view.highlighted_link_idx, None,
            "a highlight owned by the cleared surface must be retired"
        );
    }

    #[test]
    fn side_question_relative_media_link_uses_only_typed_transcript_references() {
        let directory = tempfile::tempdir().expect("temporary session");
        let media_directory = directory.path().join("videos");
        std::fs::create_dir_all(&media_directory).expect("media directory");
        let media_path = media_directory.join("1.mp4");
        std::fs::write(&media_path, b"typed media fixture").expect("media fixture");

        let mut view = AgentView::new();
        view.scrollback
            .push(ScrollbackEntry::new(RenderBlock::ToolCall(
                ToolCallBlock::Other(
                    OtherToolCallBlock::new("video_gen", "saved video")
                        .with_media_ref(&media_path, true),
                ),
            )));
        let mut app = app();
        view.enqueue(TranscriptViewAction::StartSideQuestion {
            question: "where".to_string(),
        });
        view.enqueue(TranscriptViewAction::FinishSideQuestion {
            question: "where".to_string(),
            result: Ok("[clip](videos/1.mp4)".to_string()),
        });
        view.apply_pending_actions(&mut app);

        let mut scratch = ScratchBuffer::new();
        let backend = TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut panel_links = Vec::new();
        terminal
            .draw(|frame| {
                let outcome = view.draw_prepared(
                    frame,
                    frame.area(),
                    &mut app,
                    CrabCodeTheme::NIGHT,
                    &mut scratch,
                );
                panel_links = outcome
                    .hyperlinks
                    .into_iter()
                    .filter(|span| {
                        view.side_question_area
                            .contains((span.col_start, span.row).into())
                    })
                    .collect();
            })
            .expect("draw with media link");

        assert_eq!(view.media_link_paths, vec![media_path]);
        assert!(
            panel_links
                .iter()
                .any(|span| span.url.as_ref().ends_with("/videos/1.mp4")),
            "the panel must resolve only against its transcript's typed media path: {panel_links:?}"
        );
    }

    #[test]
    fn later_painted_hitboxes_drop_only_intersecting_terminal_link_spans() {
        let mut links = vec![
            LinkSpan {
                row: 5,
                col_start: 2,
                col_end: 6,
                url: Arc::from("https://example.com/covered"),
                id: Some(1),
            },
            LinkSpan {
                row: 5,
                col_start: 6,
                col_end: 9,
                url: Arc::from("https://example.com/touching"),
                id: Some(2),
            },
            LinkSpan {
                row: 6,
                col_start: 2,
                col_end: 6,
                url: Arc::from("https://example.com/next-row"),
                id: Some(3),
            },
        ];

        drop_link_spans_intersecting(&mut links, [Rect::new(4, 5, 2, 1)]);

        assert_eq!(
            links.iter().map(|link| link.id).collect::<Vec<_>>(),
            vec![Some(2), Some(3)],
            "half-open row/column boundaries must not suppress adjacent links"
        );
    }

    #[test]
    fn minimal_welcome_is_absent_from_live_viewport_after_native_commit() {
        let mut view = AgentView::new();
        let mut app = app();
        app.set_minimal_mode(true);
        app.mark_minimal_welcome_committed();
        view.prepare(&mut app).expect("empty projection");
        let mut scratch = ScratchBuffer::new();
        let backend = TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                let outcome = view.draw_prepared(
                    frame,
                    frame.area(),
                    &mut app,
                    CrabCodeTheme::NIGHT,
                    &mut scratch,
                );
                assert!(outcome.hyperlinks.is_empty());
            })
            .expect("minimal live viewport");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.trim().is_empty(),
            "the native welcome wordmark and copy must not be repeated in AgentView's live tail: {rendered}"
        );
    }

    #[test]
    fn minimal_hold_and_commit_share_the_draw_owner_state() {
        let mut view = AgentView::new();
        view.scrollback.push(ScrollbackEntry::new(RenderBlock::stub(
            "stable",
            Color::Blue,
        )));
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline test terminal");

        assert!(view.minimal_will_commit(false));
        assert!(
            !view
                .commit_minimal(&mut terminal, false, true)
                .expect("centered hold")
        );
        assert!(
            view.minimal_will_commit(false),
            "held entry remains retryable in the same state"
        );
        assert!(
            view.commit_minimal(&mut terminal, false, false)
                .expect("release commit")
        );
        assert!(!view.minimal_will_commit(false));
        assert_eq!(
            view.scrollback.len(),
            1,
            "draw owner retains committed history"
        );
    }

    #[test]
    fn minimal_expand_reprints_latest_folded_commit_after_hold_releases() {
        let mut view = AgentView::new();
        let id = view
            .scrollback
            .push(ScrollbackEntry::new(RenderBlock::tool_call_with_details(
                "Bash",
                "ran command",
                true,
                "first output line\nsecond output line",
            )));
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("inline test terminal");

        assert!(
            view.commit_minimal(&mut terminal, false, false)
                .expect("folded commit")
        );
        let index = view.scrollback.index_of_id(id).expect("committed entry");
        assert_eq!(
            view.scrollback
                .entry(index)
                .expect("committed entry")
                .display_mode(),
            DisplayMode::Truncated,
        );

        let mut app = app();
        app.set_minimal_mode(true);
        view.enqueue(TranscriptViewAction::MinimalExpandLast);
        view.apply_pending_actions(&mut app);
        assert_eq!(view.pending_minimal_expand, VecDeque::from([id]));
        assert!(
            !view
                .commit_minimal(&mut terminal, false, true)
                .expect("centered modal hold")
        );
        assert_eq!(
            view.pending_minimal_expand,
            VecDeque::from([id]),
            "a held native-scrollback transaction must retain the request"
        );

        assert!(
            view.commit_minimal(&mut terminal, false, false)
                .expect("expanded re-print")
        );
        assert!(view.pending_minimal_expand.is_empty());
        assert_eq!(
            view.scrollback
                .entry(index)
                .expect("expanded committed entry")
                .display_mode(),
            DisplayMode::Expanded,
        );
        assert!(
            !view
                .commit_minimal(&mut terminal, false, false)
                .expect("one-shot expansion"),
            "the same folded commit is not re-printed twice"
        );
    }
}
