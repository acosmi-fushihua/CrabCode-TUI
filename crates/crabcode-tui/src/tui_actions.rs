//! Context-exact key actions for the native CrabCode terminal surface.
//!
//! The registry distinguishes interactions the Rust TUI can execute from
//! fixed historical actions whose owning surface is not ported yet. The
//! latter are consumed with an explicit renderer status instead of falling
//! through as text or pretending to have backend semantics. Backend commands
//! and SDK controls remain owned by QueryEngine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::crabcode_keybindings::{
    CrabcodeKeybindingContext, CrabcodeKeybindingEngine, CrabcodeKeybindingRegistration,
    CrabcodeKeybindingResolution,
};
use crate::terminal_capabilities::{TerminalName, ctrl_dot_shortcut_unreliable, terminal_context};
use crate::tui_render::KeyShortcut;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionContext {
    Prompt,
    Scrollback,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiActionId {
    Interrupt,
    Quit,
    Redraw,
    OpenHelp,
    ToggleTranscript,
    CancelPrompt,
    UndoPrompt,
    StashPrompt,
    OpenExternalEditor,
    FocusPrompt,
    FocusScrollback,
    ExitTranscript,
    SelectNext,
    SelectPrevious,
    CollapseSelected,
    ExpandSelected,
    ToggleSelectedFold,
    ToggleExpandAll,
    ToggleAllThinking,
    ScrollLineUp,
    ScrollLineDown,
    ScrollHalfPageUp,
    ScrollHalfPageDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    NextTurn,
    PreviousTurn,
    NextResponse,
    PreviousResponse,
    ToggleRaw,
    CopyTextSelection,
    CopyBlockContent,
    CopyBlockMeta,
    OpenBlockViewer,
    CycleLinkForward,
    CycleLinkBackward,
    KillBackgroundTask,
    SubmitPrompt,
    NewlinePrompt,
    OpenHistorySearch,
    HistorySearchNext,
    HistorySearchAccept,
    HistorySearchCancel,
    HistorySearchExecute,
    OpenGlobalSearch,
    OpenQuickOpen,
    HistoryPrevious,
    HistoryNext,
    OpenModelPicker,
    ImagePaste,
    AutocompleteAccept,
    AutocompleteDismiss,
    AutocompletePrevious,
    AutocompleteNext,
    ConfirmYes,
    ConfirmNo,
    ConfirmPrevious,
    ConfirmNext,
    ConfirmNextField,
    ConfirmPreviousField,
    ConfirmToggle,
    ToggleSyntaxHighlighting,
    AttachmentNext,
    AttachmentPrevious,
    AttachmentRemove,
    AttachmentExit,
    ToggleGoalConsole,
    DismissGoalConsole,
    DismissHelp,
    NextTab,
    PreviousTab,
    SelectDialogNext,
    SelectDialogPrevious,
    SelectDialogAccept,
    SelectDialogCancel,
    /// A fixed historical action whose owning TUI surface has not been
    /// ported. The keybinding is still resolved and consumed so it cannot
    /// fall through as text or be mistaken for implemented behavior.
    UnavailableHistorical(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiActionDef {
    pub id: TuiActionId,
    pub context: InteractionContext,
    pub default_key: KeyShortcut,
    pub alternate_keys: Vec<KeyShortcut>,
    pub requires_confirmation: bool,
    fixed_dispatch: bool,
}

#[derive(Debug)]
pub struct TuiActionRegistry {
    actions: Vec<TuiActionDef>,
    keybindings: CrabcodeKeybindingEngine<TuiActionId>,
}

impl TuiActionRegistry {
    pub fn for_screen_mode(minimal: bool) -> Self {
        let ctrl_dot_unreliable = ctrl_dot_shortcut_unreliable();
        let ctrl_dot = KeyShortcut::new(KeyCode::Char('.'), KeyModifiers::CONTROL);
        let ctrl_x = KeyShortcut::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let mut actions = vec![
            TuiActionDef {
                id: TuiActionId::Quit,
                context: InteractionContext::Global,
                default_key: KeyShortcut::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
                alternate_keys: vec![KeyShortcut::new(KeyCode::Char('d'), KeyModifiers::CONTROL)],
                requires_confirmation: true,
                fixed_dispatch: true,
            },
            TuiActionDef {
                id: TuiActionId::OpenHelp,
                context: InteractionContext::Global,
                default_key: if ctrl_dot_unreliable {
                    ctrl_x
                } else {
                    ctrl_dot
                },
                alternate_keys: vec![if ctrl_dot_unreliable {
                    ctrl_dot
                } else {
                    ctrl_x
                }],
                requires_confirmation: false,
                fixed_dispatch: true,
            },
            TuiActionDef {
                id: TuiActionId::ToggleTranscript,
                context: InteractionContext::Global,
                default_key: KeyShortcut::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
                alternate_keys: Vec::new(),
                requires_confirmation: false,
                fixed_dispatch: false,
            },
            TuiActionDef {
                id: TuiActionId::CancelPrompt,
                context: InteractionContext::Prompt,
                default_key: KeyShortcut::new(KeyCode::Esc, KeyModifiers::NONE),
                alternate_keys: Vec::new(),
                requires_confirmation: false,
                fixed_dispatch: true,
            },
            TuiActionDef {
                id: TuiActionId::UndoPrompt,
                context: InteractionContext::Prompt,
                default_key: KeyShortcut::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
                alternate_keys: vec![KeyShortcut::new(
                    KeyCode::Char('-'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )],
                requires_confirmation: false,
                fixed_dispatch: false,
            },
            TuiActionDef {
                id: TuiActionId::StashPrompt,
                context: InteractionContext::Prompt,
                default_key: KeyShortcut::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                alternate_keys: Vec::new(),
                requires_confirmation: false,
                fixed_dispatch: false,
            },
            TuiActionDef {
                id: TuiActionId::FocusScrollback,
                context: InteractionContext::Prompt,
                default_key: KeyShortcut::new(KeyCode::Tab, KeyModifiers::NONE),
                alternate_keys: Vec::new(),
                requires_confirmation: false,
                fixed_dispatch: true,
            },
            TuiActionDef {
                id: TuiActionId::OpenExternalEditor,
                context: InteractionContext::Prompt,
                default_key: KeyShortcut::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
                alternate_keys: Vec::new(),
                requires_confirmation: false,
                fixed_dispatch: false,
            },
            TuiActionDef {
                id: TuiActionId::FocusPrompt,
                context: InteractionContext::Scrollback,
                default_key: KeyShortcut::new(KeyCode::Tab, KeyModifiers::NONE),
                alternate_keys: vec![
                    KeyShortcut::new(KeyCode::Char('i'), KeyModifiers::NONE),
                    KeyShortcut::new(KeyCode::Char(' '), KeyModifiers::NONE),
                ],
                requires_confirmation: false,
                fixed_dispatch: true,
            },
            TuiActionDef {
                id: TuiActionId::ExitTranscript,
                context: InteractionContext::Scrollback,
                default_key: KeyShortcut::new(KeyCode::Char('q'), KeyModifiers::NONE),
                alternate_keys: vec![
                    KeyShortcut::new(KeyCode::Esc, KeyModifiers::NONE),
                    KeyShortcut::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                ],
                requires_confirmation: false,
                fixed_dispatch: false,
            },
            scrollback_action(
                TuiActionId::SelectNext,
                KeyShortcut::new(KeyCode::Char('j'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Down, KeyModifiers::NONE)),
            scrollback_action(
                TuiActionId::SelectPrevious,
                KeyShortcut::new(KeyCode::Char('k'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Up, KeyModifiers::NONE)),
            scrollback_action(
                TuiActionId::CollapseSelected,
                KeyShortcut::new(KeyCode::Char('h'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Left, KeyModifiers::NONE)),
            scrollback_action(
                TuiActionId::ExpandSelected,
                KeyShortcut::new(KeyCode::Char('l'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Right, KeyModifiers::NONE)),
            scrollback_action(
                TuiActionId::ToggleSelectedFold,
                KeyShortcut::new(KeyCode::Char('e'), KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::ToggleExpandAll,
                KeyShortcut::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
            ),
            scrollback_action(
                TuiActionId::ToggleAllThinking,
                KeyShortcut::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            ),
            scrollback_action(
                TuiActionId::ScrollLineUp,
                KeyShortcut::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ),
            scrollback_action(
                TuiActionId::ScrollLineDown,
                KeyShortcut::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            ),
            scrollback_action(
                TuiActionId::ScrollHalfPageUp,
                KeyShortcut::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            ),
            scrollback_action(
                TuiActionId::ScrollHalfPageDown,
                if matches!(
                    terminal_context().brand,
                    TerminalName::VsCode
                        | TerminalName::Cursor
                        | TerminalName::Windsurf
                        | TerminalName::Zed
                ) {
                    KeyShortcut::new(KeyCode::Char('D'), KeyModifiers::SHIFT)
                } else {
                    KeyShortcut::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
                },
            ),
            scrollback_action(
                TuiActionId::ScrollPageUp,
                KeyShortcut::new(KeyCode::PageUp, KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::ScrollPageDown,
                KeyShortcut::new(KeyCode::PageDown, KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::ScrollTop,
                KeyShortcut::new(KeyCode::Char('g'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Home, KeyModifiers::CONTROL)),
            scrollback_action(
                TuiActionId::ScrollBottom,
                KeyShortcut::new(KeyCode::Char('G'), KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::End, KeyModifiers::CONTROL)),
            scrollback_action(
                TuiActionId::NextTurn,
                KeyShortcut::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Right, KeyModifiers::SHIFT)),
            scrollback_action(
                TuiActionId::PreviousTurn,
                KeyShortcut::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Left, KeyModifiers::SHIFT)),
            scrollback_action(
                TuiActionId::NextResponse,
                KeyShortcut::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
            ),
            scrollback_action(
                TuiActionId::PreviousResponse,
                KeyShortcut::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
            ),
            scrollback_action(
                TuiActionId::ToggleRaw,
                KeyShortcut::new(KeyCode::Char('r'), KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::CopyTextSelection,
                KeyShortcut::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Char('c'), KeyModifiers::SUPER)),
            scrollback_action(
                TuiActionId::CopyBlockContent,
                KeyShortcut::new(KeyCode::Char('y'), KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::CopyBlockMeta,
                KeyShortcut::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            ),
            scrollback_action(
                TuiActionId::OpenBlockViewer,
                KeyShortcut::new(KeyCode::Enter, KeyModifiers::NONE),
            )
            .with_alternate(KeyShortcut::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            scrollback_action(
                TuiActionId::CycleLinkForward,
                KeyShortcut::new(KeyCode::Char('o'), KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::CycleLinkBackward,
                KeyShortcut::new(KeyCode::Char('O'), KeyModifiers::NONE),
            ),
            scrollback_action(
                TuiActionId::KillBackgroundTask,
                KeyShortcut::new(KeyCode::Char('x'), KeyModifiers::NONE),
            ),
        ];

        // Minimal mode has no interactive scrollback or dashboard.  Do not
        // register inert shortcuts for surfaces that the process cannot show.
        if minimal {
            actions.retain(|definition| {
                definition.context != InteractionContext::Scrollback
                    && definition.id != TuiActionId::FocusScrollback
                    && definition.id != TuiActionId::ToggleTranscript
            });
        }
        let keybindings =
            CrabcodeKeybindingEngine::for_renderer(crabcode_keybinding_registrations(minimal));
        Self {
            actions,
            keybindings,
        }
    }

    pub fn lookup(&self, event: &KeyEvent, context: InteractionContext) -> Option<TuiActionId> {
        let keybinding_context = match context {
            InteractionContext::Prompt => CrabcodeKeybindingContext::Chat,
            InteractionContext::Scrollback => CrabcodeKeybindingContext::Transcript,
            InteractionContext::Global => CrabcodeKeybindingContext::Global,
        };
        match self
            .keybindings
            .resolve_single(event, &[keybinding_context])
        {
            CrabcodeKeybindingResolution::Match(action) => return Some(action),
            CrabcodeKeybindingResolution::Command(_) | CrabcodeKeybindingResolution::Unmapped => {
                return None;
            }
            CrabcodeKeybindingResolution::ChordStarted
            | CrabcodeKeybindingResolution::ChordCancelled
            | CrabcodeKeybindingResolution::None => {}
        }
        self.actions
            .iter()
            .filter(|definition| definition.context == context && definition.fixed_dispatch)
            .find(|definition| {
                definition.default_key.matches(event)
                    || definition
                        .alternate_keys
                        .iter()
                        .any(|shortcut| shortcut.matches(event))
            })
            .map(|definition| definition.id)
    }

    pub(crate) fn resolve_keybinding(
        &mut self,
        event: &KeyEvent,
        active_contexts: &[CrabcodeKeybindingContext],
    ) -> CrabcodeKeybindingResolution<TuiActionId> {
        self.keybindings.resolve(event, active_contexts)
    }

    pub(crate) fn lookup_keybinding(
        &self,
        event: &KeyEvent,
        active_contexts: &[CrabcodeKeybindingContext],
    ) -> CrabcodeKeybindingResolution<TuiActionId> {
        self.keybindings.resolve_single(event, active_contexts)
    }

    #[cfg(test)]
    pub(crate) fn has_pending_chord(&self) -> bool {
        self.keybindings.has_pending_chord()
    }

    #[cfg(test)]
    pub(crate) fn expire_pending_chord(&mut self) {
        self.keybindings.expire_pending_chord();
    }

    pub fn definition(&self, id: TuiActionId) -> Option<&TuiActionDef> {
        self.actions.iter().find(|definition| definition.id == id)
    }

    #[cfg(test)]
    fn all(&self) -> &[TuiActionDef] {
        &self.actions
    }
}

pub(crate) fn crabcode_keybinding_registrations(
    minimal: bool,
) -> Vec<CrabcodeKeybindingRegistration<TuiActionId>> {
    let image_paste = if cfg!(target_os = "windows") {
        &["alt+v"][..]
    } else {
        &["ctrl+v"][..]
    };
    // The fixed historical source chooses meta+m only on Windows runtimes
    // without VT modifier support. This renderer has no Node/Bun runtime
    // version fact, so only the platform-invariant non-Windows default is
    // materialized; the action itself remains explicitly registered.
    let mode_cycle = if cfg!(target_os = "windows") {
        &[][..]
    } else {
        &["shift+tab"][..]
    };
    let binding = |action, action_name, context, default_chords: &'static [&'static str]| {
        CrabcodeKeybindingRegistration {
            action,
            action_name,
            context,
            default_chords,
        }
    };
    let unavailable =
        |action_name: &'static str, context, default_chords: &'static [&'static str]| {
            CrabcodeKeybindingRegistration {
                action: TuiActionId::UnavailableHistorical(action_name),
                action_name,
                context,
                default_chords,
            }
        };

    // Ordering follows the fixed historical DEFAULT_BINDINGS blocks. The
    // resolver is last-match-wins across all active contexts, so preserving
    // this order is part of the executable behavior (for example ThemePicker
    // ctrl+t must shadow Global app:toggleTodos).
    let mut registrations = vec![
        // Global.
        binding(
            TuiActionId::Interrupt,
            "app:interrupt",
            CrabcodeKeybindingContext::Global,
            &["ctrl+c"],
        ),
        binding(
            TuiActionId::Quit,
            "app:exit",
            CrabcodeKeybindingContext::Global,
            &["ctrl+d"],
        ),
        binding(
            TuiActionId::Redraw,
            "app:redraw",
            CrabcodeKeybindingContext::Global,
            &["ctrl+l"],
        ),
        unavailable(
            "app:toggleTodos",
            CrabcodeKeybindingContext::Global,
            &["ctrl+t"],
        ),
        binding(
            TuiActionId::ToggleTranscript,
            "app:toggleTranscript",
            CrabcodeKeybindingContext::Global,
            &["ctrl+o"],
        ),
        unavailable("app:toggleBrief", CrabcodeKeybindingContext::Global, &[]),
        unavailable(
            "app:toggleTeammatePreview",
            CrabcodeKeybindingContext::Global,
            &["ctrl+shift+o"],
        ),
        binding(
            TuiActionId::OpenHistorySearch,
            "history:search",
            CrabcodeKeybindingContext::Global,
            &["ctrl+r"],
        ),
        binding(
            TuiActionId::OpenGlobalSearch,
            "app:globalSearch",
            CrabcodeKeybindingContext::Global,
            // Fixed defaults are gated by QUICK_SEARCH. This native crate has
            // no exact authority for that build-time feature, so the action
            // remains user-bindable but its feature-owned defaults fail
            // closed.
            &[],
        ),
        binding(
            TuiActionId::OpenQuickOpen,
            "app:quickOpen",
            CrabcodeKeybindingContext::Global,
            &[],
        ),
        unavailable("app:toggleTerminal", CrabcodeKeybindingContext::Global, &[]),
        // Chat.
        binding(
            TuiActionId::CancelPrompt,
            "chat:cancel",
            CrabcodeKeybindingContext::Chat,
            &["escape"],
        ),
        unavailable(
            "chat:killAgents",
            CrabcodeKeybindingContext::Chat,
            &["ctrl+x ctrl+k"],
        ),
        binding(
            TuiActionId::ToggleGoalConsole,
            "app:toggleGoalConsole",
            CrabcodeKeybindingContext::Chat,
            &["ctrl+x ctrl+g"],
        ),
        unavailable(
            "chat:cycleMode",
            CrabcodeKeybindingContext::Chat,
            mode_cycle,
        ),
        binding(
            TuiActionId::OpenModelPicker,
            "chat:modelPicker",
            CrabcodeKeybindingContext::Chat,
            &["meta+p"],
        ),
        unavailable(
            "chat:fastMode",
            CrabcodeKeybindingContext::Chat,
            &["meta+o"],
        ),
        unavailable(
            "chat:thinkingToggle",
            CrabcodeKeybindingContext::Chat,
            &["meta+t"],
        ),
        binding(
            TuiActionId::SubmitPrompt,
            "chat:submit",
            CrabcodeKeybindingContext::Chat,
            &["enter"],
        ),
        binding(
            TuiActionId::NewlinePrompt,
            "chat:newline",
            CrabcodeKeybindingContext::Chat,
            &[],
        ),
        binding(
            TuiActionId::HistoryPrevious,
            "history:previous",
            CrabcodeKeybindingContext::Chat,
            &["up"],
        ),
        binding(
            TuiActionId::HistoryNext,
            "history:next",
            CrabcodeKeybindingContext::Chat,
            &["down"],
        ),
        binding(
            TuiActionId::UndoPrompt,
            "chat:undo",
            CrabcodeKeybindingContext::Chat,
            &["ctrl+_", "ctrl+shift+-"],
        ),
        binding(
            TuiActionId::OpenExternalEditor,
            "chat:externalEditor",
            CrabcodeKeybindingContext::Chat,
            &["ctrl+x ctrl+e", "ctrl+g"],
        ),
        binding(
            TuiActionId::StashPrompt,
            "chat:stash",
            CrabcodeKeybindingContext::Chat,
            &["ctrl+s"],
        ),
        binding(
            TuiActionId::ImagePaste,
            "chat:imagePaste",
            CrabcodeKeybindingContext::Chat,
            image_paste,
        ),
        unavailable("chat:messageActions", CrabcodeKeybindingContext::Chat, &[]),
        unavailable("voice:pushToTalk", CrabcodeKeybindingContext::Chat, &[]),
        // Autocomplete.
        binding(
            TuiActionId::AutocompleteAccept,
            "autocomplete:accept",
            CrabcodeKeybindingContext::Autocomplete,
            &["tab"],
        ),
        binding(
            TuiActionId::AutocompleteDismiss,
            "autocomplete:dismiss",
            CrabcodeKeybindingContext::Autocomplete,
            &["escape"],
        ),
        binding(
            TuiActionId::AutocompletePrevious,
            "autocomplete:previous",
            CrabcodeKeybindingContext::Autocomplete,
            &["up"],
        ),
        binding(
            TuiActionId::AutocompleteNext,
            "autocomplete:next",
            CrabcodeKeybindingContext::Autocomplete,
            &["down"],
        ),
        // Settings. This context is inactive until a Settings surface exists;
        // it must never be borrowed by unrelated text-entry dialogs.
        binding(
            TuiActionId::ConfirmNo,
            "confirm:no",
            CrabcodeKeybindingContext::Settings,
            &["escape"],
        ),
        binding(
            TuiActionId::SelectDialogPrevious,
            "select:previous",
            CrabcodeKeybindingContext::Settings,
            &["up", "k", "ctrl+p"],
        ),
        binding(
            TuiActionId::SelectDialogNext,
            "select:next",
            CrabcodeKeybindingContext::Settings,
            &["down", "j", "ctrl+n"],
        ),
        binding(
            TuiActionId::SelectDialogAccept,
            "select:accept",
            CrabcodeKeybindingContext::Settings,
            &["space"],
        ),
        unavailable(
            "settings:close",
            CrabcodeKeybindingContext::Settings,
            &["enter"],
        ),
        unavailable(
            "settings:search",
            CrabcodeKeybindingContext::Settings,
            &["/"],
        ),
        unavailable(
            "settings:retry",
            CrabcodeKeybindingContext::Settings,
            &["r"],
        ),
        // Confirmation.
        binding(
            TuiActionId::ConfirmYes,
            "confirm:yes",
            CrabcodeKeybindingContext::Confirmation,
            &["y", "enter"],
        ),
        binding(
            TuiActionId::ConfirmNo,
            "confirm:no",
            CrabcodeKeybindingContext::Confirmation,
            &["n", "escape"],
        ),
        binding(
            TuiActionId::ConfirmPrevious,
            "confirm:previous",
            CrabcodeKeybindingContext::Confirmation,
            &["up"],
        ),
        binding(
            TuiActionId::ConfirmNext,
            "confirm:next",
            CrabcodeKeybindingContext::Confirmation,
            &["down"],
        ),
        binding(
            TuiActionId::ConfirmNextField,
            "confirm:nextField",
            CrabcodeKeybindingContext::Confirmation,
            &["tab"],
        ),
        binding(
            TuiActionId::ConfirmPreviousField,
            "confirm:previousField",
            CrabcodeKeybindingContext::Confirmation,
            &[],
        ),
        unavailable(
            "confirm:cycleMode",
            CrabcodeKeybindingContext::Confirmation,
            &["shift+tab"],
        ),
        binding(
            TuiActionId::ConfirmToggle,
            "confirm:toggle",
            CrabcodeKeybindingContext::Confirmation,
            &["space"],
        ),
        unavailable(
            "confirm:toggleExplanation",
            CrabcodeKeybindingContext::Confirmation,
            &["ctrl+e"],
        ),
        unavailable(
            "permission:toggleDebug",
            CrabcodeKeybindingContext::Confirmation,
            &["ctrl+d"],
        ),
        // Tabs.
        binding(
            TuiActionId::NextTab,
            "tabs:next",
            CrabcodeKeybindingContext::Tabs,
            &["tab", "right"],
        ),
        binding(
            TuiActionId::PreviousTab,
            "tabs:previous",
            CrabcodeKeybindingContext::Tabs,
            &["shift+tab", "left"],
        ),
        // Transcript.
        unavailable(
            "transcript:toggleShowAll",
            CrabcodeKeybindingContext::Transcript,
            &["ctrl+e"],
        ),
        binding(
            TuiActionId::ExitTranscript,
            "transcript:exit",
            CrabcodeKeybindingContext::Transcript,
            &["ctrl+c", "escape", "q"],
        ),
        // History search.
        binding(
            TuiActionId::HistorySearchNext,
            "historySearch:next",
            CrabcodeKeybindingContext::HistorySearch,
            &["ctrl+r"],
        ),
        binding(
            TuiActionId::HistorySearchAccept,
            "historySearch:accept",
            CrabcodeKeybindingContext::HistorySearch,
            &["escape", "tab"],
        ),
        binding(
            TuiActionId::HistorySearchCancel,
            "historySearch:cancel",
            CrabcodeKeybindingContext::HistorySearch,
            &["ctrl+c"],
        ),
        binding(
            TuiActionId::HistorySearchExecute,
            "historySearch:execute",
            CrabcodeKeybindingContext::HistorySearch,
            &["enter"],
        ),
        // Foreground task.
        unavailable(
            "task:background",
            CrabcodeKeybindingContext::Task,
            &["ctrl+b"],
        ),
        // Theme picker.
        binding(
            TuiActionId::ToggleSyntaxHighlighting,
            "theme:toggleSyntaxHighlighting",
            CrabcodeKeybindingContext::ThemePicker,
            &["ctrl+t"],
        ),
        // These actions are present in fixed DEFAULT_BINDINGS but absent from
        // fixed KEYBINDING_ACTIONS. Exact renderer-owned scroll actions are
        // retained; selection copy fails closed because terminal selection is
        // a different semantic from copying a transcript block.
        binding(
            TuiActionId::ScrollPageUp,
            "scroll:pageUp",
            CrabcodeKeybindingContext::Scroll,
            &["pageup"],
        ),
        binding(
            TuiActionId::ScrollPageDown,
            "scroll:pageDown",
            CrabcodeKeybindingContext::Scroll,
            &["pagedown"],
        ),
        binding(
            TuiActionId::ScrollLineUp,
            "scroll:lineUp",
            CrabcodeKeybindingContext::Scroll,
            &["wheelup"],
        ),
        binding(
            TuiActionId::ScrollLineDown,
            "scroll:lineDown",
            CrabcodeKeybindingContext::Scroll,
            &["wheeldown"],
        ),
        binding(
            TuiActionId::ScrollTop,
            "scroll:top",
            CrabcodeKeybindingContext::Scroll,
            &["ctrl+home"],
        ),
        binding(
            TuiActionId::ScrollBottom,
            "scroll:bottom",
            CrabcodeKeybindingContext::Scroll,
            &["ctrl+end"],
        ),
        binding(
            TuiActionId::CopyTextSelection,
            "selection:copy",
            CrabcodeKeybindingContext::Scroll,
            &["ctrl+shift+c", "cmd+c"],
        ),
        // Help.
        binding(
            TuiActionId::DismissHelp,
            "help:dismiss",
            CrabcodeKeybindingContext::Help,
            &["escape"],
        ),
        // Attachments.
        binding(
            TuiActionId::AttachmentNext,
            "attachments:next",
            CrabcodeKeybindingContext::Attachments,
            &["right"],
        ),
        binding(
            TuiActionId::AttachmentPrevious,
            "attachments:previous",
            CrabcodeKeybindingContext::Attachments,
            &["left"],
        ),
        binding(
            TuiActionId::AttachmentRemove,
            "attachments:remove",
            CrabcodeKeybindingContext::Attachments,
            &["backspace", "delete"],
        ),
        binding(
            TuiActionId::AttachmentExit,
            "attachments:exit",
            CrabcodeKeybindingContext::Attachments,
            &["down", "escape"],
        ),
        // Footer.
        unavailable(
            "footer:up",
            CrabcodeKeybindingContext::Footer,
            &["up", "ctrl+p"],
        ),
        unavailable(
            "footer:down",
            CrabcodeKeybindingContext::Footer,
            &["down", "ctrl+n"],
        ),
        unavailable("footer:next", CrabcodeKeybindingContext::Footer, &["right"]),
        unavailable(
            "footer:previous",
            CrabcodeKeybindingContext::Footer,
            &["left"],
        ),
        unavailable(
            "footer:openSelected",
            CrabcodeKeybindingContext::Footer,
            &["enter"],
        ),
        unavailable(
            "footer:clearSelection",
            CrabcodeKeybindingContext::Footer,
            &["escape"],
        ),
        unavailable("footer:close", CrabcodeKeybindingContext::Footer, &[]),
        // Message selector / rewind.
        unavailable(
            "messageSelector:up",
            CrabcodeKeybindingContext::MessageSelector,
            &["up", "k", "ctrl+p"],
        ),
        unavailable(
            "messageSelector:down",
            CrabcodeKeybindingContext::MessageSelector,
            &["down", "j", "ctrl+n"],
        ),
        unavailable(
            "messageSelector:top",
            CrabcodeKeybindingContext::MessageSelector,
            &["ctrl+up", "shift+up", "meta+up", "shift+k"],
        ),
        unavailable(
            "messageSelector:bottom",
            CrabcodeKeybindingContext::MessageSelector,
            &["ctrl+down", "shift+down", "meta+down", "shift+j"],
        ),
        unavailable(
            "messageSelector:select",
            CrabcodeKeybindingContext::MessageSelector,
            &["enter"],
        ),
        // Fixed defaults expose these only behind MESSAGE_ACTIONS. With no
        // corresponding renderer feature fact or UI, keep them user-bindable
        // but do not activate the feature-conditional default gestures.
        unavailable(
            "messageActions:prev",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:next",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:top",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:bottom",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:prevUser",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:nextUser",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:escape",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:ctrlc",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:enter",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:c",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        unavailable(
            "messageActions:p",
            CrabcodeKeybindingContext::MessageActions,
            &[],
        ),
        // Diff dialog.
        unavailable(
            "diff:dismiss",
            CrabcodeKeybindingContext::DiffDialog,
            &["escape"],
        ),
        unavailable(
            "diff:previousSource",
            CrabcodeKeybindingContext::DiffDialog,
            &["left"],
        ),
        unavailable(
            "diff:nextSource",
            CrabcodeKeybindingContext::DiffDialog,
            &["right"],
        ),
        unavailable("diff:back", CrabcodeKeybindingContext::DiffDialog, &[]),
        unavailable(
            "diff:viewDetails",
            CrabcodeKeybindingContext::DiffDialog,
            &["enter"],
        ),
        unavailable(
            "diff:previousFile",
            CrabcodeKeybindingContext::DiffDialog,
            &["up"],
        ),
        unavailable(
            "diff:nextFile",
            CrabcodeKeybindingContext::DiffDialog,
            &["down"],
        ),
        // Model picker effort.
        unavailable(
            "modelPicker:decreaseEffort",
            CrabcodeKeybindingContext::ModelPicker,
            &["left"],
        ),
        unavailable(
            "modelPicker:increaseEffort",
            CrabcodeKeybindingContext::ModelPicker,
            &["right"],
        ),
        // Select.
        binding(
            TuiActionId::SelectDialogPrevious,
            "select:previous",
            CrabcodeKeybindingContext::Select,
            &["up", "k", "ctrl+p"],
        ),
        binding(
            TuiActionId::SelectDialogNext,
            "select:next",
            CrabcodeKeybindingContext::Select,
            &["down", "j", "ctrl+n"],
        ),
        binding(
            TuiActionId::SelectDialogAccept,
            "select:accept",
            CrabcodeKeybindingContext::Select,
            &["enter"],
        ),
        binding(
            TuiActionId::SelectDialogCancel,
            "select:cancel",
            CrabcodeKeybindingContext::Select,
            &["escape"],
        ),
        // Plugin.
        unavailable(
            "plugin:toggle",
            CrabcodeKeybindingContext::Plugin,
            &["space"],
        ),
        unavailable("plugin:install", CrabcodeKeybindingContext::Plugin, &["i"]),
        // Goal console.
        binding(
            TuiActionId::DismissGoalConsole,
            "goalConsole:dismiss",
            CrabcodeKeybindingContext::GoalConsole,
            &["escape", "q"],
        ),
    ];
    if minimal {
        registrations.retain(|registration| {
            registration.action != TuiActionId::ToggleTranscript
                && !matches!(
                    registration.context,
                    CrabcodeKeybindingContext::Transcript | CrabcodeKeybindingContext::Scroll
                )
        });
    }
    registrations
}

fn scrollback_action(id: TuiActionId, default_key: KeyShortcut) -> TuiActionDef {
    TuiActionDef {
        id,
        context: InteractionContext::Scrollback,
        default_key,
        alternate_keys: Vec::new(),
        requires_confirmation: false,
        fixed_dispatch: true,
    }
}

impl TuiActionDef {
    fn with_alternate(mut self, shortcut: KeyShortcut) -> Self {
        self.alternate_keys.push(shortcut);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_lookup_exact_context() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(
            registry.lookup(&quit, InteractionContext::Global),
            Some(TuiActionId::Quit)
        );
        assert_eq!(registry.lookup(&quit, InteractionContext::Prompt), None);
        assert_eq!(registry.lookup(&quit, InteractionContext::Scrollback), None);

        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(
            registry.lookup(&page_up, InteractionContext::Scrollback),
            Some(TuiActionId::ScrollPageUp)
        );
        assert_eq!(registry.lookup(&page_up, InteractionContext::Prompt), None);
    }

    #[test]
    fn actions_minimal_omit_unsupported() {
        let registry = TuiActionRegistry::for_screen_mode(true);
        assert!(
            registry
                .all()
                .iter()
                .all(|definition| definition.context != InteractionContext::Scrollback)
        );
        assert!(registry.definition(TuiActionId::FocusScrollback).is_none());
        assert!(registry.definition(TuiActionId::CycleLinkForward).is_none());
        assert!(registry.definition(TuiActionId::Quit).is_some());
        assert!(registry.definition(TuiActionId::OpenHelp).is_some());
        assert!(
            registry
                .definition(TuiActionId::OpenExternalEditor)
                .is_some()
        );
    }

    #[test]
    fn actions_quit_requires_confirmation() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        assert!(
            registry
                .definition(TuiActionId::Quit)
                .is_some_and(|definition| definition.requires_confirmation)
        );
    }

    #[test]
    fn actions_scrollback_context_only() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        assert_eq!(
            registry
                .all()
                .iter()
                .filter(|definition| definition.context == InteractionContext::Scrollback)
                .count(),
            29,
            "the 28 fixed upstream scrollback actions remain intact and the historical CrabCode transcript-exit action is added as the sole product overlay"
        );
        for action in [
            TuiActionId::ExitTranscript,
            TuiActionId::ScrollLineUp,
            TuiActionId::ScrollLineDown,
            TuiActionId::SelectNext,
            TuiActionId::SelectPrevious,
            TuiActionId::CollapseSelected,
            TuiActionId::ExpandSelected,
            TuiActionId::ToggleSelectedFold,
            TuiActionId::ToggleExpandAll,
            TuiActionId::ToggleAllThinking,
            TuiActionId::ScrollHalfPageUp,
            TuiActionId::ScrollHalfPageDown,
            TuiActionId::ScrollPageUp,
            TuiActionId::ScrollPageDown,
            TuiActionId::ScrollTop,
            TuiActionId::ScrollBottom,
            TuiActionId::NextTurn,
            TuiActionId::PreviousTurn,
            TuiActionId::NextResponse,
            TuiActionId::PreviousResponse,
            TuiActionId::ToggleRaw,
            TuiActionId::CopyTextSelection,
            TuiActionId::CopyBlockContent,
            TuiActionId::CopyBlockMeta,
            TuiActionId::OpenBlockViewer,
            TuiActionId::CycleLinkForward,
            TuiActionId::CycleLinkBackward,
            TuiActionId::KillBackgroundTask,
        ] {
            assert_eq!(
                registry
                    .definition(action)
                    .map(|definition| definition.context),
                Some(InteractionContext::Scrollback)
            );
        }
    }

    #[test]
    fn upstream_scrollback_selection_and_fold_keys_are_exact() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for (event, expected) in [
            (
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                TuiActionId::SelectNext,
            ),
            (
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                TuiActionId::SelectNext,
            ),
            (
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
                TuiActionId::SelectPrevious,
            ),
            (
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                TuiActionId::SelectPrevious,
            ),
            (
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                TuiActionId::CollapseSelected,
            ),
            (
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                TuiActionId::CollapseSelected,
            ),
            (
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
                TuiActionId::ExpandSelected,
            ),
            (
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
                TuiActionId::ExpandSelected,
            ),
            (
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
                TuiActionId::ToggleSelectedFold,
            ),
            (
                KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
                TuiActionId::ToggleExpandAll,
            ),
            (
                KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT),
                TuiActionId::NextResponse,
            ),
            (
                KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT),
                TuiActionId::PreviousResponse,
            ),
        ] {
            assert_eq!(
                registry.lookup(&event, InteractionContext::Scrollback),
                Some(expected)
            );
            let prompt_action = match event.code {
                KeyCode::Up => Some(TuiActionId::HistoryPrevious),
                KeyCode::Down => Some(TuiActionId::HistoryNext),
                _ => None,
            };
            assert_eq!(
                registry.lookup(&event, InteractionContext::Prompt),
                prompt_action,
                "Chat history Up/Down and transcript selection must remain context-isolated"
            );
        }
        assert_eq!(
            registry.lookup(
                &KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                InteractionContext::Scrollback,
            ),
            Some(TuiActionId::UnavailableHistorical(
                "transcript:toggleShowAll"
            )),
            "historical Ctrl-E toggles transcript truncation; it must not be falsely mapped to \
             the fixed renderer's different ToggleAllThinking semantic"
        );
    }

    #[test]
    fn upstream_scrollback_turn_raw_copy_viewer_and_task_keys_are_exact() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for (event, expected) in [
            (
                KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
                TuiActionId::NextTurn,
            ),
            (
                KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
                TuiActionId::NextTurn,
            ),
            (
                KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT),
                TuiActionId::PreviousTurn,
            ),
            (
                KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
                TuiActionId::PreviousTurn,
            ),
            (
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
                TuiActionId::ToggleRaw,
            ),
            (
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                TuiActionId::CopyBlockContent,
            ),
            (
                KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
                TuiActionId::CopyBlockMeta,
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                TuiActionId::OpenBlockViewer,
            ),
            (
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                TuiActionId::OpenBlockViewer,
            ),
            (
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                TuiActionId::KillBackgroundTask,
            ),
        ] {
            assert_eq!(
                registry.lookup(&event, InteractionContext::Scrollback),
                Some(expected)
            );
            assert_eq!(
                registry.lookup(&event, InteractionContext::Prompt),
                (event.code == KeyCode::Enter).then_some(TuiActionId::SubmitPrompt)
            );
        }
    }

    #[test]
    fn enter_dispatches_by_scrollback_or_chat_context() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            registry.lookup(&enter, InteractionContext::Scrollback),
            Some(TuiActionId::OpenBlockViewer)
        );
        assert_eq!(
            registry.lookup(&enter, InteractionContext::Prompt),
            Some(TuiActionId::SubmitPrompt)
        );
    }

    #[test]
    fn historical_meta_p_opens_model_picker_only_from_chat_context() {
        let mut registry = TuiActionRegistry::for_screen_mode(false);
        let meta_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT);
        assert_eq!(
            registry.resolve_keybinding(&meta_p, &[CrabcodeKeybindingContext::Chat]),
            CrabcodeKeybindingResolution::Match(TuiActionId::OpenModelPicker)
        );
        assert_eq!(
            registry.resolve_keybinding(&meta_p, &[CrabcodeKeybindingContext::Transcript]),
            CrabcodeKeybindingResolution::None
        );
    }

    #[test]
    fn historical_redraw_and_history_search_actions_resolve_to_executable_renderer_actions() {
        let mut registry = TuiActionRegistry::for_screen_mode(false);
        for (event, contexts, expected) in [
            (
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                vec![CrabcodeKeybindingContext::Global],
                TuiActionId::Redraw,
            ),
            (
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                vec![
                    CrabcodeKeybindingContext::HistorySearch,
                    CrabcodeKeybindingContext::Global,
                ],
                TuiActionId::HistorySearchNext,
            ),
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                vec![
                    CrabcodeKeybindingContext::HistorySearch,
                    CrabcodeKeybindingContext::Global,
                ],
                TuiActionId::HistorySearchAccept,
            ),
            (
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                vec![
                    CrabcodeKeybindingContext::HistorySearch,
                    CrabcodeKeybindingContext::Global,
                ],
                TuiActionId::HistorySearchCancel,
            ),
            (
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                vec![
                    CrabcodeKeybindingContext::HistorySearch,
                    CrabcodeKeybindingContext::Global,
                ],
                TuiActionId::HistorySearchExecute,
            ),
        ] {
            assert_eq!(
                registry.resolve_keybinding(&event, &contexts),
                CrabcodeKeybindingResolution::Match(expected)
            );
        }
    }

    #[test]
    fn feature_owned_quick_search_defaults_fail_closed_without_feature_authority() {
        let mut registry = TuiActionRegistry::for_screen_mode(false);
        for key in [
            KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::SUPER | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::SUPER | KeyModifiers::SHIFT,
            ),
        ] {
            assert_eq!(
                registry.resolve_keybinding(&key, &[CrabcodeKeybindingContext::Global]),
                CrabcodeKeybindingResolution::None
            );
        }
        let registrations = crabcode_keybinding_registrations(false);
        for action in [TuiActionId::OpenQuickOpen, TuiActionId::OpenGlobalSearch] {
            let registration = registrations
                .iter()
                .find(|registration| registration.action == action)
                .expect("feature-owned action remains available for an exact user binding");
            assert!(
                registration.default_chords.is_empty(),
                "a81 defaults QUICK_SEARCH to false and the direct renderer has no authoritative \
                 enabled feature fact"
            );
        }
    }

    #[test]
    fn fixed_historical_keybinding_vocabulary_is_complete_and_explicitly_partitioned() {
        const FIXED_SCHEMA_CONTEXTS: [CrabcodeKeybindingContext; 20] = [
            CrabcodeKeybindingContext::Global,
            CrabcodeKeybindingContext::Chat,
            CrabcodeKeybindingContext::Autocomplete,
            CrabcodeKeybindingContext::Confirmation,
            CrabcodeKeybindingContext::Help,
            CrabcodeKeybindingContext::Transcript,
            CrabcodeKeybindingContext::HistorySearch,
            CrabcodeKeybindingContext::Task,
            CrabcodeKeybindingContext::ThemePicker,
            CrabcodeKeybindingContext::Settings,
            CrabcodeKeybindingContext::Tabs,
            CrabcodeKeybindingContext::Attachments,
            CrabcodeKeybindingContext::Footer,
            CrabcodeKeybindingContext::MessageSelector,
            CrabcodeKeybindingContext::DiffDialog,
            CrabcodeKeybindingContext::ModelPicker,
            CrabcodeKeybindingContext::Select,
            CrabcodeKeybindingContext::Plugin,
            CrabcodeKeybindingContext::Scroll,
            CrabcodeKeybindingContext::MessageActions,
        ];
        const A81_RENDERER_EXTENSION_CONTEXTS: [CrabcodeKeybindingContext; 1] =
            [CrabcodeKeybindingContext::GoalConsole];
        const FIXED_SCHEMA_ACTIONS: [&str; 86] = [
            "app:interrupt",
            "app:exit",
            "app:toggleTodos",
            "app:toggleTranscript",
            "app:toggleBrief",
            "app:toggleTeammatePreview",
            "app:toggleTerminal",
            "app:redraw",
            "app:globalSearch",
            "app:quickOpen",
            "history:search",
            "history:previous",
            "history:next",
            "chat:cancel",
            "chat:killAgents",
            "chat:cycleMode",
            "chat:modelPicker",
            "chat:fastMode",
            "chat:thinkingToggle",
            "chat:submit",
            "chat:newline",
            "chat:undo",
            "chat:externalEditor",
            "chat:stash",
            "chat:imagePaste",
            "chat:messageActions",
            "autocomplete:accept",
            "autocomplete:dismiss",
            "autocomplete:previous",
            "autocomplete:next",
            "confirm:yes",
            "confirm:no",
            "confirm:previous",
            "confirm:next",
            "confirm:nextField",
            "confirm:previousField",
            "confirm:cycleMode",
            "confirm:toggle",
            "confirm:toggleExplanation",
            "tabs:next",
            "tabs:previous",
            "transcript:toggleShowAll",
            "transcript:exit",
            "historySearch:next",
            "historySearch:accept",
            "historySearch:cancel",
            "historySearch:execute",
            "task:background",
            "theme:toggleSyntaxHighlighting",
            "help:dismiss",
            "attachments:next",
            "attachments:previous",
            "attachments:remove",
            "attachments:exit",
            "footer:up",
            "footer:down",
            "footer:next",
            "footer:previous",
            "footer:openSelected",
            "footer:clearSelection",
            "footer:close",
            "messageSelector:up",
            "messageSelector:down",
            "messageSelector:top",
            "messageSelector:bottom",
            "messageSelector:select",
            "diff:dismiss",
            "diff:previousSource",
            "diff:nextSource",
            "diff:back",
            "diff:viewDetails",
            "diff:previousFile",
            "diff:nextFile",
            "modelPicker:decreaseEffort",
            "modelPicker:increaseEffort",
            "select:next",
            "select:previous",
            "select:accept",
            "select:cancel",
            "plugin:toggle",
            "plugin:install",
            "permission:toggleDebug",
            "settings:search",
            "settings:retry",
            "settings:close",
            "voice:pushToTalk",
        ];
        const A81_RENDERER_EXTENSION_ACTIONS: [&str; 2] =
            ["app:toggleGoalConsole", "goalConsole:dismiss"];
        const EXACTLY_IMPLEMENTED_SCHEMA_ACTIONS: [&str; 45] = [
            "app:interrupt",
            "app:exit",
            "app:redraw",
            "app:toggleTranscript",
            "app:globalSearch",
            "app:quickOpen",
            "history:search",
            "history:previous",
            "history:next",
            "chat:cancel",
            "chat:modelPicker",
            "chat:submit",
            "chat:newline",
            "chat:undo",
            "chat:externalEditor",
            "chat:stash",
            "chat:imagePaste",
            "autocomplete:accept",
            "autocomplete:dismiss",
            "autocomplete:previous",
            "autocomplete:next",
            "confirm:yes",
            "confirm:no",
            "confirm:previous",
            "confirm:next",
            "confirm:nextField",
            "confirm:previousField",
            "confirm:toggle",
            "transcript:exit",
            "historySearch:next",
            "historySearch:accept",
            "historySearch:cancel",
            "historySearch:execute",
            "theme:toggleSyntaxHighlighting",
            "help:dismiss",
            "tabs:next",
            "tabs:previous",
            "attachments:next",
            "attachments:previous",
            "attachments:remove",
            "attachments:exit",
            "select:next",
            "select:previous",
            "select:accept",
            "select:cancel",
        ];
        const FIXED_DEFAULT_ONLY_ACTIONS: [&str; 18] = [
            "scroll:pageUp",
            "scroll:pageDown",
            "scroll:lineUp",
            "scroll:lineDown",
            "scroll:top",
            "scroll:bottom",
            "selection:copy",
            "messageActions:prev",
            "messageActions:next",
            "messageActions:top",
            "messageActions:bottom",
            "messageActions:prevUser",
            "messageActions:nextUser",
            "messageActions:escape",
            "messageActions:ctrlc",
            "messageActions:enter",
            "messageActions:c",
            "messageActions:p",
        ];

        let registrations = crabcode_keybinding_registrations(false);
        assert_eq!(registrations.len(), 110);
        let schema_actions = FIXED_SCHEMA_ACTIONS
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let implemented_expected = EXACTLY_IMPLEMENTED_SCHEMA_ACTIONS
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let default_only_actions = FIXED_DEFAULT_ONLY_ACTIONS
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let registered_actions = registrations
            .iter()
            .map(|registration| registration.action_name)
            .collect::<std::collections::BTreeSet<_>>();
        let registered_contexts = registrations
            .iter()
            .map(|registration| registration.context)
            .collect::<std::collections::BTreeSet<_>>();
        let expected_contexts = FIXED_SCHEMA_CONTEXTS
            .into_iter()
            .chain(A81_RENDERER_EXTENSION_CONTEXTS)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(registered_contexts, expected_contexts);
        assert_eq!(
            registered_actions
                .intersection(&schema_actions)
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            schema_actions
        );
        let non_base_actions = default_only_actions
            .iter()
            .copied()
            .chain(A81_RENDERER_EXTENSION_ACTIONS)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            registered_actions
                .difference(&schema_actions)
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            non_base_actions,
            "the fixed defaults/schema inconsistency is tracked as a separate, exact set"
        );

        let implemented_actual = registrations
            .iter()
            .filter(|registration| {
                schema_actions.contains(registration.action_name)
                    && !matches!(registration.action, TuiActionId::UnavailableHistorical(_))
            })
            .map(|registration| registration.action_name)
            .collect::<std::collections::BTreeSet<_>>();
        let unavailable_actual = registrations
            .iter()
            .filter_map(|registration| match registration.action {
                TuiActionId::UnavailableHistorical(name)
                    if schema_actions.contains(registration.action_name) =>
                {
                    assert_eq!(
                        name, registration.action_name,
                        "unavailable action must carry its exact fixed name"
                    );
                    Some(name)
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(implemented_actual, implemented_expected);
        assert!(
            A81_RENDERER_EXTENSION_ACTIONS
                .into_iter()
                .all(|action| registrations.iter().any(|registration| {
                    registration.action_name == action
                        && !matches!(registration.action, TuiActionId::UnavailableHistorical(_))
                })),
            "the a81 GoalConsole renderer delta is implemented but remains outside the 235 fixed schema"
        );
        assert_eq!(unavailable_actual.len(), 41);
        assert_eq!(
            implemented_actual
                .union(&unavailable_actual)
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            schema_actions,
            "all 86 schema actions are either exact or explicit fail-closed"
        );
        assert!(
            implemented_actual.is_disjoint(&unavailable_actual),
            "no fixed action may claim both exact and unavailable semantics"
        );

        let default_only_exact = registrations
            .iter()
            .filter(|registration| default_only_actions.contains(registration.action_name))
            .filter(|registration| {
                !matches!(registration.action, TuiActionId::UnavailableHistorical(_))
            })
            .map(|registration| registration.action_name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            default_only_exact,
            [
                "scroll:pageUp",
                "scroll:pageDown",
                "scroll:lineUp",
                "scroll:lineDown",
                "scroll:top",
                "scroll:bottom",
                "selection:copy",
            ]
            .into_iter()
            .collect()
        );

        for (index, registration) in registrations.iter().enumerate() {
            assert!(
                registrations[..index].iter().all(|earlier| {
                    earlier.action_name != registration.action_name
                        || earlier.context != registration.context
                }),
                "duplicate fixed action/context pair: {}@{}",
                registration.action_name,
                registration.context.as_str(),
            );
        }
    }

    #[test]
    fn voice_action_stays_unbound_when_the_fixed_product_feature_is_disabled() {
        let registration = crabcode_keybinding_registrations(false)
            .into_iter()
            .find(|registration| registration.action_name == "voice:pushToTalk")
            .expect("fixed historical voice action remains explicitly classified");

        assert_eq!(
            registration.action,
            TuiActionId::UnavailableHistorical("voice:pushToTalk")
        );
        assert!(
            registration.default_chords.is_empty(),
            "VOICE_MODE is false in both fixed CrabCode sources and the pure-TUI build manifest"
        );
    }

    #[test]
    fn historical_dialog_history_and_attachment_bindings_are_context_exact() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for (context, event, expected) in [
            (
                CrabcodeKeybindingContext::Confirmation,
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                TuiActionId::ConfirmYes,
            ),
            (
                CrabcodeKeybindingContext::Confirmation,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                TuiActionId::ConfirmNo,
            ),
            (
                CrabcodeKeybindingContext::Confirmation,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                TuiActionId::ConfirmNextField,
            ),
            (
                CrabcodeKeybindingContext::Settings,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                TuiActionId::ConfirmNo,
            ),
            (
                CrabcodeKeybindingContext::Attachments,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
                TuiActionId::AttachmentNext,
            ),
            (
                CrabcodeKeybindingContext::Attachments,
                KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
                TuiActionId::AttachmentRemove,
            ),
        ] {
            assert_eq!(
                registry.lookup_keybinding(&event, &[context]),
                CrabcodeKeybindingResolution::Match(expected)
            );
        }

        assert_eq!(
            registry.lookup_keybinding(
                &KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                &[CrabcodeKeybindingContext::Settings],
            ),
            CrabcodeKeybindingResolution::None,
            "Settings is the historical escape-only text-entry context"
        );
        assert_eq!(
            registry.lookup_keybinding(
                &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                &[
                    CrabcodeKeybindingContext::Chat,
                    CrabcodeKeybindingContext::Autocomplete,
                ],
            ),
            CrabcodeKeybindingResolution::Match(TuiActionId::AutocompletePrevious),
            "an open autocomplete menu must keep priority over composer history"
        );
    }

    #[test]
    fn historical_external_editor_shortcut_is_prompt_context_only() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(
            registry.lookup(&ctrl_g, InteractionContext::Prompt),
            Some(TuiActionId::OpenExternalEditor)
        );
        assert_eq!(
            registry.lookup(&ctrl_g, InteractionContext::Scrollback),
            None
        );
        assert_eq!(registry.lookup(&ctrl_g, InteractionContext::Global), None);
    }

    #[test]
    fn historical_crabcode_prompt_and_transcript_keys_are_context_exact() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for (event, action) in [
            (
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                TuiActionId::CancelPrompt,
            ),
            (
                KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
                TuiActionId::UndoPrompt,
            ),
            (
                KeyEvent::new(
                    KeyCode::Char('-'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                TuiActionId::UndoPrompt,
            ),
            (
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                TuiActionId::StashPrompt,
            ),
        ] {
            assert_eq!(
                registry.lookup(&event, InteractionContext::Prompt),
                Some(action)
            );
            assert_eq!(
                registry.lookup(&event, InteractionContext::Scrollback),
                (action == TuiActionId::CancelPrompt).then_some(TuiActionId::ExitTranscript)
            );
        }

        for event in [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                registry.lookup(&event, InteractionContext::Scrollback),
                Some(TuiActionId::ExitTranscript)
            );
        }

        let toggle = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(
            registry.lookup(&toggle, InteractionContext::Global),
            Some(TuiActionId::ToggleTranscript)
        );
        assert_eq!(registry.lookup(&toggle, InteractionContext::Prompt), None);
    }

    #[test]
    fn historical_crabcode_scroll_boundaries_are_preserved_as_alternates() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for (event, expected) in [
            (
                KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
                TuiActionId::ScrollTop,
            ),
            (
                KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
                TuiActionId::ScrollBottom,
            ),
        ] {
            assert_eq!(
                registry.lookup(&event, InteractionContext::Scrollback),
                Some(expected)
            );
            assert_eq!(registry.lookup(&event, InteractionContext::Prompt), None);
        }
    }

    #[test]
    fn shortcuts_help_keeps_both_ctrl_dot_and_ctrl_x_routes() {
        let registry = TuiActionRegistry::for_screen_mode(false);
        for key in [
            KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                registry.lookup(&key, InteractionContext::Global),
                Some(TuiActionId::OpenHelp)
            );
        }
    }
}
