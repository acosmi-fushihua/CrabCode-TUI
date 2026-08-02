//! Renderer-owned CrabCode fixed-keybinding parsing and resolution.
//!
//! Production deliberately installs fixed renderer defaults only. User
//! customization remains fail-closed until an existing backend authority
//! supplies an exact renderer fact; this module reads no config, environment
//! variable, or user file and adds no protocol.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
const KEYBINDINGS_FILE: &str = "keybindings.json";
const CHORD_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CrabcodeKeybindingContext {
    Global,
    Chat,
    Autocomplete,
    Confirmation,
    Help,
    Transcript,
    HistorySearch,
    Task,
    ThemePicker,
    Settings,
    Tabs,
    Attachments,
    Footer,
    MessageSelector,
    DiffDialog,
    ModelPicker,
    Select,
    Plugin,
    Scroll,
    MessageActions,
    GoalConsole,
}

impl CrabcodeKeybindingContext {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Chat => "Chat",
            Self::Autocomplete => "Autocomplete",
            Self::Confirmation => "Confirmation",
            Self::Help => "Help",
            Self::Transcript => "Transcript",
            Self::HistorySearch => "HistorySearch",
            Self::Task => "Task",
            Self::ThemePicker => "ThemePicker",
            Self::Settings => "Settings",
            Self::Tabs => "Tabs",
            Self::Attachments => "Attachments",
            Self::Footer => "Footer",
            Self::MessageSelector => "MessageSelector",
            Self::DiffDialog => "DiffDialog",
            Self::ModelPicker => "ModelPicker",
            Self::Select => "Select",
            Self::Plugin => "Plugin",
            Self::Scroll => "Scroll",
            Self::MessageActions => "MessageActions",
            Self::GoalConsole => "GoalConsole",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CrabcodeKeybindingRegistration<A> {
    pub(crate) action: A,
    pub(crate) action_name: &'static str,
    pub(crate) context: CrabcodeKeybindingContext,
    pub(crate) default_chords: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrabcodeKeybindingResolution<A> {
    Match(A),
    /// A fixed historical `command:<name>` binding won resolution.
    ///
    /// Command bindings are dynamic user configuration, so they cannot be
    /// represented by the static renderer action registry. The TUI routes the
    /// returned name through its existing slash-command submission boundary.
    Command(String),
    /// A binding won resolution, but this renderer has no handler for it.
    ///
    /// This is distinct from `None`: it must shadow an earlier default while
    /// still allowing ordinary terminal input to continue.
    Unmapped,
    ChordStarted,
    ChordCancelled,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParsedKeystroke {
    key: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    super_key: bool,
}

type Chord = Vec<ParsedKeystroke>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedAction {
    Name(String),
    #[cfg(test)]
    Unsupported,
}

impl ParsedAction {
    fn as_name(&self) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name.as_str()),
            #[cfg(test)]
            Self::Unsupported => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ParsedBinding {
    context: String,
    chord: Chord,
    action: ParsedAction,
}

#[derive(Debug, Clone)]
struct PendingChord {
    chord: Chord,
    started_at: Instant,
}

pub(crate) struct CrabcodeKeybindingEngine<A> {
    registrations: Vec<CrabcodeKeybindingRegistration<A>>,
    bindings: Arc<RwLock<Vec<ParsedBinding>>>,
    pending: Option<PendingChord>,
}

impl<A: fmt::Debug> fmt::Debug for CrabcodeKeybindingEngine<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CrabcodeKeybindingEngine")
            .field("registrations", &self.registrations)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl<A> CrabcodeKeybindingEngine<A>
where
    A: Copy + Eq,
{
    pub(crate) fn for_renderer(registrations: Vec<CrabcodeKeybindingRegistration<A>>) -> Self {
        Self {
            bindings: Arc::new(RwLock::new(parse_default_bindings(&registrations))),
            registrations,
            pending: None,
        }
    }

    #[cfg(test)]
    fn with_user_file_fixture(
        registrations: Vec<CrabcodeKeybindingRegistration<A>>,
        path: std::path::PathBuf,
    ) -> Self {
        let defaults = parse_default_bindings(&registrations);
        Self {
            bindings: Arc::new(RwLock::new(load_fixture_bindings(&path, &defaults))),
            registrations,
            pending: None,
        }
    }

    pub(crate) fn resolve(
        &mut self,
        event: &KeyEvent,
        active_contexts: &[CrabcodeKeybindingContext],
    ) -> CrabcodeKeybindingResolution<A> {
        self.resolve_at(event, active_contexts, Instant::now())
    }

    fn resolve_at(
        &mut self,
        event: &KeyEvent,
        active_contexts: &[CrabcodeKeybindingContext],
        now: Instant,
    ) -> CrabcodeKeybindingResolution<A> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now.duration_since(pending.started_at) >= CHORD_TIMEOUT)
        {
            self.pending = None;
        }

        if event.code == KeyCode::Esc && self.pending.is_some() {
            self.pending = None;
            return CrabcodeKeybindingResolution::ChordCancelled;
        }

        let Some(current) = keystroke_from_event(event) else {
            return if self.pending.take().is_some() {
                CrabcodeKeybindingResolution::ChordCancelled
            } else {
                CrabcodeKeybindingResolution::None
            };
        };
        let was_pending = self.pending.is_some();
        let mut test_chord = self
            .pending
            .as_ref()
            .map_or_else(Vec::new, |pending| pending.chord.clone());
        test_chord.push(current);

        let bindings = self
            .bindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context_is_active = |context: &str| {
            active_contexts
                .iter()
                .any(|active| active.as_str() == context)
        };

        // The fixed implementation groups longer candidates by their parsed
        // chord string and lets the later binding for that exact chord win.
        let mut longer_winners: HashMap<Chord, ParsedAction> = HashMap::new();
        for binding in bindings.iter() {
            if context_is_active(&binding.context)
                && binding.chord.len() > test_chord.len()
                && chord_prefix_matches(&test_chord, &binding.chord)
            {
                longer_winners.insert(binding.chord.clone(), binding.action.clone());
            }
        }
        if !longer_winners.is_empty() {
            drop(bindings);
            self.pending = Some(PendingChord {
                chord: test_chord,
                started_at: now,
            });
            return CrabcodeKeybindingResolution::ChordStarted;
        }

        let exact = bindings
            .iter()
            .rfind(|binding| {
                context_is_active(&binding.context)
                    && chord_exactly_matches(&test_chord, &binding.chord)
            })
            .cloned();
        drop(bindings);
        self.pending = None;

        let Some(exact) = exact else {
            return if was_pending {
                CrabcodeKeybindingResolution::ChordCancelled
            } else {
                CrabcodeKeybindingResolution::None
            };
        };
        let Some(action_name) = exact.action.as_name() else {
            return CrabcodeKeybindingResolution::Unmapped;
        };
        if let Some(command_name) = action_name.strip_prefix("command:") {
            return CrabcodeKeybindingResolution::Command(command_name.to_string());
        }
        self.registrations
            .iter()
            .find(|registration| {
                registration.action_name == action_name
                    && active_contexts.contains(&registration.context)
            })
            .map_or(CrabcodeKeybindingResolution::Unmapped, |registration| {
                CrabcodeKeybindingResolution::Match(registration.action)
            })
    }

    pub(crate) fn resolve_single(
        &self,
        event: &KeyEvent,
        active_contexts: &[CrabcodeKeybindingContext],
    ) -> CrabcodeKeybindingResolution<A> {
        let Some(keystroke) = keystroke_from_event(event) else {
            return CrabcodeKeybindingResolution::None;
        };
        let bindings = self
            .bindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let exact = bindings.iter().rfind(|binding| {
            binding.chord.len() == 1
                && active_contexts
                    .iter()
                    .any(|context| context.as_str() == binding.context)
                && keystrokes_equal(&keystroke, &binding.chord[0])
        });
        let Some(exact) = exact else {
            return CrabcodeKeybindingResolution::None;
        };
        let Some(action_name) = exact.action.as_name() else {
            return CrabcodeKeybindingResolution::Unmapped;
        };
        if let Some(command_name) = action_name.strip_prefix("command:") {
            return CrabcodeKeybindingResolution::Command(command_name.to_string());
        }
        self.registrations
            .iter()
            .find(|registration| {
                registration.action_name == action_name
                    && active_contexts.contains(&registration.context)
            })
            .map_or(CrabcodeKeybindingResolution::Unmapped, |registration| {
                CrabcodeKeybindingResolution::Match(registration.action)
            })
    }

    #[cfg(test)]
    pub(crate) fn has_pending_chord(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    pub(crate) fn expire_pending_chord(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.started_at = Instant::now() - CHORD_TIMEOUT;
        }
    }
}

fn parse_default_bindings<A>(
    registrations: &[CrabcodeKeybindingRegistration<A>],
) -> Vec<ParsedBinding> {
    registrations
        .iter()
        .flat_map(|registration| {
            registration
                .default_chords
                .iter()
                .map(|chord| ParsedBinding {
                    context: registration.context.as_str().to_string(),
                    chord: parse_chord(chord),
                    action: ParsedAction::Name(registration.action_name.to_string()),
                })
        })
        .collect()
}

fn parse_keystroke(input: &str) -> ParsedKeystroke {
    let mut keystroke = ParsedKeystroke {
        key: String::new(),
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        super_key: false,
    };
    for part in input.split('+') {
        let lower = part.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => keystroke.ctrl = true,
            "alt" | "opt" | "option" => keystroke.alt = true,
            "shift" => keystroke.shift = true,
            "meta" => keystroke.meta = true,
            "cmd" | "command" | "super" | "win" => keystroke.super_key = true,
            "esc" => keystroke.key = "escape".to_string(),
            "return" => keystroke.key = "enter".to_string(),
            "space" => keystroke.key = " ".to_string(),
            "↑" => keystroke.key = "up".to_string(),
            "↓" => keystroke.key = "down".to_string(),
            "←" => keystroke.key = "left".to_string(),
            "→" => keystroke.key = "right".to_string(),
            _ => keystroke.key = lower,
        }
    }
    keystroke
}

fn parse_chord(input: &str) -> Chord {
    if input == " " {
        return vec![parse_keystroke("space")];
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return vec![parse_keystroke("")];
    }
    trimmed.split_whitespace().map(parse_keystroke).collect()
}

fn keystroke_from_event(event: &KeyEvent) -> Option<ParsedKeystroke> {
    if event.kind == KeyEventKind::Release || event.modifiers.contains(KeyModifiers::HYPER) {
        return None;
    }
    let (key, implied_shift) = match event.code {
        KeyCode::Esc => ("escape".to_string(), false),
        KeyCode::Enter => ("enter".to_string(), false),
        KeyCode::Tab => ("tab".to_string(), false),
        KeyCode::BackTab => ("tab".to_string(), true),
        KeyCode::Backspace => ("backspace".to_string(), false),
        KeyCode::Delete => ("delete".to_string(), false),
        KeyCode::Up => ("up".to_string(), false),
        KeyCode::Down => ("down".to_string(), false),
        KeyCode::Left => ("left".to_string(), false),
        KeyCode::Right => ("right".to_string(), false),
        KeyCode::PageUp => ("pageup".to_string(), false),
        KeyCode::PageDown => ("pagedown".to_string(), false),
        KeyCode::Home => ("home".to_string(), false),
        KeyCode::End => ("end".to_string(), false),
        KeyCode::Char(character) => (character.to_lowercase().collect(), false),
        _ => return None,
    };
    Some(ParsedKeystroke {
        key,
        ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::META),
        shift: implied_shift || event.modifiers.contains(KeyModifiers::SHIFT),
        meta: false,
        super_key: event.modifiers.contains(KeyModifiers::SUPER),
    })
}

fn keystrokes_equal(left: &ParsedKeystroke, right: &ParsedKeystroke) -> bool {
    left.key == right.key
        && left.ctrl == right.ctrl
        && left.shift == right.shift
        && (left.alt || left.meta) == (right.alt || right.meta)
        && left.super_key == right.super_key
}

fn chord_prefix_matches(prefix: &[ParsedKeystroke], chord: &[ParsedKeystroke]) -> bool {
    prefix.len() < chord.len()
        && prefix
            .iter()
            .zip(chord)
            .all(|(left, right)| keystrokes_equal(left, right))
}

fn chord_exactly_matches(left: &[ParsedKeystroke], right: &[ParsedKeystroke]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| keystrokes_equal(left, right))
}

#[cfg(test)]
fn load_fixture_bindings(path: &std::path::Path, defaults: &[ParsedBinding]) -> Vec<ParsedBinding> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return defaults.to_vec();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&content) else {
        return defaults.to_vec();
    };
    let Some(blocks) = parsed
        .as_object()
        .and_then(|object| object.get("bindings"))
        .and_then(Value::as_array)
    else {
        return defaults.to_vec();
    };
    if !blocks.iter().all(valid_binding_block) {
        return defaults.to_vec();
    }

    let mut merged = defaults.to_vec();
    for block in blocks {
        let object = block
            .as_object()
            .expect("binding block validation established an object");
        let context = object["context"]
            .as_str()
            .expect("binding block validation established a context string");
        match &object["bindings"] {
            Value::Object(bindings) => {
                for (chord, action) in bindings {
                    push_user_binding(&mut merged, context, chord, action);
                }
            }
            Value::Array(bindings) => {
                for (index, action) in bindings.iter().enumerate() {
                    push_user_binding(&mut merged, context, &index.to_string(), action);
                }
            }
            _ => unreachable!("binding block validation established an object-like value"),
        }
    }
    merged
}

#[cfg(test)]
fn valid_binding_block(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("context").is_some_and(Value::is_string)
        && object
            .get("bindings")
            .is_some_and(|bindings| bindings.is_object() || bindings.is_array())
}

#[cfg(test)]
fn push_user_binding(
    destination: &mut Vec<ParsedBinding>,
    context: &str,
    chord: &str,
    action: &Value,
) {
    // This preserves the fixed executable behavior: null entries are skipped
    // by the parser and therefore do not unbind an earlier default.
    if action.is_null() {
        return;
    }
    destination.push(ParsedBinding {
        context: context.to_string(),
        chord: parse_chord(chord),
        action: action.as_str().map_or(ParsedAction::Unsupported, |action| {
            ParsedAction::Name(action.to_string())
        }),
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestAction {
        First,
        Second,
    }

    const EMPTY: &[&str] = &[];
    const CTRL_X_CTRL_E: &[&str] = &["ctrl+x ctrl+e"];
    const CTRL_G: &[&str] = &["ctrl+g"];

    fn registrations() -> Vec<CrabcodeKeybindingRegistration<TestAction>> {
        vec![
            CrabcodeKeybindingRegistration {
                action: TestAction::First,
                action_name: "chat:first",
                context: CrabcodeKeybindingContext::Chat,
                default_chords: CTRL_X_CTRL_E,
            },
            CrabcodeKeybindingRegistration {
                action: TestAction::Second,
                action_name: "chat:second",
                context: CrabcodeKeybindingContext::Chat,
                default_chords: CTRL_G,
            },
        ]
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn parser_preserves_historical_aliases_and_lone_space() {
        assert_eq!(parse_chord(" "), vec![parse_keystroke("space")]);
        assert_eq!(parse_keystroke("control+opt+return").key, "enter");
        assert!(parse_keystroke("control+opt+return").ctrl);
        assert!(parse_keystroke("control+opt+return").alt);
        assert_eq!(
            parse_keystroke("cmd+↑"),
            ParsedKeystroke {
                key: "up".to_string(),
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
                super_key: true,
            }
        );
    }

    #[test]
    fn fixed_historical_production_renderer_is_defaults_only_without_user_file_authority() {
        let engine = CrabcodeKeybindingEngine::for_renderer(registrations());
        assert_eq!(
            engine.resolve_single(
                &press(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &[CrabcodeKeybindingContext::Chat],
            ),
            CrabcodeKeybindingResolution::Match(TestAction::Second),
            "production must install the fixed renderer default"
        );

        let source = include_str!("crabcode_keybindings.rs");
        let constructor_start = source
            .find("    pub(crate) fn for_renderer(")
            .expect("production renderer constructor");
        let fixture_start = source[constructor_start..]
            .find("    #[cfg(test)]\n    fn with_user_file_fixture(")
            .map(|offset| constructor_start + offset)
            .expect("test-only user-file fixture boundary");
        let constructor = &source[constructor_start..fixture_start];
        assert!(constructor.contains("parse_default_bindings"));
        for forbidden in [
            "std::fs",
            "std::env",
            "load_fixture_bindings",
            "Path",
            "keybindings_path",
            "watch",
            "config",
            "protocol",
        ] {
            assert!(
                !constructor.contains(forbidden),
                "production constructor must not contain renderer-owned authority `{forbidden}`"
            );
        }

        let production_prefix = source
            .split_once("#[cfg(test)]\nmod tests")
            .expect("test module boundary")
            .0;
        for copied_authority in [
            "CRABCODE_CONFIG_DIR",
            "growthBookOverrides",
            "cachedGrowthBookFeatures",
            "DISABLE_TELEMETRY",
            "tengu_keybinding_customization_release",
            "KeybindingWatcher",
        ] {
            assert!(
                !production_prefix.contains(copied_authority),
                "Rust renderer must not duplicate backend/config authority `{copied_authority}`"
            );
        }
    }

    #[test]
    fn renderer_contexts_map_once_and_goal_console_stays_separate() {
        const PRODUCT_CONTEXTS: [(CrabcodeKeybindingContext, &str); 20] = [
            (CrabcodeKeybindingContext::Global, "Global"),
            (CrabcodeKeybindingContext::Chat, "Chat"),
            (CrabcodeKeybindingContext::Autocomplete, "Autocomplete"),
            (CrabcodeKeybindingContext::Confirmation, "Confirmation"),
            (CrabcodeKeybindingContext::Help, "Help"),
            (CrabcodeKeybindingContext::Transcript, "Transcript"),
            (CrabcodeKeybindingContext::HistorySearch, "HistorySearch"),
            (CrabcodeKeybindingContext::Task, "Task"),
            (CrabcodeKeybindingContext::ThemePicker, "ThemePicker"),
            (CrabcodeKeybindingContext::Settings, "Settings"),
            (CrabcodeKeybindingContext::Tabs, "Tabs"),
            (CrabcodeKeybindingContext::Attachments, "Attachments"),
            (CrabcodeKeybindingContext::Footer, "Footer"),
            (
                CrabcodeKeybindingContext::MessageSelector,
                "MessageSelector",
            ),
            (CrabcodeKeybindingContext::DiffDialog, "DiffDialog"),
            (CrabcodeKeybindingContext::ModelPicker, "ModelPicker"),
            (CrabcodeKeybindingContext::Select, "Select"),
            (CrabcodeKeybindingContext::Plugin, "Plugin"),
            (CrabcodeKeybindingContext::Scroll, "Scroll"),
            (CrabcodeKeybindingContext::MessageActions, "MessageActions"),
        ];

        let renderer_names = PRODUCT_CONTEXTS
            .into_iter()
            .map(|(context, name)| {
                assert_eq!(
                    context.as_str(),
                    name,
                    "{name} has an aliased Rust spelling"
                );
                name.to_string()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            renderer_names.len(),
            20,
            "renderer product contexts must be unique"
        );
        assert!(
            !renderer_names.contains(CrabcodeKeybindingContext::GoalConsole.as_str()),
            "GoalConsole is a separate renderer extension"
        );

        let registered_contexts = crate::tui_actions::crabcode_keybinding_registrations(false)
            .into_iter()
            .map(|registration| registration.context.as_str().to_string())
            .collect::<BTreeSet<_>>();
        assert!(renderer_names.is_subset(&registered_contexts));
        assert_eq!(
            registered_contexts
                .difference(&renderer_names)
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["GoalConsole".to_string()]),
            "current-only renderer contexts must stay explicitly partitioned"
        );
    }

    #[test]
    fn renderer_default_bindings_are_unique_and_extensions_are_explicit() {
        let registrations = crate::tui_actions::crabcode_keybinding_registrations(false);
        let mut observed = BTreeSet::<(String, String, String)>::new();
        for registration in registrations {
            for chord in registration.default_chords {
                assert!(
                    observed.insert((
                        registration.context.as_str().to_string(),
                        (*chord).to_string(),
                        registration.action_name.to_string(),
                    )),
                    "current default context/key/action triples must not be duplicated"
                );
            }
        }
        assert_eq!(
            observed.len(),
            if cfg!(target_os = "windows") {
                121
            } else {
                122
            },
            "renderer default count changed; update this product contract deliberately"
        );
        for extension in [
            ("Chat", "ctrl+x ctrl+g", "app:toggleGoalConsole"),
            ("GoalConsole", "escape", "goalConsole:dismiss"),
            ("GoalConsole", "q", "goalConsole:dismiss"),
        ] {
            assert!(
                observed.contains(&(
                    extension.0.to_string(),
                    extension.1.to_string(),
                    extension.2.to_string(),
                )),
                "current renderer extension is not registered: {extension:?}"
            );
        }
    }

    #[test]
    fn arbitrary_chord_prefers_prefix_and_swallows_invalid_completion() {
        let mut engine = CrabcodeKeybindingEngine::for_renderer(registrations());
        let contexts = [CrabcodeKeybindingContext::Chat];

        assert_eq!(
            engine.resolve(&press(KeyCode::Char('x'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::ChordStarted
        );
        assert_eq!(
            engine.resolve(&press(KeyCode::Char('z'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::ChordCancelled
        );
        assert_eq!(
            engine.resolve(&press(KeyCode::Char('x'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::ChordStarted
        );
        assert_eq!(
            engine.resolve(&press(KeyCode::Char('e'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::Match(TestAction::First)
        );
    }

    #[test]
    fn chord_timeout_makes_next_key_a_fresh_input() {
        let mut engine = CrabcodeKeybindingEngine::for_renderer(registrations());
        let contexts = [CrabcodeKeybindingContext::Chat];
        let started = Instant::now();
        assert_eq!(
            engine.resolve_at(
                &press(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &contexts,
                started,
            ),
            CrabcodeKeybindingResolution::ChordStarted
        );
        assert_eq!(
            engine.resolve_at(
                &press(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &contexts,
                started + CHORD_TIMEOUT,
            ),
            CrabcodeKeybindingResolution::Match(TestAction::Second)
        );
    }

    #[test]
    fn user_entries_append_last_and_null_is_skipped() {
        let directory = tempdir().expect("temporary config directory");
        let path = directory.path().join(KEYBINDINGS_FILE);
        fs::write(
            &path,
            r#"{
                "bindings": [
                    {"context":"Chat","bindings":{
                        "ctrl+g":"chat:first",
                        "ctrl+x ctrl+e":null
                    }}
                ]
            }"#,
        )
        .expect("write user bindings");
        let mut engine = CrabcodeKeybindingEngine::with_user_file_fixture(registrations(), path);
        let contexts = [CrabcodeKeybindingContext::Chat];
        assert_eq!(
            engine.resolve(&press(KeyCode::Char('g'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::Match(TestAction::First)
        );
        assert_eq!(
            engine.resolve(&press(KeyCode::Char('x'), KeyModifiers::CONTROL), &contexts),
            CrabcodeKeybindingResolution::ChordStarted,
            "the fixed parser skips null instead of unbinding the default chord"
        );
    }

    #[test]
    fn malformed_wrapper_falls_back_to_defaults() {
        let directory = tempdir().expect("temporary config directory");
        let path = directory.path().join(KEYBINDINGS_FILE);
        fs::write(&path, r#"{"notBindings":[]}"#).expect("write malformed bindings");
        let engine = CrabcodeKeybindingEngine::with_user_file_fixture(registrations(), path);
        assert_eq!(
            engine.resolve_single(
                &press(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &[CrabcodeKeybindingContext::Chat],
            ),
            CrabcodeKeybindingResolution::Match(TestAction::Second)
        );
    }

    #[test]
    fn unsupported_user_action_shadows_a_registered_default() {
        let directory = tempdir().expect("temporary config directory");
        let path = directory.path().join(KEYBINDINGS_FILE);
        fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+g":"not:registered"}}]}"#,
        )
        .expect("write unsupported binding");
        let engine = CrabcodeKeybindingEngine::with_user_file_fixture(registrations(), path);
        assert_eq!(
            engine.resolve_single(
                &press(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &[CrabcodeKeybindingContext::Chat],
            ),
            CrabcodeKeybindingResolution::Unmapped
        );
    }

    #[test]
    fn historical_command_action_resolves_dynamically_without_static_registration() {
        let directory = tempdir().expect("temporary config directory");
        let path = directory.path().join(KEYBINDINGS_FILE);
        fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+g":"command:reload-plugins"}}]}"#,
        )
        .expect("write command binding");
        let engine = CrabcodeKeybindingEngine::with_user_file_fixture(registrations(), path);
        assert_eq!(
            engine.resolve_single(
                &press(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &[CrabcodeKeybindingContext::Chat],
            ),
            CrabcodeKeybindingResolution::Command("reload-plugins".to_string())
        );
    }

    #[test]
    fn action_without_default_can_be_bound_by_user() {
        let directory = tempdir().expect("temporary config directory");
        let path = directory.path().join(KEYBINDINGS_FILE);
        fs::write(
            &path,
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+n":"chat:first"}}]}"#,
        )
        .expect("write user binding");
        let registrations = vec![CrabcodeKeybindingRegistration {
            action: TestAction::First,
            action_name: "chat:first",
            context: CrabcodeKeybindingContext::Chat,
            default_chords: EMPTY,
        }];
        let engine = CrabcodeKeybindingEngine::with_user_file_fixture(registrations, path);
        assert_eq!(
            engine.resolve_single(
                &press(KeyCode::Char('n'), KeyModifiers::CONTROL),
                &[CrabcodeKeybindingContext::Chat],
            ),
            CrabcodeKeybindingResolution::Match(TestAction::First)
        );
    }
}
