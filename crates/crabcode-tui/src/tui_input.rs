//! Terminal input normalization kept outside backend/session semantics.
//!
//! The modifier rescue lifecycle is a direct, neutralized port of the fixed
//! upstream Rust TUI keyboard normalizer. Timed paste recovery is owned by the
//! fixed terminal event loop, beside the reader/drain lifecycle it depends on.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::terminal_capabilities::{ModifierDelivery, terminal_context};
use crate::tui_render::{KeyShortcut, is_shift_tab};

#[cfg(target_os = "macos")]
static PHYSICAL_MODIFIER_PROBE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

#[cfg(any(target_os = "macos", test))]
struct PhysicalModifierRequest {
    response: std::sync::mpsc::Sender<ModifierState>,
}

#[cfg(all(target_os = "macos", not(test)))]
static PHYSICAL_MODIFIER_WORKER: std::sync::OnceLock<
    Option<std::sync::mpsc::SyncSender<PhysicalModifierRequest>>,
> = std::sync::OnceLock::new();

/// Snapshot of physically-held modifier keys at a single point in time.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ModifierState {
    pub command: bool,
    pub option: bool,
    pub shift: bool,
    pub control: bool,
}

/// OS-level probe of physical modifier state. One snapshot per call.
pub trait ModifierProbe {
    fn snapshot(&self) -> ModifierState;
}

/// Production probe: macOS reads CoreGraphics, other OSes return all-false.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsModifierProbe;

impl ModifierProbe for OsModifierProbe {
    fn snapshot(&self) -> ModifierState {
        physical_modifier_snapshot().unwrap_or_default()
    }
}

/// Upgrade incoming deletion keys with modifiers dropped by the terminal.
///
/// This is the fixed upstream `KeyboardNormalizer` lifecycle with only module
/// paths and product-neutral names adapted.
#[derive(Debug, Clone, Copy)]
pub struct KeyboardNormalizer<P: ModifierProbe = OsModifierProbe> {
    probe: P,
    delivery: ModifierDelivery,
}

impl<P: ModifierProbe> KeyboardNormalizer<P> {
    #[cfg(test)]
    pub(crate) fn new(probe: P, delivery: ModifierDelivery) -> Self {
        Self { probe, delivery }
    }

    /// Upgrade a [`KeyEvent`] when a modifier is held but absent from the
    /// event. Returns `Some` only if the delivered event changed.
    pub fn rescue_key(&self, key: KeyEvent) -> Option<KeyEvent> {
        if key.code == KeyCode::Char('\u{0002}') && key.modifiers.is_empty() {
            let mut out = key;
            out.code = KeyCode::Char('b');
            out.modifiers = KeyModifiers::CONTROL;
            return Some(out);
        }
        if !self.delivery.benefits_from_rescue() {
            return None;
        }
        if !key.modifiers.is_empty() {
            return None;
        }
        if !matches!(key.code, KeyCode::Backspace | KeyCode::Delete) {
            return None;
        }
        let state = self.probe.snapshot();
        // Cmd wins per macOS convention: Cmd+Backspace (line-kill) is the
        // stronger action; almost no one holds Cmd+Opt simultaneously.
        let added = match (
            state.command && self.delivery.cmd.benefits_from_rescue(),
            state.option && self.delivery.opt.benefits_from_rescue(),
        ) {
            (true, _) => KeyModifiers::SUPER,
            (false, true) => KeyModifiers::ALT,
            _ => return None,
        };
        tracing::debug!(
            key.code = ?key.code,
            added.modifier = ?added,
            "key event rescued via OS modifier probe"
        );
        let mut out = key;
        out.modifiers |= added;
        Some(out)
    }

    /// Upgrade an [`Event`] in place, owning a fresh `Event::Key` only
    /// when a rescue actually fires.
    pub fn rescue<'a>(&self, ev: &'a Event) -> std::borrow::Cow<'a, Event> {
        if let Event::Key(k) = ev
            && let Some(upgraded) = self.rescue_key(*k)
        {
            return std::borrow::Cow::Owned(Event::Key(upgraded));
        }
        std::borrow::Cow::Borrowed(ev)
    }
}

impl KeyboardNormalizer<OsModifierProbe> {
    pub fn from_terminal_context() -> Self {
        Self {
            probe: OsModifierProbe,
            delivery: terminal_context().keyboard_capabilities().modifier_delivery,
        }
    }
}

/// Canonicalize terminal-specific Shift-Tab encodings, run the fixed upstream
/// deletion-key normalizer, then preserve CrabCode's established composer
/// call shape for terminals that drop modified-Enter flags.
pub fn normalize_key_event(mut event: KeyEvent) -> KeyEvent {
    if is_shift_tab(&event) {
        event.code = KeyCode::BackTab;
        event.modifiers = KeyModifiers::NONE;
        return event;
    }
    event = KeyboardNormalizer::from_terminal_context()
        .rescue_key(event)
        .unwrap_or(event);
    rescue_modified_enter(
        event,
        terminal_context()
            .keyboard_capabilities()
            .enter_needs_rescue(),
        OsModifierProbe,
    )
}

/// Permanently retire the macOS physical-modifier side channel before a
/// foreground job-control transition.
///
/// A production PTY sample established that `CGEventSourceFlagsState` can
/// block indefinitely inside SkyLight. Job control is one proven trigger, so
/// the side channel is retired before handing the terminal to a child. The
/// flag is deliberately process-wide and one-way: a later terminal generation
/// cannot prove that the process-global CoreGraphics connection is usable
/// again.
pub(crate) fn disable_physical_modifier_probe_for_job_control() {
    #[cfg(target_os = "macos")]
    PHYSICAL_MODIFIER_PROBE_ENABLED.store(false, std::sync::atomic::Ordering::Release);
}

/// Whether a macOS CoreGraphics modifier snapshot is semantically safe.
///
/// Local physical keyboard state is not evidence about keys pressed in an SSH
/// client, and a foreground job-control transition can leave the synchronous
/// CoreGraphics query permanently blocked. Both cases therefore disable every
/// physical-probe consumer: link hover, deletion rescue, and modified Enter.
#[cfg(target_os = "macos")]
pub(crate) fn physical_modifier_probe_available() -> bool {
    !terminal_context().is_ssh
        && PHYSICAL_MODIFIER_PROBE_ENABLED.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(any(target_os = "macos", test))]
fn delivered_super_for_key(key: &KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release
        && matches!(
            key.code,
            KeyCode::Modifier(
                crossterm::event::ModifierKeyCode::LeftSuper
                    | crossterm::event::ModifierKeyCode::RightSuper
            )
        )
    {
        false
    } else {
        key.modifiers.contains(KeyModifiers::SUPER)
    }
}

#[cfg(any(target_os = "macos", test))]
fn physical_or_delivered_link_modifier(
    delivered: bool,
    physical_probe_available: bool,
    physical_probe: impl FnOnce() -> Option<ModifierState>,
) -> bool {
    if physical_probe_available {
        physical_probe().map_or(delivered, |state| state.command)
    } else {
        delivered
    }
}

/// Check the fixed platform link-action modifier for a mouse event.
///
/// macOS mouse reports do not reliably carry Command, so the physical
/// CoreGraphics snapshot is authoritative only while its process-global
/// connection is known usable. SSH and post-job-control generations fall back
/// to the delivered Super bit. Linux and Windows use delivered Control.
#[cfg(target_os = "macos")]
pub(crate) fn link_modifier_held(mouse_modifiers: KeyModifiers) -> bool {
    physical_or_delivered_link_modifier(
        mouse_modifiers.contains(KeyModifiers::SUPER),
        physical_modifier_probe_available(),
        physical_modifier_snapshot,
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn link_modifier_held(mouse_modifiers: KeyModifiers) -> bool {
    mouse_modifiers.contains(KeyModifiers::CONTROL)
}

/// Determine the fixed link-action modifier state while processing a key.
///
/// Control release events can retain the Control bit in their delivered
/// modifier set. The same applies to delivered Super after the macOS physical
/// probe is retired, so physical modifier-key release explicitly clears hover.
#[cfg(target_os = "macos")]
pub(crate) fn link_modifier_for_key(key: &KeyEvent) -> bool {
    physical_or_delivered_link_modifier(
        delivered_super_for_key(key),
        physical_modifier_probe_available(),
        physical_modifier_snapshot,
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn link_modifier_for_key(key: &KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release
        && matches!(
            key.code,
            KeyCode::Modifier(
                crossterm::event::ModifierKeyCode::LeftControl
                    | crossterm::event::ModifierKeyCode::RightControl
            )
        )
    {
        false
    } else {
        key.modifiers.contains(KeyModifiers::CONTROL)
    }
}

fn rescue_modified_enter<P: ModifierProbe>(
    mut event: KeyEvent,
    enter_needs_rescue: bool,
    probe: P,
) -> KeyEvent {
    if !enter_needs_rescue
        || event.kind == KeyEventKind::Release
        || event.code != KeyCode::Enter
        || !event.modifiers.is_empty()
    {
        return event;
    }
    let physical = probe.snapshot();
    if physical.shift || physical.option || physical.command {
        event.modifiers = KeyModifiers::SHIFT;
    }
    event
}

/// Native clipboard-paste chords that must be intercepted before the text
/// editor. Matching is exact so AltGr and inline-paste variants remain
/// distinct interactions.
pub fn is_clipboard_paste_key(event: &KeyEvent) -> bool {
    if KeyShortcut::new(KeyCode::Char('v'), KeyModifiers::CONTROL).matches(event)
        || KeyShortcut::new(KeyCode::Char('v'), KeyModifiers::SUPER).matches(event)
    {
        return true;
    }
    #[cfg(target_os = "windows")]
    if KeyShortcut::new(KeyCode::Char('v'), KeyModifiers::ALT).matches(event) {
        return true;
    }
    false
}

/// Cut a UTF-8 string to a byte budget without retaining a partial scalar.
pub fn utf8_prefix_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(any(target_os = "macos", test))]
fn bounded_modifier_snapshot(
    enabled: &std::sync::atomic::AtomicBool,
    sender: &std::sync::mpsc::SyncSender<PhysicalModifierRequest>,
    budget: std::time::Duration,
) -> Option<ModifierState> {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{RecvTimeoutError, TrySendError};

    if !enabled.load(Ordering::Acquire) {
        return None;
    }
    let (response, receiver) = std::sync::mpsc::channel();
    match sender.try_send(PhysicalModifierRequest { response }) {
        Ok(()) => {}
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            enabled.store(false, Ordering::Release);
            return None;
        }
    }
    match receiver.recv_timeout(budget) {
        Ok(state) if enabled.load(Ordering::Acquire) => Some(state),
        Ok(_) => None,
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            enabled.store(false, Ordering::Release);
            None
        }
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn initialize_physical_modifier_worker()
-> Option<&'static std::sync::mpsc::SyncSender<PhysicalModifierRequest>> {
    PHYSICAL_MODIFIER_WORKER
        .get_or_init(|| {
            let (sender, receiver) = std::sync::mpsc::sync_channel::<PhysicalModifierRequest>(1);
            let worker = std::thread::Builder::new()
                .name("crabcode-modifier-probe".to_string())
                .spawn(move || {
                    while let Ok(request) = receiver.recv() {
                        if !PHYSICAL_MODIFIER_PROBE_ENABLED
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            break;
                        }
                        #[cfg(feature = "terminal-lifecycle-tests")]
                        if std::env::var_os("CRABCODE_TUI_TEST_ONLY_BLOCK_PHYSICAL_MODIFIER_PROBE")
                            .is_some()
                        {
                            loop {
                                std::thread::park();
                            }
                        }
                        let state = query_physical_modifier_state();
                        if request.response.send(state).is_err()
                            && !PHYSICAL_MODIFIER_PROBE_ENABLED
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            break;
                        }
                    }
                });
            match worker {
                Ok(_detached) => Some(sender),
                Err(error) => {
                    PHYSICAL_MODIFIER_PROBE_ENABLED
                        .store(false, std::sync::atomic::Ordering::Release);
                    tracing::warn!(
                        %error,
                        "physical modifier worker unavailable; using delivered terminal modifiers"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Create the sole macOS modifier worker before entering the interactive input
/// loop. The worker remains parked until a physical snapshot is requested, so
/// thread creation can never become part of a key or mouse event's hot path.
pub(crate) fn prepare_physical_modifier_probe() {
    #[cfg(all(target_os = "macos", not(test)))]
    if physical_modifier_probe_available() {
        let _ = initialize_physical_modifier_worker();
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn physical_modifier_snapshot() -> Option<ModifierState> {
    if !physical_modifier_probe_available() {
        return None;
    }
    let sender = PHYSICAL_MODIFIER_WORKER.get().and_then(Option::as_ref)?;
    let snapshot = bounded_modifier_snapshot(
        &PHYSICAL_MODIFIER_PROBE_ENABLED,
        sender,
        crate::app_event_loop::EVENT_LOOP_CADENCE,
    );
    if snapshot.is_none() {
        tracing::warn!(
            budget_ms = crate::app_event_loop::EVENT_LOOP_CADENCE.as_millis(),
            "physical modifier side channel was unavailable within the TUI event budget; retiring it"
        );
    }
    snapshot
}

#[cfg(all(target_os = "macos", not(test)))]
#[allow(unsafe_code)]
fn query_physical_modifier_state() -> ModifierState {
    const CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;
    const CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
    const CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
    const CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
    const CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    // SAFETY: stable CoreGraphics API with an integer-only signature. It
    // reads global modifier flags and does not dereference application memory.
    // This call is isolated on the single physical-modifier worker because
    // production samples prove that SkyLight can block it indefinitely.
    let flags = unsafe { CGEventSourceFlagsState(CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE) };
    ModifierState {
        command: flags & CG_EVENT_FLAG_MASK_COMMAND != 0,
        option: flags & CG_EVENT_FLAG_MASK_ALTERNATE != 0,
        shift: flags & CG_EVENT_FLAG_MASK_SHIFT != 0,
        control: flags & CG_EVENT_FLAG_MASK_CONTROL != 0,
    }
}

#[cfg(any(not(target_os = "macos"), test))]
const fn physical_modifier_snapshot() -> Option<ModifierState> {
    Some(ModifierState {
        command: false,
        option: false,
        shift: false,
        control: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, Clone, Copy)]
    struct MockProbe(ModifierState);

    impl ModifierProbe for MockProbe {
        fn snapshot(&self) -> ModifierState {
            self.0
        }
    }

    #[derive(Debug, Default, Clone, Copy)]
    struct PanicProbe;

    impl ModifierProbe for PanicProbe {
        fn snapshot(&self) -> ModifierState {
            panic!("modifier probe must remain gated")
        }
    }

    fn delivery(
        command: crate::terminal_capabilities::ModifierFate,
        option: crate::terminal_capabilities::ModifierFate,
    ) -> ModifierDelivery {
        ModifierDelivery::new_for_test(command, option)
    }

    fn normalizer(
        state: ModifierState,
        delivery: ModifierDelivery,
    ) -> KeyboardNormalizer<MockProbe> {
        KeyboardNormalizer::new(MockProbe(state), delivery)
    }

    #[test]
    fn input_shift_tab_encodings() {
        for event in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let normalized = normalize_key_event(event);
            assert_eq!(normalized.code, KeyCode::BackTab);
            assert_eq!(normalized.modifiers, KeyModifiers::NONE);
        }
    }

    #[test]
    fn clipboard_paste_shortcuts_are_exact() {
        assert!(is_clipboard_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
        assert!(is_clipboard_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::SUPER
        )));
        assert!(!is_clipboard_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_clipboard_paste_key(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        assert_eq!(
            is_clipboard_paste_key(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)),
            cfg!(target_os = "windows")
        );
    }

    #[test]
    fn input_editor_paste_byte_limit_utf8() {
        assert_eq!(utf8_prefix_within("中x", 3), "中");
        assert_eq!(utf8_prefix_within("中x", 2), "");
        assert_eq!(utf8_prefix_within("中x", 4), "中x");
    }

    #[test]
    fn fixed_keyboard_normalizer_canonicalizes_raw_ctrl_b_without_probe() {
        use crate::terminal_capabilities::ModifierFate;

        let normalizer = KeyboardNormalizer::new(
            PanicProbe,
            delivery(ModifierFate::Native, ModifierFate::Native),
        );
        let raw = KeyEvent::new(KeyCode::Char('\u{0002}'), KeyModifiers::NONE);
        let rescued = normalizer
            .rescue_key(raw)
            .expect("raw Ctrl+B must canonicalize");
        assert_eq!(rescued.code, KeyCode::Char('b'));
        assert_eq!(rescued.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn fixed_keyboard_normalizer_rescues_only_dropped_axes() {
        use crate::terminal_capabilities::ModifierFate;

        let drops_command = delivery(ModifierFate::Dropped, ModifierFate::Native);
        let rescued = normalizer(
            ModifierState {
                command: true,
                ..ModifierState::default()
            },
            drops_command,
        )
        .rescue_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .expect("dropped command modifier");
        assert_eq!(rescued.modifiers, KeyModifiers::SUPER);

        let already_modified = normalizer(
            ModifierState {
                command: true,
                ..ModifierState::default()
            },
            drops_command,
        )
        .rescue_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert!(already_modified.is_none());

        let native_command = normalizer(
            ModifierState {
                command: true,
                ..ModifierState::default()
            },
            delivery(ModifierFate::Native, ModifierFate::Native),
        );
        assert!(
            native_command
                .rescue_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .is_none()
        );
    }

    #[test]
    fn fixed_keyboard_normalizer_rescues_option_and_prefers_command() {
        use crate::terminal_capabilities::ModifierFate;

        let drops_both = delivery(ModifierFate::Dropped, ModifierFate::Dropped);
        let option_backspace = normalizer(
            ModifierState {
                option: true,
                ..ModifierState::default()
            },
            drops_both,
        )
        .rescue_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .expect("dropped option modifier");
        assert_eq!(option_backspace.modifiers, KeyModifiers::ALT);

        let both = normalizer(
            ModifierState {
                command: true,
                option: true,
                ..ModifierState::default()
            },
            drops_both,
        )
        .rescue_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
        .expect("dropped command and option modifiers");
        assert_eq!(both.modifiers, KeyModifiers::SUPER);
    }

    #[test]
    fn fixed_keyboard_normalizer_never_probes_non_deletion_keys() {
        use crate::terminal_capabilities::ModifierFate;

        let normalizer = KeyboardNormalizer::new(
            PanicProbe,
            delivery(ModifierFate::Dropped, ModifierFate::Dropped),
        );
        for code in [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Up,
        ] {
            assert!(
                normalizer
                    .rescue_key(KeyEvent::new(code, KeyModifiers::NONE))
                    .is_none(),
                "unexpected rescue for {code:?}"
            );
        }
    }

    #[test]
    fn retired_physical_link_probe_uses_only_delivered_super() {
        let plain = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!physical_or_delivered_link_modifier(
            delivered_super_for_key(&plain),
            false,
            || panic!("retired physical probe must never be called"),
        ));

        let command_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER);
        assert!(physical_or_delivered_link_modifier(
            delivered_super_for_key(&command_c),
            false,
            || panic!("retired physical probe must never be called"),
        ));

        let command_release = KeyEvent::new_with_kind(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftSuper),
            KeyModifiers::SUPER,
            KeyEventKind::Release,
        );
        assert!(!physical_or_delivered_link_modifier(
            delivered_super_for_key(&command_release),
            false,
            || panic!("release fallback must never call the retired probe"),
        ));
    }

    #[test]
    fn available_physical_link_probe_remains_authoritative() {
        assert!(physical_or_delivered_link_modifier(false, true, || {
            Some(ModifierState {
                command: true,
                ..ModifierState::default()
            })
        }));
        assert!(!physical_or_delivered_link_modifier(true, true, || {
            Some(ModifierState::default())
        }));
    }

    #[test]
    fn unavailable_physical_link_snapshot_falls_back_for_the_same_event() {
        assert!(physical_or_delivered_link_modifier(true, true, || None));
        assert!(!physical_or_delivered_link_modifier(false, true, || None));
    }

    #[test]
    fn bounded_modifier_worker_preserves_the_complete_snapshot() {
        let enabled = std::sync::atomic::AtomicBool::new(true);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let expected = ModifierState {
            command: true,
            option: true,
            shift: true,
            control: true,
        };
        let worker = std::thread::spawn(move || {
            let request: PhysicalModifierRequest = receiver.recv().expect("modifier request");
            request.response.send(expected).expect("modifier response");
        });
        assert_eq!(
            bounded_modifier_snapshot(&enabled, &sender, std::time::Duration::from_secs(1)),
            Some(expected)
        );
        worker.join().expect("modifier worker");
    }

    #[test]
    fn bounded_modifier_worker_timeout_is_one_way_and_sends_only_once() {
        let enabled = std::sync::atomic::AtomicBool::new(true);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (request_seen, request_seen_receiver) = std::sync::mpsc::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let request: PhysicalModifierRequest = receiver.recv().expect("modifier request");
            request_seen.send(()).expect("request observation");
            release_receiver.recv().expect("test release");
            let _ = request.response.send(ModifierState {
                command: true,
                ..ModifierState::default()
            });
            assert!(
                receiver.try_recv().is_err(),
                "a retired probe must not queue a second request"
            );
        });

        let budget = std::time::Duration::from_millis(16);
        let started = std::time::Instant::now();
        assert_eq!(bounded_modifier_snapshot(&enabled, &sender, budget), None);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= budget,
            "the timeout path returned before its explicit budget: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "the bounded wait exceeded its scheduling allowance: {elapsed:?}"
        );
        request_seen_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker saw exactly one request");
        assert!(!enabled.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            bounded_modifier_snapshot(&enabled, &sender, budget),
            None,
            "retirement must be process-lifetime one-way"
        );
        release.send(()).expect("release worker");
        worker.join().expect("modifier worker");
    }

    #[test]
    fn modified_enter_adapter_is_capability_gated() {
        let bare_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            rescue_modified_enter(bare_enter, false, PanicProbe),
            bare_enter,
            "native terminals must not query the OS modifier side channel"
        );
        assert_eq!(
            rescue_modified_enter(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                true,
                PanicProbe,
            ),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            "non-Enter keys must not query the OS modifier side channel"
        );

        let modified_enter = rescue_modified_enter(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            true,
            MockProbe(ModifierState {
                shift: true,
                ..ModifierState::default()
            }),
        );
        assert_eq!(modified_enter.modifiers, KeyModifiers::SHIFT);
    }
}
