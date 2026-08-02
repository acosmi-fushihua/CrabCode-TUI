//! Fixed-renderer event-loop plumbing adapted to CrabCode's direct runtime.
//!
//! This module owns readiness order, input batching, terminal-event
//! normalization, and renderer deadlines. Model, tool, session, and transport
//! semantics remain in CrabCode's existing `RuntimeHost`/`ActionDispatcher`
//! boundary; there is deliberately no second task executor or protocol here.

use std::future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crabcode_pager_render::audited_theme::system_appearance::SystemAppearanceWatcher;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{Notify, mpsc};

use crate::terminal_input::TimedInputEvent;
use crate::terminal_writer::WriterEvent;
use crate::{CrabCodeDirectRuntimeAdapter, Presenter};

pub(crate) const EVENT_LOOP_CADENCE: Duration = Duration::from_millis(16);
pub(crate) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub(crate) enum Wake {
    Signal,
    Writer(Option<WriterEvent>),
    Outbound,
    Runtime,
    RuntimeStderr,
    TerminalInput(Option<TimedInputEvent>),
    Appearance,
    ResizeDebounce,
    DeferredDraw,
    SuspendRetry,
    PendingRelaunch,
    ScrollTick,
    AnimationTick,
}

/// Run the fixed Rust terminal lifecycle with CrabCode's direct runtime as the
/// sole product callback adapter.
///
/// Lifecycle-owned work stays here: pending terminal handoff, Presenter
/// transaction, fresh deadline derivation, biased readiness, whole-batch input
/// drain, resize/full-repaint classification, scroll/animation timers,
/// appearance observation, and the post-wake presentation. The adapter only
/// applies those callbacks to the existing `TuiApp` plus
/// `RuntimeHost`/`ActionDispatcher`.
pub(crate) async fn run_fixed_terminal_lifecycle(
    adapter: &mut CrabCodeDirectRuntimeAdapter<'_>,
    terminal: &mut crate::terminal::TerminalSession,
    terminal_input: &mut crate::terminal_input::TerminalEventSource,
    mut writer_events: mpsc::UnboundedReceiver<WriterEvent>,
) -> anyhow::Result<()> {
    let mut presenter = Presenter::new();
    let (outbound_ready, runtime_ready, runtime_stderr_ready) = adapter.direct_readiness();
    let terminal_signal_ready = terminal.signal_notifier();
    let mut resize_debounce_at: Option<Instant> = None;
    let mut csi_filter = CsiFragmentFilter::new();
    let mut xt_filter = XtversionFilter::new();
    adapter.configure_scroll_cadence();
    let mut appearance_watcher = adapter.initial_appearance_watcher();

    // The fixed lifecycle renders once before waiting for the first readiness
    // source, then presents subsequent dirty state only after a wake branch.
    presenter.request(false);
    adapter.present_if_dirty(terminal, &mut presenter)?;

    while !adapter.should_quit() {
        // A native terminal signal has priority over starting a child handoff.
        // Unix wakes this loop through `Wake::Signal`; Windows' callback is
        // atomic-only and is observed by the low-frequency animation watchdog.
        adapter.service_terminal_signals(
            terminal,
            terminal_input,
            &mut presenter,
            &mut resize_debounce_at,
            || {
                csi_filter.retire_terminal_generation();
                xt_filter.retire_terminal_generation();
            },
        )?;
        adapter.inspect_terminal_liveness(terminal)?;
        if adapter.should_quit() {
            break;
        }

        adapter.materialize_pending_terminal_handoff();
        if adapter.run_pending_terminal_handoff(terminal, terminal_input, &mut presenter)? {
            resize_debounce_at = None;
            adapter.present_if_dirty(terminal, &mut presenter)?;
            continue;
        }
        adapter.service_terminal_requests(terminal, &mut presenter, &mut resize_debounce_at)?;
        // Product callbacks serviced at loop top (for example a detached link
        // opener result) may dirty renderer-local status without another
        // readiness source. Flush that request before waiting, matching the
        // upstream loop-top callbacks that use immediate presentation.
        adapter.present_if_dirty(terminal, &mut presenter)?;

        // Derive every terminal-local deadline from current state on every
        // iteration. No wake arm owns a stale scroll, resize, appearance, or
        // animation schedule.
        let now = Instant::now();
        let deferred_input_tick_at = if adapter.has_deferred_input_work() {
            Some(now + EVENT_LOOP_CADENCE)
        } else {
            None
        };
        let windows_signal_watchdog_at = if terminal_signal_ready.is_none() {
            Some(now + Duration::from_millis(100))
        } else {
            None
        };
        let animation_tick_at = [
            deferred_input_tick_at,
            windows_signal_watchdog_at,
            adapter.renderer_animation_deadline(),
            terminal.renderer_animation_deadline(now),
        ]
        .into_iter()
        .flatten()
        .min();
        let scroll_tick_at = adapter.scroll_tick_at(now);
        let suspend_retry_at = adapter.suspend_retry_at();
        let pending_relaunch_at = adapter.pending_relaunch_deadline();
        let outbound_deadline = adapter.outbound_deadline();

        let wake = {
            let input_rx = terminal_input
                .receiver_mut()
                .context("failed to borrow the terminal input receiver")?;
            select_fixed_wake(
                terminal_signal_ready.as_ref(),
                &mut writer_events,
                &outbound_ready,
                &runtime_ready,
                &runtime_stderr_ready,
                input_rx,
                appearance_watcher.as_mut(),
                outbound_deadline,
                resize_debounce_at,
                presenter.draw_scheduled_at,
                suspend_retry_at,
                pending_relaunch_at,
                scroll_tick_at,
                animation_tick_at,
            )
            .await
        };

        match wake {
            Wake::Writer(Some(writer_event)) => {
                let sequence =
                    crate::writer_event_sequence(writer_event).context("terminal output failed")?;
                presenter.acknowledge(sequence);
            }
            Wake::Writer(None) => anyhow::bail!("terminal writer stopped"),
            Wake::Outbound => {
                adapter.drive_outbound(&mut presenter, &mut resize_debounce_at);
            }
            Wake::Runtime => {
                adapter.drain_direct_runtime(
                    terminal_input,
                    &runtime_ready,
                    &mut presenter,
                    &mut resize_debounce_at,
                    &mut appearance_watcher,
                );
            }
            Wake::RuntimeStderr => {
                adapter.drain_direct_runtime_stderr(
                    &runtime_stderr_ready,
                    &mut presenter,
                    &mut resize_debounce_at,
                );
            }
            Wake::TerminalInput(Some(first)) => {
                // External SIGSTOP can suspend this task while both the
                // signal-safe resume flag and an old queued input event are
                // becoming ready. Re-check the signal owner after selection
                // and discard the selected event if a generation cutover won
                // that race.
                let terminal_generation_changed = adapter.service_terminal_signals(
                    terminal,
                    terminal_input,
                    &mut presenter,
                    &mut resize_debounce_at,
                    || {
                        csi_filter.retire_terminal_generation();
                        xt_filter.retire_terminal_generation();
                    },
                )?;
                if terminal_generation_changed {
                    adapter.inspect_terminal_liveness(terminal)?;
                    if adapter.should_quit() {
                        break;
                    }
                    adapter.present_if_dirty(terminal, &mut presenter)?;
                    continue;
                }
                if adapter.should_quit() {
                    break;
                }
                let result = {
                    let input_rx = terminal_input
                        .receiver_mut()
                        .context("failed to drain the terminal input receiver")?;
                    drain_and_process(first, input_rx, &mut csi_filter, &mut xt_filter, |routed| {
                        adapter.handle_terminal_event(terminal, routed)
                    })
                    .await?
                };
                if result.should_quit {
                    break;
                }
                apply_input_presentation(
                    result,
                    Instant::now(),
                    &mut resize_debounce_at,
                    &mut presenter,
                );
                adapter.synchronize_appearance_watcher(&mut appearance_watcher);
            }
            Wake::TerminalInput(None) => {
                return Err(terminal_input.failure().unwrap_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "CrabCode terminal input reader exited",
                    )
                }))
                .context("terminal input reader stopped");
            }
            Wake::ResizeDebounce => {
                resize_debounce_at = None;
                presenter.request(false);
            }
            Wake::DeferredDraw => {
                presenter.draw_scheduled_at = None;
                presenter.request(false);
            }
            Wake::SuspendRetry => adapter.release_suspend_retry(),
            Wake::PendingRelaunch => {
                if adapter.poll_pending_relaunch(Instant::now()) {
                    presenter.request(false);
                }
            }
            Wake::ScrollTick => {
                if adapter.tick_scroll() {
                    resize_debounce_at = None;
                    presenter.request(false);
                }
            }
            Wake::AnimationTick => {
                let now = Instant::now();
                let renderer_progressed = terminal.tick_renderer_animation(now);
                if renderer_progressed || adapter.tick_renderer_animation() {
                    resize_debounce_at = None;
                    presenter.request(false);
                }
            }
            Wake::Appearance => {
                if adapter.apply_system_appearance(appearance_watcher.as_ref())? {
                    resize_debounce_at = None;
                    presenter.request(false);
                }
            }
            // The signal bridge owns no application action. Its wake only
            // transfers control to the signal drain at the next loop top.
            Wake::Signal => {}
        }

        adapter.present_if_dirty(terminal, &mut presenter)?;
    }

    Ok(())
}

fn apply_input_presentation(
    result: DrainResult,
    now: Instant,
    resize_debounce_at: &mut Option<Instant>,
    presenter: &mut Presenter,
) {
    if !result.needs_draw {
        return;
    }
    if result.force_repaint {
        *resize_debounce_at = None;
        presenter.request(true);
    } else if result.resize_only {
        *resize_debounce_at = Some(now + RESIZE_DEBOUNCE);
    } else {
        *resize_debounce_at = None;
        presenter.request(false);
    }
}

/// Select one readiness source in the fixed renderer priority order.
///
/// Selecting directly on the dedicated reader's Tokio channel is
/// cancellation-safe: dropping a losing `recv()` future cannot consume its
/// event. Every direct-runtime callback remains gated while terminal input is
/// buffered, so continuously-ready outbound, event, or stderr work cannot
/// starve wheel or keyboard input.
#[allow(clippy::too_many_arguments)]
async fn select_fixed_wake(
    signal: Option<&Arc<Notify>>,
    writer: &mut mpsc::UnboundedReceiver<WriterEvent>,
    outbound: &Arc<Notify>,
    runtime: &Arc<Notify>,
    runtime_stderr: &Arc<Notify>,
    input_rx: &mut mpsc::UnboundedReceiver<TimedInputEvent>,
    appearance_watcher: Option<&mut SystemAppearanceWatcher>,
    outbound_deadline: Option<Instant>,
    resize_debounce_at: Option<Instant>,
    deferred_draw_at: Option<Instant>,
    suspend_retry_at: Option<Instant>,
    pending_relaunch_at: Option<Instant>,
    scroll_tick_at: Option<Instant>,
    animation_tick_at: Option<Instant>,
) -> Wake {
    let input_is_empty = input_rx.is_empty();
    // A closed, drained input channel is itself a terminal lifecycle event.
    // Do not let continuously-ready runtime notifications starve the
    // `recv() -> None` arm, or the main loop can spin forever after the
    // dedicated reader exits.
    let input_is_closed = input_rx.is_closed();
    tokio::select! {
        biased;

        _ = signal_wait(signal) => Wake::Signal,
        writer_event = writer.recv() => Wake::Writer(writer_event),
        _ = outbound.notified(), if input_is_empty && !input_is_closed => Wake::Outbound,
        _ = deadline_wait(outbound_deadline), if input_is_empty && !input_is_closed => Wake::Outbound,
        _ = runtime.notified(), if input_is_empty && !input_is_closed => Wake::Runtime,
        _ = runtime_stderr.notified(), if input_is_empty && !input_is_closed => Wake::RuntimeStderr,
        terminal_event = input_rx.recv() => Wake::TerminalInput(terminal_event),
        _ = deadline_wait(resize_debounce_at) => Wake::ResizeDebounce,
        _ = deadline_wait(deferred_draw_at) => Wake::DeferredDraw,
        _ = deadline_wait(suspend_retry_at) => Wake::SuspendRetry,
        _ = deadline_wait(pending_relaunch_at) => Wake::PendingRelaunch,
        _ = deadline_wait(scroll_tick_at) => Wake::ScrollTick,
        _ = deadline_wait(animation_tick_at) => Wake::AnimationTick,
        _ = appearance_wait(appearance_watcher) => Wake::Appearance,
    }
}

async fn appearance_wait(watcher: Option<&mut SystemAppearanceWatcher>) {
    match watcher {
        Some(watcher) => {
            if watcher.changed().await.is_err() {
                // A dead polling task must not turn this readiness arm into an
                // immediate-error hot loop. Keep the arm inert for the
                // remainder of the terminal lifecycle.
                future::pending().await
            }
        }
        None => future::pending().await,
    }
}

async fn signal_wait(signal: Option<&Arc<Notify>>) {
    match signal {
        Some(signal) => signal.notified().await,
        None => future::pending().await,
    }
}

async fn deadline_wait(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => future::pending().await,
    }
}

/// Result of draining and processing one terminal-input batch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DrainResult {
    pub(crate) needs_draw: bool,
    pub(crate) should_quit: bool,
    pub(crate) resize_only: bool,
    pub(crate) force_repaint: bool,
}

/// Terminal event after renderer-local normalization.
///
/// The provenance is renderer-private input metadata. It never crosses the
/// direct-runtime transport and cannot add a backend or public protocol field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PasteProvenance {
    /// Terminal bracketed paste or pager key-coalesced paste.
    Terminal,
    /// Linux X11 PRIMARY read triggered by one unmodified middle-button down.
    #[allow(dead_code)]
    X11Primary,
}
impl PasteProvenance {
    pub(crate) fn may_probe_clipboard_attachments(self) -> bool {
        matches!(self, Self::Terminal)
    }
}
/// Terminal event after renderer-local normalization.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RoutedInputEvent {
    pub(crate) event: Event,
    pub(crate) arrived_at: Instant,
    pub(crate) paste_provenance: PasteProvenance,
}

/// Narrow result returned by CrabCode's application-event adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HandledInput {
    pub(crate) needs_draw: bool,
    pub(crate) force_repaint: bool,
    pub(crate) stop_batch: bool,
    pub(crate) should_quit: bool,
}

/// Preserve the reader timestamp while adapting to CrabCode's event API.
fn normalize_input_event(timed: TimedInputEvent) -> RoutedInputEvent {
    let TimedInputEvent { event, arrived_at } = timed;
    #[cfg(target_os = "linux")]
    {
        use crossterm::event::{MouseButton, MouseEventKind};
        let is_unmodified_middle_down = match &event {
            Event::Mouse(mouse) => {
                mouse.kind == MouseEventKind::Down(MouseButton::Middle)
                    && mouse.modifiers.is_empty()
            }
            _ => false,
        };
        if is_unmodified_middle_down
            && let Some(text) = crate::tui_clipboard::system_primary_selection_get()
        {
            return RoutedInputEvent {
                event: Event::Paste(text),
                arrived_at,
                paste_provenance: PasteProvenance::X11Primary,
            };
        }
    }
    RoutedInputEvent {
        event,
        arrived_at,
        paste_provenance: PasteProvenance::Terminal,
    }
}

/// Process the first terminal event and every event already buffered behind it.
///
/// The collection/coalescing/filtering order is the fixed renderer order. The
/// callback is the only CrabCode adapter: it routes one already-normalized
/// event through `TuiApp` and the existing direct-runtime dispatcher.
pub(crate) async fn drain_and_process(
    first: TimedInputEvent,
    input_rx: &mut mpsc::UnboundedReceiver<TimedInputEvent>,
    csi_filter: &mut CsiFragmentFilter,
    xt_filter: &mut XtversionFilter,
    mut handle_one: impl FnMut(RoutedInputEvent) -> anyhow::Result<HandledInput>,
) -> anyhow::Result<DrainResult> {
    let mut needs_draw = false;
    let mut had_resize = false;
    let mut had_non_resize_change = false;
    let mut force_repaint = false;

    let mut raw_events = vec![first];
    drain_immediate(&mut raw_events, input_rx);

    // XTVERSION reply removal must precede paste coalescing so reply chars
    // are never folded into a synthetic Paste.
    if xt_filter.armed() {
        raw_events = filter_xtversion_with_fragment_wait(xt_filter, raw_events, input_rx).await;
    }

    if should_extend_for_paste(&raw_events) && detect_paste(&mut raw_events, input_rx).await {
        collect_remaining_paste(&mut raw_events, input_rx).await;
        // Paste extension pulled more events off the channel without passing
        // them through the still-armed filter. A late or split XTVERSION reply
        // must be removed before it can be coalesced with prompt text.
        if xt_filter.armed() {
            raw_events = filter_xtversion_with_fragment_wait(xt_filter, raw_events, input_rx).await;
        }
    }

    let coalesced = coalesce_rapid_keys(raw_events);
    let coalesced = csi_filter.filter(coalesced);
    let coalesced = coalesced
        .into_iter()
        .map(normalize_input_event)
        .collect::<Vec<_>>();

    for routed in coalesced {
        let is_resize = matches!(routed.event, Event::Resize(_, _));
        let handled = handle_one(routed)?;
        if handled.needs_draw {
            needs_draw = true;
            if is_resize {
                had_resize = true;
            } else {
                had_non_resize_change = true;
            }
        }
        if handled.force_repaint {
            force_repaint = true;
            needs_draw = true;
        }
        if handled.should_quit {
            return Ok(DrainResult {
                needs_draw,
                should_quit: true,
                resize_only: false,
                force_repaint,
            });
        }
        // A TTY-taking child must run before later buffered events mutate UI.
        if handled.stop_batch {
            break;
        }
    }

    Ok(DrainResult {
        needs_draw,
        should_quit: false,
        resize_only: had_resize && !had_non_resize_change,
        force_repaint,
    })
}

// ── Paste coalescing for terminals without bracketed paste ───────────

/// Timeout for the first extension round (detection).  If no event
/// arrives within this window the batch was a normal keystroke.
const PASTE_DETECT_TIMEOUT: Duration = Duration::from_millis(2);

/// Timeout for subsequent rounds once paste has been detected.
const PASTE_CONTINUE_TIMEOUT: Duration = Duration::from_millis(10);

/// Safety cap on events accumulated in one extension pass.
const PASTE_EXTEND_MAX_EVENTS: usize = 5_000;

/// Returns `true` when the batch contains pasteable key events but no
/// `Event::Paste` (i.e. bracketed paste is not handling it).
fn should_extend_for_paste(events: &[TimedInputEvent]) -> bool {
    !events.iter().any(|e| matches!(e.event, Event::Paste(_)))
        && events.iter().any(|e| is_pasteable_key_event(&e.event))
}

/// Wait [`PASTE_DETECT_TIMEOUT`] for a follow-up event.  Returns `true`
/// if a **pasteable key event** arrives within the window.  Non-key events
/// (mouse, focus, releases) are collected but do not count as paste evidence.
async fn detect_paste(
    batch: &mut Vec<TimedInputEvent>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TimedInputEvent>,
) -> bool {
    match tokio::time::timeout(PASTE_DETECT_TIMEOUT, input_rx.recv()).await {
        Ok(Some(ev)) => {
            let prev_len = batch.len();
            batch.push(ev);
            drain_immediate(batch, input_rx);
            batch[prev_len..]
                .iter()
                .any(|e| is_pasteable_key_event(&e.event))
        }
        _ => false,
    }
}

/// Collect remaining paste events using [`PASTE_CONTINUE_TIMEOUT`].
/// Only pasteable key events extend the timeout; non-key events are
/// collected but do not keep the loop alive.
async fn collect_remaining_paste(
    batch: &mut Vec<TimedInputEvent>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TimedInputEvent>,
) {
    let mut extended = 0usize;
    loop {
        if extended >= PASTE_EXTEND_MAX_EVENTS {
            break;
        }
        match tokio::time::timeout(PASTE_CONTINUE_TIMEOUT, input_rx.recv()).await {
            Ok(Some(ev)) => {
                let prev_len = batch.len();
                batch.push(ev);
                extended += 1;
                drain_immediate(batch, input_rx);
                if !batch[prev_len..]
                    .iter()
                    .any(|e| is_pasteable_key_event(&e.event))
                {
                    continue;
                }
            }
            _ => break,
        }
    }
}

/// Non-blocking drain of all immediately available events.
pub(super) fn drain_immediate(
    batch: &mut Vec<TimedInputEvent>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TimedInputEvent>,
) {
    while let Ok(ev) = input_rx.try_recv() {
        batch.push(ev);
    }
}

/// Minimum key events in a run to trigger paste coalescing.
const PASTE_COALESCE_THRESHOLD: usize = 3;

/// Minimum run length for the Windows path-shape coalesce branch.
/// Covers the shortest realistic dropped image path (`C:\x.png`,
/// `/a.png`) while leaving short typed prose alone.
#[cfg(target_os = "windows")]
const PATH_COALESCE_THRESHOLD: usize = 8;

/// Check if a terminal event is a pasteable key press — a character,
/// Enter, or Tab with no control modifiers (Ctrl/Alt/Super).
///
/// Only matches `Press` (not `Repeat` or `Release`). Repeat events come
/// from held keys, not paste; Release events carry no semantic content.
fn is_pasteable_key_event(ev: &Event) -> bool {
    match ev {
        Event::Key(ke) if ke.kind == KeyEventKind::Press => match ke.code {
            KeyCode::Char(_) => {
                ke.modifiers.is_empty()
                    || ke.modifiers == KeyModifiers::SHIFT
                    || crate::input::key::is_altgr(ke.modifiers)
            }
            KeyCode::Enter | KeyCode::Tab => ke.modifiers.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

/// The voice-capture chord: **Ctrl+Space** or **F8**. A press needs the exact
/// chord (matching the registry, so Shift+F8 / Ctrl+Alt+Space don't fire); a
/// release matches the key alone (Space/F8), since on Kitty the Ctrl release can
/// precede Space and drop the CONTROL bit. Callers gate release handling on an
/// owning hold session, so a stray bare release is a no-op.
fn is_voice_chord(ke: &KeyEvent) -> bool {
    match ke.kind {
        KeyEventKind::Release => matches!(ke.code, KeyCode::Char(' ') | KeyCode::F(8)),
        _ => {
            (ke.code == KeyCode::Char(' ') && ke.modifiers == KeyModifiers::CONTROL)
                || (ke.code == KeyCode::F(8) && ke.modifiers.is_empty())
        }
    }
}

/// Coalesce runs of rapid key events into synthetic `Event::Paste`
/// events. On terminals without bracketed paste, pasted text arrives
/// as individual key events; Enter keys mid-run would otherwise
/// trigger "submit prompt" and split multi-line pastes.
///
/// A contiguous run of character/Enter/Tab events is replaced with a
/// single `Event::Paste` when EITHER:
///
/// 1. `>= PASTE_COALESCE_THRESHOLD` events AND at least one Enter is
///    followed by more characters (distinguishes `type + submit` from
///    `pasted multiline`).
/// 2. **Windows only:** `>= PATH_COALESCE_THRESHOLD` events AND the
///    assembled text starts with a drag-drop-style path anchor. Some
///    Windows Terminal versions deliver dropped paths as keystrokes
///    instead of a bracketed paste; this branch recovers them.
///
/// No-op when bracketed paste already arrives as `Event::Paste`.
fn coalesce_rapid_keys(events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
    // Fast path: not enough events for coalescing to trigger.
    if events.len() < PASTE_COALESCE_THRESHOLD {
        return events;
    }

    // If Event::Paste fragments are mixed with key events (Windows
    // Terminal can split a large bracketed paste across read boundaries),
    // merge everything into a single Event::Paste.
    let (mut has_paste, mut has_keys) = (false, false);
    for e in &events {
        has_paste |= matches!(e.event, Event::Paste(_));
        has_keys |= is_pasteable_key_event(&e.event);
    }
    if has_paste {
        return if has_keys {
            merge_paste_fragments(events)
        } else {
            events
        };
    }

    // Remove Release events — handlers ignore them and they'd break run
    // detection. Exception: voice-chord releases (needed for hold-to-talk).
    let events: Vec<TimedInputEvent> = events
        .into_iter()
        .filter(|ev| {
            !matches!(&ev.event, Event::Key(ke)
                if ke.kind == KeyEventKind::Release && !is_voice_chord(ke))
        })
        .collect();

    let mut result = Vec::with_capacity(events.len());
    let mut i = 0;

    while i < events.len() {
        if is_pasteable_key_event(&events[i].event) {
            let run_start = i;
            let arrived_at = events[i].arrived_at;
            let mut text = String::new();
            let mut seen_enter = false;
            let mut has_char_after_enter = false;

            while i < events.len() && is_pasteable_key_event(&events[i].event) {
                if let Event::Key(ke) = &events[i].event {
                    match ke.code {
                        KeyCode::Char(c) => {
                            text.push(c);
                            if seen_enter {
                                has_char_after_enter = true;
                            }
                        }
                        KeyCode::Enter => {
                            text.push('\n');
                            seen_enter = true;
                        }
                        KeyCode::Tab => {
                            text.push('\t');
                            if seen_enter {
                                has_char_after_enter = true;
                            }
                        }
                        _ => unreachable!("is_pasteable_key_event guards this"),
                    }
                }
                i += 1;
            }

            let run_len = i - run_start;
            let multiline_paste = run_len >= PASTE_COALESCE_THRESHOLD && has_char_after_enter;
            // Windows fallback for drag-drops that arrive as a key
            // burst instead of a bracketed paste — reuse the drop
            // classifier's anchor detector so the two layers can't
            // drift on what counts as a path.
            #[cfg(target_os = "windows")]
            let path_shaped_drop = run_len >= PATH_COALESCE_THRESHOLD
                && crate::prompt_images::starts_with_drop_anchor(&text);
            #[cfg(not(target_os = "windows"))]
            let path_shaped_drop = false;
            if multiline_paste || path_shaped_drop {
                tracing::debug!(
                    run_len,
                    text_len = text.len(),
                    path_shape = path_shaped_drop,
                    "coalesced rapid key events into paste"
                );
                result.push(TimedInputEvent {
                    event: Event::Paste(text),
                    arrived_at,
                });
            } else {
                for ev in &events[run_start..i] {
                    result.push(ev.clone());
                }
            }
        } else {
            result.push(events[i].clone());
            i += 1;
        }
    }

    result
}

fn is_bare_esc_press(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key) if key.code == KeyCode::Esc
            && key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
    )
}

/// Merge `Event::Paste` fragments and interleaved key events into a
/// single `Event::Paste`.  Non-paste, non-key events (Resize, Mouse,
/// Focus) are preserved in order around the merged paste.
fn merge_paste_fragments(events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
    let mut result = Vec::new();
    let mut merged_text = String::new();
    let mut merged_arrived_at = None;

    for ev in events {
        match &ev.event {
            Event::Paste(text) => {
                merged_arrived_at.get_or_insert(ev.arrived_at);
                merged_text.push_str(text);
            }
            Event::Key(ke) if is_pasteable_key_event(&ev.event) => {
                merged_arrived_at.get_or_insert(ev.arrived_at);
                match ke.code {
                    KeyCode::Char(c) => merged_text.push(c),
                    KeyCode::Enter => merged_text.push('\n'),
                    KeyCode::Tab => merged_text.push('\t'),
                    _ => {}
                }
            }
            // Non-pasteable keys (Ctrl+C, Backspace, arrows, Release
            // events, etc.) are artifacts of paste fragmentation — drop.
            Event::Key(_) => {}
            _ => {
                if !merged_text.is_empty() {
                    result.push(TimedInputEvent {
                        event: Event::Paste(std::mem::take(&mut merged_text)),
                        arrived_at: merged_arrived_at
                            .take()
                            .expect("non-empty merged paste has an arrival time"),
                    });
                }
                result.push(ev);
            }
        }
    }

    if !merged_text.is_empty() {
        result.push(TimedInputEvent {
            event: Event::Paste(merged_text),
            arrived_at: merged_arrived_at.expect("non-empty merged paste has an arrival time"),
        });
    }

    result
}

/// How long the fixed startup XTVERSION filter stays armed.
const XT_ARM_WINDOW: Duration = Duration::from_secs(5);
/// Per-fragment wait for a split DCS reply.
const XT_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);
/// Total bound for one held DCS reply.
const XT_MAX_HOLD: Duration = Duration::from_secs(1);
/// Real replies are short (`kitty 0.35.2`); bound tentative input retention.
const XT_MAX_PAYLOAD: usize = 64;

/// XTVERSION DCS reply filter from the fixed Rust renderer event loop.
///
/// Crossterm surfaces `ESC P` as Alt+P, payload bytes as ordinary key
/// presses, and `ESC \` as Alt+backslash. Tentative prefixes retain their
/// reader timestamps, while unrelated events are staged behind them so a
/// failed match is released in FIFO order.
pub(crate) struct XtversionFilter {
    armed: bool,
    /// Starts on the first filter call rather than construction. A loaded
    /// startup must not consume the reply window before input is processed.
    deadline: Option<Instant>,
    state: XtState,
    staged: Vec<StagedXtEvent>,
    payload: String,
    completed: Option<String>,
}

enum StagedXtEvent {
    Tentative(TimedInputEvent),
    PassThrough(TimedInputEvent),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum XtState {
    Idle,
    EscHeld,
    AwaitGt,
    AwaitPipe,
    Payload,
    PayloadEscHeld,
}

enum XtAdvance {
    Hold,
    PassThrough,
    Complete,
    Mismatch,
}

impl XtversionFilter {
    pub(crate) fn new() -> Self {
        Self::with_armed(crabcode_pager_render::audited_terminal::xtversion::reply_pending())
    }

    fn with_armed(armed: bool) -> Self {
        Self {
            armed,
            deadline: None,
            state: XtState::Idle,
            staged: Vec::new(),
            payload: String::new(),
            completed: None,
        }
    }

    fn armed(&self) -> bool {
        self.armed
    }

    fn holding(&self) -> bool {
        self.armed
            && self
                .staged
                .iter()
                .any(|event| matches!(event, StagedXtEvent::Tentative(_)))
    }

    fn take_completed(&mut self) -> Option<String> {
        self.completed.take()
    }

    /// Drop every parser-owned byte and disarm the startup probe when terminal
    /// job control crosses an input generation. Held DCS fragments and
    /// pass-through events were read from the retired terminal generation and
    /// must never be released into the resumed composer.
    fn retire_terminal_generation(&mut self) {
        if self.armed {
            crabcode_pager_render::audited_terminal::xtversion::record_no_reply();
        }
        self.armed = false;
        self.deadline = None;
        self.state = XtState::Idle;
        self.staged.clear();
        self.payload.clear();
        self.completed = None;
    }

    fn flush(&mut self) -> Vec<TimedInputEvent> {
        self.state = XtState::Idle;
        self.payload.clear();
        std::mem::take(&mut self.staged)
            .into_iter()
            .map(|event| match event {
                StagedXtEvent::Tentative(event) | StagedXtEvent::PassThrough(event) => event,
            })
            .collect()
    }

    fn release_pass_through(&mut self) -> Vec<TimedInputEvent> {
        std::mem::take(&mut self.staged)
            .into_iter()
            .filter_map(|event| match event {
                StagedXtEvent::Tentative(_) => None,
                StagedXtEvent::PassThrough(event) => Some(event),
            })
            .collect()
    }

    fn intro_confirmed(&self) -> bool {
        matches!(self.state, XtState::Payload | XtState::PayloadEscHeld)
    }

    /// Flush a pre-intro prefix or drop a confirmed but stalled DCS reply,
    /// preserving unrelated events that arrived while the prefix was held.
    fn resolve_dead_hold(&mut self) -> Vec<TimedInputEvent> {
        if !self.intro_confirmed() {
            return self.flush();
        }
        tracing::debug!("dropping stalled XTVERSION reply fragment");
        self.state = XtState::Idle;
        self.payload.clear();
        self.release_pass_through()
    }

    fn filter(&mut self, events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
        // Once the DCS intro is confirmed, completion/dead-hold resolution owns
        // the bytes. Expiring mid-reply would leak its tail into the composer.
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + XT_ARM_WINDOW);
        if self.armed && Instant::now() > deadline && !(self.holding() && self.intro_confirmed()) {
            self.armed = false;
            crabcode_pager_render::audited_terminal::xtversion::record_no_reply();
        }
        if !self.armed {
            let mut output = self.resolve_dead_hold();
            output.extend(events);
            return output;
        }

        let mut output = Vec::with_capacity(events.len());
        for event in events {
            // A reply that completed mid-batch disarms the parser. Later input
            // in that same batch must pass through immediately.
            if !self.armed {
                output.push(event);
                continue;
            }
            match self.advance(&event.event) {
                XtAdvance::Hold => self.staged.push(StagedXtEvent::Tentative(event)),
                XtAdvance::PassThrough => {
                    if self.holding() {
                        self.staged.push(StagedXtEvent::PassThrough(event));
                    } else {
                        output.push(event);
                    }
                }
                XtAdvance::Complete => {
                    self.completed = Some(std::mem::take(&mut self.payload));
                    self.state = XtState::Idle;
                    self.armed = false;
                    output.extend(self.release_pass_through());
                }
                XtAdvance::Mismatch => {
                    output.append(&mut self.resolve_dead_hold());
                    if matches!(self.advance(&event.event), XtAdvance::Hold) {
                        self.staged.push(StagedXtEvent::Tentative(event));
                    } else {
                        output.push(event);
                    }
                }
            }
        }
        output
    }

    fn advance(&mut self, event: &Event) -> XtAdvance {
        use XtState::{AwaitGt, AwaitPipe, EscHeld, Idle, Payload, PayloadEscHeld};

        if !matches!(event, Event::Key(_)) {
            return XtAdvance::PassThrough;
        }
        if self.state == Idle && is_dcs_intro(event) {
            self.state = AwaitGt;
            return XtAdvance::Hold;
        }
        if self.state == Payload && is_dcs_terminator(event) {
            return XtAdvance::Complete;
        }
        if matches!(self.state, Idle | Payload) && is_bare_esc_press(event) {
            self.state = if self.state == Idle {
                EscHeld
            } else {
                PayloadEscHeld
            };
            return XtAdvance::Hold;
        }
        let Some(character) = xt_plain_char(event) else {
            return XtAdvance::Mismatch;
        };
        match (self.state, character) {
            (EscHeld, 'P') => self.state = AwaitGt,
            (AwaitGt, '>') => self.state = AwaitPipe,
            (AwaitPipe, '|') => self.state = Payload,
            (Payload, character)
                if is_xt_payload_char(character) && self.payload.len() < XT_MAX_PAYLOAD =>
            {
                self.payload.push(character);
            }
            (PayloadEscHeld, '\\') => return XtAdvance::Complete,
            _ => return XtAdvance::Mismatch,
        }
        XtAdvance::Hold
    }
}

/// Apply the XTVERSION filter and await split reply fragments with the fixed
/// 150 ms per-fragment and one-second total bounds.
async fn filter_xtversion_with_fragment_wait(
    xt_filter: &mut XtversionFilter,
    mut raw_events: Vec<TimedInputEvent>,
    input_rx: &mut mpsc::UnboundedReceiver<TimedInputEvent>,
) -> Vec<TimedInputEvent> {
    raw_events = xt_filter.filter(raw_events);
    let hold_deadline = Instant::now() + XT_MAX_HOLD;
    while xt_filter.holding() {
        if Instant::now() > hold_deadline {
            raw_events.extend(xt_filter.resolve_dead_hold());
            break;
        }
        match tokio::time::timeout(XT_FRAGMENT_TIMEOUT, input_rx.recv()).await {
            Ok(Some(event)) => {
                let mut more = vec![event];
                drain_immediate(&mut more, input_rx);
                raw_events.extend(xt_filter.filter(more));
            }
            _ => {
                raw_events.extend(xt_filter.resolve_dead_hold());
                break;
            }
        }
    }
    if let Some(payload) = xt_filter.take_completed() {
        crabcode_pager_render::audited_terminal::xtversion::record_reply(&payload);
    }
    raw_events
}

fn is_xt_payload_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, ' ' | '.' | '_' | '-' | '(' | ')' | '+')
}

fn is_dcs_terminator(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key) if key.kind == KeyEventKind::Press
            && ((key.code == KeyCode::Char('\\')
                && key.modifiers.contains(KeyModifiers::ALT))
                || (key.code == KeyCode::Char('g')
                    && key.modifiers == KeyModifiers::CONTROL))
    )
}

fn is_dcs_intro(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key) if key.kind == KeyEventKind::Press
            && key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('P') | KeyCode::Char('p'))
    )
}

fn xt_plain_char(event: &Event) -> Option<char> {
    match event {
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && (key.modifiers == KeyModifiers::NONE
                    || key.modifiers == KeyModifiers::SHIFT) =>
        {
            match key.code {
                KeyCode::Char(character) => Some(character),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Persistent reassembly for CSI fragments leaked across crossterm reads.
pub(crate) struct CsiFragmentFilter {
    state: CsiFragmentState,
    tentative: Vec<TimedInputEvent>,
}

impl CsiFragmentFilter {
    pub(crate) fn new() -> Self {
        Self {
            state: CsiFragmentState::Idle,
            tentative: Vec::new(),
        }
    }

    /// Drop a partial CSI sequence read from the retired terminal generation.
    ///
    /// Flushing `tentative` would turn terminal protocol bytes into resumed
    /// composer input, while retaining it would let new-generation keystrokes
    /// complete an old mouse/focus report. Both state and bytes are therefore
    /// retired together.
    fn retire_terminal_generation(&mut self) {
        self.state = CsiFragmentState::Idle;
        self.tentative.clear();
    }

    /// Process a batch of events, filtering any CSI fragments.
    /// Partial matches are held in `self.tentative` until the next call.
    /// The `esc_before_run` pop is per-call only (can't retract across batches).
    pub(super) fn filter(&mut self, events: Vec<TimedInputEvent>) -> Vec<TimedInputEvent> {
        let mut result = Vec::with_capacity(self.tentative.len() + events.len());
        let mut esc_before_run = false;
        let mut filtered_count = 0usize;

        for ev in events {
            if is_bare_esc_press(&ev.event) {
                result.append(&mut self.tentative);
                self.state = CsiFragmentState::Idle;
                result.push(ev);
                esc_before_run = true;
                continue;
            }

            match csi_filterable_char(&ev.event) {
                Some(ch) => match self.state.advance(ch) {
                    CsiAdvance::Continue(next) => {
                        self.state = next;
                        self.tentative.push(ev);
                    }
                    CsiAdvance::Complete => {
                        filtered_count += 1;
                        self.tentative.clear();
                        if esc_before_run {
                            result.pop();
                        }
                        esc_before_run = false;
                        self.state = CsiFragmentState::Idle;
                    }
                    CsiAdvance::CompleteFocus => {
                        if esc_before_run {
                            // bare \e then [I/[O in one drain batch is treated as a focus report; a typed pair rarely lands in one batch (same assumption as the mouse Complete arm)
                            filtered_count += 1;
                            self.tentative.clear();
                            result.pop(); // retract the bare Esc
                            // translate the reassembled report into its focus event so focus-driven UX (prompt refocus, recap away-timer, /gboom key-release) still fires over SSH
                            result.push(TimedInputEvent {
                                event: if ch == 'I' {
                                    Event::FocusGained
                                } else {
                                    Event::FocusLost
                                },
                                arrived_at: ev.arrived_at,
                            });
                            esc_before_run = false;
                            self.state = CsiFragmentState::Idle;
                        } else {
                            // typed `[I` / `[O` (e.g. arr[I]) — pass through
                            result.append(&mut self.tentative);
                            self.state = CsiFragmentState::Idle;
                            result.push(ev);
                        }
                    }
                    CsiAdvance::Reject => {
                        result.append(&mut self.tentative);
                        esc_before_run = false;
                        self.state = CsiFragmentState::Idle;
                        match CsiFragmentState::Idle.advance(ch) {
                            CsiAdvance::Continue(next) => {
                                self.state = next;
                                self.tentative.push(ev);
                            }
                            _ => result.push(ev),
                        }
                    }
                },
                None => {
                    result.append(&mut self.tentative);
                    self.state = CsiFragmentState::Idle;
                    esc_before_run = false;
                    result.push(ev);
                }
            }
        }

        if filtered_count > 0 {
            tracing::debug!(filtered_count, "filtered CSI fragments");
        }

        // A lone typed `[` is indistinguishable from the start of a CSI fragment —
        // an SGR mouse report `[<…M` or a focus report `[I`/`[O` — but user input
        // must render immediately. Real leaked fragments arrive with the
        // byte after `[` in the same read(); carrying only `Bracket` across batches
        // is unnecessary and holds the key until the next keystroke. Deeper partial
        // states (`[<…`) still persist for cross-batch continuation.
        if matches!(self.state, CsiFragmentState::Bracket) {
            result.append(&mut self.tentative);
            self.state = CsiFragmentState::Idle;
        }

        result
    }
}

/// States for recognizing SGR mouse `[<digits;digits;digits{M,m}` and focus `[I`/`[O`.
#[derive(Clone, Copy, Debug)]
enum CsiFragmentState {
    Idle,
    Bracket,
    LessThan,
    Digits1,
    Semi1,
    Digits2,
    Semi2,
    Digits3,
}

#[derive(Debug)]
enum CsiAdvance {
    Continue(CsiFragmentState),
    Complete,
    CompleteFocus,
    Reject,
}

impl CsiFragmentState {
    fn advance(self, ch: char) -> CsiAdvance {
        use CsiFragmentState::*;
        match (self, ch) {
            (Idle, '[') => CsiAdvance::Continue(Bracket),
            (Bracket, '<') => CsiAdvance::Continue(LessThan),
            // \e[I / \e[O focus report finals
            (Bracket, 'I') | (Bracket, 'O') => CsiAdvance::CompleteFocus,
            (LessThan | Digits1, c) if c.is_ascii_digit() => CsiAdvance::Continue(Digits1),
            (Digits1, ';') => CsiAdvance::Continue(Semi1),
            (Semi1 | Digits2, c) if c.is_ascii_digit() => CsiAdvance::Continue(Digits2),
            (Digits2, ';') => CsiAdvance::Continue(Semi2),
            (Semi2 | Digits3, c) if c.is_ascii_digit() => CsiAdvance::Continue(Digits3),
            (Digits3, 'M' | 'm') => CsiAdvance::Complete,
            _ => CsiAdvance::Reject,
        }
    }
}

fn csi_filterable_char(ev: &Event) -> Option<char> {
    match ev {
        Event::Key(ke)
            if ke.kind == KeyEventKind::Press
                && (ke.modifiers == KeyModifiers::NONE || ke.modifiers == KeyModifiers::SHIFT) =>
        {
            if let KeyCode::Char(c) = ke.code {
                Some(c)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
    }

    fn timed(event: Event, arrived_at: Instant) -> TimedInputEvent {
        TimedInputEvent { event, arrived_at }
    }

    fn press(code: KeyCode) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            Instant::now(),
        )
    }

    fn press_at(code: KeyCode, arrived_at: Instant) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            arrived_at,
        )
    }

    fn press_shift(code: KeyCode) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT)),
            Instant::now(),
        )
    }

    fn press_ctrl(code: KeyCode) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL)),
            Instant::now(),
        )
    }

    fn release(code: KeyCode) -> TimedInputEvent {
        let mut event = KeyEvent::new(code, KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        timed(Event::Key(event), Instant::now())
    }

    fn shifted(character: char) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent {
                code: KeyCode::Char(character),
                modifiers: KeyModifiers::SHIFT,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            Instant::now(),
        )
    }

    fn press_mods_at(
        code: KeyCode,
        modifiers: KeyModifiers,
        arrived_at: Instant,
    ) -> TimedInputEvent {
        timed(
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            arrived_at,
        )
    }

    fn dcs_reply_events(payload: &str, arrived_at: Instant) -> Vec<TimedInputEvent> {
        let mut events = vec![
            press_mods_at(
                KeyCode::Char('P'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                arrived_at,
            ),
            press_mods_at(KeyCode::Char('>'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('|'), KeyModifiers::NONE, arrived_at),
        ];
        events.extend(payload.chars().map(|character| {
            let modifiers = if character.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            press_mods_at(KeyCode::Char(character), modifiers, arrived_at)
        }));
        events.push(press_mods_at(
            KeyCode::Char('\\'),
            KeyModifiers::ALT,
            arrived_at,
        ));
        events
    }

    #[cfg(target_os = "linux")]
    fn mouse(kind: crossterm::event::MouseEventKind, modifiers: KeyModifiers) -> TimedInputEvent {
        timed(
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 7,
                row: 11,
                modifiers,
            }),
            Instant::now(),
        )
    }

    fn writer_events() -> (
        mpsc::UnboundedSender<WriterEvent>,
        mpsc::UnboundedReceiver<WriterEvent>,
    ) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn fixed_bias_prioritizes_signal_writer_and_buffered_input_before_callbacks() {
        let signal = Arc::new(Notify::new());
        let outbound = Arc::new(Notify::new());
        let backend = Arc::new(Notify::new());
        let stderr = Arc::new(Notify::new());
        let (writer_tx, mut writer) = writer_events();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        signal.notify_one();
        writer_tx
            .send(WriterEvent::Written(1))
            .expect("writer receiver");
        outbound.notify_one();
        backend.notify_one();
        input_tx.send(press(KeyCode::Char('x'))).expect("input");

        let next = |writer: &mut mpsc::UnboundedReceiver<WriterEvent>,
                    input_rx: &mut mpsc::UnboundedReceiver<TimedInputEvent>| {
            runtime().block_on(select_fixed_wake(
                Some(&signal),
                writer,
                &outbound,
                &backend,
                &stderr,
                input_rx,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(Instant::now() + Duration::from_secs(1)),
            ))
        };

        assert!(matches!(next(&mut writer, &mut input_rx), Wake::Signal));
        assert!(matches!(
            next(&mut writer, &mut input_rx),
            Wake::Writer(Some(WriterEvent::Written(1)))
        ));
        assert!(matches!(
            next(&mut writer, &mut input_rx),
            Wake::TerminalInput(Some(_))
        ));
        assert!(matches!(next(&mut writer, &mut input_rx), Wake::Outbound));
        assert!(matches!(next(&mut writer, &mut input_rx), Wake::Runtime));
    }

    #[test]
    fn buffered_input_gates_runtime_firehose() {
        let signal = Arc::new(Notify::new());
        let outbound = Arc::new(Notify::new());
        let backend = Arc::new(Notify::new());
        let stderr = Arc::new(Notify::new());
        let (_writer_tx, mut writer) = writer_events();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        outbound.notify_one();
        backend.notify_one();
        input_tx.send(press(KeyCode::Char('x'))).expect("input");

        let wake = runtime().block_on(select_fixed_wake(
            Some(&signal),
            &mut writer,
            &outbound,
            &backend,
            &stderr,
            &mut input_rx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Instant::now() + Duration::from_secs(1)),
        ));
        assert!(matches!(wake, Wake::TerminalInput(Some(_))));
    }

    #[test]
    fn closed_input_channel_preempts_runtime_firehose() {
        let signal = Arc::new(Notify::new());
        let outbound = Arc::new(Notify::new());
        let backend = Arc::new(Notify::new());
        let stderr = Arc::new(Notify::new());
        let (_writer_tx, mut writer) = writer_events();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        drop(input_tx);
        backend.notify_one();
        stderr.notify_one();

        let wake = runtime().block_on(select_fixed_wake(
            Some(&signal),
            &mut writer,
            &outbound,
            &backend,
            &stderr,
            &mut input_rx,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Instant::now() + Duration::from_secs(1)),
        ));

        assert!(matches!(wake, Wake::TerminalInput(None)));
    }

    #[test]
    fn resize_deadline_has_its_own_wake_identity() {
        let signal = Arc::new(Notify::new());
        let outbound = Arc::new(Notify::new());
        let backend = Arc::new(Notify::new());
        let stderr = Arc::new(Notify::new());
        let (_writer_tx, mut writer) = writer_events();
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel();

        let wake = runtime().block_on(select_fixed_wake(
            Some(&signal),
            &mut writer,
            &outbound,
            &backend,
            &stderr,
            &mut input_rx,
            None,
            None,
            Some(Instant::now()),
            None,
            None,
            None,
            None,
            None,
        ));
        assert!(matches!(wake, Wake::ResizeDebounce));
    }

    #[test]
    fn pending_relaunch_deadline_has_its_own_wake_identity() {
        let signal = Arc::new(Notify::new());
        let outbound = Arc::new(Notify::new());
        let backend = Arc::new(Notify::new());
        let stderr = Arc::new(Notify::new());
        let (_writer_tx, mut writer) = writer_events();
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel();

        let wake = runtime().block_on(select_fixed_wake(
            Some(&signal),
            &mut writer,
            &outbound,
            &backend,
            &stderr,
            &mut input_rx,
            None,
            None,
            None,
            None,
            None,
            Some(Instant::now()),
            None,
            None,
        ));
        assert!(matches!(wake, Wake::PendingRelaunch));
    }

    #[test]
    fn resize_only_defers_one_frame_but_mixed_input_draws_immediately() {
        let now = Instant::now();
        let mut resize_debounce_at = None;
        let mut presenter = Presenter::new();

        apply_input_presentation(
            DrainResult {
                needs_draw: true,
                resize_only: true,
                ..DrainResult::default()
            },
            now,
            &mut resize_debounce_at,
            &mut presenter,
        );
        assert_eq!(resize_debounce_at, Some(now + RESIZE_DEBOUNCE));
        assert!(
            !presenter.dirty,
            "a resize-only batch must wait for the fixed debounce deadline"
        );

        apply_input_presentation(
            DrainResult {
                needs_draw: true,
                resize_only: false,
                ..DrainResult::default()
            },
            now + Duration::from_millis(1),
            &mut resize_debounce_at,
            &mut presenter,
        );
        assert_eq!(resize_debounce_at, None);
        assert!(presenter.dirty);
        assert!(!presenter.force_full_repaint);
    }

    #[test]
    fn forced_repaint_cancels_resize_debounce_and_stays_sticky() {
        let now = Instant::now();
        let mut resize_debounce_at = Some(now + RESIZE_DEBOUNCE);
        let mut presenter = Presenter::new();
        presenter.request(false);

        apply_input_presentation(
            DrainResult {
                needs_draw: true,
                force_repaint: true,
                resize_only: true,
                ..DrainResult::default()
            },
            now,
            &mut resize_debounce_at,
            &mut presenter,
        );

        assert_eq!(resize_debounce_at, None);
        assert!(presenter.dirty);
        assert!(presenter.force_full_repaint);
    }

    #[test]
    fn production_entrypoint_uses_fixed_driver_not_a_second_loop() {
        let source = include_str!("lib.rs");
        let start = source
            .find("async fn run_interactive(")
            .expect("production interactive entrypoint");
        let end = source[start..]
            .find("\n/// Keep watcher ownership")
            .map(|offset| start + offset)
            .expect("end of interactive entrypoint");
        let entrypoint = &source[start..end];

        assert!(entrypoint.contains("CrabCodeDirectRuntimeAdapter::new("));
        assert!(entrypoint.contains("app_event_loop::run_fixed_terminal_lifecycle("));
        for forbidden in [
            "tokio::select!",
            "select_fixed_wake(",
            "drain_and_process(",
            "while !app.should_quit",
        ] {
            assert!(
                !entrypoint.contains(forbidden),
                "production entrypoint retained a second lifecycle: {forbidden}"
            );
        }
    }

    #[test]
    fn fixed_select_keeps_terminal_timers_before_appearance() {
        let source = include_str!("app_event_loop.rs");
        let start = source
            .find("async fn select_fixed_wake(")
            .expect("fixed wake selector");
        let end = source[start..]
            .find("\nasync fn appearance_wait")
            .map(|offset| start + offset)
            .expect("end of fixed wake selector");
        let selector = &source[start..end];
        let ordered = [
            "signal_wait(signal)",
            "writer.recv()",
            "outbound.notified()",
            "runtime.notified()",
            "input_rx.recv()",
            "deadline_wait(resize_debounce_at)",
            "deadline_wait(deferred_draw_at)",
            "deadline_wait(suspend_retry_at)",
            "deadline_wait(scroll_tick_at)",
            "deadline_wait(animation_tick_at)",
            "appearance_wait(appearance_watcher)",
        ];
        let mut previous = 0;
        for needle in ordered {
            let index = selector
                .find(needle)
                .unwrap_or_else(|| panic!("fixed wake selector omitted lifecycle arm {needle}"));
            assert!(
                index >= previous,
                "fixed wake selector reordered lifecycle arm {needle}"
            );
            previous = index;
        }
    }

    #[test]
    fn fixed_driver_preflights_signals_then_consumes_handoff_before_other_requests() {
        let source = include_str!("app_event_loop.rs");
        let start = source
            .find("pub(crate) async fn run_fixed_terminal_lifecycle(")
            .expect("fixed terminal lifecycle driver");
        let end = source[start..]
            .find("\nfn apply_input_presentation(")
            .map(|offset| start + offset)
            .expect("end of fixed terminal lifecycle driver");
        let driver = &source[start..end];
        let signals = driver
            .find("adapter.service_terminal_signals(")
            .expect("direct terminal signal preflight");
        let liveness = driver
            .find("adapter.inspect_terminal_liveness(terminal)?;")
            .expect("direct terminal liveness preflight");
        let materialize = driver
            .find("adapter.materialize_pending_terminal_handoff();")
            .expect("handoff materialization");
        let consume = driver
            .find("adapter.run_pending_terminal_handoff(")
            .expect("loop-top handoff consumer");
        let other_services = driver
            .find("adapter.service_terminal_requests(")
            .expect("non-handoff terminal service");
        let deadlines = driver
            .find("let deferred_input_tick_at")
            .expect("fresh deadline derivation");
        assert!(signals < liveness);
        assert!(liveness < materialize);
        assert!(materialize < consume);
        assert!(consume < other_services);
        assert!(other_services < deadlines);
    }

    #[test]
    fn fixed_driver_retires_persistent_parsers_on_terminal_generation_change() {
        let source = include_str!("app_event_loop.rs");
        let start = source
            .find("pub(crate) async fn run_fixed_terminal_lifecycle(")
            .expect("fixed terminal lifecycle driver");
        let end = source[start..]
            .find("\nfn apply_input_presentation(")
            .map(|offset| start + offset)
            .expect("end of fixed terminal lifecycle driver");
        let driver = &source[start..end];
        let signals = driver
            .find("adapter.service_terminal_signals(")
            .expect("loop-top terminal signal service");
        let csi_retirement = driver
            .find("csi_filter.retire_terminal_generation();")
            .expect("CSI parser generation retirement");
        let xt_retirement = driver
            .find("xt_filter.retire_terminal_generation();")
            .expect("XTVERSION parser generation retirement");
        let selected_input_recheck = driver
            .rfind("let terminal_generation_changed = adapter.service_terminal_signals(")
            .expect("post-selection signal recheck");
        let selected_input_drop = driver[selected_input_recheck..]
            .find("continue;")
            .map(|offset| selected_input_recheck + offset)
            .expect("selected pre-cutover input discard");
        let liveness = driver
            .find("adapter.inspect_terminal_liveness(terminal)?;")
            .expect("direct terminal liveness preflight");

        assert!(signals < csi_retirement);
        assert!(csi_retirement < xt_retirement);
        assert!(xt_retirement < liveness);
        assert!(liveness < selected_input_recheck);
        assert!(selected_input_recheck < selected_input_drop);
    }

    #[test]
    fn link_modifier_clock_stays_on_terminal_root_view_owner_chain() {
        let source = include_str!("app_event_loop.rs");
        let start = source
            .find("pub(crate) async fn run_fixed_terminal_lifecycle(")
            .expect("fixed terminal lifecycle driver");
        let end = source[start..]
            .find("\nfn apply_input_presentation(")
            .map(|offset| start + offset)
            .expect("end of fixed terminal lifecycle driver");
        let driver = &source[start..end];
        assert!(
            driver.contains("terminal.renderer_animation_deadline(now)"),
            "the terminal-owned AppView must supply the link-poll deadline"
        );
        assert!(
            driver.contains("terminal.tick_renderer_animation(now)"),
            "the terminal-owned AppView must consume the link-poll tick"
        );
    }

    #[test]
    fn drain_batches_events_and_aggregates_one_draw_decision() {
        let start = Instant::now();
        let first = timed(Event::Resize(80, 24), start);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        input_tx
            .send(timed(
                Event::Resize(81, 24),
                start + Duration::from_millis(1),
            ))
            .expect("second resize");
        let mut filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(false);
        let mut handled = 0;

        let result = runtime()
            .block_on(drain_and_process(
                first,
                &mut input_rx,
                &mut filter,
                &mut xt_filter,
                |_| {
                    handled += 1;
                    Ok(HandledInput {
                        needs_draw: true,
                        ..HandledInput::default()
                    })
                },
            ))
            .expect("drain succeeds");

        assert_eq!(handled, 2);
        assert_eq!(
            result,
            DrainResult {
                needs_draw: true,
                resize_only: true,
                ..DrainResult::default()
            }
        );
    }

    #[test]
    fn non_resize_change_cancels_resize_only_classification() {
        let first = timed(Event::Resize(80, 24), Instant::now());
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        input_tx.send(press(KeyCode::Char('x'))).expect("key");
        let mut filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(false);

        let result = runtime()
            .block_on(drain_and_process(
                first,
                &mut input_rx,
                &mut filter,
                &mut xt_filter,
                |_| {
                    Ok(HandledInput {
                        needs_draw: true,
                        ..HandledInput::default()
                    })
                },
            ))
            .expect("drain succeeds");

        assert!(result.needs_draw);
        assert!(!result.resize_only);
    }

    #[test]
    fn tty_suspend_arming_stops_later_buffered_events() {
        let first = press(KeyCode::Char('e'));
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        input_tx.send(press(KeyCode::Char('x'))).expect("later key");
        let mut filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(false);
        let mut handled = 0;

        runtime()
            .block_on(drain_and_process(
                first,
                &mut input_rx,
                &mut filter,
                &mut xt_filter,
                |_| {
                    handled += 1;
                    Ok(HandledInput {
                        needs_draw: true,
                        stop_batch: true,
                        ..HandledInput::default()
                    })
                },
            ))
            .expect("drain succeeds");

        assert_eq!(handled, 1);
    }

    #[test]
    fn csi_mouse_fragment_is_filtered_before_routing() {
        let mut fragment = vec![press(KeyCode::Esc), press(KeyCode::Char('['))];
        for character in "<0;10;5".chars() {
            fragment.push(press(KeyCode::Char(character)));
        }
        fragment.push(shifted('M'));
        let first = fragment.remove(0);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        for event in fragment {
            input_tx.send(event).expect("fragment event");
        }
        let mut filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(false);
        let mut handled = 0;

        runtime()
            .block_on(drain_and_process(
                first,
                &mut input_rx,
                &mut filter,
                &mut xt_filter,
                |_| {
                    handled += 1;
                    Ok(HandledInput::default())
                },
            ))
            .expect("drain succeeds");

        assert_eq!(handled, 0);
    }

    /// A typed `[` must be emitted in the same input batch rather than held
    /// until another key arrives. Deeper `[<...` terminal fragments still
    /// remain eligible for cross-batch reassembly.
    #[test]
    fn csi_filter_lone_bracket_emitted_same_batch() {
        let mut filter = CsiFragmentFilter::new();

        let bracket = press(KeyCode::Char('['));
        assert_eq!(filter.filter(vec![bracket.clone()]), vec![bracket]);
        assert!(matches!(filter.state, CsiFragmentState::Idle));
        assert!(filter.tentative.is_empty());

        let next = press(KeyCode::Char('a'));
        assert_eq!(filter.filter(vec![next.clone()]), vec![next]);
    }

    #[test]
    fn csi_filter_terminal_generation_drops_old_partial_without_consuming_new_input() {
        let mut filter = CsiFragmentFilter::new();
        let old_generation = "[<0;1;"
            .chars()
            .map(|character| press(KeyCode::Char(character)))
            .collect();

        assert!(filter.filter(old_generation).is_empty());
        assert!(matches!(filter.state, CsiFragmentState::Semi2));
        assert_eq!(filter.tentative.len(), 6);

        filter.retire_terminal_generation();

        assert!(matches!(filter.state, CsiFragmentState::Idle));
        assert!(filter.tentative.is_empty());
        let new_generation: Vec<_> = "2Muser"
            .chars()
            .map(|character| press(KeyCode::Char(character)))
            .collect();
        assert_eq!(
            filter.filter(new_generation.clone()),
            new_generation,
            "new-generation text must not complete an old mouse CSI fragment"
        );
    }

    #[test]
    fn xt_filter_swallows_full_reply() {
        let arrived_at = Instant::now();
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(dcs_reply_events("kitty 0.35.2", arrived_at));

        assert!(output.is_empty());
        assert_eq!(filter.take_completed().as_deref(), Some("kitty 0.35.2"));
        assert!(!filter.armed());
    }

    #[test]
    fn xt_filter_dead_pre_intro_hold_preserves_fifo_and_timestamps() {
        let started_at = Instant::now();
        let resize_at = started_at + Duration::from_millis(4);
        let escape = press_mods_at(KeyCode::Esc, KeyModifiers::NONE, started_at);
        let resize = timed(Event::Resize(80, 24), resize_at);
        let mut filter = XtversionFilter::with_armed(true);

        assert!(
            filter
                .filter(vec![escape.clone(), resize.clone()])
                .is_empty()
        );
        let output = filter.resolve_dead_hold();

        assert_eq!(output, vec![escape, resize]);
    }

    #[test]
    fn xt_filter_confirmed_dead_hold_releases_interleaved_pass_through() {
        let started_at = Instant::now();
        let resize_at = started_at + Duration::from_millis(5);
        let mut events = dcs_reply_events("x", started_at);
        events.pop();
        events.insert(4, timed(Event::Resize(90, 30), resize_at));
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(events).is_empty());
        let output = filter.resolve_dead_hold();

        assert_eq!(output, vec![timed(Event::Resize(90, 30), resize_at)]);
    }

    #[test]
    fn xt_filter_completed_reply_preserves_interleaved_pass_through_order() {
        let started_at = Instant::now();
        let resize_at = started_at + Duration::from_millis(3);
        let focus_at = started_at + Duration::from_millis(4);
        let mut reply = dcs_reply_events("x", started_at);
        let tail = reply.split_off(2);
        reply.push(timed(Event::Resize(80, 24), resize_at));
        reply.push(timed(Event::FocusGained, focus_at));
        reply.extend(tail);
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(reply);

        assert_eq!(
            output,
            vec![
                timed(Event::Resize(80, 24), resize_at),
                timed(Event::FocusGained, focus_at),
            ]
        );
        assert_eq!(filter.take_completed().as_deref(), Some("x"));
    }

    #[test]
    fn xt_filter_passes_surrounding_keys() {
        let arrived_at = Instant::now();
        let before = press_mods_at(KeyCode::Char('a'), KeyModifiers::NONE, arrived_at);
        let after = press_mods_at(KeyCode::Char('b'), KeyModifiers::NONE, arrived_at);
        let mut events = vec![before.clone()];
        events.extend(dcs_reply_events("tmux 3.4", arrived_at));
        events.push(after.clone());
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(events);

        assert_eq!(output, vec![before, after]);
        assert_eq!(filter.take_completed().as_deref(), Some("tmux 3.4"));
    }

    #[test]
    fn xt_filter_reply_split_across_batches() {
        let arrived_at = Instant::now();
        let events = dcs_reply_events("foot(1.22.0)", arrived_at);
        let (first, second) = events.split_at(5);
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(first.to_vec()).is_empty());
        assert!(filter.holding());
        assert!(filter.filter(second.to_vec()).is_empty());
        assert_eq!(filter.take_completed().as_deref(), Some("foot(1.22.0)"));
    }

    #[test]
    fn xt_filter_terminal_generation_drops_staged_reply_and_disarms() {
        let arrived_at = Instant::now();
        let mut old_generation = dcs_reply_events("kitty", arrived_at);
        old_generation.truncate(5);
        old_generation.push(timed(Event::Resize(91, 31), arrived_at));
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(old_generation).is_empty());
        assert!(filter.armed());
        assert!(filter.holding());
        assert!(filter.deadline.is_some());
        assert!(!filter.staged.is_empty());
        assert!(!filter.payload.is_empty());

        filter.retire_terminal_generation();

        assert!(!filter.armed());
        assert!(filter.deadline.is_none());
        assert!(matches!(filter.state, XtState::Idle));
        assert!(filter.staged.is_empty());
        assert!(filter.payload.is_empty());
        assert!(filter.take_completed().is_none());
        let new_generation = vec![
            press_mods_at(KeyCode::Char('n'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('e'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('w'), KeyModifiers::NONE, arrived_at),
        ];
        assert_eq!(filter.filter(new_generation.clone()), new_generation);
    }

    #[test]
    fn xt_filter_dead_pre_intro_prefix_is_flushed() {
        let arrived_at = Instant::now();
        let prefix = dcs_reply_events("x", arrived_at)[..2].to_vec();
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(prefix.clone()).is_empty());
        assert!(filter.holding());
        assert_eq!(filter.resolve_dead_hold(), prefix);
        assert!(filter.take_completed().is_none());
    }

    #[test]
    fn xt_filter_non_reply_key_flushes_partial_prefix() {
        let arrived_at = Instant::now();
        let mut events = dcs_reply_events("x", arrived_at)[..2].to_vec();
        events.push(press_mods_at(
            KeyCode::Enter,
            KeyModifiers::NONE,
            arrived_at,
        ));
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(events.clone());

        assert_eq!(output, events);
        assert!(!filter.holding());
        assert!(filter.take_completed().is_none());
    }

    #[test]
    fn xt_filter_accepts_split_escape_intro_and_terminator() {
        let arrived_at = Instant::now();
        let events = vec![
            press_mods_at(KeyCode::Esc, KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('P'), KeyModifiers::SHIFT, arrived_at),
            press_mods_at(KeyCode::Char('>'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('|'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('x'), KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Esc, KeyModifiers::NONE, arrived_at),
            press_mods_at(KeyCode::Char('\\'), KeyModifiers::NONE, arrived_at),
        ];
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(events).is_empty());
        assert_eq!(filter.take_completed().as_deref(), Some("x"));
    }

    #[test]
    fn xt_filter_accepts_bel_terminator() {
        let arrived_at = Instant::now();
        let mut events = dcs_reply_events("st 0.9", arrived_at);
        events.pop();
        events.push(press_mods_at(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
            arrived_at,
        ));
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(events).is_empty());
        assert_eq!(filter.take_completed().as_deref(), Some("st 0.9"));
    }

    #[test]
    fn xt_filter_disarmed_passes_everything() {
        let events = dcs_reply_events("kitty 0.35.2", Instant::now());
        let mut filter = XtversionFilter::with_armed(false);

        assert_eq!(filter.filter(events.clone()), events);
        assert!(filter.take_completed().is_none());
    }

    #[test]
    fn xt_filter_malformed_confirmed_reply_drops_fragment_not_user_key() {
        let arrived_at = Instant::now();
        let typed = press_mods_at(KeyCode::Char('/'), KeyModifiers::NONE, arrived_at);
        let mut events = dcs_reply_events("x", arrived_at);
        events.pop();
        events.push(typed.clone());
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(events);

        assert_eq!(output, vec![typed]);
        assert!(!filter.holding());
    }

    #[test]
    fn xt_filter_stray_escape_before_reply_is_released() {
        let arrived_at = Instant::now();
        let escape = press_mods_at(KeyCode::Esc, KeyModifiers::NONE, arrived_at);
        let mut events = vec![escape.clone()];
        events.extend(dcs_reply_events("wezterm 2.0", arrived_at));
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(events);

        assert_eq!(output, vec![escape]);
        assert_eq!(filter.take_completed().as_deref(), Some("wezterm 2.0"));
    }

    #[test]
    fn xt_filter_events_after_completion_pass_in_same_batch() {
        let arrived_at = Instant::now();
        let escape = press_mods_at(KeyCode::Esc, KeyModifiers::NONE, arrived_at);
        let alt_p = press_mods_at(
            KeyCode::Char('P'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            arrived_at,
        );
        let mut events = dcs_reply_events("kitty 0.35.2", arrived_at);
        events.push(escape.clone());
        events.push(alt_p.clone());
        let mut filter = XtversionFilter::with_armed(true);

        let output = filter.filter(events);

        assert_eq!(output, vec![escape, alt_p]);
        assert_eq!(filter.take_completed().as_deref(), Some("kitty 0.35.2"));
        assert!(!filter.holding());
    }

    #[test]
    fn xt_filter_resize_mid_hold_does_not_break_reply() {
        let started_at = Instant::now();
        let resize_at = started_at + Duration::from_millis(3);
        let focus_at = started_at + Duration::from_millis(4);
        let events = dcs_reply_events("kitty 0.35.2", started_at);
        let (first, second) = events.split_at(6);
        let mut first = first.to_vec();
        first.push(timed(Event::Resize(80, 24), resize_at));
        first.push(timed(Event::FocusGained, focus_at));
        let mut filter = XtversionFilter::with_armed(true);

        assert!(filter.filter(first).is_empty());
        assert!(filter.holding());
        let output = filter.filter(second.to_vec());

        assert_eq!(
            output,
            vec![
                timed(Event::Resize(80, 24), resize_at),
                timed(Event::FocusGained, focus_at),
            ]
        );
        assert_eq!(filter.take_completed().as_deref(), Some("kitty 0.35.2"));
    }

    #[test]
    fn drain_removes_xtversion_before_paste_and_application_routing() {
        let arrived_at = Instant::now();
        let first = press_mods_at(KeyCode::Char('a'), KeyModifiers::NONE, arrived_at);
        let last = press_mods_at(KeyCode::Char('b'), KeyModifiers::NONE, arrived_at);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        for event in dcs_reply_events("kitty 0.35.2", arrived_at)
            .into_iter()
            .chain(std::iter::once(last.clone()))
        {
            input_tx.send(event).expect("input receiver");
        }
        drop(input_tx);
        let mut csi_filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(true);
        let mut routed = Vec::new();

        runtime()
            .block_on(drain_and_process(
                first.clone(),
                &mut input_rx,
                &mut csi_filter,
                &mut xt_filter,
                |event| {
                    routed.push(event);
                    Ok(HandledInput::default())
                },
            ))
            .expect("drain succeeds");

        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0].event, first.event);
        assert_eq!(routed[0].arrived_at, arrived_at);
        assert_eq!(routed[1].event, last.event);
        assert_eq!(routed[1].arrived_at, arrived_at);
    }

    #[test]
    fn is_voice_chord_press_exact_release_keycode() {
        use KeyEventKind::{Press, Release};
        let hit = |code, mods, kind| {
            is_voice_chord(&KeyEvent {
                code,
                modifiers: mods,
                kind,
                state: KeyEventState::NONE,
            })
        };
        let (space, f8, control, none) = (
            KeyCode::Char(' '),
            KeyCode::F(8),
            KeyModifiers::CONTROL,
            KeyModifiers::NONE,
        );

        assert!(hit(space, control, Press));
        assert!(hit(f8, none, Press));
        assert!(!hit(space, control | KeyModifiers::ALT, Press));
        assert!(!hit(f8, KeyModifiers::SHIFT, Press));
        assert!(!hit(space, none, Press));
        assert!(hit(space, none, Release));
        assert!(hit(f8, none, Release));
        assert!(!hit(KeyCode::Char('a'), none, Release));
    }

    #[test]
    fn timed_paste_uses_first_contributing_event() {
        let start = Instant::now();
        let events = vec![
            press_at(KeyCode::Char('a'), start),
            press_at(KeyCode::Enter, start + Duration::from_millis(4)),
            press_at(KeyCode::Char('b'), start + Duration::from_millis(8)),
        ];

        let coalesced = coalesce_rapid_keys(events);
        assert_eq!(coalesced.len(), 1);
        assert_eq!(coalesced[0].arrived_at, start);
        assert_eq!(coalesced[0].event, Event::Paste("a\nb".to_owned()));

        let fragments = vec![
            timed(Event::Paste("a".to_owned()), start),
            press_at(KeyCode::Enter, start + Duration::from_millis(4)),
            press_at(KeyCode::Char('b'), start + Duration::from_millis(8)),
        ];
        let merged = merge_paste_fragments(fragments);
        assert_eq!(merged[0].arrived_at, start);
        assert_eq!(merged[0].event, Event::Paste("a\nb".to_owned()));
    }

    #[test]
    fn timed_detect_paste_collects_a_follow_up_burst_before_routing() {
        let start = Instant::now();
        let first = press_at(KeyCode::Char('a'), start);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let sender = async move {
            // Let the production drain observe an initially empty channel,
            // then deliver the follow-up before its fixed 2 ms deadline.
            tokio::task::yield_now().await;
            input_tx
                .send(press_at(KeyCode::Enter, start + Duration::from_millis(1)))
                .expect("input receiver");
            input_tx
                .send(press_at(
                    KeyCode::Char('b'),
                    start + Duration::from_millis(2),
                ))
                .expect("input receiver");
        };
        let mut csi_filter = CsiFragmentFilter::new();
        let mut xt_filter = XtversionFilter::with_armed(false);
        let mut routed = Vec::new();

        runtime().block_on(async {
            tokio::join!(
                sender,
                drain_and_process(
                    first,
                    &mut input_rx,
                    &mut csi_filter,
                    &mut xt_filter,
                    |event| {
                        routed.push(event);
                        Ok(HandledInput::default())
                    },
                )
            )
            .1
            .expect("drain succeeds");
        });

        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].event, Event::Paste("a\nb".to_owned()));
        assert_eq!(routed[0].arrived_at, start);
    }

    #[test]
    fn coalesce_multiline_paste_without_bracketed_paste() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            press(KeyCode::Char('c')),
            press(KeyCode::Char('d')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("ab\ncd".to_string()));
    }

    #[test]
    fn coalesce_filters_release_events() {
        let events = vec![
            press(KeyCode::Char('a')),
            release(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            release(KeyCode::Char('b')),
            press(KeyCode::Enter),
            release(KeyCode::Enter),
            press(KeyCode::Char('c')),
            release(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("ab\nc".to_string()));
    }

    #[test]
    fn coalesce_preserves_voice_chord_releases_for_the_application_router() {
        let ordinary_release = release(KeyCode::Char('x'));
        let space_release = release(KeyCode::Char(' '));
        let f8_release = release(KeyCode::F(8));
        let character = press(KeyCode::Char('a'));

        let result = coalesce_rapid_keys(vec![
            ordinary_release,
            space_release.clone(),
            f8_release.clone(),
            character.clone(),
        ]);

        assert_eq!(result, vec![space_release, f8_release, character]);
    }

    #[test]
    fn coalesce_preserves_shifted_chars() {
        let events = vec![
            press_shift(KeyCode::Char('H')),
            press(KeyCode::Char('i')),
            press(KeyCode::Enter),
            press_shift(KeyCode::Char('B')),
            press(KeyCode::Char('y')),
            press(KeyCode::Char('e')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("Hi\nBye".to_string()));
    }

    #[test]
    fn coalesce_below_threshold_no_change() {
        let events = vec![press(KeyCode::Char('a')), press(KeyCode::Enter)];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0].event, Event::Key(ke) if ke.code == KeyCode::Char('a')));
        assert!(matches!(&result[1].event, Event::Key(ke) if ke.code == KeyCode::Enter));
    }

    #[test]
    fn coalesce_no_enter_no_change() {
        let events = vec![
            press(KeyCode::Char('h')),
            press(KeyCode::Char('e')),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('o')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 5);
        assert!(
            result
                .iter()
                .all(|event| matches!(event.event, Event::Key(_)))
        );
    }

    #[test]
    fn coalesce_only_enters_no_change() {
        let events = vec![
            press(KeyCode::Enter),
            press(KeyCode::Enter),
            press(KeyCode::Enter),
            press(KeyCode::Enter),
        ];
        assert_eq!(coalesce_rapid_keys(events).len(), 4);
    }

    #[test]
    fn coalesce_preserves_non_key_events() {
        let events = vec![
            timed(Event::Resize(80, 24), Instant::now()),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
            timed(Event::Resize(100, 30), Instant::now()),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0].event, Event::Resize(80, 24)));
        assert_eq!(result[1].event, Event::Paste("a\nb".to_string()));
        assert!(matches!(&result[2].event, Event::Resize(100, 30)));
    }

    #[test]
    fn coalesce_ctrl_key_breaks_run() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press_ctrl(KeyCode::Char('c')),
            press(KeyCode::Enter),
            press(KeyCode::Char('d')),
        ];
        assert_eq!(coalesce_rapid_keys(events).len(), 5);
    }

    #[test]
    fn coalesce_tabs_in_pasted_code() {
        let events = vec![
            press(KeyCode::Char('i')),
            press(KeyCode::Char('f')),
            press(KeyCode::Enter),
            press(KeyCode::Tab),
            press(KeyCode::Char('x')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("if\n\tx".to_string()));
    }

    #[test]
    fn coalesce_exactly_at_threshold() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("a\nb".to_string()));
    }

    #[test]
    fn coalesce_type_then_submit_not_coalesced() {
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Char('c')),
            press(KeyCode::Enter),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 4);
        assert!(matches!(&result[3].event, Event::Key(ke) if ke.code == KeyCode::Enter));
    }

    #[test]
    fn fragmented_paste_merged_with_keys() {
        let events = vec![
            timed(Event::Paste("real paste".into()), Instant::now()),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("real pastea\nb".to_string()));
    }

    #[test]
    fn coalesce_single_event_passthrough() {
        let result = coalesce_rapid_keys(vec![press(KeyCode::Enter)]);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0].event, Event::Key(_)));
    }

    #[test]
    fn coalesce_empty_input() {
        assert!(coalesce_rapid_keys(vec![]).is_empty());
    }

    #[test]
    fn coalesce_three_lines() {
        let events = "foo\nbar\nbaz"
            .chars()
            .map(|character| {
                press(if character == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(character)
                })
            })
            .collect();
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("foo\nbar\nbaz".to_string()));
    }

    #[test]
    fn coalesce_four_lines_trailing_newline() {
        let events = "a\nb\nc\nd\n"
            .chars()
            .map(|character| {
                press(if character == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(character)
                })
            })
            .collect();
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("a\nb\nc\nd\n".to_string()));
    }

    #[test]
    fn extend_triggered_with_single_pasteable_key() {
        assert!(should_extend_for_paste(&[press(KeyCode::Char('a'))]));
    }

    #[test]
    fn extend_triggered_with_enter_key() {
        assert!(should_extend_for_paste(&[press(KeyCode::Enter)]));
    }

    #[test]
    fn extend_not_triggered_with_bracketed_paste() {
        let events = vec![
            timed(Event::Paste("hello".into()), Instant::now()),
            press(KeyCode::Char('a')),
            press(KeyCode::Enter),
            press(KeyCode::Char('b')),
        ];
        assert!(!should_extend_for_paste(&events));
    }

    #[test]
    fn extend_not_triggered_with_only_non_pasteable() {
        assert!(!should_extend_for_paste(&[timed(
            Event::Resize(80, 24),
            Instant::now(),
        )]));
    }

    #[test]
    fn merge_paste_and_key_fragments() {
        let events = vec![
            timed(Event::Paste("hello\nwor".into()), Instant::now()),
            press(KeyCode::Char('l')),
            press(KeyCode::Char('d')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("hello\nworld".to_string()));
    }

    #[test]
    fn merge_multiple_paste_fragments() {
        let events = vec![
            timed(Event::Paste("aa\n".into()), Instant::now()),
            timed(Event::Paste("bb\n".into()), Instant::now()),
            press(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("aa\nbb\nc".to_string()));
    }

    #[test]
    fn merge_preserves_non_key_events() {
        let events = vec![
            timed(Event::Paste("hello".into()), Instant::now()),
            timed(Event::Resize(80, 24), Instant::now()),
            press(KeyCode::Char('x')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].event, Event::Paste("hello".to_string()));
        assert!(matches!(result[1].event, Event::Resize(80, 24)));
        assert_eq!(result[2].event, Event::Paste("x".to_string()));
    }

    #[test]
    fn merge_skips_release_events() {
        let events = vec![
            timed(Event::Paste("ab".into()), Instant::now()),
            press(KeyCode::Char('c')),
            release(KeyCode::Char('c')),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("abc".to_string()));
    }

    #[test]
    fn pure_paste_no_merge_needed() {
        let result = coalesce_rapid_keys(vec![timed(
            Event::Paste("hello\nworld".into()),
            Instant::now(),
        )]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste("hello\nworld".to_string()));
    }

    #[test]
    fn pasteable_rejects_mouse_events() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        for event in [
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        ] {
            assert!(!is_pasteable_key_event(&event));
        }
    }

    #[test]
    fn pasteable_rejects_focus_events() {
        assert!(!is_pasteable_key_event(&Event::FocusGained));
        assert!(!is_pasteable_key_event(&Event::FocusLost));
    }

    #[test]
    fn pasteable_rejects_release_events() {
        assert!(!is_pasteable_key_event(&release(KeyCode::Char('a')).event));
        assert!(!is_pasteable_key_event(&release(KeyCode::Enter).event));
    }

    #[test]
    fn pasteable_rejects_resize() {
        assert!(!is_pasteable_key_event(&Event::Resize(80, 24)));
    }

    #[test]
    fn pasteable_rejects_repeat_events() {
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        });
        assert!(!is_pasteable_key_event(&event));
    }

    #[test]
    fn pasteable_accepts_valid_key_presses() {
        assert!(is_pasteable_key_event(&press(KeyCode::Char('a')).event));
        assert!(is_pasteable_key_event(
            &press_shift(KeyCode::Char('A')).event
        ));
        assert!(is_pasteable_key_event(&press(KeyCode::Enter).event));
        assert!(is_pasteable_key_event(&press(KeyCode::Tab).event));
    }

    #[test]
    fn extend_not_triggered_with_only_mouse_and_focus() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            timed(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 10,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            ),
            timed(Event::FocusGained, Instant::now()),
        ];
        assert!(!should_extend_for_paste(&events));
    }

    #[test]
    fn extend_triggered_only_when_key_present_in_mixed_batch() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            timed(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            ),
            press(KeyCode::Char('a')),
            timed(Event::FocusLost, Instant::now()),
        ];
        assert!(should_extend_for_paste(&events));
    }

    #[test]
    fn coalesce_mouse_events_interleaved_with_paste_chars() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            press(KeyCode::Char('a')),
            timed(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 10,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            ),
            timed(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 11,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            ),
        ];
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0].event, Event::Key(ke) if ke.code == KeyCode::Char('a')));
        assert!(matches!(&result[1].event, Event::Mouse(_)));
        assert!(matches!(&result[2].event, Event::Mouse(_)));
    }

    #[test]
    fn coalesce_mouse_breaks_key_run_preserves_events() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let events = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Enter),
            timed(
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 5,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                }),
                Instant::now(),
            ),
            press(KeyCode::Char('c')),
        ];
        assert_eq!(coalesce_rapid_keys(events).len(), 5);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_matches_each_anchor() {
        let press_run = |text: &str| {
            text.chars()
                .map(|character| press(KeyCode::Char(character)))
                .collect::<Vec<_>>()
        };
        for input in [
            r"C:\foo.png",
            "C:/foo.png",
            r"\\srv\share\a.png",
            "/Users/a/b.png",
            "file:///tmp/x.png",
            "\"C:\\My Pics\\a.png\"",
        ] {
            let result = coalesce_rapid_keys(press_run(input));
            assert_eq!(result.len(), 1, "input {input:?} should coalesce");
            assert_eq!(result[0].event, Event::Paste(input.to_string()));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_rejects_short_or_non_path() {
        let press_run = |text: &str| {
            text.chars()
                .map(|character| press(KeyCode::Char(character)))
                .collect::<Vec<_>>()
        };
        for input in ["/foo.tx", "helloworld"] {
            assert!(
                coalesce_rapid_keys(press_run(input))
                    .iter()
                    .all(|event| matches!(event.event, Event::Key(_)))
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn coalesce_path_shape_handles_shift_modifier() {
        let mut events = vec![press(KeyCode::Char('C'))];
        events.push(press_shift(KeyCode::Char(':')));
        events.extend(
            r"\foo.png"
                .chars()
                .map(|character| press(KeyCode::Char(character))),
        );
        let result = coalesce_rapid_keys(events);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].event, Event::Paste(r"C:\foo.png".to_string()));
    }

    #[test]
    fn normalization_preserves_reader_timestamp() {
        let arrived_at = Instant::now();
        let routed = normalize_input_event(timed(Event::FocusGained, arrived_at));
        assert_eq!(routed.event, Event::FocusGained);
        assert_eq!(routed.arrived_at, arrived_at);
        assert_eq!(routed.paste_provenance, PasteProvenance::Terminal);
    }

    #[test]
    fn only_terminal_paste_may_probe_unrelated_clipboard_attachments() {
        assert!(PasteProvenance::Terminal.may_probe_clipboard_attachments());
        assert!(!PasteProvenance::X11Primary.may_probe_clipboard_attachments());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unmodified_middle_down_reads_x11_primary_once() {
        use crossterm::event::{MouseButton, MouseEventKind};

        crate::tui_clipboard::set_primary_selection_test_hook(
            true,
            Some("PRIMARY\nexact".to_owned()),
        );
        let input = mouse(
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::NONE,
        );
        let arrived_at = input.arrived_at;
        let routed = normalize_input_event(input);

        assert_eq!(routed.event, Event::Paste("PRIMARY\nexact".to_owned()));
        assert_eq!(routed.arrived_at, arrived_at);
        assert_eq!(routed.paste_provenance, PasteProvenance::X11Primary);
        assert_eq!(crate::tui_clipboard::primary_selection_read_call_count(), 1);
        crate::tui_clipboard::clear_primary_selection_test_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nonqualifying_middle_events_preserve_event_without_primary_read() {
        use crossterm::event::{MouseButton, MouseEventKind};

        crate::tui_clipboard::set_primary_selection_test_hook(true, Some("PRIMARY".to_owned()));
        for input in [
            mouse(MouseEventKind::Up(MouseButton::Middle), KeyModifiers::NONE),
            mouse(
                MouseEventKind::Down(MouseButton::Middle),
                KeyModifiers::SHIFT,
            ),
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
        ] {
            let expected = input.clone();
            let routed = normalize_input_event(input);
            assert_eq!(routed.event, expected.event);
            assert_eq!(routed.arrived_at, expected.arrived_at);
            assert_eq!(routed.paste_provenance, PasteProvenance::Terminal);
        }
        assert_eq!(crate::tui_clipboard::primary_selection_read_call_count(), 0);
        crate::tui_clipboard::clear_primary_selection_test_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_or_empty_primary_preserves_middle_event() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let middle = mouse(
            MouseEventKind::Down(MouseButton::Middle),
            KeyModifiers::NONE,
        );
        crate::tui_clipboard::set_primary_selection_test_hook(false, None);
        let routed = normalize_input_event(middle.clone());
        assert_eq!(routed.event, middle.event);
        assert_eq!(routed.paste_provenance, PasteProvenance::Terminal);
        assert_eq!(crate::tui_clipboard::primary_selection_read_call_count(), 0);

        crate::tui_clipboard::set_primary_selection_test_hook(true, Some(String::new()));
        let routed = normalize_input_event(middle.clone());
        assert_eq!(routed.event, middle.event);
        assert_eq!(routed.paste_provenance, PasteProvenance::Terminal);
        assert_eq!(crate::tui_clipboard::primary_selection_read_call_count(), 1);
        crate::tui_clipboard::clear_primary_selection_test_hook();
    }
}
