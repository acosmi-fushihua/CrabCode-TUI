//! Renderer-private workspace search surfaces.
//!
//! The product adapters here reuse the existing `acosmi-index` file index and
//! content search implementation. They own only modal query/selection,
//! generation fencing, bounded results and previews; no search state or
//! request is added to the direct backend protocol.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use acosmi_index::{ContentSearchOptions, ContentSearcher, FileIndex};
use crossterm::event::Event;

use crate::picker_surface::{
    PickerConfig, PickerOutcome, PickerState, PickerStateProductExt, handle_picker_input,
    picker_config_default,
};

const QUICK_OPEN_RESULT_LIMIT: usize = 15;
const GLOBAL_SEARCH_MAX_MATCHES_PER_FILE: usize = 10;
const GLOBAL_SEARCH_MAX_TOTAL_MATCHES: usize = 500;
const GLOBAL_SEARCH_SOURCE_RESULT_LIMIT: usize =
    GLOBAL_SEARCH_MAX_MATCHES_PER_FILE * GLOBAL_SEARCH_MAX_TOTAL_MATCHES;
const GLOBAL_SEARCH_DEBOUNCE: Duration = Duration::from_millis(100);
const QUICK_OPEN_PREVIEW_LINES: usize = 20;
const GLOBAL_SEARCH_PREVIEW_CONTEXT_LINES: usize = 4;
const PREVIEW_LINE_BYTE_LIMIT: usize = 64 * 1024;

pub(crate) const QUICK_OPEN_VISIBLE_RESULTS: usize = 8;
pub(crate) const GLOBAL_SEARCH_VISIBLE_RESULTS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceSearchKind {
    QuickOpen,
    GlobalSearch,
}

impl WorkspaceSearchKind {
    pub(crate) const fn visible_results(self) -> usize {
        match self {
            Self::QuickOpen => QUICK_OPEN_VISIBLE_RESULTS,
            Self::GlobalSearch => GLOBAL_SEARCH_VISIBLE_RESULTS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSearchEntry {
    pub(crate) path: String,
    pub(crate) line: Option<usize>,
    pub(crate) text: String,
}

impl WorkspaceSearchEntry {
    fn preview_key(&self) -> WorkspacePreviewKey {
        WorkspacePreviewKey {
            path: self.path.clone(),
            line: self.line,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePreviewKey {
    path: String,
    line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspacePreview {
    key: WorkspacePreviewKey,
    pub(crate) content: String,
}

struct IndexBuildJob {
    receiver: Receiver<Result<Arc<FileIndex>, String>>,
}

struct SearchJob {
    generation: u64,
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Result<SearchResultBatch, String>>,
}

struct PreviewJob {
    generation: u64,
    cancel: Arc<AtomicBool>,
    receiver: Receiver<WorkspacePreview>,
}

struct SearchResultBatch {
    entries: Vec<WorkspaceSearchEntry>,
    truncated: bool,
}

/// Shared modal state for the fixed QuickOpen and GlobalSearch product
/// surfaces. The common `PickerState` remains the sole owner of query,
/// cursor, viewport and mouse hit geometry.
pub(crate) struct WorkspaceSearchState {
    kind: WorkspaceSearchKind,
    workspace: PathBuf,
    lifecycle: PickerState,
    entries: Vec<WorkspaceSearchEntry>,
    truncated: bool,
    searching: bool,
    error: Option<String>,
    generation: u64,
    index: Option<Arc<FileIndex>>,
    index_job: Option<IndexBuildJob>,
    search_due: Option<Instant>,
    search_job: Option<SearchJob>,
    preview_generation: u64,
    preview: Option<WorkspacePreview>,
    preview_job: Option<PreviewJob>,
}

impl WorkspaceSearchState {
    pub(crate) fn new(kind: WorkspaceSearchKind, workspace: PathBuf) -> Self {
        let mut state = Self {
            kind,
            workspace,
            lifecycle: PickerState::input_active(),
            entries: Vec::new(),
            truncated: false,
            searching: false,
            error: None,
            generation: 0,
            index: None,
            index_job: None,
            search_due: None,
            search_job: None,
            preview_generation: 0,
            preview: None,
            preview_job: None,
        };
        state.start_index_build();
        state
    }

    pub(crate) const fn kind(&self) -> WorkspaceSearchKind {
        self.kind
    }

    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub(crate) fn lifecycle(&self) -> &PickerState {
        &self.lifecycle
    }

    pub(crate) fn lifecycle_mut(&mut self) -> &mut PickerState {
        &mut self.lifecycle
    }

    pub(crate) fn query(&self) -> &str {
        self.lifecycle.query()
    }

    pub(crate) fn entries(&self) -> &[WorkspaceSearchEntry] {
        &self.entries
    }

    pub(crate) fn selected_entry(&self) -> Option<&WorkspaceSearchEntry> {
        self.lifecycle
            .selected_for(self.entries.len())
            .and_then(|index| self.entries.get(index))
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.lifecycle.selected_for(self.entries.len())
    }

    pub(crate) fn preview(&self) -> Option<&WorkspacePreview> {
        let selected_key = self.selected_entry()?.preview_key();
        self.preview
            .as_ref()
            .filter(|preview| preview.key == selected_key)
    }

    pub(crate) const fn searching(&self) -> bool {
        self.searching
    }

    pub(crate) const fn truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        self.index_job.is_some()
            || self.search_due.is_some()
            || self.search_job.is_some()
            || self.preview_job.is_some()
    }

    pub(crate) fn handle_event(&mut self, event: &Event, now: Instant) -> PickerOutcome {
        let selected_before = self.selected_index();
        let outcome = handle_picker_input(
            event,
            &mut self.lifecycle,
            self.entries.len(),
            &PickerConfig {
                esc_clears_query: false,
                ..picker_config_default()
            },
        );
        if matches!(outcome, PickerOutcome::QueryChanged) {
            self.query_changed(now);
        } else if matches!(outcome, PickerOutcome::Changed | PickerOutcome::Unchanged)
            && selected_before != self.selected_index()
        {
            self.start_preview();
        }
        outcome
    }

    pub(crate) fn focus_previous(&mut self) {
        if self.lifecycle.move_previous(self.entries.len(), &[], false) {
            self.start_preview();
        }
    }

    pub(crate) fn focus_next(&mut self) {
        if self.lifecycle.move_next(self.entries.len(), &[], false) {
            self.start_preview();
        }
    }

    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if let Some(job) = self.index_job.take() {
            match job.receiver.try_recv() {
                Ok(Ok(index)) => {
                    self.index = Some(index);
                    self.error = None;
                    changed = true;
                    if !self.query().trim().is_empty() {
                        match self.kind {
                            WorkspaceSearchKind::QuickOpen => self.start_search(),
                            WorkspaceSearchKind::GlobalSearch => {
                                if self.search_due.is_none() {
                                    self.search_due = Some(now);
                                }
                            }
                        }
                    }
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.searching = false;
                    self.search_due = None;
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.index_job = Some(job),
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("workspace file index worker disconnected".to_string());
                    self.searching = false;
                    self.search_due = None;
                    changed = true;
                }
            }
        }

        if self.search_due.is_some_and(|deadline| deadline <= now) && self.index.is_some() {
            self.search_due = None;
            self.start_search();
            changed = true;
        }

        if let Some(job) = self.search_job.take() {
            match job.receiver.try_recv() {
                Ok(Ok(batch)) => {
                    if job.generation == self.generation {
                        self.apply_search_results(batch);
                    }
                    changed = true;
                }
                Ok(Err(error)) => {
                    if job.generation == self.generation {
                        self.error = Some(error);
                        self.searching = false;
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.search_job = Some(job),
                Err(TryRecvError::Disconnected) => {
                    if job.generation == self.generation {
                        self.error = Some("workspace search worker disconnected".to_string());
                        self.searching = false;
                    }
                    changed = true;
                }
            }
        }

        if let Some(job) = self.preview_job.take() {
            match job.receiver.try_recv() {
                Ok(preview) if job.generation == self.preview_generation => {
                    self.preview = Some(preview);
                    changed = true;
                }
                Ok(_) => changed = true,
                Err(TryRecvError::Empty) => self.preview_job = Some(job),
                Err(TryRecvError::Disconnected) => {
                    if job.generation == self.preview_generation {
                        self.preview = self.selected_entry().map(|entry| WorkspacePreview {
                            key: entry.preview_key(),
                            content: "(preview unavailable)".to_string(),
                        });
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    fn start_index_build(&mut self) {
        let workspace = self.workspace.clone();
        let (sender, receiver) = sync_channel(1);
        match std::thread::Builder::new()
            .name("crabcode-tui-file-index".to_string())
            .spawn(move || {
                let mut index = FileIndex::new(&workspace);
                let result = index
                    .build()
                    .map(|_| Arc::new(index))
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            }) {
            Ok(_) => self.index_job = Some(IndexBuildJob { receiver }),
            Err(error) => {
                self.error = Some(format!("failed to start workspace file index: {error}"));
            }
        }
    }

    fn query_changed(&mut self, now: Instant) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel_search();
        self.error = None;
        self.truncated = false;
        self.preview = None;
        self.cancel_preview();
        let query = self.query().trim().to_string();
        if query.is_empty() {
            self.entries.clear();
            self.searching = false;
            self.search_due = None;
            return;
        }
        self.searching = true;
        match self.kind {
            WorkspaceSearchKind::QuickOpen => {
                self.search_due = None;
                if self.index.is_some() {
                    self.start_search();
                }
            }
            WorkspaceSearchKind::GlobalSearch => {
                let query_lower = query.to_lowercase();
                self.entries
                    .retain(|entry| entry.text.to_lowercase().contains(&query_lower));
                self.lifecycle
                    .set_selected(self.entries.len().saturating_sub(1), self.entries.len());
                self.search_due = Some(now + GLOBAL_SEARCH_DEBOUNCE);
            }
        }
    }

    fn start_search(&mut self) {
        let Some(index) = self.index.as_ref().map(Arc::clone) else {
            return;
        };
        let query = self.query().trim().to_string();
        if query.is_empty() {
            return;
        }
        self.cancel_search();
        let kind = self.kind;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = sync_channel(1);
        match std::thread::Builder::new()
            .name(match kind {
                WorkspaceSearchKind::QuickOpen => "crabcode-tui-quick-open".to_string(),
                WorkspaceSearchKind::GlobalSearch => "crabcode-tui-global-search".to_string(),
            })
            .spawn(move || {
                let result = match kind {
                    WorkspaceSearchKind::QuickOpen => {
                        Ok(search_quick_open(&index, &query, &worker_cancel))
                    }
                    WorkspaceSearchKind::GlobalSearch => {
                        search_global(&index, &query, &worker_cancel)
                    }
                };
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.searching = true;
                self.search_job = Some(SearchJob {
                    generation,
                    cancel,
                    receiver,
                });
            }
            Err(error) => {
                self.searching = false;
                self.error = Some(format!("failed to start workspace search: {error}"));
            }
        }
    }

    fn cancel_search(&mut self) {
        if let Some(job) = self.search_job.take() {
            job.cancel.store(true, Ordering::Release);
        }
    }

    fn apply_search_results(&mut self, mut batch: SearchResultBatch) {
        // The fixed picker uses direction="up": the best result is adjacent
        // to the input at the bottom and remaining results extend upward.
        batch.entries.reverse();
        self.entries = batch.entries;
        self.truncated = batch.truncated;
        self.searching = false;
        self.error = None;
        self.lifecycle
            .set_selected(self.entries.len().saturating_sub(1), self.entries.len());
        self.start_preview();
    }

    fn start_preview(&mut self) {
        self.cancel_preview();
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let Some(entry) = self.selected_entry().cloned() else {
            self.preview = None;
            return;
        };
        if self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.key == entry.preview_key())
        {
            return;
        }
        self.preview = None;
        let workspace = self.workspace.clone();
        let key = entry.preview_key();
        let worker_key = key.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let generation = self.preview_generation;
        let kind = self.kind;
        let (sender, receiver) = sync_channel(1);
        match std::thread::Builder::new()
            .name("crabcode-tui-file-preview".to_string())
            .spawn(move || {
                let content = read_workspace_preview(&workspace, &entry, kind, &worker_cancel)
                    .unwrap_or_else(|_| "(preview unavailable)".to_string());
                let _ = sender.send(WorkspacePreview {
                    key: worker_key,
                    content,
                });
            }) {
            Ok(_) => {
                self.preview_job = Some(PreviewJob {
                    generation,
                    cancel,
                    receiver,
                });
            }
            Err(_) => {
                self.preview = Some(WorkspacePreview {
                    key,
                    content: "(preview unavailable)".to_string(),
                });
            }
        }
    }

    fn cancel_preview(&mut self) {
        if let Some(job) = self.preview_job.take() {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

impl Drop for WorkspaceSearchState {
    fn drop(&mut self) {
        self.cancel_search();
        self.cancel_preview();
    }
}

fn search_quick_open(index: &FileIndex, query: &str, cancel: &AtomicBool) -> SearchResultBatch {
    let entries = if cancel.load(Ordering::Acquire) {
        Vec::new()
    } else {
        index
            .search(query, QUICK_OPEN_RESULT_LIMIT)
            .into_iter()
            .take_while(|_| !cancel.load(Ordering::Acquire))
            .map(|result| WorkspaceSearchEntry {
                path: normalize_relative_path(&result.path),
                line: None,
                text: String::new(),
            })
            .collect()
    };
    SearchResultBatch {
        entries,
        truncated: false,
    }
}

fn search_global(
    index: &FileIndex,
    query: &str,
    cancel: &AtomicBool,
) -> Result<SearchResultBatch, String> {
    if cancel.load(Ordering::Acquire) {
        return Ok(SearchResultBatch {
            entries: Vec::new(),
            truncated: false,
        });
    }
    let options = ContentSearchOptions {
        pattern: query.to_string(),
        case_sensitive: false,
        literal: true,
        max_results: GLOBAL_SEARCH_SOURCE_RESULT_LIMIT,
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        include_hidden: true,
    };
    let source =
        ContentSearcher::search(index.root(), &options).map_err(|error| error.to_string())?;
    let mut per_file = HashMap::<String, usize>::new();
    let mut entries = Vec::new();
    let mut truncated = false;
    for matched in source {
        if cancel.load(Ordering::Acquire) {
            return Ok(SearchResultBatch {
                entries: Vec::new(),
                truncated: false,
            });
        }
        let relative = matched
            .path
            .strip_prefix(index.root())
            .unwrap_or(&matched.path)
            .to_string_lossy();
        let path = normalize_relative_path(&relative);
        let count = per_file.entry(path.clone()).or_default();
        if *count >= GLOBAL_SEARCH_MAX_MATCHES_PER_FILE {
            continue;
        }
        *count += 1;
        entries.push(WorkspaceSearchEntry {
            path,
            line: Some(matched.line_number),
            text: matched.line_content,
        });
        if entries.len() == GLOBAL_SEARCH_MAX_TOTAL_MATCHES {
            truncated = true;
            break;
        }
    }
    Ok(SearchResultBatch { entries, truncated })
}

fn read_workspace_preview(
    workspace: &Path,
    entry: &WorkspaceSearchEntry,
    kind: WorkspaceSearchKind,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let path = canonical_workspace_file(workspace, &entry.path)?;
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let (start, count) = match (kind, entry.line) {
        (WorkspaceSearchKind::GlobalSearch, Some(line)) => (
            line.saturating_sub(GLOBAL_SEARCH_PREVIEW_CONTEXT_LINES + 1),
            GLOBAL_SEARCH_PREVIEW_CONTEXT_LINES * 2 + 1,
        ),
        _ => (0, QUICK_OPEN_PREVIEW_LINES),
    };
    let mut content = Vec::new();
    let mut line = Vec::new();
    let mut line_index = 0;
    while content.len() < count {
        if cancel.load(Ordering::Acquire) {
            return Err("preview cancelled".to_string());
        }
        line.clear();
        let bytes = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            break;
        }
        if line_index >= start {
            if line.contains(&0) {
                return Err("binary preview unavailable".to_string());
            }
            if line.len() > PREVIEW_LINE_BYTE_LIMIT {
                line.truncate(PREVIEW_LINE_BYTE_LIMIT);
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                let _ = line.pop();
            }
            let mut text = String::from_utf8_lossy(&line).into_owned();
            if line_index == 0 {
                text = text.trim_start_matches('\u{feff}').to_string();
            }
            content.push(text);
        }
        line_index += 1;
    }
    Ok(content.join("\n"))
}

pub(crate) fn canonical_workspace_file(
    workspace: &Path,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let canonical_workspace =
        std::fs::canonicalize(workspace).map_err(|error| error.to_string())?;
    let candidate = canonical_workspace.join(relative_path);
    let canonical_candidate =
        std::fs::canonicalize(&candidate).map_err(|error| error.to_string())?;
    if !canonical_candidate.starts_with(&canonical_workspace) {
        return Err(format!(
            "workspace search result `{relative_path}` resolves outside {}",
            canonical_workspace.display()
        ));
    }
    if !canonical_candidate.is_file() {
        return Err(format!(
            "workspace search result `{relative_path}` is not a file"
        ));
    }
    Ok(canonical_candidate)
}

fn normalize_relative_path(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn poll_until_settled(state: &mut WorkspaceSearchState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.has_pending_work() && Instant::now() < deadline {
            let _ = state.poll(Instant::now() + GLOBAL_SEARCH_DEBOUNCE);
            std::thread::yield_now();
        }
        assert!(!state.has_pending_work(), "workspace search did not settle");
    }

    #[test]
    fn quick_open_reuses_existing_file_index_and_keeps_best_result_by_the_input() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join("src")).expect("mkdir");
        fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        fs::write(workspace.path().join("src/mapping.rs"), "// map\n").expect("write");
        let mut state =
            WorkspaceSearchState::new(WorkspaceSearchKind::QuickOpen, workspace.path().into());
        state.lifecycle_mut().set_query("main");
        state.query_changed(Instant::now());
        poll_until_settled(&mut state);

        assert_eq!(
            state.selected_entry().map(|entry| entry.path.as_str()),
            Some("src/main.rs")
        );
        assert!(state.preview().is_some());
    }

    #[test]
    fn global_search_is_literal_case_insensitive_and_caps_each_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join("src")).expect("mkdir");
        let repeated = (1..=14)
            .map(|line| format!("Needle {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(workspace.path().join("src/many.txt"), repeated).expect("write");
        fs::write(
            workspace.path().join("src/literal.txt"),
            "prefix NEEDLE suffix\n",
        )
        .expect("write");
        let mut state =
            WorkspaceSearchState::new(WorkspaceSearchKind::GlobalSearch, workspace.path().into());
        state.lifecycle_mut().set_query("needle");
        state.query_changed(Instant::now());
        poll_until_settled(&mut state);

        let many = state
            .entries()
            .iter()
            .filter(|entry| entry.path == "src/many.txt")
            .count();
        assert_eq!(many, GLOBAL_SEARCH_MAX_MATCHES_PER_FILE);
        assert!(
            state
                .entries()
                .iter()
                .any(|entry| { entry.path == "src/literal.txt" && entry.line == Some(1) })
        );
    }

    #[test]
    fn canonical_result_guard_rejects_symlink_escape() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape"))
                .expect("symlink");
            assert!(
                canonical_workspace_file(workspace.path(), "escape")
                    .expect_err("escape must fail")
                    .contains("outside")
            );
        }
    }
}
