//! Renderer-private `Projection` to fixed scrollback adaptation.
//!
//! This module is deliberately not a backend adapter. It consumes only
//! [`ProjectedItem`] values already owned by CrabCode's read-only projection
//! and mutates only renderer state. It sends no request, introduces no wire
//! field, and has no transport or frontend dependency.

use std::collections::{HashMap, HashSet};

use crabcode_pager_render::diff::diff_hunks_from_strings;
use crabcode_pager_render::scrollback::blocks::tool::{
    ExecuteToolCallBlock, ListDirToolCallBlock, OtherToolCallBlock, ReadToolCallBlock,
    SearchInputMeta, SearchOutputMode, SearchToolCallBlock, ToolCallBlock, UseToolCallBlock,
    WebFetchToolCallBlock, WebSearchToolCallBlock,
};
use crabcode_pager_render::scrollback::blocks::{
    CrabCodeAdvisorBlock, CrabCodeAdvisorInvocationState, CrabCodeDiagnostic,
    CrabCodeDiagnosticFile, CrabCodeDiagnosticSeverity, CrabCodeDirectApiError,
    CrabCodeDirectAttachmentBlock, CrabCodeDirectFileContent, CrabCodeDirectProgressBlock,
    CrabCodeDirectSystemBlock, CrabCodeHookPermissionDecision, CrabCodeMessageLevel,
    CrabCodeProjectionBlock, CrabCodeProjectionKind, CrabCodeRelevantMemory, CrabCodeSdkImageBlock,
    CrabCodeSdkImageMediaType, CrabCodeSdkSystemBlock, CrabCodeSdkSystemSubtype,
    CrabCodeSdkSystemTone, CrabCodeTaskStatus, CrabCodeToolBlock, CrabCodeToolPayload,
    CrabCodeToolResultTone, LineRange, SessionEvent, SubagentBlock, SubagentBlockKind,
    WorkflowBlock, WorkflowBlockPhase, WorkflowBlockStatus,
};
use crabcode_pager_render::scrollback::{EntryId, RenderBlock, ScrollbackEntry, ScrollbackState};
use thiserror::Error;

use crate::sdk_projection::{
    AdvisorInvocationState, AdvisorPresentation, AdvisorResultPresentation, AssistantBlockType,
    DirectAttachmentData, DirectDiagnosticSeverity, DirectFileAttachmentContent,
    DirectHookPermissionDecision, DirectNestedMessageKind, DirectProgressPresentation,
    DirectSystemData, DirectTaskStatus, DirectTaskType, DirectUserBlockType,
    DirectWorkflowPhaseState, DirectWorkflowStatus, ImageMediaType, ImageProvenance, ProjectedItem,
    ProjectedKind, ProjectedSystemSubtype, ProjectionItemRemoval, SystemLevel, ThinkingKind,
    ToolPresentation,
};
use crate::sdk_runtime::SystemSubtype;

#[derive(Debug, Clone)]
struct TrackedItem {
    entry_id: EntryId,
    source: ProjectedItem,
    render_options: SynchronizationOptions,
    timeline_segment: usize,
}

#[derive(Debug, Clone)]
struct TrackedRendererNotice {
    entry_id: EntryId,
    text: String,
    placement: RendererNoticePlacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RendererNoticePlacement {
    BootPrefix,
    AfterRawSequence(Option<u64>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RendererNoticeProjection {
    pub(crate) id: u64,
    pub(crate) placement: RendererNoticePlacement,
    pub(crate) text: String,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum PreparedRenderBlock {
    Render(RenderBlock),
    SkipToolResult { diagnostic: String },
}

const DIRECT_NESTED_LIVE_MESSAGE_LIMIT: usize = 3;
const NO_CONTENT_MESSAGE: &str = "(no content)";
const LOCAL_COMMAND_CAVEAT_TAG: &str = "local-command-caveat";
const TICK_TAG: &str = "tick";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SynchronizationOptions {
    /// Historical `verbose`; controls SkillTool's full-message mode and inner
    /// Message rendering. It is deliberately distinct from Ctrl-O transcript.
    pub(crate) presentation_verbose: bool,
    /// Renderer-local equivalent of AgentTool's fixed `isTranscriptMode`.
    pub(crate) agent_transcript_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SynchronizationMode {
    /// One-shot/read-only surfaces may materialize an exact snapshot.
    Snapshot,
    /// The live transcript is an event-driven owner: append/update/finalize
    /// only, with deletion driven by explicit canonical removal effects.
    Lifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectNestedFamily {
    progress_type: String,
    parent_tool_use_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectNestedGroup {
    family: DirectNestedFamily,
    raw_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectNestedFamilyKind {
    Agent,
    Skill,
}

#[derive(Debug)]
struct DirectNestedFamilyPlan {
    kind: DirectNestedFamilyKind,
    first_index: usize,
    last_index: usize,
    anchor_key: String,
    prompt: Option<String>,
    groups: Vec<DirectNestedGroup>,
    visible_groups: HashSet<DirectNestedGroup>,
    first_item_by_group: HashMap<DirectNestedGroup, usize>,
    item_indices_by_group: HashMap<DirectNestedGroup, Vec<usize>>,
    tool_use_groups: HashSet<DirectNestedGroup>,
    hidden_count: usize,
}

#[derive(Debug, Default)]
struct PreparedTranscript {
    rows: Vec<(ProjectedItem, RenderBlock)>,
    diagnostics: Vec<String>,
}

/// Stable renderer identity and lifecycle state for one projected transcript.
///
/// The live path drives native [`ScrollbackState`] entries incrementally and
/// permits renderer-owned lifecycle entries (notably pre-created thinking).
/// Exact snapshot ownership remains available only to read-only surfaces.
#[derive(Debug, Default)]
pub(crate) struct ProjectionScrollbackAdapter {
    renderer_notices: HashMap<u64, TrackedRendererNotice>,
    renderer_notice_order: Vec<u64>,
    runtime_notice_order: Vec<u64>,
    tracked: HashMap<String, TrackedItem>,
    order: Vec<String>,
    entry_order: Vec<EntryId>,
    lifecycle_session_id: Option<String>,
    last_item_removal_id: u64,
    request_generation: Option<u64>,
    request_started_sequence: u64,
    precreated_thinking: Option<EntryId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProjectionScrollbackDelta {
    pub(crate) appended: usize,
    pub(crate) updated: usize,
    pub(crate) removed: usize,
    pub(crate) unchanged: usize,
    pub(crate) reordered: bool,
}

impl std::ops::AddAssign for ProjectionScrollbackDelta {
    fn add_assign(&mut self, rhs: Self) {
        self.appended = self.appended.saturating_add(rhs.appended);
        self.updated = self.updated.saturating_add(rhs.updated);
        self.removed = self.removed.saturating_add(rhs.removed);
        self.unchanged = self.unchanged.saturating_add(rhs.unchanged);
        self.reordered = self.reordered || rhs.reordered;
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum ProjectionScrollbackError {
    #[error("projected item at index {index} has an empty stable key")]
    EmptyKey { index: usize },
    #[error("projected transcript contains duplicate stable key `{key}`")]
    DuplicateKey { key: String },
    #[error("renderer notice projection contains duplicate stable id `{id}`")]
    DuplicateRendererNoticeId { id: u64 },
    #[error("runtime renderer notice anchors are not monotonic at stable id `{id}`")]
    RendererNoticeAnchorOutOfOrder { id: u64 },
    #[error("runtime renderer notices changed or disappeared before the append-only tail")]
    RuntimeRendererNoticeHistoryChanged,
    #[error(
        "projected item `{key}` ({kind:?}) still requires its fixed `{consumer}` renderer consumer"
    )]
    UnclosedConsumer {
        key: String,
        kind: ProjectedKind,
        consumer: &'static str,
    },
    #[error(
        "projected item `{key}` has inconsistent typed presentation: expected {expected}, observed {observed}"
    )]
    InconsistentPresentation {
        key: String,
        expected: &'static str,
        observed: &'static str,
    },
    #[error(
        "scrollback ownership diverged: adapter tracks {tracked} entries but state contains {state}"
    )]
    StateOwnershipDiverged { tracked: usize, state: usize },
    #[error(
        "scrollback ownership diverged at index {index}: adapter expected EntryId {expected}, state contains {observed}"
    )]
    StateOrderDiverged {
        index: usize,
        expected: u64,
        observed: u64,
    },
}

/// Build a renderer-only snapshot for a secondary read-only surface.
///
/// The live transcript keeps its persistent adapter and state in `AgentView`.
/// A session-preview surface has no mutation, streaming, selection, or backend
/// authority to retain between frames, so it may request this one-shot state
/// without exposing `ProjectionScrollbackAdapter` as a second owner.
pub(crate) fn project_scrollback_snapshot(
    items: &[ProjectedItem],
) -> Result<ScrollbackState, ProjectionScrollbackError> {
    let mut adapter = ProjectionScrollbackAdapter::default();
    let mut state = ScrollbackState::new();
    adapter.synchronize(&mut state, items)?;
    Ok(state)
}

impl ProjectionScrollbackAdapter {
    /// Synchronize a complete projected item slice into a dedicated fixed
    /// [`ScrollbackState`].
    ///
    /// Every item is validated and converted before state mutation begins.
    /// Consequently an unclosed consumer, duplicate key, or inconsistent typed
    /// presentation leaves both the scrollback and this identity map unchanged.
    pub(crate) fn synchronize(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        self.synchronize_with_options(state, items, SynchronizationOptions::default())
    }

    pub(crate) fn synchronize_with_options(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
        options: SynchronizationOptions,
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        self.synchronize_with_options_and_notices(state, items, &[], options)
    }

    /// Synchronize the backend-owned projection plus renderer-local notices
    /// into the same persistent scrollback owner.
    ///
    /// Notices are deliberately not converted into [`ProjectedItem`] values:
    /// they are process-local presentation facts, not backend transcript or
    /// wire protocol. Boot diagnostics remain a stable prefix. Runtime
    /// diagnostics form append-only chronological barriers so later backend
    /// rows cannot jump ahead of text already printed into native scrollback.
    pub(crate) fn synchronize_with_options_and_notices(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
        renderer_notices: &[RendererNoticeProjection],
        options: SynchronizationOptions,
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        self.synchronize_with_options_and_optional_notices(
            state,
            items,
            Some(renderer_notices),
            options,
            SynchronizationMode::Snapshot,
        )
    }

    /// Advance the live Grok scrollback lifecycle.
    ///
    /// Unlike [`Self::synchronize_with_options_and_notices`], this path never
    /// treats absence from a later projection snapshot as deletion and never
    /// reorders native entries. Canonical removal effects, stable-key updates,
    /// and explicit turn completion are its only destructive transitions.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advance_lifecycle_with_options_and_notices(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
        renderer_notices: &[RendererNoticeProjection],
        options: SynchronizationOptions,
        item_removals: &[ProjectionItemRemoval],
        session_id: Option<&str>,
        request_generation: u64,
        turn_running: bool,
        request_started_sequence: Option<u64>,
        latest_sequence: u64,
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        let mut delta = self.ensure_lifecycle_session(state, session_id);
        delta += self.apply_explicit_item_removals(state, item_removals);
        delta += self.retire_transient_rows_before(state, latest_sequence);
        delta += self.synchronize_with_options_and_optional_notices(
            state,
            items,
            Some(renderer_notices),
            options,
            SynchronizationMode::Lifecycle,
        )?;
        delta += self.reconcile_turn_lifecycle(
            state,
            request_generation,
            turn_running,
            request_started_sequence,
        );
        Ok(delta)
    }

    /// Synchronize backend projection while a setup-owned centered surface is
    /// active, without materializing, deleting, translating, or re-identifying
    /// renderer notices hidden behind that exclusive surface.
    #[cfg(test)]
    pub(crate) fn synchronize_with_options_preserving_notices(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
        options: SynchronizationOptions,
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        self.synchronize_with_options_and_optional_notices(
            state,
            items,
            None,
            options,
            SynchronizationMode::Snapshot,
        )
    }

    fn synchronize_with_options_and_optional_notices(
        &mut self,
        state: &mut ScrollbackState,
        items: &[ProjectedItem],
        renderer_notices: Option<&[RendererNoticeProjection]>,
        options: SynchronizationOptions,
        mode: SynchronizationMode,
    ) -> Result<ProjectionScrollbackDelta, ProjectionScrollbackError> {
        match mode {
            SynchronizationMode::Snapshot => self.validate_state_ownership(state)?,
            SynchronizationMode::Lifecycle => self.validate_lifecycle_ownership(state)?,
        }
        let retained_notices = renderer_notices.is_none().then(|| {
            self.renderer_notice_order
                .iter()
                .map(|id| {
                    let tracked = self
                        .renderer_notices
                        .get(id)
                        .expect("renderer notice order is ownership-validated");
                    RendererNoticeProjection {
                        id: *id,
                        placement: tracked.placement,
                        text: tracked.text.clone(),
                    }
                })
                .collect::<Vec<_>>()
        });
        let renderer_notices =
            renderer_notices.unwrap_or_else(|| retained_notices.as_deref().unwrap_or(&[]));

        let mut seen = HashSet::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            if item.key.is_empty() {
                return Err(ProjectionScrollbackError::EmptyKey { index });
            }
            if !seen.insert(item.key.clone()) {
                return Err(ProjectionScrollbackError::DuplicateKey {
                    key: item.key.clone(),
                });
            }
        }
        let mut seen_notice_ids = HashSet::with_capacity(renderer_notices.len());
        let mut previous_runtime_anchor: Option<Option<u64>> = None;
        let mut runtime_notices = Vec::new();
        for notice in renderer_notices {
            if !seen_notice_ids.insert(notice.id) {
                return Err(ProjectionScrollbackError::DuplicateRendererNoticeId { id: notice.id });
            }
            let RendererNoticePlacement::AfterRawSequence(anchor) = notice.placement else {
                continue;
            };
            if previous_runtime_anchor.is_some_and(|previous| {
                matches!((previous, anchor), (Some(_), None))
                    || matches!((previous, anchor), (Some(previous), Some(current)) if current < previous)
            }) {
                return Err(
                    ProjectionScrollbackError::RendererNoticeAnchorOutOfOrder { id: notice.id },
                );
            }
            previous_runtime_anchor = Some(anchor);
            runtime_notices.push(notice);
        }
        if runtime_notices.len() < self.runtime_notice_order.len()
            || runtime_notices.iter().zip(&self.runtime_notice_order).any(
                |(notice, expected_id)| {
                    notice.id != *expected_id
                        || self
                            .renderer_notices
                            .get(expected_id)
                            .is_none_or(|tracked| tracked.placement != notice.placement)
                },
            )
        {
            return Err(ProjectionScrollbackError::RuntimeRendererNoticeHistoryChanged);
        }
        let prepared = prepare_transcript(items, options)?;
        for diagnostic in prepared.diagnostics {
            tracing::error!("{diagnostic}");
        }
        let prepared = prepared.rows;

        let selected_before = state.selected();
        let selected_id =
            selected_before.and_then(|index| state.entry(index).map(|entry| entry.id));
        let desired_keys = prepared
            .iter()
            .map(|(item, _)| item.key.clone())
            .collect::<Vec<_>>();
        let desired_key_set = desired_keys.iter().cloned().collect::<HashSet<_>>();
        let mut delta = ProjectionScrollbackDelta::default();

        let desired_notice_ids = renderer_notices
            .iter()
            .map(|notice| notice.id)
            .collect::<HashSet<_>>();
        let removed_notice_ids = match mode {
            SynchronizationMode::Snapshot => self
                .renderer_notices
                .keys()
                .filter(|id| !desired_notice_ids.contains(id))
                .copied()
                .collect::<Vec<_>>(),
            SynchronizationMode::Lifecycle => Vec::new(),
        };
        for id in removed_notice_ids {
            let tracked = self
                .renderer_notices
                .remove(&id)
                .expect("the renderer notice id was collected from the tracked map");
            let removed = state.remove_entry(tracked.entry_id);
            debug_assert!(removed);
            delta.removed = delta.removed.saturating_add(1);
        }
        for notice in renderer_notices {
            let Some(tracked) = self.renderer_notices.get_mut(&notice.id) else {
                let entry_id = state.push(ScrollbackEntry::new(RenderBlock::system(
                    notice.text.clone(),
                )));
                self.renderer_notices.insert(
                    notice.id,
                    TrackedRendererNotice {
                        entry_id,
                        text: notice.text.clone(),
                        placement: notice.placement,
                    },
                );
                delta.appended = delta.appended.saturating_add(1);
                continue;
            };
            if tracked.text == notice.text && tracked.placement == notice.placement {
                delta.unchanged = delta.unchanged.saturating_add(1);
                continue;
            }
            let replaced = state.replace_entry_block_preserving_view(
                tracked.entry_id,
                RenderBlock::system(notice.text.clone()),
                false,
            );
            debug_assert!(replaced);
            tracked.text.clone_from(&notice.text);
            tracked.placement = notice.placement;
            delta.updated = delta.updated.saturating_add(1);
        }

        let removed_keys = match mode {
            SynchronizationMode::Snapshot => self
                .order
                .iter()
                .filter(|key| !desired_key_set.contains(*key))
                .cloned()
                .collect::<Vec<_>>(),
            SynchronizationMode::Lifecycle => self
                .tracked
                .iter()
                .filter(|(key, tracked)| {
                    lifecycle_correlation_retired(key, tracked, &prepared, &desired_key_set)
                })
                .map(|(key, _)| key.clone())
                .collect(),
        };
        for key in removed_keys {
            let tracked = self
                .tracked
                .remove(&key)
                .expect("the ownership preflight proved the tracked entry exists");
            let removed = state.remove_entry(tracked.entry_id);
            debug_assert!(removed);
            delta.removed = delta.removed.saturating_add(1);
        }

        for (item, mut block) in prepared {
            if mode == SynchronizationMode::Lifecycle
                && self.precreated_thinking.is_some()
                && item_is_current_request_output(&item, self.request_started_sequence)
                && !matches!(item.kind, ProjectedKind::Thinking | ProjectedKind::User)
            {
                delta += self.finish_precreated_thinking(state);
            }
            if mode == SynchronizationMode::Lifecycle
                && item.kind == ProjectedKind::Thinking
                && !item.streaming
                && item.text.is_empty()
            {
                if let Some(tracked) = self.tracked.remove(&item.key) {
                    if state.remove_entry(tracked.entry_id) {
                        delta.removed = delta.removed.saturating_add(1);
                    }
                    self.order.retain(|key| key != &item.key);
                }
                delta += self.finish_precreated_thinking(state);
                continue;
            }
            if let Some(tracked) = self.tracked.get_mut(&item.key) {
                if tracked.render_options == options
                    && same_render_projection(&tracked.source, &item)
                {
                    tracked.source = item;
                    delta.unchanged = delta.unchanged.saturating_add(1);
                    continue;
                }
                if tracked.render_options == options
                    && apply_incremental_text_update(
                        state,
                        tracked.entry_id,
                        &tracked.source,
                        &item,
                    )
                {
                    tracked.source = item;
                    delta.updated = delta.updated.saturating_add(1);
                    continue;
                }
                if let Some(previous) = state.get_by_id(tracked.entry_id) {
                    inherit_fixed_component_state(&previous.block, &mut block);
                }
                let replaced = state.replace_entry_block_preserving_view(
                    tracked.entry_id,
                    block,
                    item.streaming,
                );
                debug_assert!(replaced);
                tracked.source = item;
                tracked.render_options = options;
                delta.updated = delta.updated.saturating_add(1);
                continue;
            }

            let entry_id = if mode == SynchronizationMode::Lifecycle
                && item.kind == ProjectedKind::Thinking
                && item.streaming
                && let Some(precreated) = self.precreated_thinking.take()
            {
                let replaced = state.replace_entry_block_preserving_view(precreated, block, true);
                debug_assert!(replaced);
                delta.updated = delta.updated.saturating_add(1);
                precreated
            } else {
                let entry = if item.streaming {
                    ScrollbackEntry::running(block)
                } else {
                    ScrollbackEntry::new(block)
                };
                delta.appended = delta.appended.saturating_add(1);
                state.push(entry)
            };
            self.tracked.insert(
                item.key.clone(),
                TrackedItem {
                    entry_id,
                    timeline_segment: runtime_segment_for_new_item(
                        &item,
                        &runtime_notices,
                        self.runtime_notice_order.len(),
                    ),
                    source: item,
                    render_options: options,
                },
            );
        }

        let current_ids = (0..state.len())
            .map(|index| {
                state
                    .entry(index)
                    .expect("the index is bounded by ScrollbackState::len")
                    .id
            })
            .collect::<Vec<_>>();
        let committed_order = match mode {
            SynchronizationMode::Snapshot => {
                let mut desired_ids = renderer_notices
                    .iter()
                    .filter(|notice| notice.placement == RendererNoticePlacement::BootPrefix)
                    .map(|notice| {
                        self.renderer_notices
                            .get(&notice.id)
                            .expect("every prepared renderer notice was retained")
                            .entry_id
                    })
                    .collect::<Vec<_>>();
                for segment in 0..=runtime_notices.len() {
                    desired_ids.extend(desired_keys.iter().filter_map(|key| {
                        let tracked = self
                            .tracked
                            .get(key)
                            .expect("every prepared item was retained");
                        (tracked.timeline_segment == segment).then_some(tracked.entry_id)
                    }));
                    if let Some(notice) = runtime_notices.get(segment) {
                        desired_ids.push(
                            self.renderer_notices
                                .get(&notice.id)
                                .expect("every prepared runtime notice was retained")
                                .entry_id,
                        );
                    }
                }
                let desired_id_set = desired_ids.iter().copied().collect::<HashSet<_>>();
                let committed_ids = self
                    .entry_order
                    .iter()
                    .copied()
                    .filter(|id| {
                        desired_id_set.contains(id) && state.is_native_scrollback_committed(*id)
                    })
                    .collect::<Vec<_>>();
                if !committed_ids.is_empty() {
                    let committed_id_set = committed_ids.iter().copied().collect::<HashSet<_>>();
                    desired_ids = committed_ids
                        .into_iter()
                        .chain(
                            desired_ids
                                .into_iter()
                                .filter(|id| !committed_id_set.contains(id)),
                        )
                        .collect();
                }
                if current_ids != desired_ids {
                    let reordered = state.reorder_entries_exact(&desired_ids);
                    debug_assert!(reordered);
                    delta.reordered = true;
                }
                self.order = desired_keys;
                desired_ids
            }
            SynchronizationMode::Lifecycle => {
                self.order.retain(|key| self.tracked.contains_key(key));
                for key in desired_keys {
                    if !self.order.contains(&key) {
                        self.order.push(key);
                    }
                }
                current_ids
            }
        };

        if let Some(selected_before) = selected_before {
            if let Some(index) = selected_id.and_then(|id| state.index_of_id(id)) {
                state.set_selected(Some(index));
            } else if !state.is_empty() {
                state.set_selected(Some(selected_before.min(state.len() - 1)));
            }
        }

        self.entry_order = committed_order;
        if mode == SynchronizationMode::Snapshot {
            self.renderer_notice_order = renderer_notices.iter().map(|notice| notice.id).collect();
        } else {
            self.renderer_notice_order
                .retain(|id| self.renderer_notices.contains_key(id));
            for notice in renderer_notices {
                if !self.renderer_notice_order.contains(&notice.id) {
                    self.renderer_notice_order.push(notice.id);
                }
            }
        }
        self.runtime_notice_order = runtime_notices.iter().map(|notice| notice.id).collect();
        Ok(delta)
    }

    pub(crate) fn entry_id(&self, key: &str) -> Option<EntryId> {
        self.tracked.get(key).map(|tracked| tracked.entry_id)
    }

    pub(crate) fn key_for_entry_id(&self, entry_id: EntryId) -> Option<&str> {
        self.tracked
            .iter()
            .find_map(|(key, tracked)| (tracked.entry_id == entry_id).then_some(key.as_str()))
    }

    fn ensure_lifecycle_session(
        &mut self,
        state: &mut ScrollbackState,
        session_id: Option<&str>,
    ) -> ProjectionScrollbackDelta {
        let incoming = session_id.map(str::to_string);
        let changed = self.lifecycle_session_id.is_some()
            && incoming.is_some()
            && self.lifecycle_session_id != incoming;
        if !changed {
            if self.lifecycle_session_id.is_none() {
                self.lifecycle_session_id = incoming;
            }
            return ProjectionScrollbackDelta::default();
        }

        let removed = state.len();
        state.clear();
        *self = Self {
            lifecycle_session_id: incoming,
            ..Self::default()
        };
        ProjectionScrollbackDelta {
            removed,
            ..ProjectionScrollbackDelta::default()
        }
    }

    fn apply_explicit_item_removals(
        &mut self,
        state: &mut ScrollbackState,
        removals: &[ProjectionItemRemoval],
    ) -> ProjectionScrollbackDelta {
        let mut delta = ProjectionScrollbackDelta::default();
        for removal in removals {
            if removal.id <= self.last_item_removal_id {
                continue;
            }
            self.last_item_removal_id = self.last_item_removal_id.max(removal.id);
            let Some(tracked) = self.tracked.remove(&removal.key) else {
                continue;
            };
            if state.remove_entry(tracked.entry_id) {
                delta.removed = delta.removed.saturating_add(1);
            }
            self.order.retain(|key| key != &removal.key);
            self.entry_order.retain(|id| *id != tracked.entry_id);
        }
        delta
    }

    fn retire_transient_rows_before(
        &mut self,
        state: &mut ScrollbackState,
        latest_sequence: u64,
    ) -> ProjectionScrollbackDelta {
        let keys = self
            .tracked
            .iter()
            .filter(|(_, tracked)| direct_api_error_retry_attempt(&tracked.source).is_some())
            .filter(|(_, tracked)| {
                tracked
                    .source
                    .raw_sequences
                    .iter()
                    .copied()
                    .max()
                    .is_some_and(|sequence| sequence < latest_sequence)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let mut delta = ProjectionScrollbackDelta::default();
        for key in keys {
            let Some(tracked) = self.tracked.remove(&key) else {
                continue;
            };
            if state.remove_entry(tracked.entry_id) {
                delta.removed = delta.removed.saturating_add(1);
            }
            self.order.retain(|tracked_key| tracked_key != &key);
            self.entry_order.retain(|id| *id != tracked.entry_id);
        }
        delta
    }

    fn reconcile_turn_lifecycle(
        &mut self,
        state: &mut ScrollbackState,
        request_generation: u64,
        turn_running: bool,
        request_started_sequence: Option<u64>,
    ) -> ProjectionScrollbackDelta {
        let mut delta = ProjectionScrollbackDelta::default();
        if !turn_running {
            delta += self.finish_precreated_thinking(state);
            let running = self
                .tracked
                .values()
                .filter_map(|tracked| {
                    state
                        .get_by_id(tracked.entry_id)
                        .is_some_and(|entry| entry.is_running)
                        .then_some(tracked.entry_id)
                })
                .collect::<Vec<_>>();
            for entry_id in running {
                state.finish_running(entry_id);
                delta.updated = delta.updated.saturating_add(1);
            }
            self.request_generation = None;
            return delta;
        }

        if request_generation == 0 || self.request_generation == Some(request_generation) {
            return delta;
        }

        // A new QueryEngine inference request is a new activity phase inside
        // the same user turn. Finish the prior response/thinking streams, but
        // keep the enclosing turn clock and already completed tools intact.
        delta += self.finish_precreated_thinking(state);
        let prior_streams = self
            .tracked
            .values()
            .filter(|tracked| {
                tracked.source.streaming
                    && matches!(
                        tracked.source.kind,
                        ProjectedKind::Assistant | ProjectedKind::Thinking
                    )
            })
            .filter_map(|tracked| {
                state
                    .get_by_id(tracked.entry_id)
                    .is_some_and(|entry| entry.is_running)
                    .then_some(tracked.entry_id)
            })
            .collect::<Vec<_>>();
        for entry_id in prior_streams {
            state.finish_running(entry_id);
            delta.updated = delta.updated.saturating_add(1);
        }

        self.request_generation = Some(request_generation);
        self.request_started_sequence = request_started_sequence.unwrap_or_default();
        let current_request_has_output = self.tracked.values().any(|tracked| {
            item_is_current_request_output(&tracked.source, self.request_started_sequence)
        });
        if !current_request_has_output
            && self.precreated_thinking.is_none()
            && crabcode_pager_render::appearance::cache::load_show_thinking_blocks()
        {
            let entry_id = state.push(ScrollbackEntry::running(RenderBlock::thinking_streaming()));
            self.precreated_thinking = Some(entry_id);
            delta.appended = delta.appended.saturating_add(1);
        }
        self.entry_order = (0..state.len())
            .filter_map(|index| state.entry(index).map(|entry| entry.id))
            .collect();
        delta
    }

    fn finish_precreated_thinking(
        &mut self,
        state: &mut ScrollbackState,
    ) -> ProjectionScrollbackDelta {
        let Some(entry_id) = self.precreated_thinking.take() else {
            return ProjectionScrollbackDelta::default();
        };
        let empty = state.get_by_id(entry_id).is_some_and(|entry| {
            matches!(&entry.block, RenderBlock::Thinking(thinking) if thinking.text().is_empty())
        });
        if empty && state.remove_entry(entry_id) {
            self.entry_order.retain(|id| *id != entry_id);
            return ProjectionScrollbackDelta {
                removed: 1,
                ..ProjectionScrollbackDelta::default()
            };
        }
        if state
            .get_by_id(entry_id)
            .is_some_and(|entry| entry.is_running)
        {
            state.finish_running(entry_id);
            return ProjectionScrollbackDelta {
                updated: 1,
                ..ProjectionScrollbackDelta::default()
            };
        }
        ProjectionScrollbackDelta::default()
    }

    fn validate_lifecycle_ownership(
        &self,
        state: &ScrollbackState,
    ) -> Result<(), ProjectionScrollbackError> {
        let owned = self
            .tracked
            .values()
            .map(|tracked| tracked.entry_id)
            .chain(self.renderer_notices.values().map(|notice| notice.entry_id))
            .chain(self.precreated_thinking)
            .collect::<Vec<_>>();
        let unique = owned.iter().copied().collect::<HashSet<_>>();
        if unique.len() != owned.len()
            || owned
                .iter()
                .any(|entry_id| state.get_by_id(*entry_id).is_none())
        {
            return Err(ProjectionScrollbackError::StateOwnershipDiverged {
                tracked: owned.len(),
                state: state.len(),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn ordered_keys(&self) -> &[String] {
        &self.order
    }

    fn validate_state_ownership(
        &self,
        state: &ScrollbackState,
    ) -> Result<(), ProjectionScrollbackError> {
        let tracked_count = self.renderer_notices.len().saturating_add(self.order.len());
        let renderer_notice_order_valid = self.renderer_notice_order.len()
            == self.renderer_notices.len()
            && self
                .renderer_notice_order
                .iter()
                .all(|id| self.renderer_notices.contains_key(id))
            && self
                .renderer_notice_order
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len()
                == self.renderer_notice_order.len();
        if state.len() != tracked_count
            || self.tracked.len() != self.order.len()
            || !renderer_notice_order_valid
        {
            return Err(ProjectionScrollbackError::StateOwnershipDiverged {
                tracked: tracked_count,
                state: state.len(),
            });
        }
        if self.entry_order.len() != tracked_count {
            return Err(ProjectionScrollbackError::StateOwnershipDiverged {
                tracked: tracked_count,
                state: state.len(),
            });
        }
        for (index, expected) in self.entry_order.iter().copied().enumerate() {
            let observed = state
                .entry(index)
                .ok_or(ProjectionScrollbackError::StateOwnershipDiverged {
                    tracked: tracked_count,
                    state: state.len(),
                })?
                .id;
            if expected != observed {
                return Err(ProjectionScrollbackError::StateOrderDiverged {
                    index,
                    expected: expected.value(),
                    observed: observed.value(),
                });
            }
        }
        Ok(())
    }
}

fn item_is_current_request_output(item: &ProjectedItem, request_started_sequence: u64) -> bool {
    item.raw_sequences
        .iter()
        .copied()
        .max()
        .is_some_and(|sequence| sequence > request_started_sequence)
        && matches!(
            item.kind,
            ProjectedKind::Assistant
                | ProjectedKind::Thinking
                | ProjectedKind::ToolUse
                | ProjectedKind::ToolResult
                | ProjectedKind::TerminalOutput
        )
}

fn lifecycle_correlation_retired(
    key: &str,
    tracked: &TrackedItem,
    prepared: &[(ProjectedItem, RenderBlock)],
    desired_keys: &HashSet<String>,
) -> bool {
    if desired_keys.contains(key) {
        return false;
    }

    // A result that arrived before its invocation is initially shown as a
    // native Other block. Once an owner with the same tool-use id arrives,
    // retire only that orphan and let the invocation's stable native block own
    // the merged result lifecycle.
    if matches!(
        tracked.source.kind,
        ProjectedKind::ToolResult | ProjectedKind::TerminalOutput
    ) && let Some(tool_use_id) = tracked.source.tool_use_id.as_deref()
        && prepared.iter().any(|(item, _)| {
            item.key != key
                && item.tool_use_id.as_deref() == Some(tool_use_id)
                && item.kind == ProjectedKind::ToolUse
        })
    {
        return true;
    }

    // API retry rows are transient activity records. A later visible event is
    // their explicit lifecycle terminal; retaining every historical retry as
    // a permanent row would diverge from Grok's TurnActivity::Retrying model.
    if direct_api_error_retry_attempt(&tracked.source).is_some() {
        let last_sequence = tracked
            .source
            .raw_sequences
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        return prepared.iter().any(|(item, _)| {
            item.raw_sequences
                .iter()
                .copied()
                .max()
                .is_some_and(|sequence| sequence > last_sequence)
        });
    }

    false
}

fn runtime_segment_for_new_item(
    item: &ProjectedItem,
    runtime_notices: &[&RendererNoticeProjection],
    existing_runtime_notice_count: usize,
) -> usize {
    let Some(first_visible_sequence) = item.raw_sequences.iter().copied().max() else {
        // A production projection row always retains its source sequence.
        // Tests and renderer-only snapshots may construct sequence-free rows;
        // first observation is then the only factual lifecycle boundary.
        return runtime_notices.len();
    };
    runtime_notices
        .iter()
        .filter(|notice| match notice.placement {
            RendererNoticePlacement::BootPrefix => false,
            RendererNoticePlacement::AfterRawSequence(None) => true,
            RendererNoticePlacement::AfterRawSequence(Some(anchor)) => {
                anchor < first_visible_sequence
            }
        })
        .count()
        .max(existing_runtime_notice_count)
}

#[derive(Debug, Clone)]
struct DirectNestedItemInfo {
    family: DirectNestedFamily,
    family_kind: DirectNestedFamilyKind,
    group: DirectNestedGroup,
    message_kind: DirectNestedMessageKind,
    prompt: String,
}

fn direct_nested_item_info(
    item: &ProjectedItem,
) -> Result<Option<DirectNestedItemInfo>, ProjectionScrollbackError> {
    let Some(DirectProgressPresentation::Nested {
        progress_type,
        parent_tool_use_id,
        progress_tool_use_id,
        prompt,
        message_kind,
        ..
    }) = item.presentation.direct_progress.as_ref()
    else {
        return Ok(None);
    };
    let family_kind = match progress_type.as_str() {
        "agent_progress" => DirectNestedFamilyKind::Agent,
        "skill_progress" => DirectNestedFamilyKind::Skill,
        _ => {
            return inconsistent(
                item,
                "fixed agent_progress or skill_progress nested family",
                "unknown nested progress family",
            );
        }
    };
    let [raw_sequence] = item.raw_sequences.as_slice() else {
        return inconsistent(
            item,
            "one outer progress-envelope raw sequence",
            "nested item without exactly one outer-envelope sequence",
        );
    };
    let identity = item
        .presentation
        .direct_progress_identity
        .as_ref()
        .ok_or_else(|| ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "nested progress identity",
            observed: "nested progress without its validated outer identity",
        })?;
    if identity.progress_type != *progress_type
        || identity.parent_tool_use_id != *parent_tool_use_id
        || identity.tool_use_id != *progress_tool_use_id
        || identity.raw_sequence != *raw_sequence
    {
        return inconsistent(
            item,
            "nested presentation and complete outer progress identity agreement",
            "divergent nested family, parent, progress tool, or raw sequence",
        );
    }
    let family = DirectNestedFamily {
        progress_type: progress_type.clone(),
        parent_tool_use_id: parent_tool_use_id.clone(),
    };
    Ok(Some(DirectNestedItemInfo {
        group: DirectNestedGroup {
            family: family.clone(),
            raw_sequence: *raw_sequence,
        },
        family,
        family_kind,
        message_kind: *message_kind,
        prompt: prompt.clone(),
    }))
}

fn build_direct_nested_plans(
    items: &[ProjectedItem],
    _options: SynchronizationOptions,
) -> Result<HashMap<DirectNestedFamily, DirectNestedFamilyPlan>, ProjectionScrollbackError> {
    let mut plans = HashMap::<DirectNestedFamily, DirectNestedFamilyPlan>::new();
    for (index, item) in items.iter().enumerate() {
        let Some(info) = direct_nested_item_info(item)? else {
            continue;
        };
        let plan = plans
            .entry(info.family.clone())
            .or_insert_with(|| DirectNestedFamilyPlan {
                kind: info.family_kind,
                first_index: index,
                last_index: index,
                anchor_key: item.key.clone(),
                // Fixed AgentTool reads only `progressMessages[0]?.data.prompt`.
                // A later non-empty prompt must never be promoted into the
                // transcript header when the first outer envelope had none.
                prompt: (!info.prompt.is_empty()).then(|| info.prompt.clone()),
                groups: Vec::new(),
                visible_groups: HashSet::new(),
                first_item_by_group: HashMap::new(),
                item_indices_by_group: HashMap::new(),
                tool_use_groups: HashSet::new(),
                hidden_count: 0,
            });
        if plan.kind != info.family_kind {
            return inconsistent(
                item,
                "one fixed nested renderer kind per family",
                "nested family changed renderer kind",
            );
        }
        plan.last_index = index;

        // External AgentTool filters every user progress envelope before its
        // processed-message grouping. The marker remains available only for
        // prompt/initializing and completion bookkeeping.
        if plan.kind == DirectNestedFamilyKind::Agent
            && info.message_kind == DirectNestedMessageKind::User
        {
            continue;
        }
        if !plan.first_item_by_group.contains_key(&info.group) {
            plan.first_item_by_group.insert(info.group.clone(), index);
            plan.groups.push(info.group.clone());
        }
        plan.item_indices_by_group
            .entry(info.group.clone())
            .or_default()
            .push(index);
        if item.presentation.assistant_block == Some(AssistantBlockType::ToolUse) {
            plan.tool_use_groups.insert(info.group);
        }
    }

    for plan in plans.values_mut() {
        // CrabCode does not yet expose Grok's separate child-session viewer.
        // Keep every Agent and Skill lifecycle in the main scrollback. Native
        // fold/group state may condense it visually; a snapshot-driven
        // last-three filter must not delete already-owned lifecycle rows.
        let show_all = true;
        let visible_start = if show_all {
            0
        } else {
            plan.groups
                .len()
                .saturating_sub(DIRECT_NESTED_LIVE_MESSAGE_LIMIT)
        };
        plan.visible_groups
            .extend(plan.groups[visible_start..].iter().cloned());
        plan.hidden_count = match plan.kind {
            DirectNestedFamilyKind::Agent => plan.groups[..visible_start]
                .iter()
                .filter(|group| plan.tool_use_groups.contains(*group))
                .count(),
            DirectNestedFamilyKind::Skill => visible_start,
        };
    }
    Ok(plans)
}

fn prepare_transcript(
    items: &[ProjectedItem],
    options: SynchronizationOptions,
) -> Result<PreparedTranscript, ProjectionScrollbackError> {
    let collapsed_items = collapse_labeled_stop_hook_summaries(items);
    let lifecycle_items = coalesce_tool_lifecycles(&collapsed_items);
    let items = lifecycle_items.as_slice();
    let plans = build_direct_nested_plans(items, options)?;
    let mut prepared = PreparedTranscript::default();
    let mut prepared_keys = HashSet::with_capacity(items.len());

    for (index, item) in items.iter().enumerate() {
        // The fixed reorder lifecycle retains an API retry row only when it
        // is the final message. A later message removes every older retry
        // row, while consecutive retries replace one another.
        if direct_api_error_retry_attempt(item).is_some() && index + 1 != items.len() {
            continue;
        }
        if historical_direct_item_is_hidden(item, options) {
            continue;
        }
        let Some(info) = direct_nested_item_info(item)? else {
            push_prepared_render(
                &mut prepared,
                &mut prepared_keys,
                item.clone(),
                prepare_render_block(item)?,
            )?;
            continue;
        };
        let plan = plans.get(&info.family).ok_or_else(|| {
            ProjectionScrollbackError::InconsistentPresentation {
                key: item.key.clone(),
                expected: "one retained family plan for every validated nested item",
                observed: "validated nested item without its family plan",
            }
        })?;

        if index == plan.first_index {
            match plan.kind {
                DirectNestedFamilyKind::Agent => {
                    if let Some(prompt) = plan.prompt.clone() {
                        let source = synthetic_nested_item(
                            item,
                            &plan.anchor_key,
                            "agent-prompt",
                            "Prompt",
                            prompt.clone(),
                        );
                        push_prepared_render(
                            &mut prepared,
                            &mut prepared_keys,
                            source,
                            PreparedRenderBlock::Render(RenderBlock::user_prompt(prompt)),
                        )?;
                    } else if plan.groups.is_empty() {
                        push_agent_initializing(
                            &mut prepared,
                            &mut prepared_keys,
                            item,
                            &plan.anchor_key,
                        )?;
                    }
                }
                DirectNestedFamilyKind::Skill => {}
            }
        }

        let group_is_visible = plan.visible_groups.contains(&info.group);
        match plan.kind {
            DirectNestedFamilyKind::Agent => {
                if info.message_kind == DirectNestedMessageKind::Assistant
                    && group_is_visible
                    && item.kind != ProjectedKind::Progress
                    && direct_nested_inner_is_visible(
                        item,
                        plan.kind,
                        info.message_kind,
                        options.presentation_verbose,
                    )
                {
                    let (source, inner) = prepare_direct_nested_inner(item)?;
                    match inner {
                        PreparedRenderBlock::Render(inner) => {
                            push_prepared_render(
                                &mut prepared,
                                &mut prepared_keys,
                                source,
                                PreparedRenderBlock::Render(inner),
                            )?;
                        }
                        PreparedRenderBlock::SkipToolResult { diagnostic } => {
                            prepared.diagnostics.push(diagnostic);
                        }
                    }
                }
            }
            DirectNestedFamilyKind::Skill => {
                if group_is_visible && plan.first_item_by_group.get(&info.group) == Some(&index) {
                    let group_indices =
                        plan.item_indices_by_group.get(&info.group).ok_or_else(|| {
                            ProjectionScrollbackError::InconsistentPresentation {
                                key: item.key.clone(),
                                expected: "all nested group items retained by outer raw sequence",
                                observed: "visible Skill group without retained item indices",
                            }
                        })?;
                    for group_index in group_indices {
                        let group_item = items.get(*group_index).ok_or_else(|| {
                            ProjectionScrollbackError::InconsistentPresentation {
                                key: item.key.clone(),
                                expected: "nested group index inside projected item slice",
                                observed: "nested group index outside projected item slice",
                            }
                        })?;
                        let group_info = direct_nested_item_info(group_item)?.ok_or_else(|| {
                            ProjectionScrollbackError::InconsistentPresentation {
                                key: group_item.key.clone(),
                                expected: "typed nested presentation for retained group item",
                                observed: "retained group item without nested presentation",
                            }
                        })?;
                        if !direct_nested_inner_is_visible(
                            group_item,
                            plan.kind,
                            group_info.message_kind,
                            options.presentation_verbose,
                        ) {
                            continue;
                        }
                        let (source, inner) = prepare_direct_nested_inner(group_item)?;
                        match inner {
                            PreparedRenderBlock::Render(inner) => push_prepared_render(
                                &mut prepared,
                                &mut prepared_keys,
                                source,
                                PreparedRenderBlock::Render(inner),
                            )?,
                            PreparedRenderBlock::SkipToolResult { diagnostic } => {
                                prepared.diagnostics.push(diagnostic);
                            }
                        }
                    }
                }
            }
        }

        if index == plan.last_index && plan.hidden_count > 0 {
            let text = if plan.hidden_count == 1 {
                "+1 more tool use".to_string()
            } else {
                format!("+{} more tool uses", plan.hidden_count)
            };
            let source = synthetic_nested_item(
                item,
                &plan.anchor_key,
                "hidden-summary",
                "More tool uses",
                text,
            );
            let block = RenderBlock::system(source.text.clone());
            push_prepared_render(
                &mut prepared,
                &mut prepared_keys,
                source,
                PreparedRenderBlock::Render(block),
            )?;
        }
    }
    Ok(prepared)
}

/// Reconcile the commit/progress plane into the invocation that owns a tool
/// lifecycle. The renderer then sees one stable row whose native block moves
/// from running to completed/failed instead of an invocation plus one or more
/// detached SourceNull/result rows.
fn coalesce_tool_lifecycles(items: &[ProjectedItem]) -> Vec<ProjectedItem> {
    let mut coalesced = items.to_vec();
    let mut invocation_indices = HashMap::<String, Vec<usize>>::new();
    for (index, item) in items.iter().enumerate() {
        if item.kind == ProjectedKind::ToolUse
            && item.presentation.tool.is_some()
            && let Some(tool_use_id) = item.tool_use_id.as_ref()
        {
            invocation_indices
                .entry(tool_use_id.clone())
                .or_default()
                .push(index);
        }
    }

    let mut consumed = HashSet::new();
    for (event_index, event) in items.iter().enumerate() {
        if (matches!(
            event.presentation.direct_progress,
            Some(DirectProgressPresentation::Nested { .. })
        ) && event.kind == ProjectedKind::Progress)
            || !matches!(
                event.kind,
                ProjectedKind::Progress | ProjectedKind::ToolResult | ProjectedKind::TerminalOutput
            )
        {
            continue;
        }
        let Some(tool_use_id) = event.tool_use_id.as_ref() else {
            continue;
        };
        let Some(candidates) = invocation_indices.get(tool_use_id) else {
            continue;
        };
        let Some(invocation_index) = candidates
            .iter()
            .copied()
            .rfind(|candidate| *candidate <= event_index)
            .or_else(|| candidates.first().copied())
        else {
            continue;
        };
        let invocation = &mut coalesced[invocation_index];
        let Some(invocation_tool) = invocation.presentation.tool.as_mut() else {
            continue;
        };
        invocation_tool.lifecycle_output = (!event.text.is_empty()).then(|| event.text.clone());
        if let Some(event_tool) = event.presentation.tool.as_ref() {
            if event_tool.result.is_some() {
                invocation_tool.result.clone_from(&event_tool.result);
            }
            if event_tool.is_error.is_some() {
                invocation_tool.is_error = event_tool.is_error;
            }
            if event_tool.lifecycle_output.is_some() {
                invocation_tool
                    .lifecycle_output
                    .clone_from(&event_tool.lifecycle_output);
            }
        }
        for sequence in &event.raw_sequences {
            if !invocation.raw_sequences.contains(sequence) {
                invocation.raw_sequences.push(*sequence);
            }
        }
        if matches!(
            event.kind,
            ProjectedKind::ToolResult | ProjectedKind::TerminalOutput
        ) || matches!(
            event.presentation.direct_progress,
            Some(DirectProgressPresentation::Mcp { ref status, .. })
                if matches!(status.as_str(), "completed" | "failed")
        ) {
            invocation.streaming = false;
        }
        consumed.insert(event_index);
    }

    coalesced
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| (!consumed.contains(&index)).then_some(item))
        .collect()
}

fn direct_system_data(item: &ProjectedItem) -> Option<&DirectSystemData> {
    item.presentation
        .system
        .as_ref()?
        .direct
        .as_ref()
        .map(|direct| &direct.data)
}

pub(crate) fn direct_api_error_retry_attempt(item: &ProjectedItem) -> Option<&serde_json::Number> {
    match direct_system_data(item)? {
        DirectSystemData::ApiError { retry_attempt, .. } => Some(retry_attempt),
        _ => None,
    }
}

fn collapse_labeled_stop_hook_summaries(items: &[ProjectedItem]) -> Vec<ProjectedItem> {
    let mut collapsed = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        let Some(label) = labeled_stop_hook_label(&items[index]) else {
            collapsed.push(items[index].clone());
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        while index < items.len()
            && labeled_stop_hook_label(&items[index]).as_deref() == Some(label.as_str())
        {
            index += 1;
        }
        if index - start == 1 {
            collapsed.push(items[start].clone());
            continue;
        }

        let mut merged = items[start].clone();
        let mut hook_count = 0.0;
        let mut hook_infos = Vec::new();
        let mut hook_errors = Vec::new();
        let mut prevented_continuation = false;
        let mut has_output = false;
        let mut total_duration_ms = 0.0_f64;
        for source in &items[start..index] {
            let Some(DirectSystemData::StopHookSummary {
                hook_count: source_count,
                hook_infos: source_infos,
                hook_errors: source_errors,
                prevented_continuation: source_prevented,
                has_output: source_has_output,
                total_duration_ms: source_duration,
                ..
            }) = direct_system_data(source)
            else {
                unreachable!("the labeled stop-hook grouping predicate proved this variant")
            };
            hook_count += source_count.as_f64().unwrap_or_default();
            hook_infos.extend(source_infos.iter().cloned());
            hook_errors.extend(source_errors.iter().cloned());
            prevented_continuation |= source_prevented;
            has_output |= source_has_output;
            total_duration_ms = total_duration_ms.max(
                source_duration
                    .as_ref()
                    .and_then(serde_json::Number::as_f64)
                    .unwrap_or_default(),
            );
            merged
                .raw_sequences
                .extend(source.raw_sequences.iter().copied());
        }
        merged.raw_sequences.sort_unstable();
        merged.raw_sequences.dedup();
        if let Some(DirectSystemData::StopHookSummary {
            hook_count: merged_count,
            hook_infos: merged_infos,
            hook_errors: merged_errors,
            prevented_continuation: merged_prevented,
            has_output: merged_has_output,
            total_duration_ms: merged_duration,
            ..
        }) = merged
            .presentation
            .system
            .as_mut()
            .and_then(|system| system.direct.as_mut())
            .map(|direct| &mut direct.data)
        {
            *merged_count =
                serde_json::Number::from_f64(hook_count).expect("sum of JSON numbers is finite");
            *merged_infos = hook_infos;
            *merged_errors = hook_errors;
            *merged_prevented = prevented_continuation;
            *merged_has_output = has_output;
            *merged_duration = Some(
                serde_json::Number::from_f64(total_duration_ms)
                    .expect("maximum of JSON numbers is finite"),
            );
        }
        collapsed.push(merged);
    }
    collapsed
}

fn labeled_stop_hook_label(item: &ProjectedItem) -> Option<String> {
    match direct_system_data(item)? {
        DirectSystemData::StopHookSummary {
            hook_label: Some(label),
            ..
        } => Some(label.clone()),
        _ => None,
    }
}

/// Apply the fixed outer user-message visibility filter before constructing a
/// scrollback row.
///
/// The direct runtime's current compile-time KAIROS/KAIROS_CHANNELS facts are
/// both false, so every `isMeta: true` user message is hidden. This includes
/// the synthetic local-command caveat emitted before slash-command results.
/// Transcript-only messages become visible only in the existing Ctrl-O
/// transcript state. Text-component null branches are filtered per projected
/// block so another visible block from the same user envelope is retained.
fn historical_direct_item_is_hidden(item: &ProjectedItem, options: SynchronizationOptions) -> bool {
    if let Some(direct_user) = item.presentation.direct_user.as_ref() {
        if direct_user.is_meta == Some(true) {
            return true;
        }
        if direct_user.is_visible_in_transcript_only == Some(true) && !options.agent_transcript_mode
        {
            return true;
        }
        if direct_user.block_type == DirectUserBlockType::Text
            && direct_user.is_compact_summary != Some(true)
            && direct_user
                .plan_content
                .as_deref()
                .is_none_or(str::is_empty)
            && historical_user_text_is_null(&item.text)
        {
            return true;
        }
    }

    let direct_system = item
        .presentation
        .system
        .as_ref()
        .and_then(|system| system.direct.as_ref());
    if matches!(
        direct_system.map(|system| &system.data),
        Some(DirectSystemData::ApiError { retry_attempt, .. })
            if retry_attempt.as_f64().is_some_and(|attempt| attempt < 4.0)
    ) {
        return true;
    }
    matches!(
        direct_system.map(|system| &system.data),
        Some(DirectSystemData::LocalCommand { content })
            if historical_user_text_is_null(content)
    )
}

fn historical_user_text_is_null(text: &str) -> bool {
    text.trim() == NO_CONTENT_MESSAGE
        || exact_tag_text(text, TICK_TAG).is_some()
        || text.contains(&format!("<{LOCAL_COMMAND_CAVEAT_TAG}>"))
}

fn push_agent_initializing(
    prepared: &mut PreparedTranscript,
    prepared_keys: &mut HashSet<String>,
    item: &ProjectedItem,
    stable_anchor_key: &str,
) -> Result<(), ProjectionScrollbackError> {
    let mut source = synthetic_nested_item(
        item,
        stable_anchor_key,
        "agent-initializing",
        "Initializing",
        "Initializing…".to_string(),
    );
    source.streaming = true;
    let child_session_id = item
        .tool_use_id
        .clone()
        .unwrap_or_else(|| stable_anchor_key.to_string());
    push_prepared_render(
        prepared,
        prepared_keys,
        source,
        PreparedRenderBlock::Render(RenderBlock::Subagent(SubagentBlock::started(
            "Subagent",
            child_session_id,
            "general-purpose",
            None,
            None,
            None,
            false,
        ))),
    )
}

fn direct_nested_inner_is_visible(
    item: &ProjectedItem,
    family_kind: DirectNestedFamilyKind,
    message_kind: DirectNestedMessageKind,
    presentation_verbose: bool,
) -> bool {
    let _ = (item, family_kind, message_kind, presentation_verbose);
    true
}

fn synthetic_nested_item(
    source_item: &ProjectedItem,
    stable_anchor_key: &str,
    suffix: &str,
    title: &str,
    text: String,
) -> ProjectedItem {
    let mut source = source_item.clone();
    source.key = format!("{stable_anchor_key}::{suffix}");
    source.kind = ProjectedKind::System;
    source.title = title.to_string();
    source.text = text;
    source.streaming = false;
    source
}

fn prepare_direct_nested_inner(
    item: &ProjectedItem,
) -> Result<(ProjectedItem, PreparedRenderBlock), ProjectionScrollbackError> {
    let mut inner = item.clone();
    inner.presentation.direct_progress = None;
    inner.presentation.direct_progress_identity = None;
    let prepared = if inner.kind == ProjectedKind::Progress {
        PreparedRenderBlock::Render(RenderBlock::system(""))
    } else {
        prepare_render_block(&inner)?
    };
    // Stable identity and revision comparisons stay attached to the original
    // projected row; only the block conversion consumes the stripped clone.
    Ok((item.clone(), prepared))
}

fn push_prepared_render(
    prepared: &mut PreparedTranscript,
    seen: &mut HashSet<String>,
    source: ProjectedItem,
    block: PreparedRenderBlock,
) -> Result<(), ProjectionScrollbackError> {
    match block {
        PreparedRenderBlock::Render(block) => {
            if !seen.insert(source.key.clone()) {
                return Err(ProjectionScrollbackError::DuplicateKey { key: source.key });
            }
            prepared.rows.push((source, block));
        }
        PreparedRenderBlock::SkipToolResult { diagnostic } => {
            prepared.diagnostics.push(diagnostic);
        }
    }
    Ok(())
}

/// Contain the only historical dynamic tool-result renderer failure at the
/// equivalent production item boundary.
///
/// The fixed historical TUI invoked a JavaScript tool-owned callback and
/// converted a synchronous callback throw into a logged `null`. This renderer
/// has no dynamic callback: untrusted durable results first enter the closed
/// [`ToolPresentation`] carrier and all production painting is an exhaustive
/// Rust enum match. The remaining fallible boundary is therefore this typed
/// conversion. A failure for a named ordinary result is logged with the tool
/// name and original error and removes only that result row. Its independently
/// projected invocation remains in `prepared`, matching the historical
/// collapsed-call-site behavior without attempting to catch arbitrary Rust
/// panics under the release `panic=abort` profile.
fn prepare_render_block(
    item: &ProjectedItem,
) -> Result<PreparedRenderBlock, ProjectionScrollbackError> {
    match render_block_for(item) {
        Ok(block) => Ok(PreparedRenderBlock::Render(block)),
        Err(error) => {
            let Some(tool_name) = degradable_tool_result_name(item) else {
                return Err(error);
            };
            Ok(PreparedRenderBlock::SkipToolResult {
                diagnostic: format!("Error rendering tool result for {tool_name}: {error}"),
            })
        }
    }
}

fn degradable_tool_result_name(item: &ProjectedItem) -> Option<&str> {
    matches!(
        item.kind,
        ProjectedKind::ToolResult | ProjectedKind::TerminalOutput
    )
    .then_some(())?;
    item.presentation
        .tool
        .as_ref()?
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
}

fn same_render_projection(left: &ProjectedItem, right: &ProjectedItem) -> bool {
    left.key == right.key
        && left.kind == right.kind
        && left.title == right.title
        && left.text == right.text
        && left.streaming == right.streaming
        && left.tool_use_id == right.tool_use_id
        && left.presentation == right.presentation
}

/// Preserve the fixed streaming block's internal markdown/timing lifecycle
/// when a same-key text stream grows cumulatively. A corrected/non-cumulative
/// final frame returns `false` and takes the exact in-place replacement path.
fn apply_incremental_text_update(
    state: &mut ScrollbackState,
    entry_id: EntryId,
    previous: &ProjectedItem,
    next: &ProjectedItem,
) -> bool {
    if !previous.streaming || previous.kind != next.kind || !next.text.starts_with(&previous.text) {
        return false;
    }
    let suffix = &next.text[previous.text.len()..];
    let appended = match next.kind {
        ProjectedKind::Assistant => {
            suffix.is_empty() || state.push_chunk_to_agent(entry_id, suffix)
        }
        ProjectedKind::Thinking => {
            suffix.is_empty() || state.push_chunk_to_thinking(entry_id, suffix)
        }
        ProjectedKind::User
        | ProjectedKind::ToolUse
        | ProjectedKind::ToolResult
        | ProjectedKind::TerminalOutput
        | ProjectedKind::System
        | ProjectedKind::Progress
        | ProjectedKind::Warning
        | ProjectedKind::Error => return false,
    };
    if !appended {
        return false;
    }
    if !next.streaming {
        state.finish_running(entry_id);
    }
    true
}

/// Construct only blocks whose current typed projection is sufficient for the
/// fixed renderer without reconstructing semantics from `Value`, a title, or
/// fallback prose. Title/text remain eligible as display payload after the
/// block class has been proven independently; they never select that class.
fn render_block_for(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.stream_error.is_some() {
        return map_stream_error(item);
    }
    if item.presentation.plain_system {
        return map_plain_system(item);
    }
    if item.presentation.system.is_some() {
        return map_system(item);
    }
    if item.presentation.advisor.is_some() {
        return map_advisor(item);
    }
    if item.presentation.direct_progress.is_some() {
        return map_direct_progress(item);
    }
    if item.presentation.direct_attachment.is_some() {
        return map_direct_attachment(item);
    }
    if item.presentation.image.is_some() {
        return map_image(item);
    }
    if item.presentation.tool.is_some() {
        return map_tool(item);
    }

    match item.kind {
        ProjectedKind::User => map_user(item),
        ProjectedKind::Assistant => map_assistant(item),
        ProjectedKind::Thinking => map_thinking(item),
        // `RenderBlock::System` could represent an independently proven,
        // untyped generic notice. The current Projection has no such System
        // producer: SDK/historical systems carry `presentation.system`, while
        // historical attachment notices carry `direct_attachment`. Admit none
        // until a concrete source is proven rather than inferring one from
        // title/text.
        ProjectedKind::System => unclosed(item, "untyped system"),
        ProjectedKind::ToolUse => unclosed(item, "tool-use"),
        ProjectedKind::ToolResult => unclosed(item, "tool-result"),
        ProjectedKind::TerminalOutput => unclosed(item, "terminal-output"),
        ProjectedKind::Progress => unclosed(item, "progress"),
        ProjectedKind::Warning => unclosed(item, "warning"),
        ProjectedKind::Error => unclosed(item, "error"),
    }
}

fn map_stream_error(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.kind != ProjectedKind::Error || item.streaming {
        return inconsistent(
            item,
            "completed in-stream error row",
            "stream error on a non-error or streaming row",
        );
    }
    let presentation = item.presentation.stream_error.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed in-stream error presentation",
            observed: "missing in-stream error presentation",
        }
    })?;
    if item.presentation.plain_system
        || item.presentation.system.is_some()
        || item.presentation.advisor.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_attachment.is_some()
        || item.presentation.image.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.assistant_block.is_some()
        || item.presentation.direct_user.is_some()
        || item.presentation.direct_assistant.is_some()
        || item.presentation.direct_progress_identity.is_some()
    {
        return inconsistent(
            item,
            "standalone in-stream error presentation",
            "stream error combined with another specialized presentation",
        );
    }
    let detail = match (&presentation.error_type, &presentation.error_code) {
        (Some(kind), Some(code)) => format!("{} [{kind}; {code}]", presentation.message),
        (Some(kind), None) => format!("{} [{kind}]", presentation.message),
        (None, Some(code)) => format!("{} [{code}]", presentation.message),
        (None, None) => presentation.message.clone(),
    };
    Ok(RenderBlock::session_event(SessionEvent::TurnFailed {
        error: detail,
        elapsed: None,
    }))
}

fn map_plain_system(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.kind != ProjectedKind::System {
        return inconsistent(
            item,
            "producer-classified plain system row",
            "plain-system marker on a non-system row",
        );
    }
    let presentation = &item.presentation;
    if presentation.assistant_block.is_some()
        || presentation.direct_assistant.is_some()
        || presentation.direct_user.is_some()
        || presentation.direct_progress_identity.is_some()
        || presentation.system.is_some()
        || presentation.tool.is_some()
        || presentation.thinking.is_some()
        || presentation.image.is_some()
        || presentation.advisor.is_some()
        || presentation.direct_progress.is_some()
        || presentation.direct_attachment.is_some()
    {
        return inconsistent(
            item,
            "plain system without a specialized presentation",
            "plain system combined with specialized presentation state",
        );
    }
    Ok(RenderBlock::system(item.text.clone()))
}

fn projection_block(kind: CrabCodeProjectionKind) -> RenderBlock {
    RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock::new(kind))
}

fn inherit_fixed_component_state(previous: &RenderBlock, replacement: &mut RenderBlock) {
    let (RenderBlock::CrabCodeProjection(previous), RenderBlock::CrabCodeProjection(replacement)) =
        (previous, replacement)
    else {
        return;
    };
    replacement.inherit_turn_duration_component_state(previous);
}

fn map_advisor(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_attachment.is_some()
        || item.presentation.image.is_some()
    {
        return inconsistent(
            item,
            "typed advisor presentation",
            "advisor combined with a different specialized presentation",
        );
    }
    let advisor = item.presentation.advisor.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed advisor presentation",
            observed: "missing advisor presentation",
        }
    })?;
    let block = match advisor {
        AdvisorPresentation::Invocation { input, state } => {
            require_advisor_invocation_kind(item)?;
            CrabCodeAdvisorBlock::Invocation {
                input: payload_from_input(input),
                state: match state {
                    AdvisorInvocationState::InProgress => {
                        CrabCodeAdvisorInvocationState::InProgress
                    }
                    AdvisorInvocationState::Succeeded => CrabCodeAdvisorInvocationState::Succeeded,
                    AdvisorInvocationState::Failed => CrabCodeAdvisorInvocationState::Failed,
                },
            }
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Feedback { text }) => {
            require_advisor_result_kind(item)?;
            CrabCodeAdvisorBlock::Feedback { text: text.clone() }
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Redacted) => {
            require_advisor_result_kind(item)?;
            CrabCodeAdvisorBlock::Redacted
        }
        AdvisorPresentation::Result(AdvisorResultPresentation::Error { error_code }) => {
            require_advisor_result_kind(item)?;
            CrabCodeAdvisorBlock::Error {
                error_code: error_code.clone(),
            }
        }
    };
    Ok(projection_block(CrabCodeProjectionKind::Advisor(block)))
}

fn require_advisor_invocation_kind(item: &ProjectedItem) -> Result<(), ProjectionScrollbackError> {
    if item.kind != ProjectedKind::ToolUse {
        return inconsistent(
            item,
            "advisor invocation tool-use row",
            "non-tool-use advisor invocation row",
        );
    }
    Ok(())
}

fn require_advisor_result_kind(item: &ProjectedItem) -> Result<(), ProjectionScrollbackError> {
    if item.kind != ProjectedKind::ToolResult {
        return inconsistent(
            item,
            "advisor result tool-result row",
            "non-tool-result advisor result row",
        );
    }
    Ok(())
}

fn map_tool(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.advisor.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_attachment.is_some()
        || item.presentation.image.is_some()
        || item.presentation.thinking.is_some()
    {
        return inconsistent(
            item,
            "ordinary typed tool presentation",
            "tool combined with a different specialized presentation",
        );
    }
    if item.presentation.direct_progress_identity.is_some() {
        return inconsistent(
            item,
            "ordinary typed tool without direct-progress identity",
            "tool retained a direct-progress identity without its specialized payload",
        );
    }
    let tool = item.presentation.tool.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed tool presentation",
            observed: "missing tool presentation",
        }
    })?;

    let block = match item.kind {
        ProjectedKind::ToolUse => return map_tool_invocation(item, tool),
        ProjectedKind::ToolResult | ProjectedKind::TerminalOutput => {
            let Some(block) = map_tool_result(item, tool)? else {
                let mut fallback = OtherToolCallBlock::new(
                    tool.name.as_deref().unwrap_or("Uncorrelated tool result"),
                    "",
                )
                .with_output(item.text.clone());
                if tool.is_error == Some(true) {
                    fallback = fallback.with_error(if item.text.is_empty() {
                        "Tool result reported an error".to_string()
                    } else {
                        item.text.clone()
                    });
                }
                return Ok(RenderBlock::ToolCall(ToolCallBlock::Other(fallback)));
            };
            let CrabCodeToolBlock::Result {
                name,
                result,
                is_error,
                ..
            } = block
            else {
                unreachable!("map_tool_result returns only result blocks")
            };
            let output = tool_payload_text(result);
            let mut fallback = OtherToolCallBlock::new(name, "result");
            if !output.is_empty() {
                fallback = fallback.with_output(output.clone());
            }
            if is_error == Some(true) {
                fallback = fallback.with_error(if output.is_empty() {
                    "Tool result reported an error".to_string()
                } else {
                    output
                });
            }
            return Ok(RenderBlock::ToolCall(ToolCallBlock::Other(fallback)));
        }
        ProjectedKind::Progress => map_tool_progress(item, tool)?,
        ProjectedKind::User
        | ProjectedKind::Assistant
        | ProjectedKind::Thinking
        | ProjectedKind::System
        | ProjectedKind::Warning
        | ProjectedKind::Error => {
            return inconsistent(
                item,
                "tool-use, tool-result, terminal-output, or progress row",
                "typed tool on an incompatible projected kind",
            );
        }
    };
    Ok(projection_block(CrabCodeProjectionKind::Tool(block)))
}

fn tool_payload_text(payload: CrabCodeToolPayload) -> String {
    match payload {
        CrabCodeToolPayload::Json(text)
        | CrabCodeToolPayload::Text(text)
        | CrabCodeToolPayload::PartialJson(text) => text,
        CrabCodeToolPayload::Null => "null".to_string(),
        CrabCodeToolPayload::Missing => String::new(),
    }
}

fn map_tool_invocation(
    item: &ProjectedItem,
    tool: &ToolPresentation,
) -> Result<RenderBlock, ProjectionScrollbackError> {
    let name = tool
        .name
        .as_ref()
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            if item.title.is_empty() {
                "Unknown tool".to_string()
            } else {
                item.title.clone()
            }
        });
    if let Some(input) = tool.input.as_ref()
        && let Some(block) = map_native_tool_lifecycle(item, tool, &name, input)
    {
        return Ok(block);
    }

    // Malformed, partial, or genuinely product-specific payloads remain
    // visible in the native Other block. They are local presentation
    // failures, never a reason to stop the backend turn.
    let summary = tool.input.as_ref().map_or_else(
        || tool.partial_input_json.clone().unwrap_or_default(),
        compact_json,
    );
    let mut fallback = OtherToolCallBlock::new(name, summary);
    if let Some(output) = native_tool_output(tool) {
        fallback = fallback.with_output(output.clone());
        if tool.is_error == Some(true) {
            fallback = fallback.with_error(output);
        }
    } else if tool.is_error == Some(true) {
        fallback = fallback.with_error("Tool call failed");
    }
    Ok(RenderBlock::ToolCall(ToolCallBlock::Other(fallback)))
}

fn map_native_tool_lifecycle(
    item: &ProjectedItem,
    tool: &ToolPresentation,
    name: &str,
    input: &serde_json::Value,
) -> Option<RenderBlock> {
    let normalized = name.to_ascii_lowercase();
    let output = native_tool_output(tool);
    let error = (tool.is_error == Some(true)).then(|| {
        output
            .clone()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "Tool call failed".to_string())
    });

    if matches!(
        normalized.as_str(),
        "bash" | "execute" | "shell" | "shell_command" | "run_command"
    ) {
        let command = input_string(input, &["command", "cmd"])?;
        let mut block = ExecuteToolCallBlock::new(command);
        if let Some(description) = input_string(input, &["description"]) {
            block = block.with_description(description);
        }
        if let Some(output) = output {
            block = block.with_output(output);
        }
        if let Some(error) = error {
            block = block.with_error(error);
        }
        return Some(RenderBlock::ToolCall(ToolCallBlock::Execute(block)));
    }

    if matches!(normalized.as_str(), "read" | "read_file") {
        let path = input_string(input, &["file_path", "path"])?;
        let mut block = ReadToolCallBlock::new(path);
        if let Some(offset) = input_u64(input, &["offset", "start_line"]) {
            let start = usize::try_from(offset.max(1)).ok()?;
            let end = input_u64(input, &["limit", "line_count"])
                .and_then(|count| usize::try_from(count).ok())
                .map_or(start, |count| start.saturating_add(count.saturating_sub(1)));
            block = block.with_line_range(LineRange::new(start, end));
        }
        if let Some(content) = output {
            let total_lines = content.lines().count();
            block = block.with_content(content, total_lines);
        }
        if let Some(error) = error {
            block = block.with_error(error);
        }
        return Some(RenderBlock::ToolCall(ToolCallBlock::Read(block)));
    }

    if matches!(normalized.as_str(), "edit" | "replace" | "write") {
        let path = input_string(input, &["file_path", "path"])?;
        if let Some(error) = error {
            return Some(RenderBlock::edit_failed(path, error));
        }
        return map_exact_file_edit_invocation(name, input);
    }

    if matches!(
        normalized.as_str(),
        "grep" | "glob" | "search" | "search_files" | "find"
    ) {
        let pattern = input_string(input, &["pattern", "query", "glob"])?;
        let mut block = SearchToolCallBlock::new(pattern);
        block.meta = SearchInputMeta {
            path: input_string(input, &["path"]),
            glob: input_string(input, &["glob"]),
            output_mode: if normalized == "glob" {
                SearchOutputMode::FilesWithMatches
            } else {
                SearchOutputMode::from_str_opt(
                    input.get("output_mode").and_then(serde_json::Value::as_str),
                )
            },
            case_insensitive: input
                .get("-i")
                .or_else(|| input.get("case_insensitive"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            file_type: input_string(input, &["type"]),
            multiline: input
                .get("multiline")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        };
        if let Some(paths) = tool.result.as_ref().and_then(value_string_array) {
            block.match_count = paths.len();
            block.file_paths = paths;
        } else if let Some(output) = output.as_ref().filter(|output| !output.is_empty()) {
            block.match_count = output.lines().count();
        }
        if let Some(error) = error {
            block = block.with_error(error);
        }
        return Some(RenderBlock::ToolCall(ToolCallBlock::Search(block)));
    }

    if matches!(
        normalized.as_str(),
        "listdir" | "list_dir" | "ls" | "directory_list"
    ) {
        let path = input_string(input, &["path", "directory"]).unwrap_or_else(|| ".".to_string());
        let mut block = ListDirToolCallBlock::new(path);
        if let Some(output) = output {
            block = block.with_output(output);
        }
        if let Some(error) = error {
            block = block.with_error(error);
        }
        return Some(RenderBlock::ToolCall(ToolCallBlock::ListDir(block)));
    }

    if matches!(
        normalized.as_str(),
        "websearch" | "web_search" | "search_web"
    ) {
        let query = input_string(input, &["query", "search_query"])?;
        let mut block = WebSearchToolCallBlock::new(query);
        block.content = output;
        if let Some(result) = tool.result.as_ref() {
            collect_urls(result, &mut block.citations);
            block.citations.sort();
            block.citations.dedup();
        }
        block.error = error;
        return Some(RenderBlock::ToolCall(ToolCallBlock::WebSearch(block)));
    }

    if matches!(normalized.as_str(), "webfetch" | "web_fetch" | "fetch") {
        let url = input_string(input, &["url", "uri"])?;
        let mut block = WebFetchToolCallBlock::new(url);
        block.output = output;
        if let Some(result) = tool.result.as_ref().and_then(serde_json::Value::as_object) {
            block.status_code = result
                .get("status")
                .or_else(|| result.get("status_code"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok());
            block.content_type = result
                .get("content_type")
                .or_else(|| result.get("contentType"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            block.bytes = result
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .and_then(|bytes| usize::try_from(bytes).ok());
        }
        block.error = error;
        return Some(RenderBlock::ToolCall(ToolCallBlock::WebFetch(block)));
    }

    if normalized == "agent" {
        let description = input_string(input, &["description", "prompt", "task"])
            .unwrap_or_else(|| "Subagent task".to_string());
        let child_id = item.tool_use_id.clone().unwrap_or_else(|| item.key.clone());
        let is_background = input
            .get("run_in_background")
            .or_else(|| input.get("background"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut block = SubagentBlock::started(
            description,
            child_id,
            input_string(input, &["subagent_type", "agent_type"])
                .unwrap_or_else(|| "general-purpose".to_string()),
            input_string(input, &["persona"]),
            input_string(input, &["role"]),
            input_string(input, &["model"]),
            is_background,
        );
        block.activity_label = item
            .streaming
            .then(|| output.as_deref()?.lines().next().map(str::to_string))
            .flatten();
        if !item.streaming {
            block.kind = if let Some(error) = error {
                if error.to_ascii_lowercase().contains("cancel") {
                    SubagentBlockKind::Cancelled {
                        elapsed: std::time::Duration::ZERO,
                    }
                } else {
                    SubagentBlockKind::Failed {
                        elapsed: std::time::Duration::ZERO,
                        error: Some(error),
                    }
                }
            } else {
                SubagentBlockKind::Completed {
                    elapsed: std::time::Duration::ZERO,
                }
            };
        }
        return Some(RenderBlock::Subagent(block));
    }

    if normalized == "skill" {
        let summary = input_string(input, &["skill", "name", "command"])
            .unwrap_or_else(|| compact_json(input));
        let mut block = OtherToolCallBlock::new("Skill", summary);
        if let Some(output) = output {
            block = block.with_output(output);
        }
        if let Some(error) = error {
            block = block.with_error(error);
        }
        return Some(RenderBlock::ToolCall(ToolCallBlock::Skill(block)));
    }

    if normalized.starts_with("mcp__")
        || item.presentation.assistant_block == Some(AssistantBlockType::McpToolUse)
    {
        let qualified = name.strip_prefix("mcp__").unwrap_or(name);
        let mut block = UseToolCallBlock::new(qualified);
        if let Some(object) = input.as_object() {
            block.input_args = object
                .iter()
                .map(|(key, value)| (key.clone(), inline_json_value(value)))
                .collect();
        }
        block.output = output;
        block.error = error;
        return Some(RenderBlock::ToolCall(ToolCallBlock::UseTool(block)));
    }

    None
}

fn native_tool_output(tool: &ToolPresentation) -> Option<String> {
    tool.lifecycle_output
        .clone()
        .or_else(|| tool.result.as_ref().map(result_value_text))
}

fn result_value_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(object) = value.as_object() {
        let text_parts = ["stdout", "stderr", "output", "content", "text"]
            .into_iter()
            .filter_map(|key| object.get(key).and_then(serde_json::Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        if !text_parts.is_empty() {
            return text_parts.join("\n");
        }
    }
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unrenderable tool result>".to_string())
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_string())
}

fn inline_json_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| compact_json(value))
}

fn input_string(input: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        input
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn input_u64(input: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_u64))
}

fn value_string_array(value: &serde_json::Value) -> Option<Vec<String>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect()
    })
}

fn collect_urls(value: &serde_json::Value, urls: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value)
            if value.starts_with("https://") || value.starts_with("http://") =>
        {
            urls.push(value.clone());
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_urls(value, urls);
            }
        }
        serde_json::Value::Object(object) => {
            for value in object.values() {
                collect_urls(value, urls);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

/// Route the one historical direct-TUI tool presentation whose typed input
/// exactly matches a fixed Rust render block.
///
/// This is deliberately a closed structural match. The renderer does not
/// inspect the filesystem, infer aliases, parse display text, or accept
/// partially streamed JSON. Any missing/wrongly typed field falls back to the
/// lossless generic tool carrier.
fn map_exact_file_edit_invocation(name: &str, input: &serde_json::Value) -> Option<RenderBlock> {
    if name != "Edit" {
        return None;
    }
    let input = input.as_object()?;
    let path = input.get("file_path")?.as_str()?;
    let old_text = input.get("old_string")?.as_str()?;
    let new_text = input.get("new_string")?.as_str()?;
    Some(RenderBlock::edit_with_hunks(
        path,
        diff_hunks_from_strings(old_text, new_text, 1),
    ))
}

fn map_tool_result(
    item: &ProjectedItem,
    tool: &ToolPresentation,
) -> Result<Option<CrabCodeToolBlock>, ProjectionScrollbackError> {
    if tool.input.is_some() || tool.partial_input_json.is_some() {
        return inconsistent(
            item,
            "ordinary tool result without invocation input",
            "tool result carrying invocation input fields",
        );
    }
    let Some(name) = tool.name.as_ref().filter(|name| !name.is_empty()) else {
        // Exact historical `UserToolResultMessage`: a missing preceding
        // tool-use lookup returns `null`; it does not invent a generic label.
        return Ok(None);
    };
    let result = if let Some(direct_user) = item
        .presentation
        .direct_user
        .as_ref()
        .filter(|direct_user| direct_user.block_type == DirectUserBlockType::ToolResult)
    {
        if tool.is_error == Some(true) {
            // The fixed error branch consumes the tool-result block content,
            // not the successful envelope-level `toolUseResult`.
            tool.result
                .as_ref()
                .map_or(CrabCodeToolPayload::Missing, payload_from_result)
        } else {
            let Some(renderer_result) = direct_user
                .tool_use_result
                .as_ref()
                .filter(|result| json_value_is_truthy(result))
            else {
                // Exact fixed `UserToolSuccessMessage`: no truthy
                // `message.toolUseResult` means no result row.
                return Ok(None);
            };
            if tool.result.as_ref() != Some(renderer_result) {
                return inconsistent(
                    item,
                    "matching direct toolUseResult carrier",
                    "tool result diverged from direct user presentation",
                );
            }
            payload_from_result(renderer_result)
        }
    } else {
        tool.result
            .as_ref()
            .map_or(CrabCodeToolPayload::Missing, payload_from_result)
    };
    Ok(Some(CrabCodeToolBlock::Result {
        name: name.clone(),
        result,
        is_error: tool.is_error,
        tone: if item.kind == ProjectedKind::TerminalOutput {
            CrabCodeToolResultTone::Terminal
        } else {
            CrabCodeToolResultTone::Result
        },
    }))
}

fn json_value_is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

fn map_tool_progress(
    item: &ProjectedItem,
    tool: &ToolPresentation,
) -> Result<CrabCodeToolBlock, ProjectionScrollbackError> {
    if item.presentation.assistant_block.is_some()
        || item.presentation.direct_user.is_some()
        || item.presentation.direct_assistant.is_some()
        || tool.input.is_some()
        || tool.partial_input_json.is_some()
        || tool.result.is_some()
        || tool.is_error.is_some()
    {
        return inconsistent(
            item,
            "SDK tool_progress presentation",
            "tool progress carrying content-block or invocation/result fields",
        );
    }
    let name = tool
        .name
        .as_ref()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "SDK tool_progress name",
            observed: "missing/empty tool progress name",
        })?
        .clone();
    Ok(CrabCodeToolBlock::Progress {
        name,
        // The class comes exclusively from `ToolPresentation` + Progress.
        // `text` is the already-projected display payload after that class is
        // established; it never participates in dispatch.
        detail: item.text.clone(),
    })
}

fn payload_from_input(value: &serde_json::Value) -> CrabCodeToolPayload {
    match value {
        serde_json::Value::Null => CrabCodeToolPayload::Null,
        _ => CrabCodeToolPayload::Json(
            serde_json::to_string_pretty(value)
                .expect("serde_json::Value serialization is infallible"),
        ),
    }
}

fn payload_from_result(value: &serde_json::Value) -> CrabCodeToolPayload {
    match value {
        serde_json::Value::Null => CrabCodeToolPayload::Null,
        serde_json::Value::String(text) => CrabCodeToolPayload::Text(text.clone()),
        _ => CrabCodeToolPayload::Json(
            serde_json::to_string_pretty(value)
                .expect("serde_json::Value serialization is infallible"),
        ),
    }
}

fn map_image(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.kind != ProjectedKind::User {
        return inconsistent(item, "direct user image", "non-user image projection");
    }
    if item.presentation.system.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.advisor.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_progress_identity.is_some()
        || item.presentation.direct_attachment.is_some()
        || item.presentation.assistant_block.is_some()
        || item.presentation.direct_assistant.is_some()
    {
        return inconsistent(
            item,
            "direct user image presentation",
            "image combined with a different specialized presentation",
        );
    }
    if let Some(direct_user) = item.presentation.direct_user.as_ref() {
        if direct_user.block_type != DirectUserBlockType::Image {
            return inconsistent(
                item,
                "direct user image block",
                "non-image direct user block with image provenance",
            );
        }
        return Ok(projection_block(CrabCodeProjectionKind::UserImage {
            image_id: direct_user.render_image_id,
        }));
    }
    let image = item.presentation.image.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed SDK image provenance",
            observed: "missing image provenance",
        }
    })?;
    let block = match image {
        ImageProvenance::Base64 {
            media_type,
            encoded_len,
        } => CrabCodeSdkImageBlock::Base64 {
            media_type: match media_type {
                ImageMediaType::Jpeg => CrabCodeSdkImageMediaType::Jpeg,
                ImageMediaType::Png => CrabCodeSdkImageMediaType::Png,
                ImageMediaType::Gif => CrabCodeSdkImageMediaType::Gif,
                ImageMediaType::Webp => CrabCodeSdkImageMediaType::Webp,
            },
            encoded_len: *encoded_len,
        },
        ImageProvenance::Url { url } => CrabCodeSdkImageBlock::Url { url: url.clone() },
        ImageProvenance::File { file_id } => CrabCodeSdkImageBlock::File {
            file_id: file_id.clone(),
        },
    };
    Ok(projection_block(CrabCodeProjectionKind::SdkImage(block)))
}

fn map_system(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.advisor.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_attachment.is_some()
        || item.presentation.image.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.assistant_block.is_some()
        || item.presentation.direct_user.is_some()
        || item.presentation.direct_assistant.is_some()
        || item.presentation.direct_progress_identity.is_some()
    {
        return inconsistent(
            item,
            "typed system presentation",
            "system combined with a different specialized presentation",
        );
    }
    let system = item.presentation.system.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed system presentation",
            observed: "missing system presentation",
        }
    })?;
    match &system.subtype {
        ProjectedSystemSubtype::Sdk(subtype) => {
            if system.direct.is_some() {
                return inconsistent(
                    item,
                    "SDK system without historical direct data",
                    "SDK system carrying historical direct data",
                );
            }
            map_sdk_system(item, subtype, system.level)
        }
        ProjectedSystemSubtype::Historical(discriminator) => {
            let direct = system.direct.as_ref().ok_or_else(|| {
                ProjectionScrollbackError::InconsistentPresentation {
                    key: item.key.clone(),
                    expected: "historical system direct data",
                    observed: "missing historical direct data",
                }
            })?;
            if discriminator != direct_system_discriminator(&direct.data) {
                return inconsistent(
                    item,
                    "matching historical system discriminator",
                    "historical discriminator/direct-data mismatch",
                );
            }
            map_direct_system(item, &direct.data, system.level)
        }
    }
}

fn map_sdk_system(
    item: &ProjectedItem,
    subtype: &SystemSubtype,
    level: Option<SystemLevel>,
) -> Result<RenderBlock, ProjectionScrollbackError> {
    let (subtype, tone, allowed_kind) = match subtype {
        SystemSubtype::Init => {
            return inconsistent(
                item,
                "non-init SDK system row",
                "system:init has no transcript row",
            );
        }
        SystemSubtype::CompactBoundary => (
            CrabCodeSdkSystemSubtype::CompactBoundary,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::Status => (
            CrabCodeSdkSystemSubtype::Status,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::PostTurnSummary => (
            CrabCodeSdkSystemSubtype::PostTurnSummary,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::ApiRetry => (
            CrabCodeSdkSystemSubtype::ApiRetry,
            CrabCodeSdkSystemTone::Warning,
            &[ProjectedKind::Warning][..],
        ),
        SystemSubtype::LocalCommandOutput => (
            CrabCodeSdkSystemSubtype::LocalCommandOutput,
            CrabCodeSdkSystemTone::Terminal,
            &[ProjectedKind::TerminalOutput][..],
        ),
        SystemSubtype::HookStarted => (
            CrabCodeSdkSystemSubtype::HookStarted,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::HookProgress => (
            CrabCodeSdkSystemSubtype::HookProgress,
            CrabCodeSdkSystemTone::Terminal,
            &[ProjectedKind::TerminalOutput][..],
        ),
        SystemSubtype::HookResponse => (
            CrabCodeSdkSystemSubtype::HookResponse,
            if item.kind == ProjectedKind::Error {
                CrabCodeSdkSystemTone::Error
            } else {
                CrabCodeSdkSystemTone::Terminal
            },
            &[ProjectedKind::TerminalOutput, ProjectedKind::Error][..],
        ),
        SystemSubtype::TaskNotification => (
            CrabCodeSdkSystemSubtype::TaskNotification,
            CrabCodeSdkSystemTone::Progress,
            &[ProjectedKind::Progress][..],
        ),
        SystemSubtype::TaskStarted => (
            CrabCodeSdkSystemSubtype::TaskStarted,
            CrabCodeSdkSystemTone::Progress,
            &[ProjectedKind::Progress][..],
        ),
        SystemSubtype::TaskProgress => (
            CrabCodeSdkSystemSubtype::TaskProgress,
            CrabCodeSdkSystemTone::Progress,
            &[ProjectedKind::Progress][..],
        ),
        SystemSubtype::SessionStateChanged => (
            CrabCodeSdkSystemSubtype::SessionStateChanged,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::FilesPersisted => (
            CrabCodeSdkSystemSubtype::FilesPersisted,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
        SystemSubtype::ElicitationComplete => (
            CrabCodeSdkSystemSubtype::ElicitationComplete,
            CrabCodeSdkSystemTone::System,
            &[ProjectedKind::System][..],
        ),
    };
    if !allowed_kind.contains(&item.kind) {
        return inconsistent(
            item,
            "SDK subtype-compatible projected kind",
            "SDK subtype/projected-kind mismatch",
        );
    }
    Ok(projection_block(CrabCodeProjectionKind::SdkSystem(
        CrabCodeSdkSystemBlock {
            subtype,
            tone,
            level: level.map(map_message_level),
            title: item.title.clone(),
            text: item.text.clone(),
        },
    )))
}

fn map_direct_system(
    item: &ProjectedItem,
    data: &DirectSystemData,
    level: Option<SystemLevel>,
) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.kind != ProjectedKind::System {
        return inconsistent(
            item,
            "historical direct system kind",
            "non-system historical direct row",
        );
    }
    let mapped_level = level.map(map_message_level);
    let block = match data {
        DirectSystemData::Informational { content, .. } => {
            CrabCodeDirectSystemBlock::Informational {
                content: content.clone(),
                level: mapped_level,
            }
        }
        DirectSystemData::PermissionRetry { commands } => {
            CrabCodeDirectSystemBlock::PermissionRetry {
                commands: commands.clone(),
            }
        }
        DirectSystemData::ScheduledTaskFire { content } => {
            CrabCodeDirectSystemBlock::ScheduledTaskFire {
                content: content.clone(),
            }
        }
        DirectSystemData::StopHookSummary {
            hook_count,
            hook_infos,
            hook_errors,
            prevented_continuation,
            stop_reason,
            hook_label,
            ..
        } => CrabCodeDirectSystemBlock::StopHookSummary {
            hook_count: hook_count.clone(),
            hook_info_count: hook_infos.len(),
            hook_errors: hook_errors.clone(),
            prevented_continuation: *prevented_continuation,
            stop_reason: stop_reason.clone(),
            hook_label: hook_label.clone(),
        },
        DirectSystemData::TurnDuration {
            duration_ms,
            budget_tokens,
            budget_limit,
            budget_nudges,
        } => {
            // The fixed component reads `showTurnDuration ?? true` once at
            // mount. The current direct renderer context has no such field and
            // this adapter must not expand that wire, so the only proven
            // production value here is the fixed default. An explicit false
            // configuration remains a tracked parity denominator rather than
            // an invented Rust config authority. Likewise, the fixed
            // AppStateStore task summary has no identity-equivalent Rust input,
            // so this block deliberately does not fabricate a suffix from
            // unrelated task collections.
            CrabCodeDirectSystemBlock::turn_duration(
                duration_ms.clone(),
                budget_tokens.clone(),
                budget_limit.clone(),
                budget_nudges.clone(),
                true,
            )
        }
        DirectSystemData::AwaySummary { content } => CrabCodeDirectSystemBlock::AwaySummary {
            content: content.clone(),
        },
        DirectSystemData::MemorySaved {
            written_paths,
            team_count,
        } => CrabCodeDirectSystemBlock::MemorySaved {
            written_paths: written_paths.clone(),
            team_count: team_count.clone(),
        },
        DirectSystemData::AgentsKilled => CrabCodeDirectSystemBlock::AgentsKilled,
        DirectSystemData::ApiMetrics => CrabCodeDirectSystemBlock::ApiMetrics,
        DirectSystemData::LocalCommand { content } => {
            return Ok(map_historical_user_text(content, None));
        }
        DirectSystemData::ApiError {
            error,
            retry_in_ms,
            retry_attempt,
            max_retries,
        } => CrabCodeDirectSystemBlock::ApiError {
            error: CrabCodeDirectApiError {
                message: error.message.clone(),
                status: error.status.clone(),
                nested_message: error.nested_message.clone(),
                deeply_nested_message: error.deeply_nested_message.clone(),
                connection_code: error.connection_code.clone(),
            },
            retry_in_ms: retry_in_ms.clone(),
            retry_attempt: retry_attempt.clone(),
            max_retries: max_retries.clone(),
            mounted_at: std::time::Instant::now(),
        },
        DirectSystemData::CompactBoundary => CrabCodeDirectSystemBlock::CompactBoundary,
        DirectSystemData::MicrocompactBoundary => CrabCodeDirectSystemBlock::MicrocompactBoundary,
        DirectSystemData::CommandInput { content } => CrabCodeDirectSystemBlock::CommandInput {
            content: content.clone(),
            level: mapped_level,
        },
        DirectSystemData::Thinking => CrabCodeDirectSystemBlock::Thinking,
        DirectSystemData::FileSnapshot { content } => CrabCodeDirectSystemBlock::FileSnapshot {
            content: content.clone(),
            level: mapped_level,
        },
    };
    Ok(projection_block(CrabCodeProjectionKind::DirectSystem(
        block,
    )))
}

fn direct_system_discriminator(data: &DirectSystemData) -> &'static str {
    match data {
        DirectSystemData::Informational { .. } => "informational",
        DirectSystemData::PermissionRetry { .. } => "permission_retry",
        DirectSystemData::ScheduledTaskFire { .. } => "scheduled_task_fire",
        DirectSystemData::StopHookSummary { .. } => "stop_hook_summary",
        DirectSystemData::TurnDuration { .. } => "turn_duration",
        DirectSystemData::AwaySummary { .. } => "away_summary",
        DirectSystemData::MemorySaved { .. } => "memory_saved",
        DirectSystemData::AgentsKilled => "agents_killed",
        DirectSystemData::ApiMetrics => "api_metrics",
        DirectSystemData::LocalCommand { .. } => "local_command",
        DirectSystemData::ApiError { .. } => "api_error",
        DirectSystemData::CompactBoundary => "compact_boundary",
        DirectSystemData::MicrocompactBoundary => "microcompact_boundary",
        DirectSystemData::CommandInput { .. } => "command_input",
        DirectSystemData::Thinking => "thinking",
        DirectSystemData::FileSnapshot { .. } => "file_snapshot",
    }
}

fn map_message_level(level: SystemLevel) -> CrabCodeMessageLevel {
    match level {
        SystemLevel::Info => CrabCodeMessageLevel::Info,
        SystemLevel::Warning => CrabCodeMessageLevel::Warning,
        SystemLevel::Error => CrabCodeMessageLevel::Error,
        SystemLevel::Suggestion => CrabCodeMessageLevel::Suggestion,
    }
}

fn map_direct_progress(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.advisor.is_some()
        || item.presentation.image.is_some()
        || item.presentation.direct_user.is_some()
        || item.presentation.direct_assistant.is_some()
    {
        return inconsistent(
            item,
            "historical direct progress presentation",
            "direct progress combined with a different specialized presentation",
        );
    }
    let progress = item.presentation.direct_progress.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed historical direct progress presentation",
            observed: "missing historical direct progress presentation",
        }
    })?;
    let identity = item
        .presentation
        .direct_progress_identity
        .as_ref()
        .ok_or_else(|| ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "historical direct progress identity",
            observed: "missing historical direct progress identity",
        })?;

    if let DirectProgressPresentation::Workflow {
        task_id,
        workflow,
        phase,
        phase_index: _,
        message,
        agents_started,
        agents_completed,
        phases,
        status,
    } = progress
    {
        if identity.progress_type != "workflow_progress" {
            return inconsistent(
                item,
                "matching workflow progress discriminator",
                "workflow progress identity/payload mismatch",
            );
        }
        if item.kind != ProjectedKind::Progress {
            return inconsistent(
                item,
                "native workflow progress row",
                "workflow progress on a non-progress row",
            );
        }
        if *agents_completed > *agents_started {
            return inconsistent(
                item,
                "bounded workflow agent counters",
                "workflow completed-agent count exceeds started-agent count",
            );
        }
        let running = *status == DirectWorkflowStatus::Running;
        if item.streaming != running {
            return inconsistent(
                item,
                "workflow streaming state matching its lifecycle status",
                "workflow streaming/status mismatch",
            );
        }
        if let Some(attachment) = item.presentation.direct_attachment.as_ref() {
            match &attachment.data {
                DirectAttachmentData::TaskStatus {
                    task_id: attachment_task_id,
                    task_type: DirectTaskType::LocalWorkflow,
                    status: attachment_status,
                    ..
                } if attachment_task_id == task_id
                    && workflow_status_from_task(*attachment_status) == *status => {}
                _ => {
                    return inconsistent(
                        item,
                        "correlated local-workflow task status",
                        "workflow progress combined with an unrelated task attachment",
                    );
                }
            }
        }
        let mut block = WorkflowBlock::started(task_id, workflow, message);
        block.phases = phases
            .iter()
            .map(|entry| WorkflowBlockPhase {
                title: entry.title.clone(),
                state: match entry.state {
                    DirectWorkflowPhaseState::Pending => "pending",
                    DirectWorkflowPhaseState::Active => "active",
                    DirectWorkflowPhaseState::Done => "done",
                }
                .to_string(),
            })
            .collect();
        block.current_phase = phase.clone();
        block.active_agents = if running {
            agents_started.saturating_sub(*agents_completed)
        } else {
            0
        };
        block.status = match status {
            DirectWorkflowStatus::Running => WorkflowBlockStatus::Running,
            DirectWorkflowStatus::Completed => WorkflowBlockStatus::Done {
                elapsed: std::time::Duration::ZERO,
            },
            DirectWorkflowStatus::Failed => WorkflowBlockStatus::Failed {
                elapsed: std::time::Duration::ZERO,
            },
            DirectWorkflowStatus::Cancelled => WorkflowBlockStatus::Cancelled {
                elapsed: std::time::Duration::ZERO,
            },
        };
        return Ok(RenderBlock::Workflow(block));
    }
    if item.presentation.direct_attachment.is_some() {
        return inconsistent(
            item,
            "historical direct progress presentation",
            "non-workflow progress combined with an attachment presentation",
        );
    }
    if !item.streaming {
        return inconsistent(
            item,
            "streaming historical direct progress row",
            "non-streaming historical direct progress row",
        );
    }

    let (expected_progress_type, block) = match progress {
        DirectProgressPresentation::Shell {
            progress_type,
            output,
            elapsed_time_seconds,
            total_lines,
            total_bytes,
            timeout_ms,
            ..
        } => {
            if item.kind != ProjectedKind::TerminalOutput {
                return inconsistent(
                    item,
                    "historical shell terminal-output row",
                    "non-terminal-output shell progress row",
                );
            }
            (
                progress_type.as_str(),
                CrabCodeDirectProgressBlock::Shell {
                    output: output.clone(),
                    full_output: item.text.clone(),
                    elapsed_time_seconds: elapsed_time_seconds.clone(),
                    total_lines: total_lines.clone(),
                    total_bytes: total_bytes.clone(),
                    timeout_ms: timeout_ms.clone(),
                },
            )
        }
        DirectProgressPresentation::Nested { .. } => {
            return inconsistent(
                item,
                "nested progress prepared by the outer-message lifecycle",
                "scalar nested-progress conversion bypassed grouping",
            );
        }
        DirectProgressPresentation::Mcp {
            server_name,
            tool_name,
            progress,
            total,
            progress_message,
            percentage,
            ..
        } => {
            require_progress_kind(item)?;
            (
                "mcp_progress",
                CrabCodeDirectProgressBlock::Mcp {
                    progress: progress.clone(),
                    total: total.clone(),
                    progress_message: progress_message.clone(),
                    percentage: *percentage,
                    server_name: server_name.clone(),
                    tool_name: tool_name.clone(),
                },
            )
        }
        DirectProgressPresentation::SearchQuery { query } => {
            require_progress_kind(item)?;
            (
                "query_update",
                CrabCodeDirectProgressBlock::SearchQuery {
                    query: query.clone(),
                },
            )
        }
        DirectProgressPresentation::SearchResults {
            query,
            result_count,
        } => {
            require_progress_kind(item)?;
            (
                "search_results_received",
                CrabCodeDirectProgressBlock::SearchResults {
                    query: query.clone(),
                    result_count: *result_count,
                },
            )
        }
        DirectProgressPresentation::WaitingForTask {
            task_description,
            task_type,
        } => {
            require_progress_kind(item)?;
            (
                "waiting_for_task",
                CrabCodeDirectProgressBlock::WaitingForTask {
                    task_description: task_description.clone(),
                    task_type: task_type.clone(),
                },
            )
        }
        DirectProgressPresentation::Hook {
            hook_event,
            in_progress_count,
            resolved_count,
        } => {
            require_progress_kind(item)?;
            (
                "hook_progress",
                CrabCodeDirectProgressBlock::Hook {
                    hook_event: hook_event.clone(),
                    in_progress_count: *in_progress_count,
                    resolved_count: *resolved_count,
                },
            )
        }
        DirectProgressPresentation::Workflow { .. } => {
            unreachable!("workflow progress returned through its native lifecycle above")
        }
    };
    if identity.progress_type != expected_progress_type {
        return inconsistent(
            item,
            "matching historical direct progress discriminator",
            "historical direct progress identity/payload mismatch",
        );
    }
    Ok(projection_block(CrabCodeProjectionKind::DirectProgress(
        block,
    )))
}

fn require_progress_kind(item: &ProjectedItem) -> Result<(), ProjectionScrollbackError> {
    if item.kind != ProjectedKind::Progress {
        return inconsistent(
            item,
            "historical progress row",
            "non-progress historical direct progress row",
        );
    }
    Ok(())
}

fn map_direct_attachment(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.advisor.is_some()
        || item.presentation.direct_progress.is_some()
        || item.presentation.direct_progress_identity.is_some()
        || item.presentation.image.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.assistant_block.is_some()
        || item.presentation.direct_user.is_some()
        || item.presentation.direct_assistant.is_some()
    {
        return inconsistent(
            item,
            "historical direct attachment presentation",
            "direct attachment combined with a different specialized presentation",
        );
    }
    if item.streaming {
        return inconsistent(
            item,
            "completed historical direct attachment row",
            "streaming historical direct attachment row",
        );
    }
    let attachment = item
        .presentation
        .direct_attachment
        .as_ref()
        .ok_or_else(|| ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed historical direct attachment presentation",
            observed: "missing historical direct attachment presentation",
        })?;
    if let DirectAttachmentData::TaskStatus {
        task_id,
        task_type,
        status,
        description,
    } = &attachment.data
    {
        if item.kind != ProjectedKind::System {
            return inconsistent(
                item,
                "task-status system row",
                "task-status attachment on a non-system row",
            );
        }
        return Ok(map_native_task_status(
            task_id,
            *task_type,
            *status,
            description,
        ));
    }
    let (expected_kind, block) = match &attachment.data {
        DirectAttachmentData::Directory { display_path } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::Directory {
                display_path: display_path.clone(),
            },
        ),
        DirectAttachmentData::File {
            display_path,
            content,
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::File {
                display_path: display_path.clone(),
                content: map_direct_file_content(content),
            },
        ),
        DirectAttachmentData::CompactFileReference { display_path } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::CompactFileReference {
                display_path: display_path.clone(),
            },
        ),
        DirectAttachmentData::PdfReference {
            display_path,
            page_count,
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::PdfReference {
                display_path: display_path.clone(),
                page_count: *page_count,
            },
        ),
        DirectAttachmentData::SelectedLines {
            ide_name,
            line_start,
            line_end,
            display_path,
        } => {
            if line_end < line_start {
                return inconsistent(
                    item,
                    "selected-lines attachment with lineEnd >= lineStart",
                    "selected-lines attachment with reversed range",
                );
            }
            (
                ProjectedKind::System,
                CrabCodeDirectAttachmentBlock::SelectedLines {
                    ide_name: ide_name.clone(),
                    line_start: *line_start,
                    line_end: *line_end,
                    display_path: display_path.clone(),
                },
            )
        }
        DirectAttachmentData::NestedMemory { display_path } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::NestedMemory {
                display_path: display_path.clone(),
            },
        ),
        DirectAttachmentData::RelevantMemories { memories } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::RelevantMemories {
                memories: memories
                    .iter()
                    .map(|memory| CrabCodeRelevantMemory {
                        path: memory.path.clone(),
                        content: memory.content.clone(),
                    })
                    .collect(),
            },
        ),
        DirectAttachmentData::DynamicSkill {
            skill_names,
            display_path,
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::DynamicSkill {
                skill_names: skill_names.clone(),
                display_path: display_path.clone(),
            },
        ),
        DirectAttachmentData::SkillListing {
            skill_count,
            is_initial,
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::SkillListing {
                skill_count: *skill_count,
                is_initial: *is_initial,
            },
        ),
        DirectAttachmentData::AgentListingDelta {
            added_types,
            is_initial,
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::AgentListingDelta {
                added_types: added_types.clone(),
                is_initial: *is_initial,
            },
        ),
        DirectAttachmentData::QueuedCommand {
            text,
            image_paste_ids,
            command_mode: _,
            is_meta: _,
            origin: _,
        } => {
            if item.kind != ProjectedKind::User {
                return inconsistent(
                    item,
                    "queued-command user kind",
                    "non-user queued-command attachment",
                );
            }
            // AttachmentMessage routes the prompt through the complete
            // UserTextMessage discriminator and then emits one UserImageMessage
            // label for every imagePasteId. It does not inspect queue metadata
            // or command mode here; image-store paths are not serialized, so
            // the renderer retains the exact plain labels without inventing
            // hyperlink targets.
            let text_block = map_historical_user_text(text, None);
            return Ok(projection_block(CrabCodeProjectionKind::DirectAttachment(
                CrabCodeDirectAttachmentBlock::QueuedCommand {
                    text: Box::new(text_block),
                    text_is_hidden: false,
                    image_paste_ids: image_paste_ids.clone(),
                },
            )));
        }
        DirectAttachmentData::PlanFileReference { plan_file_path } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::PlanFileReference {
                plan_file_path: plan_file_path.clone(),
            },
        ),
        DirectAttachmentData::InvokedSkills { skill_names } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::InvokedSkills {
                skill_names: skill_names.clone(),
            },
        ),
        DirectAttachmentData::Diagnostics { files } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::Diagnostics {
                files: files.iter().map(map_diagnostic_file).collect(),
            },
        ),
        DirectAttachmentData::McpResource { name, server, uri } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::McpResource {
                name: name.clone(),
                server: server.clone(),
                uri: uri.clone(),
            },
        ),
        DirectAttachmentData::HookBlockingError {
            hook_name,
            hook_event,
            blocking_error,
            ..
        } => (
            ProjectedKind::Error,
            CrabCodeDirectAttachmentBlock::HookBlockingError {
                hook_name: hook_name.clone(),
                hook_event: hook_event.clone(),
                blocking_error: blocking_error.clone(),
            },
        ),
        DirectAttachmentData::HookNonBlockingError {
            hook_name,
            hook_event,
            ..
        } => (
            ProjectedKind::Error,
            CrabCodeDirectAttachmentBlock::HookNonBlockingError {
                hook_name: hook_name.clone(),
                hook_event: hook_event.clone(),
            },
        ),
        DirectAttachmentData::HookErrorDuringExecution {
            hook_name,
            hook_event,
            ..
        } => (
            ProjectedKind::Warning,
            CrabCodeDirectAttachmentBlock::HookErrorDuringExecution {
                hook_name: hook_name.clone(),
                hook_event: hook_event.clone(),
            },
        ),
        DirectAttachmentData::HookStoppedContinuation {
            hook_name,
            hook_event,
            message,
            ..
        } => (
            ProjectedKind::Warning,
            CrabCodeDirectAttachmentBlock::HookStoppedContinuation {
                hook_name: hook_name.clone(),
                hook_event: hook_event.clone(),
                message: message.clone(),
            },
        ),
        DirectAttachmentData::HookSystemMessage {
            hook_name, content, ..
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::HookSystemMessage {
                hook_name: hook_name.clone(),
                content: content.clone(),
            },
        ),
        DirectAttachmentData::HookPermissionDecision {
            hook_event,
            decision,
            ..
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::HookPermissionDecision {
                hook_event: hook_event.clone(),
                decision: match decision {
                    DirectHookPermissionDecision::Allow => CrabCodeHookPermissionDecision::Allow,
                    DirectHookPermissionDecision::Deny => CrabCodeHookPermissionDecision::Deny,
                },
            },
        ),
        DirectAttachmentData::TaskStatus {
            status,
            description,
            ..
        } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::TaskStatus {
                status: map_task_status(*status),
                description: description.clone(),
            },
        ),
        DirectAttachmentData::TeammateShutdownBatch { count } => (
            ProjectedKind::System,
            CrabCodeDirectAttachmentBlock::TeammateShutdownBatch { count: *count },
        ),
    };
    if item.kind != expected_kind {
        return inconsistent(
            item,
            "historical attachment-compatible projected kind",
            "historical attachment/projected-kind mismatch",
        );
    }
    Ok(projection_block(CrabCodeProjectionKind::DirectAttachment(
        block,
    )))
}

fn map_native_task_status(
    task_id: &str,
    task_type: DirectTaskType,
    status: DirectTaskStatus,
    description: &str,
) -> RenderBlock {
    if task_type == DirectTaskType::LocalWorkflow {
        let mut block = WorkflowBlock::started(task_id, "workflow", description);
        block.status = match status {
            DirectTaskStatus::Pending | DirectTaskStatus::Running => WorkflowBlockStatus::Running,
            DirectTaskStatus::Completed => WorkflowBlockStatus::Done {
                elapsed: std::time::Duration::ZERO,
            },
            DirectTaskStatus::Failed => WorkflowBlockStatus::Failed {
                elapsed: std::time::Duration::ZERO,
            },
            DirectTaskStatus::Killed => WorkflowBlockStatus::Cancelled {
                elapsed: std::time::Duration::ZERO,
            },
        };
        return RenderBlock::Workflow(block);
    }
    if matches!(
        task_type,
        DirectTaskType::LocalAgent
            | DirectTaskType::RemoteAgent
            | DirectTaskType::InProcessTeammate
    ) {
        let mut block = SubagentBlock::started(
            description,
            task_id,
            match task_type {
                DirectTaskType::LocalAgent => "local-agent",
                DirectTaskType::RemoteAgent => "remote-agent",
                DirectTaskType::InProcessTeammate => "teammate",
                _ => unreachable!("the outer match selected an agent task"),
            },
            None,
            None,
            None,
            true,
        );
        block.kind = match status {
            DirectTaskStatus::Pending | DirectTaskStatus::Running => SubagentBlockKind::Started,
            DirectTaskStatus::Completed => SubagentBlockKind::Completed {
                elapsed: std::time::Duration::ZERO,
            },
            DirectTaskStatus::Failed => SubagentBlockKind::Failed {
                elapsed: std::time::Duration::ZERO,
                error: None,
            },
            DirectTaskStatus::Killed => SubagentBlockKind::Cancelled {
                elapsed: std::time::Duration::ZERO,
            },
        };
        return RenderBlock::Subagent(block);
    }

    match status {
        DirectTaskStatus::Pending | DirectTaskStatus::Running => {
            RenderBlock::bg_task(description, task_id)
        }
        DirectTaskStatus::Completed => {
            RenderBlock::bg_task_completed(description, task_id, std::time::Duration::ZERO)
        }
        DirectTaskStatus::Failed => {
            RenderBlock::bg_task_failed(description, task_id, std::time::Duration::ZERO, None, None)
        }
        DirectTaskStatus::Killed => RenderBlock::bg_task_failed(
            description,
            task_id,
            std::time::Duration::ZERO,
            None,
            Some("killed".to_string()),
        ),
    }
}

fn workflow_status_from_task(status: DirectTaskStatus) -> DirectWorkflowStatus {
    match status {
        DirectTaskStatus::Pending | DirectTaskStatus::Running => DirectWorkflowStatus::Running,
        DirectTaskStatus::Completed => DirectWorkflowStatus::Completed,
        DirectTaskStatus::Failed => DirectWorkflowStatus::Failed,
        DirectTaskStatus::Killed => DirectWorkflowStatus::Cancelled,
    }
}

fn map_direct_file_content(content: &DirectFileAttachmentContent) -> CrabCodeDirectFileContent {
    match content {
        DirectFileAttachmentContent::Notebook { cell_count } => {
            CrabCodeDirectFileContent::Notebook {
                cell_count: *cell_count,
            }
        }
        DirectFileAttachmentContent::Unchanged => CrabCodeDirectFileContent::Unchanged,
        DirectFileAttachmentContent::Text {
            line_count,
            truncated,
        } => CrabCodeDirectFileContent::Text {
            line_count: *line_count,
            truncated: *truncated,
        },
        DirectFileAttachmentContent::Binary { original_size } => {
            CrabCodeDirectFileContent::Binary {
                original_size: original_size.clone(),
            }
        }
    }
}

fn map_diagnostic_file(
    file: &crate::sdk_projection::DirectDiagnosticFile,
) -> CrabCodeDiagnosticFile {
    CrabCodeDiagnosticFile {
        uri: file.uri.clone(),
        diagnostics: file
            .diagnostics
            .iter()
            .map(|diagnostic| CrabCodeDiagnostic {
                message: diagnostic.message.clone(),
                severity: match diagnostic.severity {
                    DirectDiagnosticSeverity::Error => CrabCodeDiagnosticSeverity::Error,
                    DirectDiagnosticSeverity::Warning => CrabCodeDiagnosticSeverity::Warning,
                    DirectDiagnosticSeverity::Info => CrabCodeDiagnosticSeverity::Info,
                    DirectDiagnosticSeverity::Hint => CrabCodeDiagnosticSeverity::Hint,
                },
                start_line: diagnostic.start_line.clone(),
                start_character: diagnostic.start_character.clone(),
                code: diagnostic.code.clone(),
                source: diagnostic.source.clone(),
            })
            .collect(),
    }
}

fn map_task_status(status: DirectTaskStatus) -> CrabCodeTaskStatus {
    match status {
        DirectTaskStatus::Pending => CrabCodeTaskStatus::Pending,
        DirectTaskStatus::Running => CrabCodeTaskStatus::Running,
        DirectTaskStatus::Completed => CrabCodeTaskStatus::Completed,
        DirectTaskStatus::Failed => CrabCodeTaskStatus::Failed,
        DirectTaskStatus::Killed => CrabCodeTaskStatus::Killed,
    }
}

fn map_user(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.assistant_block.is_some()
        || item.presentation.system.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.direct_progress_identity.is_some()
    {
        return inconsistent(
            item,
            "plain user presentation",
            "typed non-user presentation",
        );
    }
    if let Some(direct_user) = item.presentation.direct_user.as_ref() {
        if !matches!(
            direct_user.block_type,
            DirectUserBlockType::Text
                | DirectUserBlockType::Document
                | DirectUserBlockType::ConnectorText
        ) {
            return inconsistent(
                item,
                "visible direct user text/document presentation",
                "non-textual direct user presentation reached the prompt renderer",
            );
        }
        return Ok(map_historical_user_text(
            &item.text,
            direct_user.plan_content.as_deref(),
        ));
    }
    if item.presentation.direct_assistant.is_some() {
        return inconsistent(
            item,
            "plain user presentation",
            "historical direct assistant identity on user row",
        );
    }
    // SDK replay users (for example the output injected after the existing
    // `set_model` control) deliberately carry `parent_tool_use_id: null`.
    // They therefore have no direct-query presentation, but they still enter
    // the same fixed UserTextMessage content router as direct user rows.
    Ok(map_historical_user_text(&item.text, None))
}

/// Route the fixed `UserTextMessage` branches that are produced by local
/// command execution without reopening backend or protocol ownership.
///
/// Outer visibility and component-null branches have already been applied by
/// [`historical_direct_item_is_hidden`]. The remaining command breadcrumb and
/// terminal-output tags are renderer markup, so they are stripped here before
/// entering searchable scrollback. Unknown-command text and ordinary user
/// prompts retain the normal user block.
fn map_historical_user_text(content: &str, plan_content: Option<&str>) -> RenderBlock {
    if let Some(plan) = plan_content.filter(|plan| !plan.is_empty()) {
        return RenderBlock::user_prompt(plan.to_string());
    }

    if content.starts_with("<bash-stdout") || content.starts_with("<bash-stderr") {
        return RenderBlock::system(indented_tagged_output(
            content,
            "bash-stdout",
            "bash-stderr",
        ));
    }
    if content.starts_with("<local-command-stdout") || content.starts_with("<local-command-stderr")
    {
        return RenderBlock::local_command_output(content.to_string());
    }
    if let Some(input) = exact_tag_text(content, "bash-input") {
        return RenderBlock::bash_prompt(input.trim().to_string());
    }
    if let Some(command) = exact_tag_text(content, "command-message")
        && !command.trim().is_empty()
    {
        let command = command.trim();
        if exact_tag_text(content, "skill-format").is_some_and(|value| value.trim() == "true") {
            return RenderBlock::skill_prompt(command.to_string());
        }
        let args = exact_tag_text(content, "command-args")
            .map(str::trim)
            .filter(|args| !args.is_empty());
        let display = match args {
            Some(args) => format!("/{command} {args}"),
            None => format!("/{command}"),
        };
        return RenderBlock::user_prompt(display);
    }

    RenderBlock::user_prompt(content.to_string())
}

fn exact_tag_text<'a>(content: &'a str, tag: &str) -> Option<&'a str> {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let start = content.find(&opening)?.checked_add(opening.len())?;
    let end = content.get(start..)?.find(&closing)?.checked_add(start)?;
    content.get(start..end)
}

fn indented_tagged_output(content: &str, stdout_tag: &str, stderr_tag: &str) -> String {
    let output = [stdout_tag, stderr_tag]
        .into_iter()
        .filter_map(|tag| exact_tag_text(content, tag))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let output = if output.is_empty() {
        NO_CONTENT_MESSAGE
    } else {
        output.as_str()
    };
    format!("  ⎿  {}", output.replace('\n', "\n     "))
}

fn map_assistant(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.thinking.is_some()
        || item.presentation.direct_progress_identity.is_some()
    {
        return inconsistent(
            item,
            "assistant text presentation",
            "typed non-assistant presentation",
        );
    }
    if item.presentation.direct_user.is_some() {
        return inconsistent(
            item,
            "assistant text presentation",
            "historical direct user identity on assistant row",
        );
    }
    match item.presentation.assistant_block {
        Some(AssistantBlockType::Text | AssistantBlockType::ConnectorText) => {}
        Some(
            AssistantBlockType::Thinking
            | AssistantBlockType::RedactedThinking
            | AssistantBlockType::ToolUse
            | AssistantBlockType::ServerToolUse
            | AssistantBlockType::McpToolUse
            | AssistantBlockType::WebSearchToolResult
            | AssistantBlockType::WebFetchToolResult
            | AssistantBlockType::CodeExecutionToolResult
            | AssistantBlockType::BashCodeExecutionToolResult
            | AssistantBlockType::TextEditorCodeExecutionToolResult
            | AssistantBlockType::ToolSearchToolResult
            | AssistantBlockType::McpToolResult
            | AssistantBlockType::ContainerUpload
            | AssistantBlockType::AdvisorToolResult,
        ) => {
            return inconsistent(
                item,
                "assistant text/connector_text block",
                "non-text assistant block",
            );
        }
        Some(AssistantBlockType::Compaction) => {
            return unclosed(item, "assistant compaction");
        }
        None => {
            return inconsistent(
                item,
                "typed assistant text/connector_text block",
                "missing assistant block type",
            );
        }
    }

    if item.streaming {
        let mut block = RenderBlock::agent_message_streaming();
        block
            .as_agent_message_mut()
            .expect("the constructor returns an AgentMessage block")
            .push_chunk(&item.text);
        Ok(block)
    } else {
        Ok(RenderBlock::agent_message(item.text.clone()))
    }
}

fn map_thinking(item: &ProjectedItem) -> Result<RenderBlock, ProjectionScrollbackError> {
    if item.presentation.system.is_some()
        || item.presentation.tool.is_some()
        || item.presentation.direct_progress_identity.is_some()
    {
        return inconsistent(
            item,
            "ordinary thinking presentation",
            "typed system/tool presentation",
        );
    }
    if let Some(direct_user) = item.presentation.direct_user.as_ref()
        && !matches!(
            direct_user.block_type,
            DirectUserBlockType::Thinking | DirectUserBlockType::RedactedThinking
        )
    {
        return inconsistent(
            item,
            "direct user thinking presentation",
            "non-thinking direct user block reached thinking renderer",
        );
    }
    let thinking = item.presentation.thinking.as_ref().ok_or_else(|| {
        ProjectionScrollbackError::InconsistentPresentation {
            key: item.key.clone(),
            expected: "typed ordinary thinking content",
            observed: "missing thinking presentation",
        }
    })?;
    if thinking.kind == ThinkingKind::Redacted {
        match item.presentation.assistant_block {
            None | Some(AssistantBlockType::RedactedThinking) => {}
            Some(_) => {
                return inconsistent(
                    item,
                    "redacted-thinking assistant block",
                    "non-redacted assistant block",
                );
            }
        }
        return Ok(projection_block(CrabCodeProjectionKind::RedactedThinking));
    }
    if thinking.content != item.text {
        return inconsistent(
            item,
            "matching typed thinking content and projected text",
            "divergent thinking content",
        );
    }
    match item.presentation.assistant_block {
        None | Some(AssistantBlockType::Thinking) => {}
        Some(AssistantBlockType::RedactedThinking) => {
            return inconsistent(
                item,
                "ordinary thinking assistant block",
                "redacted assistant block paired with ordinary thinking state",
            );
        }
        Some(_) => {
            return inconsistent(
                item,
                "ordinary thinking assistant block",
                "non-thinking assistant block",
            );
        }
    }

    if item.streaming {
        let mut block = RenderBlock::thinking_streaming();
        block
            .as_thinking_mut()
            .expect("the constructor returns a Thinking block")
            .push_chunk(&thinking.content);
        Ok(block)
    } else {
        Ok(RenderBlock::thinking(thinking.content.clone()))
    }
}

fn unclosed<T>(
    item: &ProjectedItem,
    consumer: &'static str,
) -> Result<T, ProjectionScrollbackError> {
    Err(ProjectionScrollbackError::UnclosedConsumer {
        key: item.key.clone(),
        kind: item.kind,
        consumer,
    })
}

fn inconsistent<T>(
    item: &ProjectedItem,
    expected: &'static str,
    observed: &'static str,
) -> Result<T, ProjectionScrollbackError> {
    Err(ProjectionScrollbackError::InconsistentPresentation {
        key: item.key.clone(),
        expected,
        observed,
    })
}

#[cfg(test)]
mod tests {
    use crabcode_pager_render::appearance::AppearanceConfig;
    use crabcode_pager_render::scrollback::{
        BlockContent, BlockContext, DisplayMode, RenderBlock, ScrollbackState, ToolCallBlock,
    };
    use crabcode_pager_render::theme::Theme;

    use super::*;
    use crate::sdk_projection::{
        DirectAssistantPresentation, DirectAttachmentPresentation, DirectMessageIdentity,
        DirectProgressIdentity, DirectSystemPresentation, DirectTaskType, DirectUserPresentation,
        ProjectedPresentation, SystemPresentation, ThinkingPresentation,
    };

    fn item(key: &str, kind: ProjectedKind, text: &str) -> ProjectedItem {
        ProjectedItem {
            key: key.to_string(),
            kind,
            title: match kind {
                ProjectedKind::User => "User",
                ProjectedKind::Assistant => "Assistant",
                ProjectedKind::Thinking => "Thinking",
                ProjectedKind::System => "System",
                ProjectedKind::ToolUse => "Tool",
                ProjectedKind::ToolResult => "Tool result",
                ProjectedKind::TerminalOutput => "Terminal",
                ProjectedKind::Progress => "Progress",
                ProjectedKind::Warning => "Warning",
                ProjectedKind::Error => "Error",
            }
            .to_string(),
            text: text.to_string(),
            streaming: false,
            raw_sequences: vec![1],
            tool_use_id: None,
            presentation: ProjectedPresentation::default(),
        }
    }

    fn raw_direct(sequence: u64, value: serde_json::Value) -> crate::sdk_runtime::RawEnvelope {
        let classification =
            crate::sdk_runtime::classify_envelope(&value).expect("classify direct envelope");
        crate::sdk_runtime::RawEnvelope {
            sequence,
            encoded_len: serde_json::to_vec(&value)
                .expect("encode direct envelope")
                .len(),
            value,
            classification,
            correlation: None,
        }
    }

    fn boot_notice(id: u64, text: &str) -> RendererNoticeProjection {
        RendererNoticeProjection {
            id,
            placement: RendererNoticePlacement::BootPrefix,
            text: text.to_string(),
        }
    }

    fn runtime_notice(
        id: u64,
        after_sequence: Option<u64>,
        text: &str,
    ) -> RendererNoticeProjection {
        RendererNoticeProjection {
            id,
            placement: RendererNoticePlacement::AfterRawSequence(after_sequence),
            text: text.to_string(),
        }
    }

    fn assistant(key: &str, text: &str, streaming: bool) -> ProjectedItem {
        let mut item = item(key, ProjectedKind::Assistant, text);
        item.streaming = streaming;
        item.presentation.assistant_block = Some(AssistantBlockType::Text);
        item
    }

    fn thinking(key: &str, text: &str, streaming: bool) -> ProjectedItem {
        let mut item = item(key, ProjectedKind::Thinking, text);
        item.streaming = streaming;
        item.presentation.assistant_block = Some(AssistantBlockType::Thinking);
        item.presentation.thinking = Some(ThinkingPresentation {
            kind: ThinkingKind::Thinking,
            content: text.to_string(),
            signature: Some("signature".to_string()),
        });
        item
    }

    fn direct_progress(
        key: &str,
        kind: ProjectedKind,
        text: &str,
        progress_type: &str,
        presentation: DirectProgressPresentation,
    ) -> ProjectedItem {
        let mut item = item(key, kind, text);
        item.streaming = true;
        item.presentation.direct_progress_identity = Some(DirectProgressIdentity {
            uuid: format!("{key}-uuid"),
            tool_use_id: format!("{key}-progress-tool"),
            parent_tool_use_id: format!("{key}-parent-tool"),
            progress_type: progress_type.to_string(),
            raw_sequence: 2,
        });
        item.presentation.direct_progress = Some(presentation);
        item
    }

    fn nested_progress_item(
        mut item: ProjectedItem,
        raw_sequence: u64,
        progress_type: &str,
        parent_tool_use_id: &str,
        message_kind: DirectNestedMessageKind,
        prompt: &str,
    ) -> ProjectedItem {
        item.raw_sequences = vec![raw_sequence];
        item.presentation.direct_progress_identity = Some(DirectProgressIdentity {
            uuid: format!("progress-{raw_sequence}"),
            tool_use_id: format!("progress-tool-{raw_sequence}"),
            parent_tool_use_id: parent_tool_use_id.to_string(),
            progress_type: progress_type.to_string(),
            raw_sequence,
        });
        item.presentation.direct_progress = Some(DirectProgressPresentation::Nested {
            progress_type: progress_type.to_string(),
            parent_tool_use_id: parent_tool_use_id.to_string(),
            progress_tool_use_id: format!("progress-tool-{raw_sequence}"),
            prompt: prompt.to_string(),
            agent_id: "agent-1".to_string(),
            message_kind,
            usage: None,
        });
        item
    }

    fn direct_attachment(
        key: &str,
        kind: ProjectedKind,
        data: DirectAttachmentData,
    ) -> ProjectedItem {
        let mut item = item(key, kind, "fallback text must not select the renderer");
        item.presentation.direct_attachment = Some(DirectAttachmentPresentation {
            identity: DirectMessageIdentity {
                uuid: format!("{key}-uuid"),
            },
            data,
        });
        item
    }

    fn direct_system(
        key: &str,
        level: Option<SystemLevel>,
        data: DirectSystemData,
    ) -> ProjectedItem {
        let mut item = item(key, ProjectedKind::System, "typed direct system");
        item.presentation.system = Some(SystemPresentation {
            subtype: ProjectedSystemSubtype::Historical(
                direct_system_discriminator(&data).to_string(),
            ),
            level,
            direct: Some(DirectSystemPresentation {
                identity: DirectMessageIdentity {
                    uuid: format!("{key}-uuid"),
                },
                data,
            }),
        });
        item
    }

    fn typed_tool(
        key: &str,
        kind: ProjectedKind,
        name: Option<&str>,
        input: Option<serde_json::Value>,
        result: Option<serde_json::Value>,
        is_error: Option<bool>,
    ) -> ProjectedItem {
        let mut item = item(key, kind, "display text must not select the renderer");
        item.title = "misleading title".to_string();
        item.presentation.tool = Some(ToolPresentation {
            name: name.map(str::to_string),
            input,
            partial_input_json: None,
            lifecycle_output: None,
            result,
            is_error,
        });
        item
    }

    fn direct_user_presentation(
        key: &str,
        block_type: DirectUserBlockType,
    ) -> DirectUserPresentation {
        DirectUserPresentation {
            identity: DirectMessageIdentity {
                uuid: format!("{key}-message"),
            },
            timestamp: "2026-07-28T00:00:00Z".to_string(),
            is_meta: None,
            is_visible_in_transcript_only: None,
            is_compact_summary: None,
            source_tool_use_id: None,
            origin: None,
            compact_summary: None,
            plan_content: None,
            tool_use_result: None,
            block_type,
            image_paste_id: None,
            render_image_id: None,
        }
    }

    fn direct_tool_result(
        key: &str,
        renderer_result: Option<serde_json::Value>,
        block_content: Option<serde_json::Value>,
        is_error: Option<bool>,
    ) -> ProjectedItem {
        let mut item = typed_tool(
            key,
            ProjectedKind::ToolResult,
            Some("RepoProbe"),
            None,
            block_content,
            is_error,
        );
        let mut direct_user = direct_user_presentation(key, DirectUserBlockType::ToolResult);
        direct_user.tool_use_result = renderer_result;
        item.presentation.direct_user = Some(direct_user);
        item
    }

    #[test]
    fn appends_closed_text_blocks_with_stable_entry_ids() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let items = vec![
            item("user", ProjectedKind::User, "hello"),
            assistant("assistant", "world", false),
            thinking("thinking", "reason", false),
        ];

        let first = adapter.synchronize(&mut state, &items).unwrap();
        assert_eq!(
            first,
            ProjectionScrollbackDelta {
                appended: 3,
                ..ProjectionScrollbackDelta::default()
            }
        );
        let ids = items
            .iter()
            .map(|item| adapter.entry_id(&item.key).unwrap())
            .collect::<Vec<_>>();

        let second = adapter.synchronize(&mut state, &items).unwrap();
        assert_eq!(
            second,
            ProjectionScrollbackDelta {
                unchanged: 3,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert_eq!(
            ids,
            items
                .iter()
                .map(|item| adapter.entry_id(&item.key).unwrap())
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(block)) if block.text == "hello"
        ));
        assert!(matches!(
            state.entry(1).map(|entry| &entry.block),
            Some(RenderBlock::AgentMessage(block)) if block.text() == "world"
        ));
        assert!(matches!(
            state.entry(2).map(|entry| &entry.block),
            Some(RenderBlock::Thinking(block)) if block.text() == "reason"
        ));
    }

    #[test]
    fn renderer_notices_share_the_single_scrollback_owner_without_becoming_projection_items() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let items = vec![item("user", ProjectedKind::User, "hello")];
        let notices = vec![boot_notice(7, "中文启动提醒"), boot_notice(8, "安全检查")];

        let first = adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &items,
                &notices,
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert_eq!(
            first,
            ProjectionScrollbackDelta {
                appended: 3,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "中文启动提醒"
        ));
        assert!(matches!(
            state.entry(1).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "安全检查"
        ));
        assert!(matches!(
            state.entry(2).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(block)) if block.text == "hello"
        ));
        assert_eq!(adapter.ordered_keys(), ["user"]);

        let notice_ids = [
            state.entry(0).expect("first notice").id,
            state.entry(1).expect("second notice").id,
        ];
        let frozen = adapter
            .synchronize_with_options_preserving_notices(
                &mut state,
                &items,
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert_eq!(
            frozen,
            ProjectionScrollbackDelta {
                unchanged: 3,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert_eq!(
            [
                state.entry(0).expect("first notice").id,
                state.entry(1).expect("second notice").id,
            ],
            notice_ids
        );

        let translated = vec![
            boot_notice(7, "Chinese startup notice"),
            boot_notice(8, "安全检查"),
        ];
        let second = adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &items,
                &translated,
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert_eq!(
            second,
            ProjectionScrollbackDelta {
                updated: 1,
                unchanged: 2,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "Chinese startup notice"
        ));

        let third = adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &items,
                &translated[..1],
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert_eq!(
            third,
            ProjectionScrollbackDelta {
                removed: 1,
                unchanged: 2,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn runtime_notices_remain_between_prior_and_later_projection_rows() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let mut prior = vec![
            item("a", ProjectedKind::User, "a"),
            assistant("b", "b", false),
        ];
        prior[0].raw_sequences = vec![1];
        prior[1].raw_sequences = vec![2];
        let boot = boot_notice(1, "boot");
        adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &prior,
                std::slice::from_ref(&boot),
                SynchronizationOptions::default(),
            )
            .unwrap();
        let mut emitted = Vec::new();
        crabcode_pager_render::scrollback::minimal::commit_leading_run(
            &mut state,
            false,
            |state, index| {
                emitted.push(state.entry(index).expect("committed entry").id);
                true
            },
        );
        assert_eq!(emitted.len(), 3);

        let mut later = prior.clone();
        let mut c = assistant("c", "c", false);
        c.raw_sequences = vec![3];
        later.push(c);
        let notices = vec![boot, runtime_notice(2, Some(2), "editor wait")];
        adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &later,
                &notices,
                SynchronizationOptions::default(),
            )
            .unwrap();

        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "boot"
        ));
        assert_eq!(state.entry(1).map(|entry| entry.id), adapter.entry_id("a"));
        assert_eq!(state.entry(2).map(|entry| entry.id), adapter.entry_id("b"));
        assert!(matches!(
            state.entry(3).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "editor wait"
        ));
        assert_eq!(state.entry(4).map(|entry| entry.id), adapter.entry_id("c"));
        crabcode_pager_render::scrollback::minimal::commit_leading_run(
            &mut state,
            false,
            |state, index| {
                emitted.push(state.entry(index).expect("committed entry").id);
                true
            },
        );
        assert_eq!(
            emitted,
            (0..state.len())
                .map(|index| state.entry(index).expect("ordered entry").id)
                .collect::<Vec<_>>(),
            "native commit order and the owned timeline agree at the runtime-notice barrier"
        );

        let mut d = assistant("d", "d", false);
        d.raw_sequences = vec![4];
        later.push(d);
        adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &later,
                &notices,
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            state.entry(3).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "editor wait"
        ));
        assert_eq!(state.entry(5).map(|entry| entry.id), adapter.entry_id("d"));

        let mut revealed_old = item("old-quiet", ProjectedKind::User, "old");
        revealed_old.raw_sequences = vec![1];
        let mut revealed = vec![revealed_old];
        revealed.extend(later);
        adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &revealed,
                &notices,
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            state.entry(3).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "editor wait"
        ));
        assert_eq!(
            state.entry(5).map(|entry| entry.id),
            adapter.entry_id("old-quiet"),
            "a newly revealed row cannot move ahead of an already printed row"
        );
        crabcode_pager_render::scrollback::minimal::commit_leading_run(
            &mut state,
            false,
            |state, index| {
                emitted.push(state.entry(index).expect("committed entry").id);
                true
            },
        );
        assert_eq!(
            emitted,
            (0..state.len())
                .map(|index| state.entry(index).expect("ordered entry").id)
                .collect::<Vec<_>>(),
            "late visibility cannot make owned memory disagree with immutable native scrollback"
        );
    }

    #[test]
    fn committed_projection_order_precedes_rows_revealed_later() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let a = item("a", ProjectedKind::User, "a");
        let c = assistant("c", "c", false);
        adapter
            .synchronize(&mut state, &[a.clone(), c.clone()])
            .unwrap();
        let mut emitted = Vec::new();
        crabcode_pager_render::scrollback::minimal::commit_leading_run(
            &mut state,
            false,
            |state, index| {
                emitted.push(state.entry(index).expect("committed entry").id);
                true
            },
        );

        let b = assistant("b", "b", false);
        adapter.synchronize(&mut state, &[a, b, c]).unwrap();
        assert_eq!(
            (0..state.len())
                .map(|index| state.entry(index).expect("ordered entry").id)
                .collect::<Vec<_>>(),
            [
                adapter.entry_id("a").expect("a"),
                adapter.entry_id("c").expect("c"),
                adapter.entry_id("b").expect("b"),
            ],
        );
        crabcode_pager_render::scrollback::minimal::commit_leading_run(
            &mut state,
            false,
            |state, index| {
                emitted.push(state.entry(index).expect("committed entry").id);
                true
            },
        );
        assert_eq!(
            emitted,
            (0..state.len())
                .map(|index| state.entry(index).expect("ordered entry").id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn notice_and_projection_failure_is_atomic() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let initial = vec![item("user", ProjectedKind::User, "kept")];
        let notice = boot_notice(41, "original");
        adapter
            .synchronize_with_options_and_notices(
                &mut state,
                &initial,
                std::slice::from_ref(&notice),
                SynchronizationOptions::default(),
            )
            .unwrap();
        state.set_selected(Some(1));
        let notice_id = state.entry(0).expect("boot notice").id;
        let item_id = adapter.entry_id("user").expect("projected item");
        let generation = state.content_generation();
        let selection = state.selected();

        let invalid = item("invalid", ProjectedKind::System, "untyped");
        let translated = boot_notice(41, "translated");
        let result = adapter.synchronize_with_options_and_notices(
            &mut state,
            &[initial[0].clone(), invalid],
            std::slice::from_ref(&translated),
            SynchronizationOptions::default(),
        );

        assert!(matches!(
            result,
            Err(ProjectionScrollbackError::UnclosedConsumer {
                kind: ProjectedKind::System,
                ..
            })
        ));
        assert_eq!(state.content_generation(), generation);
        assert_eq!(state.selected(), selection);
        assert_eq!(state.entry(0).map(|entry| entry.id), Some(notice_id));
        assert_eq!(state.entry(1).map(|entry| entry.id), Some(item_id));
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::System(block)) if block.text == "original"
        ));
        assert_eq!(adapter.key_for_entry_id(item_id), Some("user"));
        assert_eq!(adapter.key_for_entry_id(notice_id), None);
    }

    #[test]
    fn producer_classified_plain_system_uses_fixed_generic_system_block() {
        let mut summary = item(
            "streamlined-tool-summary",
            ProjectedKind::System,
            "Read 2 files",
        );
        summary.presentation.plain_system = true;

        assert!(matches!(
            render_block_for(&summary),
            Ok(RenderBlock::System(ref block)) if block.text == "Read 2 files"
        ));

        summary.kind = ProjectedKind::Warning;
        assert!(matches!(
            render_block_for(&summary),
            Err(ProjectionScrollbackError::InconsistentPresentation { .. })
        ));
    }

    #[test]
    fn fixed_direct_system_visibility_and_complete_static_rows_map_without_fallbacks() {
        for (key, data) in [
            (
                "info",
                DirectSystemData::Informational {
                    content: "information".to_string(),
                    tool_use_id: None,
                },
            ),
            (
                "command-input",
                DirectSystemData::CommandInput {
                    content: "/status".to_string(),
                },
            ),
            (
                "file-snapshot",
                DirectSystemData::FileSnapshot {
                    content: "snapshot".to_string(),
                },
            ),
        ] {
            assert!(matches!(
                render_block_for(&direct_system(key, Some(SystemLevel::Info), data)),
                Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                    kind: CrabCodeProjectionKind::DirectSystem(_),
                }))
            ));
        }

        let stop = direct_system(
            "stop",
            Some(SystemLevel::Info),
            DirectSystemData::StopHookSummary {
                hook_count: serde_json::Number::from(2),
                hook_infos: vec![],
                hook_errors: vec!["denied".to_string()],
                prevented_continuation: true,
                stop_reason: Some("blocked".to_string()),
                has_output: false,
                tool_use_id: None,
                hook_label: None,
                total_duration_ms: None,
            },
        );
        assert!(matches!(
            render_block_for(&stop),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectSystem(
                    CrabCodeDirectSystemBlock::StopHookSummary {
                        prevented_continuation: true,
                        ref hook_errors,
                        ..
                    },
                ),
            })) if hook_errors == &["denied"]
        ));

        let stop_with_verbose_detail = direct_system(
            "stop-with-info",
            Some(SystemLevel::Info),
            DirectSystemData::StopHookSummary {
                hook_count: serde_json::Number::from(1),
                hook_infos: vec![crate::sdk_projection::DirectStopHookInfo {
                    hook_name: "Stop".to_string(),
                    duration_ms: serde_json::Number::from(1),
                }],
                hook_errors: vec!["denied".to_string()],
                prevented_continuation: true,
                stop_reason: Some("blocked".to_string()),
                has_output: false,
                tool_use_id: None,
                hook_label: None,
                total_duration_ms: None,
            },
        );
        assert!(matches!(
            render_block_for(&stop_with_verbose_detail),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectSystem(
                    CrabCodeDirectSystemBlock::StopHookSummary {
                        hook_info_count: 1,
                        ..
                    },
                ),
            }))
        ));

        let memory = direct_system(
            "memory",
            None,
            DirectSystemData::MemorySaved {
                written_paths: vec!["/tmp/private.md".to_string(), "/tmp/team.md".to_string()],
                team_count: Some(serde_json::Number::from(1)),
            },
        );
        assert!(matches!(
            render_block_for(&memory),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectSystem(
                    CrabCodeDirectSystemBlock::MemorySaved {
                        ref written_paths,
                        team_count: Some(ref team_count),
                    },
                ),
            })) if written_paths.len() == 2 && team_count.as_u64() == Some(1)
        ));
    }

    #[test]
    fn turn_duration_maps_with_fixed_default_and_retains_mount_verb_across_revision() {
        fn completion_verb(state: &ScrollbackState) -> (&str, bool, u64) {
            let Some(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind:
                    CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::TurnDuration {
                        completion_verb,
                        show_turn_duration,
                        duration_ms,
                        ..
                    }),
            })) = state.entry(0).map(|entry| &entry.block)
            else {
                panic!("expected one typed turn-duration row");
            };
            (
                completion_verb,
                *show_turn_duration,
                duration_ms.as_u64().expect("integer duration"),
            )
        }

        let initial = direct_system(
            "turn-duration",
            None,
            DirectSystemData::TurnDuration {
                duration_ms: serde_json::Number::from(1_000),
                budget_tokens: None,
                budget_limit: None,
                budget_nudges: None,
            },
        );
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, std::slice::from_ref(&initial))
            .expect("legal turn duration has a closed consumer");
        let (mounted_verb, show_turn_duration, duration_ms) = completion_verb(&state);
        let mounted_verb = mounted_verb.to_string();
        assert!(show_turn_duration, "fixed absent-config fallback is true");
        assert_eq!(duration_ms, 1_000);

        let revised = direct_system(
            "turn-duration",
            None,
            DirectSystemData::TurnDuration {
                duration_ms: serde_json::Number::from(2_000),
                budget_tokens: Some(serde_json::Number::from(100)),
                budget_limit: Some(serde_json::Number::from(200)),
                budget_nudges: Some(serde_json::Number::from(1)),
            },
        );
        let delta = adapter
            .synchronize(&mut state, &[revised])
            .expect("same component identity accepts a projection revision");
        assert_eq!(delta.updated, 1);
        let (revised_verb, show_turn_duration, duration_ms) = completion_verb(&state);
        assert_eq!(revised_verb, mounted_verb);
        assert!(show_turn_duration);
        assert_eq!(duration_ms, 2_000);
    }

    #[test]
    fn api_error_retry_visibility_replacement_and_tail_removal_match_fixed_lifecycle() {
        fn retry(key: &str, attempt: u64, message: &str) -> ProjectedItem {
            direct_system(
                key,
                Some(SystemLevel::Error),
                DirectSystemData::ApiError {
                    error: crate::sdk_projection::DirectApiErrorPresentation {
                        message: Some(message.to_string()),
                        status: Some(serde_json::Number::from(503)),
                        nested_message: None,
                        deeply_nested_message: None,
                        connection_code: None,
                    },
                    retry_in_ms: serde_json::Number::from(5_000),
                    retry_attempt: serde_json::Number::from(attempt),
                    max_retries: serde_json::Number::from(6),
                },
            )
        }

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, &[retry("early", 3, "hidden")])
            .unwrap();
        assert!(state.is_empty(), "external attempts 1-3 render null");

        let fourth = retry("fourth", 4, "first visible");
        let fifth = retry("fifth", 5, "replacement");
        adapter
            .synchronize(&mut state, &[fourth.clone(), fifth.clone()])
            .unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(adapter.ordered_keys(), &["fifth"]);
        assert_eq!(
            state
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("replacement\n5000\n5\n6")
        );
        assert!(
            !crabcode_pager_render::scrollback::minimal::scan_frontier(&state, true).will_commit,
            "a live retry must remain retractable while the turn is running"
        );
        assert!(
            !crabcode_pager_render::scrollback::minimal::scan_frontier(&state, false).will_commit,
            "a final retry must remain retractable across an idle boundary"
        );
        let mut prematurely_committed = Vec::new();
        assert_eq!(
            crabcode_pager_render::scrollback::minimal::commit_leading_run(
                &mut state,
                false,
                |state, index| {
                    prematurely_committed.push(
                        state
                            .entry(index)
                            .and_then(|entry| entry.block.searchable_text()),
                    );
                    true
                },
            ),
            0
        );
        assert!(
            prematurely_committed.is_empty(),
            "a retry row can never enter immutable native scrollback"
        );

        let mut later = item("later", ProjectedKind::Assistant, "later response");
        later.presentation.assistant_block = Some(AssistantBlockType::Text);
        adapter
            .synchronize(&mut state, &[fourth, fifth, later])
            .unwrap();
        assert_eq!(
            state.len(),
            1,
            "a later message removes every API retry row"
        );
        assert_eq!(adapter.ordered_keys(), &["later"]);
        let mut committed_after_retraction = Vec::new();
        assert_eq!(
            crabcode_pager_render::scrollback::minimal::commit_leading_run(
                &mut state,
                false,
                |state, index| {
                    committed_after_retraction.push(
                        state
                            .entry(index)
                            .and_then(|entry| entry.block.searchable_text()),
                    );
                    true
                },
            ),
            1
        );
        assert_eq!(
            committed_after_retraction,
            vec![Some("later response".to_string())],
            "only the later successful response may enter native scrollback"
        );
    }

    #[test]
    fn queued_command_uses_special_user_text_and_plain_image_labels_for_all_metadata() {
        let queued = direct_attachment(
            "queued-special",
            ProjectedKind::User,
            DirectAttachmentData::QueuedCommand {
                text: "<local-command-stdout>done</local-command-stdout>".to_string(),
                image_paste_ids: vec![7, 9],
                command_mode: Some("task-notification".to_string()),
                is_meta: Some(true),
                origin: Some(crate::sdk_projection::DirectMessageOriginKind::Coordinator),
            },
        );
        let block = render_block_for(&queued).expect("fixed queued-command consumer");
        assert_eq!(
            block.searchable_text().as_deref(),
            Some("done\n[Image #7]\n[Image #9]")
        );
        let output = block.output(&BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        });
        let rendered = output
            .lines
            .iter()
            .map(|line| {
                line.content
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("done"));
        assert!(rendered.contains("#7]"));
        assert!(rendered.contains("#9]"));
    }

    #[test]
    fn consecutive_labeled_stop_hooks_collapse_and_expand_as_fixed_blank_details() {
        let stop = |key: &str, count: u64, error: Option<&str>| {
            direct_system(
                key,
                Some(SystemLevel::Info),
                DirectSystemData::StopHookSummary {
                    hook_count: serde_json::Number::from(count),
                    hook_infos: vec![crate::sdk_projection::DirectStopHookInfo {
                        hook_name: "producer-name-is-not-read-by-fixed-component".to_string(),
                        duration_ms: serde_json::Number::from(12),
                    }],
                    hook_errors: error.into_iter().map(str::to_string).collect(),
                    prevented_continuation: false,
                    stop_reason: None,
                    has_output: false,
                    tool_use_id: None,
                    hook_label: Some("PostToolUse".to_string()),
                    total_duration_ms: Some(serde_json::Number::from(12)),
                },
            )
        };
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(
                &mut state,
                &[stop("first", 1, None), stop("second", 2, Some("failed"))],
            )
            .unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(adapter.ordered_keys(), &["first"]);
        let output = state.entry(0).unwrap().block.output(&BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        });
        let rendered = output
            .lines
            .iter()
            .map(|line| line.content.to_string())
            .collect::<Vec<_>>();
        assert!(rendered[0].contains("3 PostToolUse {count}"));
        assert_eq!(
            rendered.iter().filter(|line| line.trim() == "⏿").count(),
            2,
            "fixed generated component reads absent command/promptText aliases"
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("producer-name-is-not-read"))
        );
    }

    #[test]
    fn in_process_teammate_completion_uses_native_subagent_lifecycle() {
        let teammate = direct_attachment(
            "teammate",
            ProjectedKind::System,
            DirectAttachmentData::TaskStatus {
                task_id: "task-1".to_string(),
                task_type: crate::sdk_projection::DirectTaskType::InProcessTeammate,
                status: DirectTaskStatus::Completed,
                description: "review".to_string(),
            },
        );
        assert!(matches!(
            render_block_for(&teammate),
            Ok(RenderBlock::Subagent(block))
                if block.child_session_id == "task-1"
                    && block.description == "review"
                    && matches!(block.kind, SubagentBlockKind::Completed { .. })
        ));
    }

    #[test]
    fn same_key_update_and_streaming_final_preserve_identity_and_view_state() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let first = thinking("thinking", "one", true);
        adapter
            .synchronize(&mut state, std::slice::from_ref(&first))
            .unwrap();
        let id = adapter.entry_id("thinking").unwrap();
        state.set_selected(Some(0));
        {
            let entry = state.get_by_id_mut(id).unwrap();
            entry.set_display_mode(DisplayMode::Expanded);
            entry.display_mode_pinned = true;
            entry.toggle_raw();
        }

        let final_item = thinking("thinking", "one two", false);
        let delta = adapter
            .synchronize(&mut state, std::slice::from_ref(&final_item))
            .unwrap();

        assert_eq!(
            delta,
            ProjectionScrollbackDelta {
                updated: 1,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert_eq!(adapter.entry_id("thinking"), Some(id));
        let entry = state.get_by_id(id).unwrap();
        assert!(!entry.is_running);
        assert_eq!(entry.display_mode(), DisplayMode::Expanded);
        assert!(entry.display_mode_pinned);
        assert!(entry.raw);
        assert_eq!(state.selected(), Some(0));
        assert!(matches!(
            &entry.block,
            RenderBlock::Thinking(block) if block.text() == "one two"
        ));
    }

    #[test]
    fn delete_and_reorder_keep_surviving_ids_and_selected_identity() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let initial = vec![
            item("a", ProjectedKind::User, "a"),
            assistant("b", "b", false),
            item("c", ProjectedKind::User, "c"),
        ];
        adapter.synchronize(&mut state, &initial).unwrap();
        let a = adapter.entry_id("a").unwrap();
        let b = adapter.entry_id("b").unwrap();
        let c = adapter.entry_id("c").unwrap();
        state.set_selected(Some(1));

        let desired = vec![
            item("c", ProjectedKind::User, "c"),
            assistant("b", "b", false),
        ];
        let delta = adapter.synchronize(&mut state, &desired).unwrap();

        assert_eq!(
            delta,
            ProjectionScrollbackDelta {
                removed: 1,
                unchanged: 2,
                reordered: true,
                ..ProjectionScrollbackDelta::default()
            }
        );
        assert_eq!(adapter.entry_id("a"), None);
        assert_eq!(adapter.entry_id("b"), Some(b));
        assert_eq!(adapter.entry_id("c"), Some(c));
        assert!(state.get_by_id(a).is_none());
        assert_eq!(state.entry(0).map(|entry| entry.id), Some(c));
        assert_eq!(state.entry(1).map(|entry| entry.id), Some(b));
        assert_eq!(state.selected(), Some(1));
        assert_eq!(adapter.ordered_keys(), &["c".to_string(), "b".to_string()]);
    }

    #[test]
    fn unclosed_specialized_consumer_is_atomic() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let initial = vec![item("user", ProjectedKind::User, "kept")];
        adapter.synchronize(&mut state, &initial).unwrap();
        let id = adapter.entry_id("user").unwrap();
        let generation = state.content_generation();

        let system = item("system", ProjectedKind::System, "untyped");
        let error = adapter.synchronize(&mut state, &[initial[0].clone(), system]);

        assert!(matches!(
            error,
            Err(ProjectionScrollbackError::UnclosedConsumer {
                kind: ProjectedKind::System,
                consumer: "untyped system",
                ..
            })
        ));
        assert_eq!(state.len(), 1);
        assert_eq!(state.entry(0).map(|entry| entry.id), Some(id));
        assert_eq!(state.content_generation(), generation);
        assert_eq!(adapter.ordered_keys(), &["user".to_string()]);
    }

    #[test]
    fn owner_correlated_tool_result_updates_one_native_invocation_row() {
        let mut invocation = typed_tool(
            "tool-use",
            ProjectedKind::ToolUse,
            Some("Read"),
            Some(serde_json::json!({"file_path": "src/lib.rs"})),
            None,
            None,
        );
        invocation.tool_use_id = Some("tool-1".to_string());
        invocation.streaming = true;
        invocation.presentation.assistant_block = Some(AssistantBlockType::ToolUse);

        let mut result = typed_tool(
            "tool-result",
            ProjectedKind::ToolResult,
            Some("Read"),
            None,
            Some(serde_json::json!("file contents")),
            Some(false),
        );
        result.text = "file contents".to_string();
        result.tool_use_id = Some("tool-1".to_string());

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, &[invocation, result])
            .unwrap();

        assert_eq!(
            state.len(),
            1,
            "commit result is merged into its live owner"
        );
        assert_eq!(adapter.entry_id("tool-result"), None);
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::ToolCall(ToolCallBlock::Read(block)))
                if block.path == "src/lib.rs"
                    && block.content.as_deref() == Some("file contents")
        ));
        assert!(state.entry(0).is_some_and(|entry| !entry.is_running));
    }

    #[test]
    fn tool_result_containment_does_not_hide_non_callback_projection_errors() {
        let projected = item("system", ProjectedKind::System, "untyped");
        assert!(matches!(
            prepare_render_block(&projected),
            Err(ProjectionScrollbackError::UnclosedConsumer {
                kind: ProjectedKind::System,
                ..
            })
        ));
    }

    #[test]
    fn every_unclosed_projected_kind_fails_without_generic_fallback() {
        for kind in [
            ProjectedKind::ToolUse,
            ProjectedKind::ToolResult,
            ProjectedKind::TerminalOutput,
            ProjectedKind::System,
            ProjectedKind::Progress,
            ProjectedKind::Warning,
            ProjectedKind::Error,
        ] {
            let projected = item("candidate", kind, "payload");
            assert!(matches!(
                render_block_for(&projected),
                Err(ProjectionScrollbackError::UnclosedConsumer {
                    kind: observed,
                    ..
                }) if observed == kind
            ));
        }
    }

    #[test]
    fn typed_closed_consumers_dispatch_without_title_or_text_inference() {
        let mut advisor = item("advisor", ProjectedKind::ToolResult, "misleading fallback");
        advisor.title = "Not advisor".to_string();
        advisor.presentation.advisor = Some(AdvisorPresentation::Result(
            AdvisorResultPresentation::Feedback {
                text: "typed feedback".to_string(),
            },
        ));
        assert!(matches!(
            render_block_for(&advisor),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Feedback { ref text }),
            })) if text == "typed feedback"
        ));

        let mut system = item("system", ProjectedKind::Warning, "retry payload");
        system.title = "misleading generic title".to_string();
        system.presentation.system = Some(SystemPresentation {
            subtype: ProjectedSystemSubtype::Sdk(SystemSubtype::ApiRetry),
            level: Some(SystemLevel::Warning),
            direct: None,
        });
        assert!(matches!(
            render_block_for(&system),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::SdkSystem(CrabCodeSdkSystemBlock {
                    subtype: CrabCodeSdkSystemSubtype::ApiRetry,
                    tone: CrabCodeSdkSystemTone::Warning,
                    ..
                }),
            }))
        ));

        let progress = direct_progress(
            "search",
            ProjectedKind::Progress,
            "misleading fallback",
            "search_results_received",
            DirectProgressPresentation::SearchResults {
                query: "typed query".to_string(),
                result_count: 7,
            },
        );
        assert!(matches!(
            render_block_for(&progress),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectProgress(
                    CrabCodeDirectProgressBlock::SearchResults {
                        ref query,
                        result_count: 7,
                    },
                ),
            })) if query == "typed query"
        ));

        let attachment = direct_attachment(
            "directory",
            ProjectedKind::System,
            DirectAttachmentData::Directory {
                display_path: "typed/path".to_string(),
            },
        );
        assert!(matches!(
            render_block_for(&attachment),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectAttachment(
                    CrabCodeDirectAttachmentBlock::Directory { ref display_path },
                ),
            })) if display_path == "typed/path"
        ));
    }

    #[test]
    fn sdk_hook_response_error_uses_typed_subtype_and_error_kind() {
        let mut system = item(
            "hook-response-error",
            ProjectedKind::Error,
            "misleading fallback text",
        );
        system.title = "misleading generic title".to_string();
        system.presentation.system = Some(SystemPresentation {
            subtype: ProjectedSystemSubtype::Sdk(SystemSubtype::HookResponse),
            level: Some(SystemLevel::Error),
            direct: None,
        });

        assert!(matches!(
            render_block_for(&system),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::SdkSystem(CrabCodeSdkSystemBlock {
                    subtype: CrabCodeSdkSystemSubtype::HookResponse,
                    tone: CrabCodeSdkSystemTone::Error,
                    level: Some(CrabCodeMessageLevel::Error),
                    ..
                }),
            }))
        ));
    }

    #[test]
    fn unknown_assistant_tool_uses_native_other_without_losing_input() {
        let input = serde_json::json!({"path": "src/lib.rs", "line": 7});
        let mut invocation = typed_tool(
            "tool-use",
            ProjectedKind::ToolUse,
            Some("RepoProbe"),
            Some(input.clone()),
            None,
            None,
        );
        invocation.presentation.assistant_block = Some(AssistantBlockType::ToolUse);
        let expected_summary = input.to_string();

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "RepoProbe" && block.summary == expected_summary
        ));
    }

    #[test]
    fn exact_edit_invocation_enters_fixed_diff_block_without_backend_lookup() {
        let mut invocation = typed_tool(
            "edit",
            ProjectedKind::ToolUse,
            Some("Edit"),
            Some(serde_json::json!({
                "file_path": "src/lib.rs",
                "old_string": "same\nold\n",
                "new_string": "same\nnew\n",
                "replace_all": false
            })),
            None,
            None,
        );
        invocation.presentation.assistant_block = Some(AssistantBlockType::ToolUse);

        let block = render_block_for(&invocation).expect("exact Edit input maps");
        assert!(matches!(
            &block,
            RenderBlock::ToolCall(ToolCallBlock::Edit(edit))
                if edit.path == "src/lib.rs"
                    && edit.copy_text().contains("\n-old\n+new\n")
        ));

        let output = block.output(&BlockContext {
            mode: DisplayMode::Expanded,
            is_running: false,
            width: 80,
            raw: false,
            max_lines: None,
            appearance: AppearanceConfig::default(),
            is_selected: false,
            cwd: None,
        });
        let theme = Theme::current();
        assert!(
            output
                .lines
                .iter()
                .any(|line| line.background == Some(theme.diff_delete_bg))
        );
        assert!(
            output
                .lines
                .iter()
                .any(|line| line.background == Some(theme.diff_insert_bg))
        );
    }

    #[test]
    fn incomplete_edit_shape_degrades_locally_to_native_other() {
        let input = serde_json::json!({
            "file_path": "src/lib.rs",
            "old_string": "old"
        });
        let mut invocation = typed_tool(
            "edit-incomplete",
            ProjectedKind::ToolUse,
            Some("Edit"),
            Some(input.clone()),
            None,
            None,
        );
        invocation.presentation.assistant_block = Some(AssistantBlockType::ToolUse);
        let expected_summary = input.to_string();

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "Edit" && block.summary == expected_summary
        ));
    }

    #[test]
    fn streaming_partial_json_remains_visible_in_native_other() {
        let mut invocation = typed_tool(
            "stream-tool-use",
            ProjectedKind::ToolUse,
            Some("RepoProbe"),
            None,
            None,
            None,
        );
        invocation.streaming = true;
        invocation.presentation.assistant_block = Some(AssistantBlockType::ToolUse);
        invocation
            .presentation
            .tool
            .as_mut()
            .unwrap()
            .partial_input_json = Some("{\"path\":\"src".to_string());

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "RepoProbe" && block.summary == "{\"path\":\"src"
        ));
    }

    #[test]
    fn orphan_user_tool_result_uses_native_other_and_preserves_payload() {
        let result = typed_tool(
            "tool-result",
            ProjectedKind::ToolResult,
            Some("RepoProbe"),
            None,
            Some(serde_json::json!({"matches": ["a", "b"]})),
            Some(false),
        );

        assert!(matches!(
            render_block_for(&result),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "RepoProbe"
                    && block.output.as_deref().is_some_and(|output| output.contains("\"matches\""))
                    && block.error.is_none()
        ));
    }

    #[test]
    fn advisor_invocation_preserves_exact_input_and_resolved_state() {
        let input = serde_json::json!({
            "focus": ["safety", "correctness"],
            "nested": {"preserved": true}
        });
        let mut invocation = item(
            "advisor-invocation",
            ProjectedKind::ToolUse,
            "fallback must not select advisor rendering",
        );
        invocation.presentation.advisor = Some(AdvisorPresentation::Invocation {
            input: input.clone(),
            state: AdvisorInvocationState::Succeeded,
        });

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::Advisor(
                    CrabCodeAdvisorBlock::Invocation {
                        input: CrabCodeToolPayload::Json(ref encoded),
                        state: CrabCodeAdvisorInvocationState::Succeeded,
                    },
                ),
            })) if serde_json::from_str::<serde_json::Value>(encoded).ok() == Some(input)
        ));
    }

    #[test]
    fn direct_success_without_truthy_result_remains_visible_as_native_other() {
        for (key, renderer_result) in [
            ("missing-result", None),
            ("null-result", Some(serde_json::Value::Null)),
            ("false-result", Some(serde_json::Value::Bool(false))),
            (
                "empty-result",
                Some(serde_json::Value::String(String::new())),
            ),
        ] {
            let result = direct_tool_result(key, renderer_result.clone(), renderer_result, None);
            assert!(matches!(
                render_block_for(&result),
                Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                    if block.name == "RepoProbe"
            ));
        }
    }

    #[test]
    fn direct_success_preserves_lossless_result_in_native_other() {
        let renderer_result = serde_json::json!({
            "matches":["a","b"],
            "nested":{"preserved":true}
        });
        let result = direct_tool_result(
            "direct-result",
            Some(renderer_result.clone()),
            Some(renderer_result),
            Some(false),
        );

        assert!(matches!(
            render_block_for(&result),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.output.as_deref().is_some_and(|output|
                    output.contains("\"preserved\": true") && output.contains("\"matches\""))
                    && block.error.is_none()
        ));
    }

    #[test]
    fn direct_error_preserves_block_content_in_native_other() {
        let result = direct_tool_result(
            "direct-error",
            Some(serde_json::json!({"must_not_render":"success payload"})),
            Some(serde_json::Value::String("exact error content".to_string())),
            Some(true),
        );

        assert!(matches!(
            render_block_for(&result),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.output.as_deref() == Some("exact error content")
                    && block.error.as_deref() == Some("exact error content")
        ));
    }

    #[test]
    fn terminal_tool_result_preserves_text_error_in_native_other() {
        let result = typed_tool(
            "terminal-result",
            ProjectedKind::TerminalOutput,
            Some("Bash"),
            None,
            Some(serde_json::Value::String("exit 7".to_string())),
            Some(true),
        );

        assert!(matches!(
            render_block_for(&result),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "Bash"
                    && block.output.as_deref() == Some("exit 7")
                    && block.error.as_deref() == Some("exit 7")
        ));
    }

    #[test]
    fn sdk_tool_progress_uses_typed_tool_identity_before_display_detail() {
        let progress = typed_tool(
            "tool-progress",
            ProjectedKind::Progress,
            Some("RepoProbe"),
            None,
            None,
            None,
        );

        assert!(matches!(
            render_block_for(&progress),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Progress {
                    ref name,
                    ref detail,
                }),
            })) if name == "RepoProbe"
                && detail == "display text must not select the renderer"
        ));
    }

    #[test]
    fn uncorrelated_user_tool_result_is_visible_in_native_other() {
        let result = typed_tool(
            "uncorrelated-result",
            ProjectedKind::ToolResult,
            None,
            None,
            Some(serde_json::Value::String("orphan".to_string())),
            Some(false),
        );

        assert!(matches!(
            render_block_for(&result),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "Uncorrelated tool result"
                    && block.output.as_deref() == Some("display text must not select the renderer")
        ));
    }

    #[test]
    fn sdk_read_tool_use_maps_to_native_read() {
        let invocation = typed_tool(
            "sdk-user-tool-use",
            ProjectedKind::ToolUse,
            Some("Read"),
            Some(serde_json::json!({"file_path": "README.md"})),
            None,
            None,
        );

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Read(block)))
                if block.path == "README.md"
        ));
    }

    #[test]
    fn non_advisor_server_tool_use_is_visible_in_native_other() {
        let mut invocation = typed_tool(
            "server-tool-use",
            ProjectedKind::ToolUse,
            Some("remote_lookup"),
            Some(serde_json::json!({"query": "typed"})),
            None,
            None,
        );
        invocation.presentation.assistant_block = Some(AssistantBlockType::ServerToolUse);

        assert!(matches!(
            render_block_for(&invocation),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "remote_lookup" && block.summary.contains("typed")
        ));
    }

    #[test]
    fn synthetic_compaction_cannot_silently_disappear_through_source_null() {
        let mut compaction = item(
            "assistant-compaction",
            ProjectedKind::Assistant,
            "invalid synthetic compaction row",
        );
        compaction.presentation.assistant_block = Some(AssistantBlockType::Compaction);

        assert!(matches!(
            render_block_for(&compaction),
            Err(ProjectionScrollbackError::UnclosedConsumer {
                consumer: "assistant compaction",
                ..
            })
        ));
    }

    #[test]
    fn direct_user_standard_capabilities_use_native_visible_routes() {
        let mut document = item("direct-document", ProjectedKind::User, "document body");
        document.presentation.direct_user = Some(direct_user_presentation(
            "direct-document",
            DirectUserBlockType::Document,
        ));
        assert!(matches!(
            render_block_for(&document),
            Ok(RenderBlock::UserPrompt(_))
        ));

        let mut direct_thinking = thinking("direct-thinking", "reason", false);
        direct_thinking.presentation.direct_user = Some(direct_user_presentation(
            "direct-thinking",
            DirectUserBlockType::Thinking,
        ));
        assert!(matches!(
            render_block_for(&direct_thinking),
            Ok(RenderBlock::Thinking(_))
        ));

        let mut read = typed_tool(
            "direct-read",
            ProjectedKind::ToolUse,
            Some("Read"),
            Some(serde_json::json!({"file_path": "README.md"})),
            None,
            None,
        );
        read.presentation.direct_user = Some(direct_user_presentation(
            "direct-read",
            DirectUserBlockType::ToolUse,
        ));
        assert!(matches!(
            render_block_for(&read),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Read(block)))
                if block.path == "README.md"
        ));

        let mut connector = item("direct-connector", ProjectedKind::User, "connector text");
        connector.presentation.direct_user = Some(direct_user_presentation(
            "direct-connector",
            DirectUserBlockType::ConnectorText,
        ));
        assert!(matches!(
            render_block_for(&connector),
            Ok(RenderBlock::UserPrompt(_))
        ));
    }

    #[test]
    fn assistant_container_upload_remains_visible_in_native_other() {
        let mut upload = typed_tool(
            "assistant-container-upload",
            ProjectedKind::ToolResult,
            Some("container_upload"),
            None,
            Some(serde_json::json!({"file_id": "file-1"})),
            Some(false),
        );
        upload.presentation.assistant_block = Some(AssistantBlockType::ContainerUpload);

        assert!(matches!(
            render_block_for(&upload),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.name == "container_upload"
                    && block.output.as_deref().is_some_and(|output| output.contains("file-1"))
        ));
    }

    #[test]
    fn assistant_server_mcp_and_web_lifecycles_use_native_visible_blocks() {
        let mut server = typed_tool(
            "assistant-server",
            ProjectedKind::ToolUse,
            Some("remote_lookup"),
            Some(serde_json::json!({"query": "typed"})),
            None,
            None,
        );
        server.presentation.assistant_block = Some(AssistantBlockType::ServerToolUse);
        assert!(matches!(
            render_block_for(&server),
            Ok(RenderBlock::ToolCall(ToolCallBlock::Other(_)))
        ));

        let mut mcp = typed_tool(
            "assistant-mcp",
            ProjectedKind::ToolUse,
            Some("mcp__github__search"),
            Some(serde_json::json!({"query": "renderer"})),
            None,
            None,
        );
        mcp.presentation.assistant_block = Some(AssistantBlockType::McpToolUse);
        assert!(matches!(
            render_block_for(&mcp),
            Ok(RenderBlock::ToolCall(ToolCallBlock::UseTool(block)))
                if block.tool_name == "github__search"
        ));

        let mut search = typed_tool(
            "web-search-use",
            ProjectedKind::ToolUse,
            Some("web_search"),
            Some(serde_json::json!({"query": "CrabCode renderer"})),
            None,
            None,
        );
        search.tool_use_id = Some("web-1".to_string());
        search.streaming = true;
        search.presentation.assistant_block = Some(AssistantBlockType::ServerToolUse);

        let mut result = typed_tool(
            "web-search-result",
            ProjectedKind::ToolResult,
            Some("web_search"),
            None,
            Some(serde_json::json!({
                "content": "search body",
                "url": "https://example.invalid/source"
            })),
            Some(false),
        );
        result.text = "search body".to_string();
        result.tool_use_id = Some("web-1".to_string());
        result.presentation.assistant_block = Some(AssistantBlockType::WebSearchToolResult);

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter.synchronize(&mut state, &[search, result]).unwrap();
        assert_eq!(state.len(), 1);
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::ToolCall(ToolCallBlock::WebSearch(block)))
                if block.query == "CrabCode renderer"
                    && block.content.as_deref() == Some("search body")
                    && block.citations == ["https://example.invalid/source"]
        ));
    }

    #[test]
    fn sdk_base64_image_maps_only_from_typed_media_provenance() {
        let mut image = item("sdk-base64-image", ProjectedKind::User, "misleading");
        image.presentation.image = Some(ImageProvenance::Base64 {
            media_type: ImageMediaType::Webp,
            encoded_len: 321,
        });

        assert!(matches!(
            render_block_for(&image),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::SdkImage(CrabCodeSdkImageBlock::Base64 {
                    media_type: CrabCodeSdkImageMediaType::Webp,
                    encoded_len: 321,
                }),
            }))
        ));
    }

    #[test]
    fn sdk_url_image_maps_only_from_typed_url_provenance() {
        let mut image = item("sdk-url-image", ProjectedKind::User, "misleading");
        image.presentation.image = Some(ImageProvenance::Url {
            url: "https://example.invalid/image.png".to_string(),
        });

        assert!(matches!(
            render_block_for(&image),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::SdkImage(CrabCodeSdkImageBlock::Url {
                    ref url,
                }),
            })) if url == "https://example.invalid/image.png"
        ));
    }

    #[test]
    fn sdk_file_image_maps_only_from_typed_file_provenance() {
        let mut image = item("sdk-file-image", ProjectedKind::User, "misleading");
        image.presentation.image = Some(ImageProvenance::File {
            file_id: "file_123".to_string(),
        });

        assert!(matches!(
            render_block_for(&image),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::SdkImage(CrabCodeSdkImageBlock::File {
                    ref file_id,
                }),
            })) if file_id == "file_123"
        ));
    }

    #[test]
    fn direct_image_uses_typed_historical_ordinal_only() {
        let mut image = item("image", ProjectedKind::User, "base64 must not be rendered");
        image.presentation.image = Some(ImageProvenance::Base64 {
            media_type: ImageMediaType::Png,
            encoded_len: 8,
        });
        image.presentation.direct_user = Some(DirectUserPresentation {
            identity: DirectMessageIdentity {
                uuid: "image-message".to_string(),
            },
            timestamp: "2026-07-28T00:00:00Z".to_string(),
            is_meta: None,
            is_visible_in_transcript_only: None,
            is_compact_summary: None,
            source_tool_use_id: None,
            origin: None,
            compact_summary: None,
            plan_content: None,
            tool_use_result: None,
            block_type: DirectUserBlockType::Image,
            image_paste_id: None,
            render_image_id: Some(4),
        });

        assert!(matches!(
            render_block_for(&image),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::UserImage { image_id: Some(4) },
            }))
        ));
    }

    #[test]
    fn nested_progress_requires_group_preparation_and_task_status_uses_native_subagents() {
        let nested = direct_progress(
            "nested",
            ProjectedKind::Assistant,
            "nested message fallback",
            "agent_progress",
            DirectProgressPresentation::Nested {
                progress_type: "agent_progress".to_string(),
                parent_tool_use_id: "parent".to_string(),
                progress_tool_use_id: "progress".to_string(),
                prompt: "prompt".to_string(),
                agent_id: "agent".to_string(),
                message_kind: crate::sdk_projection::DirectNestedMessageKind::Assistant,
                usage: None,
            },
        );
        assert!(matches!(
            render_block_for(&nested),
            Err(ProjectionScrollbackError::InconsistentPresentation {
                expected: "nested progress prepared by the outer-message lifecycle",
                ..
            })
        ));

        let queued = direct_attachment(
            "queued",
            ProjectedKind::User,
            DirectAttachmentData::QueuedCommand {
                text: "queued".to_string(),
                image_paste_ids: vec![1],
                command_mode: None,
                is_meta: None,
                origin: None,
            },
        );
        assert!(matches!(
            render_block_for(&queued),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::DirectAttachment(
                    CrabCodeDirectAttachmentBlock::QueuedCommand {
                        ref image_paste_ids,
                        text_is_hidden: false,
                        ..
                    },
                ),
            })) if image_paste_ids == &[1]
        ));

        for task_type in [
            DirectTaskType::LocalAgent,
            DirectTaskType::InProcessTeammate,
        ] {
            let task = direct_attachment(
                "agent-task",
                ProjectedKind::System,
                DirectAttachmentData::TaskStatus {
                    task_id: "task".to_string(),
                    task_type,
                    status: DirectTaskStatus::Running,
                    description: "work".to_string(),
                },
            );
            assert!(matches!(
                render_block_for(&task),
                Ok(RenderBlock::Subagent(block))
                    if block.child_session_id == "task"
                        && block.description == "work"
                        && matches!(block.kind, SubagentBlockKind::Started)
            ));
        }
    }

    #[test]
    fn workflow_progress_and_terminal_task_status_share_native_workflow_lifecycle() {
        let mut workflow = direct_progress(
            "workflow",
            ProjectedKind::Progress,
            "writer: running",
            "workflow_progress",
            DirectProgressPresentation::Workflow {
                task_id: "local_workflow_1".to_string(),
                workflow: "deep-research".to_string(),
                phase: Some("Synthesize".to_string()),
                phase_index: 1,
                message: "writer: running".to_string(),
                agents_started: 3,
                agents_completed: 1,
                phases: vec![
                    crate::sdk_projection::DirectWorkflowPhase {
                        index: 0,
                        title: "Discover".to_string(),
                        state: DirectWorkflowPhaseState::Done,
                    },
                    crate::sdk_projection::DirectWorkflowPhase {
                        index: 1,
                        title: "Synthesize".to_string(),
                        state: DirectWorkflowPhaseState::Active,
                    },
                ],
                status: DirectWorkflowStatus::Running,
            },
        );
        assert!(matches!(
            render_block_for(&workflow),
            Ok(RenderBlock::Workflow(block))
                if block.run_id == "local_workflow_1"
                    && block.name == "deep-research"
                    && block.objective == "writer: running"
                    && block.active_agents == 2
                    && block.phases.len() == 2
                    && matches!(block.status, WorkflowBlockStatus::Running)
        ));

        workflow.streaming = false;
        if let Some(DirectProgressPresentation::Workflow { status, .. }) =
            workflow.presentation.direct_progress.as_mut()
        {
            *status = DirectWorkflowStatus::Completed;
        }
        workflow.presentation.direct_attachment = Some(DirectAttachmentPresentation {
            identity: DirectMessageIdentity {
                uuid: "workflow-terminal".to_string(),
            },
            data: DirectAttachmentData::TaskStatus {
                task_id: "local_workflow_1".to_string(),
                task_type: DirectTaskType::LocalWorkflow,
                status: DirectTaskStatus::Completed,
                description: "Deep research".to_string(),
            },
        });
        assert!(matches!(
            render_block_for(&workflow),
            Ok(RenderBlock::Workflow(block))
                if block.active_agents == 0
                    && matches!(block.status, WorkflowBlockStatus::Done { .. })
        ));

        let standalone = direct_attachment(
            "standalone-workflow",
            ProjectedKind::System,
            DirectAttachmentData::TaskStatus {
                task_id: "local_workflow_2".to_string(),
                task_type: DirectTaskType::LocalWorkflow,
                status: DirectTaskStatus::Failed,
                description: "Workflow failed".to_string(),
            },
        );
        assert!(matches!(
            render_block_for(&standalone),
            Ok(RenderBlock::Workflow(block))
                if block.run_id == "local_workflow_2"
                    && matches!(block.status, WorkflowBlockStatus::Failed { .. })
        ));
    }

    #[test]
    fn in_stream_error_uses_native_turn_failed_event() {
        let mut error = item("stream-error", ProjectedKind::Error, "gateway overloaded");
        error.presentation.stream_error = Some(crate::sdk_projection::StreamErrorPresentation {
            error_type: Some("overloaded_error".to_string()),
            error_code: Some("NEXUS_OVERLOADED".to_string()),
            message: "gateway overloaded".to_string(),
        });
        assert!(matches!(
            render_block_for(&error),
            Ok(RenderBlock::SessionEvent(block))
                if matches!(
                    block.event,
                    SessionEvent::TurnFailed { ref error, elapsed: None }
                        if error.contains("gateway overloaded")
                            && error.contains("overloaded_error")
                            && error.contains("NEXUS_OVERLOADED")
                )
        ));
    }

    #[test]
    fn agent_nested_progress_preserves_prompt_groups_and_selection_identity() {
        let parent = "agent-tool";
        let marker = nested_progress_item(
            item("agent-user-marker", ProjectedKind::Progress, ""),
            0,
            "agent_progress",
            parent,
            DirectNestedMessageKind::User,
            "inspect **all** files",
        );
        let mut hidden_tool = typed_tool(
            "agent-group-1-tool",
            ProjectedKind::ToolUse,
            Some("Read"),
            Some(serde_json::json!({"file_path":"one"})),
            None,
            None,
        );
        hidden_tool.presentation.assistant_block = Some(AssistantBlockType::ToolUse);
        hidden_tool.streaming = true;
        let hidden_tool = nested_progress_item(
            hidden_tool,
            1,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let hidden_text = nested_progress_item(
            assistant("agent-group-2-text", "second", false),
            2,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let mut hidden_nonstandard_tool = typed_tool(
            "agent-group-2-server-tool",
            ProjectedKind::ToolUse,
            Some("server_lookup"),
            Some(serde_json::json!({"query":"two"})),
            None,
            None,
        );
        hidden_nonstandard_tool.presentation.assistant_block =
            Some(AssistantBlockType::ServerToolUse);
        let hidden_nonstandard_tool = nested_progress_item(
            hidden_nonstandard_tool,
            2,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let mut visible_tool = typed_tool(
            "agent-group-3-tool",
            ProjectedKind::ToolUse,
            Some("Read"),
            Some(serde_json::json!({"file_path":"three"})),
            None,
            None,
        );
        visible_tool.presentation.assistant_block = Some(AssistantBlockType::ToolUse);
        visible_tool.streaming = true;
        let visible_tool = nested_progress_item(
            visible_tool,
            3,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let visible_detail = nested_progress_item(
            assistant("agent-group-3-text", "third detail", false),
            3,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let fourth_thinking = nested_progress_item(
            thinking("agent-group-4-thinking", "private reasoning", false),
            4,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let fourth = nested_progress_item(
            assistant("agent-group-4-text", "fourth", false),
            4,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let fifth = nested_progress_item(
            assistant("agent-group-5-text", "fifth", false),
            5,
            "agent_progress",
            parent,
            DirectNestedMessageKind::Assistant,
            "inspect **all** files",
        );
        let items = vec![
            marker,
            hidden_tool,
            hidden_text,
            hidden_nonstandard_tool,
            visible_tool,
            visible_detail,
            fourth_thinking,
            fourth,
            fifth,
        ];
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();

        adapter
            .synchronize_with_options(&mut state, &items, SynchronizationOptions::default())
            .unwrap();
        assert_eq!(
            state.len(),
            9,
            "the native prompt and complete child lifecycle remain visible"
        );
        assert!(
            state.entry(1).is_some_and(|entry| entry.is_running),
            "the visible unresolved tool invocation retains running state"
        );
        assert!(
            state.entry(8).is_some_and(|entry| !entry.is_running),
            "complete assistant message text is static"
        );
        assert!(adapter.entry_id("agent-group-4-thinking").is_some());
        let fifth_id = adapter.entry_id("agent-group-5-text").unwrap();
        state.set_selected(state.index_of_id(fifth_id));

        adapter
            .synchronize_with_options(
                &mut state,
                &items,
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: true,
                },
            )
            .unwrap();
        assert_eq!(
            state.len(),
            9,
            "transcript mode does not duplicate the native prompt or child lifecycle"
        );
        assert_eq!(adapter.entry_id("agent-group-5-text"), Some(fifth_id));
        assert_eq!(
            state
                .selected()
                .and_then(|index| state.entry(index))
                .map(|entry| entry.id),
            Some(fifth_id)
        );
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(_))
        ));
        assert!(adapter.entry_id("agent-group-4-thinking").is_some());

        adapter
            .synchronize_with_options(
                &mut state,
                &items,
                SynchronizationOptions {
                    presentation_verbose: true,
                    agent_transcript_mode: false,
                },
            )
            .unwrap();
        assert_eq!(
            state.len(),
            9,
            "verbose does not delete or fork the native child lifecycle"
        );
        assert!(adapter.entry_id("agent-group-4-thinking").is_some());
    }

    #[test]
    fn renderer_context_revision_rebuilds_prompt_and_source_in_place() {
        let message = nested_progress_item(
            assistant("agent-context-message", "renderer context", false),
            1,
            "agent_progress",
            "agent-tool",
            DirectNestedMessageKind::Assistant,
            "inspect",
        );
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();

        let initial = adapter
            .synchronize_with_options(
                &mut state,
                std::slice::from_ref(&message),
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: false,
                },
            )
            .unwrap();
        assert_eq!(initial.appended, 2);
        assert!(matches!(
            state.entry(1).map(|entry| &entry.block),
            Some(RenderBlock::AgentMessage(_))
        ));
        let entry_id = adapter.entry_id("agent-context-message").unwrap();
        state.set_selected(state.index_of_id(entry_id));

        let revised = adapter
            .synchronize_with_options(
                &mut state,
                std::slice::from_ref(&message),
                SynchronizationOptions {
                    presentation_verbose: true,
                    agent_transcript_mode: false,
                },
            )
            .unwrap();
        assert_eq!(revised.updated, 2);
        assert_eq!(revised.unchanged, 0);
        assert_eq!(
            adapter.entry_id("agent-context-message"),
            Some(entry_id),
            "renderer-context replacement preserves stable entry identity"
        );
        assert_eq!(
            state
                .selected()
                .and_then(|index| state.entry(index))
                .map(|entry| entry.id),
            Some(entry_id),
            "renderer-context replacement preserves selection identity"
        );
        assert!(matches!(
            state.entry(1).map(|entry| &entry.block),
            Some(RenderBlock::AgentMessage(_))
        ));
    }

    #[test]
    fn agent_nested_prompt_is_stable_and_empty_prompt_uses_initializing() {
        let marker = nested_progress_item(
            item("agent-prompt-marker", ProjectedKind::Progress, ""),
            1,
            "agent_progress",
            "agent-tool",
            DirectNestedMessageKind::User,
            "inspect",
        );
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, std::slice::from_ref(&marker))
            .unwrap();
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(_))
        ));

        adapter
            .synchronize_with_options(
                &mut state,
                &[marker],
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: true,
                },
            )
            .unwrap();
        assert_eq!(state.len(), 1);
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::UserPrompt(_))
        ));

        let empty_first_prompt = nested_progress_item(
            item("agent-empty-first-prompt", ProjectedKind::Progress, ""),
            2,
            "agent_progress",
            "second-agent-tool",
            DirectNestedMessageKind::User,
            "",
        );
        let later_prompt = nested_progress_item(
            assistant("agent-later-prompt", "visible", false),
            3,
            "agent_progress",
            "second-agent-tool",
            DirectNestedMessageKind::Assistant,
            "must not be promoted",
        );
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize_with_options(
                &mut state,
                &[empty_first_prompt, later_prompt],
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: true,
                },
            )
            .unwrap();
        assert_eq!(state.len(), 1);
        assert!(
            !(0..state.len()).any(|index| {
                matches!(
                    state.entry(index).map(|entry| &entry.block),
                    Some(RenderBlock::UserPrompt(_))
                )
            }),
            "fixed AgentTool reads only progressMessages[0].data.prompt"
        );
    }

    #[test]
    fn nested_progress_rejects_divergent_outer_progress_tool_identity_atomically() {
        let mut nested = nested_progress_item(
            assistant("nested-identity", "content", false),
            1,
            "agent_progress",
            "agent-tool",
            DirectNestedMessageKind::Assistant,
            "inspect",
        );
        nested
            .presentation
            .direct_progress_identity
            .as_mut()
            .unwrap()
            .tool_use_id = "different-progress-tool".to_string();
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        assert!(matches!(
            adapter.synchronize(&mut state, &[nested]),
            Err(ProjectionScrollbackError::InconsistentPresentation {
                expected: "nested presentation and complete outer progress identity agreement",
                ..
            })
        ));
        assert!(state.is_empty());
        assert!(adapter.ordered_keys().is_empty());
    }

    #[test]
    fn skill_nested_progress_preserves_every_native_lifecycle_row() {
        let mut items = (0_u64..3)
            .map(|sequence| {
                nested_progress_item(
                    assistant(
                        &format!("skill-group-{sequence}"),
                        &format!("message {sequence}\nsecond visible source row"),
                        false,
                    ),
                    sequence,
                    "skill_progress",
                    "skill-tool",
                    DirectNestedMessageKind::Assistant,
                    "",
                )
            })
            .collect::<Vec<_>>();
        let user_tool_result = typed_tool(
            "skill-group-3-user-result",
            ProjectedKind::ToolResult,
            Some("Read"),
            None,
            Some(serde_json::json!("done")),
            Some(false),
        );
        items.push(nested_progress_item(
            user_tool_result,
            3,
            "skill_progress",
            "skill-tool",
            DirectNestedMessageKind::User,
            "",
        ));
        items.push(nested_progress_item(
            assistant(
                "skill-group-4-visible",
                "visible child output\n\nsecond row",
                false,
            ),
            4,
            "skill_progress",
            "skill-tool",
            DirectNestedMessageKind::Assistant,
            "",
        ));

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize_with_options(
                &mut state,
                &items,
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: true,
                },
            )
            .unwrap();

        assert_eq!(state.len(), 5, "all five child groups remain owned");
        assert!(matches!(
            state.entry(0).map(|entry| &entry.block),
            Some(RenderBlock::AgentMessage(_))
        ));
        assert_eq!(
            state
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("message 0 second visible source row")
        );
        assert!(matches!(
            state.entry(3).map(|entry| &entry.block),
            Some(RenderBlock::ToolCall(ToolCallBlock::Other(block)))
                if block.output.as_deref() == Some("done")
        ));
        assert_eq!(
            state
                .entry(4)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("visible child output\n\nsecond row")
        );

        adapter
            .synchronize_with_options(
                &mut state,
                &items,
                SynchronizationOptions {
                    presentation_verbose: true,
                    agent_transcript_mode: false,
                },
            )
            .unwrap();
        assert_eq!(
            state.len(),
            5,
            "verbose does not rebuild or duplicate native child groups"
        );
    }

    #[test]
    fn direct_user_text_uses_the_fixed_user_text_route_and_redacted_thinking_stays_typed() {
        let mut user = item("direct-user", ProjectedKind::User, "<local-command>");
        user.presentation.direct_user = Some(DirectUserPresentation {
            identity: DirectMessageIdentity {
                uuid: "direct-user".to_string(),
            },
            timestamp: "2026-07-28T00:00:00Z".to_string(),
            is_meta: None,
            is_visible_in_transcript_only: None,
            is_compact_summary: None,
            source_tool_use_id: None,
            origin: None,
            compact_summary: None,
            plan_content: None,
            tool_use_result: None,
            block_type: DirectUserBlockType::Text,
            image_paste_id: None,
            render_image_id: None,
        });
        assert!(matches!(
            render_block_for(&user),
            Ok(RenderBlock::UserPrompt(_))
        ));

        let mut redacted = thinking("redacted", "ciphertext", false);
        redacted.presentation.assistant_block = Some(AssistantBlockType::RedactedThinking);
        redacted.presentation.thinking = Some(ThinkingPresentation {
            kind: ThinkingKind::Redacted,
            content: "ciphertext".to_string(),
            signature: None,
        });
        redacted.presentation.direct_assistant = Some(DirectAssistantPresentation {
            identity: DirectMessageIdentity {
                uuid: "assistant".to_string(),
            },
            timestamp: "2026-07-28T00:00:00Z".to_string(),
            request_id: None,
            is_api_error_message: None,
            advisor_model: None,
            message_id: "message".to_string(),
            model: "model".to_string(),
            usage: None,
        });
        assert!(matches!(
            render_block_for(&redacted),
            Ok(RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock {
                kind: CrabCodeProjectionKind::RedactedThinking,
            }))
        ));
    }

    #[test]
    fn observed_unknown_slash_command_hides_meta_caveat_and_keeps_tui_alive() {
        let mut projection = crate::sdk_projection::Projection::default();
        let caveat = serde_json::json!({
            "type":"user",
            "uuid":"8373d6f4-be8f-4b4d-9740-6696934319aa",
            "timestamp":"2026-07-30T02:45:53.000Z",
            "isMeta":true,
            "message":{
                "role":"user",
                "content":"<local-command-caveat>Caveat: generated locally.</local-command-caveat>"
            }
        });
        let unknown = serde_json::json!({
            "type":"user",
            "uuid":"unknown-skill-m",
            "timestamp":"2026-07-30T02:45:53.001Z",
            "message":{"role":"user","content":"Unknown skill: m"}
        });
        assert!(matches!(
            projection.ingest(raw_direct(13, caveat)),
            crate::sdk_projection::ProjectionEffect::None
        ));
        assert!(matches!(
            projection.ingest(raw_direct(14, unknown)),
            crate::sdk_projection::ProjectionEffect::None
        ));

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, projection.items())
            .expect("the observed local-command sequence must remain renderable");

        assert_eq!(state.len(), 1, "the meta caveat must not consume a row");
        assert_eq!(
            state
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("Unknown skill: m")
        );
    }

    #[test]
    fn sdk_model_switch_replay_strips_renderer_markup_without_exiting() {
        let mut projection = crate::sdk_projection::Projection::default();
        let replay = serde_json::json!({
            "type":"user",
            "message":{
                "role":"user",
                "content":"<local-command-stdout>Set model to deepseek-v4-flash</local-command-stdout>"
            },
            "session_id":"session-model-switch",
            "parent_tool_use_id":null,
            "uuid":"model-output",
            "timestamp":"2026-07-30T02:46:00.002Z",
            "isReplay":true
        });
        assert!(matches!(
            projection.ingest(raw_direct(0, replay)),
            crate::sdk_projection::ProjectionEffect::None
        ));
        assert!(
            projection.items()[0].presentation.direct_user.is_none(),
            "the real SDKUserMessageReplay shape is not a direct-query user"
        );
        assert_eq!(projection.items()[0].kind, ProjectedKind::User);
        assert_eq!(projection.items()[0].title, "User (history)");
        assert_eq!(projection.items()[0].tool_use_id, None);

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, projection.items())
            .expect("model-switch breadcrumb sequence must remain renderable");

        let searchable = (0..state.len())
            .filter_map(|index| state.entry(index)?.block.searchable_text())
            .collect::<Vec<_>>();
        assert_eq!(
            searchable,
            vec!["Set model to deepseek-v4-flash".to_string()]
        );
        assert!(
            searchable
                .iter()
                .all(|text| !text.contains("<command-") && !text.contains("<local-command-")),
            "renderer-only XML must never leak into scrollback"
        );
    }

    #[test]
    fn local_command_output_routing_keeps_the_fixed_starts_with_boundary() {
        let routed = map_historical_user_text(
            concat!(
                "<local-command-stderr>second</local-command-stderr>",
                "<local-command-stdout>first</local-command-stdout>",
            ),
            None,
        );
        assert!(
            matches!(&routed, RenderBlock::LocalCommandOutput(_)),
            "tagged output must enter the dedicated historical renderer"
        );
        assert_eq!(
            routed.searchable_text().as_deref(),
            Some("first\nsecond"),
            "the historical renderer consumes stdout before stderr"
        );

        for content in [
            " <local-command-stdout>not a routed output</local-command-stdout>",
            "ordinary text <local-command-stderr>also not routed</local-command-stderr>",
        ] {
            let block = map_historical_user_text(content, None);
            assert!(
                matches!(&block, RenderBlock::UserPrompt(_)),
                "historical UserTextMessage uses startsWith, not includes: {content}"
            );
            assert_eq!(block.searchable_text().as_deref(), Some(content));
        }
    }

    #[test]
    fn system_local_command_uses_the_same_user_text_route() {
        let mut projection = crate::sdk_projection::Projection::default();
        let command = serde_json::json!({
            "type":"system",
            "subtype":"local_command",
            "uuid":"system-model-command",
            "timestamp":"2026-07-30T02:46:01.000Z",
            "isMeta":false,
            "level":"info",
            "content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args>custom:model</command-args>"
        });
        assert!(matches!(
            projection.ingest(raw_direct(0, command)),
            crate::sdk_projection::ProjectionEffect::None
        ));

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize(&mut state, projection.items())
            .expect("system/local_command must not hit an unclosed consumer");
        assert_eq!(
            state
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("/model custom:model")
        );
    }

    #[test]
    fn transcript_only_direct_user_text_tracks_ctrl_o_without_protocol_state() {
        let mut user = item("transcript-only", ProjectedKind::User, "history detail");
        let mut presentation =
            direct_user_presentation("transcript-only", DirectUserBlockType::Text);
        presentation.is_visible_in_transcript_only = Some(true);
        user.presentation.direct_user = Some(presentation);

        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        adapter
            .synchronize_with_options(
                &mut state,
                std::slice::from_ref(&user),
                SynchronizationOptions::default(),
            )
            .unwrap();
        assert_eq!(state.len(), 0);

        adapter
            .synchronize_with_options(
                &mut state,
                &[user],
                SynchronizationOptions {
                    presentation_verbose: false,
                    agent_transcript_mode: true,
                },
            )
            .unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(
            state
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("history detail")
        );
    }

    #[test]
    fn red_consumer_preflight_preserves_identity_selection_fold_raw_and_generation() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let initial = thinking("thinking", "kept", false);
        adapter
            .synchronize(&mut state, std::slice::from_ref(&initial))
            .unwrap();
        let id = adapter.entry_id("thinking").unwrap();
        state.set_selected(Some(0));
        {
            let entry = state.get_by_id_mut(id).unwrap();
            entry.set_display_mode(DisplayMode::Expanded);
            entry.display_mode_pinned = true;
            entry.toggle_raw();
        }
        let generation = state.content_generation();
        let nested = direct_progress(
            "nested",
            ProjectedKind::Assistant,
            "nested",
            "agent_progress",
            DirectProgressPresentation::Nested {
                progress_type: "agent_progress".to_string(),
                parent_tool_use_id: "parent".to_string(),
                progress_tool_use_id: "progress".to_string(),
                prompt: "prompt".to_string(),
                agent_id: "agent".to_string(),
                message_kind: crate::sdk_projection::DirectNestedMessageKind::Assistant,
                usage: None,
            },
        );

        assert!(adapter.synchronize(&mut state, &[initial, nested]).is_err());
        assert_eq!(state.len(), 1);
        assert_eq!(state.entry(0).map(|entry| entry.id), Some(id));
        let entry = state.get_by_id(id).unwrap();
        assert_eq!(entry.display_mode(), DisplayMode::Expanded);
        assert!(entry.display_mode_pinned);
        assert!(entry.raw);
        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.content_generation(), generation);
        assert_eq!(adapter.ordered_keys(), &["thinking".to_string()]);
    }

    #[test]
    fn duplicate_key_preflight_is_atomic() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        let duplicate = item("same", ProjectedKind::User, "one");
        let error = adapter.synchronize(&mut state, &[duplicate.clone(), duplicate]);
        assert!(matches!(
            error,
            Err(ProjectionScrollbackError::DuplicateKey { ref key }) if key == "same"
        ));
        assert!(state.is_empty());
        assert!(adapter.ordered_keys().is_empty());
    }

    #[test]
    fn external_state_entry_is_rejected_before_projection_work() {
        let mut adapter = ProjectionScrollbackAdapter::default();
        let mut state = ScrollbackState::new();
        state.push_block(RenderBlock::system("foreign"));

        let error = adapter.synchronize(
            &mut state,
            &[item("candidate", ProjectedKind::User, "text")],
        );
        assert_eq!(
            error,
            Err(ProjectionScrollbackError::StateOwnershipDiverged {
                tracked: 0,
                state: 1,
            })
        );
        assert_eq!(state.len(), 1);
    }
}
