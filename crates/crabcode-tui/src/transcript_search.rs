//! Full-text search over CrabCode's renderer-owned transcript projection.
//!
//! The lifecycle in this module is ported from the fixed upstream
//! `scrollback/search.rs`, `search/matcher.rs`, and `input/line_editor.rs` at
//! revision `a5727c5960452e7527a154b25cb5bf00cda0545e`. The only product
//! adapter is [`TranscriptSearchDocument`]: CrabCode's existing projection
//! supplies a stable item key and already-renderable text. No backend request,
//! public wire subtype, session method, or persistence surface is introduced.

use std::ops::Range;
use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, Sender, channel},
};
use std::thread::{self, JoinHandle};

use crabcode_ratatui_textarea::{
    EditBuffer, EditCommand, EditOutcome, SingleLineViewport, classify_key_event,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// One renderer-projected transcript item supplied to the fixed search core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptSearchDocument {
    pub(crate) key: String,
    pub(crate) text: String,
}

/// A located query match within one transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptMatch {
    pub(crate) key: String,
    pub(crate) line_in_item: usize,
    pub(crate) byte_range: Range<usize>,
    pub(crate) ordinal_in_item: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    #[cfg(test)]
    Substring,
    Regex,
}

/// Fixed smart-case substring/regex matcher.
#[derive(Debug, Clone)]
struct TextMatcher {
    regex: regex::Regex,
    query: String,
    is_error: bool,
}

impl TextMatcher {
    fn new(query: impl Into<String>, kind: QueryKind) -> Self {
        let query = query.into();
        let smart_ci = !query.chars().any(char::is_uppercase);
        let (regex, is_error) = match kind {
            #[cfg(test)]
            QueryKind::Substring => {
                let escaped = regex::escape(&query);
                let regex = regex::RegexBuilder::new(&escaped)
                    .case_insensitive(smart_ci)
                    .build()
                    .unwrap_or_else(|_| regex::Regex::new("(?:)").expect("static regex"));
                (regex, false)
            }
            QueryKind::Regex => match regex::RegexBuilder::new(&query)
                .case_insensitive(smart_ci)
                .build()
            {
                Ok(regex) => (regex, false),
                Err(_) => (
                    regex::Regex::new(r"\z.").expect("static never-match regex"),
                    true,
                ),
            },
        };
        Self {
            regex,
            query,
            is_error,
        }
    }

    fn query(&self) -> &str {
        &self.query
    }

    fn is_error(&self) -> bool {
        self.is_error
    }

    fn compiled_regex(&self) -> &regex::Regex {
        &self.regex
    }
}

#[derive(Debug)]
struct IndexedDocument {
    key: String,
    text: String,
}

/// Generation-keyed searchable-text cache.
#[derive(Debug, Default)]
struct TranscriptSearchIndex {
    documents: Arc<[IndexedDocument]>,
    built_generation: Option<u64>,
}

impl TranscriptSearchIndex {
    fn sync(
        &mut self,
        content_generation: u64,
        documents: impl FnOnce() -> Vec<TranscriptSearchDocument>,
    ) -> bool {
        if self.built_generation == Some(content_generation) {
            return false;
        }
        self.documents = documents()
            .into_iter()
            .filter(|document| !document.text.is_empty())
            .map(|document| IndexedDocument {
                key: document.key,
                text: document.text,
            })
            .collect::<Vec<_>>()
            .into();
        self.built_generation = Some(content_generation);
        true
    }

    fn documents_arc(&self) -> Arc<[IndexedDocument]> {
        Arc::clone(&self.documents)
    }

    #[cfg(test)]
    fn find(&self, matcher: &TextMatcher) -> Vec<TranscriptMatch> {
        scan_matches(&self.documents, matcher)
    }
}

/// Scan the cached corpus in transcript order.
///
/// This retains the fixed upstream newline walk and zero-width rejection.
fn scan_matches(documents: &[IndexedDocument], matcher: &TextMatcher) -> Vec<TranscriptMatch> {
    if matcher.query().is_empty() {
        return Vec::new();
    }
    let regex = matcher.compiled_regex();
    let mut matches = Vec::new();
    for document in documents {
        let mut line = 0usize;
        let mut counted_to = 0usize;
        let mut ordinal_in_item = 0usize;
        for matched in regex.find_iter(&document.text) {
            if matched.start() == matched.end() {
                continue;
            }
            line += document.text[counted_to..matched.start()]
                .matches('\n')
                .count();
            counted_to = matched.start();
            matches.push(TranscriptMatch {
                key: document.key.clone(),
                line_in_item: line,
                byte_range: matched.range(),
                ordinal_in_item,
            });
            ordinal_in_item = ordinal_in_item.saturating_add(1);
        }
    }
    matches
}

#[derive(Clone, Default, Debug)]
struct SearchSnapshot {
    matches: Arc<[TranscriptMatch]>,
    request_generation: u64,
    query: String,
}

enum SearchMessage {
    Update {
        corpus: Option<Arc<[IndexedDocument]>>,
        query: String,
        request_generation: u64,
    },
    Stop,
}

#[derive(Default)]
struct DrainedUpdate {
    corpus: Option<Arc<[IndexedDocument]>>,
    query: Option<String>,
    request_generation: Option<u64>,
    stop: bool,
}

fn drain_to_latest(first: SearchMessage, receiver: &Receiver<SearchMessage>) -> DrainedUpdate {
    let mut output = DrainedUpdate::default();
    let mut message = first;
    loop {
        match message {
            SearchMessage::Update {
                corpus,
                query,
                request_generation,
            } => {
                if corpus.is_some() {
                    output.corpus = corpus;
                }
                output.query = Some(query);
                output.request_generation = Some(request_generation);
            }
            SearchMessage::Stop => {
                output.stop = true;
                return output;
            }
        }
        match receiver.try_recv() {
            Ok(next) => message = next,
            Err(_) => return output,
        }
    }
}

#[derive(Debug)]
struct SearchDaemon {
    shared: Arc<Mutex<SearchSnapshot>>,
    sender: Sender<SearchMessage>,
    _handle: JoinHandle<()>,
}

impl SearchDaemon {
    fn new() -> Self {
        let shared = Arc::new(Mutex::new(SearchSnapshot::default()));
        let (sender, receiver) = channel::<SearchMessage>();
        let output = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("crabcode-transcript-search".to_string())
            .spawn(move || {
                let mut corpus: Arc<[IndexedDocument]> = Arc::from([]);
                let mut query = String::new();
                while let Ok(message) = receiver.recv() {
                    let update = drain_to_latest(message, &receiver);
                    if update.stop {
                        break;
                    }
                    if let Some(new_corpus) = update.corpus {
                        corpus = new_corpus;
                    }
                    if let Some(new_query) = update.query {
                        query = new_query;
                    }
                    let Some(request_generation) = update.request_generation else {
                        continue;
                    };
                    let matcher = TextMatcher::new(query.as_str(), QueryKind::Regex);
                    let matches: Arc<[TranscriptMatch]> = if query.is_empty() || matcher.is_error()
                    {
                        Arc::from([])
                    } else {
                        scan_matches(&corpus, &matcher).into()
                    };
                    *output.lock().expect("transcript search snapshot mutex") = SearchSnapshot {
                        matches,
                        request_generation,
                        query: query.clone(),
                    };
                }
            })
            .expect("transcript search worker thread");
        Self {
            shared,
            sender,
            _handle: handle,
        }
    }
}

impl Drop for SearchDaemon {
    fn drop(&mut self) {
        let _ = self.sender.send(SearchMessage::Stop);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineEditOutcome {
    Unhandled,
    HandledNoChange,
    CursorChanged,
    TextChanged,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LineEditor {
    buffer: EditBuffer,
}

impl LineEditor {
    fn text(&self) -> &str {
        self.buffer.text()
    }

    fn set_text(&mut self, text: impl Into<String>) {
        self.buffer = EditBuffer::from_text(sanitize_single_line(text));
    }

    fn insert_paste(&mut self, text: &str) -> LineEditOutcome {
        let cleaned = sanitize_single_line(text);
        if cleaned.is_empty() {
            return LineEditOutcome::HandledNoChange;
        }
        Self::from_edit_outcome(self.buffer.insert_str(&cleaned))
    }

    fn handle_key(&mut self, key: &KeyEvent) -> LineEditOutcome {
        if key.kind == KeyEventKind::Release {
            return LineEditOutcome::Unhandled;
        }
        let command = match key {
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::SUPER,
                ..
            } => Some(EditCommand::MoveLogicalLineStart),
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::SUPER,
                ..
            } => Some(EditCommand::MoveLogicalLineEnd),
            _ => classify_key_event(key),
        };
        let Some(command) = command else {
            return LineEditOutcome::Unhandled;
        };
        Self::from_edit_outcome(self.buffer.apply(command))
    }

    fn viewport(&self, width: usize) -> SingleLineViewport {
        self.buffer.single_line_viewport(width)
    }

    fn from_edit_outcome(outcome: EditOutcome) -> LineEditOutcome {
        match outcome {
            EditOutcome::Unchanged => LineEditOutcome::HandledNoChange,
            EditOutcome::CursorOnly => LineEditOutcome::CursorChanged,
            EditOutcome::TextOnly(_) | EditOutcome::TextAndCursor(_) => {
                LineEditOutcome::TextChanged
            }
        }
    }
}

fn sanitize_single_line(text: impl Into<String>) -> String {
    let mut text = text.into();
    text.retain(|character| !matches!(character, '\r' | '\n'));
    text
}

/// Interactive two-phase transcript search.
#[derive(Debug)]
pub(crate) struct TranscriptSearchState {
    editor: LineEditor,
    index: TranscriptSearchIndex,
    matcher: TextMatcher,
    matches: Arc<[TranscriptMatch]>,
    current: Option<usize>,
    composing: bool,
    daemon: SearchDaemon,
    last_seen_generation: u64,
    request_generation: u64,
}

impl TranscriptSearchState {
    pub(crate) fn open() -> Self {
        Self {
            editor: LineEditor::default(),
            index: TranscriptSearchIndex::default(),
            matcher: TextMatcher::new("", QueryKind::Regex),
            matches: Arc::from([]),
            current: None,
            composing: true,
            daemon: SearchDaemon::new(),
            last_seen_generation: 0,
            request_generation: 0,
        }
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        self.editor.set_text(query);
    }

    pub(crate) fn refresh_query(
        &mut self,
        content_generation: u64,
        documents: impl FnOnce() -> Vec<TranscriptSearchDocument>,
    ) {
        let query = self.editor.text().to_owned();
        self.matcher = TextMatcher::new(query.as_str(), QueryKind::Regex);
        if query.is_empty() || self.matcher.is_error() {
            self.matches = Arc::from([]);
            self.current = None;
        }
        let corpus = self
            .index
            .sync(content_generation, documents)
            .then(|| self.index.documents_arc());
        let Some(request_generation) = self.request_generation.checked_add(1) else {
            tracing::debug!(
                "transcript search request generation exhausted; dropping query update"
            );
            return;
        };
        self.request_generation = request_generation;
        if let Err(error) = self.daemon.sender.send(SearchMessage::Update {
            corpus,
            query,
            request_generation,
        }) {
            tracing::debug!(%error, "transcript search daemon unavailable; dropping query update");
        }
    }

    pub(crate) fn apply_query_key(&mut self, key: &KeyEvent) -> LineEditOutcome {
        self.editor.handle_key(key)
    }

    pub(crate) fn apply_query_paste(&mut self, text: &str) -> LineEditOutcome {
        self.editor.insert_paste(text)
    }

    pub(crate) fn poll(&mut self) -> bool {
        let guard = self
            .daemon
            .shared
            .lock()
            .expect("transcript search snapshot mutex");
        if guard.request_generation == self.last_seen_generation {
            return false;
        }
        self.last_seen_generation = guard.request_generation;
        if guard.request_generation != self.request_generation || guard.query != self.query() {
            return false;
        }
        let matches = Arc::clone(&guard.matches);
        drop(guard);
        self.matches = matches;
        self.current = (!self.matches.is_empty()).then_some(0);
        true
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        self.request_generation != self.last_seen_generation
    }

    pub(crate) fn next(&mut self) {
        self.step(1);
    }

    pub(crate) fn previous(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        let len = self.matches.len();
        if len == 0 {
            self.current = None;
            return;
        }
        let from = self.current.unwrap_or(0) as isize;
        self.current = Some((from + delta).rem_euclid(len as isize) as usize);
    }

    pub(crate) fn accept(&mut self) {
        self.composing = false;
    }

    pub(crate) fn current(&self) -> Option<&TranscriptMatch> {
        self.current.and_then(|index| self.matches.get(index))
    }

    pub(crate) fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub(crate) fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub(crate) fn query(&self) -> &str {
        self.editor.text()
    }

    pub(crate) fn query_viewport(&self, width: usize) -> SingleLineViewport {
        self.editor.viewport(width)
    }

    pub(crate) fn highlight_regex(&self) -> Option<regex::Regex> {
        (!self.query().is_empty() && !self.matcher.is_error())
            .then(|| self.matcher.compiled_regex().clone())
    }

    pub(crate) fn has_error(&self) -> bool {
        self.matcher.is_error()
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.composing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents(values: &[(&str, &str)]) -> Vec<TranscriptSearchDocument> {
        values
            .iter()
            .map(|(key, text)| TranscriptSearchDocument {
                key: (*key).to_string(),
                text: (*text).to_string(),
            })
            .collect()
    }

    fn update_and_wait(
        search: &mut TranscriptSearchState,
        generation: u64,
        query: &str,
        values: &[(&str, &str)],
    ) {
        search.set_query(query);
        search.refresh_query(generation, || documents(values));
        for _ in 0..1_000 {
            if search.poll() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("transcript search daemon did not publish {query:?}");
    }

    #[test]
    fn index_keeps_transcript_order_line_and_match_ordinal() {
        let mut index = TranscriptSearchIndex::default();
        assert!(index.sync(1, || {
            documents(&[("first", "foo\nbar foo"), ("second", "foo")])
        }));
        let matcher = TextMatcher::new("foo", QueryKind::Substring);
        let matches = index.find(&matcher);

        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].key, "first");
        assert_eq!(matches[0].line_in_item, 0);
        assert_eq!(matches[0].ordinal_in_item, 0);
        assert_eq!(matches[1].line_in_item, 1);
        assert_eq!(matches[1].ordinal_in_item, 1);
        assert_eq!(matches[2].key, "second");
        assert_eq!(matches[2].ordinal_in_item, 0);
    }

    #[test]
    fn invalid_and_zero_width_regexes_never_create_matches() {
        let mut index = TranscriptSearchIndex::default();
        assert!(index.sync(1, || documents(&[("item", "abc")])));

        let invalid = TextMatcher::new("[invalid", QueryKind::Regex);
        assert!(invalid.is_error());
        assert!(index.find(&invalid).is_empty());
        assert!(
            index
                .find(&TextMatcher::new("x*", QueryKind::Regex))
                .is_empty()
        );
    }

    #[test]
    fn generation_cache_rebuilds_only_for_content_changes() {
        let mut index = TranscriptSearchIndex::default();
        assert!(index.sync(1, || documents(&[("item", "one")])));
        assert!(!index.sync(1, || panic!("unchanged generation rebuilt corpus")));
        assert!(index.sync(2, || documents(&[("item", "two")])));
        assert_eq!(
            index
                .find(&TextMatcher::new("two", QueryKind::Substring))
                .len(),
            1
        );
    }

    #[test]
    fn daemon_results_are_two_phase_and_navigation_wraps() {
        let mut search = TranscriptSearchState::open();
        assert!(search.is_composing());
        update_and_wait(
            &mut search,
            1,
            "foo",
            &[("first", "foo"), ("second", "foo")],
        );
        assert_eq!(search.current_index(), Some(0));
        assert_eq!(search.match_count(), 2);
        search.accept();
        search.next();
        assert_eq!(search.current_index(), Some(1));
        search.next();
        assert_eq!(search.current_index(), Some(0));
        search.previous();
        assert_eq!(search.current_index(), Some(1));
    }

    #[test]
    fn query_editor_retains_fixed_single_line_key_and_paste_rules() {
        let mut search = TranscriptSearchState::open();
        assert_eq!(
            search.apply_query_paste("one\r\ntwo"),
            LineEditOutcome::TextChanged
        );
        assert_eq!(search.query(), "onetwo");
        assert_eq!(
            search.apply_query_key(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            LineEditOutcome::CursorChanged
        );
        assert_eq!(
            search.apply_query_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            LineEditOutcome::TextChanged
        );
        assert_eq!(search.query(), "xonetwo");
    }

    #[test]
    fn stale_daemon_snapshot_cannot_replace_newer_visible_query() {
        let mut search = TranscriptSearchState::open();
        search.set_query("old");
        search.refresh_query(1, || documents(&[("item", "old new")]));
        search.set_query("new");
        search.refresh_query(1, || panic!("same corpus generation must be reused"));

        for _ in 0..1_000 {
            if search.poll() && search.current().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(search.query(), "new");
        assert_eq!(
            search.current().map(|matched| matched.byte_range.clone()),
            Some(4..7)
        );
    }
}
