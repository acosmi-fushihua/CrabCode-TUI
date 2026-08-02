//! Fixed session-picker presentation component.
//!
//! This module is deliberately an injected-data UI boundary. CrabCode's
//! historical session catalog, transcript loading, rename, tag, and fork
//! operations are backend/session-storage authorities; the Rust renderer must
//! not reproduce them by scanning or mutating `~/.crabcode`. The bundled
//! direct runtime injects those facts through the process-private startup
//! setup lifecycle; no backend or public SDK capability is added here.

use std::collections::{HashMap, HashSet};

use crabcode_pager_render::audited_theme::{CrabCodeTheme, CrabCodeThemeKind};
use crabcode_pager_render::scrollback::{ScratchBuffer, ScrollbackPane};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde::Deserialize;
use unicode_width::UnicodeWidthStr as _;

use crate::picker_surface::{
    PickerEntry, PickerHitAreas, PickerOutcome, PickerRow, PickerState, PickerStateProductExt,
    handle_picker_input, picker_config_default, render_divider, render_picker_content,
    render_picker_search_bar_with_label,
};
use crate::scrollback_projection::project_scrollback_snapshot;
use crate::sdk_projection::ProjectedItem;
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::tui_app::UiLanguage;

pub const INITIAL_PAGE_SIZE: usize = 50;
const PANEL_CHROME_ROWS: u16 = 4;

/// One authority-supplied picker row. Every field is presentation-only.
///
/// `id` and `group_id` are opaque identities. The component never interprets
/// them as filesystem paths, session UUIDs, or backend arguments.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionPickerEntry {
    pub id: String,
    pub title: String,
    /// Authority-computed historical search corpus (display title, branch,
    /// tag, and PR metadata). It is presentation-only and not interpreted as
    /// a session identifier.
    pub search_text: String,
    pub metadata: String,
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub group_id: Option<String>,
    pub in_current_worktree: bool,
}

/// Product actions emitted by the renderer and consumed by an existing
/// CrabCode session authority. No action writes storage or expands wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerAction {
    None,
    SelectRequested { id: String },
    PreviewRequested { id: String },
    PreviewClosed,
    RenameRequested { id: String, title: String },
    LoadMoreRequested { count: usize },
    ReloadRequested { all_projects: bool },
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPickerUpdate {
    pub needs_frame: bool,
    pub stop_batch: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum SessionPickerView {
    Loading,
    List,
    Rename {
        id: String,
        original_title: String,
        editor: PickerState,
    },
    PreviewLoading {
        id: String,
    },
    Preview {
        id: String,
        items: Vec<ProjectedItem>,
        metadata: String,
        message_count: usize,
        branch: Option<String>,
        scroll: usize,
        rendered_line_count: usize,
    },
    CrossProject {
        command: String,
        copied: bool,
    },
}

#[derive(Debug, Clone)]
struct VisibleEntry {
    entry_index: usize,
    indent: u8,
    collapsible: bool,
    expanded: bool,
    fork_count: usize,
}

#[derive(Debug, Clone)]
struct SelectionAnchor {
    id: Option<String>,
    fallback_index: usize,
    scroll_delta: Option<isize>,
}

/// Reusable fixed-picker state/action/render component.
///
/// The sole AppView owner calls `prepare_frame`, routes normalized input to
/// `handle_event`, and calls `render` inside its frame transaction.
#[derive(Debug)]
pub struct SessionPickerComponent {
    entries: Vec<SessionPickerEntry>,
    state: PickerState,
    show_all_projects: bool,
    show_all_worktrees: bool,
    branch_filter_enabled: bool,
    current_branch: Option<String>,
    has_multiple_worktrees: bool,
    rename_enabled: bool,
    selected_tag_index: usize,
    expanded_group_ids: HashSet<String>,
    has_more: bool,
    load_more_pending: bool,
    cancelled_preview_id: Option<String>,
    view: SessionPickerView,
}

impl SessionPickerComponent {
    pub fn new(
        entries: Vec<SessionPickerEntry>,
        has_more: bool,
        current_branch: Option<String>,
        has_multiple_worktrees: bool,
    ) -> Self {
        Self {
            entries,
            state: PickerState::default(),
            show_all_projects: false,
            show_all_worktrees: false,
            branch_filter_enabled: false,
            current_branch,
            has_multiple_worktrees,
            rename_enabled: true,
            selected_tag_index: 0,
            expanded_group_ids: HashSet::new(),
            has_more,
            load_more_pending: false,
            cancelled_preview_id: None,
            view: SessionPickerView::List,
        }
    }

    pub fn loading(initial_search: Option<&str>) -> Self {
        let mut picker = Self::new(Vec::new(), false, None, false);
        picker.state = initial_search
            .filter(|query| !query.is_empty())
            .map_or_else(PickerState::default, PickerState::input_with_query);
        picker.view = SessionPickerView::Loading;
        picker
    }

    pub fn replace_catalog(
        &mut self,
        entries: Vec<SessionPickerEntry>,
        has_more: bool,
        show_all_projects: bool,
        current_branch: Option<String>,
        has_multiple_worktrees: bool,
        rename_enabled: bool,
    ) {
        self.show_all_projects = show_all_projects;
        self.current_branch = current_branch;
        self.has_multiple_worktrees = has_multiple_worktrees;
        self.rename_enabled = rename_enabled;
        self.view = SessionPickerView::List;
        self.replace_entries(entries, has_more);
    }

    pub fn append_catalog(
        &mut self,
        entries: Vec<SessionPickerEntry>,
        has_more: bool,
        show_all_projects: bool,
        current_branch: Option<String>,
        has_multiple_worktrees: bool,
        rename_enabled: bool,
    ) {
        self.show_all_projects = show_all_projects;
        self.current_branch = current_branch;
        self.has_multiple_worktrees = has_multiple_worktrees;
        self.rename_enabled = rename_enabled;
        self.view = SessionPickerView::List;
        self.append_entries(entries, has_more);
    }

    pub fn begin_loading(&mut self) {
        self.view = SessionPickerView::Loading;
        self.load_more_pending = false;
    }

    pub fn show_cross_project(&mut self, command: String, copied: bool) {
        self.view = SessionPickerView::CrossProject { command, copied };
    }

    pub const fn show_all_projects(&self) -> bool {
        self.show_all_projects
    }

    pub fn preview_loading_id(&self) -> Option<&str> {
        match &self.view {
            SessionPickerView::PreviewLoading { id } => Some(id),
            _ => None,
        }
    }

    pub fn take_cancelled_preview(&mut self, id: &str) -> bool {
        if self.cancelled_preview_id.as_deref() != Some(id) {
            return false;
        }
        self.cancelled_preview_id = None;
        true
    }

    /// Replace an authority-supplied page while preserving selection by opaque
    /// entry identity.
    pub fn replace_entries(&mut self, entries: Vec<SessionPickerEntry>, has_more: bool) {
        let anchor = self.capture_selection();
        self.entries = entries;
        self.has_more = has_more;
        self.load_more_pending = false;
        self.clamp_tag_index();
        self.restore_selection(anchor);
    }

    /// Append a progressive page supplied by the authority. Duplicate opaque
    /// identities are ignored; ordering remains producer-defined.
    pub fn append_entries(&mut self, entries: Vec<SessionPickerEntry>, has_more: bool) {
        let anchor = self.capture_selection();
        let mut seen = self
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<HashSet<_>>();
        self.entries.extend(
            entries
                .into_iter()
                .filter(|entry| seen.insert(entry.id.clone())),
        );
        self.has_more = has_more;
        self.load_more_pending = false;
        self.clamp_tag_index();
        self.restore_selection(anchor);
    }

    /// Complete the previously requested preview with already-authoritative
    /// projected transcript items. Rendering reuses the connected TUI's full
    /// transcript item renderer.
    pub fn complete_preview(
        &mut self,
        id: &str,
        items: Vec<ProjectedItem>,
        metadata: String,
        message_count: usize,
        branch: Option<String>,
    ) -> bool {
        if !matches!(&self.view, SessionPickerView::PreviewLoading { id: pending } if pending == id)
        {
            return false;
        }
        self.view = SessionPickerView::Preview {
            id: id.to_string(),
            items,
            metadata,
            message_count,
            branch,
            scroll: 0,
            rendered_line_count: 0,
        };
        true
    }

    /// Fail a preview request without inventing a fallback transcript.
    pub fn fail_preview(&mut self, id: &str) -> bool {
        if !matches!(&self.view, SessionPickerView::PreviewLoading { id: pending } if pending == id)
        {
            return false;
        }
        self.view = SessionPickerView::List;
        true
    }

    /// Derive the fixed historical progressive-load request before a frame.
    pub fn prepare_frame(&mut self, terminal_rows: u16) -> SessionPickerAction {
        if !matches!(self.view, SessionPickerView::List) || !self.has_more || self.load_more_pending
        {
            return SessionPickerAction::None;
        }
        let visible = self.visible_entries();
        let (_, buffer, load_count) = progressive_window(terminal_rows);
        if self.state.selected.saturating_add(buffer) < visible.len() {
            return SessionPickerAction::None;
        }
        self.load_more_pending = true;
        SessionPickerAction::LoadMoreRequested { count: load_count }
    }

    pub fn handle_event(&mut self, event: &Event) -> (SessionPickerAction, SessionPickerUpdate) {
        match &mut self.view {
            SessionPickerView::Rename { .. } => return self.handle_rename_event(event),
            SessionPickerView::PreviewLoading { id } if is_plain_key(event, KeyCode::Esc) => {
                self.cancelled_preview_id = Some(id.clone());
                self.view = SessionPickerView::List;
                return changed(SessionPickerAction::None, true);
            }
            SessionPickerView::Loading
            | SessionPickerView::PreviewLoading { .. }
            | SessionPickerView::CrossProject { .. } => return unchanged(),
            SessionPickerView::Preview { .. } => return self.handle_preview_event(event),
            SessionPickerView::List => {}
        }

        // Progressive catalog expansion is serialized through the currently
        // held setup interaction. Keep the historical list visible while the
        // authority loads the next page, but do not emit a second interaction
        // until the append completes and releases that fence.
        if self.load_more_pending {
            return unchanged();
        }

        if self.entries.is_empty() {
            if is_ctrl_char(event, 'c') {
                return changed(SessionPickerAction::Cancelled, true);
            }
            return unchanged();
        }

        if !self.state.search_active && is_ctrl_char(event, 'a') {
            self.show_all_projects = !self.show_all_projects;
            let all_projects = self.show_all_projects;
            self.begin_loading();
            return changed(SessionPickerAction::ReloadRequested { all_projects }, true);
        }
        if !self.state.search_active && self.current_branch.is_some() && is_ctrl_char(event, 'b') {
            let anchor = self.capture_selection();
            self.branch_filter_enabled = !self.branch_filter_enabled;
            self.restore_selection(anchor);
            return changed(SessionPickerAction::None, false);
        }
        if !self.state.search_active && self.has_multiple_worktrees && is_ctrl_char(event, 'w') {
            let anchor = self.capture_selection();
            self.show_all_worktrees = !self.show_all_worktrees;
            self.restore_selection(anchor);
            return changed(SessionPickerAction::None, false);
        }

        let visible = self.visible_entries();
        let focused = visible
            .get(self.state.selected)
            .and_then(|visible| self.entries.get(visible.entry_index))
            .cloned();
        if !self.state.search_active
            && is_ctrl_char(event, 'v')
            && let Some(entry) = focused
        {
            self.view = SessionPickerView::PreviewLoading {
                id: entry.id.clone(),
            };
            return changed(SessionPickerAction::PreviewRequested { id: entry.id }, true);
        }
        if self.rename_enabled
            && !self.state.search_active
            && is_ctrl_char(event, 'r')
            && let Some(entry) = focused
        {
            self.view = SessionPickerView::Rename {
                id: entry.id,
                original_title: entry.title,
                editor: PickerState::input_active(),
            };
            return changed(SessionPickerAction::None, true);
        }

        let tags = self.tag_labels();
        let tag_refs = tags.iter().map(String::as_str).collect::<Vec<_>>();
        let mut config = picker_config_default();
        config.show_search_hint = true;
        config.expandable = true;
        config.tabs = (tag_refs.len() > 1).then_some(tag_refs.as_slice());
        config.active_tab = self
            .selected_tag_index
            .min(tag_refs.len().saturating_sub(1));
        let anchor = self.capture_selection();
        let outcome = handle_picker_input(event, &mut self.state, visible.len(), &config);
        match outcome {
            PickerOutcome::Selected(index) => visible
                .get(index)
                .and_then(|item| self.entries.get(item.entry_index))
                .map_or_else(unchanged, |entry| {
                    changed(
                        SessionPickerAction::SelectRequested {
                            id: entry.id.clone(),
                        },
                        true,
                    )
                }),
            PickerOutcome::Closed => changed(SessionPickerAction::Cancelled, true),
            PickerOutcome::TabChanged(index) => {
                self.selected_tag_index = index.min(tags.len().saturating_sub(1));
                self.restore_selection(anchor);
                changed(SessionPickerAction::None, false)
            }
            PickerOutcome::Expand(index) => {
                if let Some(item) = visible.get(index)
                    && item.collapsible
                {
                    let group = group_key(&self.entries[item.entry_index]);
                    self.expanded_group_ids.insert(group);
                }
                changed(SessionPickerAction::None, false)
            }
            PickerOutcome::Collapse(index) => {
                if let Some(item) = visible.get(index) {
                    let group = group_key(&self.entries[item.entry_index]);
                    self.expanded_group_ids.remove(&group);
                    self.restore_selection(anchor);
                }
                changed(SessionPickerAction::None, false)
            }
            PickerOutcome::Changed | PickerOutcome::QueryChanged => {
                changed(SessionPickerAction::None, false)
            }
            PickerOutcome::Unchanged
            | PickerOutcome::Copy(_)
            | PickerOutcome::SubmitQuery
            | PickerOutcome::FilterCycled
            | PickerOutcome::Action(_)
            | PickerOutcome::NonSelectableClick(_) => unchanged(),
        }
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut Frame<'_>,
        theme: &CrabCodeTheme,
        _theme_kind: CrabCodeThemeKind,
        _syntax_highlighting_disabled: bool,
        language: UiLanguage,
    ) {
        match &mut self.view {
            SessionPickerView::Loading => render_loading(
                frame,
                theme,
                language.text("正在加载会话…", "Loading sessions…"),
            ),
            SessionPickerView::List if self.entries.is_empty() => {
                render_empty(frame, theme, language);
            }
            SessionPickerView::List => {
                let visible = self.visible_entries();
                let tags = self.tag_labels();
                render_list(
                    frame,
                    theme,
                    &self.entries,
                    &visible,
                    &mut self.state,
                    self.show_all_projects,
                    self.show_all_worktrees,
                    self.has_multiple_worktrees,
                    self.branch_filter_enabled,
                    self.current_branch.as_deref(),
                    &tags,
                    self.selected_tag_index,
                    self.rename_enabled,
                    language,
                );
            }
            SessionPickerView::Rename {
                original_title,
                editor,
                ..
            } => render_rename(frame, theme, original_title, editor, language),
            SessionPickerView::PreviewLoading { .. } => {
                render_preview_loading(frame, theme, language)
            }
            SessionPickerView::Preview {
                items,
                metadata,
                message_count,
                branch,
                scroll,
                rendered_line_count,
                ..
            } => render_preview(
                frame,
                theme,
                items,
                metadata,
                *message_count,
                branch.as_deref(),
                scroll,
                rendered_line_count,
                language,
            ),
            SessionPickerView::CrossProject { command, copied } => {
                render_cross_project(frame, theme, command, *copied, language)
            }
        }
    }

    fn handle_rename_event(&mut self, event: &Event) -> (SessionPickerAction, SessionPickerUpdate) {
        let SessionPickerView::Rename { id, editor, .. } = &mut self.view else {
            return unchanged();
        };
        if is_plain_key(event, KeyCode::Esc) {
            self.view = SessionPickerView::List;
            return changed(SessionPickerAction::None, true);
        }
        if is_plain_key(event, KeyCode::Enter) {
            let id = id.clone();
            let title = editor.query().trim().to_string();
            if title.is_empty() {
                return unchanged();
            }
            self.view = SessionPickerView::List;
            return changed(SessionPickerAction::RenameRequested { id, title }, true);
        }
        let config = picker_config_default();
        let outcome = handle_picker_input(event, editor, 0, &config);
        if matches!(
            outcome,
            PickerOutcome::Changed | PickerOutcome::QueryChanged
        ) {
            changed(SessionPickerAction::None, false)
        } else {
            unchanged()
        }
    }

    fn handle_preview_event(
        &mut self,
        event: &Event,
    ) -> (SessionPickerAction, SessionPickerUpdate) {
        let SessionPickerView::Preview {
            id,
            scroll,
            rendered_line_count,
            ..
        } = &mut self.view
        else {
            return unchanged();
        };
        if is_plain_key(event, KeyCode::Esc) {
            self.view = SessionPickerView::List;
            return changed(SessionPickerAction::PreviewClosed, true);
        }
        if is_plain_key(event, KeyCode::Enter) {
            return changed(
                SessionPickerAction::SelectRequested { id: id.clone() },
                true,
            );
        }
        let before = *scroll;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll
                        .saturating_add(1)
                        .min(rendered_line_count.saturating_sub(1));
                }
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => {
                    *scroll = scroll
                        .saturating_add(10)
                        .min(rendered_line_count.saturating_sub(1));
                }
                KeyCode::Home => *scroll = 0,
                KeyCode::End => *scroll = rendered_line_count.saturating_sub(1),
                _ => {}
            },
            _ => {}
        }
        if *scroll != before {
            changed(SessionPickerAction::None, false)
        } else {
            unchanged()
        }
    }

    fn tag_labels(&self) -> Vec<String> {
        let mut tags = self
            .entries
            .iter()
            .filter_map(|entry| entry.tag.clone())
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        let mut labels = Vec::with_capacity(tags.len() + 1);
        labels.push("All".to_string());
        labels.extend(tags);
        labels
    }

    fn selected_tag(&self) -> Option<String> {
        if self.selected_tag_index == 0 {
            None
        } else {
            self.tag_labels().get(self.selected_tag_index).cloned()
        }
    }

    fn clamp_tag_index(&mut self) {
        self.selected_tag_index = self
            .selected_tag_index
            .min(self.tag_labels().len().saturating_sub(1));
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let selected_tag = self.selected_tag();
        let query = self.state.query().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.show_all_worktrees || entry.in_current_worktree)
            .filter(|(_, entry)| {
                !self.branch_filter_enabled
                    || self
                        .current_branch
                        .as_ref()
                        .is_some_and(|branch| entry.branch.as_ref() == Some(branch))
            })
            .filter(|(_, entry)| {
                selected_tag
                    .as_ref()
                    .is_none_or(|tag| entry.tag.as_ref() == Some(tag))
            })
            .filter(|(_, entry)| {
                query.is_empty() || entry.search_text.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn visible_entries(&self) -> Vec<VisibleEntry> {
        let filtered = self.filtered_indices();
        let mut groups = HashMap::<String, Vec<usize>>::new();
        for index in &filtered {
            groups
                .entry(group_key(&self.entries[*index]))
                .or_default()
                .push(*index);
        }
        let mut emitted = HashSet::new();
        let mut visible = Vec::new();
        for index in filtered {
            let group = group_key(&self.entries[index]);
            if !emitted.insert(group.clone()) {
                continue;
            }
            let members = &groups[&group];
            let expanded = self.expanded_group_ids.contains(&group);
            visible.push(VisibleEntry {
                entry_index: members[0],
                indent: 0,
                collapsible: members.len() > 1,
                expanded,
                fork_count: members.len().saturating_sub(1),
            });
            if expanded {
                visible.extend(members.iter().skip(1).map(|entry_index| VisibleEntry {
                    entry_index: *entry_index,
                    indent: 1,
                    collapsible: false,
                    expanded: false,
                    fork_count: 0,
                }));
            }
        }
        visible
    }

    fn capture_selection(&self) -> SelectionAnchor {
        let visible = self.visible_entries();
        SelectionAnchor {
            id: visible
                .get(self.state.selected)
                .and_then(|item| self.entries.get(item.entry_index))
                .map(|entry| entry.id.clone()),
            fallback_index: self.state.selected,
            scroll_delta: self
                .state
                .scroll_offset
                .map(|offset| self.state.selected as isize - offset as isize),
        }
    }

    fn restore_selection(&mut self, anchor: SelectionAnchor) {
        let visible = self.visible_entries();
        let selected = anchor
            .id
            .as_ref()
            .and_then(|id| {
                visible
                    .iter()
                    .position(|item| self.entries[item.entry_index].id == *id)
            })
            .or_else(|| (!visible.is_empty()).then(|| anchor.fallback_index.min(visible.len() - 1)))
            .unwrap_or(0);
        self.state.selected = selected;
        self.state.scroll_offset = anchor.scroll_delta.map(|delta| {
            (selected as isize - delta)
                .max(0)
                .min(visible.len().saturating_sub(1) as isize) as usize
        });
    }
}

fn group_key(entry: &SessionPickerEntry) -> String {
    entry.group_id.clone().unwrap_or_else(|| entry.id.clone())
}

fn progressive_window(terminal_rows: u16) -> (usize, usize, usize) {
    let visible = usize::from(terminal_rows.saturating_sub(PANEL_CHROME_ROWS).max(1));
    (
        visible,
        visible.saturating_mul(2),
        visible.saturating_mul(3),
    )
}

fn picker_layout(area: Rect) -> (Rect, Rect, Rect, Rect, Rect) {
    let title = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let search = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1).min(1),
    );
    let divider = Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(2).min(1),
    );
    let footer = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        area.height.min(1),
    );
    let list = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(4),
    );
    (title, search, divider, list, footer)
}

fn render_localized_search_bar(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    theme: &CrabCodeTheme,
    state: &PickerState,
    language: UiLanguage,
) {
    let label = language.text(" 搜索: ", " search: ");
    if language == UiLanguage::EnUs {
        render_picker_search_bar_with_label(
            buffer, x, y, width, theme, label, state, true, false, None,
        );
        return;
    }

    // The shared custom-label API intentionally measures ASCII labels in
    // bytes. Draw the Chinese label separately, then give that API only the
    // remaining query cells so its grapheme viewport and cursor stay aligned.
    let label_width = label.width().min(usize::from(width)) as u16;
    buffer.set_span(
        x,
        y,
        &Span::styled(label, Style::default().fg(theme.gray)),
        width,
    );
    render_picker_search_bar_with_label(
        buffer,
        x.saturating_add(label_width),
        y,
        width.saturating_sub(label_width),
        theme,
        "",
        state,
        true,
        false,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    frame: &mut Frame<'_>,
    theme: &CrabCodeTheme,
    entries: &[SessionPickerEntry],
    visible: &[VisibleEntry],
    state: &mut PickerState,
    show_all_projects: bool,
    show_all_worktrees: bool,
    has_multiple_worktrees: bool,
    branch_filter_enabled: bool,
    current_branch: Option<&str>,
    tags: &[String],
    selected_tag_index: usize,
    rename_enabled: bool,
    language: UiLanguage,
) {
    let area = frame.area();
    if area.width == 0 || area.height < PANEL_CHROME_ROWS {
        return;
    }
    let (title, search, divider, list, footer) = picker_layout(area);
    let title_text = if tags.len() > 1 {
        tags.iter()
            .enumerate()
            .map(|(index, tag)| {
                let display_tag = if index == 0 {
                    language.text("全部", "All")
                } else {
                    tag.as_str()
                };
                if index == selected_tag_index {
                    format!("[{display_tag}]")
                } else {
                    display_tag.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    } else {
        language.text("恢复会话", "Resume session").to_string()
    };
    frame.buffer_mut().set_span(
        title.x.saturating_add(1),
        title.y,
        &Span::styled(
            title_text,
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        title.width.saturating_sub(1),
    );
    render_localized_search_bar(
        frame.buffer_mut(),
        search.x.saturating_add(1),
        search.y,
        search.width.saturating_sub(1),
        theme,
        state,
        language,
    );
    render_divider(
        frame.buffer_mut(),
        divider.x,
        divider.y,
        divider.width,
        theme,
        None,
    );

    struct RowData {
        label: String,
        right: String,
    }
    let row_data = visible
        .iter()
        .map(|item| {
            let entry = &entries[item.entry_index];
            let prefix = if item.indent > 0 { "  ↳ " } else { "" };
            let suffix = if item.collapsible {
                match language {
                    UiLanguage::ZhCn => format!("（+{} 个分支）", item.fork_count),
                    UiLanguage::EnUs => format!(" ({} forks)", item.fork_count),
                }
            } else {
                String::new()
            };
            RowData {
                label: format!("{prefix}{}{suffix}", entry.title),
                right: entry.metadata.clone(),
            }
        })
        .collect::<Vec<_>>();
    let rows = visible
        .iter()
        .zip(&row_data)
        .enumerate()
        .map(|(index, (item, data))| {
            PickerEntry::Row(PickerRow {
                label: &data.label,
                right_label: &data.right,
                selected: index == state.selected,
                expanded: item.expanded,
                fields: &[],
                description_lines: &[],
                summary_lines: &[],
                dimmed: false,
                indent: item.indent,
                badge: "",
                badge_color: None,
                collapsible: item.collapsible,
                underline_last_desc: false,
            })
        })
        .collect::<Vec<_>>();
    let hit_areas = render_picker_content(
        frame.buffer_mut(),
        list,
        theme,
        state,
        &rows,
        &vec![false; rows.len()],
        &[],
        Some(theme.bg_base),
        false,
    );
    state.hit_areas = Some(PickerHitAreas {
        close_button: Rect::default(),
        search_bar: search,
        item_rects: hit_areas.item_rects,
        entry_indices: hit_areas.entry_indices,
        tab_rects: Vec::new(),
        filter_rect: None,
    });

    let mut footer_text = if show_all_projects {
        language
            .text("ctrl+a 显示当前目录", "ctrl+a show Current directory")
            .to_string()
    } else {
        language
            .text("ctrl+a 显示所有项目", "ctrl+a show All projects")
            .to_string()
    };
    footer_text.push_str(language.text(" · ctrl+b 切换分支过滤", " · ctrl+b toggle branch"));
    if has_multiple_worktrees {
        footer_text.push_str(if show_all_worktrees {
            language.text(
                " · ctrl+w 显示当前工作树",
                " · ctrl+w show Current worktree",
            )
        } else {
            language.text(" · ctrl+w 显示所有工作树", " · ctrl+w show All worktrees")
        });
    }
    footer_text.push_str(language.text(" · ctrl+v 预览", " · ctrl+v preview"));
    if rename_enabled {
        footer_text.push_str(language.text(" · ctrl+r 重命名", " · ctrl+r rename"));
    }
    footer_text
        .push_str(language.text(" · 输入以搜索 · esc 取消", " · type to search · esc cancel"));
    if branch_filter_enabled {
        footer_text.push_str(&format!(
            "{}{}",
            language.text(" · 分支 ", " · branch "),
            current_branch.unwrap_or(language.text("不可用", "unavailable"))
        ));
    }
    frame.buffer_mut().set_span(
        footer.x.saturating_add(1),
        footer.y,
        &Span::styled(footer_text, theme.dim()),
        footer.width.saturating_sub(1),
    );
}

fn render_empty(frame: &mut Frame<'_>, theme: &CrabCodeTheme, language: UiLanguage) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.buffer_mut().set_span(
        area.x,
        area.y,
        &Span::raw(language.text("未找到可恢复的会话", "No conversations found to resume")),
        area.width,
    );
    if area.height > 1 {
        frame.buffer_mut().set_span(
            area.x,
            area.y + 1,
            &Span::styled(
                language.text(
                    "按 Ctrl+C 退出并开始新会话。",
                    "Press Ctrl+C to exit and start a new conversation.",
                ),
                theme.dim(),
            ),
            area.width,
        );
    }
}

fn render_loading(frame: &mut Frame<'_>, theme: &CrabCodeTheme, message: &str) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.buffer_mut().set_span(
        area.x.saturating_add(1),
        area.y,
        &Span::styled(message, theme.dim()),
        area.width.saturating_sub(1),
    );
}

fn render_cross_project(
    frame: &mut Frame<'_>,
    theme: &CrabCodeTheme,
    command: &str,
    copied: bool,
    language: UiLanguage,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let visible_command = sanitize_bounded_terminal_text(command);
    let lines = [
        language.text(
            "此会话来自不同的目录。",
            "This conversation is from a different directory.",
        ),
        language.text("如需恢复，请运行：", "To resume, run:"),
        visible_command.as_ref(),
        if copied {
            language.text("（命令已复制到剪贴板）", "(Command copied to clipboard)")
        } else {
            language.text(
                "（无法将命令复制到剪贴板）",
                "(Unable to copy command to clipboard)",
            )
        },
    ];
    for (offset, line) in lines.into_iter().enumerate() {
        if offset >= usize::from(area.height) {
            break;
        }
        frame.buffer_mut().set_span(
            area.x.saturating_add(1),
            area.y.saturating_add(offset as u16),
            &Span::styled(
                line,
                if offset == 2 {
                    Style::default()
                } else {
                    theme.dim()
                },
            ),
            area.width.saturating_sub(1),
        );
    }
}

fn render_rename(
    frame: &mut Frame<'_>,
    theme: &CrabCodeTheme,
    original_title: &str,
    editor: &PickerState,
    language: UiLanguage,
) {
    let area = frame.area();
    if area.width < 8 || area.height < 4 {
        return;
    }
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y,
        &Span::styled(
            language.text("重命名会话", "Rename session"),
            Style::default()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
        area.width.saturating_sub(1),
    );
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y + 1,
        &Span::styled(original_title, theme.dim()),
        area.width.saturating_sub(1),
    );
    render_localized_search_bar(
        frame.buffer_mut(),
        area.x + 1,
        area.y + 2,
        area.width.saturating_sub(1),
        theme,
        editor,
        language,
    );
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y + 3,
        &Span::styled(
            language.text("enter 保存 · esc 取消", "enter save · esc cancel"),
            theme.dim(),
        ),
        area.width.saturating_sub(1),
    );
}

fn render_preview_loading(frame: &mut Frame<'_>, theme: &CrabCodeTheme, language: UiLanguage) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y,
        &Span::styled(
            language.text("正在加载会话…", "Loading session…"),
            theme.dim(),
        ),
        area.width.saturating_sub(1),
    );
    if area.height > 1 {
        frame.buffer_mut().set_span(
            area.x + 1,
            area.y + area.height - 1,
            &Span::styled(language.text("esc 取消", "esc cancel"), theme.dim()),
            area.width.saturating_sub(1),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_preview(
    frame: &mut Frame<'_>,
    theme: &CrabCodeTheme,
    items: &[ProjectedItem],
    metadata: &str,
    message_count: usize,
    branch: Option<&str>,
    scroll: &mut usize,
    rendered_line_count: &mut usize,
    language: UiLanguage,
) {
    let area = frame.area();
    if area.width < 8 || area.height < 3 {
        return;
    }
    let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
    match project_scrollback_snapshot(items) {
        Ok(scrollback) if scrollback.is_empty() => {
            *rendered_line_count = 1;
            *scroll = 0;
            frame.render_widget(
                Paragraph::new(Line::styled(
                    language.text("（无可显示的消息）", "(no displayable messages)"),
                    theme.dim(),
                )),
                body,
            );
        }
        Ok(mut scrollback) => {
            scrollback.prepare_layout(body.width.max(1), body.height);
            scrollback.set_scroll_offset(*scroll);
            let (resolved_scroll, _, total_height) = scrollback.scroll_info();
            *scroll = resolved_scroll;
            *rendered_line_count = total_height;
            let mut scratch = ScratchBuffer::new();
            ScrollbackPane::new().render_with_scratch(
                body,
                frame.buffer_mut(),
                &scrollback,
                &mut scratch,
            );
        }
        Err(_) => {
            // A preview is renderer-injected data, not a second protocol or
            // backend authority. Refuse an unclosed projection instead of
            // reviving the legacy ProjectedItem renderer as a fallback.
            *rendered_line_count = 1;
            *scroll = 0;
            frame.render_widget(
                Paragraph::new(Line::styled(
                    language.text(
                        "（预览不可用：渲染覆盖不完整）",
                        "(preview unavailable: renderer coverage is incomplete)",
                    ),
                    Style::default().fg(theme.warning),
                )),
                body,
            );
        }
    }
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y + area.height - 2,
        &Span::styled(
            match language {
                UiLanguage::ZhCn => format!(
                    "{metadata} · {message_count} 条消息{}",
                    branch.map_or_else(String::new, |branch| format!(" · {branch}"))
                ),
                UiLanguage::EnUs => format!(
                    "{metadata} · {message_count} messages{}",
                    branch.map_or_else(String::new, |branch| format!(" · {branch}"))
                ),
            },
            theme.dim(),
        ),
        area.width.saturating_sub(1),
    );
    frame.buffer_mut().set_span(
        area.x + 1,
        area.y + area.height - 1,
        &Span::styled(
            language.text(
                "↑/↓ 滚动 · enter 恢复 · esc 取消",
                "↑/↓ scroll · enter resume · esc cancel",
            ),
            theme.dim(),
        ),
        area.width.saturating_sub(1),
    );
}

fn is_plain_key(event: &Event, code: KeyCode) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind != KeyEventKind::Release
                && key.code == code
                && key.modifiers == KeyModifiers::NONE
    )
}

fn is_ctrl_char(event: &Event, value: char) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind != KeyEventKind::Release
                && key.code == KeyCode::Char(value)
                && key.modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn changed(
    action: SessionPickerAction,
    stop_batch: bool,
) -> (SessionPickerAction, SessionPickerUpdate) {
    (
        action,
        SessionPickerUpdate {
            needs_frame: true,
            stop_batch,
        },
    )
}

fn unchanged() -> (SessionPickerAction, SessionPickerUpdate) {
    (SessionPickerAction::None, SessionPickerUpdate::default())
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    fn entry(id: &str, title: &str) -> SessionPickerEntry {
        SessionPickerEntry {
            id: id.to_string(),
            title: title.to_string(),
            search_text: title.to_string(),
            metadata: "now · main".to_string(),
            tag: None,
            branch: Some("main".to_string()),
            group_id: None,
            in_current_worktree: true,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn render_text(
        picker: &mut SessionPickerComponent,
        language: UiLanguage,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    language,
                );
            })
            .expect("session-picker frame");
        terminal.backend().to_string()
    }

    #[test]
    fn injected_component_emits_actions_without_storage_authority() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "First")], false, None, false);
        let (action, update) = picker.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            SessionPickerAction::SelectRequested {
                id: "opaque-1".to_string()
            }
        );
        assert!(update.stop_batch);

        let (action, _) = picker.handle_event(&key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(action, SessionPickerAction::None);
        for character in ['n', 'e', 'w'] {
            let _ = picker.handle_event(&key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let (action, _) = picker.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            SessionPickerAction::RenameRequested {
                id: "opaque-1".to_string(),
                title: "new".to_string(),
            }
        );
    }

    #[test]
    fn fixed_progressive_ratios_and_pending_fence_are_preserved() {
        let mut picker = SessionPickerComponent::new(
            (0..10)
                .map(|index| entry(&format!("id-{index}"), &format!("Session {index}")))
                .collect(),
            true,
            None,
            false,
        );
        assert_eq!(progressive_window(24), (20, 40, 60));
        assert_eq!(
            picker.prepare_frame(24),
            SessionPickerAction::LoadMoreRequested { count: 60 }
        );
        assert_eq!(picker.prepare_frame(24), SessionPickerAction::None);
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Enter, KeyModifiers::NONE))
                .0,
            SessionPickerAction::None,
            "the visible list must not emit a second interaction while its page request is pending"
        );
        picker.append_entries(vec![entry("id-10", "Session 10")], false);
        assert_eq!(picker.prepare_frame(24), SessionPickerAction::None);
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Enter, KeyModifiers::NONE))
                .0,
            SessionPickerAction::SelectRequested {
                id: "id-0".to_string(),
            }
        );
    }

    #[test]
    fn selection_reanchors_by_opaque_identity_across_injected_pages() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("a", "A"), entry("b", "B")], false, None, false);
        let _ = picker.handle_event(&key(KeyCode::Down, KeyModifiers::NONE));
        picker.replace_entries(
            vec![entry("x", "X"), entry("a", "A"), entry("b", "B")],
            false,
        );
        assert_eq!(
            picker.visible_entries()[picker.state.selected].entry_index,
            2
        );
    }

    #[test]
    fn preview_is_authority_injected_and_uses_full_projected_item_renderer() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "First")], false, None, false);
        let (action, _) = picker.handle_event(&key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(
            action,
            SessionPickerAction::PreviewRequested {
                id: "opaque-1".to_string()
            }
        );
        assert!(picker.complete_preview(
            "opaque-1",
            vec![ProjectedItem {
                key: "assistant-1".to_string(),
                kind: crate::sdk_projection::ProjectedKind::Assistant,
                title: "Assistant".to_string(),
                text: "**rendered** transcript".to_string(),
                streaming: false,
                raw_sequences: vec![1],
                tool_use_id: None,
                presentation: crate::sdk_projection::ProjectedPresentation {
                    assistant_block: Some(crate::sdk_projection::AssistantBlockType::Text,),
                    ..Default::default()
                },
            }],
            "now · main".to_string(),
            1,
            Some("main".to_string()),
        ));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    UiLanguage::EnUs,
                );
            })
            .expect("preview frame");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("rendered"));
        assert!(rendered.contains("1 messages"));
    }

    #[test]
    fn footer_keeps_fixed_historical_action_order() {
        let mut picker = SessionPickerComponent::new(
            vec![entry("opaque-1", "First")],
            false,
            Some("main".to_string()),
            true,
        );
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    UiLanguage::EnUs,
                );
            })
            .expect("picker frame");
        let rendered = terminal.backend().to_string();
        let a = rendered.find("ctrl+a").expect("project action");
        let b = rendered.find("ctrl+b").expect("branch action");
        let w = rendered.find("ctrl+w").expect("worktree action");
        let v = rendered.find("ctrl+v").expect("preview action");
        let r = rendered.find("ctrl+r").expect("rename action");
        assert!(a < b && b < w && w < v && v < r);
    }

    #[test]
    fn resume_panel_renders_title_rows_and_footer() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "first task")], false, None, false);
        let backend = TestBackend::new(160, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    UiLanguage::EnUs,
                );
            })
            .expect("resume panel frame");

        let text = terminal.backend().to_string();
        assert!(text.contains("Resume session"), "title:\n{text}");
        assert!(text.contains("first task"), "session row:\n{text}");
        assert!(
            text.contains("ctrl+a show All projects"),
            "resume footer:\n{text}"
        );
        assert!(text.contains("esc cancel"), "resume footer:\n{text}");
        assert!(
            !text.contains("r refresh"),
            "resume footer must stay session-picker copy:\n{text}"
        );
    }

    #[test]
    fn renderer_owned_copy_uses_explicit_language_and_preserves_authority_text() {
        let mut root = entry("root", "TITLE-RAW");
        root.metadata = "META-RAW".to_string();
        root.branch = Some("BRANCH-RAW".to_string());
        root.group_id = Some("GROUP-RAW".to_string());
        let mut fork = entry("fork", "FORK-TITLE-RAW");
        fork.metadata = "FORK-META-RAW".to_string();
        fork.branch = Some("BRANCH-RAW".to_string());
        fork.group_id = Some("GROUP-RAW".to_string());

        let mut picker = SessionPickerComponent::new(
            vec![root, fork],
            false,
            Some("BRANCH-RAW".to_string()),
            true,
        );
        picker.branch_filter_enabled = true;
        let list = render_text(&mut picker, UiLanguage::ZhCn, 240, 12);
        assert!(list.contains("恢复会话"), "localized title:\n{list}");
        assert!(list.contains("TITLE-RAW"), "authority title:\n{list}");
        assert!(list.contains("META-RAW"), "authority metadata:\n{list}");
        assert!(list.contains("（+1 个分支）"), "localized forks:\n{list}");
        assert!(
            list.contains("ctrl+a 显示所有项目"),
            "localized project shortcut:\n{list}"
        );
        assert!(
            list.contains("ctrl+w 显示所有工作树"),
            "localized worktree shortcut:\n{list}"
        );
        assert!(
            list.contains("ctrl+b 切换分支过滤"),
            "localized branch shortcut:\n{list}"
        );
        assert!(
            list.contains("ctrl+v 预览"),
            "localized preview shortcut:\n{list}"
        );
        assert!(
            list.contains("ctrl+r 重命名"),
            "localized rename shortcut:\n{list}"
        );
        assert!(
            list.contains("输入以搜索"),
            "localized search shortcut:\n{list}"
        );
        assert!(
            list.contains("分支 BRANCH-RAW"),
            "authority branch:\n{list}"
        );
        assert!(list.contains("搜索:"), "localized search label:\n{list}");

        for entry in &mut picker.entries {
            entry.tag = Some("TAG-RAW".to_string());
        }
        let tagged = render_text(&mut picker, UiLanguage::ZhCn, 120, 8);
        assert!(
            tagged.contains("[全部]"),
            "localized built-in tag:\n{tagged}"
        );
        assert!(tagged.contains("TAG-RAW"), "authority tag:\n{tagged}");

        let _ = picker.handle_event(&key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let rename = render_text(&mut picker, UiLanguage::ZhCn, 100, 8);
        assert!(
            rename.contains("重命名会话"),
            "localized rename title:\n{rename}"
        );
        assert!(
            rename.contains("TITLE-RAW"),
            "authority rename title:\n{rename}"
        );
        assert!(
            rename.contains("enter 保存 · esc 取消"),
            "localized rename footer:\n{rename}"
        );

        let mut loading = SessionPickerComponent::loading(None);
        let loading_text = render_text(&mut loading, UiLanguage::ZhCn, 80, 6);
        assert!(loading_text.contains("正在加载会话…"));

        let mut empty = SessionPickerComponent::new(Vec::new(), false, None, false);
        let empty_text = render_text(&mut empty, UiLanguage::ZhCn, 100, 6);
        assert!(empty_text.contains("未找到可恢复的会话"));
        assert!(empty_text.contains("按 Ctrl+C 退出并开始新会话。"));

        let mut cross = SessionPickerComponent::new(Vec::new(), false, None, false);
        cross.view = SessionPickerView::CrossProject {
            command: "COMMAND-RAW --resume SESSION-RAW".to_string(),
            copied: false,
        };
        let cross_text = render_text(&mut cross, UiLanguage::ZhCn, 120, 8);
        assert!(cross_text.contains("此会话来自不同的目录。"));
        assert!(cross_text.contains("如需恢复，请运行："));
        assert!(cross_text.contains("COMMAND-RAW --resume SESSION-RAW"));
        assert!(cross_text.contains("（无法将命令复制到剪贴板）"));
        cross.view = SessionPickerView::CrossProject {
            command: "COMMAND-RAW --resume SESSION-RAW".to_string(),
            copied: true,
        };
        assert!(
            render_text(&mut cross, UiLanguage::ZhCn, 120, 8).contains("（命令已复制到剪贴板）")
        );

        let mut preview = SessionPickerComponent::new(
            vec![entry("preview", "PREVIEW-TITLE-RAW")],
            false,
            None,
            false,
        );
        let _ = preview.handle_event(&key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        let preview_loading = render_text(&mut preview, UiLanguage::ZhCn, 100, 8);
        assert!(preview_loading.contains("正在加载会话…"));
        assert!(preview_loading.contains("esc 取消"));
        assert!(preview.complete_preview(
            "preview",
            Vec::new(),
            "PREVIEW-META-RAW".to_string(),
            7,
            Some("PREVIEW-BRANCH-RAW".to_string()),
        ));
        let preview_text = render_text(&mut preview, UiLanguage::ZhCn, 120, 10);
        assert!(preview_text.contains("（无可显示的消息）"));
        assert!(preview_text.contains("PREVIEW-META-RAW · 7 条消息 · PREVIEW-BRANCH-RAW"));
        assert!(preview_text.contains("↑/↓ 滚动 · enter 恢复 · esc 取消"));

        let mut unavailable =
            SessionPickerComponent::new(vec![entry("bad", "BAD-PREVIEW-RAW")], false, None, false);
        let _ = unavailable.handle_event(&key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(unavailable.complete_preview(
            "bad",
            vec![ProjectedItem {
                key: String::new(),
                kind: crate::sdk_projection::ProjectedKind::Assistant,
                title: "TITLE-RAW".to_string(),
                text: "TEXT-RAW".to_string(),
                streaming: false,
                raw_sequences: vec![1],
                tool_use_id: None,
                presentation: crate::sdk_projection::ProjectedPresentation {
                    assistant_block: Some(crate::sdk_projection::AssistantBlockType::Text),
                    ..Default::default()
                },
            }],
            "META-RAW".to_string(),
            1,
            None,
        ));
        assert!(
            render_text(&mut unavailable, UiLanguage::ZhCn, 120, 8)
                .contains("（预览不可用：渲染覆盖不完整）")
        );
    }

    #[test]
    fn resume_search_uses_picker_grapheme_viewport_at_narrow_width() {
        let grapheme = "👩🏽\u{200d}💻";
        let combining = "e\u{301}";
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "match")], false, None, false);
        picker.state.set_query(format!("a{grapheme}{combining}"));
        picker.state.search_active = true;

        let area = Rect::new(0, 0, 14, 5);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    UiLanguage::EnUs,
                );
            })
            .expect("narrow resume panel frame");
        let actual = terminal.backend().buffer().clone();

        let mut expected = Buffer::empty(area);
        render_picker_search_bar_with_label(
            &mut expected,
            1,
            1,
            13,
            &CrabCodeTheme::NIGHT,
            " search: ",
            &picker.state,
            true,
            false,
            None,
        );
        for x in 1..14 {
            let actual_cell = actual.cell((x, 1)).expect("actual search cell");
            let expected_cell = expected.cell((x, 1)).expect("expected search cell");
            assert_eq!(actual_cell.symbol(), expected_cell.symbol(), "column {x}");
            assert_eq!(actual_cell.style(), expected_cell.style(), "column {x}");
        }
        let text = terminal.backend().to_string();
        assert!(text.contains(grapheme), "ZWJ grapheme was split: {text:?}");
        assert!(
            text.contains(combining),
            "combining grapheme was split: {text:?}"
        );
        assert_eq!(
            actual.cell((13, 1)).expect("cursor cell").bg,
            CrabCodeTheme::NIGHT.text_primary
        );
    }

    #[test]
    fn chinese_search_label_keeps_grapheme_viewport_and_cursor_cell_aligned() {
        let grapheme = "👩🏽\u{200d}💻";
        let combining = "e\u{301}";
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "match")], false, None, false);
        picker.state.set_query(format!("a{grapheme}{combining}"));
        picker.state.search_active = true;

        let backend = TestBackend::new(14, 5);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                picker.render(
                    frame,
                    &CrabCodeTheme::NIGHT,
                    CrabCodeThemeKind::Dark,
                    false,
                    UiLanguage::ZhCn,
                );
            })
            .expect("narrow Chinese resume panel frame");

        let text = terminal.backend().to_string();
        assert!(text.contains("搜索:"), "localized search label: {text:?}");
        assert!(text.contains(grapheme), "ZWJ grapheme was split: {text:?}");
        assert!(
            text.contains(combining),
            "combining grapheme was split: {text:?}"
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((12, 1))
                .expect("Chinese search cursor cell")
                .bg,
            CrabCodeTheme::NIGHT.text_primary
        );
    }

    #[test]
    fn empty_state_only_ctrl_c_cancels() {
        let mut picker = SessionPickerComponent::new(Vec::new(), false, None, false);
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Esc, KeyModifiers::NONE))
                .0,
            SessionPickerAction::None
        );
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .0,
            SessionPickerAction::Cancelled
        );
    }

    #[test]
    fn initial_title_query_and_authority_search_corpus_are_preserved() {
        let mut picker = SessionPickerComponent::loading(Some("pr #42"));
        picker.replace_catalog(
            vec![
                SessionPickerEntry {
                    search_text: "First main pr #42 repo".to_string(),
                    ..entry("a", "First")
                },
                entry("b", "Second"),
            ],
            false,
            false,
            Some("main".to_string()),
            false,
            true,
        );
        assert_eq!(picker.state.query(), "pr #42");
        assert_eq!(picker.visible_entries().len(), 1);
        assert_eq!(
            picker.entries[picker.visible_entries()[0].entry_index].id,
            "a"
        );
    }

    #[test]
    fn project_toggle_requests_an_authoritative_catalog_reload() {
        let mut picker = SessionPickerComponent::new(vec![entry("a", "First")], false, None, false);
        let (action, update) = picker.handle_event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(
            action,
            SessionPickerAction::ReloadRequested { all_projects: true }
        );
        assert!(update.stop_batch);
        assert!(picker.show_all_projects());
        assert!(matches!(picker.view, SessionPickerView::Loading));
    }

    #[test]
    fn preview_escape_closes_both_loading_and_complete_views() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "First")], false, None, false);
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Char('v'), KeyModifiers::CONTROL))
                .0,
            SessionPickerAction::PreviewRequested {
                id: "opaque-1".to_string(),
            }
        );
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Esc, KeyModifiers::NONE))
                .0,
            SessionPickerAction::None,
            "loading cancellation is remembered until the authority's pending preview completes"
        );
        assert!(picker.take_cancelled_preview("opaque-1"));
        assert!(!picker.take_cancelled_preview("opaque-1"));

        let _ = picker.handle_event(&key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(picker.complete_preview("opaque-1", Vec::new(), "now".to_string(), 0, None,));
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Esc, KeyModifiers::NONE))
                .0,
            SessionPickerAction::PreviewClosed
        );
    }

    #[test]
    fn rename_escape_is_local_and_never_closes_the_catalog_interaction() {
        let mut picker =
            SessionPickerComponent::new(vec![entry("opaque-1", "First")], false, None, false);
        let _ = picker.handle_event(&key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(
            picker
                .handle_event(&key(KeyCode::Esc, KeyModifiers::NONE))
                .0,
            SessionPickerAction::None
        );
        assert!(matches!(picker.view, SessionPickerView::List));
    }
}
