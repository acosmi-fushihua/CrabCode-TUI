//! Terminal ownership, screen-mode selection, and native-scrollback commits.
//!
//! Terminal buffering, inline viewport mutation, native scrollback insertion,
//! hyperlink-aware differential output, and resize behavior are owned by the
//! fixed upstream lifecycle foundation in `crabcode-ratatui-inline`. CrabCode
//! keeps only the product-specific terminal setup and backend adapter here.

// This module is the narrow OS terminal-ownership boundary. Every unsafe site
// calls a documented libc descriptor/termios primitive and carries a local
// SAFETY invariant; keeping the allowance here prevents unsafe code from
// spreading into renderer state, projection, or backend adapters.
#![allow(unsafe_code)]

use std::ffi::OsString;
#[cfg(unix)]
use std::io::Read as _;
use std::io::{self, IsTerminal as _};
use std::process::{Command, Stdio};
use std::sync::Once;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crabcode_pager_render::audited_theme::CrabCodeThemeKind;
use crabcode_ratatui_inline::{LinkSpan, Terminal};
use crossterm::cursor::{Hide, MoveTo, Show};
#[cfg(windows)]
use crossterm::event::DisableMouseCapture;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, ScrollUp,
    SetTitle, disable_raw_mode, enable_raw_mode,
};
#[cfg(test)]
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget as _};
#[cfg(test)]
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{TerminalOptions, Viewport};
use tokio::sync::Notify;

#[cfg(test)]
use crate::frame_transaction::CursorAction;
use crate::frame_transaction::{CursorState, draw_frame as present_terminal_frame};
#[cfg(test)]
use crate::sdk_projection::ProjectedItem;
use crate::terminal_capabilities::{
    HyperlinkRoute, MultiplexerKind, TerminalContextExt as _, terminal_context,
};
use crate::terminal_writer::{
    CrabCodeFrameWriter, CrabCodeWriterThread, EmergencyOutputFence, TerminalControlLease,
    WriterDrain, WriterEvent, WriterSync, install_terminal_control_sender,
    lock_terminal_output_for_active_write, lock_terminal_output_for_restore, spawn_terminal_writer,
    terminal_writer_emergency_stopped, writer_event_channel,
};
use crate::tui_app::{InitialSessionRequest, TuiApp, UiLanguage};
use crate::tui_render::{CrabCodeTheme, write_osc8_close};

const LEGACY_FULLSCREEN_ENV: &str = "CRABCODE_NO_FLICKER";
const LEGACY_USER_TYPE_ENV: &str = "USER_TYPE";
const LEGACY_DISABLE_MOUSE_ENV: &str = "CRABCODE_DISABLE_MOUSE";
const LEGACY_DISABLE_MOUSE_CLICKS_ENV: &str = "CRABCODE_DISABLE_MOUSE_CLICKS";
const LEGACY_DISABLE_TERMINAL_TITLE_ENV: &str = "CRABCODE_DISABLE_TERMINAL_TITLE";
const ENSURE_CRON_DAEMON_FLAG: &str = "--ensure-cron-daemon";
const MIN_TERMINAL_COLUMNS: u16 = 20;
const MIN_TERMINAL_ROWS: u16 = 8;
const MINIMAL_WELCOME_MIN_WIDTH: u16 = 8;
const MOUSE_TRACKING_RESET: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l";
const MOUSE_PASTE_RESET: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[?2004l";

static PANIC_HOOK: Once = Once::new();
static TERMINAL_SESSION_CLAIMED: AtomicBool = AtomicBool::new(false);
static TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);
static EMERGENCY_TERMINAL_PHASE: AtomicU8 = AtomicU8::new(EmergencyTerminalPhase::Restored as u8);
static KEYBOARD_ENHANCEMENT_PUSHED: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);
static TERMINAL_TITLE_CHANGED: AtomicBool = AtomicBool::new(false);
static CURSOR_COLOR_CHANGED: AtomicBool = AtomicBool::new(false);
static ACTIVE_MODE: AtomicU8 = AtomicU8::new(TerminalMode::Fullscreen as u8);
static TERMINAL_ACQUISITION_MUTATION: AtomicU8 =
    AtomicU8::new(TerminalAcquisitionMutation::Idle as u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TerminalAcquisitionMutation {
    Idle = 0,
    Kernel = 1,
    Setup = 2,
}

struct TerminalAcquisitionMutationGuard<'a> {
    state: &'a AtomicU8,
}

impl TerminalAcquisitionMutationGuard<'static> {
    fn begin(kind: TerminalAcquisitionMutation) -> io::Result<Self> {
        Self::begin_with(&TERMINAL_ACQUISITION_MUTATION, kind, || {
            terminal_writer_emergency_stopped()
        })
    }
}

impl<'a> TerminalAcquisitionMutationGuard<'a> {
    fn begin_with(
        state: &'a AtomicU8,
        kind: TerminalAcquisitionMutation,
        emergency_stopped: impl Fn() -> bool,
    ) -> io::Result<Self> {
        debug_assert_ne!(kind, TerminalAcquisitionMutation::Idle);
        if emergency_stopped() {
            return Err(terminal_protocol_emergency_error());
        }
        state
            .compare_exchange(
                TerminalAcquisitionMutation::Idle as u8,
                kind as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another terminal protocol acquisition mutation is active",
                )
            })?;
        let guard = Self { state };
        // Close the check/CAS race: an emergency that published its latch
        // immediately before or after our CAS makes this mutation fail before
        // touching the terminal.
        if emergency_stopped() {
            drop(guard);
            return Err(terminal_protocol_emergency_error());
        }
        Ok(guard)
    }

    fn finish_after_kernel_mutation(
        self,
        emergency_stopped: impl Fn() -> bool,
        rollback: impl FnOnce(),
    ) -> io::Result<()> {
        if emergency_stopped() {
            // Keep Kernel published until rollback is complete. A contended
            // fatal restorer waits only for this output-free critical section,
            // then independently reapplies the immutable kernel snapshot.
            rollback();
            drop(self);
            return Err(terminal_protocol_emergency_error());
        }
        drop(self);
        Ok(())
    }
}

impl Drop for TerminalAcquisitionMutationGuard<'_> {
    fn drop(&mut self) {
        self.state
            .store(TerminalAcquisitionMutation::Idle as u8, Ordering::SeqCst);
    }
}

fn terminal_protocol_emergency_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal protocol cannot mutate after emergency restoration",
    )
}

fn wait_for_kernel_acquisition_rollback(fence: EmergencyOutputFence) {
    wait_for_kernel_acquisition_rollback_with(&TERMINAL_ACQUISITION_MUTATION, fence);
}

fn wait_for_kernel_acquisition_rollback_with(state: &AtomicU8, fence: EmergencyOutputFence) {
    if fence != EmergencyOutputFence::Contended {
        return;
    }
    // Kernel acquisition contains only raw-mode/console-mode calls and never
    // writes terminal output. Waiting here cannot inherit the unbounded
    // stdout-writer stall that the Contended fence was designed to avoid.
    while state.load(Ordering::SeqCst) == TerminalAcquisitionMutation::Kernel as u8 {
        std::thread::yield_now();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum EmergencyTerminalPhase {
    Restored = 0,
    ProtocolActive = 1,
    ChildHandoff = 2,
}

impl EmergencyTerminalPhase {
    fn current() -> Self {
        match EMERGENCY_TERMINAL_PHASE.load(Ordering::Acquire) {
            value if value == Self::ProtocolActive as u8 => Self::ProtocolActive,
            value if value == Self::ChildHandoff as u8 => Self::ChildHandoff,
            _ => Self::Restored,
        }
    }

    fn publish(self) {
        #[cfg(windows)]
        crate::terminal_fault::set_protocol_restore_enabled(
            self == EmergencyTerminalPhase::ProtocolActive,
        );
        EMERGENCY_TERMINAL_PHASE.store(self as u8, Ordering::Release);
    }
}
#[cfg(unix)]
struct EmergencyTerminalBaseline {
    termios: libc::termios,
    termios_fd: i32,
    output_fd: i32,
}
#[cfg(unix)]
static EMERGENCY_TERMINAL_BASELINE: std::sync::RwLock<Option<EmergencyTerminalBaseline>> =
    std::sync::RwLock::new(None);

#[cfg(unix)]
impl Drop for EmergencyTerminalBaseline {
    fn drop(&mut self) {
        // SAFETY: one capsule exclusively owns both descriptors. Replacement
        // happens under EMERGENCY_TERMINAL_BASELINE while no protocol
        // generation is active; emergency readers use only try_read guards.
        unsafe {
            libc::close(self.termios_fd);
            if self.output_fd >= 0 {
                libc::close(self.output_fd);
            }
        }
    }
}

/// Fixed-upstream crash restore bytes. Emergency paths use this only through a
/// separately opened nonblocking tty descriptor and only after the ordered
/// output gate is proven quiescent.
pub(crate) const FATAL_FAULT_TERMINAL_RESTORE: &[u8] =
    b"\x1b[?2026l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[?1049l";
const EMERGENCY_TERMINAL_RESTORE: &[u8] = FATAL_FAULT_TERMINAL_RESTORE;
const EMERGENCY_SYNC_CURSOR_RESTORE: &[u8] = b"\x1b[?2026l\x1b[?25h";
const EMERGENCY_PASTE_FOCUS_RESTORE: &[u8] = b"\x1b[?2004l\x1b[?1004l";
const EMERGENCY_KEYBOARD_RESTORE: &[u8] = b"\x1b[<u";
const EMERGENCY_ALT_SCREEN_RESTORE: &[u8] = b"\x1b[?1049l";
const EMERGENCY_CURSOR_COLOR_RESET: &[u8] = b"\x1b]112\x07";
const EMERGENCY_TITLE_CLEAR: &[u8] = b"\x1b]0;\x07";

pub type TuiTerminal = Terminal<CrosstermBackend<CrabCodeFrameWriter>>;

pub(crate) trait SynchronizedFrameBackend: ratatui::backend::Backend + io::Write {
    fn frame_writer_mut(&mut self) -> &mut CrabCodeFrameWriter;
}

impl SynchronizedFrameBackend for CrosstermBackend<CrabCodeFrameWriter> {
    fn frame_writer_mut(&mut self) -> &mut CrabCodeFrameWriter {
        self.writer_mut()
    }
}

/// The three terminal presentation contracts.
///
/// `Inline` and `Minimal` both preserve the main screen and shell scrollback.
/// `Minimal` additionally commits finalized transcript items into native
/// scrollback and keeps only current work in its live viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalMode {
    Fullscreen = 0,
    Inline = 1,
    Minimal = 2,
}

impl TerminalMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Inline => "inline",
            Self::Minimal => "minimal",
        }
    }

    const fn localized_label(self, language: UiLanguage) -> &'static str {
        match language {
            UiLanguage::ZhCn => match self {
                Self::Fullscreen => "全屏",
                Self::Inline => "内联",
                Self::Minimal => "精简",
            },
            UiLanguage::EnUs => self.label(),
        }
    }

    const fn uses_alternate_screen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }

    const fn is_fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }

    fn from_atomic(value: u8) -> Self {
        match value {
            1 => Self::Inline,
            2 => Self::Minimal,
            _ => Self::Fullscreen,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalFallback {
    MinimalToInline,
    MinimalToFixed,
    InlineToFixed,
}

impl TerminalFallback {
    const fn notice(self, language: UiLanguage) -> &'static str {
        match language {
            UiLanguage::ZhCn => match self {
                Self::MinimalToInline => "精简内联视口能力探测失败；正在使用全高度内联模式",
                Self::MinimalToFixed => "精简和全高度内联视口能力探测均失败；正在使用固定内联视口",
                Self::InlineToFixed => "内联视口能力探测失败；正在使用固定内联视口",
            },
            UiLanguage::EnUs => match self {
                Self::MinimalToInline => {
                    "minimal inline viewport capability probe failed; using full-height inline mode"
                }
                Self::MinimalToFixed => {
                    "minimal and full-height inline viewport probes failed; using a fixed inline viewport"
                }
                Self::InlineToFixed => {
                    "inline viewport capability probe failed; using a fixed inline viewport"
                }
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalViewportAttempt {
    Fullscreen,
    Inline(u16),
    Fixed(Rect),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSetupAttempt {
    viewport: TerminalViewportAttempt,
    effective_mode: TerminalMode,
    fallback: Option<TerminalFallback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModePreference {
    Fullscreen,
    Inline,
    Minimal,
}

impl ModePreference {
    const fn label(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Inline => "no-alt-screen",
            Self::Minimal => "minimal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalModeSource {
    Cli,
    Environment,
    Auto,
}

impl TerminalModeSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cli => "CLI",
            Self::Environment => "environment",
            Self::Auto => "auto detection",
        }
    }

    const fn localized_label(self, language: UiLanguage) -> &'static str {
        match language {
            UiLanguage::ZhCn => match self {
                Self::Cli => "CLI",
                Self::Environment => "环境变量",
                Self::Auto => "自动检测",
            },
            UiLanguage::EnUs => self.label(),
        }
    }
}

/// Fully resolved terminal plan. Resolution happens before direct-runtime
/// startup and before raw mode, so invalid terminal arguments/environment
/// remain ordinary shell errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPlan {
    pub mode: TerminalMode,
    pub source: TerminalModeSource,
    pub reason: String,
    reason_zh_cn: String,
    /// Historical CrabCode fullscreen-only mouse escape hatch. Inline keeps
    /// the fixed Rust renderer's mouse lifecycle; minimal never captures.
    pub(crate) disable_fullscreen_mouse_tracking: bool,
    /// Historical fullscreen click/drag gate. Wheel input remains enabled.
    pub(crate) mouse_clicks_disabled: bool,
    /// Explicit renderer-owned CLI override. `None` means no CLI override was
    /// supplied; it must not be interpreted as `false` because the effective
    /// legacy value still depends on the authoritative CrabCode config.
    pub presentation_verbose_override: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessOptions {
    pub terminal_plan: TerminalPlan,
    pub initial_session: InitialSessionRequest,
    pub initial_prompt: Option<String>,
    pub composer_prefill: Option<String>,
    /// Existing historical CLI switch. This is an outer-renderer fact as well
    /// as a child argument: an empty backend catalog alone cannot prevent the
    /// native renderer from exposing its fixed local slash fallbacks before
    /// initialize completes.
    pub slash_commands_enabled: bool,
    pub(crate) launch_provenance: LaunchProvenance,
    pub runtime_args: Vec<OsString>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct LaunchProvenance {
    deep_link_origin: bool,
    deep_link_repo: Option<String>,
    deep_link_last_fetch_ms: Option<f64>,
    prefill_source_length: Option<usize>,
}

impl TerminalPlan {
    pub(crate) fn summary(&self, language: UiLanguage) -> String {
        match language {
            UiLanguage::ZhCn => format!(
                "终端模式：{} → {}（{}）",
                self.source.localized_label(language),
                self.mode.localized_label(language),
                self.reason_zh_cn
            ),
            UiLanguage::EnUs => format!(
                "terminal {} selected by {} ({})",
                self.mode.localized_label(language),
                self.source.localized_label(language),
                self.reason
            ),
        }
    }

    pub(crate) fn effective_summary(
        &self,
        language: UiLanguage,
        active_mode: TerminalMode,
    ) -> String {
        let summary = self.summary(language);
        if active_mode == self.mode {
            return summary;
        }
        match language {
            UiLanguage::ZhCn => format!(
                "{summary} · 当前回退为{}模式",
                active_mode.localized_label(language)
            ),
            UiLanguage::EnUs => format!(
                "{summary} · active {} fallback",
                active_mode.localized_label(language)
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalSetupError {
    #[error("conflicting terminal mode arguments: `{first}` and `{second}`")]
    ConflictingModes { first: String, second: String },
    #[error("`--prefill` requires prompt text")]
    MissingPrefill,
    #[error("`--prefill` prompt text must be valid UTF-8")]
    InvalidPrefillEncoding,
    #[error(
        "`--prefill` contains {actual} bytes after trimming; the native composer limit is {maximum} bytes"
    )]
    PrefillTooLarge { actual: usize, maximum: usize },
    #[error("`--deep-link-repo` requires a repository slug")]
    MissingDeepLinkRepo,
    #[error("`--deep-link-repo` must be valid UTF-8")]
    InvalidDeepLinkRepoEncoding,
    #[error("`--deep-link-last-fetch` requires a millisecond timestamp")]
    MissingDeepLinkLastFetch,
    #[error("`--deep-link-last-fetch` must be valid UTF-8")]
    InvalidDeepLinkLastFetchEncoding,
    #[error("only one positional prompt is accepted by the interactive TUI")]
    MultiplePrompts,
    #[error("TERM=dumb cannot safely host an interactive Rust TUI")]
    DumbTerminal,
    #[error(
        "`--ensure-cron-daemon` is a process-owned lifecycle route and cannot be combined with other arguments"
    )]
    InvalidCronEnsureInvocation,
    #[error("`{option}` is unavailable in the pure direct TUI: {reason}")]
    ExcludedSurfaceOption {
        option: &'static str,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreTerminalAction {
    EnsureCronDaemon,
}

/// Resolve process-owned lifecycle work before TERM inspection, direct-runtime
/// startup, or raw-mode acquisition.
pub(crate) fn resolve_pre_terminal_action(
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<Option<PreTerminalAction>, TerminalSetupError> {
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let has_ensure = args
        .iter()
        .skip(1)
        .any(|argument| argument == ENSURE_CRON_DAEMON_FLAG);
    if !has_ensure {
        return Ok(None);
    }
    if args.len() == 2 && args[1] == ENSURE_CRON_DAEMON_FLAG {
        return Ok(Some(PreTerminalAction::EnsureCronDaemon));
    }
    Err(TerminalSetupError::InvalidCronEnsureInvocation)
}

/// Parse process arguments and environment, then apply conservative detection.
///
/// Returns `Ok(None)` after printing `--help`/`--version`.
pub fn resolve_process_options() -> Result<Option<ProcessOptions>, TerminalSetupError> {
    let cli = parse_cli_mode(std::env::args_os())?;
    if cli.print_help {
        print_help();
        return Ok(None);
    }
    if cli.print_version {
        println!("crabcode-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(None);
    }

    let mut terminal_plan = resolve_mode_inputs(cli.preference, DetectionFacts::from_process())?;
    terminal_plan.presentation_verbose_override = cli.presentation_verbose_override;
    Ok(Some(ProcessOptions {
        terminal_plan,
        initial_session: cli.initial_session,
        initial_prompt: cli.initial_prompt,
        composer_prefill: cli.composer_prefill,
        slash_commands_enabled: !cli.disable_slash_commands,
        launch_provenance: cli.launch_provenance,
        runtime_args: cli.runtime_args,
    }))
}

/// Preserve the fixed historical interactive route when the initial prompt is
/// piped on Unix: consume the complete pipe, merge it after the positional
/// prompt, then make the controlling terminal the renderer's stdin.
///
/// This runs before the direct runtime is spawned and owns no backend field or
/// wire message. Windows has no historical `/dev/tty` override and therefore
/// retains the ordinary interactive-stdin requirement.
pub(crate) fn prepare_interactive_stdin(
    initial_prompt: Option<String>,
) -> io::Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(initial_prompt);
    }
    #[cfg(unix)]
    {
        if env_value_is_truthy(std::env::var("CI").ok().as_deref()) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "piped stdin cannot drive the interactive TUI in CI",
            ));
        }
        prepare_unix_piped_interactive_stdin(initial_prompt)
    }
    #[cfg(not(unix))]
    {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive stdin must be attached to a terminal on this platform",
        ))
    }
}

#[cfg(unix)]
fn prepare_unix_piped_interactive_stdin(
    initial_prompt: Option<String>,
) -> io::Result<Option<String>> {
    use std::os::fd::AsFd as _;

    use nix::errno::Errno;
    use nix::poll::{PollFd, PollFlags, poll};

    // Open and validate the replacement before consuming the only copy of the
    // pipe. O_NOCTTY cannot create or change a controlling-terminal relation.
    // SAFETY: the path is a static NUL-terminated C string.
    let tty_fd = unsafe {
        libc::open(
            c"/dev/tty".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if tty_fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "cannot open /dev/tty for piped interactive input: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: this branch owns tty_fd until the final dup/close transaction.
    if unsafe { libc::isatty(tty_fd) } != 1 {
        // SAFETY: this branch owns tty_fd.
        unsafe {
            libc::close(tty_fd);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "/dev/tty is not an interactive terminal",
        ));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let stdin = io::stdin();
    let timed_out = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break true;
        }
        let remaining_millis = deadline
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, u128::from(u16::MAX)) as u16;
        let mut descriptors = [PollFd::new(
            stdin.as_fd(),
            PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
        )];
        match poll(&mut descriptors, remaining_millis) {
            Ok(0) => break true,
            Ok(_) => {
                if descriptors[0]
                    .revents()
                    .is_some_and(|events| events.contains(PollFlags::POLLNVAL))
                {
                    // SAFETY: this branch owns tty_fd.
                    unsafe {
                        libc::close(tty_fd);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "piped stdin became invalid before it could be read",
                    ));
                }
                break false;
            }
            Err(Errno::EINTR) => continue,
            Err(error) => {
                // SAFETY: this branch owns tty_fd.
                unsafe {
                    libc::close(tty_fd);
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
    };

    let mut piped = Vec::new();
    if timed_out {
        eprintln!(
            "Warning: no stdin data received in 3s, proceeding without it. \
             If piping from a slow command, redirect stdin explicitly: \
             < /dev/null to skip, or wait longer."
        );
    } else if let Err(error) = stdin.lock().read_to_end(&mut piped) {
        // SAFETY: this branch owns tty_fd.
        unsafe {
            libc::close(tty_fd);
        }
        return Err(error);
    }

    // Replace only this renderer process's fd 0. Its private StructuredIO
    // child receives an explicit pipe from sdk_runtime and is unaffected.
    // SAFETY: tty_fd is a validated open terminal descriptor and dup2 targets
    // the process's conventional stdin descriptor.
    if unsafe { libc::dup2(tty_fd, libc::STDIN_FILENO) } < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: this branch owns tty_fd.
        unsafe {
            libc::close(tty_fd);
        }
        return Err(error);
    }
    // SAFETY: fd 0 now owns the duplicated reference.
    unsafe {
        libc::close(tty_fd);
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "restored /dev/tty stdin failed terminal validation",
        ));
    }

    Ok(merge_historical_piped_prompt(
        initial_prompt,
        String::from_utf8_lossy(&piped).as_ref(),
    ))
}

fn merge_historical_piped_prompt(initial_prompt: Option<String>, piped: &str) -> Option<String> {
    let mut parts = Vec::with_capacity(2);
    if let Some(prompt) = initial_prompt.filter(|prompt| !prompt.is_empty()) {
        parts.push(prompt);
    }
    if !piped.is_empty() {
        parts.push(piped.to_string());
    }
    let merged = parts.join("\n");
    (!merged.is_empty()).then_some(merged)
}

#[derive(Debug, Default, Clone, PartialEq)]
struct CliMode {
    preference: Option<ModePreference>,
    initial_session: InitialSessionRequest,
    initial_prompt: Option<String>,
    composer_prefill: Option<String>,
    launch_provenance: LaunchProvenance,
    presentation_verbose_override: Option<bool>,
    disable_slash_commands: bool,
    continue_requested: bool,
    runtime_args: Vec<OsString>,
    print_help: bool,
    print_version: bool,
}

fn parse_cli_mode(
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<CliMode, TerminalSetupError> {
    let mut args = args.into_iter().map(Into::into).peekable();
    let _binary = args.next();
    let mut parsed = CliMode::default();
    while let Some(argument) = args.next() {
        if argument.to_str() == Some("--") {
            let remaining = args.collect::<Vec<_>>();
            for reserved in &remaining {
                if let Some(value) = reserved.to_str()
                    && let Some(error) = excluded_surface_option(value, true)
                {
                    return Err(error);
                }
            }
            parsed.runtime_args.extend(remaining);
            break;
        }
        let Some(argument_text) = argument.to_str() else {
            parsed.runtime_args.push(argument);
            continue;
        };
        if let Some(error) = excluded_surface_option(argument_text, false) {
            return Err(error);
        }
        match argument_text {
            "-h" | "--help" => parsed.print_help = true,
            "-V" | "--version" => parsed.print_version = true,
            "--verbose" => parsed.presentation_verbose_override = Some(true),
            "--disable-slash-commands" => {
                parsed.disable_slash_commands = true;
                // Preserve the existing child option unchanged. The Rust-side
                // boolean controls presentation only and creates no second
                // command or protocol authority.
                parsed.runtime_args.push(argument);
            }
            "--fullscreen" => {
                set_cli_preference(
                    &mut parsed.preference,
                    ModePreference::Fullscreen,
                    argument_text,
                )?;
            }
            "--no-alt-screen" => {
                set_cli_preference(
                    &mut parsed.preference,
                    ModePreference::Inline,
                    argument_text,
                )?;
            }
            "--minimal" => {
                set_cli_preference(
                    &mut parsed.preference,
                    ModePreference::Minimal,
                    argument_text,
                )?;
            }
            "-c" | "--continue" => {
                parsed.continue_requested = true;
                parsed.initial_session = InitialSessionRequest::Continue;
                remove_runtime_resume_arguments(&mut parsed.runtime_args);
                parsed.runtime_args.push(argument);
            }
            "-r" | "--resume" => {
                let supplied_value = args
                    .peek()
                    .and_then(|next| next.to_str())
                    .filter(|next| !next.starts_with('-'))
                    .map(str::to_string);
                let consumed = supplied_value
                    .as_ref()
                    .map(|_| args.next().expect("peeked resume value exists"));
                if parsed.continue_requested {
                    continue;
                }
                remove_runtime_resume_arguments(&mut parsed.runtime_args);
                set_resume_selection(
                    &mut parsed,
                    supplied_value.as_deref(),
                    Some(argument),
                    consumed,
                );
            }
            value if value.starts_with("--resume=") => {
                let supplied_value = value
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                if parsed.continue_requested {
                    continue;
                }
                remove_runtime_resume_arguments(&mut parsed.runtime_args);
                set_resume_selection(
                    &mut parsed,
                    (!supplied_value.is_empty()).then_some(supplied_value),
                    Some(argument.clone()),
                    None,
                );
            }
            "--prefill" => {
                let value = args.next().ok_or(TerminalSetupError::MissingPrefill)?;
                let value = value
                    .to_str()
                    .ok_or(TerminalSetupError::InvalidPrefillEncoding)?;
                set_composer_prefill(&mut parsed, value)?;
            }
            value if value.starts_with("--prefill=") => {
                let value = value
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                set_composer_prefill(&mut parsed, value)?;
            }
            "--deep-link-origin" => {
                parsed.launch_provenance.deep_link_origin = true;
            }
            "--deep-link-repo" => {
                let value = args.next().ok_or(TerminalSetupError::MissingDeepLinkRepo)?;
                let value = value
                    .to_str()
                    .ok_or(TerminalSetupError::InvalidDeepLinkRepoEncoding)?;
                parsed.launch_provenance.deep_link_repo = Some(value.to_string());
            }
            value if value.starts_with("--deep-link-repo=") => {
                let value = value
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                parsed.launch_provenance.deep_link_repo = Some(value.to_string());
            }
            "--deep-link-last-fetch" => {
                let value = args
                    .next()
                    .ok_or(TerminalSetupError::MissingDeepLinkLastFetch)?;
                let value = value
                    .to_str()
                    .ok_or(TerminalSetupError::InvalidDeepLinkLastFetchEncoding)?;
                parsed.launch_provenance.deep_link_last_fetch_ms =
                    parse_legacy_optional_milliseconds(value);
            }
            value if value.starts_with("--deep-link-last-fetch=") => {
                let value = value
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                parsed.launch_provenance.deep_link_last_fetch_ms =
                    parse_legacy_optional_milliseconds(value);
            }
            "--from-pr" => {
                unreachable!("--from-pr is rejected by excluded_surface_option")
            }
            value if value.starts_with("--from-pr=") => {
                unreachable!("--from-pr is rejected by excluded_surface_option")
            }
            value if runtime_option_requires_value(value) => {
                parsed.runtime_args.push(argument);
                if let Some(value) = args.next() {
                    parsed.runtime_args.push(value);
                }
            }
            value if runtime_option_requires_variadic_value(value) => {
                parsed.runtime_args.push(argument);
                while args
                    .peek()
                    .and_then(|next| next.to_str())
                    .is_some_and(|next| !next.starts_with('-'))
                {
                    if let Some(value) = args.next() {
                        parsed.runtime_args.push(value);
                    }
                }
            }
            value if runtime_option_accepts_optional_value(value) => {
                parsed.runtime_args.push(argument);
                if args
                    .peek()
                    .and_then(|next| next.to_str())
                    .is_some_and(|next| !next.starts_with('-'))
                    && let Some(value) = args.next()
                {
                    parsed.runtime_args.push(value);
                }
            }
            value if value.starts_with('-') => parsed.runtime_args.push(argument),
            _ => {
                if parsed.initial_prompt.is_some() {
                    return Err(TerminalSetupError::MultiplePrompts);
                }
                parsed.initial_prompt = Some(argument_text.to_string());
            }
        }
    }
    Ok(parsed)
}

fn is_long_option(value: &str, option: &str) -> bool {
    value == option
        || value
            .strip_prefix(option)
            .is_some_and(|suffix| suffix.starts_with('='))
}

/// Classify routes that cannot be user-selected through the pure interactive
/// TUI. The private child necessarily uses some of these spellings as its
/// process-owned StructuredIO contract; accepting the same spellings from the
/// outer argv would either switch products or pretend that a headless option
/// has an interactive rendering meaning.
///
/// Tokens after `--` are checked too. This parser historically removed the
/// separator and forwarded the remainder to the child, so the separator must
/// not bypass the child-reserved argument boundary.
fn excluded_surface_option(value: &str, after_separator: bool) -> Option<TerminalSetupError> {
    let excluded =
        |option, reason| Some(TerminalSetupError::ExcludedSurfaceOption { option, reason });

    if is_long_option(value, "--screen-mode") || is_long_option(value, "--inline") {
        return excluded(
            if value.starts_with("--screen-mode") {
                "--screen-mode"
            } else {
                "--inline"
            },
            "this renderer-owned option has no fixed-upstream or historical CrabCode authority; use --minimal, --fullscreen, or --no-alt-screen",
        );
    }
    if value == "-p" || is_long_option(value, "--print") || is_long_option(value, "--no-print") {
        return excluded(
            "--print",
            "print mode is a non-interactive headless route; crabcode-tui owns only the interactive TUI",
        );
    }
    if is_long_option(value, "--input-format")
        || is_long_option(value, "--output-format")
        || is_long_option(value, "--include-partial-messages")
        || is_long_option(value, "--no-include-partial-messages")
        || is_long_option(value, "--include-hook-events")
        || is_long_option(value, "--no-include-hook-events")
    {
        return excluded(
            if value.starts_with("--input-format") {
                "--input-format"
            } else if value.starts_with("--output-format") {
                "--output-format"
            } else if value.contains("partial-messages") {
                "--include-partial-messages"
            } else {
                "--include-hook-events"
            },
            "the private StructuredIO formats and event stream are process-owned and are not user-configurable TUI presentation options",
        );
    }
    if is_long_option(value, "--sdk-url") {
        return excluded(
            "--sdk-url",
            "remote WebSocket SDK I/O is a different transport route from the required local StructuredIO backend",
        );
    }
    if is_long_option(value, "--assistant") {
        return excluded(
            "--assistant",
            "assistant mode is an Agent SDK daemon/viewer route, not the local interactive TUI",
        );
    }
    if is_long_option(value, "--remote-control") || is_long_option(value, "--rc") {
        return excluded(
            if value.starts_with("--rc") {
                "--rc"
            } else {
                "--remote-control"
            },
            "Remote Control depends on the excluded remote-bridge product surface",
        );
    }
    if is_long_option(value, "--remote") {
        return excluded(
            "--remote",
            "CCR remote execution is a different runtime route from the required local StructuredIO backend",
        );
    }
    if is_long_option(value, "--teleport") {
        return excluded(
            "--teleport",
            "teleport resumes a remote session route rather than the required local StructuredIO backend",
        );
    }
    if is_long_option(value, "--from-pr") {
        return excluded(
            "--from-pr",
            "pull-request session discovery is not part of the existing direct backend protocol",
        );
    }
    if let Some(option) = [
        "--chrome",
        "--no-chrome",
        "--caps",
        "--profile",
        "--session",
    ]
    .into_iter()
    .find(|option| is_long_option(value, option))
    {
        return excluded(
            option,
            "the legacy real-Chrome extension integration is a removed GUI surface; the preserved native browser backend remains independent of the pure TUI",
        );
    }
    if is_long_option(value, "--init-only") {
        return excluded(
            "--init-only",
            "this lifecycle route runs startup hooks and exits without an interactive session",
        );
    }
    if let Some(option) = [
        "--json-schema",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "--max-turns",
        "--max-budget-usd",
        "--task-budget",
        "--no-session-persistence",
        "--resume-session-at",
        "--rewind-files",
        "--fallback-model",
        "--workload",
        "--enable-auth-status",
    ]
    .into_iter()
    .find(|option| is_long_option(value, option))
    {
        return excluded(
            option,
            "this option changes headless/StructuredIO execution rather than native interactive rendering",
        );
    }
    if is_long_option(value, "--no-verbose") {
        return excluded(
            "--no-verbose",
            "the legacy CLI defines only the positive --verbose override; the negative form has no established renderer semantics",
        );
    }
    if after_separator && is_long_option(value, "--verbose") {
        return excluded(
            "--verbose",
            "after `--` this spelling would cross the child-reserved transport boundary instead of expressing an outer renderer override",
        );
    }
    None
}

/// Existing CrabCode options whose separate-token form consumes exactly one
/// following argv entry.
fn runtime_option_requires_value(value: &str) -> bool {
    matches!(
        value,
        "--debug-file"
            | "--thinking"
            | "--max-thinking-tokens"
            | "--system-prompt"
            | "--system-prompt-file"
            | "--append-system-prompt"
            | "--append-system-prompt-file"
            | "--permission-mode"
            | "--model"
            | "--effort"
            | "--agent"
            | "--settings"
            | "--session-id"
            | "-n"
            | "--name"
            | "--agents"
            | "--setting-sources"
            | "--plugin-dir"
            | "--advisor"
            | "--messaging-socket-path"
            | "--agent-id"
            | "--agent-name"
            | "--team-name"
            | "--agent-color"
            | "--parent-session-id"
            | "--teammate-mode"
            | "--agent-type"
    )
}

fn set_resume_selection(
    parsed: &mut CliMode,
    supplied_value: Option<&str>,
    flag: Option<OsString>,
    separate_value: Option<OsString>,
) {
    if supplied_value.is_some_and(is_canonical_session_id) {
        let value = supplied_value.expect("canonical resume value exists");
        parsed.initial_session = InitialSessionRequest::ResumeExact {
            session_id: value.to_string(),
        };
    } else {
        let initial_search = supplied_value
            .map(crate::text_safety::trim_ecmascript_whitespace)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        parsed.initial_session = InitialSessionRequest::ResumePicker { initial_search };
    }

    // The bundled direct child remains the sole session-storage authority.
    // Preserve the exact historical option spelling/value so Commander and
    // the pre-initialize direct setup adapter observe the same invocation.
    if let Some(flag) = flag {
        parsed.runtime_args.push(flag);
    }
    if let Some(value) = separate_value {
        parsed.runtime_args.push(value);
    }
}

fn is_canonical_session_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn remove_runtime_resume_arguments(arguments: &mut Vec<OsString>) {
    let previous = std::mem::take(arguments);
    let mut source = previous.into_iter().peekable();
    while let Some(argument) = source.next() {
        match argument.to_str() {
            Some("-r" | "--resume") => {
                if source
                    .peek()
                    .and_then(|next| next.to_str())
                    .is_some_and(|next| !next.starts_with('-'))
                {
                    let _value = source.next();
                }
            }
            Some(value) if value.starts_with("--resume=") => {}
            _ => arguments.push(argument),
        }
    }
}

fn set_composer_prefill(parsed: &mut CliMode, value: &str) -> Result<(), TerminalSetupError> {
    parsed.launch_provenance.prefill_source_length =
        (!value.is_empty()).then(|| value.encode_utf16().count());
    // The legacy renderer seeds `earlyInputBuffer`, then
    // `consumeEarlyInput()` applies JavaScript `trim()` before initializing
    // the composer. Rust's Unicode-aware `trim()` matches that observable
    // presentation behavior.
    let normalized = crate::text_safety::trim_ecmascript_whitespace(value);
    let maximum = crate::tui_app::MAX_COMPOSER_TEXT_BYTES;
    if normalized.len() > maximum {
        return Err(TerminalSetupError::PrefillTooLarge {
            actual: normalized.len(),
            maximum,
        });
    }
    parsed.composer_prefill = (!normalized.is_empty()).then(|| normalized.to_string());
    Ok(())
}

fn parse_legacy_optional_milliseconds(value: &str) -> Option<f64> {
    // The legacy Commander parser returns `undefined` for a non-finite
    // Number(value), rather than rejecting the process.
    let value = crate::text_safety::trim_ecmascript_whitespace(value);
    if value.is_empty() {
        return Some(0.0);
    }
    let radix_value = [
        ("0x", 16_u32),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ]
    .into_iter()
    .find_map(|(prefix, radix)| {
        value
            .strip_prefix(prefix)
            .map(|digits| parse_legacy_power_of_two_integer(digits, radix))
    });
    match radix_value {
        Some(parsed) => parsed.filter(|value| value.is_finite()),
        None => value.parse::<f64>().ok().filter(|value| value.is_finite()),
    }
}

/// Convert JavaScript `Number()`'s binary/octal/hex integer forms without a
/// `u64` ceiling or cumulative floating-point double rounding.
///
/// These radices are powers of two, so retaining the leading 53 bits and
/// applying IEEE-754 round-to-nearest-ties-to-even exactly reproduces the
/// integer-to-Number conversion. Every digit is validated before overflow is
/// returned; `0x1...z` must remain `NaN`, not become infinity early.
fn parse_legacy_power_of_two_integer(digits: &str, radix: u32) -> Option<f64> {
    let bits_per_digit = match radix {
        2 => 1_usize,
        8 => 3,
        16 => 4,
        _ => return None,
    };
    let parsed_digits = digits
        .bytes()
        .map(|byte| {
            let value = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a') + 10,
                b'A'..=b'F' => u32::from(byte - b'A') + 10,
                _ => return None,
            };
            (value < radix).then_some(value)
        })
        .collect::<Option<Vec<_>>>()?;
    if parsed_digits.is_empty() {
        return None;
    }

    let Some((first_index, first)) = parsed_digits
        .iter()
        .enumerate()
        .find(|(_, digit)| **digit != 0)
    else {
        return Some(0.0);
    };
    let first_bits = (u32::BITS - first.leading_zeros()) as usize;
    let trailing_digits = parsed_digits.len().saturating_sub(first_index + 1);
    let bit_length = trailing_digits
        .checked_mul(bits_per_digit)
        .and_then(|bits| bits.checked_add(first_bits))
        .unwrap_or(usize::MAX);

    let mut retained = 0_u64;
    let mut seen_bits = 0_usize;
    let mut round_bit = false;
    let mut sticky = false;
    for digit in &parsed_digits[first_index..] {
        for shift in (0..bits_per_digit).rev() {
            let bit = (digit >> shift) & 1;
            if seen_bits == 0 && bit == 0 {
                continue;
            }
            match seen_bits {
                0..=52 => retained = (retained << 1) | u64::from(bit),
                53 => round_bit = bit != 0,
                _ => sticky |= bit != 0,
            }
            seen_bits = seen_bits.saturating_add(1);
        }
    }
    debug_assert_eq!(seen_bits, bit_length);

    if bit_length <= 53 {
        return Some(retained as f64);
    }
    if round_bit && (sticky || retained & 1 == 1) {
        retained += 1;
    }
    let mut exponent = bit_length.saturating_sub(1);
    if retained == 1_u64 << 53 {
        retained >>= 1;
        exponent = exponent.saturating_add(1);
    }
    if exponent > 1_023 {
        return Some(f64::INFINITY);
    }
    let exponent_bits = ((exponent as u64) + 1_023) << 52;
    let fraction_bits = retained & ((1_u64 << 52) - 1);
    Some(f64::from_bits(exponent_bits | fraction_bits))
}

impl LaunchProvenance {
    /// Materialize both renderer languages from one observation of dynamic
    /// launch facts. The language picker may run after renderer context is
    /// bound, so retaining this pair prevents an early default language from
    /// freezing launch diagnostics in the wrong locale.
    pub(crate) fn localized_notice(&self) -> Option<(String, String)> {
        if !self.deep_link_origin {
            return self.prefill_source_length.map(|_| {
                (
                    "使用预填提示启动——按 Enter 前请先检查。".to_string(),
                    "Launched with a pre-filled prompt — review it before pressing Enter."
                        .to_string(),
                )
            });
        }
        let cwd = std::env::current_dir().ok()?;
        let home = dirs::home_dir();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64()
            * 1_000.0;
        Some((
            self.notice_at(&cwd, home.as_deref(), now_ms, UiLanguage::ZhCn)?,
            self.notice_at(&cwd, home.as_deref(), now_ms, UiLanguage::EnUs)?,
        ))
    }

    fn notice_at(
        &self,
        cwd: &std::path::Path,
        home: Option<&std::path::Path>,
        now_ms: f64,
        language: UiLanguage,
    ) -> Option<String> {
        if !self.deep_link_origin {
            return self.prefill_source_length.map(|_| match language {
                UiLanguage::ZhCn => "使用预填提示启动——按 Enter 前请先检查。".to_string(),
                UiLanguage::EnUs => {
                    "Launched with a pre-filled prompt — review it before pressing Enter."
                        .to_string()
                }
            });
        }

        let visible_cwd = tildify_path(cwd, home);
        let mut lines = vec![match language {
            UiLanguage::ZhCn => format!("此会话由外部深层链接在 {visible_cwd} 中打开"),
            UiLanguage::EnUs => {
                format!("This session was opened by an external deep link in {visible_cwd}")
            }
        }];
        if let Some(repo) = self
            .deep_link_repo
            .as_deref()
            .filter(|repo| !repo.is_empty())
        {
            let (age, stale) = match self.deep_link_last_fetch_ms {
                Some(last_fetch_ms) => (
                    format_relative_milliseconds(last_fetch_ms, now_ms, language),
                    now_ms - last_fetch_ms > 7.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
                ),
                None => (language.text("从未", "never").to_string(), true),
            };
            lines.push(match language {
                UiLanguage::ZhCn => format!(
                    "已从本地克隆解析 {repo} · 上次拉取：{age}{}",
                    if stale {
                        "——CRABCODE.md 可能已过期"
                    } else {
                        ""
                    }
                ),
                UiLanguage::EnUs => format!(
                    "Resolved {repo} from local clones · last fetched {age}{}",
                    if stale {
                        " — CRABCODE.md may be stale"
                    } else {
                        ""
                    }
                ),
            });
        }
        if let Some(length) = self.prefill_source_length {
            lines.push(match (language, length > 1_000) {
                (UiLanguage::ZhCn, true) => format!(
                    "下方提示（{} 个字符）由链接提供——按 Enter 前请滚动并完整检查。",
                    format_compact_number(length)
                ),
                (UiLanguage::ZhCn, false) => {
                    "下方提示由链接提供——按 Enter 前请仔细检查。".to_string()
                }
                (UiLanguage::EnUs, true) => format!(
                    "The prompt below ({} chars) was supplied by the link — scroll to review the entire prompt before pressing Enter.",
                    format_compact_number(length)
                ),
                (UiLanguage::EnUs, false) => {
                    "The prompt below was supplied by the link — review carefully before pressing Enter."
                        .to_string()
                }
            });
        }
        Some(lines.join("\n"))
    }
}

fn tildify_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    let Some(home) = home else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
        }
        _ => path.display().to_string(),
    }
}

fn format_relative_milliseconds(timestamp_ms: f64, now_ms: f64, language: UiLanguage) -> String {
    let diff_seconds = ((timestamp_ms - now_ms) / 1_000.0).trunc();
    const INTERVALS: [(f64, &str, &str); 7] = [
        (31_536_000.0, "年", "y"),
        (2_592_000.0, "个月", "mo"),
        (604_800.0, "周", "w"),
        (86_400.0, "天", "d"),
        (3_600.0, "小时", "h"),
        (60.0, "分钟", "m"),
        (1.0, "秒", "s"),
    ];
    for (seconds, zh_unit, en_unit) in INTERVALS {
        if diff_seconds.abs() >= seconds {
            let value = (diff_seconds / seconds).trunc() as i64;
            return match (language, diff_seconds < 0.0) {
                (UiLanguage::ZhCn, true) => format!("{}{zh_unit}前", value.unsigned_abs()),
                (UiLanguage::ZhCn, false) => format!("{value}{zh_unit}后"),
                (UiLanguage::EnUs, true) => {
                    format!("{}{en_unit} ago", value.unsigned_abs())
                }
                (UiLanguage::EnUs, false) => format!("in {value}{en_unit}"),
            };
        }
    }
    match (language, diff_seconds <= 0.0) {
        (UiLanguage::ZhCn, true) => "0秒前".to_string(),
        (UiLanguage::ZhCn, false) => "0秒后".to_string(),
        (UiLanguage::EnUs, true) => "0s ago".to_string(),
        (UiLanguage::EnUs, false) => "in 0s".to_string(),
    }
}

fn format_compact_number(value: usize) -> String {
    const UNITS: [(usize, &str); 4] = [
        (1_000_000_000_000, "t"),
        (1_000_000_000, "b"),
        (1_000_000, "m"),
        (1_000, "k"),
    ];
    for (index, (threshold, _suffix)) in UNITS.into_iter().enumerate() {
        if value >= threshold {
            let mut unit_index = index;
            let mut tenths = value.saturating_mul(10).saturating_add(threshold / 2) / threshold;
            // Intl compact notation promotes a rounded `1000.0k` to `1.0m`.
            if tenths >= 10_000 && unit_index > 0 {
                unit_index -= 1;
                let promoted_threshold = UNITS[unit_index].0;
                tenths = value
                    .saturating_mul(10)
                    .saturating_add(promoted_threshold / 2)
                    / promoted_threshold;
            }
            return format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit_index].1);
        }
    }
    value.to_string()
}

/// Commander `<value...>` options consume every following non-option token.
///
/// This is observably different from a required single value when the native
/// TUI is invoked directly. The sole positional prompt must precede a
/// variadic option, matching Commander's grammar; otherwise every token up to
/// the next option belongs to that backend option.
fn runtime_option_requires_variadic_value(value: &str) -> bool {
    let spelling = value
        .split_once('=')
        .map_or(value, |(spelling, _)| spelling);
    matches!(
        spelling,
        "--allowedTools"
            | "--allowed-tools"
            | "--tools"
            | "--disallowedTools"
            | "--disallowed-tools"
            | "--mcp-config"
            | "--betas"
            | "--add-dir"
            | "--file"
            | "--channels"
            | "--dangerously-load-development-channels"
    )
}

/// Commander optional-value options consume the next non-option token. Mirror
/// that grammar before identifying CrabCode TUI's one positional prompt;
/// otherwise a debug filter, worktree name, PR/session selector, or remote
/// description would be silently submitted as user content instead of reaching
/// the unchanged backend CLI. `--resume`/`-r` are handled earlier because only
/// canonical session UUIDs may cross the existing direct-backend boundary.
fn runtime_option_accepts_optional_value(value: &str) -> bool {
    matches!(value, "-d" | "--debug" | "-w" | "--worktree" | "--tasks")
}

fn set_cli_preference(
    slot: &mut Option<ModePreference>,
    preference: ModePreference,
    raw: &str,
) -> Result<(), TerminalSetupError> {
    *slot = match (*slot, preference) {
        (Some(ModePreference::Minimal), ModePreference::Fullscreen)
        | (Some(ModePreference::Fullscreen), ModePreference::Minimal) => {
            return Err(TerminalSetupError::ConflictingModes {
                first: slot
                    .expect("matched an existing preference")
                    .label()
                    .to_string(),
                second: raw.to_string(),
            });
        }
        // The fixed upstream accepts `--minimal --no-alt-screen`; minimal
        // wins because it is resolved before the alternate-screen policy.
        (Some(ModePreference::Minimal), ModePreference::Inline)
        | (Some(ModePreference::Inline), ModePreference::Minimal) => Some(ModePreference::Minimal),
        // `--fullscreen --no-alt-screen` is also accepted upstream:
        // fullscreen opts out of minimal, while no-alt-screen still selects
        // the standard inline renderer.
        (Some(ModePreference::Fullscreen), ModePreference::Inline)
        | (Some(ModePreference::Inline), ModePreference::Fullscreen) => {
            Some(ModePreference::Inline)
        }
        (_, preference) => Some(preference),
    };
    Ok(())
}

fn print_help() {
    println!(
        "CrabCode Rust TUI\n\n\
         Usage: crabcode-tui [--minimal | --fullscreen] [--no-alt-screen] [--continue | --resume SESSION_UUID] [RUNTIME_ARGS...]\n\n\
         Sessions:\n  \
           --continue             resume the most recently updated non-archived session in this cwd\n  \
           --resume SESSION_UUID   resume the canonical session UUID directly\n\n\
         Modes:\n  \
           --fullscreen       use the fixed standard renderer; alt-screen still follows terminal policy\n  \
           --no-alt-screen    preserve the main screen and native shell scrollback\n  \
           --minimal          commit finalized transcript items to native scrollback and keep a live region\n\n\
         Historical environment: {LEGACY_FULLSCREEN_ENV}=1|0, {LEGACY_DISABLE_MOUSE_ENV}=1,\n  \
           {LEGACY_DISABLE_MOUSE_CLICKS_ENV}=1. Unset fullscreen defaults follow {LEGACY_USER_TYPE_ENV}\n\
           plus the fixed Zellij/tmux-control and terminal-safety policies.\n\
         Precedence: fixed CLI flags > explicit historical environment > automatic policy\n\n\
         Backend model/MCP/add-dir arguments are forwarded to the private direct CrabCode runtime.\n  \
         Headless, remote, SDK transport, and child-reserved StructuredIO arguments are rejected,\n  \
         including when written after `--`."
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectionFacts {
    term: Option<String>,
    tmux: bool,
    zellij: bool,
    tmux_control_mode: bool,
    mouse_reporting_leaks_as_raw_text: bool,
    legacy_fullscreen: Option<String>,
    user_type: Option<String>,
    disable_mouse_tracking: bool,
    disable_mouse_clicks: bool,
}

impl DetectionFacts {
    fn from_process() -> Self {
        let context = terminal_context();
        let tmux = context.multiplexer == MultiplexerKind::Tmux;
        let zellij = context.multiplexer == MultiplexerKind::Zellij;
        let tmux_control_mode = tmux
            && (is_tmux_control_mode_env_heuristic(
                std::env::var_os("TMUX").is_some(),
                std::env::var("TERM_PROGRAM").ok().as_deref(),
                std::env::var("TERM").ok().as_deref(),
            ) || detect_tmux_control_mode());
        Self {
            term: std::env::var("TERM").ok(),
            tmux,
            zellij,
            tmux_control_mode,
            mouse_reporting_leaks_as_raw_text: context.mouse_reporting_leaks_as_raw_text(),
            legacy_fullscreen: std::env::var(LEGACY_FULLSCREEN_ENV).ok(),
            user_type: std::env::var(LEGACY_USER_TYPE_ENV).ok(),
            disable_mouse_tracking: env_value_is_truthy(
                std::env::var(LEGACY_DISABLE_MOUSE_ENV).ok().as_deref(),
            ),
            disable_mouse_clicks: env_value_is_truthy(
                std::env::var(LEGACY_DISABLE_MOUSE_CLICKS_ENV)
                    .ok()
                    .as_deref(),
            ),
        }
    }
}

fn env_value_is_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn env_value_is_defined_falsy(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

fn legacy_fullscreen_override(value: Option<&str>) -> Option<bool> {
    if env_value_is_defined_falsy(value) {
        Some(false)
    } else if env_value_is_truthy(value) {
        Some(true)
    } else {
        None
    }
}

/// Historical CrabCode's zero-subprocess iTerm2 `tmux -CC` clue.
///
/// The fixed Rust renderer's `#{client_flags}` probe remains the authoritative
/// backstop. This clue only preserves the historical fast path and therefore
/// cannot turn a positive result into a negative one.
fn is_tmux_control_mode_env_heuristic(
    tmux: bool,
    term_program: Option<&str>,
    term: Option<&str>,
) -> bool {
    tmux && term_program == Some("iTerm.app")
        && !term.unwrap_or_default().starts_with("screen")
        && !term.unwrap_or_default().starts_with("tmux")
}

/// Pinned upstream alternate-screen policy.
///
/// Normal tmux, GNU screen, Byobu, SSH, and unidentified terminals do not
/// imply inline rendering. In automatic mode only Zellij and a tmux client
/// that is positively reported as control mode disable the alternate screen.
/// A failed tmux query is not evidence of control mode and therefore preserves
/// the normal fullscreen default. Explicit fullscreen/inline/minimal choices
/// are resolved before this automatic-only predicate.
fn determine_alt_screen_policy(facts: &DetectionFacts, is_control_mode: bool) -> bool {
    if facts.zellij {
        return false;
    }
    if facts.tmux && is_control_mode {
        return false;
    }
    true
}

/// Ask the current tmux client for its authoritative flag set. The exact
/// upstream fail-closed query contract returns `false` when tmux is absent,
/// the command fails, or the output does not contain `control-mode`.
fn detect_tmux_control_mode() -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "#{client_flags}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("control-mode"))
}

fn resolve_mode_inputs(
    cli: Option<ModePreference>,
    facts: DetectionFacts,
) -> Result<TerminalPlan, TerminalSetupError> {
    if let Some(preference) = cli {
        return resolve_preference(
            preference,
            TerminalModeSource::Cli,
            "fixed renderer CLI flag",
            "固定渲染器 CLI 参数",
            facts,
        );
    }
    resolve_auto_mode(facts)
}

fn resolve_preference(
    preference: ModePreference,
    source: TerminalModeSource,
    reason_en_us: &str,
    reason_zh_cn: &str,
    facts: DetectionFacts,
) -> Result<TerminalPlan, TerminalSetupError> {
    reject_dumb_terminal(&facts)?;
    let (mode, detail_en_us, detail_zh_cn) = match preference {
        ModePreference::Minimal => (
            TerminalMode::Minimal,
            "--minimal selected scrollback-native rendering",
            "--minimal 选择了原生终端回滚区渲染",
        ),
        ModePreference::Inline => (
            TerminalMode::Inline,
            "--no-alt-screen selected standard inline rendering",
            "--no-alt-screen 选择了标准内联渲染",
        ),
        ModePreference::Fullscreen => {
            if determine_alt_screen_policy(&facts, facts.tmux_control_mode) {
                (
                    TerminalMode::Fullscreen,
                    "--fullscreen selected the standard renderer and automatic terminal policy selected the alternate screen",
                    "--fullscreen 选择了标准渲染器，自动终端策略选择了备用屏幕",
                )
            } else if facts.zellij {
                (
                    TerminalMode::Inline,
                    "--fullscreen selected the standard renderer; fixed Zellij policy kept it inline",
                    "--fullscreen 选择了标准渲染器；固定 Zellij 策略使其保持内联",
                )
            } else {
                (
                    TerminalMode::Inline,
                    "--fullscreen selected the standard renderer; fixed tmux control-mode policy kept it inline",
                    "--fullscreen 选择了标准渲染器；固定 tmux control-mode 策略使其保持内联",
                )
            }
        }
    };
    Ok(plan_for(
        mode,
        source,
        format!("{reason_en_us}: {detail_en_us}"),
        format!("{reason_zh_cn}：{detail_zh_cn}"),
        &facts,
    ))
}

fn resolve_auto_mode(facts: DetectionFacts) -> Result<TerminalPlan, TerminalSetupError> {
    reject_dumb_terminal(&facts)?;
    if let Some(fullscreen) = legacy_fullscreen_override(facts.legacy_fullscreen.as_deref()) {
        let (mode, reason_en_us, reason_zh_cn) = if fullscreen {
            (
                TerminalMode::Fullscreen,
                "historical CRABCODE_NO_FLICKER explicit opt-in selected the alternate screen",
                "历史兼容项 CRABCODE_NO_FLICKER 已显式启用，选择了备用屏幕",
            )
        } else {
            (
                TerminalMode::Inline,
                "historical CRABCODE_NO_FLICKER explicit opt-out selected standard inline rendering",
                "历史兼容项 CRABCODE_NO_FLICKER 已显式停用，选择了标准内联渲染",
            )
        };
        return Ok(plan_for(
            mode,
            TerminalModeSource::Environment,
            reason_en_us.to_string(),
            reason_zh_cn.to_string(),
            &facts,
        ));
    }
    if facts.mouse_reporting_leaks_as_raw_text {
        return Ok(plan_for(
            TerminalMode::Minimal,
            TerminalModeSource::Auto,
            "JetBrains JediTerm on native Windows leaks VT mouse reports as input; fixed automatic policy selected minimal rendering".to_string(),
            "原生 Windows 上的 JetBrains JediTerm 会将 VT 鼠标报告泄漏为输入；固定自动策略选择了精简渲染".to_string(),
            &facts,
        ));
    }
    let product_defaults_to_fullscreen = facts.user_type.as_deref() == Some("ant");
    let terminal_allows_fullscreen = determine_alt_screen_policy(&facts, facts.tmux_control_mode);
    let (mode, reason_en_us, reason_zh_cn) = if product_defaults_to_fullscreen
        && terminal_allows_fullscreen
    {
        (
            TerminalMode::Fullscreen,
            "historical USER_TYPE=ant default and fixed terminal policy selected the alternate screen",
            "历史兼容默认值 USER_TYPE=ant 与固定终端策略选择了备用屏幕",
        )
    } else if !product_defaults_to_fullscreen {
        (
            TerminalMode::Inline,
            "historical external-user default selected standard inline rendering",
            "历史外部用户默认值选择了标准内联渲染",
        )
    } else if facts.zellij {
        (
            TerminalMode::Inline,
            "Zellij detected; fixed automatic policy selected standard inline rendering",
            "检测到 Zellij；固定自动策略选择了标准内联渲染",
        )
    } else {
        (
            TerminalMode::Inline,
            "tmux control mode detected; fixed automatic policy selected standard inline rendering",
            "检测到 tmux control mode；固定自动策略选择了标准内联渲染",
        )
    };
    Ok(plan_for(
        mode,
        TerminalModeSource::Auto,
        reason_en_us.to_string(),
        reason_zh_cn.to_string(),
        &facts,
    ))
}

fn plan_for(
    mode: TerminalMode,
    source: TerminalModeSource,
    reason: String,
    reason_zh_cn: String,
    facts: &DetectionFacts,
) -> TerminalPlan {
    TerminalPlan {
        mode,
        source,
        reason,
        reason_zh_cn,
        disable_fullscreen_mouse_tracking: facts.disable_mouse_tracking,
        mouse_clicks_disabled: mode == TerminalMode::Fullscreen && facts.disable_mouse_clicks,
        presentation_verbose_override: None,
    }
}

fn reject_dumb_terminal(facts: &DetectionFacts) -> Result<(), TerminalSetupError> {
    if facts
        .term
        .as_deref()
        .is_some_and(|term| term.eq_ignore_ascii_case("dumb"))
    {
        Err(TerminalSetupError::DumbTerminal)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSignal {
    Terminate(i32),
    Suspend,
    Resume,
}

pub struct TerminalSession {
    ownership: TerminalOwnershipGuard,
    terminal: Option<TuiTerminal>,
    /// Persistent fixed-lifecycle root renderer. It lives with the terminal
    /// generation so every frame, resize, suspend/resume, and minimal native
    /// commit observes the same AgentView/ScrollbackState owner.
    app_view: crate::app_view::AppView,
    writer_thread: Option<CrabCodeWriterThread>,
    terminal_control_lease: Option<TerminalControlLease>,
    writer_sync: WriterSync,
    writer_events: Option<tokio::sync::mpsc::UnboundedReceiver<WriterEvent>>,
    setup_fallback: Option<TerminalFallback>,
    last_rendered_frame: Option<Buffer>,
    last_rendered_hyperlinks: Option<Vec<LinkSpan>>,
    cursor_state: CursorState,
    focus_heal: FocusHealState,
    hyperlink_route: HyperlinkRoute,
    backend_size: Size,
    resize_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FocusHealState {
    armed: bool,
}

impl Default for FocusHealState {
    fn default() -> Self {
        Self { armed: true }
    }
}

impl FocusHealState {
    fn observe(&mut self, gained: bool, policy_enabled: bool) -> bool {
        if !policy_enabled {
            return false;
        }
        if !gained {
            self.armed = true;
            return false;
        }
        if !self.armed {
            return false;
        }
        self.armed = false;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PresentationProgress {
    pub queued: u64,
    pub written: u64,
}

/// Fixed terminal-ownership guard core.
///
/// The guard claims the process lifecycle and installs its signal owner before
/// raw/protocol setup, then remains the sole authority through writer
/// publication, child-TTY handoff, suspend/resume, final drain, and Drop.
/// `TerminalSession` keeps rendering buffers and CrabCode's stdout writer
/// adapter only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalOwnershipPhase {
    Acquiring,
    Active,
    ChildHandoff,
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalAcquisitionStage {
    Unarmed,
    SignalOwner,
    Protocol,
    WriterOwner,
}

struct TerminalOwnershipGuard {
    mode: TerminalMode,
    phase: TerminalOwnershipPhase,
    acquisition_stage: TerminalAcquisitionStage,
    lifecycle_claimed: bool,
    signal_owner_installed: bool,
    disable_fullscreen_mouse_tracking: bool,
    restore_on_drop: fn(),
    #[cfg(unix)]
    signals: Option<SignalMonitor>,
    #[cfg(windows)]
    signals: Option<WindowsSignalMonitor>,
}

impl TerminalOwnershipGuard {
    fn acquire(plan: &TerminalPlan) -> io::Result<Self> {
        if terminal_writer_emergency_stopped() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal lifecycle cannot restart after emergency restoration",
            ));
        }
        TERMINAL_SESSION_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another CrabCode terminal session already owns this process terminal",
                )
            })?;
        let mut ownership = Self {
            mode: plan.mode,
            phase: TerminalOwnershipPhase::Acquiring,
            acquisition_stage: TerminalAcquisitionStage::Unarmed,
            lifecycle_claimed: true,
            signal_owner_installed: false,
            disable_fullscreen_mouse_tracking: plan.disable_fullscreen_mouse_tracking,
            restore_on_drop: restore_terminal_on_guard_drop,
            #[cfg(any(unix, windows))]
            signals: None,
        };
        // Publish one immutable emergency snapshot for this exact ownership
        // generation before installing any signal owner or entering raw
        // mode. A later generation must never expose the previous
        // generation's termios or visual-output descriptor.
        capture_emergency_terminal_baseline()?;
        ownership.install_signal_monitor()?;
        enter_terminal_mode(
            plan.mode,
            mouse_capture_for_mode(plan.mode, plan.disable_fullscreen_mouse_tracking),
        )?;
        ownership.phase = TerminalOwnershipPhase::Active;
        ownership.acquisition_stage = TerminalAcquisitionStage::Protocol;
        #[cfg(feature = "terminal-lifecycle-tests")]
        if let Err(error) = wait_test_only_terminal_acquisition_barrier() {
            ownership.restore_incomplete_generation();
            return Err(error);
        }
        Ok(ownership)
    }

    const fn mode(&self) -> TerminalMode {
        self.mode
    }

    fn set_effective_mode(&mut self, mode: TerminalMode) {
        self.mode = mode;
    }

    const fn renderer_active(&self) -> bool {
        matches!(self.phase, TerminalOwnershipPhase::Active)
    }

    const fn owns_generation(&self) -> bool {
        matches!(
            self.phase,
            TerminalOwnershipPhase::Active | TerminalOwnershipPhase::ChildHandoff
        )
    }

    fn publish_writer_owner(&mut self) -> io::Result<()> {
        if !self.renderer_active() || self.acquisition_stage != TerminalAcquisitionStage::Protocol {
            return Err(io::Error::other(
                "terminal writer owner must follow active protocol acquisition exactly once",
            ));
        }
        if !self.signal_monitor_installed() {
            return Err(io::Error::other(
                "terminal writer owner requires the pre-acquisition signal owner",
            ));
        }
        self.acquisition_stage = TerminalAcquisitionStage::WriterOwner;
        Ok(())
    }

    fn signal_monitor_installed(&self) -> bool {
        self.signal_owner_installed
    }

    fn install_signal_owner<T>(
        &mut self,
        install: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        if self.phase != TerminalOwnershipPhase::Acquiring
            || self.acquisition_stage != TerminalAcquisitionStage::Unarmed
        {
            return Err(io::Error::other(
                "terminal signal owner must be installed exactly once before protocol acquisition",
            ));
        }
        let owner = install()?;
        self.signal_owner_installed = true;
        self.acquisition_stage = TerminalAcquisitionStage::SignalOwner;
        Ok(owner)
    }

    fn install_signal_monitor(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            if self.signals.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "terminal signal monitor is already installed",
                ));
            }
            self.signals = Some(self.install_signal_owner(SignalMonitor::install)?);
        }
        #[cfg(windows)]
        {
            if self.signals.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "terminal signal monitor is already installed",
                ));
            }
            self.signals = Some(self.install_signal_owner(WindowsSignalMonitor::install)?);
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.install_signal_owner(|| Ok(()))?;
        }
        Ok(())
    }

    fn finish_restore(&mut self, result: io::Result<()>) -> io::Result<()> {
        self.phase = TerminalOwnershipPhase::Restored;
        result
    }

    fn restore_generation(
        &mut self,
        terminal: TuiTerminal,
        writer_thread: CrabCodeWriterThread,
    ) -> io::Result<()> {
        let result = restore_terminal(terminal, writer_thread, self.mode);
        self.finish_restore(result)
    }

    fn restore_incomplete_generation(&mut self) {
        restore_active_terminal_normally(None);
        let _ = self.finish_restore(Ok(()));
    }

    fn reacquire_protocol(&mut self) -> io::Result<()> {
        if self.renderer_active() {
            return Ok(());
        }
        capture_emergency_terminal_baseline()?;
        enter_terminal_mode(
            self.mode,
            mouse_capture_for_mode(self.mode, self.disable_fullscreen_mouse_tracking),
        )?;
        self.phase = TerminalOwnershipPhase::Active;
        self.acquisition_stage = TerminalAcquisitionStage::Protocol;
        Ok(())
    }

    fn release_for_child(&mut self) -> io::Result<()> {
        if self.renderer_active() {
            release_tty_for_child(self.mode)?;
            self.phase = TerminalOwnershipPhase::ChildHandoff;
        }
        Ok(())
    }

    fn reacquire_after_child(&mut self) -> io::Result<()> {
        if self.phase == TerminalOwnershipPhase::ChildHandoff {
            reacquire_tty_after_child(self.mode)?;
            self.phase = TerminalOwnershipPhase::Active;
        }
        Ok(())
    }

    fn pending_signal(&self) -> Option<TerminalSignal> {
        #[cfg(any(unix, windows))]
        {
            self.signals.as_ref().and_then(|signals| signals.try_recv())
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    }

    fn signal_notifier(&self) -> Option<std::sync::Arc<Notify>> {
        #[cfg(unix)]
        {
            self.signals.as_ref().map(SignalMonitor::notifier)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    #[cfg(test)]
    fn for_test(
        phase: TerminalOwnershipPhase,
        acquisition_stage: TerminalAcquisitionStage,
        restore_on_drop: fn(),
    ) -> Self {
        Self {
            mode: TerminalMode::Fullscreen,
            phase,
            acquisition_stage,
            lifecycle_claimed: false,
            signal_owner_installed: acquisition_stage != TerminalAcquisitionStage::Unarmed,
            disable_fullscreen_mouse_tracking: false,
            restore_on_drop,
            #[cfg(any(unix, windows))]
            signals: None,
        }
    }
}

impl Drop for TerminalOwnershipGuard {
    fn drop(&mut self) {
        if self.owns_generation() {
            (self.restore_on_drop)();
            self.phase = TerminalOwnershipPhase::Restored;
        }
        #[cfg(any(unix, windows))]
        drop(self.signals.take());
        if self.lifecycle_claimed {
            TERMINAL_SESSION_CLAIMED.store(false, Ordering::Release);
            self.lifecycle_claimed = false;
        }
    }
}

fn restore_terminal_on_guard_drop() {
    restore_active_terminal(None);
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn wait_test_only_terminal_acquisition_barrier() -> io::Result<()> {
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_ACQUISITION_READY_FILE";
    const RELEASE_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_ACQUISITION_RELEASE_FILE";
    let (ready, release) = match (
        std::env::var_os(READY_FILE_ENV),
        std::env::var_os(RELEASE_FILE_ENV),
    ) {
        (Some(ready), Some(release)) => (ready, release),
        (None, None) => return Ok(()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{READY_FILE_ENV} and {RELEASE_FILE_ENV} must be set together"),
            ));
        }
    };
    std::fs::write(&ready, b"ready")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !std::path::Path::new(&release).is_file() {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "test-only terminal acquisition barrier was not released at {}",
                    std::path::Path::new(&release).display()
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}

impl TerminalSession {
    pub fn enter(plan: &TerminalPlan) -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "crabcode-tui requires interactive stdin and stdout",
            ));
        }
        let (columns, rows) = crossterm::terminal::size()?;
        if columns < MIN_TERMINAL_COLUMNS || rows < MIN_TERMINAL_ROWS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "terminal is {columns}x{rows}; crabcode-tui requires at least \
                     {MIN_TERMINAL_COLUMNS}x{MIN_TERMINAL_ROWS}"
                ),
            ));
        }

        install_restore_panic_hook();
        let mut ownership = TerminalOwnershipGuard::acquire(plan)?;

        let (writer_sync, writer_events) = writer_event_channel();
        let (terminal, writer_thread, effective_mode, setup_fallback) = create_terminal(
            plan.mode,
            Size::new(columns, rows),
            true,
            writer_sync.clone(),
        )?;
        ownership.set_effective_mode(effective_mode);
        let mut session = Self {
            ownership,
            terminal: Some(terminal),
            app_view: crate::app_view::AppView::new(),
            writer_thread: Some(writer_thread),
            terminal_control_lease: None,
            writer_sync,
            writer_events: Some(writer_events),
            setup_fallback,
            last_rendered_frame: None,
            last_rendered_hyperlinks: None,
            cursor_state: CursorState::default(),
            focus_heal: FocusHealState::default(),
            hyperlink_route: terminal_context().hyperlink_route(),
            backend_size: Size::new(columns, rows),
            resize_pending: false,
        };
        if let Err(error) = session.ownership.publish_writer_owner() {
            drop(session);
            return Err(error);
        }
        if cursor_color_requires_late_apply(plan.mode, effective_mode) {
            // Minimal intentionally leaves the terminal-native cursor alone.
            // If its viewport probe downgrades to ordinary inline rendering,
            // complete the fixed two-phase theme handshake now. Construct the
            // session first so a rejected active-write guard drops the
            // terminal sender before joining its writer thread.
            let _output_guard = match lock_terminal_output_for_active_write() {
                Ok(guard) => guard,
                Err(error) => {
                    drop(session);
                    return Err(error);
                }
            };
            apply_cursor_color(&mut io::stdout(), effective_mode);
        }
        if let Err(error) = session.terminal_mut().clear() {
            drop(session);
            return Err(error);
        }
        // Fixed-upstream ordering: fire the XTVERSION query immediately
        // before the sole crossterm reader starts. The application event loop
        // owns the reply filter, so a DCS response can never surface as typed
        // prompt text or be folded into paste coalescing.
        {
            let _output_guard = lock_terminal_output_for_active_write()?;
            crabcode_pager_render::audited_terminal::xtversion::probe_at_startup();
        }
        if let Err(error) = session.install_terminal_control_lease() {
            drop(session);
            return Err(error);
        }
        Ok(session)
    }

    pub(crate) const fn mode(&self) -> TerminalMode {
        self.ownership.mode()
    }

    pub(crate) fn localized_setup_notice(&self) -> Option<(&'static str, &'static str)> {
        self.setup_fallback.map(|fallback| {
            (
                fallback.notice(UiLanguage::ZhCn),
                fallback.notice(UiLanguage::EnUs),
            )
        })
    }

    fn terminal_mut(&mut self) -> &mut TuiTerminal {
        match self.terminal.as_mut() {
            Some(terminal) => terminal,
            None => panic!("CrabCode terminal is not active"),
        }
    }

    fn install_terminal_control_lease(&mut self) -> io::Result<()> {
        if self.terminal_control_lease.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "CrabCode terminal control writer is already installed",
            ));
        }
        let sender = self
            .terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("CrabCode terminal is not active"))?
            .backend_mut()
            .frame_writer_mut()
            .terminal_control_sender();
        self.terminal_control_lease = Some(install_terminal_control_sender(sender)?);
        Ok(())
    }

    fn release_terminal_control_lease(&mut self) -> io::Result<()> {
        match self.terminal_control_lease.take() {
            Some(lease) => lease.release(),
            None => Ok(()),
        }
    }

    /// Atomically commit minimal-mode history and draw one blink-preserving
    /// differential frame.
    ///
    /// Resize, native-scrollback insertion, live viewport repaint, and cursor
    /// transition share one synchronized-output transaction. The renderer
    /// returns the desired visible cursor position. A completely unchanged
    /// presentation is discarded before it reaches stdout.
    pub fn present(&mut self, app: &mut TuiApp) -> io::Result<()> {
        self.present_with_repaint(app, false)
    }

    /// Present through the fixed-upstream transaction, optionally invalidating
    /// the whole Ratatui surface inside the same synchronized frame.
    ///
    /// The event-loop Presenter owns when this flag becomes sticky. This
    /// adapter only applies it to CrabCode's TerminalSession and does not own
    /// any backend or protocol state.
    pub(crate) fn present_with_repaint(
        &mut self,
        app: &mut TuiApp,
        force_full_repaint: bool,
    ) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        // Close projection conversion before the first terminal mutation.
        // The persistent view applies every queued semantic input to the same
        // ScrollbackState that minimal commit and frame paint use below.
        self.app_view.prepare(app).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("transcript projection could not be rendered: {error}"),
            )
        })?;
        let mode = self.ownership.mode();
        let resize_pending = self.resize_pending;
        let result = {
            let TerminalSession {
                terminal,
                app_view,
                cursor_state,
                last_rendered_frame,
                last_rendered_hyperlinks,
                backend_size,
                hyperlink_route,
                ..
            } = self;
            let terminal = terminal
                .as_mut()
                .ok_or_else(|| io::Error::other("CrabCode terminal is not active"))?;
            present_terminal_frame(
                terminal,
                cursor_state,
                last_rendered_frame,
                last_rendered_hyperlinks,
                backend_size,
                resize_pending,
                mode,
                *hyperlink_route,
                |terminal| {
                    if force_full_repaint {
                        terminal.clear()?;
                    }
                    let hold_native_commits = crate::tui_ui::minimal_centered_surface_active(app);
                    let welcome_will_commit = mode == TerminalMode::Minimal
                        && app.minimal_welcome_pending()
                        && terminal.viewport_area().width >= MINIMAL_WELCOME_MIN_WIDTH;
                    let history_will_commit = mode == TerminalMode::Minimal
                        && !hold_native_commits
                        && app_view.minimal_will_commit(app);
                    let viewport_changed = if mode == TerminalMode::Minimal {
                        let screen = terminal.last_known_area();
                        let target =
                            app_view.minimal_viewport_height(app, screen.width, screen.height);
                        sync_minimal_viewport(
                            terminal,
                            target,
                            welcome_will_commit || history_will_commit,
                        )?
                    } else {
                        false
                    };
                    // The fixed lifecycle commits a fresh-session welcome
                    // before the first finalized conversation block.
                    let welcomed = if mode == TerminalMode::Minimal {
                        commit_minimal_welcome(terminal, app)?
                    } else {
                        false
                    };
                    let committed = if mode == TerminalMode::Minimal {
                        app_view.commit_minimal(terminal, app, hold_native_commits)?
                    } else {
                        false
                    };
                    let render_outcome = {
                        let mut frame = terminal.get_frame();
                        app_view.draw_prepared(&mut frame, app).map_err(|error| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("prepared transcript could not be rendered: {error}"),
                            )
                        })?
                    };
                    Ok((
                        render_outcome,
                        force_full_repaint || viewport_changed || welcomed || committed,
                    ))
                },
            )
        };
        if result.is_ok() {
            self.resize_pending = false;
        }
        result
    }

    /// Query the persistent root view's next renderer-local animation
    /// deadline. This keeps transcript timing on the terminal → AppView →
    /// AgentView owner chain instead of recreating it in product state.
    pub(crate) fn renderer_animation_deadline(
        &mut self,
        now: std::time::Instant,
    ) -> Option<std::time::Instant> {
        self.app_view.renderer_animation_deadline(now)
    }

    /// Advance one due renderer-local animation deadline.
    pub(crate) fn tick_renderer_animation(&mut self, now: std::time::Instant) -> bool {
        self.app_view.tick_renderer_animation(now)
    }

    pub(crate) fn retire_input_side_channels_for_terminal_generation(&mut self) {
        self.app_view
            .retire_input_side_channels_for_terminal_generation();
    }

    /// Route input through the same persistent AppView that owns frame paint.
    pub(crate) fn handle_renderer_input(
        &mut self,
        app: &mut TuiApp,
        event: crossterm::event::Event,
        arrived_at: std::time::Instant,
        paste_provenance: crate::app_event_loop::PasteProvenance,
    ) -> crate::tui_app::TerminalEventOutcome {
        self.app_view
            .handle_input(app, event, arrived_at, paste_provenance)
    }

    /// Return the last payload accepted from the UI producer and the last
    /// payload durably handed to the terminal output (`write_all` plus
    /// `flush`). A healthy but still-running write is not an error.
    ///
    /// The event loop uses the final queued sequence number of a presentation
    /// as its ACK target. The target is reserved before the complete
    /// synchronized frame enters the fixed-upstream ordered writer channel.
    pub(crate) fn presentation_progress(&self) -> io::Result<PresentationProgress> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        let sync = self
            .writer_thread
            .as_ref()
            .ok_or_else(|| io::Error::other("CrabCode terminal writer is not active"))?
            .writer_sync();
        Ok(PresentationProgress {
            queued: sync.queued(),
            written: sync.written(),
        })
    }

    /// Submit OSC/BEL notification bytes through the sole ordered terminal
    /// writer. This preserves frame/control ordering and keeps every terminal
    /// mutation inside the current ownership generation.
    pub(crate) fn enqueue_control_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        self.terminal_mut()
            .backend_mut()
            .frame_writer_mut()
            .enqueue_control_bytes(bytes)
    }

    pub(crate) fn take_presentation_events(
        &mut self,
    ) -> io::Result<tokio::sync::mpsc::UnboundedReceiver<WriterEvent>> {
        self.writer_events
            .take()
            .ok_or_else(|| io::Error::other("CrabCode terminal writer events already taken"))
    }

    /// Wait a bounded interval for every accepted presentation to reach and
    /// flush through the sole terminal writer.
    ///
    /// Child-process handoff code parks input first, then calls this method.
    /// `TimedOut` is retryable and must not release terminal ownership.
    pub(crate) fn wait_writer_drained(
        &self,
        timeout: std::time::Duration,
    ) -> io::Result<WriterDrain> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        self.writer_thread
            .as_ref()
            .ok_or_else(|| io::Error::other("CrabCode terminal writer is not active"))?
            .writer_sync()
            .wait_drained(timeout)
    }

    /// Close the process-global standalone-control route before a blocking
    /// child handoff. Input is already parked by the caller. Closing first
    /// makes the subsequent writer drain a stable frontier: no clipboard
    /// worker can enqueue OSC bytes after the drain and race the child's tty.
    pub(crate) fn park_control_writer_for_child(&mut self) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        if self.terminal_control_lease.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "CrabCode terminal control writer is not installed",
            ));
        }
        self.release_terminal_control_lease()
    }

    /// Restore the exact still-live frame-writer generation after all direct
    /// child-exit terminal probes are complete and before input is unparked.
    pub(crate) fn restore_control_writer_after_child(&mut self) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        self.install_terminal_control_lease()
    }

    /// Probe the cursor immediately before a child takes the TTY.
    ///
    /// Only minimal mode needs CPR: fullscreen restores its alternate-screen
    /// cursor, while ordinary inline mode does not own the native-scrollback
    /// re-anchor contract.
    pub(crate) fn probe_cursor_before_child(&self) -> io::Result<Option<(u16, u16)>> {
        if !self.ownership.renderer_active() || self.ownership.mode() != TerminalMode::Minimal {
            return Ok(None);
        }
        let _output_guard = lock_terminal_output_for_active_write()?;
        Ok(crossterm::cursor::position().ok())
    }

    /// Return the post-child cursor only when it differs from the pre-child
    /// position. Call while the input reader remains parked.
    pub(crate) fn probe_moved_cursor_after_child(
        &self,
        before: Option<(u16, u16)>,
    ) -> io::Result<Option<(u16, u16)>> {
        if !self.ownership.renderer_active() || self.ownership.mode() != TerminalMode::Minimal {
            return Ok(None);
        }
        let Some(before) = before else {
            return Ok(None);
        };
        let _output_guard = lock_terminal_output_for_active_write()?;
        let Some(after) = crossterm::cursor::position().ok() else {
            return Ok(None);
        };
        Ok((after != before).then_some(after))
    }

    /// Temporarily return raw/alternate-screen ownership to a blocking child
    /// without destroying the terminal or its ordered writer generation.
    ///
    /// The fixed child-handoff state machine calls this only after input is
    /// parked and every accepted writer payload is durably flushed.
    pub(crate) fn release_tty_for_child(&mut self) -> io::Result<()> {
        self.ownership.release_for_child()
    }

    /// Reacquire the same raw/alternate-screen generation after a blocking
    /// child exits. Presentation invalidation remains owned by
    /// [`Self::restore_after_child`] and the event-loop Presenter.
    pub(crate) fn reacquire_tty_after_child(&mut self) -> io::Result<()> {
        self.ownership.reacquire_after_child()
    }

    /// Re-anchor minimal's live viewport after main-screen child output and
    /// invalidate every renderer diff cache before the next presentation.
    ///
    /// The child bypassed Ratatui. This method deliberately does not clear
    /// outside the frame transaction: the caller makes the fixed-upstream
    /// full-repaint request, which clears and redraws inside one synchronized
    /// presentation.
    pub(crate) fn restore_after_child(
        &mut self,
        moved_cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        let terminal = self
            .terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("CrabCode terminal is not active"))?;
        if let Some((_column, row)) = moved_cursor
            && self.ownership.mode() == TerminalMode::Minimal
        {
            let screen = terminal.last_known_area();
            let current = terminal.viewport_area();
            let viewport_height = current.height.max(1).min(screen.height.max(1));
            ratatui::backend::Backend::append_lines(
                terminal.backend_mut(),
                viewport_height.saturating_sub(1),
            )?;
            let available = screen.height.saturating_sub(row).saturating_sub(1);
            let top =
                row.saturating_sub(viewport_height.saturating_sub(1).saturating_sub(available));
            terminal.set_viewport_area(Rect {
                y: top,
                height: viewport_height,
                ..current
            });
        }
        self.last_rendered_frame = None;
        self.last_rendered_hyperlinks = None;
        self.cursor_state.mark_disturbed();
        Ok(())
    }

    /// Readiness notification for Unix signals bridged out of
    /// `signal_hook`'s iterator thread. Windows console handlers remain
    /// atomic-only and therefore deliberately return `None`; the event loop
    /// uses its low-frequency watchdog fallback there.
    pub(crate) fn signal_notifier(&self) -> Option<std::sync::Arc<Notify>> {
        self.ownership.signal_notifier()
    }

    /// Restore the parent shell before stopping the process or launching an
    /// external terminal owner. Idempotency prevents a queued SIGCONT from
    /// double-entering raw mode.
    pub fn suspend(&mut self) -> io::Result<()> {
        // Close the process-global route before consuming the frame writer.
        // From this point until resume installs a fresh generation, OSC52 and
        // other standalone controls fail closed instead of reaching stdout.
        let release_result = self.release_terminal_control_lease();
        if self.ownership.owns_generation() {
            let restore_result = match (self.terminal.take(), self.writer_thread.take()) {
                (Some(terminal), Some(writer_thread)) => {
                    self.ownership.restore_generation(terminal, writer_thread)
                }
                (terminal, writer_thread) => {
                    drop(terminal);
                    drop(writer_thread);
                    self.ownership.restore_incomplete_generation();
                    Err(io::Error::other(
                        "CrabCode terminal ownership was incomplete during restore",
                    ))
                }
            };
            return merge_terminal_results(release_result, restore_result);
        }
        release_result
    }

    /// Reassert the live renderer's terminal protocol after an externally
    /// imposed stop/resume that bypassed [`Self::suspend`].
    ///
    /// The caller must park the sole input reader, close the standalone
    /// control lease, and drain the ordered writer before entering this
    /// transaction. The live Ratatui terminal and writer generation are
    /// retained; only terminal-side modes and renderer diff caches are healed.
    pub(crate) fn heal_after_external_resume(&mut self) -> io::Result<()> {
        if !self.ownership.renderer_active() {
            return Err(io::Error::other("CrabCode terminal is not active"));
        }
        if self.terminal_control_lease.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "standalone terminal controls must be parked before external resume healing",
            ));
        }

        let mode = self.ownership.mode();
        let mouse_capture = MOUSE_CAPTURE_ENABLED.load(Ordering::Acquire);
        let keyboard_enhancement = KEYBOARD_ENHANCEMENT_PUSHED.load(Ordering::Acquire);
        {
            let _output_guard = lock_terminal_output_for_active_write()?;
            let kernel_mutation =
                TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Kernel)?;
            crossterm::terminal::reassert_raw_mode()?;
            #[cfg(windows)]
            windows_console::configure_output();
            kernel_mutation.finish_after_kernel_mutation(
                terminal_writer_emergency_stopped,
                restore_aborted_terminal_kernel_acquisition,
            )?;
            let _setup_mutation =
                match TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Setup) {
                    Ok(guard) => guard,
                    Err(error) => {
                        if terminal_writer_emergency_stopped() {
                            restore_active_terminal_for_emergency(EmergencyOutputFence::Quiesced);
                        }
                        return Err(error);
                    }
                };
            set_terminal_title("");
            stop_terminal_setup_after_emergency()?;
            let mut stdout = io::stdout();
            let setup_result = write_terminal_setup(
                &mut stdout,
                mode,
                mouse_capture,
                terminal_context().mouse_reporting_leaks_as_raw_text(),
            );
            if let Err(error) = setup_result {
                if terminal_writer_emergency_stopped() {
                    let _ = stop_terminal_setup_after_emergency();
                }
                return Err(error);
            }
            stop_terminal_setup_after_emergency()?;
            apply_cursor_color(&mut stdout, mode);
            stop_terminal_setup_after_emergency()?;
            if keyboard_enhancement {
                stop_terminal_setup_after_emergency()?;
                let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
                // Kitty's keyboard protocol is a stack. The historical
                // direct TUI normalizes an external resume with pop-before-
                // push so repeated healing retains exactly one owned layer;
                // pop on an empty terminal-side stack is harmless.
                KEYBOARD_ENHANCEMENT_PUSHED.store(true, Ordering::Release);
                let keyboard_result = execute!(
                    stdout,
                    PopKeyboardEnhancementFlags,
                    PushKeyboardEnhancementFlags(flags)
                );
                if let Err(error) = keyboard_result {
                    if terminal_writer_emergency_stopped() {
                        let _ = stop_terminal_setup_after_emergency();
                    }
                    return Err(error);
                }
                stop_terminal_setup_after_emergency()?;
            }
        }

        self.last_rendered_frame = None;
        self.last_rendered_hyperlinks = None;
        self.cursor_state = CursorState::default();
        self.focus_heal = FocusHealState::default();
        // The next forced presentation samples CrosstermBackend::size and
        // performs resize + clear + repaint inside one synchronized frame.
        self.resize_pending = true;
        Ok(())
    }

    /// Re-acquire terminal ownership after a foreground resume.
    pub fn resume(&mut self) -> io::Result<()> {
        if self.ownership.renderer_active() {
            return Ok(());
        }
        // Resolve dimensions before taking raw/screen ownership so even a
        // terminal-size failure remains an ordinary restored-shell error.
        let (columns, rows) = crossterm::terminal::size()?;
        self.ownership.reacquire_protocol()?;
        let create_result =
            // The application already selected its action registry for this
            // effective mode. Fixed remains an equivalent Inline backend, but
            // a resume must not silently switch Minimal interaction semantics
            // without giving the application a matching state transition.
            create_terminal(
                self.ownership.mode(),
                Size::new(columns, rows),
                false,
                self.writer_sync.clone(),
            );
        let (terminal, writer_thread, effective_mode, setup_fallback) = match create_result {
            Ok(created) => created,
            Err(error) => {
                self.ownership.restore_incomplete_generation();
                return Err(error);
            }
        };
        self.terminal = Some(terminal);
        self.writer_thread = Some(writer_thread);
        self.ownership.set_effective_mode(effective_mode);
        if setup_fallback.is_some() {
            self.setup_fallback = setup_fallback;
        }
        self.last_rendered_frame = None;
        self.last_rendered_hyperlinks = None;
        self.cursor_state = CursorState::default();
        self.focus_heal = FocusHealState::default();
        self.backend_size = Size::new(columns, rows);
        self.resize_pending = false;
        if let Err(error) = self.ownership.publish_writer_owner() {
            return restore_after_failed_resume_step(
                error,
                self.suspend(),
                "terminal restoration after writer-owner publication failure",
            );
        }
        let resize_result = (|| {
            let _output_guard = lock_terminal_output_for_active_write()?;
            self.terminal_mut()
                .autoresize()
                .and_then(|()| self.terminal_mut().clear())
        })();
        if let Err(error) = resize_result {
            return restore_after_failed_resume_step(
                error,
                self.suspend(),
                "terminal restoration after autoresize/clear failure",
            );
        }
        if let Err(error) = self.install_terminal_control_lease() {
            return restore_after_failed_resume_step(
                error,
                self.suspend(),
                "terminal restoration after control-writer install failure",
            );
        }
        Ok(())
    }

    pub fn resized(&mut self) -> io::Result<()> {
        // Keep the resize event as renderer state only. The deferred
        // presentation consumes this bit and performs even a same-size
        // invalidation inside the synchronized frame transaction.
        self.resize_pending = true;
        Ok(())
    }

    /// Heal rows repainted by an embedded editor or multiplexer outside the
    /// renderer. Repeated FocusGained events without an intervening FocusLost
    /// are one incident and therefore trigger at most one synchronized clear.
    pub fn focus_changed(&mut self, gained: bool) -> io::Result<bool> {
        let policy_enabled =
            crate::terminal_capabilities::terminal_context().repaints_pane_out_of_band();
        if !self.focus_heal.observe(gained, policy_enabled) {
            return Ok(false);
        }
        let terminal = self
            .terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("CrabCode terminal is not active"))?;
        terminal
            .backend_mut()
            .frame_writer_mut()
            .begin_synchronized_frame()?;
        if let Err(error) = terminal.clear() {
            terminal
                .backend_mut()
                .frame_writer_mut()
                .abort_synchronized_frame();
            return Err(error);
        }
        if let Err(error) = terminal
            .backend_mut()
            .frame_writer_mut()
            .finish_synchronized_frame()
        {
            terminal
                .backend_mut()
                .frame_writer_mut()
                .abort_synchronized_frame();
            return Err(error);
        }
        self.last_rendered_frame = None;
        self.last_rendered_hyperlinks = None;
        self.cursor_state.mark_disturbed();
        Ok(true)
    }

    pub fn pending_signal(&self) -> Option<TerminalSignal> {
        self.ownership.pending_signal()
    }

    /// Detect a detached Unix PTY even when the terminal library has no input
    /// event to deliver. This is distinct from a temporarily idle stdin: the
    /// kernel reports `POLLHUP`/`POLLERR` only after the controlling reader
    /// disappears. Without this check a headless pane close can leave both the
    /// TUI and its private runtime child orphaned indefinitely.
    #[cfg(unix)]
    pub fn input_disconnected(&self) -> io::Result<bool> {
        use std::os::fd::AsFd as _;

        use nix::errno::Errno;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        let stdin = io::stdin();
        match nix::unistd::tcgetpgrp(&stdin) {
            Ok(_) => {}
            Err(Errno::EIO | Errno::ENOTTY | Errno::ENXIO) => return Ok(true),
            Err(Errno::EINTR) => return Ok(false),
            Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        }
        let mut descriptors = [PollFd::new(
            stdin.as_fd(),
            PollFlags::POLLHUP | PollFlags::POLLERR,
        )];
        let result = match poll(&mut descriptors, PollTimeout::ZERO) {
            Ok(result) => result,
            Err(Errno::EINTR) => return Ok(false),
            Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        };
        if result == 0 {
            return Ok(false);
        }
        let Some(events) = descriptors[0].revents() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal liveness poll returned unknown event flags",
            ));
        };
        Ok(events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL))
    }

    #[cfg(not(unix))]
    pub const fn input_disconnected(&self) -> io::Result<bool> {
        Ok(false)
    }

    /// Stop with SIGSTOP rather than re-raising SIGTSTP: SIGSTOP cannot be
    /// intercepted by our signal monitor. The shell's later SIGCONT lets this
    /// call return, after which the terminal is reacquired.
    #[cfg(unix)]
    pub fn stop_until_continued(&mut self) -> io::Result<()> {
        self.suspend()?;
        if let Some(signals) = self.ownership.signals.as_ref() {
            // This SIGCONT completes the explicit suspend owned by this call.
            // Suppress its later async Resume observation; the caller performs
            // the one input cutover after this method returns.
            signals.expect_internal_resume();
        }
        if let Err(error) = nix::sys::signal::raise(nix::sys::signal::Signal::SIGSTOP) {
            if let Some(signals) = self.ownership.signals.as_ref() {
                signals.cancel_internal_resume_expectation();
            }
            return Err(io::Error::other(error));
        }
        self.resume()
    }

    #[cfg(not(unix))]
    pub fn stop_until_continued(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "foreground suspend/resume is not implemented on this platform",
        ))
    }
}

/// Keep minimal mode's live viewport synchronized with the exact post-commit
/// content height. A commit's `insert_before` owns clearing and repositioning,
/// so only mutate the viewport area before that write; overlay/prompt-only
/// changes use `set_viewport_height` to clear stale rows safely.
fn sync_minimal_viewport<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    target: u16,
    will_commit: bool,
) -> io::Result<bool> {
    let current = terminal.viewport_area();
    let ceiling = terminal.last_known_area().height.max(1);
    let target = target.clamp(1, ceiling);
    if current.height == target {
        return Ok(false);
    }
    if will_commit {
        terminal.set_viewport_area(Rect {
            height: target,
            ..current
        });
    } else {
        terminal.set_viewport_height(target)?;
    }
    Ok(true)
}

#[cfg(test)]
fn reanchored_minimal_viewport(screen: Rect, current: Rect, cursor_row: u16) -> Rect {
    let height = current.height.max(1).min(screen.height.max(1));
    let available = screen.height.saturating_sub(cursor_row).saturating_sub(1);
    let top = cursor_row.saturating_sub(height.saturating_sub(1).saturating_sub(available));
    Rect {
        y: top,
        height,
        ..current
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

fn create_terminal(
    requested_mode: TerminalMode,
    size: Size,
    allow_semantic_fallback: bool,
    writer_sync: WriterSync,
) -> io::Result<(
    TuiTerminal,
    CrabCodeWriterThread,
    TerminalMode,
    Option<TerminalFallback>,
)> {
    let attempts = terminal_setup_attempts(requested_mode, size, allow_semantic_fallback);
    let mut last_error = None;
    for attempt in attempts {
        if matches!(attempt.viewport, TerminalViewportAttempt::Fixed(_)) {
            // The inline probes have fully dropped their backends and joined
            // their writers before this direct terminal mutation. Reserve the
            // main-screen rows that the fixed viewport will own.
            let _output_guard = lock_terminal_output_for_active_write()?;
            let mut stdout = io::stdout();
            execute!(stdout, ScrollUp(size.height), MoveTo(0, 0))?;
        }
        let (writer, writer_thread) = spawn_terminal_writer(writer_sync.clone())?;
        let mut terminal_attempt = PendingTerminalAttempt {
            writer_thread: Some(writer_thread),
        };
        let backend = CrosstermBackend::new(writer);
        let terminal_result = {
            // Inline construction can issue a direct CPR through Crossterm's
            // backend. Hold the restoration gate only for the constructor;
            // release it before any failed attempt drops and joins its writer.
            let _output_guard = lock_terminal_output_for_active_write()?;
            match attempt.viewport {
                TerminalViewportAttempt::Fullscreen => Terminal::new(backend),
                TerminalViewportAttempt::Inline(rows) => Terminal::with_options(
                    backend,
                    TerminalOptions {
                        viewport: Viewport::Inline(rows),
                    },
                ),
                TerminalViewportAttempt::Fixed(area) => Terminal::with_options(
                    backend,
                    TerminalOptions {
                        viewport: Viewport::Fixed(area),
                    },
                ),
            }
        };
        match terminal_result {
            Ok(terminal) => {
                let Some(writer_thread) = terminal_attempt.writer_thread.take() else {
                    return Err(io::Error::other(
                        "CrabCode terminal writer ownership was lost during setup",
                    ));
                };
                if let Err(error) =
                    apply_effective_terminal_mode(requested_mode, attempt.effective_mode)
                {
                    drop(terminal);
                    let _join_result = writer_thread.join();
                    return Err(error);
                }
                return Ok((
                    terminal,
                    writer_thread,
                    attempt.effective_mode,
                    attempt.fallback,
                ));
            }
            Err(error) => {
                // Constructor failure drops the consumed backend and sole
                // sender. Join before a retry so no bytes from a failed probe
                // can overtake the next terminal instance.
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("terminal setup produced no attempts")))
}

fn terminal_setup_attempts(
    requested_mode: TerminalMode,
    size: Size,
    allow_semantic_fallback: bool,
) -> Vec<TerminalSetupAttempt> {
    let full = Rect::new(0, 0, size.width, size.height);
    match requested_mode {
        TerminalMode::Fullscreen => vec![TerminalSetupAttempt {
            viewport: TerminalViewportAttempt::Fullscreen,
            effective_mode: TerminalMode::Fullscreen,
            fallback: None,
        }],
        TerminalMode::Inline => vec![
            TerminalSetupAttempt {
                viewport: TerminalViewportAttempt::Inline(size.height),
                effective_mode: TerminalMode::Inline,
                fallback: None,
            },
            TerminalSetupAttempt {
                viewport: TerminalViewportAttempt::Fixed(full),
                effective_mode: TerminalMode::Inline,
                fallback: Some(TerminalFallback::InlineToFixed),
            },
        ],
        TerminalMode::Minimal => {
            let mut attempts = vec![TerminalSetupAttempt {
                viewport: TerminalViewportAttempt::Inline(
                    MIN_TERMINAL_ROWS.min(size.height).max(1),
                ),
                effective_mode: TerminalMode::Minimal,
                fallback: None,
            }];
            if allow_semantic_fallback {
                attempts.extend([
                    TerminalSetupAttempt {
                        viewport: TerminalViewportAttempt::Inline(size.height),
                        effective_mode: TerminalMode::Inline,
                        fallback: Some(TerminalFallback::MinimalToInline),
                    },
                    TerminalSetupAttempt {
                        viewport: TerminalViewportAttempt::Fixed(full),
                        effective_mode: TerminalMode::Inline,
                        fallback: Some(TerminalFallback::MinimalToFixed),
                    },
                ]);
            }
            attempts
        }
    }
}

fn apply_effective_terminal_mode(
    requested_mode: TerminalMode,
    effective_mode: TerminalMode,
) -> io::Result<()> {
    ACTIVE_MODE.store(effective_mode as u8, Ordering::Release);
    if requested_mode == TerminalMode::Minimal && effective_mode == TerminalMode::Inline {
        // Minimal setup intentionally disables mouse reporting. Its verified
        // fallback is ordinary inline mode, so restore that input capability
        // before the event reader starts. Publish the cleanup obligation first
        // in case the escape write is partial.
        MOUSE_CAPTURE_ENABLED.store(true, Ordering::Release);
        let _output_guard = lock_terminal_output_for_active_write()?;
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    Ok(())
}

struct PendingTerminalAttempt {
    writer_thread: Option<CrabCodeWriterThread>,
}

impl Drop for PendingTerminalAttempt {
    fn drop(&mut self) {
        if let Some(writer_thread) = self.writer_thread.take() {
            let _ = writer_thread.join();
        }
    }
}

#[cfg(test)]
pub(crate) fn active_mode_is_minimal() -> bool {
    TerminalMode::from_atomic(ACTIVE_MODE.load(Ordering::Acquire)) == TerminalMode::Minimal
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MinimalWelcomeCard {
    title: &'static str,
    hint: &'static str,
    height: u16,
    wordmark: bool,
}

fn minimal_welcome_card(width: u16, language: crate::tui_app::UiLanguage) -> MinimalWelcomeCard {
    let wordmark = usize::from(width.saturating_sub(2)) >= crate::tui_ui::CRAB_WORDMARK_WIDTH;
    MinimalWelcomeCard {
        // These are the existing CrabCode empty-transcript strings. The
        // renderer lifecycle moves them into native scrollback; it does not
        // invent product metadata or require a backend field.
        title: language.text("CrabCode 已就绪。", "CrabCode is ready."),
        hint: language.text(
            "请在下方输入提示词。使用 /help 查看 TUI 操作说明。",
            "Type a prompt below. /help shows TUI controls.",
        ),
        // Rounded card (2) + vertical padding (2) + text content (2), plus
        // the fixed three-row wordmark and its one-row margin when the exact
        // 48-column banner fits, then one native-scrollback gap.
        height: if wordmark { 11 } else { 7 },
        wordmark,
    }
}

fn render_minimal_welcome_card(
    buffer: &mut Buffer,
    card: MinimalWelcomeCard,
    theme: CrabCodeTheme,
    theme_kind: CrabCodeThemeKind,
) {
    let card_area = Rect {
        height: buffer.area.height.saturating_sub(1),
        ..buffer.area
    };
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.gray_dim))
        .render(card_area, buffer);

    let x = card_area.x.saturating_add(2);
    let width = card_area.width.saturating_sub(4);
    let mut title_y = card_area.y.saturating_add(2);
    if card.wordmark {
        let mark_x = card_area.x.saturating_add(1);
        let mark_width = card_area.width.saturating_sub(2);
        for (row, line) in crate::tui_ui::crab_wordmark_lines(
            crate::tui_ui::CrabWordmarkLayout::Single,
            theme_kind,
        )
        .into_iter()
        .enumerate()
        {
            buffer.set_line(
                mark_x,
                card_area
                    .y
                    .saturating_add(1)
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                &line,
                mark_width,
            );
        }
        title_y = title_y.saturating_add(4);
    }
    buffer.set_line(
        x,
        title_y,
        &Line::from(Span::styled(
            card.title,
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )),
        width,
    );
    buffer.set_line(
        x,
        title_y.saturating_add(1),
        &Line::from(Span::styled(card.hint, Style::default().fg(theme.gray))),
        width,
    );
}

fn commit_minimal_welcome_with(
    app: &mut TuiApp,
    width: u16,
    emit: impl FnOnce(MinimalWelcomeCard) -> io::Result<()>,
) -> io::Result<bool> {
    if app.setup_surface_exclusive()
        || !app.minimal_welcome_pending()
        || width < MINIMAL_WELCOME_MIN_WIDTH
    {
        return Ok(false);
    }
    let card = minimal_welcome_card(width, app.ui_language());
    emit(card)?;
    // Clear only after the native insertion succeeds. If terminal output
    // fails, the same fresh-session card remains retryable next frame.
    app.mark_minimal_welcome_committed();
    Ok(true)
}

fn commit_minimal_welcome<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
) -> io::Result<bool> {
    let width = terminal.viewport_area().width;
    let theme = app.renderer_theme();
    let theme_kind = app.renderer_theme_kind();
    commit_minimal_welcome_with(app, width, |card| {
        terminal.insert_before(card.height, move |buffer| {
            render_minimal_welcome_card(buffer, card, theme, theme_kind);
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmergencyAfterRestore {
    ContinueQuiesced,
    ExitContended,
}

const fn emergency_after_restore(fence: EmergencyOutputFence) -> EmergencyAfterRestore {
    match fence {
        EmergencyOutputFence::Quiesced => EmergencyAfterRestore::ContinueQuiesced,
        EmergencyOutputFence::Contended => EmergencyAfterRestore::ExitContended,
    }
}

fn complete_panic_cleanup_with(
    fence: EmergencyOutputFence,
    restore: impl FnOnce(EmergencyOutputFence),
    kill_runtimes: impl FnOnce(),
    report: impl FnOnce(),
) -> EmergencyAfterRestore {
    restore(fence);
    let after_restore = emergency_after_restore(fence);
    if after_restore == EmergencyAfterRestore::ContinueQuiesced {
        kill_runtimes();
        report();
    }
    after_restore
}

fn install_restore_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // This process-wide hook is installed lazily by the first TUI
            // session, but it must not change panic policy before ownership,
            // after normal teardown, between later terminal generations, or
            // while a blocking child owns the cooked terminal.
            if EmergencyTerminalPhase::current() != EmergencyTerminalPhase::ProtocolActive
                || !TERMINAL_OWNED.load(Ordering::Acquire)
            {
                previous(info);
                return;
            }
            let writer_guard = crate::terminal_writer::emergency_stop_terminal_writer();
            let fence = writer_guard.output_fence();
            let after_restore = complete_panic_cleanup_with(
                fence,
                restore_active_terminal_for_emergency,
                crate::sdk_runtime::try_force_kill_active_runtimes,
                || previous(info),
            );
            if after_restore == EmergencyAfterRestore::ExitContended {
                // An uncancellable write may resume as soon as this thread
                // yields. After exact kernel restoration, exit immediately:
                // no child traversal, reporting, hook, or destructor may
                // create a cooked-shell late-write window.
                fatal_process_exit(101);
            }
            // Match the fixed mother lifecycle: after terminal restoration
            // is serialized, terminate the registered direct runtime before
            // delegating diagnostics to the previous hook. The emergency
            // helper is lock-bounded and does not initialize new state.
        }));
    });
}

/// Drain terminal replies or keystrokes that predate this renderer ownership
/// generation. The post-setup VTE grace matches the fixed Rust lifecycle;
/// normal teardown gets a separate 10ms drain after output ownership ends.
fn drain_pending_events_with_timeout(timeout: std::time::Duration) {
    while crossterm::event::poll(timeout).unwrap_or(false) {
        if crossterm::event::read().is_err() {
            break;
        }
    }
}

/// Sanitize one terminal title before crossterm embeds it in an OSC sequence.
///
/// Session titles are backend-owned text. Removing every control character
/// prevents BEL/ESC from terminating the title OSC and injecting a second
/// terminal command. The fixed renderer's 80-character ceiling is retained;
/// CrabCode's historical product title supplies the actual visible wording.
fn terminal_title_string(title: &str) -> String {
    let sanitized = title
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    if sanitized.is_empty() {
        "CrabCode".to_string()
    } else {
        const PRODUCT_SUFFIX: &str = " - CrabCode";
        let title_budget = 80_usize.saturating_sub(PRODUCT_SUFFIX.chars().count());
        let truncated = sanitized.chars().take(title_budget).collect::<String>();
        format!("{truncated}{PRODUCT_SUFFIX}")
    }
}

pub(crate) fn set_terminal_title(title: &str) {
    if env_value_is_truthy(
        std::env::var(LEGACY_DISABLE_TERMINAL_TITLE_ENV)
            .ok()
            .as_deref(),
    ) {
        return;
    }
    // Publish the matching clear obligation before the OSC write; a failed
    // write can still have partially changed the host title.
    TERMINAL_TITLE_CHANGED.store(true, Ordering::Release);
    let title = terminal_title_string(title);
    let _ = execute!(io::stdout(), SetTitle(title));
}

fn clear_terminal_title() {
    if !TERMINAL_TITLE_CHANGED.swap(false, Ordering::AcqRel) {
        return;
    }
    // Historical CrabCode clears its title on every path that changed it.
    let _ = execute!(io::stdout(), SetTitle(""));
}

/// Apply the fixed renderer's OSC 12 cursor-color lifecycle to the concrete
/// renderer palette. Minimal is terminal-native upstream and therefore
/// intentionally leaves the profile cursor untouched.
fn apply_cursor_color(writer: &mut impl io::Write, mode: TerminalMode) {
    let Some(sequence) = cursor_color_sequence(
        mode,
        std::env::var_os("NO_COLOR").is_some(),
        CrabCodeTheme::current(),
    ) else {
        return;
    };
    // A partial OSC write may already have changed the cursor color. Publish
    // the reset obligation before touching the terminal.
    CURSOR_COLOR_CHANGED.store(true, Ordering::Release);
    let _ = writer.write_all(sequence.as_bytes());
    let _ = writer.flush();
}

fn cursor_color_sequence(
    mode: TerminalMode,
    no_color: bool,
    theme: CrabCodeTheme,
) -> Option<String> {
    if mode == TerminalMode::Minimal || no_color {
        return None;
    }
    let Color::Rgb(red, green, blue) = theme.accent_user else {
        return None;
    };
    Some(format!("\x1b]12;rgb:{red:02x}/{green:02x}/{blue:02x}\x07"))
}

fn cursor_color_requires_late_apply(requested: TerminalMode, effective: TerminalMode) -> bool {
    requested == TerminalMode::Minimal && effective != TerminalMode::Minimal
}

#[cfg(unix)]
fn open_validated_emergency_terminal_output(
    path: *const libc::c_char,
    reference_fd: i32,
) -> io::Result<i32> {
    // SAFETY: `path` is a live NUL-terminated ttyname buffer or the static
    // `/dev/tty` fallback. O_NOCTTY prevents this safety descriptor from
    // changing session ownership.
    let fd = unsafe {
        libc::open(
            path,
            libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let validate = (|| {
        // SAFETY: both stat destinations are initialized by successful fstat
        // calls before they are assumed initialized.
        let mut stdout_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let mut output_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(reference_fd, stdout_stat.as_mut_ptr()) } != 0
            || unsafe { libc::fstat(fd, output_stat.as_mut_ptr()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both preceding fstat calls succeeded.
        let stdout_stat = unsafe { stdout_stat.assume_init() };
        let output_stat = unsafe { output_stat.assume_init() };
        if stdout_stat.st_rdev != output_stat.st_rdev
            || output_stat.st_mode & libc::S_IFMT != libc::S_IFCHR
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "emergency terminal output does not identify the stdout terminal device",
            ));
        }
        Ok(())
    })();
    if let Err(error) = validate {
        // SAFETY: this branch owns the descriptor returned by open.
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    Ok(fd)
}

#[cfg(unix)]
fn open_emergency_terminal_output() -> io::Result<i32> {
    let mut tty_name = [0 as libc::c_char; 1024];
    // SAFETY: tty_name is a writable buffer and ttyname_r receives its exact
    // length. TerminalSession has already required stdout to be a tty.
    let name_result =
        unsafe { libc::ttyname_r(libc::STDOUT_FILENO, tty_name.as_mut_ptr(), tty_name.len()) };
    let primary = if name_result == 0 {
        open_validated_emergency_terminal_output(tty_name.as_ptr(), libc::STDOUT_FILENO)
    } else {
        Err(io::Error::from_raw_os_error(name_result))
    };
    let fd = match primary {
        Ok(fd) => fd,
        Err(primary_error) => {
            let fallback = c"/dev/tty";
            open_validated_emergency_terminal_output(fallback.as_ptr(), libc::STDOUT_FILENO)
                .map_err(|fallback_error| {
                    io::Error::new(
                        fallback_error.kind(),
                        format!(
                            "cannot open independent emergency terminal output from stdout \
                             ({primary_error}) or /dev/tty ({fallback_error})"
                        ),
                    )
                })?
        }
    };
    Ok(fd)
}

#[cfg(unix)]
fn capture_emergency_terminal_baseline() -> io::Result<()> {
    if TERMINAL_OWNED.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot replace the emergency terminal baseline while a generation is active",
        ));
    }
    let mut baseline = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: baseline points to writable termios storage for stdin.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, baseline.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Keep a stable descriptor for the same terminal OFD. The inherited fd 0
    // can be closed or replaced by later process-local code; emergency
    // tcsetattr must never target whatever happens to reuse that number.
    let termios_fd =
        unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0 as libc::c_int) };
    if termios_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // The independent visual descriptor is a best-effort enhancement. A
    // restrictive tty mount or host may reject the reopen even though stdin
    // and stdout are valid interactive terminals; that must not make normal
    // TUI startup unavailable.
    let output_fd = open_emergency_terminal_output().unwrap_or(-1);
    // SAFETY: tcgetattr succeeded. Replacing a capsule takes the exclusive
    // write side of the baseline lock, so its descriptors cannot be reclaimed
    // while one or more emergency readers hold shared guards.
    let baseline = EmergencyTerminalBaseline {
        termios: unsafe { baseline.assume_init() },
        termios_fd,
        output_fd,
    };
    *EMERGENCY_TERMINAL_BASELINE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(baseline);
    Ok(())
}

#[cfg(windows)]
fn capture_emergency_terminal_baseline() -> io::Result<()> {
    if TERMINAL_OWNED.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot replace the emergency terminal baseline while a generation is active",
        ));
    }
    windows_console::capture_baseline();
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn capture_emergency_terminal_baseline() -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn try_emergency_terminal_baseline()
-> Option<std::sync::RwLockReadGuard<'static, Option<EmergencyTerminalBaseline>>> {
    match EMERGENCY_TERMINAL_BASELINE.try_read() {
        Ok(baseline) => Some(baseline),
        Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

#[cfg(unix)]
fn write_emergency_terminal_bytes_to(fd: i32, bytes: &[u8]) {
    if fd < 0 {
        return;
    }
    let mut written = 0_usize;
    // A fixed attempt ceiling makes EINTR and partial writes bounded. EAGAIN
    // simply leaves visual cleanup incomplete; kernel termios restoration is
    // the authoritative emergency guarantee.
    for _ in 0..8 {
        if written >= bytes.len() {
            break;
        }
        // SAFETY: the baseline guard keeps fd live and bytes remains valid.
        let result =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if result > 0 {
            written = written.saturating_add(result as usize);
            continue;
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break;
    }
}

#[cfg(windows)]
fn write_emergency_terminal_bytes(bytes: &[u8]) {
    windows_console::write_emergency_bytes(bytes);
}

/// Win32 unhandled-exception-filter seam. The terminal-only fault module calls
/// this only for the fixed fatal exception set while TUI protocol ownership is
/// active; it performs one allocation-free write to the existing stdout route.
#[cfg(windows)]
pub(crate) fn write_fatal_fault_terminal_restore() {
    windows_console::write_emergency_bytes(FATAL_FAULT_TERMINAL_RESTORE);
}

#[cfg(not(any(unix, windows)))]
fn write_emergency_terminal_bytes(_bytes: &[u8]) {}

#[cfg(any(windows, test))]
mod windows_console {
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct ConsoleBaseline {
        pub(super) stdin_mode: Option<u32>,
        pub(super) stdout_mode: Option<u32>,
        pub(super) output_code_page: Option<u32>,
    }

    pub(super) fn restore_console_baseline_with(
        baseline: ConsoleBaseline,
        mut restore_stdin: impl FnMut(u32),
        mut restore_stdout: impl FnMut(u32),
        mut restore_output_code_page: impl FnMut(u32),
    ) {
        if let Some(mode) = baseline.stdin_mode {
            restore_stdin(mode);
        }
        if let Some(mode) = baseline.stdout_mode {
            restore_stdout(mode);
        }
        if let Some(code_page) = baseline.output_code_page {
            restore_output_code_page(code_page);
        }
    }

    pub(super) const fn native_selection_mode(mode: u32) -> u32 {
        (mode & !ENABLE_MOUSE_INPUT)
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_WINDOW_INPUT
    }

    pub(super) const fn virtual_terminal_output_mode(mode: u32) -> u32 {
        mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    }

    #[cfg(windows)]
    mod imp {
        use std::sync::{RwLock, TryLockError};

        const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
        const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
        const CP_UTF8: u32 = 65001;

        // One immutable, generation-scoped snapshot prevents simultaneous
        // fatal restorers from consuming different fields. Capture replaces
        // the complete value only while the terminal is cooked and unowned;
        // emergency readers never wait behind that replacement.
        static CONSOLE_BASELINE: RwLock<Option<super::ConsoleBaseline>> = RwLock::new(None);

        unsafe extern "system" {
            fn GetStdHandle(standard_handle: u32) -> *mut core::ffi::c_void;
            fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
            fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: u32) -> i32;
            fn GetConsoleOutputCP() -> u32;
            fn SetConsoleOutputCP(code_page: u32) -> i32;
        }

        fn console_mode(standard_handle: u32) -> Option<(*mut core::ffi::c_void, u32)> {
            // SAFETY: Win32 validates the standard-handle identifier and
            // writes one u32 only when the handle is a console.
            unsafe {
                let handle = GetStdHandle(standard_handle);
                if handle.is_null() || handle == -1_isize as *mut _ {
                    return None;
                }
                let mut mode = 0;
                if GetConsoleMode(handle, &mut mode) == 0 {
                    return None;
                }
                Some((handle, mode))
            }
        }

        pub(super) fn capture_baseline() {
            let stdin_mode = console_mode(STD_INPUT_HANDLE).map(|(_handle, mode)| mode);
            let stdout_mode = console_mode(STD_OUTPUT_HANDLE).map(|(_handle, mode)| mode);
            // SAFETY: GetConsoleOutputCP carries no pointers. Zero denotes an
            // unavailable console code page and is not a restorable baseline.
            let code_page = unsafe { GetConsoleOutputCP() };
            let baseline = super::ConsoleBaseline {
                stdin_mode,
                stdout_mode,
                output_code_page: (code_page != 0).then_some(code_page),
            };
            *CONSOLE_BASELINE
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(baseline);
        }

        pub(super) fn write_emergency_bytes(bytes: &[u8]) {
            unsafe extern "system" {
                fn WriteFile(
                    file: *mut core::ffi::c_void,
                    buffer: *const u8,
                    bytes_to_write: u32,
                    bytes_written: *mut u32,
                    overlapped: *mut core::ffi::c_void,
                ) -> i32;
            }

            // Fixed-upstream Windows crash restoration writes directly to a
            // standard console handle. This bypasses Rust formatting,
            // allocation, Crossterm's raw-mode mutex, and the stdout writer.
            unsafe {
                let Some((handle, _mode)) = console_mode(STD_OUTPUT_HANDLE) else {
                    return;
                };
                let Ok(length) = u32::try_from(bytes.len()) else {
                    return;
                };
                let mut written = 0_u32;
                let _ = WriteFile(
                    handle,
                    bytes.as_ptr(),
                    length,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
        }

        pub(super) fn configure_output() {
            // SAFETY: code-page APIs carry no pointers. A zero result from
            // GetConsoleOutputCP means unavailable.
            let original_code_page = unsafe { GetConsoleOutputCP() };
            if original_code_page != 0 && original_code_page != CP_UTF8 {
                unsafe {
                    let _ = SetConsoleOutputCP(CP_UTF8);
                }
            }

            let Some((handle, mode)) = console_mode(STD_OUTPUT_HANDLE) else {
                return;
            };
            let configured = super::virtual_terminal_output_mode(mode);
            if configured != mode {
                unsafe {
                    let _ = SetConsoleMode(handle, configured);
                }
            }
        }

        pub(super) fn enable_native_selection() {
            let Some((handle, mode)) = console_mode(STD_INPUT_HANDLE) else {
                return;
            };
            let configured = super::native_selection_mode(mode);
            if configured != mode {
                unsafe {
                    let _ = SetConsoleMode(handle, configured);
                }
            }
        }

        pub(super) fn restore() {
            let baseline = match CONSOLE_BASELINE.try_read() {
                Ok(baseline) => baseline,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                // A writer exists only while publishing a freshly captured
                // cooked baseline. Skipping that old generation is safer than
                // waiting in a fatal path.
                Err(TryLockError::WouldBlock) => return,
            };
            let Some(baseline) = *baseline else {
                return;
            };

            super::restore_console_baseline_with(
                baseline,
                |stdin_mode| {
                    if let Some((handle, current)) = console_mode(STD_INPUT_HANDLE)
                        && current != stdin_mode
                    {
                        unsafe {
                            let _ = SetConsoleMode(handle, stdin_mode);
                        }
                    }
                },
                |stdout_mode| {
                    if let Some((handle, current)) = console_mode(STD_OUTPUT_HANDLE)
                        && current != stdout_mode
                    {
                        unsafe {
                            let _ = SetConsoleMode(handle, stdout_mode);
                        }
                    }
                },
                |code_page| unsafe {
                    let _ = SetConsoleOutputCP(code_page);
                },
            );
        }
    }

    #[cfg(windows)]
    pub(super) fn capture_baseline() {
        imp::capture_baseline();
    }

    #[cfg(windows)]
    pub(super) fn configure_output() {
        imp::configure_output();
    }

    #[cfg(windows)]
    pub(super) fn enable_native_selection() {
        imp::enable_native_selection();
    }

    #[cfg(windows)]
    pub(super) fn restore() {
        imp::restore();
    }

    #[cfg(windows)]
    pub(super) fn write_emergency_bytes(bytes: &[u8]) {
        imp::write_emergency_bytes(bytes);
    }
}

const fn mouse_capture_for_mode(
    mode: TerminalMode,
    disable_fullscreen_mouse_tracking: bool,
) -> bool {
    match mode {
        TerminalMode::Minimal => false,
        TerminalMode::Fullscreen => !disable_fullscreen_mouse_tracking,
        TerminalMode::Inline => true,
    }
}

fn enter_terminal_mode(mode: TerminalMode, mouse_capture: bool) -> io::Result<()> {
    // Signal/panic restoration uses this same re-entrant gate. The short
    // Kernel phase below contains no terminal output and is the only phase a
    // contended fatal restorer may wait for. Setup may block in the terminal
    // driver, so emergency cleanup latches it and exits rather than waiting.
    let _output_guard = lock_terminal_output_for_active_write()?;
    let kernel_mutation =
        TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Kernel)?;
    TERMINAL_OWNED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another CrabCode terminal session already owns this process terminal",
            )
        })?;
    EmergencyTerminalPhase::ProtocolActive.publish();
    ACTIVE_MODE.store(mode as u8, Ordering::Release);
    if let Err(error) = enable_raw_mode() {
        TERMINAL_OWNED.store(false, Ordering::Release);
        EmergencyTerminalPhase::Restored.publish();
        return Err(error);
    }
    #[cfg(windows)]
    windows_console::configure_output();
    #[cfg(windows)]
    if !mouse_capture {
        windows_console::enable_native_selection();
    }
    kernel_mutation.finish_after_kernel_mutation(
        terminal_writer_emergency_stopped,
        restore_aborted_terminal_kernel_acquisition,
    )?;

    let _setup_mutation =
        match TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Setup) {
            Ok(guard) => guard,
            Err(error) => {
                if terminal_writer_emergency_stopped() {
                    restore_active_terminal_for_emergency(EmergencyOutputFence::Quiesced);
                } else {
                    restore_active_terminal(None);
                }
                return Err(error);
            }
        };
    // Publish ownership before emitting multi-command setup. If a backend or
    // writer panics after the first escape sequence, the process panic hook
    // can still restore raw mode, cursor visibility, and screen ownership.
    // Publish the conservative cleanup obligation before the first setup
    // byte. A partial write may have enabled one of crossterm's five mouse
    // modes even when the setup call returns an error.
    MOUSE_CAPTURE_ENABLED.store(mouse_capture, Ordering::Release);
    stop_terminal_setup_after_emergency()?;
    drain_pending_events_with_timeout(std::time::Duration::ZERO);
    stop_terminal_setup_after_emergency()?;
    set_terminal_title("");
    stop_terminal_setup_after_emergency()?;
    let mut stdout = io::stdout();
    let result = write_terminal_setup(
        &mut stdout,
        mode,
        mouse_capture,
        terminal_context().mouse_reporting_leaks_as_raw_text(),
    );
    if let Err(error) = result {
        if terminal_writer_emergency_stopped() {
            restore_active_terminal_for_emergency(EmergencyOutputFence::Quiesced);
        } else {
            restore_active_terminal(None);
        }
        return Err(error);
    }
    stop_terminal_setup_after_emergency()?;
    let drain_timeout = if terminal_context().vte_version.is_some() {
        std::time::Duration::from_millis(20)
    } else {
        std::time::Duration::ZERO
    };
    drain_pending_events_with_timeout(drain_timeout);
    stop_terminal_setup_after_emergency()?;
    apply_cursor_color(&mut stdout, mode);
    stop_terminal_setup_after_emergency()?;
    let context = crate::terminal_capabilities::terminal_context();
    if context.kitty_skip_reason().is_none()
        && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
    {
        stop_terminal_setup_after_emergency()?;
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        // Publish the pop obligation before the multi-byte push. A failed
        // write may still have installed the keyboard layer in the terminal;
        // an unmatched pop is harmless, while omitting it leaks CSI-u mode
        // into the parent shell.
        KEYBOARD_ENHANCEMENT_PUSHED.store(true, Ordering::Release);
        let _ = execute!(stdout, PushKeyboardEnhancementFlags(flags));
        stop_terminal_setup_after_emergency()?;
    }
    Ok(())
}

fn stop_terminal_setup_after_emergency() -> io::Result<()> {
    if !terminal_writer_emergency_stopped() {
        return Ok(());
    }
    // This thread owns the output gate and is between setup operations, so it
    // can complete the visual and kernel rollback that the contended fatal
    // thread deliberately skipped.
    restore_active_terminal_for_emergency(EmergencyOutputFence::Quiesced);
    Err(terminal_protocol_emergency_error())
}

/// Drop the terminal (closing the writer channel) and join the writer thread.
/// After this returns, direct stdout teardown bytes are guaranteed to land
/// strictly after every accepted frame.
fn drain_writer_thread_before_teardown(
    terminal: TuiTerminal,
    writer_thread: CrabCodeWriterThread,
) -> io::Result<()> {
    drop(terminal);
    writer_thread.join()
}

fn merge_terminal_results(first: io::Result<()>, second: io::Result<()>) -> io::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(io::Error::new(
            first.kind(),
            format!("{first}; terminal restoration also failed: {second}"),
        )),
    }
}

fn restore_after_failed_resume_step(
    primary: io::Error,
    restoration: io::Result<()>,
    restoration_context: &str,
) -> io::Result<()> {
    match restoration {
        Ok(()) => Err(primary),
        Err(restoration_error) => Err(io::Error::new(
            primary.kind(),
            format!("{primary}; {restoration_context} also failed: {restoration_error}"),
        )),
    }
}

fn retain_first_terminal_error(outcome: &mut io::Result<()>, next: io::Result<()>) {
    if outcome.is_ok()
        && let Err(error) = next
    {
        *outcome = Err(error);
    }
}

const fn should_clear_terminal_before_restore(
    mode: TerminalMode,
    writer_failed: bool,
    terminal_owned: bool,
) -> bool {
    terminal_owned && mode.is_fullscreen() && !writer_failed
}

/// Consume one terminal/writer ownership generation and restore the shell.
///
/// This is the fixed-upstream restoration transaction with only the proven
/// CrabCode output/global-state adapters substituted: a healthy fullscreen
/// generation emits its final clear, the ordered writer is drained, teardown
/// runs even when that drain fails, then pending replies and raw mode are
/// restored.
fn restore_terminal_with(
    mut terminal: TuiTerminal,
    writer_thread: CrabCodeWriterThread,
    mode: TerminalMode,
    drain: impl FnOnce(TuiTerminal, CrabCodeWriterThread) -> io::Result<()>,
    teardown: impl FnOnce(TerminalMode, Option<u16>),
) -> io::Result<()> {
    if should_clear_terminal_before_restore(
        mode,
        writer_thread.writer_sync().failed(),
        TERMINAL_OWNED.load(Ordering::Acquire),
    ) {
        let _ = terminal.clear();
        {
            use std::io::Write;
            let _ = terminal.backend_mut().flush();
        }
    }
    let inline_cursor_row = (!mode.is_fullscreen()).then(|| terminal.viewport_area().bottom());
    let drain_result = drain(terminal, writer_thread);
    // Fixed-upstream ownership publication rule: direct teardown, terminal
    // reply drain, and tcsetattr restoration are one serialized transaction.
    // A second signal must continue to observe TERMINAL_OWNED until the shell
    // has actually reclaimed the tty; otherwise it can exit between escape
    // sequences and leave raw mode or mouse capture behind.
    let _output_guard = lock_terminal_output_for_restore();
    teardown(mode, inline_cursor_row);
    drain_pending_events_with_timeout(std::time::Duration::from_millis(10));
    let _ = disable_raw_mode();
    // Crossterm's Windows raw-mode restore normalizes several input bits.
    // Reapply the exact pre-session console snapshot last so uncommon but
    // valid LINE/ECHO/PROCESSED baselines are not silently broadened.
    #[cfg(windows)]
    windows_console::restore();
    TERMINAL_OWNED.store(false, Ordering::Release);
    EmergencyTerminalPhase::Restored.publish();
    drain_result
}

fn restore_terminal(
    terminal: TuiTerminal,
    writer_thread: CrabCodeWriterThread,
    mode: TerminalMode,
) -> io::Result<()> {
    restore_terminal_with(
        terminal,
        writer_thread,
        mode,
        drain_writer_thread_before_teardown,
        restore_owned_terminal_sequences,
    )
}

/// Fixed child handoff screen adapter. The parent terminal and writer remain
/// live; the event-loop state machine has already parked input and drained the
/// writer before this direct stdout mutation.
fn release_tty_for_child(mode: TerminalMode) -> io::Result<()> {
    let _output_guard = lock_terminal_output_for_active_write()?;
    if mode.uses_alternate_screen() {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
    let _ = disable_raw_mode();
    #[cfg(windows)]
    windows_console::restore();
    // The blocking child now owns the cooked terminal. Reacquisition must
    // capture and publish a fresh baseline before raw mode is re-entered.
    EmergencyTerminalPhase::ChildHandoff.publish();
    TERMINAL_OWNED.store(false, Ordering::Release);
    Ok(())
}

/// Reverse [`release_tty_for_child`] without constructing a new terminal or
/// writer generation.
fn reacquire_tty_after_child(mode: TerminalMode) -> io::Result<()> {
    let _output_guard = lock_terminal_output_for_active_write()?;
    capture_emergency_terminal_baseline()?;
    let kernel_mutation =
        TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Kernel)?;
    TERMINAL_OWNED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            EmergencyTerminalPhase::ChildHandoff.publish();
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal ownership changed during child reacquisition",
            )
        })?;
    EmergencyTerminalPhase::ProtocolActive.publish();
    if let Err(error) = enable_raw_mode() {
        TERMINAL_OWNED.store(false, Ordering::Release);
        EmergencyTerminalPhase::ChildHandoff.publish();
        #[cfg(windows)]
        windows_console::restore();
        return Err(error);
    }
    #[cfg(windows)]
    windows_console::configure_output();
    kernel_mutation.finish_after_kernel_mutation(
        terminal_writer_emergency_stopped,
        restore_aborted_child_kernel_acquisition,
    )?;

    let _setup_mutation =
        match TerminalAcquisitionMutationGuard::begin(TerminalAcquisitionMutation::Setup) {
            Ok(guard) => guard,
            Err(error) => {
                if terminal_writer_emergency_stopped() {
                    restore_active_terminal_for_emergency(EmergencyOutputFence::Quiesced);
                } else {
                    let _ = disable_raw_mode();
                    #[cfg(windows)]
                    windows_console::restore();
                    TERMINAL_OWNED.store(false, Ordering::Release);
                    EmergencyTerminalPhase::ChildHandoff.publish();
                }
                return Err(error);
            }
        };
    if mode.uses_alternate_screen()
        && let Err(error) = execute!(io::stdout(), EnterAlternateScreen)
    {
        let _ = disable_raw_mode();
        #[cfg(windows)]
        windows_console::restore();
        TERMINAL_OWNED.store(false, Ordering::Release);
        EmergencyTerminalPhase::ChildHandoff.publish();
        return Err(error);
    }
    stop_terminal_setup_after_emergency()?;
    Ok(())
}

fn restore_active_terminal(inline_cursor_row: Option<u16>) {
    restore_active_terminal_with(inline_cursor_row, false);
}

fn restore_active_terminal_normally(inline_cursor_row: Option<u16>) {
    restore_active_terminal_with(inline_cursor_row, true);
}

/// Roll back the output-free raw/console acquisition critical section.
///
/// No terminal escape has been emitted at this point, so exact kernel state
/// is both necessary and sufficient. The mutation guard remains published as
/// `Kernel` until this function returns, which lets a contended fatal restorer
/// wait for this bounded rollback without ever waiting on terminal output.
fn restore_aborted_terminal_kernel_acquisition() {
    restore_aborted_terminal_kernel_acquisition_to(EmergencyTerminalPhase::Restored);
}

fn restore_aborted_child_kernel_acquisition() {
    restore_aborted_terminal_kernel_acquisition_to(EmergencyTerminalPhase::ChildHandoff);
}

fn restore_aborted_terminal_kernel_acquisition_to(restored_phase: EmergencyTerminalPhase) {
    #[cfg(unix)]
    if let Some(baseline_guard) = try_emergency_terminal_baseline()
        && let Some(baseline) = baseline_guard.as_ref()
    {
        // SAFETY: the read guard keeps the immutable descriptor and termios
        // snapshot alive through the immediate restoration.
        unsafe {
            libc::tcsetattr(
                baseline.termios_fd,
                libc::TCSANOW,
                std::ptr::addr_of!(baseline.termios),
            );
        }
    }
    #[cfg(windows)]
    windows_console::restore();
    KEYBOARD_ENHANCEMENT_PUSHED.store(false, Ordering::Release);
    MOUSE_CAPTURE_ENABLED.store(false, Ordering::Release);
    CURSOR_COLOR_CHANGED.store(false, Ordering::Release);
    TERMINAL_TITLE_CHANGED.store(false, Ordering::Release);
    TERMINAL_OWNED.store(false, Ordering::Release);
    restored_phase.publish();
}

/// Emergency-only terminal restoration.
///
/// This path never calls Crossterm's raw-mode mutex and never writes through
/// inherited stdout. On Unix, a quiesced writer permits a fixed-attempt,
/// nonblocking visual reset through the pre-opened independent tty descriptor.
/// The fixed Windows console path uses one synchronous best-effort `WriteFile`
/// and therefore has no claimed hard deadline. Under output contention, exact
/// kernel state wins and all visual output is skipped before the caller
/// terminates the process.
fn restore_active_terminal_for_emergency(fence: EmergencyOutputFence) {
    wait_for_kernel_acquisition_rollback(fence);
    if !TERMINAL_OWNED.load(Ordering::Acquire) {
        return;
    }

    // Kernel state is authoritative. Hold one shared Unix baseline guard for
    // the complete restoration so simultaneous fatal readers can both apply
    // the same termios snapshot and neither can race descriptor reclamation.
    #[cfg(unix)]
    let baseline_guard = try_emergency_terminal_baseline();
    #[cfg(unix)]
    let baseline = baseline_guard
        .as_deref()
        .and_then(std::option::Option::as_ref);
    #[cfg(unix)]
    if let Some(baseline) = baseline {
        // SAFETY: the shared guard keeps this generation's stable descriptor
        // and termios snapshot alive for the complete ioctl and visual tail.
        unsafe {
            libc::tcsetattr(
                baseline.termios_fd,
                libc::TCSANOW,
                std::ptr::addr_of!(baseline.termios),
            );
        }
    }
    #[cfg(windows)]
    windows_console::restore();

    if fence == EmergencyOutputFence::Quiesced {
        let write_visual = |bytes: &[u8]| {
            #[cfg(unix)]
            if let Some(baseline) = baseline {
                write_emergency_terminal_bytes_to(baseline.output_fd, bytes);
            }
            #[cfg(windows)]
            write_emergency_terminal_bytes(bytes);
            #[cfg(not(any(unix, windows)))]
            let _ = bytes;
        };
        let mode = TerminalMode::from_atomic(ACTIVE_MODE.load(Ordering::Acquire));
        let mouse_capture = MOUSE_CAPTURE_ENABLED.load(Ordering::Acquire);
        let keyboard_enhancement = KEYBOARD_ENHANCEMENT_PUSHED.load(Ordering::Acquire);
        if mode.uses_alternate_screen() && mouse_capture && keyboard_enhancement {
            // The common fullscreen state is byte-for-byte the fixed mother
            // lifecycle's canonical crash restore sequence.
            write_visual(EMERGENCY_TERMINAL_RESTORE);
        } else {
            // Inline/minimal and partial-setup generations must not pop an
            // outer keyboard stack or leave an alternate screen they never
            // entered. Emit only obligations this generation published.
            write_visual(EMERGENCY_SYNC_CURSOR_RESTORE);
            if mouse_capture {
                write_visual(MOUSE_TRACKING_RESET);
            }
            write_visual(EMERGENCY_PASTE_FOCUS_RESTORE);
            if keyboard_enhancement {
                write_visual(EMERGENCY_KEYBOARD_RESTORE);
            }
            if mode.uses_alternate_screen() {
                write_visual(EMERGENCY_ALT_SCREEN_RESTORE);
            }
        }
        if CURSOR_COLOR_CHANGED.load(Ordering::Acquire) {
            write_visual(EMERGENCY_CURSOR_COLOR_RESET);
        }
        if TERMINAL_TITLE_CHANGED.load(Ordering::Acquire) {
            write_visual(EMERGENCY_TITLE_CLEAR);
        }
    }

    if fence == EmergencyOutputFence::Quiesced {
        KEYBOARD_ENHANCEMENT_PUSHED.store(false, Ordering::Release);
        MOUSE_CAPTURE_ENABLED.store(false, Ordering::Release);
        CURSOR_COLOR_CHANGED.store(false, Ordering::Release);
        TERMINAL_TITLE_CHANGED.store(false, Ordering::Release);
        TERMINAL_OWNED.store(false, Ordering::Release);
        EmergencyTerminalPhase::Restored.publish();
    }
}

/// Atomic-only activity seam consumed by the fatal-memory-signal module.
///
/// The signal handler must not enter the normal emergency path: that path
/// deliberately coordinates writers and generation snapshots with Rust locks.
/// A fatal signal can safely observe only this published ownership state and
/// emit the fixed, allocation-free restore bytes before restoring termios.
#[cfg(unix)]
pub(crate) fn fatal_fault_protocol_restore_required() -> bool {
    EmergencyTerminalPhase::current() == EmergencyTerminalPhase::ProtocolActive
        && TERMINAL_OWNED.load(Ordering::Acquire)
}

#[cfg(unix)]
fn fatal_process_exit(exit_code: i32) -> ! {
    // SAFETY: this is the non-returning tail after lock-bounded emergency
    // restoration. `_exit` skips destructors/stdio flushes that could block or
    // re-enter terminal ownership.
    unsafe { libc::_exit(exit_code) }
}

#[cfg(windows)]
fn fatal_process_exit(exit_code: i32) -> ! {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

    // SAFETY: GetCurrentProcess returns the current process pseudo-handle.
    // TerminateProcess is the required non-returning emergency operation.
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), exit_code as u32);
    }
    std::process::abort()
}

#[cfg(not(any(unix, windows)))]
fn fatal_process_exit(_exit_code: i32) -> ! {
    std::process::abort()
}

fn restore_active_terminal_with(inline_cursor_row: Option<u16>, drain_input: bool) {
    let _output_guard = lock_terminal_output_for_restore();
    if !TERMINAL_OWNED.load(Ordering::Acquire) {
        return;
    }
    let mode = TerminalMode::from_atomic(ACTIVE_MODE.load(Ordering::Acquire));
    restore_owned_terminal_sequences(mode, inline_cursor_row);
    if drain_input {
        drain_pending_events_with_timeout(std::time::Duration::from_millis(10));
    }
    let _disable_result = disable_raw_mode();
    #[cfg(windows)]
    windows_console::restore();
    // Publish the restored state last. The fixed signal lifecycle depends on
    // this release store to distinguish "teardown in progress" from "the
    // shell owns the terminal".
    TERMINAL_OWNED.store(false, Ordering::Release);
    EmergencyTerminalPhase::Restored.publish();
}

fn restore_owned_terminal_sequences(mode: TerminalMode, inline_cursor_row: Option<u16>) {
    if !TERMINAL_OWNED.load(Ordering::Acquire) {
        return;
    }
    emit_terminal_teardown(mode, inline_cursor_row);
}

fn emit_terminal_teardown(mode: TerminalMode, inline_cursor_row: Option<u16>) {
    let mut stdout = io::stdout();
    let terminal_last_row = crossterm::terminal::size()
        .map(|(_, rows)| rows.saturating_sub(1))
        .unwrap_or(23);
    let pop_keyboard = KEYBOARD_ENHANCEMENT_PUSHED.swap(false, Ordering::AcqRel);
    let mouse_capture = MOUSE_CAPTURE_ENABLED.swap(false, Ordering::AcqRel);
    let cursor_color = CURSOR_COLOR_CHANGED.swap(false, Ordering::AcqRel);
    let _restore_result = write_terminal_teardown(
        &mut stdout,
        mode,
        inline_cursor_row,
        terminal_last_row,
        cursor_color,
        pop_keyboard,
        mouse_capture,
    );
    clear_terminal_title();
}

fn write_terminal_setup(
    writer: &mut impl io::Write,
    mode: TerminalMode,
    mouse_capture: bool,
    mouse_reporting_leaks_as_raw_text: bool,
) -> io::Result<()> {
    if mode.uses_alternate_screen() {
        execute!(writer, EnterAlternateScreen)?;
    }
    if mouse_capture {
        execute!(writer, EnableMouseCapture)?;
    } else if mouse_reporting_leaks_as_raw_text {
        // Crossterm's Windows input implementation does not consume the VT
        // mouse stream emitted by JediTerm. Assert every mouse mode off in
        // minimal mode, including modes left behind by an earlier process.
        writer.write_all(MOUSE_TRACKING_RESET)?;
    }
    if mode.uses_alternate_screen() {
        execute!(
            writer,
            EnableFocusChange,
            EnableBracketedPaste,
            Hide,
            Clear(ClearType::All)
        )
    } else {
        execute!(writer, EnableFocusChange, EnableBracketedPaste, Hide)
    }
}

fn write_terminal_teardown(
    writer: &mut impl io::Write,
    mode: TerminalMode,
    inline_cursor_row: Option<u16>,
    terminal_last_row: u16,
    cursor_color_changed: bool,
    pop_keyboard_enhancement: bool,
    mouse_capture_enabled: bool,
) -> io::Result<()> {
    let mut outcome = Ok(());
    // A terminal write can fail after emitting only the synchronized-update
    // Begin prefix. End must therefore be the first restoration command,
    // before cursor/screen/raw-mode cleanup, and is harmless when no frame was
    // open.
    retain_first_terminal_error(&mut outcome, execute!(writer, EndSynchronizedUpdate));
    retain_first_terminal_error(&mut outcome, write_osc8_close(writer));
    retain_first_terminal_error(&mut outcome, writer.flush());
    if cursor_color_changed {
        retain_first_terminal_error(&mut outcome, writer.write_all(EMERGENCY_CURSOR_COLOR_RESET));
        retain_first_terminal_error(&mut outcome, writer.flush());
    }
    if mouse_capture_enabled {
        // Use the raw reset on every host. On Windows this repairs ANSI
        // terminals such as JediTerm; the crossterm command below additionally
        // restores the native console input mode.
        retain_first_terminal_error(&mut outcome, writer.write_all(MOUSE_PASTE_RESET));
        retain_first_terminal_error(&mut outcome, writer.flush());
        #[cfg(windows)]
        retain_first_terminal_error(&mut outcome, execute!(writer, DisableMouseCapture));
    } else {
        retain_first_terminal_error(&mut outcome, execute!(writer, DisableBracketedPaste));
    }
    retain_first_terminal_error(&mut outcome, execute!(writer, DisableFocusChange));
    if pop_keyboard_enhancement {
        retain_first_terminal_error(&mut outcome, execute!(writer, PopKeyboardEnhancementFlags));
    }
    if mode.uses_alternate_screen() {
        retain_first_terminal_error(&mut outcome, execute!(writer, Show, LeaveAlternateScreen));
    } else {
        let row = inline_cursor_row
            .unwrap_or(terminal_last_row)
            .min(terminal_last_row);
        retain_first_terminal_error(&mut outcome, execute!(writer, MoveTo(0, row), Show));
        retain_first_terminal_error(&mut outcome, writeln!(writer));
    }
    retain_first_terminal_error(&mut outcome, writer.flush());
    outcome
}

#[cfg(unix)]
struct SignalMonitor {
    receiver: std::sync::mpsc::Receiver<TerminalSignal>,
    pending_termination: std::sync::Arc<AtomicI32>,
    pending_resume: std::sync::Arc<AtomicBool>,
    internal_resume_expected: AtomicBool,
    resume_registration: signal_hook::SigId,
    notifier: std::sync::Arc<Notify>,
    handle: signal_hook::iterator::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl SignalMonitor {
    fn install() -> io::Result<Self> {
        use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP};

        ignore_background_terminal_signals()?;
        let mut signals = signal_hook::iterator::Signals::new([
            SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGCONT,
        ])?;
        let handle = signals.handle();
        let (sender, receiver) = std::sync::mpsc::sync_channel(16);
        let pending_termination = std::sync::Arc::new(AtomicI32::new(0));
        let thread_termination = std::sync::Arc::clone(&pending_termination);
        // The iterator thread provides async readiness, but it is not
        // guaranteed to run before an already-ready terminal-input branch
        // after external SIGSTOP/SIGCONT. This signal-safe flag is the
        // immediate resume fence checked by the event loop.
        let pending_resume = std::sync::Arc::new(AtomicBool::new(false));
        let resume_registration =
            signal_hook::flag::register(SIGCONT, std::sync::Arc::clone(&pending_resume))?;
        let termination_seen = std::sync::Arc::new(AtomicBool::new(false));
        let thread_termination_seen = std::sync::Arc::clone(&termination_seen);
        #[cfg(feature = "terminal-lifecycle-tests")]
        let first_termination_observed =
            std::env::var_os("CRABCODE_TUI_TEST_ONLY_FIRST_TERMINATION_FILE")
                .map(std::path::PathBuf::from);
        let notifier = std::sync::Arc::new(Notify::new());
        let thread_notifier = std::sync::Arc::clone(&notifier);
        let thread = std::thread::Builder::new()
            .name("crabcode-tui-signals".to_string())
            .spawn(move || {
                for signal in signals.forever() {
                    if signal == SIGCONT {
                        // The signal-safe flag is the resume authority. This
                        // arm only wakes the async driver; queuing a second
                        // Resume would cross one terminal generation twice.
                        thread_notifier.notify_one();
                        continue;
                    }
                    let event = match signal {
                        SIGTSTP => TerminalSignal::Suspend,
                        other => {
                            match observe_termination_signal(&thread_termination_seen) {
                                TerminationObservation::First => {
                                    // Suspend/resume transitions may
                                    // legitimately fill their bounded queue.
                                    // The first termination lives in its own
                                    // slot and requests graceful event-loop
                                    // shutdown.
                                    thread_termination.store(other, Ordering::Release);
                                    thread_notifier.notify_one();
                                    #[cfg(feature = "terminal-lifecycle-tests")]
                                    if let Some(path) = &first_termination_observed {
                                        let _ = std::fs::write(path, b"observed");
                                    }
                                }
                                TerminationObservation::Repeated => {
                                    force_exit_after_signal(other);
                                }
                            }
                            continue;
                        }
                    };
                    if sender.try_send(event).is_err() {
                        // A full queue already contains a signal for the UI
                        // loop. Never block this dedicated signal bridge.
                        continue;
                    }
                    thread_notifier.notify_one();
                }
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                signal_hook::low_level::unregister(resume_registration);
                return Err(error);
            }
        };
        Ok(Self {
            receiver,
            pending_termination,
            pending_resume,
            internal_resume_expected: AtomicBool::new(false),
            resume_registration,
            notifier,
            handle,
            thread: Some(thread),
        })
    }

    fn try_recv(&self) -> Option<TerminalSignal> {
        let signal = self.pending_termination.swap(0, Ordering::AcqRel);
        if signal != 0 {
            return Some(TerminalSignal::Terminate(signal));
        }
        if let Ok(signal) = self.receiver.try_recv() {
            return Some(signal);
        }
        if take_external_resume_observation(&self.pending_resume, &self.internal_resume_expected) {
            Some(TerminalSignal::Resume)
        } else {
            None
        }
    }

    fn expect_internal_resume(&self) {
        self.internal_resume_expected.store(true, Ordering::Release);
    }

    fn cancel_internal_resume_expectation(&self) {
        self.internal_resume_expected
            .store(false, Ordering::Release);
    }

    fn notifier(&self) -> std::sync::Arc<Notify> {
        std::sync::Arc::clone(&self.notifier)
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminationObservation {
    First,
    Repeated,
}

#[cfg(unix)]
fn take_external_resume_observation(pending: &AtomicBool, expected_internal: &AtomicBool) -> bool {
    pending.swap(false, Ordering::AcqRel) && !expected_internal.swap(false, Ordering::AcqRel)
}

#[cfg(unix)]
fn observe_termination_signal(seen: &AtomicBool) -> TerminationObservation {
    if seen.swap(true, Ordering::AcqRel) {
        TerminationObservation::Repeated
    } else {
        TerminationObservation::First
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn ignore_background_terminal_signals() -> io::Result<()> {
    for signal in [libc::SIGTTIN, libc::SIGTTOU] {
        // SAFETY: process-global disposition change performed before the
        // dedicated signal iterator thread starts. SIG_IGN requires no Rust
        // callback and is inherited by the fixed upstream lifecycle.
        if unsafe { libc::signal(signal, libc::SIG_IGN) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn force_exit_after_signal(signal: i32) -> ! {
    let writer_guard = crate::terminal_writer::emergency_stop_terminal_writer();
    let fence = writer_guard.output_fence();
    restore_active_terminal_for_emergency(fence);
    if emergency_after_restore(fence) == EmergencyAfterRestore::ExitContended {
        fatal_process_exit(128_i32.saturating_add(signal));
    }
    crate::sdk_runtime::try_force_kill_active_runtimes();
    fatal_process_exit(128_i32.saturating_add(signal))
}

#[cfg(unix)]
impl Drop for SignalMonitor {
    fn drop(&mut self) {
        signal_hook::low_level::unregister(self.resume_registration);
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _join_result = thread.join();
        }
    }
}

#[cfg(windows)]
static WINDOWS_SIGNAL_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static WINDOWS_IMMEDIATE_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static WINDOWS_CTRL_C_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(windows)]
unsafe extern "system" fn windows_console_handler(control: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    let bit = match control {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            WINDOWS_CTRL_C_COUNT.fetch_add(1, Ordering::AcqRel);
            1
        }
        CTRL_CLOSE_EVENT => {
            WINDOWS_IMMEDIATE_BITS.fetch_or(2, Ordering::Release);
            2
        }
        CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            WINDOWS_IMMEDIATE_BITS.fetch_or(4, Ordering::Release);
            4
        }
        _ => return 0,
    };
    WINDOWS_SIGNAL_BITS.fetch_or(bit, Ordering::Release);
    1
}

#[cfg(windows)]
struct WindowsSignalMonitor {
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl WindowsSignalMonitor {
    fn install() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // Clear the previous generation before publishing the handler. A
        // console event accepted after SetConsoleCtrlHandler succeeds must
        // never be erased by startup bookkeeping.
        WINDOWS_SIGNAL_BITS.store(0, Ordering::Release);
        WINDOWS_IMMEDIATE_BITS.store(0, Ordering::Release);
        WINDOWS_CTRL_C_COUNT.store(0, Ordering::Release);
        // SAFETY: installs a process-global handler that performs one atomic
        // OR and returns. No allocation, lock, terminal IO, or Rust unwinding
        // occurs in the Windows callback context.
        if unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("crabcode-tui-console-signals".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    if let Some(code) = windows_force_exit_code(
                        WINDOWS_IMMEDIATE_BITS.load(Ordering::Acquire),
                        WINDOWS_CTRL_C_COUNT.load(Ordering::Acquire),
                    ) {
                        force_exit_after_windows_signal(code);
                    }
                    std::thread::park_timeout(std::time::Duration::from_millis(10));
                }
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                // SAFETY: roll back the exact handler published above because
                // no monitor object exists to own it.
                let _ = unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 0) };
                return Err(error);
            }
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn try_recv(&self) -> Option<TerminalSignal> {
        let bits = WINDOWS_SIGNAL_BITS.swap(0, Ordering::AcqRel);
        if bits & 2 != 0 {
            Some(TerminalSignal::Terminate(1))
        } else if bits & 4 != 0 {
            Some(TerminalSignal::Terminate(0))
        } else if bits & 1 != 0 {
            Some(TerminalSignal::Terminate(130))
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn windows_force_exit_code(immediate_bits: u32, ctrl_c_count: u32) -> Option<i32> {
    if immediate_bits & 2 != 0 {
        Some(1)
    } else if immediate_bits & 4 != 0 {
        Some(0)
    } else if ctrl_c_count >= 2 {
        Some(130)
    } else {
        None
    }
}

#[cfg(windows)]
fn force_exit_after_windows_signal(exit_code: i32) -> ! {
    let writer_guard = crate::terminal_writer::emergency_stop_terminal_writer();
    let fence = writer_guard.output_fence();
    restore_active_terminal_for_emergency(fence);
    if emergency_after_restore(fence) == EmergencyAfterRestore::ExitContended {
        fatal_process_exit(exit_code);
    }
    crate::sdk_runtime::try_force_kill_active_runtimes();
    fatal_process_exit(exit_code)
}

#[cfg(windows)]
impl Drop for WindowsSignalMonitor {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
        // SAFETY: removes the exact process-global callback installed by
        // `install`; no callback-owned memory is freed.
        let _ = unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk_projection::ProjectedKind;
    use crate::terminal_writer::{InMemoryFrameReceiver, in_memory_frame_writer};
    use crossterm::QueueableCommand as _;
    use ratatui::backend::{ClearType as BackendClearType, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::Position;

    static TEST_GUARD_DROP_RESTORES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn record_test_guard_drop_restore() {
        TEST_GUARD_DROP_RESTORES.fetch_add(1, Ordering::AcqRel);
    }

    fn native_hyperlink_route() -> HyperlinkRoute {
        HyperlinkRoute {
            emit_osc8: true,
            emit_id: true,
            skip_reason: None,
        }
    }

    struct FixedFrameBackend {
        inner: CrosstermBackend<CrabCodeFrameWriter>,
        size: Size,
        cursor: Position,
        drawn: Vec<(u16, u16, Cell)>,
    }

    #[derive(Default)]
    struct FlushRecordingWriter {
        bytes: Vec<u8>,
        flush_offsets: Vec<usize>,
    }

    #[derive(Default)]
    struct FailFirstWriteThenRecord {
        failed: bool,
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl io::Write for FailFirstWriteThenRecord {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.failed {
                self.failed = true;
                return Err(io::Error::other("injected first teardown write failure"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    impl io::Write for FlushRecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_offsets.push(self.bytes.len());
            Ok(())
        }
    }

    impl SynchronizedFrameBackend for FixedFrameBackend {
        fn frame_writer_mut(&mut self) -> &mut CrabCodeFrameWriter {
            self.inner.writer_mut()
        }
    }

    impl io::Write for FixedFrameBackend {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            io::Write::write(&mut self.inner, bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.inner)
        }
    }

    impl ratatui::backend::Backend for FixedFrameBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            let content = content.collect::<Vec<_>>();
            self.drawn.extend(
                content
                    .iter()
                    .map(|(column, row, cell)| (*column, *row, (*cell).clone())),
            );
            self.inner.draw(content.into_iter())
        }

        fn append_lines(&mut self, lines: u16) -> io::Result<()> {
            self.inner.append_lines(lines)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.cursor)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            let position = position.into();
            self.inner.set_cursor_position(position)?;
            self.cursor = position;
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: BackendClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size::default(),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            ratatui::backend::Backend::flush(&mut self.inner)
        }
    }

    fn fixed_frame_terminal() -> (Terminal<FixedFrameBackend>, InMemoryFrameReceiver) {
        let (writer, receiver) = in_memory_frame_writer();
        let size = Size::new(40, 8);
        let backend = FixedFrameBackend {
            inner: CrosstermBackend::new(writer),
            size,
            cursor: Position::ORIGIN,
            drawn: Vec::new(),
        };
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, size.width, size.height)),
            },
        )
        .expect("fixed terminal");
        (terminal, receiver)
    }

    fn render_outcome(cursor: Option<(u16, u16)>) -> crate::tui_ui::RenderOutcome {
        crate::tui_ui::RenderOutcome {
            cursor,
            hyperlinks: Vec::new(),
        }
    }

    fn facts(term: Option<&str>, tmux: bool, zellij: bool, ssh: bool) -> DetectionFacts {
        let _ = ssh;
        DetectionFacts {
            term: term.map(str::to_string),
            tmux,
            zellij,
            tmux_control_mode: false,
            mouse_reporting_leaks_as_raw_text: false,
            legacy_fullscreen: None,
            user_type: Some("ant".to_string()),
            disable_mouse_tracking: false,
            disable_mouse_clicks: false,
        }
    }

    #[test]
    fn ratatui_diff_preserves_coordinates_beyond_u16_flat_index() {
        let area = Rect::new(0, 0, 420, 160);
        let previous = Buffer::empty(area);
        let mut current = previous.clone();
        current.content[65_536].set_symbol("x");

        let updates = previous.diff(&current);

        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].0, updates[0].1), (16, 156));
    }

    #[test]
    fn cli_mode_parser_accepts_explicit_forms_and_rejects_conflicts() {
        let parsed = parse_cli_mode(["crabcode-tui", "--minimal"]).expect("valid minimal mode");
        assert_eq!(parsed.preference, Some(ModePreference::Minimal));
        let parsed = parse_cli_mode(["crabcode-tui", "--no-alt-screen"]).expect("valid alias");
        assert_eq!(parsed.preference, Some(ModePreference::Inline));
        let parsed = parse_cli_mode(["crabcode-tui", "--fullscreen"]).expect("valid fullscreen");
        assert_eq!(parsed.preference, Some(ModePreference::Fullscreen));
        let parsed = parse_cli_mode(["crabcode-tui", "--minimal", "--no-alt-screen"])
            .expect("fixed upstream lets minimal win over no-alt-screen");
        assert_eq!(parsed.preference, Some(ModePreference::Minimal));
        let parsed = parse_cli_mode(["crabcode-tui", "--fullscreen", "--no-alt-screen"])
            .expect("fixed upstream lets no-alt-screen constrain the standard renderer");
        assert_eq!(parsed.preference, Some(ModePreference::Inline));
        assert!(matches!(
            parse_cli_mode(["crabcode-tui", "--minimal", "--fullscreen"]),
            Err(TerminalSetupError::ConflictingModes { .. })
        ));
        for unsupported in ["--screen-mode", "--screen-mode=minimal", "--inline"] {
            assert!(
                matches!(
                    parse_cli_mode(["crabcode-tui", unsupported]),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{unsupported} has no fixed-upstream or historical authority"
            );
        }
    }

    #[test]
    fn hidden_cron_ensure_route_is_exact_and_process_owned() {
        assert_eq!(
            resolve_pre_terminal_action(["crabcode-tui", "--ensure-cron-daemon"])
                .expect("exact lifecycle route"),
            Some(PreTerminalAction::EnsureCronDaemon)
        );
        assert_eq!(
            resolve_pre_terminal_action(["crabcode-tui", "--model", "best"])
                .expect("ordinary TUI route"),
            None
        );
        assert!(matches!(
            resolve_pre_terminal_action([
                "crabcode-tui",
                "--ensure-cron-daemon",
                "--model",
                "best"
            ]),
            Err(TerminalSetupError::InvalidCronEnsureInvocation)
        ));
    }

    #[test]
    fn bridge_and_ccr_routes_are_explicitly_excluded_from_the_pure_tui() {
        for invocation in [
            vec!["crabcode-tui", "--remote", "task"],
            vec!["crabcode-tui", "--remote=task"],
            vec!["crabcode-tui", "--remote-control"],
            vec!["crabcode-tui", "--remote-control=name"],
            vec!["crabcode-tui", "--rc"],
            vec!["crabcode-tui", "--rc=name"],
            vec!["crabcode-tui", "--teleport", "remote-session-id"],
            vec!["crabcode-tui", "--teleport=remote-session-id"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn chrome_extension_surface_is_explicitly_excluded_from_the_pure_tui() {
        for invocation in [
            vec!["crabcode-tui", "--chrome"],
            vec!["crabcode-tui", "--no-chrome"],
            vec!["crabcode-tui", "--caps", "automation,observability"],
            vec!["crabcode-tui", "--caps=automation,observability"],
            vec!["crabcode-tui", "--profile", "default"],
            vec!["crabcode-tui", "--profile=default"],
            vec!["crabcode-tui", "--session", "named-session"],
            vec!["crabcode-tui", "--session=named-session"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn historical_piped_prompt_is_appended_after_the_positional_prompt_losslessly() {
        assert_eq!(
            merge_historical_piped_prompt(Some("positional".to_string()), "piped\n"),
            Some("positional\npiped\n".to_string())
        );
        assert_eq!(
            merge_historical_piped_prompt(None, "piped"),
            Some("piped".to_string())
        );
        assert_eq!(
            merge_historical_piped_prompt(Some("positional".to_string()), ""),
            Some("positional".to_string())
        );
        assert_eq!(merge_historical_piped_prompt(Some(String::new()), ""), None);
        assert_eq!(
            merge_historical_piped_prompt(Some(" ".to_string()), " "),
            Some(" \n ".to_string()),
            "historical JavaScript Boolean filtering preserves whitespace-only strings"
        );
        assert_eq!(
            merge_historical_piped_prompt(None, "\u{fffd}"),
            Some("\u{fffd}".to_string()),
            "invalid piped UTF-8 is represented lossily like Node's UTF-8 decoder"
        );
    }

    #[test]
    fn pure_interactive_tui_rejects_print_remote_and_assistant_routes() {
        for invocation in [
            vec!["crabcode-tui", "-p"],
            vec!["crabcode-tui", "--print"],
            vec!["crabcode-tui", "--no-print"],
            vec!["crabcode-tui", "--sdk-url", "ws://127.0.0.1/session"],
            vec!["crabcode-tui", "--sdk-url=ws://127.0.0.1/session"],
            vec!["crabcode-tui", "--assistant"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn child_structured_io_contract_is_never_a_user_selectable_tui_option() {
        for invocation in [
            vec!["crabcode-tui", "--input-format", "stream-json"],
            vec!["crabcode-tui", "--input-format=text"],
            vec!["crabcode-tui", "--output-format", "stream-json"],
            vec!["crabcode-tui", "--output-format=json"],
            vec!["crabcode-tui", "--include-partial-messages"],
            vec!["crabcode-tui", "--no-include-partial-messages"],
            vec!["crabcode-tui", "--include-hook-events"],
            vec!["crabcode-tui", "--no-include-hook-events"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn headless_structured_and_exit_only_options_are_rejected_by_the_tui() {
        for invocation in [
            vec!["crabcode-tui", "--json-schema", "{}"],
            vec!["crabcode-tui", "--json-schema={}"],
            vec!["crabcode-tui", "--replay-user-messages"],
            vec!["crabcode-tui", "--permission-prompt-tool", "mcp__prompt"],
            vec!["crabcode-tui", "--max-turns", "2"],
            vec!["crabcode-tui", "--max-budget-usd=1"],
            vec!["crabcode-tui", "--task-budget", "1024"],
            vec!["crabcode-tui", "--no-session-persistence"],
            vec!["crabcode-tui", "--resume-session-at", "message-id"],
            vec!["crabcode-tui", "--rewind-files=user-message-id"],
            vec!["crabcode-tui", "--fallback-model", "default"],
            vec!["crabcode-tui", "--workload=cron"],
            vec!["crabcode-tui", "--enable-auth-status"],
            vec!["crabcode-tui", "--init-only"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn argument_separator_does_not_bypass_child_reserved_surface_checks() {
        for invocation in [
            vec!["crabcode-tui", "--", "--print"],
            vec!["crabcode-tui", "--", "--output-format=stream-json"],
            vec!["crabcode-tui", "--", "--include-hook-events"],
            vec!["crabcode-tui", "--", "--sdk-url", "ws://example.invalid"],
            vec!["crabcode-tui", "--", "--json-schema={}"],
            vec!["crabcode-tui", "--", "--verbose"],
            vec!["crabcode-tui", "--", "--no-verbose"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation.clone()),
                    Err(TerminalSetupError::ExcludedSurfaceOption { .. })
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn verbose_is_carried_only_as_an_unresolved_native_presentation_override() {
        let absent = parse_cli_mode(["crabcode-tui", "--model", "best"])
            .expect("ordinary runtime arguments");
        assert_eq!(absent.presentation_verbose_override, None);

        let explicit = parse_cli_mode(["crabcode-tui", "--verbose", "--model", "best", "prompt"])
            .expect("explicit native presentation override");
        assert_eq!(explicit.presentation_verbose_override, Some(true));
        assert_eq!(
            explicit.runtime_args,
            ["--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(explicit.initial_prompt.as_deref(), Some("prompt"));
        assert!(
            matches!(
                parse_cli_mode(["crabcode-tui", "--no-verbose"]),
                Err(TerminalSetupError::ExcludedSurfaceOption { .. })
            ),
            "the pinned CLI has no negative verbose override to emulate"
        );
    }

    #[test]
    fn disable_slash_commands_is_both_a_native_fact_and_an_unchanged_child_flag() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--disable-slash-commands",
            "--model",
            "best",
            "literal prompt",
        ])
        .expect("fixed historical boolean option");
        assert!(parsed.disable_slash_commands);
        assert_eq!(
            parsed.runtime_args,
            ["--disable-slash-commands", "--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(parsed.initial_prompt.as_deref(), Some("literal prompt"));

        let ordinary =
            parse_cli_mode(["crabcode-tui", "--model", "best"]).expect("ordinary runtime args");
        assert!(!ordinary.disable_slash_commands);
    }

    #[test]
    fn interactive_backend_options_are_not_misclassified_as_headless() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--thinking",
            "adaptive",
            "--max-thinking-tokens",
            "4096",
            "--fork-session",
        ])
        .expect("interactive backend options");
        assert_eq!(
            parsed.runtime_args,
            [
                "--thinking",
                "adaptive",
                "--max-thinking-tokens",
                "4096",
                "--fork-session",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn cli_parser_preserves_only_backend_owned_session_selection_shapes() {
        let canonical_a = "10000000-0000-4000-8000-000000000001";
        let canonical_b = "20000000-0000-4000-8000-000000000002";
        let continue_cli = parse_cli_mode(["crabcode-tui", "--continue"]).expect("continue");
        assert_eq!(
            continue_cli.initial_session,
            InitialSessionRequest::Continue
        );
        assert_eq!(
            continue_cli.runtime_args,
            vec![OsString::from("--continue")]
        );
        let separate_exact =
            parse_cli_mode(["crabcode-tui", "--resume", canonical_a]).expect("separate exact id");
        assert_eq!(
            separate_exact.initial_session,
            InitialSessionRequest::ResumeExact {
                session_id: canonical_a.to_string()
            }
        );
        assert_eq!(
            separate_exact.runtime_args,
            vec![OsString::from("--resume"), OsString::from(canonical_a)]
        );
        let equals_exact = parse_cli_mode([
            "crabcode-tui",
            "--minimal",
            &format!("--resume={canonical_b}"),
        ])
        .expect("exact id with terminal mode");
        assert_eq!(
            equals_exact.initial_session,
            InitialSessionRequest::ResumeExact {
                session_id: canonical_b.to_string()
            }
        );
        assert_eq!(
            equals_exact.runtime_args,
            vec![OsString::from(format!("--resume={canonical_b}"))]
        );
        for invocation in [
            vec!["crabcode-tui", "--continue", "--resume", canonical_a],
            vec!["crabcode-tui", "--resume", canonical_a, "--continue"],
        ] {
            let parsed = parse_cli_mode(invocation).expect("continue has legacy precedence");
            assert_eq!(parsed.initial_session, InitialSessionRequest::Continue);
            assert_eq!(parsed.runtime_args, vec![OsString::from("--continue")]);
        }
        for (invocation, expected_search, expected_runtime_args) in [
            (
                vec!["crabcode-tui", "--resume"],
                None,
                vec![OsString::from("--resume")],
            ),
            (
                vec!["crabcode-tui", "--resume="],
                None,
                vec![OsString::from("--resume=")],
            ),
            (
                vec!["crabcode-tui", "--resume", ""],
                None,
                vec![OsString::from("--resume"), OsString::from("")],
            ),
            (
                vec!["crabcode-tui", "--resume", "My Session"],
                Some("My Session"),
                vec![OsString::from("--resume"), OsString::from("My Session")],
            ),
        ] {
            let parsed = parse_cli_mode(invocation).expect("resume discovery");
            assert_eq!(
                parsed.initial_session,
                InitialSessionRequest::ResumePicker {
                    initial_search: expected_search.map(str::to_string),
                }
            );
            assert_eq!(parsed.runtime_args, expected_runtime_args);
        }
    }

    #[test]
    fn later_continue_suppresses_an_earlier_resume_picker_request() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--resume",
            "\u{feff}  My Session \u{feff}",
            "--continue",
        ])
        .expect("continue has legacy precedence");
        assert_eq!(parsed.initial_session, InitialSessionRequest::Continue);
        assert_eq!(
            parsed.runtime_args,
            vec![OsString::from("--continue")],
            "continue must remove the earlier resume option before child startup"
        );
    }

    #[test]
    fn cli_parser_forwards_runtime_arguments_and_removes_only_the_separator() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--minimal",
            "explain this",
            "--model",
            "best",
            "--mcp-config",
            "a.json",
            "--add-dir",
            "/tmp/extra",
            "--",
            "--strict-mcp-config",
            "--allowedTools",
            "Bash",
        ])
        .expect("runtime passthrough");
        assert_eq!(parsed.preference, Some(ModePreference::Minimal));
        assert_eq!(
            parsed.runtime_args,
            [
                "--model",
                "best",
                "--mcp-config",
                "a.json",
                "--add-dir",
                "/tmp/extra",
                "--strict-mcp-config",
                "--allowedTools",
                "Bash",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
        assert_eq!(parsed.initial_prompt.as_deref(), Some("explain this"));
        assert!(!parsed.runtime_args.iter().any(|argument| argument == "--"));
    }

    #[test]
    fn cli_parser_rejects_multiple_positional_prompts() {
        assert!(matches!(
            parse_cli_mode(["crabcode-tui", "first", "second"]),
            Err(TerminalSetupError::MultiplePrompts)
        ));
    }

    #[test]
    fn cli_parser_keeps_prefill_in_the_native_composer_only() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--prefill",
            "  review before sending  ",
            "--model",
            "best",
        ])
        .expect("native prefill");
        assert_eq!(
            parsed.composer_prefill.as_deref(),
            Some("review before sending")
        );
        assert_eq!(
            parsed.runtime_args,
            ["--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );

        let parsed = parse_cli_mode(["crabcode-tui", "--prefill=second"])
            .expect("equals-form native prefill");
        assert_eq!(parsed.composer_prefill.as_deref(), Some("second"));
        assert!(parsed.runtime_args.is_empty());
    }

    #[test]
    fn cli_parser_prefill_fails_closed_on_missing_or_oversized_text() {
        assert!(matches!(
            parse_cli_mode(["crabcode-tui", "--prefill"]),
            Err(TerminalSetupError::MissingPrefill)
        ));
        let oversized = "x".repeat(crate::tui_app::MAX_COMPOSER_TEXT_BYTES + 1);
        assert!(matches!(
            parse_cli_mode(vec![
                "crabcode-tui".to_string(),
                "--prefill".to_string(),
                oversized,
            ]),
            Err(TerminalSetupError::PrefillTooLarge { .. })
        ));
    }

    #[test]
    fn cli_parser_keeps_deep_link_provenance_out_of_the_backend_argv() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "--deep-link-origin",
            "--deep-link-repo",
            "acosmi/crabcode",
            "--deep-link-last-fetch",
            "1720000000000",
            "--prefill",
            "😀",
            "--model",
            "best",
        ])
        .expect("deep-link renderer options");
        assert!(parsed.launch_provenance.deep_link_origin);
        assert_eq!(
            parsed.launch_provenance.deep_link_repo.as_deref(),
            Some("acosmi/crabcode")
        );
        assert_eq!(
            parsed.launch_provenance.deep_link_last_fetch_ms,
            Some(1_720_000_000_000.0)
        );
        assert_eq!(
            parsed.launch_provenance.prefill_source_length,
            Some(2),
            "JavaScript String.length counts the emoji as two UTF-16 code units"
        );
        assert_eq!(
            parsed.runtime_args,
            ["--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn deep_link_notice_reproduces_provenance_and_staleness_lines() {
        let provenance = LaunchProvenance {
            deep_link_origin: true,
            deep_link_repo: Some("acosmi/crabcode".to_string()),
            deep_link_last_fetch_ms: Some(2.0 * 86_400_000.0),
            prefill_source_length: Some(1_001),
        };
        let notice = provenance
            .notice_at(
                std::path::Path::new("/home/test/work"),
                Some(std::path::Path::new("/home/test")),
                10.0 * 86_400_000.0,
                UiLanguage::EnUs,
            )
            .expect("deep-link notice");
        assert_eq!(
            notice,
            "This session was opened by an external deep link in ~/work\n\
             Resolved acosmi/crabcode from local clones · last fetched 1w ago — CRABCODE.md may be stale\n\
             The prompt below (1.0k chars) was supplied by the link — scroll to review the entire prompt before pressing Enter."
        );
    }

    #[test]
    fn ordinary_prefill_notice_does_not_claim_a_deep_link_origin() {
        let provenance = LaunchProvenance {
            prefill_source_length: Some(5),
            ..LaunchProvenance::default()
        };
        assert_eq!(
            provenance
                .notice_at(std::path::Path::new("/work"), None, 0.0, UiLanguage::EnUs,)
                .as_deref(),
            Some("Launched with a pre-filled prompt — review it before pressing Enter.")
        );
    }

    #[test]
    fn launch_provenance_notice_uses_selected_chinese_chrome_and_preserves_dynamic_values() {
        let provenance = LaunchProvenance {
            deep_link_origin: true,
            deep_link_repo: Some("acosmi/crabcode".to_string()),
            deep_link_last_fetch_ms: Some(2.0 * 86_400_000.0),
            prefill_source_length: Some(1_001),
        };
        let notice = provenance
            .notice_at(
                std::path::Path::new("/home/test/work"),
                Some(std::path::Path::new("/home/test")),
                10.0 * 86_400_000.0,
                UiLanguage::ZhCn,
            )
            .expect("deep-link notice");
        assert_eq!(
            notice,
            "此会话由外部深层链接在 ~/work 中打开\n\
             已从本地克隆解析 acosmi/crabcode · 上次拉取：1周前——CRABCODE.md 可能已过期\n\
             下方提示（1.0k 个字符）由链接提供——按 Enter 前请滚动并完整检查。"
        );
    }

    #[test]
    fn compact_prefill_lengths_match_the_legacy_intl_outputs() {
        assert_eq!(format_compact_number(1_001), "1.0k");
        assert_eq!(format_compact_number(1_050), "1.1k");
        assert_eq!(format_compact_number(1_150), "1.2k");
        assert_eq!(format_compact_number(9_999), "10.0k");
        assert_eq!(format_compact_number(999_999), "1.0m");
        assert_eq!(format_compact_number(1_000_000), "1.0m");
        assert_eq!(format_compact_number(8_388_608), "8.4m");
    }

    #[test]
    fn prefill_trim_and_timestamp_number_follow_ecmascript_edges() {
        assert_eq!(
            crate::text_safety::trim_ecmascript_whitespace("\u{FEFF} x \u{FEFF}"),
            "x"
        );
        assert_eq!(
            crate::text_safety::trim_ecmascript_whitespace("\u{0085}x\u{0085}"),
            "\u{0085}x\u{0085}",
            "ECMAScript trim does not remove NEXT LINE"
        );
        assert_eq!(parse_legacy_optional_milliseconds(""), Some(0.0));
        assert_eq!(parse_legacy_optional_milliseconds("  "), Some(0.0));
        assert_eq!(parse_legacy_optional_milliseconds("0x10"), Some(16.0));
        assert_eq!(parse_legacy_optional_milliseconds("0b10"), Some(2.0));
        assert_eq!(parse_legacy_optional_milliseconds("0o10"), Some(8.0));
        assert_eq!(
            parse_legacy_optional_milliseconds("0x10000000000000000").map(f64::to_bits),
            Some(0x43f0_0000_0000_0000),
            "JavaScript Number accepts finite radix integers beyond u64"
        );
        assert_eq!(
            parse_legacy_optional_milliseconds("0xffffffffffffffffffffffffffffffff")
                .map(f64::to_bits),
            Some(0x47f0_0000_0000_0000)
        );
        let exact_rounding_fixture = concat!(
            "0x54f261e321849a54b5f2b832fa94561a952c360903ab319895484964919e5e0",
            "187c6086a85c98310751d600193fb59b431d1f0f5f567a7cb2b229696e2b8118",
            "72f9bf8c8e1c160f7640642a45faba22547d882430c0ee31e24546931fceb29be",
            "5082799a7"
        );
        assert_eq!(
            parse_legacy_optional_milliseconds(exact_rounding_fixture).map(f64::to_bits),
            Some(0x7215_3c98_78c8_6127),
            "the pinned JS result requires one final ties-to-even conversion, not repeated f64 folding"
        );
        assert_eq!(
            parse_legacy_optional_milliseconds(&format!("0x{}", "f".repeat(256))),
            None,
            "a Number overflow is converted to the legacy undefined state"
        );
        assert_eq!(parse_legacy_optional_milliseconds("0x10z"), None);
        assert_eq!(parse_legacy_optional_milliseconds("+"), None);
        assert_eq!(parse_legacy_optional_milliseconds("Infinity"), None);
    }

    #[test]
    fn from_pr_fails_closed_before_direct_runtime_startup() {
        for invocation in [
            vec!["crabcode-tui", "--from-pr"],
            vec!["crabcode-tui", "--from-pr", "12suffix"],
            vec![
                "crabcode-tui",
                "--from-pr=https://github.com/acosmi/crabcode/pull/42",
            ],
            vec!["crabcode-tui", "--from-pr", "--continue"],
        ] {
            assert!(
                matches!(
                    parse_cli_mode(invocation),
                    Err(TerminalSetupError::ExcludedSurfaceOption {
                        option: "--from-pr",
                        ..
                    })
                ),
                "PR lookup has no existing direct-backend control"
            );
        }
    }

    #[test]
    fn commander_optional_values_are_not_misclassified_as_the_tui_prompt() {
        for flag in ["-d", "--debug", "-w", "--worktree", "--tasks"] {
            let parsed = parse_cli_mode(["crabcode-tui", flag, "optional-value", "real prompt"])
                .expect("optional-value runtime option");
            assert_eq!(
                parsed.runtime_args,
                [flag, "optional-value"]
                    .into_iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>(),
                "{flag}"
            );
            assert_eq!(
                parsed.initial_prompt.as_deref(),
                Some("real prompt"),
                "{flag}"
            );
        }
    }

    #[test]
    fn commander_optional_value_stops_before_the_next_option() {
        let parsed = parse_cli_mode(["crabcode-tui", "--debug", "--model", "best", "real prompt"])
            .expect("bare optional-value option");
        assert_eq!(
            parsed.runtime_args,
            ["--debug", "--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(parsed.initial_prompt.as_deref(), Some("real prompt"));
    }

    #[test]
    fn commander_variadic_values_are_forwarded_without_becoming_prompts() {
        for flag in [
            "--allowedTools",
            "--allowed-tools",
            "--tools",
            "--disallowedTools",
            "--disallowed-tools",
            "--mcp-config",
            "--betas",
            "--add-dir",
            "--file",
            "--channels",
            "--dangerously-load-development-channels",
        ] {
            let parsed = parse_cli_mode([
                "crabcode-tui",
                "real prompt",
                flag,
                "first-value",
                "second-value",
                "--model",
                "best",
            ])
            .expect("variadic runtime option");
            assert_eq!(
                parsed.runtime_args,
                [flag, "first-value", "second-value", "--model", "best"]
                    .into_iter()
                    .map(OsString::from)
                    .collect::<Vec<_>>(),
                "{flag}"
            );
            assert_eq!(
                parsed.initial_prompt.as_deref(),
                Some("real prompt"),
                "{flag}"
            );
        }
    }

    #[test]
    fn commander_variadic_value_with_equals_consumes_its_remaining_values() {
        let parsed = parse_cli_mode([
            "crabcode-tui",
            "real prompt",
            "--mcp-config=first.json",
            "second.json",
            "--model",
            "best",
        ])
        .expect("equals-form variadic runtime option");
        assert_eq!(
            parsed.runtime_args,
            ["--mcp-config=first.json", "second.json", "--model", "best"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(parsed.initial_prompt.as_deref(), Some("real prompt"));
    }

    #[test]
    fn commander_variadic_without_a_prior_prompt_keeps_all_non_options_as_values() {
        let parsed = parse_cli_mode(["crabcode-tui", "--mcp-config", "first.json", "second.json"])
            .expect("variadic runtime option without prompt");
        assert_eq!(
            parsed.runtime_args,
            ["--mcp-config", "first.json", "second.json"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert!(parsed.initial_prompt.is_none());
    }

    #[test]
    fn explicit_standard_renderer_honors_terminal_policy_and_dumb_still_fails_closed() {
        let plan = resolve_preference(
            ModePreference::Fullscreen,
            TerminalModeSource::Cli,
            "test",
            "测试",
            facts(Some("xterm-256color"), false, false, false),
        )
        .expect("explicit selection");
        assert_eq!(plan.mode, TerminalMode::Fullscreen);
        assert_eq!(plan.source, TerminalModeSource::Cli);

        let zellij = resolve_preference(
            ModePreference::Fullscreen,
            TerminalModeSource::Cli,
            "test",
            "测试",
            facts(Some("xterm-256color"), false, true, false),
        )
        .expect("explicit standard renderer under Zellij");
        assert_eq!(zellij.mode, TerminalMode::Inline);

        assert!(matches!(
            resolve_preference(
                ModePreference::Minimal,
                TerminalModeSource::Cli,
                "test",
                "测试",
                facts(Some("dumb"), false, false, false)
            ),
            Err(TerminalSetupError::DumbTerminal)
        ));
    }

    #[test]
    fn explicit_environment_wins_over_auto_and_cli_wins_over_environment() {
        let mut auto_would_inline = facts(Some("xterm-256color"), true, false, true);
        auto_would_inline.user_type = None;
        auto_would_inline.legacy_fullscreen = Some("true".to_string());
        let environment =
            resolve_mode_inputs(None, auto_would_inline.clone()).expect("environment override");
        assert_eq!(environment.mode, TerminalMode::Fullscreen);
        assert_eq!(environment.source, TerminalModeSource::Environment);

        let cli = resolve_mode_inputs(Some(ModePreference::Minimal), auto_would_inline)
            .expect("CLI override");
        assert_eq!(cli.mode, TerminalMode::Minimal);
        assert_eq!(cli.source, TerminalModeSource::Cli);
    }

    #[test]
    fn terminal_plan_status_uses_selected_language_and_preserves_protocol_tokens() {
        let plan = resolve_mode_inputs(
            Some(ModePreference::Minimal),
            facts(Some("xterm-256color"), false, false, false),
        )
        .expect("localized terminal plan");

        assert_eq!(
            plan.summary(UiLanguage::EnUs),
            "terminal minimal selected by CLI (fixed renderer CLI flag: --minimal selected scrollback-native rendering)"
        );
        assert_eq!(
            plan.summary(UiLanguage::ZhCn),
            "终端模式：CLI → 精简（固定渲染器 CLI 参数：--minimal 选择了原生终端回滚区渲染）"
        );
        assert_eq!(
            plan.effective_summary(UiLanguage::ZhCn, TerminalMode::Inline),
            "终端模式：CLI → 精简（固定渲染器 CLI 参数：--minimal 选择了原生终端回滚区渲染） · 当前回退为内联模式"
        );
        assert_eq!(
            TerminalFallback::MinimalToInline.notice(UiLanguage::ZhCn),
            "精简内联视口能力探测失败；正在使用全高度内联模式"
        );
        assert_eq!(
            TerminalFallback::MinimalToInline.notice(UiLanguage::EnUs),
            "minimal inline viewport capability probe failed; using full-height inline mode"
        );
        assert!(plan.reason.contains("--minimal"));
    }

    #[test]
    fn auto_mode_selects_minimal_for_jetbrains_windows_mouse_leak() {
        let mut leak = facts(Some("xterm-256color"), false, false, false);
        leak.mouse_reporting_leaks_as_raw_text = true;
        let plan = resolve_auto_mode(leak).expect("auto plan for the pinned leak");
        assert_eq!(plan.mode, TerminalMode::Minimal);
        assert!(plan.reason.contains("JediTerm"));
    }

    #[test]
    fn explicit_fullscreen_overrides_jetbrains_windows_auto_minimal() {
        let mut leak = facts(Some("xterm-256color"), false, false, false);
        leak.mouse_reporting_leaks_as_raw_text = true;
        let plan = resolve_preference(
            ModePreference::Fullscreen,
            TerminalModeSource::Cli,
            "explicit test",
            "显式测试",
            leak,
        )
        .expect("explicit fullscreen");
        assert_eq!(plan.mode, TerminalMode::Fullscreen);
    }

    #[test]
    fn explicit_inline_overrides_jetbrains_windows_auto_minimal() {
        let mut leak = facts(Some("xterm-256color"), false, false, false);
        leak.mouse_reporting_leaks_as_raw_text = true;
        let plan = resolve_preference(
            ModePreference::Inline,
            TerminalModeSource::Cli,
            "explicit test",
            "显式测试",
            leak,
        )
        .expect("explicit inline");
        assert_eq!(plan.mode, TerminalMode::Inline);
    }

    #[test]
    fn invalid_historical_environment_value_is_unset_not_a_new_protocol_error() {
        let mut external = facts(Some("xterm-256color"), false, false, false);
        external.user_type = None;
        external.legacy_fullscreen = Some("not-a-boolean".to_string());

        let plan = resolve_mode_inputs(None, external).expect("invalid legacy value is unset");

        assert_eq!(plan.mode, TerminalMode::Inline);
        assert_eq!(plan.source, TerminalModeSource::Auto);
    }

    #[test]
    fn historical_boolean_environment_uses_the_exact_recognized_vocabulary() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert_eq!(
                legacy_fullscreen_override(Some(value)),
                Some(true),
                "{value}"
            );
        }
        for value in ["0", "false", "FALSE", " no ", "off"] {
            assert_eq!(
                legacy_fullscreen_override(Some(value)),
                Some(false),
                "{value}"
            );
        }
        for value in ["", "2", "enabled", "disabled"] {
            assert_eq!(legacy_fullscreen_override(Some(value)), None, "{value}");
        }
        assert_eq!(legacy_fullscreen_override(None), None);
    }

    #[test]
    fn historical_iterm_tmux_control_clue_is_positive_only() {
        assert!(is_tmux_control_mode_env_heuristic(
            true,
            Some("iTerm.app"),
            Some("xterm-256color")
        ));
        for (tmux, term_program, term) in [
            (false, Some("iTerm.app"), Some("xterm-256color")),
            (true, Some("Apple_Terminal"), Some("xterm-256color")),
            (true, Some("iTerm.app"), Some("screen-256color")),
            (true, Some("iTerm.app"), Some("tmux-256color")),
        ] {
            assert!(!is_tmux_control_mode_env_heuristic(
                tmux,
                term_program,
                term
            ));
        }
    }

    #[test]
    fn alt_screen_policy_matrix() {
        let plain = facts(Some("xterm-256color"), false, false, false);
        let tmux = facts(Some("screen-256color"), true, false, false);
        let screen = facts(Some("screen-256color"), false, false, false);
        let zellij = facts(Some("xterm-256color"), false, true, false);
        let ssh = facts(Some("xterm-256color"), false, false, true);
        let unknown = facts(None, false, false, false);

        for facts in [&plain, &tmux, &screen, &ssh, &unknown] {
            assert!(determine_alt_screen_policy(facts, false));
        }
        assert!(!determine_alt_screen_policy(&zellij, false));
        assert!(!determine_alt_screen_policy(&tmux, true));

        for facts in [plain, tmux, screen, ssh, unknown] {
            assert_eq!(
                resolve_auto_mode(facts).expect("auto plan").mode,
                TerminalMode::Fullscreen
            );
        }
        assert_eq!(
            resolve_auto_mode(zellij).expect("auto zellij").mode,
            TerminalMode::Inline
        );
        let mut tmux_control = facts(Some("screen-256color"), true, false, false);
        tmux_control.tmux_control_mode = true;
        assert_eq!(
            resolve_auto_mode(tmux_control)
                .expect("auto tmux control mode")
                .mode,
            TerminalMode::Inline
        );
    }

    #[test]
    fn historical_mouse_flags_keep_their_original_fullscreen_scope() {
        assert!(!mouse_capture_for_mode(TerminalMode::Minimal, false));
        assert!(mouse_capture_for_mode(TerminalMode::Inline, true));
        assert!(mouse_capture_for_mode(TerminalMode::Fullscreen, false));
        assert!(!mouse_capture_for_mode(TerminalMode::Fullscreen, true));

        let mut fullscreen = facts(Some("xterm-256color"), false, false, false);
        fullscreen.disable_mouse_tracking = true;
        fullscreen.disable_mouse_clicks = true;
        let fullscreen = resolve_auto_mode(fullscreen).expect("fullscreen plan");
        assert!(fullscreen.disable_fullscreen_mouse_tracking);
        assert!(fullscreen.mouse_clicks_disabled);

        let mut inline = facts(Some("xterm-256color"), false, false, false);
        inline.user_type = None;
        inline.disable_mouse_clicks = true;
        let inline = resolve_auto_mode(inline).expect("inline plan");
        assert_eq!(inline.mode, TerminalMode::Inline);
        assert!(!inline.mouse_clicks_disabled);
    }

    #[test]
    fn terminal_title_strips_controls_caps_length_and_preserves_product_name() {
        assert_eq!(
            terminal_title_string("\x1b]0;Injected\x07CrabCode"),
            "]0;InjectedCrabCode - CrabCode"
        );
        assert_eq!(terminal_title_string("\n\r\t"), "CrabCode");
        let capped = terminal_title_string(&"a".repeat(81));
        assert_eq!(capped.chars().count(), 80);
        assert!(capped.ends_with(" - CrabCode"));
    }

    #[test]
    fn cursor_color_follows_crabcode_theme_and_skips_terminal_native_minimal() {
        assert_eq!(
            cursor_color_sequence(TerminalMode::Fullscreen, false, CrabCodeTheme::dark())
                .as_deref(),
            Some("\x1b]12;rgb:c8/c8/c8\x07")
        );
        assert_eq!(
            cursor_color_sequence(TerminalMode::Fullscreen, false, CrabCodeTheme::light())
                .as_deref(),
            Some("\x1b]12;rgb:44/44/44\x07")
        );
        assert_eq!(
            cursor_color_sequence(TerminalMode::Minimal, false, CrabCodeTheme::dark()),
            None
        );
        assert_eq!(
            cursor_color_sequence(TerminalMode::Fullscreen, true, CrabCodeTheme::dark()),
            None
        );
        assert!(cursor_color_requires_late_apply(
            TerminalMode::Minimal,
            TerminalMode::Inline
        ));
        assert!(!cursor_color_requires_late_apply(
            TerminalMode::Minimal,
            TerminalMode::Minimal
        ));
        assert!(!cursor_color_requires_late_apply(
            TerminalMode::Inline,
            TerminalMode::Inline
        ));
    }

    #[test]
    fn minimal_welcome_and_print_once_history_consume_the_concrete_palette() {
        let card = minimal_welcome_card(80, crate::tui_app::UiLanguage::ZhCn);
        assert!(card.wordmark);
        assert_eq!(card.height, 11);
        let area = Rect::new(0, 0, 80, card.height);
        let mut dark_buffer = Buffer::empty(area);
        let mut light_buffer = Buffer::empty(area);
        render_minimal_welcome_card(
            &mut dark_buffer,
            card,
            CrabCodeTheme::dark(),
            CrabCodeThemeKind::Dark,
        );
        render_minimal_welcome_card(
            &mut light_buffer,
            card,
            CrabCodeTheme::light(),
            CrabCodeThemeKind::Light,
        );
        assert_eq!(dark_buffer[(2, 6)].fg, CrabCodeTheme::dark().text_primary);
        assert_eq!(light_buffer[(2, 6)].fg, CrabCodeTheme::light().text_primary);
        assert_ne!(dark_buffer[(2, 6)].fg, light_buffer[(2, 6)].fg);
        assert_eq!(dark_buffer[(1, 1)].fg, Color::Rgb(255, 80, 80));
        assert_eq!(light_buffer[(1, 1)].fg, Color::Rgb(220, 38, 38));
        assert_eq!(
            minimal_welcome_card(49, crate::tui_app::UiLanguage::ZhCn).height,
            7
        );

        let item = projected(
            "theme-history",
            "```rust\nfn main() { let value = 1; }\n```",
            false,
        );
        let dark = crate::tui_ui::terminal_lines_for_projected_item_with_theme(
            &item,
            80,
            CrabCodeTheme::dark(),
            CrabCodeThemeKind::Dark,
            false,
        );
        let light = crate::tui_ui::terminal_lines_for_projected_item_with_theme(
            &item,
            80,
            CrabCodeTheme::light(),
            CrabCodeThemeKind::Light,
            false,
        );
        let colors = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
                .collect::<Vec<_>>()
        };
        assert_ne!(colors(&dark), colors(&light));
    }

    #[test]
    fn windows_console_mode_transforms_preserve_unrelated_bits() {
        const WINDOW_INPUT: u32 = 0x0008;
        const MOUSE_INPUT: u32 = 0x0010;
        const QUICK_EDIT: u32 = 0x0040;
        const EXTENDED_FLAGS: u32 = 0x0080;
        const PROCESSED_OUTPUT: u32 = 0x0001;
        const VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        let original_input = u32::MAX;
        let native = windows_console::native_selection_mode(original_input);
        assert_eq!(native & MOUSE_INPUT, 0);
        assert_ne!(native & WINDOW_INPUT, 0);
        assert_ne!(native & QUICK_EDIT, 0);
        assert_ne!(native & EXTENDED_FLAGS, 0);
        assert_eq!(
            native & !(MOUSE_INPUT | WINDOW_INPUT | QUICK_EDIT | EXTENDED_FLAGS),
            original_input & !(MOUSE_INPUT | WINDOW_INPUT | QUICK_EDIT | EXTENDED_FLAGS)
        );

        assert_eq!(
            windows_console::virtual_terminal_output_mode(0),
            PROCESSED_OUTPUT | VIRTUAL_TERMINAL_PROCESSING
        );
    }

    #[test]
    fn windows_double_fatal_restores_complete_snapshot_before_any_exit() {
        use std::sync::{Arc, Barrier, Mutex, RwLock, mpsc};

        let baseline = windows_console::ConsoleBaseline {
            stdin_mode: Some(1),
            stdout_mode: Some(2),
            output_code_page: Some(65_001),
        };
        let published = Arc::new(RwLock::new(Some(baseline)));
        let first_reader = published.read().expect("first fatal snapshot");
        let second_reader = published.read().expect("second fatal snapshot");

        let (replacement_attempted_tx, replacement_attempted_rx) = mpsc::channel();
        let (replacement_complete_tx, replacement_complete_rx) = mpsc::channel();
        let replacement_store = Arc::clone(&published);
        let replacement = std::thread::spawn(move || {
            replacement_attempted_tx
                .send(())
                .expect("announce replacement");
            *replacement_store.write().expect("replace console snapshot") = None;
            replacement_complete_tx
                .send(())
                .expect("announce replacement completion");
        });
        replacement_attempted_rx
            .recv()
            .expect("replacement writer reached lock");
        assert!(
            replacement_complete_rx
                .recv_timeout(std::time::Duration::from_millis(10))
                .is_err(),
            "a new generation cannot reclaim a snapshot held by two fatal readers"
        );
        drop(first_reader);
        assert!(
            replacement_complete_rx
                .recv_timeout(std::time::Duration::from_millis(10))
                .is_err(),
            "reclamation must wait for the final fatal reader"
        );

        let console = Arc::new(Mutex::new([11_u32, 22_u32, 437_u32]));
        let first_stdin_restored = Arc::new(Barrier::new(2));
        let second_exit_recorded = Arc::new(Barrier::new(2));
        let exit_observations = Arc::new(Mutex::new(Vec::new()));

        let first_console_stdin = Arc::clone(&console);
        let first_console_stdout = Arc::clone(&console);
        let first_console_code_page = Arc::clone(&console);
        let first_barrier = Arc::clone(&first_stdin_restored);
        let first_exit_barrier = Arc::clone(&second_exit_recorded);
        let first_snapshot = *second_reader;
        let first = std::thread::spawn(move || {
            windows_console::restore_console_baseline_with(
                first_snapshot.expect("published snapshot"),
                |mode| {
                    first_console_stdin.lock().expect("stdin state")[0] = mode;
                    first_barrier.wait();
                    first_exit_barrier.wait();
                },
                |mode| first_console_stdout.lock().expect("stdout state")[1] = mode,
                |code_page| {
                    first_console_code_page.lock().expect("code-page state")[2] = code_page;
                },
            );
        });

        let second_console_stdin = Arc::clone(&console);
        let second_console_stdout = Arc::clone(&console);
        let second_console_code_page = Arc::clone(&console);
        let second_console_exit = Arc::clone(&console);
        let second_barrier = Arc::clone(&first_stdin_restored);
        let second_exit_barrier = Arc::clone(&second_exit_recorded);
        let second_observations = Arc::clone(&exit_observations);
        let second_snapshot = *second_reader;
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            windows_console::restore_console_baseline_with(
                second_snapshot.expect("published snapshot"),
                |mode| second_console_stdin.lock().expect("stdin state")[0] = mode,
                |mode| second_console_stdout.lock().expect("stdout state")[1] = mode,
                |code_page| {
                    second_console_code_page.lock().expect("code-page state")[2] = code_page;
                },
            );
            second_observations
                .lock()
                .expect("exit observations")
                .push(*second_console_exit.lock().expect("console state"));
            second_exit_barrier.wait();
        });

        first.join().expect("first fatal restorer");
        second.join().expect("second fatal restorer");
        assert_eq!(
            *exit_observations.lock().expect("exit observations"),
            [[1, 2, 65_001]],
            "every fatal restorer owns the complete immutable snapshot before its exit"
        );

        drop(second_reader);
        replacement_complete_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replacement completes after both readers");
        replacement.join().expect("replacement writer");
    }

    #[test]
    fn windows_fatal_snapshot_try_read_never_waits_for_a_publisher() {
        use std::sync::{RwLock, TryLockError};

        let published = RwLock::new(Some(windows_console::ConsoleBaseline {
            stdin_mode: Some(1),
            stdout_mode: Some(2),
            output_code_page: Some(65_001),
        }));
        let publisher = published.write().expect("hold snapshot publisher");
        assert!(matches!(
            published.try_read(),
            Err(TryLockError::WouldBlock)
        ));
        drop(publisher);

        let source = include_str!("terminal.rs");
        let restore_start = source
            .find("        pub(super) fn restore() {")
            .expect("Windows console restore");
        let restore_end = source[restore_start..]
            .find("\n        }\n    }\n\n    #[cfg(windows)]")
            .map(|offset| restore_start + offset)
            .expect("Windows console restore boundary");
        let restore = &source[restore_start..restore_end];
        assert!(
            restore.contains("CONSOLE_BASELINE.try_read()")
                && restore.contains("Err(TryLockError::WouldBlock) => return"),
            "the production fatal path must skip a publishing snapshot instead of waiting"
        );
    }

    #[test]
    fn seq_cst_crossed_publications_cannot_both_observe_false() {
        #[derive(Clone, Copy)]
        enum Operation {
            PublishEmergency,
            ObserveKernel,
            PublishKernel,
            ObserveEmergency,
        }

        let operations = [
            Operation::PublishEmergency,
            Operation::ObserveKernel,
            Operation::PublishKernel,
            Operation::ObserveEmergency,
        ];
        let mut legal_interleavings = 0;
        for first in 0..4 {
            for second in 0..4 {
                for third in 0..4 {
                    for fourth in 0..4 {
                        let order = [first, second, third, fourth];
                        let has_duplicate = {
                            let mut seen = [false; 4];
                            order.iter().any(|index| {
                                let duplicate = seen[*index];
                                seen[*index] = true;
                                duplicate
                            })
                        };
                        if has_duplicate {
                            continue;
                        }
                        let position = |needle| {
                            order
                                .iter()
                                .position(|value| *value == needle)
                                .expect("permutation member")
                        };
                        // Preserve both threads' program order:
                        // emergency store -> Kernel load, and
                        // Kernel store -> emergency load.
                        if position(0) > position(1) || position(2) > position(3) {
                            continue;
                        }
                        legal_interleavings += 1;
                        let emergency = AtomicBool::new(false);
                        let kernel = AtomicBool::new(false);
                        let mut fatal_observed_kernel = None;
                        let mut owner_observed_emergency = None;
                        for index in order {
                            match operations[index] {
                                Operation::PublishEmergency => {
                                    emergency.store(true, Ordering::SeqCst);
                                }
                                Operation::ObserveKernel => {
                                    fatal_observed_kernel = Some(kernel.load(Ordering::SeqCst));
                                }
                                Operation::PublishKernel => {
                                    kernel.store(true, Ordering::SeqCst);
                                }
                                Operation::ObserveEmergency => {
                                    owner_observed_emergency =
                                        Some(emergency.load(Ordering::SeqCst));
                                }
                            }
                        }
                        assert!(
                            fatal_observed_kernel.expect("fatal observation")
                                || owner_observed_emergency.expect("owner observation"),
                            "a SeqCst total order cannot let both crossed loads miss their peer store"
                        );
                    }
                }
            }
        }
        assert_eq!(legal_interleavings, 6);

        let terminal_source = include_str!("terminal.rs");
        let writer_source = include_str!("terminal_writer.rs");
        assert!(
            writer_source.contains("TERMINAL_WRITER_EMERGENCY_STOP.store(true, Ordering::SeqCst)"),
            "production emergency publication must participate in the SeqCst total order"
        );
        assert!(
            terminal_source.contains(
                "TerminalAcquisitionMutation::Idle as u8,\n                kind as u8,\n                Ordering::SeqCst,\n                Ordering::SeqCst,"
            ) && terminal_source.contains(
                "while state.load(Ordering::SeqCst) == TerminalAcquisitionMutation::Kernel as u8"
            ),
            "production acquisition publication and fatal observation must be SeqCst"
        );
    }

    #[test]
    fn contended_fatal_cannot_be_followed_by_raw_mutation() {
        use std::sync::{Arc, Barrier, Mutex};

        // First close the original check-then-CAS hole: fatal publication
        // between the output-gate check and Kernel publication rejects the
        // owner before any mutation.
        let state = Arc::new(AtomicU8::new(TerminalAcquisitionMutation::Idle as u8));
        let emergency = Arc::new(AtomicBool::new(false));
        let checked = Arc::new(Barrier::new(2));
        let restored = Arc::new(Barrier::new(2));
        let trace = Arc::new(Mutex::new(Vec::new()));

        let owner_state = Arc::clone(&state);
        let owner_emergency = Arc::clone(&emergency);
        let owner_checked = Arc::clone(&checked);
        let owner_restored = Arc::clone(&restored);
        let owner_trace = Arc::clone(&trace);
        let owner = std::thread::spawn(move || {
            assert!(!owner_emergency.load(Ordering::SeqCst));
            owner_checked.wait();
            owner_restored.wait();
            let result = TerminalAcquisitionMutationGuard::begin_with(
                &owner_state,
                TerminalAcquisitionMutation::Kernel,
                || owner_emergency.load(Ordering::SeqCst),
            );
            assert!(result.is_err());
            owner_trace.lock().expect("owner trace").push("rejected");
        });

        let fatal_emergency = Arc::clone(&emergency);
        let fatal_checked = Arc::clone(&checked);
        let fatal_restored = Arc::clone(&restored);
        let fatal_trace = Arc::clone(&trace);
        let fatal = std::thread::spawn(move || {
            fatal_checked.wait();
            fatal_emergency.store(true, Ordering::SeqCst);
            fatal_trace.lock().expect("fatal trace").push("restore");
            fatal_restored.wait();
        });
        owner.join().expect("late owner");
        fatal.join().expect("early fatal");
        assert_eq!(
            *trace.lock().expect("trace"),
            ["restore", "rejected"],
            "restoration observed before Kernel publication must forbid raw/setup"
        );

        // Then cover a fatal that lands inside the already-published,
        // output-free Kernel phase. It waits for the owner to roll back raw
        // state; restoration/exit cannot overtake that rollback, and setup is
        // never authorized.
        let state = Arc::new(AtomicU8::new(TerminalAcquisitionMutation::Idle as u8));
        let emergency = Arc::new(AtomicBool::new(false));
        let kernel_published = Arc::new(Barrier::new(2));
        let emergency_published = Arc::new(Barrier::new(2));
        let trace = Arc::new(Mutex::new(Vec::new()));

        let owner_state = Arc::clone(&state);
        let owner_emergency = Arc::clone(&emergency);
        let owner_kernel = Arc::clone(&kernel_published);
        let owner_release = Arc::clone(&emergency_published);
        let owner_trace = Arc::clone(&trace);
        let owner = std::thread::spawn(move || {
            let guard = TerminalAcquisitionMutationGuard::begin_with(
                &owner_state,
                TerminalAcquisitionMutation::Kernel,
                || owner_emergency.load(Ordering::SeqCst),
            )
            .expect("publish Kernel mutation");
            owner_trace
                .lock()
                .expect("owner trace")
                .push("kernel-authorized");
            owner_kernel.wait();
            owner_release.wait();
            owner_trace.lock().expect("owner trace").push("enable-raw");
            let result = guard.finish_after_kernel_mutation(
                || owner_emergency.load(Ordering::SeqCst),
                || owner_trace.lock().expect("owner trace").push("rollback"),
            );
            assert!(result.is_err());
        });

        let fatal_state = Arc::clone(&state);
        let fatal_emergency = Arc::clone(&emergency);
        let fatal_kernel = Arc::clone(&kernel_published);
        let fatal_release = Arc::clone(&emergency_published);
        let fatal_trace = Arc::clone(&trace);
        let fatal = std::thread::spawn(move || {
            fatal_kernel.wait();
            fatal_emergency.store(true, Ordering::SeqCst);
            fatal_trace
                .lock()
                .expect("fatal trace")
                .push("emergency-latched");
            fatal_release.wait();
            wait_for_kernel_acquisition_rollback_with(
                &fatal_state,
                EmergencyOutputFence::Contended,
            );
            let mut trace = fatal_trace.lock().expect("fatal trace");
            trace.push("restore");
            trace.push("exit");
        });

        owner.join().expect("kernel owner");
        fatal.join().expect("contended fatal");
        let trace = trace.lock().expect("trace");
        let position = |event| trace.iter().position(|value| *value == event).expect(event);
        assert!(
            position("kernel-authorized") < position("emergency-latched")
                && position("emergency-latched") < position("enable-raw")
                && position("enable-raw") < position("rollback")
                && position("rollback") < position("restore")
                && position("restore") < position("exit"),
            "a contended fatal must observe raw rollback before restoration/exit: {trace:?}"
        );
        assert!(!trace.contains(&"setup"));
    }

    #[test]
    fn emergency_during_child_reacquire_returns_to_child_handoff() {
        let state = AtomicU8::new(TerminalAcquisitionMutation::Idle as u8);
        let emergency = AtomicBool::new(false);
        let owned = AtomicBool::new(false);
        let phase = AtomicU8::new(EmergencyTerminalPhase::ChildHandoff as u8);
        let raw_calls = std::cell::Cell::new(0_u8);
        let setup_calls = std::cell::Cell::new(0_u8);

        let guard = TerminalAcquisitionMutationGuard::begin_with(
            &state,
            TerminalAcquisitionMutation::Kernel,
            || emergency.load(Ordering::SeqCst),
        )
        .expect("publish child Kernel transition");
        owned.store(true, Ordering::Release);
        phase.store(
            EmergencyTerminalPhase::ProtocolActive as u8,
            Ordering::Release,
        );
        raw_calls.set(raw_calls.get() + 1);
        emergency.store(true, Ordering::SeqCst);
        let result = guard.finish_after_kernel_mutation(
            || emergency.load(Ordering::SeqCst),
            || {
                owned.store(false, Ordering::Release);
                phase.store(
                    EmergencyTerminalPhase::ChildHandoff as u8,
                    Ordering::Release,
                );
            },
        );
        if result.is_ok() {
            setup_calls.set(setup_calls.get() + 1);
        }
        assert!(result.is_err());
        assert_eq!(raw_calls.get(), 1);
        assert_eq!(setup_calls.get(), 0);
        assert!(!owned.load(Ordering::Acquire));
        assert_eq!(
            phase.load(Ordering::Acquire),
            EmergencyTerminalPhase::ChildHandoff as u8
        );

        let source = include_str!("terminal.rs");
        let child_start = source
            .find("fn reacquire_tty_after_child(mode: TerminalMode)")
            .expect("child reacquisition");
        let child_end = source[child_start..]
            .find("\nfn restore_active_terminal(")
            .map(|offset| child_start + offset)
            .expect("child reacquisition boundary");
        let child = &source[child_start..child_end];
        assert!(
            child.contains(
                "kernel_mutation.finish_after_kernel_mutation(\n        terminal_writer_emergency_stopped,\n        restore_aborted_child_kernel_acquisition,"
            ),
            "the production child path must roll an interrupted Kernel transition back to ChildHandoff"
        );
    }

    #[test]
    fn emergency_during_external_resume_heal_never_starts_setup() {
        let state = AtomicU8::new(TerminalAcquisitionMutation::Idle as u8);
        let emergency = AtomicBool::new(false);
        let reassert_calls = std::cell::Cell::new(0_u8);
        let setup_calls = std::cell::Cell::new(0_u8);
        let rollback_calls = std::cell::Cell::new(0_u8);

        let guard = TerminalAcquisitionMutationGuard::begin_with(
            &state,
            TerminalAcquisitionMutation::Kernel,
            || emergency.load(Ordering::SeqCst),
        )
        .expect("publish resume-heal Kernel transition");
        reassert_calls.set(reassert_calls.get() + 1);
        emergency.store(true, Ordering::SeqCst);
        let result = guard.finish_after_kernel_mutation(
            || emergency.load(Ordering::SeqCst),
            || rollback_calls.set(rollback_calls.get() + 1),
        );
        if result.is_ok()
            && TerminalAcquisitionMutationGuard::begin_with(
                &state,
                TerminalAcquisitionMutation::Setup,
                || emergency.load(Ordering::SeqCst),
            )
            .is_ok()
        {
            setup_calls.set(setup_calls.get() + 1);
        }
        assert!(result.is_err());
        assert_eq!(reassert_calls.get(), 1);
        assert_eq!(rollback_calls.get(), 1);
        assert_eq!(setup_calls.get(), 0);

        let source = include_str!("terminal.rs");
        let heal_start = source
            .find("    pub(crate) fn heal_after_external_resume(")
            .expect("external resume heal");
        let heal_end = source[heal_start..]
            .find("\n    /// Re-acquire terminal ownership")
            .map(|offset| heal_start + offset)
            .expect("external resume heal boundary");
        let heal = &source[heal_start..heal_end];
        let kernel = heal
            .find("TerminalAcquisitionMutation::Kernel")
            .expect("heal Kernel transition");
        let reassert = heal
            .find("crossterm::terminal::reassert_raw_mode()")
            .expect("heal raw reassert");
        let finish = heal
            .find("kernel_mutation.finish_after_kernel_mutation(")
            .expect("heal rollback fence");
        let setup = heal
            .find("TerminalAcquisitionMutation::Setup")
            .expect("heal Setup transition");
        assert!(
            kernel < reassert && reassert < finish && finish < setup,
            "external resume healing must finish its rollback-fenced Kernel transition before setup"
        );
    }

    #[test]
    fn panic_on_kernel_transition_owner_does_not_wait_on_itself() {
        let state = AtomicU8::new(TerminalAcquisitionMutation::Idle as u8);
        let result = std::panic::catch_unwind(|| {
            let _guard = TerminalAcquisitionMutationGuard::begin_with(
                &state,
                TerminalAcquisitionMutation::Kernel,
                || false,
            )
            .expect("publish same-thread Kernel transition");
            wait_for_kernel_acquisition_rollback_with(&state, EmergencyOutputFence::Quiesced);
            panic!("injected Kernel transition panic");
        });
        assert!(result.is_err());
        assert_eq!(
            state.load(Ordering::SeqCst),
            TerminalAcquisitionMutation::Idle as u8,
            "same-thread panic cleanup must neither self-wait nor strand Kernel publication"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn bounded_writer_drain_is_exposed_on_the_terminal_session() {
        let wait: fn(&TerminalSession, std::time::Duration) -> io::Result<WriterDrain> =
            TerminalSession::wait_writer_drained;
        let release: fn(&mut TerminalSession) -> io::Result<()> =
            TerminalSession::release_tty_for_child;
        let reacquire: fn(&mut TerminalSession) -> io::Result<()> =
            TerminalSession::reacquire_tty_after_child;
        let before: fn(&TerminalSession) -> io::Result<Option<(u16, u16)>> =
            TerminalSession::probe_cursor_before_child;
        let after: fn(&TerminalSession, Option<(u16, u16)>) -> io::Result<Option<(u16, u16)>> =
            TerminalSession::probe_moved_cursor_after_child;
        let restore: fn(&mut TerminalSession, Option<(u16, u16)>) -> io::Result<()> =
            TerminalSession::restore_after_child;
        let _ = (wait, release, reacquire, before, after, restore);
    }

    #[test]
    fn minimal_child_reanchor_matches_the_fixed_cursor_geometry() {
        let screen = Rect::new(0, 0, 80, 24);
        let current = Rect::new(0, 15, 80, 8);
        assert_eq!(
            reanchored_minimal_viewport(screen, current, 20),
            Rect::new(0, 16, 80, 8)
        );
        assert_eq!(
            reanchored_minimal_viewport(screen, current, 23),
            Rect::new(0, 16, 80, 8)
        );
        assert_eq!(
            reanchored_minimal_viewport(screen, current, 2),
            Rect::new(0, 2, 80, 8)
        );
    }

    #[test]
    fn restore_runs_teardown_even_when_writer_failed() {
        let (writer, writer_thread) =
            spawn_terminal_writer(WriterSync::new()).expect("single test writer");
        let backend = CrosstermBackend::new(writer);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("test terminal");
        let teardown_called = std::cell::Cell::new(false);

        let result = restore_terminal_with(
            terminal,
            writer_thread,
            TerminalMode::Inline,
            |terminal, writer_thread| {
                drop(terminal);
                drop(writer_thread);
                Err(io::Error::other("injected drain failure"))
            },
            |_, _| teardown_called.set(true),
        );

        assert!(result.is_err());
        assert!(teardown_called.get());
    }

    #[test]
    fn ownership_guard_rolls_back_each_partial_stage_and_drop_is_idempotent() {
        use std::cell::Cell;

        TEST_GUARD_DROP_RESTORES.store(0, Ordering::Release);

        // A pre-protocol guard owns no terminal mutation, so an early signal
        // installation failure or ordinary Drop has nothing to restore.
        drop(TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Acquiring,
            TerminalAcquisitionStage::Unarmed,
            record_test_guard_drop_restore,
        ));
        assert_eq!(TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire), 0);

        let mut guard = TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Acquiring,
            TerminalAcquisitionStage::Unarmed,
            record_test_guard_drop_restore,
        );
        let signal_called = Cell::new(false);
        let signal_error = guard
            .install_signal_owner(|| {
                signal_called.set(true);
                Err::<(), _>(io::Error::other("injected early signal install failure"))
            })
            .expect_err("injected early signal install must fail");
        assert_eq!(signal_error.kind(), io::ErrorKind::Other);
        assert!(signal_called.get());
        assert_eq!(guard.acquisition_stage, TerminalAcquisitionStage::Unarmed);
        drop(guard);
        assert_eq!(TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire), 0);

        // Once protocol acquisition begins, signal installation is too late
        // and the installer must not run. Dropping the active protocol stage
        // performs exactly one rollback.
        let mut guard = TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Active,
            TerminalAcquisitionStage::Protocol,
            record_test_guard_drop_restore,
        );
        let late_signal_called = Cell::new(false);
        assert!(
            guard
                .install_signal_owner(|| {
                    late_signal_called.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(!late_signal_called.get());
        guard
            .publish_writer_owner()
            .expect("writer follows pre-armed protocol acquisition");
        assert_eq!(
            guard.acquisition_stage,
            TerminalAcquisitionStage::WriterOwner
        );
        drop(guard);
        assert_eq!(TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire), 1);

        // A raw/protocol generation with no writer still rolls back once.
        drop(TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Active,
            TerminalAcquisitionStage::Protocol,
            record_test_guard_drop_restore,
        ));
        assert_eq!(TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire), 2);

        // Once final restoration has run, even a writer-drain error is
        // reported without a second teardown from Drop.
        let mut guard = TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Active,
            TerminalAcquisitionStage::WriterOwner,
            record_test_guard_drop_restore,
        );
        assert!(
            guard
                .finish_restore(Err(io::Error::other("injected drain failure")))
                .is_err()
        );
        drop(guard);
        assert_eq!(
            TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire),
            2,
            "restored guard Drop must be idempotent after a reported drain failure"
        );

        // Successful acquisition is strictly Signal -> Protocol -> Writer.
        let mut guard = TerminalOwnershipGuard::for_test(
            TerminalOwnershipPhase::Acquiring,
            TerminalAcquisitionStage::Unarmed,
            record_test_guard_drop_restore,
        );
        assert_eq!(
            guard
                .install_signal_owner(|| Ok(7_u8))
                .expect("publish signal"),
            7
        );
        assert_eq!(
            guard.acquisition_stage,
            TerminalAcquisitionStage::SignalOwner
        );
        guard.phase = TerminalOwnershipPhase::Active;
        guard.acquisition_stage = TerminalAcquisitionStage::Protocol;
        guard.publish_writer_owner().expect("publish writer");
        assert_eq!(
            guard.acquisition_stage,
            TerminalAcquisitionStage::WriterOwner
        );
        guard.finish_restore(Ok(())).expect("finish restoration");
        drop(guard);
        assert_eq!(TEST_GUARD_DROP_RESTORES.load(Ordering::Acquire), 2);
    }

    #[test]
    fn production_acquisition_publishes_signal_owner_before_post_acquisition_io() {
        let source = include_str!("terminal.rs");
        let acquire_signature = [
            "    fn acquire(plan: &TerminalPlan)",
            " -> io::Result<Self> {",
        ]
        .concat();
        let acquire_start = source
            .find(&acquire_signature)
            .expect("TerminalOwnershipGuard::acquire");
        let acquire_end = source[acquire_start..]
            .find("\n    const fn mode(")
            .map(|offset| acquire_start + offset)
            .expect("TerminalOwnershipGuard::acquire boundary");
        let acquire = &source[acquire_start..acquire_end];
        let emergency_fence = acquire
            .find("if terminal_writer_emergency_stopped()")
            .expect("process-lifetime emergency fence");
        let signal = acquire
            .find("ownership.install_signal_monitor()?")
            .expect("pre-acquisition signal owner");
        let protocol = acquire
            .find("enter_terminal_mode(")
            .expect("protocol acquisition");
        let barrier = acquire
            .find("wait_test_only_terminal_acquisition_barrier()")
            .expect("deterministic acquisition barrier");
        assert!(
            emergency_fence < signal && signal < protocol && protocol < barrier,
            "emergency fencing and signal ownership must precede every raw/protocol mutation"
        );

        let setup_start = source
            .find("fn enter_terminal_mode(")
            .expect("terminal protocol acquisition");
        let setup_end = source[setup_start..]
            .find("\n/// Drop the terminal")
            .map(|offset| setup_start + offset)
            .expect("terminal protocol acquisition boundary");
        let setup = &source[setup_start..setup_end];
        let output_gate = setup
            .find("let _output_guard = lock_terminal_output_for_active_write()?")
            .expect("active protocol output gate");
        let kernel_phase = setup
            .find("TerminalAcquisitionMutation::Kernel")
            .expect("kernel acquisition phase");
        let ownership = setup
            .find("TERMINAL_OWNED")
            .expect("terminal ownership publication");
        let raw = setup
            .find("enable_raw_mode()")
            .expect("raw-mode acquisition");
        let kernel_finish = setup
            .find("kernel_mutation.finish_after_kernel_mutation(")
            .expect("kernel mutation rollback fence");
        let setup_phase = setup
            .find("TerminalAcquisitionMutation::Setup")
            .expect("setup acquisition phase");
        let setup_fence = setup
            .find("stop_terminal_setup_after_emergency()?")
            .expect("setup emergency fence");
        let protocol_output = setup
            .find("write_terminal_setup(")
            .expect("terminal protocol output");
        assert!(
            output_gate < kernel_phase
                && kernel_phase < ownership
                && ownership < raw
                && raw < kernel_finish
                && kernel_finish < setup_phase
                && setup_phase < setup_fence
                && setup_fence < protocol_output,
            "raw mode must finish its rollback-fenced Kernel phase before emergency-fenced setup output"
        );

        let enter_signature = [
            "    pub fn enter(plan: &TerminalPlan)",
            " -> io::Result<Self> {",
        ]
        .concat();
        let start = source
            .find(&enter_signature)
            .expect("TerminalSession::enter");
        let end = source[start..]
            .find("\n    pub(crate) const fn mode(")
            .map(|offset| start + offset)
            .expect("TerminalSession::enter boundary");
        let enter = &source[start..end];
        let protocol = enter
            .find("TerminalOwnershipGuard::acquire(plan)?")
            .expect("signal and protocol acquisition");
        let writer = enter.find("create_terminal(").expect("writer owner");
        let publish_writer = enter
            .find("session.ownership.publish_writer_owner()")
            .expect("writer publication");
        let clear = enter
            .find("session.terminal_mut().clear()")
            .expect("initial terminal clear");
        let xtversion = enter
            .find("xtversion::probe_at_startup()")
            .expect("XTVERSION probe");
        let control = enter
            .find("session.install_terminal_control_lease()")
            .expect("standalone-control owner");
        let cursor = enter
            .find("apply_cursor_color(&mut io::stdout(), effective_mode)")
            .expect("late cursor-color output");
        let cursor_gate = enter[..cursor]
            .rfind("lock_terminal_output_for_active_write()")
            .expect("late cursor-color active-write gate");
        let xtversion_gate = enter[..xtversion]
            .rfind("lock_terminal_output_for_active_write()")
            .expect("XTVERSION active-write gate");
        assert!(
            protocol < writer
                && writer < publish_writer
                && publish_writer < clear
                && clear < xtversion
                && xtversion < control,
            "production ownership order must remain signal -> protocol -> writer -> post-acquisition I/O"
        );
        assert!(
            publish_writer < cursor_gate
                && cursor_gate < cursor
                && clear < xtversion_gate
                && xtversion_gate < xtversion,
            "every direct post-protocol setup write must pass the fail-closed active-write gate"
        );
        assert!(
            !enter.contains("acquisition_output_guard"),
            "TerminalSession::enter must not hold the output gate across writer drop/join error paths"
        );

        let resume_start = source.find("    pub fn resume(").expect("resume");
        let resume_end = source[resume_start..]
            .find("\n    pub fn resized(")
            .map(|offset| resume_start + offset)
            .expect("resume boundary");
        let resume = &source[resume_start..resume_end];
        let protocol = resume
            .find("self.ownership.reacquire_protocol()?")
            .expect("resume protocol acquisition");
        let writer = resume
            .find("create_terminal(")
            .expect("resume writer owner");
        let publish_writer = resume
            .find("self.ownership.publish_writer_owner()")
            .expect("resume writer publication");
        let clear = resume
            .find(".autoresize()")
            .expect("resume autoresize and clear");
        let resize_gate = resume[..clear]
            .rfind("lock_terminal_output_for_active_write()")
            .expect("resume cursor-query active-write gate");
        let control = resume
            .find("self.install_terminal_control_lease()")
            .expect("resume standalone-control owner");
        assert!(
            protocol < writer
                && writer < publish_writer
                && publish_writer < clear
                && clear < control,
            "resume must publish its writer/signal generation before post-acquisition terminal I/O"
        );
        assert!(
            publish_writer < resize_gate && resize_gate < clear,
            "resume autoresize must serialize any direct cursor query with restoration"
        );

        for (function, end_marker) in [
            (
                "    pub(crate) fn probe_cursor_before_child(",
                "\n    /// Return the post-child cursor",
            ),
            (
                "    pub(crate) fn probe_moved_cursor_after_child(",
                "\n    /// Temporarily return raw/alternate-screen ownership",
            ),
        ] {
            let start = source.find(function).expect("child cursor probe");
            let end = source[start..]
                .find(end_marker)
                .map(|offset| start + offset)
                .expect("child cursor probe boundary");
            let probe = &source[start..end];
            let gate = probe
                .find("lock_terminal_output_for_active_write()")
                .expect("child cursor-query active-write gate");
            let cursor = probe
                .find("crossterm::cursor::position()")
                .expect("child cursor query");
            assert!(
                gate < cursor,
                "child cursor queries must serialize with terminal restoration"
            );
        }

        let frame_source = include_str!("frame_transaction.rs");
        let resize = frame_source
            .find("terminal.resize(Rect::new(")
            .expect("frame resize");
        let resize_gate = frame_source[..resize]
            .rfind("lock_terminal_output_for_active_write()")
            .expect("frame-resize cursor-query active-write gate");
        assert!(
            resize_gate < resize,
            "frame resize must serialize any inline cursor query with restoration"
        );
    }

    #[test]
    fn terminal_ownership_is_published_restored_only_after_tty_recovery() {
        let source = include_str!("terminal.rs");
        for (function, end_marker) in [
            ("fn restore_terminal_with(", "\nfn restore_terminal("),
            (
                "fn restore_active_terminal_with(",
                "\nfn restore_owned_terminal_sequences(",
            ),
        ] {
            let start = source.find(function).expect("restoration function exists");
            let end = source[start..]
                .find(end_marker)
                .map(|offset| start + offset)
                .expect("restoration function boundary exists");
            let body = &source[start..end];
            let teardown = body
                .find("restore_owned_terminal_sequences")
                .or_else(|| body.find("teardown(mode, inline_cursor_row)"))
                .expect("teardown occurs in restoration transaction");
            let raw = body
                .find("disable_raw_mode()")
                .expect("raw mode restoration occurs in transaction");
            let mark = body
                .find("TERMINAL_OWNED.store(false, Ordering::Release)")
                .expect("restored ownership publication exists");
            assert!(
                teardown < raw && raw < mark,
                "{function} must emit teardown, restore termios, then publish unowned"
            );
            assert!(
                body.find("lock_terminal_output_for_restore()")
                    .is_some_and(|lock| lock < teardown),
                "{function} must serialize before emitting teardown"
            );
        }
    }

    #[test]
    fn child_handoff_drop_never_clears_the_cooked_main_screen() {
        assert!(!should_clear_terminal_before_restore(
            TerminalMode::Fullscreen,
            false,
            false,
        ));
        assert!(should_clear_terminal_before_restore(
            TerminalMode::Fullscreen,
            false,
            true,
        ));
        assert!(!should_clear_terminal_before_restore(
            TerminalMode::Fullscreen,
            true,
            true,
        ));
        assert!(!should_clear_terminal_before_restore(
            TerminalMode::Minimal,
            false,
            true,
        ));
    }

    #[test]
    fn resume_failure_matrix_preserves_primary_kind_and_reports_restore_failure() {
        for (step, primary_kind) in [
            (
                "terminal restoration after writer-owner publication failure",
                io::ErrorKind::PermissionDenied,
            ),
            (
                "terminal restoration after autoresize/clear failure",
                io::ErrorKind::InvalidData,
            ),
            (
                "terminal restoration after control-writer install failure",
                io::ErrorKind::AlreadyExists,
            ),
        ] {
            let primary_only = restore_after_failed_resume_step(
                io::Error::new(primary_kind, "primary resume failure"),
                Ok(()),
                step,
            )
            .expect_err("a failed resume step remains an error");
            assert_eq!(primary_only.kind(), primary_kind);
            assert_eq!(primary_only.to_string(), "primary resume failure");

            let combined = restore_after_failed_resume_step(
                io::Error::new(primary_kind, "primary resume failure"),
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected restoration failure",
                )),
                step,
            )
            .expect_err("resume and restoration failures must both be observable");
            assert_eq!(
                combined.kind(),
                primary_kind,
                "restoration cleanup must not replace the resume-step error kind"
            );
            assert!(combined.to_string().contains("primary resume failure"));
            assert!(combined.to_string().contains(step));
            assert!(
                combined
                    .to_string()
                    .contains("injected restoration failure")
            );
        }

        let source = include_str!("terminal.rs");
        let resume_start = source.find("    pub fn resume(").expect("resume");
        let resume_end = source[resume_start..]
            .find("\n    pub fn resized(")
            .map(|offset| resume_start + offset)
            .expect("resume boundary");
        let resume = &source[resume_start..resume_end];
        for context in [
            "terminal restoration after writer-owner publication failure",
            "terminal restoration after autoresize/clear failure",
            "terminal restoration after control-writer install failure",
        ] {
            assert!(
                resume.contains(context),
                "every fallible post-acquisition resume step must use the executable restoration seam"
            );
        }
    }

    #[test]
    fn terminal_setup_attempts_follow_the_fixed_capability_fallback_order() {
        let size = Size::new(100, 30);
        assert_eq!(
            terminal_setup_attempts(TerminalMode::Minimal, size, true),
            vec![
                TerminalSetupAttempt {
                    viewport: TerminalViewportAttempt::Inline(MIN_TERMINAL_ROWS),
                    effective_mode: TerminalMode::Minimal,
                    fallback: None,
                },
                TerminalSetupAttempt {
                    viewport: TerminalViewportAttempt::Inline(30),
                    effective_mode: TerminalMode::Inline,
                    fallback: Some(TerminalFallback::MinimalToInline),
                },
                TerminalSetupAttempt {
                    viewport: TerminalViewportAttempt::Fixed(Rect::new(0, 0, 100, 30)),
                    effective_mode: TerminalMode::Inline,
                    fallback: Some(TerminalFallback::MinimalToFixed),
                },
            ]
        );
        assert_eq!(
            terminal_setup_attempts(TerminalMode::Inline, size, true),
            vec![
                TerminalSetupAttempt {
                    viewport: TerminalViewportAttempt::Inline(30),
                    effective_mode: TerminalMode::Inline,
                    fallback: None,
                },
                TerminalSetupAttempt {
                    viewport: TerminalViewportAttempt::Fixed(Rect::new(0, 0, 100, 30)),
                    effective_mode: TerminalMode::Inline,
                    fallback: Some(TerminalFallback::InlineToFixed),
                },
            ]
        );
        assert_eq!(
            terminal_setup_attempts(TerminalMode::Minimal, size, false),
            vec![TerminalSetupAttempt {
                viewport: TerminalViewportAttempt::Inline(MIN_TERMINAL_ROWS),
                effective_mode: TerminalMode::Minimal,
                fallback: None,
            }],
            "resume must fail closed instead of silently changing the application's interaction mode"
        );
    }

    #[test]
    fn repaints_pane_out_of_band_at_most_once_per_focus_incident() {
        let mut state = FocusHealState::default();
        assert!(state.observe(true, true));
        assert!(!state.observe(true, true));
        assert!(!state.observe(false, true));
        assert!(state.observe(true, true));
        assert!(!state.observe(true, false));

        let mut disabled = FocusHealState::default();
        assert!(!disabled.observe(true, false));
        assert!(!disabled.observe(false, false));
        assert!(!disabled.observe(true, false));
    }

    #[test]
    fn minimal_welcome_success_prevents_duplicate_insert() {
        let mut app = TuiApp::new(&serde_json::json!({}), InitialSessionRequest::New, None);
        app.set_minimal_mode(true);
        let mut inserts = 0_usize;
        assert!(
            !commit_minimal_welcome_with(&mut app, 80, |_| {
                panic!("setup must not commit the main-screen welcome")
            })
            .expect("setup welcome deferral")
        );
        app.release_startup_barrier_for_test();
        assert!(
            commit_minimal_welcome_with(&mut app, 80, |_| {
                inserts += 1;
                Ok(())
            })
            .expect("first welcome insert")
        );
        assert!(
            !commit_minimal_welcome_with(&mut app, 80, |_| {
                panic!("a committed welcome must never be inserted twice")
            })
            .expect("deduplicated welcome")
        );
        assert_eq!(inserts, 1);
    }

    #[test]
    fn minimal_welcome_narrow_and_failed_inserts_remain_retryable() {
        let mut app = TuiApp::new(&serde_json::json!({}), InitialSessionRequest::New, None);
        app.set_minimal_mode(true);
        app.release_startup_barrier_for_test();

        assert!(
            !commit_minimal_welcome_with(&mut app, MINIMAL_WELCOME_MIN_WIDTH - 1, |_| panic!(
                "narrow terminals must defer without emitting"
            ),)
            .expect("narrow welcome deferral")
        );
        assert!(app.minimal_welcome_pending());

        let failed = commit_minimal_welcome_with(&mut app, 80, |_| {
            Err(io::Error::other("injected terminal insert failure"))
        });
        assert!(failed.is_err());
        assert!(app.minimal_welcome_pending());

        assert!(
            commit_minimal_welcome_with(&mut app, 80, |_| Ok(()))
                .expect("retry welcome after failed insert")
        );
        assert!(!app.minimal_welcome_pending());
    }

    #[test]
    fn persistent_app_view_is_statically_bound_before_any_interactive_frame() {
        let source = include_str!("terminal.rs");
        let start = source
            .find("pub fn present(&mut self, app: &mut TuiApp)")
            .expect("production terminal presenter");
        let end = source[start..]
            .find("pub(crate) fn presentation_progress")
            .map(|offset| start + offset)
            .expect("end of production presenter");
        let presenter = &source[start..end];
        assert!(
            presenter.contains("self.app_view.prepare(app)")
                && presenter.contains("app_view.draw_prepared(&mut frame, app)"),
            "every interactive frame must dispatch through the persistent AppView"
        );
        assert_eq!(
            presenter
                .matches("app_view.draw_prepared(&mut frame, app)")
                .count(),
            1,
            "the production presenter must have one root-view draw dispatch"
        );
    }

    #[test]
    fn renderer_animation_clock_delegates_only_to_persistent_app_view() {
        let source = include_str!("terminal.rs");
        let deadline = source
            .find("pub(crate) fn renderer_animation_deadline(")
            .expect("terminal renderer deadline");
        let tick = source[deadline..]
            .find("pub(crate) fn tick_renderer_animation(")
            .map(|offset| deadline + offset)
            .expect("terminal renderer tick");
        let retire = source[tick..]
            .find("pub(crate) fn retire_input_side_channels_for_terminal_generation")
            .map(|offset| tick + offset)
            .expect("end of terminal renderer clock methods");
        let deadline_method = &source[deadline..tick];
        let tick_method = &source[tick..retire];
        assert_eq!(
            deadline_method
                .matches("self.app_view.renderer_animation_deadline(now)")
                .count(),
            1,
            "the deadline must delegate exactly once to the persistent AppView"
        );
        assert_eq!(
            tick_method
                .matches("self.app_view.tick_renderer_animation(now)")
                .count(),
            1,
            "the tick must delegate exactly once to the persistent AppView"
        );
        for method in [deadline_method, tick_method] {
            assert!(
                !method.contains("TuiApp"),
                "the link modifier clock must not return to the legacy product transcript owner"
            );
        }
    }

    #[test]
    fn minimal_welcome_commits_before_first_history_item() {
        let source = include_str!("terminal.rs");
        let start = source
            .find("pub(crate) fn present_with_repaint(")
            .expect("production terminal presenter");
        let end = source[start..]
            .find("pub(crate) fn presentation_progress")
            .map(|offset| start + offset)
            .expect("end of production presenter");
        let presenter = &source[start..end];
        let welcome = presenter
            .find("commit_minimal_welcome(terminal, app)")
            .expect("native welcome commit");
        let history = presenter
            .find("app_view.commit_minimal(terminal, app, hold_native_commits)")
            .expect("native history commit");
        assert!(
            welcome < history,
            "fresh-session welcome must enter native scrollback before the first finalized history item"
        );
    }

    #[test]
    fn writer_frame_idle_second_render_emits_zero_bytes() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);
        let render = |frame: &mut Frame<'_>| {
            frame.render_widget(Paragraph::new("hello world"), frame.area());
            None
        };

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                Ok((render_outcome(render(&mut frame)), false))
            },
        )
        .expect("first frame");
        let first = receiver.recv().expect("first frame payload");
        assert!(!first.is_empty());

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                Ok((render_outcome(render(&mut frame)), false))
            },
        )
        .expect("unchanged frame");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "an unchanged frame must not enqueue synchronized markers or cursor commands"
        );
    }

    #[test]
    fn same_size_resize_event_invalidates_the_frame_inside_one_transaction() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);
        let render = |terminal: &mut Terminal<FixedFrameBackend>| {
            let mut frame = terminal.get_frame();
            frame.render_widget(Paragraph::new("same-size resize"), frame.area());
            Ok((render_outcome(None), false))
        };

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            render,
        )
        .expect("initial frame");
        let _initial = receiver.recv().expect("initial frame payload");

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            true,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            render,
        )
        .expect("same-size resize repaint");
        let repaint = receiver.recv().expect("resize repaint payload");
        assert!(
            !repaint.is_empty(),
            "an explicit resize event must invalidate even when dimensions return to the prior size"
        );
    }

    #[test]
    fn forced_repaint_invalidates_identical_frame_inside_one_transaction() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);
        let render = |frame: &mut Frame<'_>| {
            frame.render_widget(Paragraph::new("forced repaint"), frame.area());
        };

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                render(&mut frame);
                Ok((render_outcome(None), false))
            },
        )
        .expect("initial frame");
        let _initial = receiver.recv().expect("initial frame payload");

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                terminal.clear()?;
                let mut frame = terminal.get_frame();
                render(&mut frame);
                Ok((render_outcome(None), true))
            },
        )
        .expect("forced frame");
        let forced = receiver.recv().expect("forced repaint payload");
        let forced_text = String::from_utf8_lossy(&forced);
        assert!(
            forced_text.contains("forced") && forced_text.contains("repaint"),
            "a forced repaint must redraw identical cells after the synchronized clear: \
             {forced_text:?}",
        );
        assert!(
            forced.starts_with(b"\x1b[?2026h") && forced.ends_with(b"\x1b[?2026l"),
            "clear and full repaint must remain one synchronized presentation"
        );
    }

    #[test]
    fn writer_cursor_visible_same_position_no_changes_preserves_blink() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);
        let render = |frame: &mut Frame<'_>| {
            frame.render_widget(Paragraph::new("composer"), frame.area());
            Some((5, 1))
        };

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                Ok((render_outcome(render(&mut frame)), false))
            },
        )
        .expect("first visible-cursor frame");
        let _first = receiver.recv().expect("first frame payload");
        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                Ok((render_outcome(render(&mut frame)), false))
            },
        )
        .expect("unchanged visible-cursor frame");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "redundant Show/MoveTo would reset the terminal cursor blink timer"
        );
    }

    #[test]
    fn product_frame_emits_and_clears_osc8_through_the_present_transaction() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);
        let linked = LinkSpan {
            row: 0,
            col_start: 0,
            col_end: 2,
            url: std::sync::Arc::from("https://example.test"),
            id: None,
        };

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("AB"), frame.area());
                Ok((
                    crate::tui_ui::RenderOutcome {
                        cursor: None,
                        hyperlinks: vec![linked.clone()],
                    },
                    false,
                ))
            },
        )
        .expect("linked frame");
        let linked_payload = receiver.recv().expect("linked frame payload");
        let linked_payload = String::from_utf8_lossy(&linked_payload);
        assert!(
            linked_payload.contains("\x1b]8;;https://example.test\x07"),
            "production frame omitted OSC 8 open: {linked_payload:?}"
        );
        assert!(
            linked_payload.contains("\x1b]8;;\x07"),
            "production frame omitted OSC 8 close: {linked_payload:?}"
        );

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("AB"), frame.area());
                Ok((render_outcome(None), false))
            },
        )
        .expect("link removal frame");
        let removal_payload = receiver.recv().expect("link removal frame payload");
        let removal_payload = String::from_utf8_lossy(&removal_payload);
        assert!(
            removal_payload.contains("AB"),
            "link removal must rewrite identical cells: {removal_payload:?}"
        );
        assert!(
            !removal_payload.contains("\x1b]8;"),
            "link removal must not repaint the old OSC 8 target: {removal_payload:?}"
        );

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("AB"), frame.area());
                Ok((render_outcome(None), false))
            },
        )
        .expect("unchanged unlinked frame");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "an unchanged link layer and frame must emit zero bytes"
        );
    }

    #[test]
    fn product_frame_suppresses_osc8_on_a_fail_closed_terminal_route() {
        let (mut terminal, receiver) = fixed_frame_terminal();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            HyperlinkRoute {
                emit_osc8: false,
                emit_id: false,
                skip_reason: Some("unknown_terminal"),
            },
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("AB"), frame.area());
                Ok((
                    crate::tui_ui::RenderOutcome {
                        cursor: None,
                        hyperlinks: vec![LinkSpan {
                            row: 0,
                            col_start: 0,
                            col_end: 2,
                            url: std::sync::Arc::from("https://example.test"),
                            id: Some(7),
                        }],
                    },
                    false,
                ))
            },
        )
        .expect("frame on fail-closed hyperlink route");
        let payload = receiver.recv().expect("frame payload");
        assert!(
            !payload.windows(4).any(|window| window == b"\x1b]8;"),
            "a fail-closed terminal route must not receive OSC 8"
        );
        assert_eq!(last_hyperlinks, Some(Vec::new()));
    }

    #[test]
    fn failed_frame_enqueue_resets_native_diff_state_for_retry() {
        let (mut terminal, disconnected_receiver) = fixed_frame_terminal();
        drop(disconnected_receiver);
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = Size::new(40, 8);

        let error = present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("retry"), frame.area());
                Ok((render_outcome(None), false))
            },
        )
        .expect_err("disconnected frame queue must reject the frame");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(last_frame.is_none());
        assert!(last_hyperlinks.is_none());

        let (replacement_writer, receiver) = in_memory_frame_writer();
        terminal.backend_mut().inner = CrosstermBackend::new(replacement_writer);
        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Fullscreen,
            native_hyperlink_route(),
            |terminal| {
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("retry"), frame.area());
                Ok((render_outcome(None), false))
            },
        )
        .expect("retry after replacing the failed writer");
        let retry_payload = receiver.recv().expect("retried frame payload");
        assert!(
            String::from_utf8_lossy(&retry_payload).contains("retry"),
            "the retry must redraw cells discarded with the failed transaction"
        );
    }

    fn product_inline_height_sequence(sizes: &[u16]) -> Vec<u16> {
        assert!(!sizes.is_empty(), "height fixture needs an initial size");
        let (writer, receiver) = in_memory_frame_writer();
        let initial_size = Size::new(40, sizes[0]);
        let backend = FixedFrameBackend {
            inner: CrosstermBackend::new(writer),
            size: initial_size,
            cursor: Position::ORIGIN,
            drawn: Vec::new(),
        };
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(initial_size.height),
            },
        )
        .expect("inline terminal");
        let _setup_payloads = receiver.try_iter().collect::<Vec<_>>();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = initial_size;
        let mut observed = Vec::new();
        for height in sizes.iter().copied().skip(1) {
            terminal.backend_mut().size = Size::new(40, height);
            present_terminal_frame(
                &mut terminal,
                &mut cursor,
                &mut last_frame,
                &mut last_hyperlinks,
                &mut backend_size,
                false,
                TerminalMode::Inline,
                native_hyperlink_route(),
                |terminal| {
                    let mut frame = terminal.get_frame();
                    frame.render_widget(Paragraph::new(format!("height-{height}")), frame.area());
                    Ok((render_outcome(None), false))
                },
            )
            .expect("resize inline terminal");
            observed.push(terminal.get_frame().area().height);
            let _payload = receiver.recv().expect("resized frame payload");
        }
        observed
    }

    #[test]
    fn inline_full_height_grows_with_the_terminal() {
        assert_eq!(product_inline_height_sequence(&[8, 24]), vec![24]);
    }

    #[test]
    fn inline_viewport_shrinks_with_the_terminal() {
        assert_eq!(product_inline_height_sequence(&[24, 10]), vec![10]);
    }

    #[test]
    fn inline_viewport_smart_expands_after_a_shrink() {
        assert_eq!(product_inline_height_sequence(&[24, 10, 30]), vec![10, 30]);
    }

    #[test]
    fn inline_resize_rerenders_wrapped_history_in_the_new_geometry() {
        let (writer, receiver) = in_memory_frame_writer();
        let initial_size = Size::new(10, 6);
        let backend = FixedFrameBackend {
            inner: CrosstermBackend::new(writer),
            size: initial_size,
            cursor: Position::ORIGIN,
            drawn: Vec::new(),
        };
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(initial_size.height),
            },
        )
        .expect("inline terminal");
        let _setup_payloads = receiver.try_iter().collect::<Vec<_>>();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = initial_size;

        let present_history = |terminal: &mut Terminal<FixedFrameBackend>,
                               cursor: &mut CursorState,
                               last_frame: &mut Option<Buffer>,
                               last_hyperlinks: &mut Option<Vec<LinkSpan>>,
                               backend_size: &mut Size| {
            present_terminal_frame(
                terminal,
                cursor,
                last_frame,
                last_hyperlinks,
                backend_size,
                false,
                TerminalMode::Inline,
                native_hyperlink_route(),
                |terminal| {
                    let mut frame = terminal.get_frame();
                    frame.render_widget(
                        Paragraph::new("abcdefghijklmno").wrap(Wrap { trim: false }),
                        frame.area(),
                    );
                    Ok((render_outcome(None), false))
                },
            )
        };
        present_history(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
        )
        .expect("initial wrapped history frame");
        let _initial_payload = receiver.recv().expect("initial frame payload");

        terminal.backend_mut().size = Size::new(5, 6);
        present_history(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
        )
        .expect("resized wrapped history frame");
        let _resized_payload = receiver.recv().expect("resized frame payload");

        let resized = last_frame.as_ref().expect("resized frame snapshot");
        assert_eq!(resized.area.width, 5);
        let rows = (0..3)
            .map(|y| (0..5).map(|x| resized[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(rows, ["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn inline_scrollback_commit_is_one_synchronized_output_transaction() {
        let (writer, receiver) = in_memory_frame_writer();
        let size = Size::new(40, 8);
        let backend = FixedFrameBackend {
            inner: CrosstermBackend::new(writer),
            size,
            cursor: Position::ORIGIN,
            drawn: Vec::new(),
        };
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(3),
            },
        )
        .expect("minimal inline terminal");
        let _setup_payloads = receiver.try_iter().collect::<Vec<_>>();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = size;

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Minimal,
            native_hyperlink_route(),
            |terminal| {
                terminal.insert_before(1, |buffer| {
                    Paragraph::new("甲").render(buffer.area, buffer);
                })?;
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("乙"), frame.area());
                Ok((render_outcome(None), true))
            },
        )
        .expect("commit history and draw the live viewport");

        let payload = receiver.recv().expect("one complete presentation");
        let begin = b"\x1b[?2026h";
        let end = b"\x1b[?2026l";
        assert_eq!(
            payload
                .windows(begin.len())
                .filter(|bytes| *bytes == begin)
                .count(),
            1,
            "the presentation must contain exactly one synchronized begin marker"
        );
        assert_eq!(
            payload
                .windows(end.len())
                .filter(|bytes| *bytes == end)
                .count(),
            1,
            "the presentation must contain exactly one synchronized end marker"
        );
        assert!(payload.starts_with(begin));
        assert!(payload.ends_with(end));
        let history = payload
            .windows("甲".len())
            .position(|bytes| bytes == "甲".as_bytes())
            .expect("committed history cell");
        let live = payload
            .windows("乙".len())
            .position(|bytes| bytes == "乙".as_bytes())
            .expect("live viewport cell");
        assert!(
            history < live,
            "native history insertion must precede live viewport paint inside the same transaction"
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn product_minimal_present_resizes_the_live_viewport_on_every_frame() {
        let (writer, receiver) = in_memory_frame_writer();
        let initial_size = Size::new(40, 8);
        let backend = FixedFrameBackend {
            inner: CrosstermBackend::new(writer),
            size: initial_size,
            cursor: Position::ORIGIN,
            drawn: Vec::new(),
        };
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(MIN_TERMINAL_ROWS),
            },
        )
        .expect("minimal terminal");
        let _setup_payloads = receiver.try_iter().collect::<Vec<_>>();
        let mut cursor = CursorState::default();
        let mut last_frame = None;
        let mut last_hyperlinks = None;
        let mut backend_size = initial_size;

        terminal.backend_mut().size = Size::new(40, 24);
        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Minimal,
            native_hyperlink_route(),
            |terminal| {
                let changed = sync_minimal_viewport(terminal, 13, false)?;
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("minimal"), frame.area());
                Ok((render_outcome(None), changed))
            },
        )
        .expect("resize minimal terminal");
        assert_eq!(
            terminal.get_frame().area().height,
            13,
            "live content height must replace the initial setup floor"
        );

        present_terminal_frame(
            &mut terminal,
            &mut cursor,
            &mut last_frame,
            &mut last_hyperlinks,
            &mut backend_size,
            false,
            TerminalMode::Minimal,
            native_hyperlink_route(),
            |terminal| {
                let changed = sync_minimal_viewport(terminal, 9, false)?;
                let mut frame = terminal.get_frame();
                frame.render_widget(Paragraph::new("minimal"), frame.area());
                Ok((render_outcome(None), changed))
            },
        )
        .expect("shrink minimal terminal");
        assert_eq!(terminal.get_frame().area().height, 9);
    }

    #[test]
    fn minimal_never_uses_ris_rerender_or_emit_to_scrollback() {
        let source = include_str!("frame_transaction.rs");
        let start = source
            .find("fn draw_frame<")
            .expect("production frame presenter");
        let end = source[start..]
            .find("\nfn apply_cursor_action")
            .map(|offset| start + offset)
            .expect("end of production frame presenter");
        let production_resize_path = &source[start..end];
        for forbidden in [
            "resize_purge_rerender",
            "emit_to_scrollback",
            "resize_viewport_height",
        ] {
            assert!(
                !production_resize_path.contains(forbidden),
                "minimal frame resize path references history re-emission helper {forbidden}"
            );
        }
        assert!(
            production_resize_path.contains("set_viewport_height"),
            "minimal resize must adjust only the live viewport"
        );
    }

    #[test]
    fn cursor_state_matches_fixed_upstream_transition_matrix() {
        let hidden = CursorState::default();
        assert_eq!(hidden.action(None, false), CursorAction::None);
        assert_eq!(
            hidden.action(Some((5, 10)), false),
            CursorAction::Show(5, 10)
        );

        let visible = CursorState {
            last_position: Some((5, 10)),
            disturbed_outside_frame: false,
        };
        assert_eq!(visible.action(Some((5, 10)), false), CursorAction::None);
        assert_eq!(
            visible.action(Some((5, 10)), true),
            CursorAction::Reposition(5, 10)
        );
        assert_eq!(
            visible.action(Some((6, 10)), false),
            CursorAction::Reposition(6, 10)
        );
        assert_eq!(visible.action(None, false), CursorAction::Hide);
    }

    #[test]
    fn terminal_restore_ends_a_partial_synchronized_frame_first() {
        let mut expected_prefix = Vec::new();
        expected_prefix
            .queue(EndSynchronizedUpdate)
            .expect("encode synchronized end");
        let mut expected_close = Vec::new();
        write_osc8_close(&mut expected_close).expect("encode OSC 8 close");
        for mode in [TerminalMode::Fullscreen, TerminalMode::Inline] {
            let mut output = FlushRecordingWriter::default();
            write_terminal_teardown(&mut output, mode, Some(3), 7, false, false, false)
                .expect("encode terminal teardown");
            assert!(
                output.bytes.starts_with(&expected_prefix),
                "teardown for {mode:?} must repair synchronized output before every other command"
            );
            assert_eq!(
                output.flush_offsets.first().copied(),
                Some(expected_prefix.len()),
                "the synchronized End must reach the tty in its own flush before later teardown encoding can fail"
            );
            assert_eq!(
                output
                    .bytes
                    .get(expected_prefix.len()..expected_prefix.len() + expected_close.len()),
                Some(expected_close.as_slice()),
                "teardown must close a partially emitted OSC 8 hyperlink immediately after synchronized output"
            );
            assert_eq!(
                output.flush_offsets.get(1).copied(),
                Some(expected_prefix.len() + expected_close.len()),
                "the OSC 8 repair must be flushed before later teardown encoding can fail"
            );
        }
    }

    #[test]
    fn teardown_write_failure_does_not_skip_later_recovery_steps() {
        use crossterm::QueueableCommand as _;

        let mut output = FailFirstWriteThenRecord::default();
        let error = write_terminal_teardown(
            &mut output,
            TerminalMode::Fullscreen,
            None,
            7,
            true,
            true,
            true,
        )
        .expect_err("the first write failure remains observable");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        let mut expected = Vec::new();
        write_osc8_close(&mut expected).expect("encode hyperlink close");
        expected.extend_from_slice(b"\x1b]112\x07");
        expected.extend_from_slice(MOUSE_PASTE_RESET);
        expected
            .queue(DisableFocusChange)
            .expect("encode focus reset");
        expected
            .queue(PopKeyboardEnhancementFlags)
            .expect("encode keyboard pop");
        expected.queue(Show).expect("encode cursor show");
        expected
            .queue(LeaveAlternateScreen)
            .expect("encode alternate-screen leave");
        assert_eq!(
            output.bytes, expected,
            "every recovery step after the injected EndSynchronizedUpdate failure must still run"
        );
        assert!(
            output.flushes >= 4,
            "independent recovery groups must still reach their flush attempts"
        );
    }

    #[test]
    fn keyboard_enhancement_stack_is_popped_during_terminal_restore() {
        use crossterm::QueueableCommand as _;

        let source = include_str!("terminal.rs");
        let setup_start = source
            .find("fn enter_terminal_mode(")
            .expect("terminal setup function");
        let setup_end = source[setup_start..]
            .find("\n/// Drop the terminal")
            .map(|offset| setup_start + offset)
            .expect("terminal setup function boundary");
        let setup = &source[setup_start..setup_end];
        let publish = setup
            .find("KEYBOARD_ENHANCEMENT_PUSHED.store(true, Ordering::Release)")
            .expect("keyboard cleanup obligation publication");
        let push = setup
            .find("execute!(stdout, PushKeyboardEnhancementFlags(flags))")
            .expect("keyboard enhancement push");
        assert!(
            publish < push,
            "a partial keyboard push must already own a matching pop obligation"
        );

        let mut output = FlushRecordingWriter::default();
        write_terminal_teardown(
            &mut output,
            TerminalMode::Fullscreen,
            None,
            7,
            false,
            true,
            false,
        )
        .expect("encode keyboard-aware terminal teardown");

        let mut pop = Vec::new();
        pop.queue(PopKeyboardEnhancementFlags)
            .expect("encode keyboard pop");
        let mut leave = Vec::new();
        leave
            .queue(LeaveAlternateScreen)
            .expect("encode alternate-screen leave");
        let pop_at = output
            .bytes
            .windows(pop.len())
            .position(|window| window == pop.as_slice())
            .expect("keyboard pop must be present");
        let leave_at = output
            .bytes
            .windows(leave.len())
            .position(|window| window == leave.as_slice())
            .expect("alternate-screen leave must be present");
        assert!(
            pop_at < leave_at,
            "keyboard flags must be popped before returning screen ownership"
        );
    }

    #[test]
    fn nonminimal_terminal_setup_enables_mouse_capture() {
        use crossterm::QueueableCommand as _;

        let mut expected = Vec::new();
        expected
            .queue(EnableMouseCapture)
            .expect("encode mouse capture");
        for mode in [TerminalMode::Fullscreen, TerminalMode::Inline] {
            let mut output = Vec::new();
            write_terminal_setup(&mut output, mode, true, false).expect("encode terminal setup");
            assert!(
                output
                    .windows(expected.len())
                    .any(|window| window == expected.as_slice()),
                "{mode:?} must enable the mouse events consumed by TuiApp"
            );
        }
    }

    #[test]
    fn minimal_terminal_setup_never_enables_mouse_capture() {
        use crossterm::QueueableCommand as _;

        let mut enabled = Vec::new();
        enabled
            .queue(EnableMouseCapture)
            .expect("encode mouse capture");
        let mut output = Vec::new();
        write_terminal_setup(&mut output, TerminalMode::Minimal, false, false)
            .expect("encode minimal setup");
        assert!(
            !output
                .windows(enabled.len())
                .any(|window| window == enabled.as_slice())
        );
    }

    #[test]
    fn leaking_minimal_terminal_asserts_all_vt_mouse_modes_off() {
        let mut output = Vec::new();
        write_terminal_setup(&mut output, TerminalMode::Minimal, false, true)
            .expect("encode leaking-terminal minimal setup");
        assert!(
            output.starts_with(MOUSE_TRACKING_RESET),
            "the pinned JediTerm/Windows path must reset modes left by an earlier process"
        );
    }

    #[test]
    fn mouse_aware_teardown_resets_mouse_and_paste_before_focus() {
        use crossterm::QueueableCommand as _;

        let mut output = FlushRecordingWriter::default();
        write_terminal_teardown(
            &mut output,
            TerminalMode::Fullscreen,
            None,
            7,
            false,
            false,
            true,
        )
        .expect("encode mouse-aware teardown");
        let mouse_at = output
            .bytes
            .windows(MOUSE_PASTE_RESET.len())
            .position(|window| window == MOUSE_PASTE_RESET)
            .expect("raw mouse/paste reset");
        let mut focus = Vec::new();
        focus
            .queue(DisableFocusChange)
            .expect("encode focus disable");
        let focus_at = output
            .bytes
            .windows(focus.len())
            .position(|window| window == focus.as_slice())
            .expect("focus disable");
        assert!(mouse_at < focus_at);
    }

    #[cfg(unix)]
    #[test]
    fn unix_first_termination_is_graceful_and_every_later_signal_forces() {
        let seen = AtomicBool::new(false);
        assert_eq!(
            observe_termination_signal(&seen),
            TerminationObservation::First
        );
        assert_eq!(
            observe_termination_signal(&seen),
            TerminationObservation::Repeated
        );
        assert_eq!(
            observe_termination_signal(&seen),
            TerminationObservation::Repeated,
            "consuming the first pending slot must never re-arm graceful handling"
        );

        let pending = AtomicI32::new(0);
        pending.store(15, Ordering::Release);
        assert_eq!(pending.swap(0, Ordering::AcqRel), 15);
    }

    #[cfg(unix)]
    #[test]
    fn unix_resume_flag_distinguishes_external_from_expected_internal_continue() {
        let pending = AtomicBool::new(true);
        let expected_internal = AtomicBool::new(false);
        assert!(take_external_resume_observation(
            &pending,
            &expected_internal
        ));
        assert!(!pending.load(Ordering::Acquire));

        pending.store(true, Ordering::Release);
        expected_internal.store(true, Ordering::Release);
        assert!(!take_external_resume_observation(
            &pending,
            &expected_internal
        ));
        assert!(!pending.load(Ordering::Acquire));
        assert!(!expected_internal.load(Ordering::Acquire));
    }

    #[test]
    fn panic_cleanup_restores_and_kills_before_the_previous_hook_reports() {
        use std::cell::RefCell;

        let trace = RefCell::new(Vec::new());
        let outcome = complete_panic_cleanup_with(
            EmergencyOutputFence::Quiesced,
            |_| trace.borrow_mut().push("restore"),
            || trace.borrow_mut().push("kill-runtimes"),
            || trace.borrow_mut().push("previous-hook"),
        );
        assert_eq!(outcome, EmergencyAfterRestore::ContinueQuiesced,);
        assert_eq!(
            trace.into_inner(),
            ["restore", "kill-runtimes", "previous-hook"],
            "the fixed mother panic lifecycle restores the terminal and kills direct runtimes before reporting"
        );

        let trace = RefCell::new(Vec::new());
        let outcome = complete_panic_cleanup_with(
            EmergencyOutputFence::Contended,
            |_| trace.borrow_mut().push("restore"),
            || trace.borrow_mut().push("kill-runtimes"),
            || trace.borrow_mut().push("previous-hook"),
        );
        assert_eq!(outcome, EmergencyAfterRestore::ExitContended,);
        assert_eq!(
            trace.into_inner(),
            ["restore"],
            "an uncancellable in-flight write must exit immediately after kernel restoration"
        );
    }

    fn projected(key: &str, text: &str, streaming: bool) -> ProjectedItem {
        ProjectedItem {
            key: key.to_string(),
            kind: ProjectedKind::Assistant,
            title: "Assistant".to_string(),
            text: text.to_string(),
            streaming,
            raw_sequences: vec![1],
            tool_use_id: None,
            presentation: crate::sdk_projection::ProjectedPresentation::default(),
        }
    }
}
