mod agent_view;
mod app_event_loop;
mod app_view;
mod composer_image;
mod context_visualization;
mod crabcode_image_overlay;
mod crabcode_keybindings;
#[cfg(test)]
mod crabcode_markdown;
#[cfg(test)]
mod crabcode_mermaid;
#[cfg(test)]
mod crabcode_mermaid_affordance;
mod crabcode_mermaid_worker;
mod dialog_interaction;
mod external_editor;
mod frame_transaction;
mod generated_renderer_contract;
mod input;
mod modal_surface;
mod model_management;
mod pending_relaunch;
mod picker_surface;
#[cfg(any(target_os = "windows", test))]
mod prompt_images;
mod release_package_smoke;
mod renderer_diagnostics;
mod retained_command_surface;
pub mod runtime_host;
mod usage_plugin_management;
// CrabCode StructuredIO-to-RenderBlock product adapter consumed by AgentView.
// It does not define or widen the backend wire protocol.
mod scrollback_projection;
pub mod sdk_projection;
pub mod sdk_runtime;
mod selection_surface;
pub mod session_picker;
mod status_surface;
mod task_panel;
mod terminal;
mod terminal_capabilities;
mod terminal_fault;
mod terminal_input;
mod terminal_notifications;
pub mod terminal_output;
mod terminal_writer;
pub mod text_safety;
mod transcript_search;
pub mod tui_actions;
pub mod tui_app;
pub mod tui_clipboard;
pub mod tui_input;
mod tui_link_opener;
pub mod tui_links;
pub mod tui_render;
mod tui_scroll;
pub mod tui_ui;
mod turn_lifecycle;
mod workspace_search;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use crate::runtime_host::RuntimeHost;
use crate::sdk_runtime::{OutboundDeliveryId, OutboundSubmitError, ShutdownError, ShutdownOutcome};
#[cfg(test)]
use crate::tui_app::InitialSessionRequest;
use crate::tui_app::{HostAction, OutboundPurpose, TuiApp, UiLanguage};
use crate::tui_links::LinkTarget;
use anyhow::Context as _;
use crabcode_pager_render::audited_theme::{
    CrabCodeTheme, cache as theme_cache, system_appearance::SystemAppearanceWatcher,
};

const MAX_RUNTIME_EVENTS_PER_TICK: usize = 32;
const MAX_STDERR_FRAMES_PER_TICK: usize = 64;
const TERMINAL_INPUT_PARK_TIMEOUT: Duration = Duration::from_millis(500);
const EXTERNAL_RESUME_WRITER_DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

const fn external_editor_handoff_wait(language: UiLanguage) -> &'static str {
    language.text(
        "编辑器正在等待安全的终端交接",
        "Editor is waiting for a safe terminal handoff",
    )
}

fn pending_relaunch_deferred(language: UiLanguage, target_version: &str, error: &str) -> String {
    match language {
        UiLanguage::ZhCn => {
            format!("CrabCode {target_version} 已安装 · 自动重启已推迟：{error}")
        }
        UiLanguage::EnUs => {
            format!("CrabCode {target_version} is installed · automatic restart deferred: {error}")
        }
    }
}

fn pending_relaunch_restarting(language: UiLanguage, target_version: &str) -> String {
    match language {
        UiLanguage::ZhCn => {
            format!("CrabCode {target_version} 已安装 · 正在重启当前空闲会话")
        }
        UiLanguage::EnUs => {
            format!("CrabCode {target_version} is installed · restarting this idle session")
        }
    }
}

fn terminal_terminate_status(language: UiLanguage, signal_number: i32) -> String {
    match language {
        UiLanguage::ZhCn => format!("收到终止信号 {signal_number}，正在恢复终端"),
        UiLanguage::EnUs => {
            format!("Received termination signal {signal_number}; restoring the terminal")
        }
    }
}

const fn terminal_disconnected_status(language: UiLanguage) -> &'static str {
    language.text(
        "终端已断开；正在关闭直连运行环境",
        "Terminal disconnected; closing the direct runtime",
    )
}

const fn effective_presentation_verbose(cli_override: Option<bool>, config_verbose: bool) -> bool {
    match cli_override {
        Some(value) => value,
        None => config_verbose,
    }
}

pub fn run() -> anyhow::Result<()> {
    if let Some(result) = release_package_smoke::maybe_run(std::env::args_os()) {
        return result;
    }
    if let Some(exit_code) =
        crabcode_mermaid_worker::maybe_run_render_subprocess(std::env::args_os())
    {
        if exit_code == 0 {
            return Ok(());
        }
        anyhow::bail!("isolated CrabCode Mermaid renderer failed");
    }
    if let Some(action) = terminal::resolve_pre_terminal_action(std::env::args_os())
        .context("failed to resolve a CrabCode TUI process lifecycle action")?
    {
        return match action {
            terminal::PreTerminalAction::EnsureCronDaemon => {
                acosmi_daemon_launcher::cron::ensure_cron_daemon()
                    .map(|_handle| ())
                    .context("failed to ensure the CrabCode cron daemon")
            }
        };
    }
    let Some(process_options) = terminal::resolve_process_options()
        .context("failed to resolve the CrabCode TUI terminal mode")?
    else {
        return Ok(());
    };
    let terminal::ProcessOptions {
        terminal_plan,
        initial_session,
        initial_prompt,
        composer_prefill,
        slash_commands_enabled,
        launch_provenance,
        runtime_args,
    } = process_options;
    let initial_prompt = terminal::prepare_interactive_stdin(initial_prompt)
        .context("failed to prepare CrabCode interactive stdin")?;
    terminal_fault::install();
    let event_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("failed to create the CrabCode TUI event runtime")?;

    let launch_cwd =
        std::env::current_dir().context("failed to resolve the CrabCode runtime workspace")?;

    // Exact UUID/continue launches retain the ordinary pre-terminal spawn
    // path. Bare/title resume uses the process-private startup picker over the
    // existing direct session-storage authority, then rewrites only the
    // already-supported resume option before initialize completes. Before
    // connected terminal ownership, consume only the closed setup prefix so
    // Auto may use the startup-only OSC 11 fallback without racing a reader.
    let (mut host, source_cwd) =
        RuntimeHost::spawn_uninitialized_in(runtime_args, launch_cwd.clone())
            .context("failed to spawn the direct CrabCode runtime")?;
    let presentation_verbose = terminal_plan.presentation_verbose_override.unwrap_or(false);
    let empty_initialize = serde_json::json!({});
    let mut app = TuiApp::new_with_prefill_and_presentation(
        &empty_initialize,
        initial_session,
        initial_prompt,
        composer_prefill,
        presentation_verbose,
    );
    let diagnostics_root = acosmi_daemon_launcher::paths::home_dir();
    let diagnostics = renderer_diagnostics::RendererDiagnostics::from_state_root(&diagnostics_root)
        .unwrap_or_else(|error| {
            eprintln!("CrabCode renderer diagnostics are disabled for this run: {error}");
            renderer_diagnostics::RendererDiagnostics::default()
        });
    app.set_renderer_diagnostics(diagnostics);
    app.set_slash_commands_enabled(slash_commands_enabled);
    app.configure_pre_initialize_setup(source_cwd)
        .map_err(anyhow::Error::msg)
        .context("failed to bind the native setup lifecycle to its source workspace")?;
    app.set_minimal_mode(terminal_plan.mode == terminal::TerminalMode::Minimal);
    let mut dispatcher = ActionDispatcher::default();
    let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepare_renderer_context_before_terminal(
            &mut app,
            &mut host,
            &mut dispatcher,
            &event_runtime,
        )?;
        if let Some((zh_cn, en_us)) = launch_provenance.localized_notice() {
            app.push_localized_startup_notice(zh_cn, en_us);
        }
        let setting = app
            .renderer_theme_setting()
            .context("direct renderer context omitted its authoritative theme setting")?;
        let concrete = CrabCodeTheme::apply_initial_kind(setting);
        app.apply_runtime_theme_kind(concrete)
            .map_err(anyhow::Error::msg)
            .context("failed to bind the startup theme to the CrabCode renderer")?;
        run_connected(
            &mut app,
            &mut host,
            &mut dispatcher,
            &terminal_plan,
            &event_runtime,
            &launch_cwd,
        )
    }));

    let end_session_allowed = app.shutdown_end_session_allowed();
    let pre_shutdown_failure = app.fatal.clone();
    // This is deliberately outside every terminal/event-loop `?` path. Even
    // render, input, resize, and injected-panic failures restore the terminal
    // first and then close the process-owned child at its authoritative phase
    // boundary. `end_session` belongs to StructuredIO and is invalid while the
    // fixed setup router still owns stdin.
    dispatcher.discard_pending_before_shutdown(&mut app);
    settle_outbound_after_terminal(&mut app, &host, &mut dispatcher, &event_runtime);
    let shutdown = if end_session_allowed {
        host.shutdown(Some("tui_exit"))
    } else {
        host.shutdown_before_runtime_handoff()
    };
    drain_after_shutdown(&mut app, &host);

    match execution {
        Err(payload) => {
            if let Err(error) = shutdown {
                eprintln!("CrabCode runtime also failed to close after panic: {error}");
            }
            std::panic::resume_unwind(payload)
        }
        Ok(Ok(mut outcome)) => {
            if let Some(reason) = pre_shutdown_failure {
                return Err(preserve_primary_shutdown_failure(
                    anyhow::anyhow!("direct CrabCode runtime failed: {reason}"),
                    shutdown,
                ));
            }
            shutdown.context("failed to close the direct CrabCode runtime")?;
            if let InteractiveOutcome::Relaunch(request) = &mut outcome {
                request
                    .mark_runtime_stopped()
                    .context("pending relaunch crossed the runtime shutdown fence out of order")?;
            }
            if let InteractiveOutcome::Relaunch(request) = outcome
                && let Err(error) = request.spawn()
            {
                eprintln!(
                    "CrabCode update is installed, but automatic restart failed: {error}. \
                     Start CrabCode again to use the update."
                );
            }
            Ok(())
        }
        Ok(Err(error)) => {
            let primary = pre_shutdown_failure.map_or(error, |reason| {
                anyhow::anyhow!("direct CrabCode runtime failed: {reason}")
            });
            Err(preserve_primary_shutdown_failure(primary, shutdown))
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{primary}")]
struct PrimaryShutdownFailure {
    primary: String,
    #[source]
    cleanup: ShutdownError,
}

fn preserve_primary_shutdown_failure(
    primary: anyhow::Error,
    shutdown: Result<ShutdownOutcome, ShutdownError>,
) -> anyhow::Error {
    match shutdown {
        Ok(_) => primary,
        Err(cleanup) => anyhow::Error::new(PrimaryShutdownFailure {
            primary: format!("{primary:#}"),
            cleanup,
        }),
    }
}

/// Consume only the fixed direct runtime's renderer-context prefix before any
/// terminal reader exists.
///
/// The existing SDK initialize request remains byte-for-byte unchanged. Its
/// process-private setup router emits `renderer_context` first and then waits
/// for this renderer's correlated response, so returning as soon as that
/// closed DTO is bound cannot consume workspace trust, onboarding, or the
/// initialize response outside the native TUI lifecycle.
fn prepare_renderer_context_before_terminal(
    app: &mut TuiApp,
    host: &mut RuntimeHost,
    dispatcher: &mut ActionDispatcher,
    event_runtime: &tokio::runtime::Runtime,
) -> anyhow::Result<()> {
    let initial_actions = app.initial_actions();
    if !matches!(initial_actions.as_slice(), [HostAction::Initialize]) {
        anyhow::bail!(
            "direct renderer bootstrap expected exactly one existing SDK initialize action"
        );
    }
    dispatcher.enqueue(app, initial_actions);

    event_runtime.block_on(async {
        let outbound_ready = host.outbound_notifier();
        let runtime_ready = host.event_notifier();
        let runtime_stderr_ready = host.stderr_notifier();

        loop {
            let _progress = dispatcher.drive(app, host);
            let _stderr_progress = drain_runtime_stderr(app, host, runtime_stderr_ready.as_ref());
            if let Some(reason) = app.fatal.clone() {
                anyhow::bail!("direct renderer bootstrap failed: {reason}");
            }
            if app.setup_config_verbose().is_some()
                && app.renderer_theme_setting().is_some()
                && app.renderer_syntax_highlighting_disabled().is_some()
            {
                // Preserve level-triggered readiness if the child already
                // queued the next setup event after accepting the response.
                runtime_ready.notify_one();
                return Ok(());
            }

            match host.try_recv_event() {
                Ok(runtime_event) => {
                    let actions = app.handle_runtime_event(runtime_event);
                    dispatcher.enqueue(app, actions);
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("direct runtime stopped before emitting renderer_context");
                }
            }

            let outbound_deadline = host.next_outbound_deadline();
            tokio::select! {
                biased;

                _ = outbound_ready.notified() => {}
                _ = runtime_ready.notified() => {}
                _ = runtime_stderr_ready.notified() => {}
                _ = async {
                    match outbound_deadline {
                        Some(deadline) => tokio::time::sleep_until(
                            tokio::time::Instant::from_std(deadline)
                        ).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    })
}

fn run_connected(
    app: &mut TuiApp,
    host: &mut RuntimeHost,
    dispatcher: &mut ActionDispatcher,
    terminal_plan: &terminal::TerminalPlan,
    event_runtime: &tokio::runtime::Runtime,
    launch_cwd: &std::path::Path,
) -> anyhow::Result<InteractiveOutcome> {
    tui_input::prepare_physical_modifier_probe();
    let mut terminal = terminal::TerminalSession::enter(terminal_plan)
        .context("failed to take ownership of the interactive terminal")?;
    let effective_minimal = terminal.mode() == terminal::TerminalMode::Minimal;
    app.set_minimal_mode(effective_minimal);
    crabcode_pager_render::modal_window::set_embedded(effective_minimal);
    app.set_mouse_clicks_disabled(terminal_plan.mouse_clicks_disabled);
    if let Some((zh_cn, en_us)) = terminal.localized_setup_notice() {
        app.push_localized_startup_notice(zh_cn.to_string(), en_us.to_string());
    }
    let mut terminal_input = terminal_input::TerminalEventSource::start()
        .context("failed to start the CrabCode terminal input reader")?;
    let writer_events = terminal
        .take_presentation_events()
        .context("failed to take the fixed-upstream terminal writer event stream")?;
    #[cfg(feature = "terminal-lifecycle-tests")]
    inject_test_only_fault_after_raw()?;
    let effective_mode = terminal_plan.effective_summary(app.ui_language(), terminal.mode());
    app.status = format!("{} · {effective_mode}", app.status);

    let initial_actions = app.initial_actions();
    dispatcher.enqueue(app, initial_actions);
    let _progress = dispatcher.drive(app, host);
    let outcome = event_runtime.block_on(run_interactive(
        app,
        host,
        dispatcher,
        &mut terminal,
        &mut terminal_input,
        writer_events,
        terminal_plan.presentation_verbose_override,
        launch_cwd,
    ));
    // Stop the only terminal reader before releasing raw/screen ownership, so
    // no background read can consume input intended for the restored shell.
    terminal_input
        .stop()
        .context("failed to stop terminal input during TUI shutdown")?;
    terminal
        .suspend()
        .context("failed to drain terminal output during TUI shutdown")?;
    let mut outcome = outcome?;
    if let InteractiveOutcome::Relaunch(request) = &mut outcome {
        request
            .mark_terminal_restored()
            .context("pending relaunch crossed the terminal restoration fence out of order")?;
    }
    Ok(outcome)
}

#[derive(Debug)]
enum InteractiveOutcome {
    Quit,
    Relaunch(pending_relaunch::PendingRelaunch),
}

#[derive(Debug)]
struct PendingExternalHandoff {
    request: ExternalHandoffRequest,
    retry_after: Option<Instant>,
    wait_reported: bool,
}

#[derive(Debug)]
enum ExternalHandoffRequest {
    EditPrompt {
        original_text: String,
    },
    OpenFile {
        canonical_path: PathBuf,
        line: Option<usize>,
    },
}

impl PendingExternalHandoff {
    fn edit_prompt(original_text: String) -> Self {
        Self {
            request: ExternalHandoffRequest::EditPrompt { original_text },
            retry_after: None,
            wait_reported: false,
        }
    }

    fn open_file(canonical_path: PathBuf, line: Option<usize>) -> Self {
        Self {
            request: ExternalHandoffRequest::OpenFile {
                canonical_path,
                line,
            },
            retry_after: None,
            wait_reported: false,
        }
    }
}

const SUSPEND_RETRY_DELAY: Duration = Duration::from_millis(250);

fn suspend_retry_ready(retry_after: Option<Instant>, now: Instant) -> bool {
    retry_after.is_none_or(|deadline| now >= deadline)
}

/// Arm the deferred retry and return whether this pending handoff needs feedback.
fn defer_suspend_retry(
    retry_after: &mut Option<Instant>,
    wait_reported: &mut bool,
    now: Instant,
) -> bool {
    debug_assert!(retry_after.is_none());
    *retry_after = Some(now + SUSPEND_RETRY_DELAY);
    let should_report = !*wait_reported;
    *wait_reported = true;
    should_report
}

fn requeue_after_suspend_timeout<T>(pending: &mut Option<T>, request: T) {
    // The child never started, so preserve the one-shot request.
    *pending = Some(request);
}

#[derive(Debug)]
enum DispatchFailure {
    Recoverable(String),
    DeliveryIndeterminate(String),
}

fn classify_send_error(error: sdk_runtime::SendError) -> DispatchFailure {
    let fatal = matches!(
        error,
        sdk_runtime::SendError::Closed
            | sdk_runtime::SendError::Write(_)
            | sdk_runtime::SendError::TimedOut { .. }
    );
    let message = error.to_string();
    if fatal {
        DispatchFailure::DeliveryIndeterminate(message)
    } else {
        DispatchFailure::Recoverable(message)
    }
}

/// Finish one terminal-generation cutover while the sole input reader remains
/// parked. Every operation is attempted in caller-defined order; the first
/// error kind remains authoritative and later failures are appended as
/// cleanup context. Input ownership is returned only when the complete
/// transaction succeeds.
fn complete_parked_terminal_cutover<const N: usize>(
    steps: [(io::Result<()>, &'static str); N],
    mut unpark_reader: impl FnMut(),
) -> io::Result<()> {
    let mut first_error: Option<io::Error> = None;
    for (result, context) in steps {
        let Err(error) = result else {
            continue;
        };
        first_error = Some(match first_error {
            None => io::Error::new(error.kind(), format!("{context}: {error}")),
            Some(first) => io::Error::new(
                first.kind(),
                format!("{first}; {context} also failed: {error}"),
            ),
        });
    }
    match first_error {
        Some(error) => Err(error),
        None => {
            unpark_reader();
            Ok(())
        }
    }
}

/// The only product callback boundary used by the fixed terminal lifecycle.
///
/// This adapter keeps the existing direct `RuntimeHost`/`ActionDispatcher`
/// semantics intact. It exposes renderer-local callbacks to the lifecycle
/// driver and deliberately owns no transport DTO, session protocol, or GUI
/// compatibility path.
pub(crate) struct CrabCodeDirectRuntimeAdapter<'a> {
    app: &'a mut TuiApp,
    host: &'a mut RuntimeHost,
    dispatcher: &'a mut ActionDispatcher,
    terminal_notifications: terminal_notifications::TerminalNotificationService,
    applied_setup_verbose: Option<bool>,
    pending_external_handoff: Option<PendingExternalHandoff>,
    pending_relaunch_monitor: pending_relaunch::PendingRelaunchMonitor,
    pending_relaunch: Option<pending_relaunch::PendingRelaunch>,
    launch_cwd: PathBuf,
    presentation_verbose_override: Option<bool>,
}

impl<'a> CrabCodeDirectRuntimeAdapter<'a> {
    fn new(
        app: &'a mut TuiApp,
        host: &'a mut RuntimeHost,
        dispatcher: &'a mut ActionDispatcher,
        presentation_verbose_override: Option<bool>,
        launch_cwd: &std::path::Path,
    ) -> Self {
        Self {
            app,
            host,
            dispatcher,
            terminal_notifications: terminal_notifications::TerminalNotificationService::default(),
            applied_setup_verbose: None,
            pending_external_handoff: None,
            pending_relaunch_monitor: pending_relaunch::PendingRelaunchMonitor::from_process(
                Instant::now(),
            ),
            pending_relaunch: None,
            launch_cwd: launch_cwd.to_path_buf(),
            presentation_verbose_override,
        }
    }

    pub(crate) fn direct_readiness(
        &self,
    ) -> (
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        (
            self.host.outbound_notifier(),
            self.host.event_notifier(),
            self.host.stderr_notifier(),
        )
    }

    pub(crate) fn should_quit(&self) -> bool {
        self.app.should_quit
    }

    pub(crate) fn pending_relaunch_deadline(&self) -> Option<Instant> {
        if self.pending_relaunch.is_some() {
            None
        } else {
            self.pending_relaunch_monitor.deadline()
        }
    }

    pub(crate) fn poll_pending_relaunch(&mut self, now: Instant) -> bool {
        if self.pending_relaunch.is_some() {
            return false;
        }
        let facts = pending_relaunch::DirectIdleFacts {
            busy: self.app.busy(),
            session_id: self.app.projection.session_id(),
            session_state: self.app.projection.session_state(),
            active_task_count: self.app.active_tasks.len(),
            processing_background_task: self.app.processing_background_task(),
            fatal: self.app.fatal.is_some(),
        };
        let Some(request) = self.pending_relaunch_monitor.poll(
            now,
            env!("CARGO_PKG_VERSION"),
            facts,
            self.app.composer.text(),
            &self.launch_cwd,
        ) else {
            return false;
        };
        if let Err(error) = request.preflight() {
            let status = pending_relaunch_deferred(
                self.app.ui_language(),
                request.target_version(),
                &error.to_string(),
            );
            let changed = self.app.status != status;
            self.app.status = status;
            return changed;
        }
        self.app.status =
            pending_relaunch_restarting(self.app.ui_language(), request.target_version());
        self.app.should_quit = true;
        self.pending_relaunch = Some(request);
        true
    }

    fn take_pending_relaunch(&mut self) -> Option<pending_relaunch::PendingRelaunch> {
        self.pending_relaunch.take()
    }

    pub(crate) fn service_terminal_signals(
        &mut self,
        terminal: &mut terminal::TerminalSession,
        terminal_input: &mut terminal_input::TerminalEventSource,
        presenter: &mut Presenter,
        resize_debounce_at: &mut Option<Instant>,
        mut retire_event_loop_parsers: impl FnMut(),
    ) -> anyhow::Result<bool> {
        let mut terminal_generation_changed = false;
        while let Some(signal) = terminal.pending_signal() {
            match signal {
                terminal::TerminalSignal::Terminate(number) => {
                    self.app.status = terminal_terminate_status(self.app.ui_language(), number);
                    self.app.should_quit = true;
                }
                terminal::TerminalSignal::Suspend => {
                    if !terminal_input.park_reader(TERMINAL_INPUT_PARK_TIMEOUT) {
                        terminal_input.unpark_reader();
                        anyhow::bail!(
                            "terminal input reader did not park before interactive suspend"
                        );
                    }
                    // A production macOS PTY sample proved that the
                    // process-global CoreGraphics modifier query may never
                    // return after SIGSTOP/SIGCONT. Retire that renderer-only
                    // side channel before crossing job control; terminal
                    // delivered modifiers remain available after resume.
                    tui_input::disable_physical_modifier_probe_for_job_control();
                    self.app
                        .retire_input_side_channels_for_terminal_generation();
                    terminal.retire_input_side_channels_for_terminal_generation();
                    if let Err(error) = terminal.stop_until_continued() {
                        return complete_parked_terminal_cutover(
                            [(
                                Err(error),
                                "failed to suspend and restore the interactive terminal",
                            )],
                            || terminal_input.unpark_reader(),
                        )
                        .map(|()| false)
                        .context("interactive suspend cutover failed closed");
                    }
                    terminal_input.retire_crossterm_generation();
                    let discarded = terminal_input.discard_pending().map(|_| ());
                    retire_event_loop_parsers();
                    *resize_debounce_at = None;
                    presenter.reset_after_terminal_generation_change();
                    // This tcflush is the input-generation linearization
                    // point. Bytes accepted before it are old; bytes arriving
                    // after it remain queued for the reader after unpark.
                    let tty_discarded = terminal_input.discard_pending_tty_input();
                    complete_parked_terminal_cutover(
                        [
                            (
                                discarded,
                                "failed to discard input accepted before the resumed terminal generation",
                            ),
                            (
                                tty_discarded,
                                "failed to establish the resumed tty input generation boundary",
                            ),
                        ],
                        || terminal_input.unpark_reader(),
                    )
                    .context("interactive suspend input cutover failed closed")?;
                    terminal_generation_changed = true;
                }
                terminal::TerminalSignal::Resume => {
                    if !terminal_input.park_reader(TERMINAL_INPUT_PARK_TIMEOUT) {
                        terminal_input.unpark_reader();
                        anyhow::bail!(
                            "terminal input reader did not park before external resume cutover"
                        );
                    }
                    // A SIGCONT observed outside the explicit suspend branch
                    // retires future physical probes. An uncatchable external
                    // SIGSTOP cannot unwind a probe that was already blocked,
                    // so this is deliberately not claimed as in-flight
                    // recovery.
                    tui_input::disable_physical_modifier_probe_for_job_control();
                    self.app
                        .retire_input_side_channels_for_terminal_generation();
                    terminal.retire_input_side_channels_for_terminal_generation();

                    if let Err(error) = terminal.park_control_writer_for_child() {
                        return Err(error).context(
                            "failed to close standalone terminal controls before external resume",
                        );
                    }
                    let writer_drain =
                        terminal.wait_writer_drained(EXTERNAL_RESUME_WRITER_DRAIN_TIMEOUT);
                    match writer_drain {
                        Ok(terminal_writer::WriterDrain::Drained) => {}
                        Ok(terminal_writer::WriterDrain::TimedOut) => {
                            let restore = terminal.restore_control_writer_after_child();
                            return complete_parked_terminal_cutover(
                                [
                                    (
                                        Err(io::Error::new(
                                            io::ErrorKind::TimedOut,
                                            "terminal writer did not drain before external resume healing",
                                        )),
                                        "external-resume writer drain",
                                    ),
                                    (
                                        restore,
                                        "failed to restore standalone terminal controls after the writer drain timeout",
                                    ),
                                ],
                                || terminal_input.unpark_reader(),
                            )
                            .map(|()| false)
                            .context("external resume writer fence failed closed");
                        }
                        Err(error) => {
                            let restore = terminal.restore_control_writer_after_child();
                            return complete_parked_terminal_cutover(
                                [
                                    (
                                        Err(error),
                                        "terminal writer failed before external resume healing",
                                    ),
                                    (
                                        restore,
                                        "failed to restore standalone terminal controls after writer failure",
                                    ),
                                ],
                                || terminal_input.unpark_reader(),
                            )
                            .map(|()| false)
                            .context("external resume writer fence failed closed");
                        }
                    }

                    if let Err(error) = terminal.heal_after_external_resume() {
                        return complete_parked_terminal_cutover(
                            [(Err(error), "failed to heal the externally resumed terminal")],
                            || terminal_input.unpark_reader(),
                        )
                        .map(|()| false)
                        .context("external resume terminal healing failed closed");
                    }
                    terminal_input.retire_crossterm_generation();
                    let discarded = terminal_input.discard_pending().map(|_| ());
                    let control_restored = terminal.restore_control_writer_after_child();
                    retire_event_loop_parsers();
                    *resize_debounce_at = None;
                    presenter.reset_after_terminal_generation_change();
                    // Final tty flush is the exact input-generation
                    // linearization point and is intentionally the last input
                    // mutation before the sole reader is released.
                    let tty_discarded = terminal_input.discard_pending_tty_input();
                    #[cfg(feature = "terminal-lifecycle-tests")]
                    let readiness = publish_test_only_resume_cutover_ready();
                    #[cfg(not(feature = "terminal-lifecycle-tests"))]
                    let readiness = Ok(());
                    complete_parked_terminal_cutover(
                        [
                            (
                                discarded,
                                "failed to discard queued input accepted before the external resume cutover",
                            ),
                            (
                                control_restored,
                                "failed to restore standalone terminal controls after external resume",
                            ),
                            (
                                tty_discarded,
                                "failed to establish the external-resume tty input boundary",
                            ),
                            (
                                readiness,
                                "failed to publish external-resume cutover readiness",
                            ),
                        ],
                        || terminal_input.unpark_reader(),
                    )
                    .context("external resume input cutover failed closed")?;
                    terminal_generation_changed = true;
                }
            }
        }
        Ok(terminal_generation_changed)
    }

    pub(crate) fn inspect_terminal_liveness(
        &mut self,
        terminal: &terminal::TerminalSession,
    ) -> anyhow::Result<()> {
        if terminal
            .input_disconnected()
            .context("failed to inspect terminal input liveness")?
        {
            self.app.status = terminal_disconnected_status(self.app.ui_language()).to_string();
            self.app.should_quit = true;
        }
        Ok(())
    }

    pub(crate) fn service_terminal_requests(
        &mut self,
        terminal: &mut terminal::TerminalSession,
        presenter: &mut Presenter,
        resize_debounce_at: &mut Option<Instant>,
    ) -> anyhow::Result<()> {
        self.terminal_notifications
            .synchronize_renderer_config(self.app);
        if let Some(request) = self.app.take_terminal_notification_request() {
            self.terminal_notifications
                .send(terminal, &request)
                .context("failed to emit a CrabCode terminal notification")?;
        }
        if let Some(target) = self.app.take_link_open_request() {
            handle_link_open(self.app, &target);
            *resize_debounce_at = None;
            presenter.request(false);
        }
        Ok(())
    }

    pub(crate) fn materialize_pending_terminal_handoff(&mut self) {
        if self.pending_external_handoff.is_none()
            && let Some(original_text) = self.app.take_external_editor_request()
        {
            self.pending_external_handoff =
                Some(PendingExternalHandoff::edit_prompt(original_text));
        }
        if self.pending_external_handoff.is_none()
            && let Some(request) = self.app.take_external_file_open_request()
        {
            self.pending_external_handoff = Some(PendingExternalHandoff::open_file(
                request.path,
                request.line,
            ));
        }
    }

    pub(crate) fn run_pending_terminal_handoff(
        &mut self,
        terminal: &mut terminal::TerminalSession,
        terminal_input: &mut terminal_input::TerminalEventSource,
        presenter: &mut Presenter,
    ) -> anyhow::Result<bool> {
        run_pending_external_handoff(
            self.app,
            terminal,
            terminal_input,
            &mut self.pending_external_handoff,
            presenter,
        )
    }

    pub(crate) fn present_if_dirty(
        &mut self,
        terminal: &mut terminal::TerminalSession,
        presenter: &mut Presenter,
    ) -> anyhow::Result<()> {
        let progress = terminal
            .presentation_progress()
            .context("failed to inspect CrabCode terminal presentation progress")?;
        presenter.acknowledge(progress.written);
        let queued_before = progress.queued;
        let queued_after = std::cell::Cell::new(queued_before);
        let written_after = std::cell::Cell::new(progress.written);
        let mut presentation_error = None;
        let drew = if self.app.initial_presentation_ready() {
            presenter.try_present(
                queued_before,
                |force_full_repaint| {
                    if let Err(error) = terminal.present_with_repaint(self.app, force_full_repaint)
                    {
                        presentation_error = Some(error);
                        return;
                    }
                    match terminal.presentation_progress() {
                        Ok(progress) => {
                            queued_after.set(progress.queued);
                            written_after.set(progress.written);
                        }
                        Err(error) => presentation_error = Some(error),
                    }
                },
                || queued_after.get(),
            )
        } else {
            false
        };
        if let Some(error) = presentation_error {
            return Err(error).context("failed to present CrabCode TUI frame");
        }
        if drew {
            presenter.mark_drawn(Instant::now());
            presenter.acknowledge(written_after.get());
        }
        Ok(())
    }

    pub(crate) fn renderer_animation_deadline(&self) -> Option<Instant> {
        self.app.renderer_animation_deadline()
    }

    pub(crate) fn has_deferred_input_work(&self) -> bool {
        self.app.has_deferred_input_work()
    }

    pub(crate) fn scroll_tick_at(&mut self, now: Instant) -> Option<Instant> {
        self.app.mouse_scroll_deadline(now).map(|delay| now + delay)
    }

    pub(crate) fn suspend_retry_at(&self) -> Option<Instant> {
        self.pending_external_handoff
            .as_ref()
            .and_then(|pending| pending.retry_after)
    }

    pub(crate) fn outbound_deadline(&self) -> Option<Instant> {
        self.host.next_outbound_deadline()
    }

    pub(crate) fn drive_outbound(
        &mut self,
        presenter: &mut Presenter,
        resize_debounce_at: &mut Option<Instant>,
    ) {
        if self.dispatcher.drive(self.app, self.host).progressed {
            *resize_debounce_at = None;
            presenter.request(false);
        }
    }

    pub(crate) fn drain_direct_runtime(
        &mut self,
        terminal_input: &terminal_input::TerminalEventSource,
        runtime_ready: &tokio::sync::Notify,
        presenter: &mut Presenter,
        resize_debounce_at: &mut Option<Instant>,
        appearance_watcher: &mut Option<SystemAppearanceWatcher>,
    ) {
        if drain_runtime_events(
            self.app,
            self.host,
            self.dispatcher,
            terminal_input,
            runtime_ready,
        ) {
            *resize_debounce_at = None;
            let _requested_immediately =
                presenter.request_throttled(Instant::now(), app_event_loop::EVENT_LOOP_CADENCE);
        }
        synchronize_setup_presentation(
            self.app,
            self.presentation_verbose_override,
            &mut self.applied_setup_verbose,
        );
        let initial_actions = self.app.initial_actions();
        if !initial_actions.is_empty() {
            *resize_debounce_at = None;
            presenter.request(false);
        }
        self.dispatcher.enqueue(self.app, initial_actions);
        if self.dispatcher.drive(self.app, self.host).progressed {
            *resize_debounce_at = None;
            presenter.request(false);
        }
        sync_appearance_watcher(self.app, appearance_watcher);
    }

    pub(crate) fn drain_direct_runtime_stderr(
        &mut self,
        runtime_stderr_ready: &tokio::sync::Notify,
        presenter: &mut Presenter,
        resize_debounce_at: &mut Option<Instant>,
    ) {
        if drain_runtime_stderr(self.app, self.host, runtime_stderr_ready) {
            *resize_debounce_at = None;
            presenter.request(false);
        }
    }

    pub(crate) fn handle_terminal_event(
        &mut self,
        terminal: &mut terminal::TerminalSession,
        routed: app_event_loop::RoutedInputEvent,
    ) -> anyhow::Result<app_event_loop::HandledInput> {
        if matches!(routed.event, crossterm::event::Event::Resize(_, _)) {
            terminal
                .resized()
                .context("failed to resize the interactive terminal viewport")?;
        }
        let mut force_repaint = match &routed.event {
            crossterm::event::Event::FocusGained => terminal
                .focus_changed(true)
                .context("failed to heal the terminal after focus returned")?,
            crossterm::event::Event::FocusLost => {
                terminal
                    .focus_changed(false)
                    .context("failed to arm terminal focus healing")?;
                false
            }
            _ => false,
        };
        let outcome = terminal.handle_renderer_input(
            self.app,
            routed.event,
            routed.arrived_at,
            routed.paste_provenance,
        );
        force_repaint |= self.app.take_force_repaint_request();
        let mut needs_draw = outcome.needs_frame;
        self.dispatcher.enqueue(self.app, outcome.actions);
        needs_draw |= self.dispatcher.drive(self.app, self.host).progressed;
        if let Some(target) = self.app.take_link_open_request() {
            handle_link_open(self.app, &target);
            needs_draw = true;
        }
        if self.pending_external_handoff.is_none()
            && let Some(original_text) = self.app.take_external_editor_request()
        {
            self.pending_external_handoff =
                Some(PendingExternalHandoff::edit_prompt(original_text));
            needs_draw = true;
        }
        if self.pending_external_handoff.is_none()
            && let Some(request) = self.app.take_external_file_open_request()
        {
            self.pending_external_handoff = Some(PendingExternalHandoff::open_file(
                request.path,
                request.line,
            ));
            needs_draw = true;
        }
        Ok(app_event_loop::HandledInput {
            needs_draw,
            force_repaint,
            stop_batch: self.pending_external_handoff.is_some(),
            should_quit: self.app.should_quit,
        })
    }

    pub(crate) fn synchronize_appearance_watcher(
        &self,
        watcher: &mut Option<SystemAppearanceWatcher>,
    ) {
        sync_appearance_watcher(self.app, watcher);
    }

    pub(crate) fn release_suspend_retry(&mut self) {
        if let Some(pending) = self.pending_external_handoff.as_mut() {
            pending.retry_after = None;
        }
    }

    pub(crate) fn tick_scroll(&mut self) -> bool {
        self.app.tick_mouse_scroll()
    }

    pub(crate) fn tick_renderer_animation(&mut self) -> bool {
        let renderer_animation_progressed = self.app.tick_renderer_animation(Instant::now());
        let (deferred_actions, deferred_progressed) = self.app.poll_deferred_input_with_progress();
        let progressed =
            renderer_animation_progressed || deferred_progressed || !deferred_actions.is_empty();
        self.dispatcher.enqueue(self.app, deferred_actions);
        progressed | self.dispatcher.drive(self.app, self.host).progressed
    }

    pub(crate) fn apply_system_appearance(
        &mut self,
        appearance_watcher: Option<&SystemAppearanceWatcher>,
    ) -> anyhow::Result<bool> {
        let Some(appearance) = appearance_watcher.and_then(SystemAppearanceWatcher::current) else {
            return Ok(false);
        };
        let Some(concrete) = theme_cache::apply_runtime_appearance(Some(appearance)) else {
            return Ok(false);
        };
        self.app
            .apply_runtime_theme_kind(concrete)
            .map_err(anyhow::Error::msg)
            .context("failed to apply a system appearance change")
    }

    pub(crate) fn initial_appearance_watcher(&self) -> Option<SystemAppearanceWatcher> {
        SystemAppearanceWatcher::start_if_auto(
            self.app
                .renderer_active_theme_setting()
                .is_some_and(|setting| setting.is_auto()),
        )
    }

    pub(crate) fn configure_scroll_cadence(&mut self) {
        self.app
            .set_mouse_scroll_redraw_cadence(app_event_loop::EVENT_LOOP_CADENCE);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive(
    app: &mut TuiApp,
    host: &mut RuntimeHost,
    dispatcher: &mut ActionDispatcher,
    terminal: &mut terminal::TerminalSession,
    terminal_input: &mut terminal_input::TerminalEventSource,
    writer_events: tokio::sync::mpsc::UnboundedReceiver<terminal_writer::WriterEvent>,
    presentation_verbose_override: Option<bool>,
    launch_cwd: &std::path::Path,
) -> anyhow::Result<InteractiveOutcome> {
    crabcode_image_overlay::reset_terminal_image_owner();
    let pending_relaunch = {
        let mut adapter = CrabCodeDirectRuntimeAdapter::new(
            app,
            host,
            dispatcher,
            presentation_verbose_override,
            launch_cwd,
        );
        app_event_loop::run_fixed_terminal_lifecycle(
            &mut adapter,
            terminal,
            terminal_input,
            writer_events,
        )
        .await?;
        adapter.take_pending_relaunch()
    };
    finish_interactive_outcome(
        app,
        terminal,
        pending_relaunch.map_or(InteractiveOutcome::Quit, InteractiveOutcome::Relaunch),
    )
}

/// Keep watcher ownership aligned with the process-local Auto setting.
fn sync_appearance_watcher(app: &TuiApp, watcher: &mut Option<SystemAppearanceWatcher>) {
    let should_auto = app
        .renderer_active_theme_setting()
        .is_some_and(|setting| setting.is_auto());
    debug_assert_eq!(
        should_auto,
        theme_cache::is_auto_mode(),
        "active renderer setting and audited theme cache diverged"
    );
    if should_auto != watcher.is_some() {
        *watcher = SystemAppearanceWatcher::start_if_auto(should_auto);
    }
}

fn writer_event_sequence(event: terminal_writer::WriterEvent) -> std::io::Result<u64> {
    match event {
        terminal_writer::WriterEvent::Written(sequence) => Ok(sequence),
        terminal_writer::WriterEvent::Failed(error) => Err(error),
    }
}

fn handle_link_open(app: &mut TuiApp, target: &LinkTarget) {
    // Fixed renderer lifecycle: OS URL/file openers do not inherit the TTY,
    // so they never suspend raw mode or replace the sole input reader.
    match tui_link_opener::try_open_target(target) {
        tui_link_opener::OpenLinkResult::Opened => app.link_open_succeeded(target),
        tui_link_opener::OpenLinkResult::HandlerUnavailable(reason) => {
            app.link_open_failed(target, &reason);
        }
        tui_link_opener::OpenLinkResult::Rejected => app.link_open_rejected(),
    }
}

fn run_pending_external_handoff(
    app: &mut TuiApp,
    terminal: &mut terminal::TerminalSession,
    terminal_input: &mut terminal_input::TerminalEventSource,
    pending: &mut Option<PendingExternalHandoff>,
    presenter: &mut Presenter,
) -> anyhow::Result<bool> {
    let Some(mut request) = pending.take() else {
        return Ok(false);
    };
    if !suspend_retry_ready(request.retry_after, Instant::now()) {
        *pending = Some(request);
        return Ok(false);
    }
    request.retry_after = None;

    match &request.request {
        ExternalHandoffRequest::EditPrompt { original_text } => {
            let prepared = match external_editor::prepare_prompt(original_text) {
                external_editor::PromptEditPreparation::Ready(prepared) => prepared,
                external_editor::PromptEditPreparation::Finished(result) => {
                    app.complete_external_editor(original_text, result);
                    presenter.request(false);
                    return Ok(true);
                }
            };

            let mut edit_result = external_editor::PromptEditOutcome::NoChange;
            let moved_cursor =
                match external_editor::suspend_for_child(terminal, terminal_input, || {
                    edit_result = prepared.run()
                }) {
                    Ok(moved_cursor) => moved_cursor,
                    Err(error) if error.is_retryable_before_child() => {
                        defer_external_handoff(app, presenter, pending, request);
                        return Ok(true);
                    }
                    Err(error) => {
                        return Err(error)
                            .context("failed to hand the terminal to the external prompt editor");
                    }
                };
            terminal
                .restore_after_child(moved_cursor)
                .context("failed to restore presentation after closing the prompt editor")?;

            app.complete_external_editor(original_text, edit_result);
        }
        ExternalHandoffRequest::OpenFile {
            canonical_path,
            line,
        } => {
            let prepared = match external_editor::prepare_file_open(canonical_path.clone(), *line) {
                external_editor::FileOpenPreparation::Ready(prepared) => prepared,
                external_editor::FileOpenPreparation::Unavailable => {
                    // Fixed historical QuickOpen/GlobalSearch close the
                    // picker and emit no UI result when no editor exists.
                    presenter.request(false);
                    return Ok(true);
                }
            };
            if !prepared.requires_terminal_handoff() {
                // Historical QuickOpen/GlobalSearch GUI editors detach and
                // never own stdin/stdout. Keep the native renderer active;
                // only the already-closed picker needs a normal redraw.
                prepared.run();
                presenter.request(false);
                return Ok(true);
            }
            let moved_cursor =
                match external_editor::suspend_for_child(terminal, terminal_input, || {
                    prepared.run()
                }) {
                    Ok(moved_cursor) => moved_cursor,
                    Err(error) if error.is_retryable_before_child() => {
                        defer_external_handoff(app, presenter, pending, request);
                        return Ok(true);
                    }
                    Err(error) => {
                        return Err(error)
                            .context("failed to hand the terminal to the external file editor");
                    }
                };
            terminal
                .restore_after_child(moved_cursor)
                .context("failed to restore presentation after closing the file editor")?;
        }
    }

    presenter.request(true);
    Ok(true)
}

fn defer_external_handoff(
    app: &mut TuiApp,
    presenter: &mut Presenter,
    pending: &mut Option<PendingExternalHandoff>,
    mut request: PendingExternalHandoff,
) {
    if defer_suspend_retry(
        &mut request.retry_after,
        &mut request.wait_reported,
        Instant::now(),
    ) {
        let notice = external_editor_handoff_wait(app.ui_language()).to_string();
        app.status.clone_from(&notice);
        app.push_runtime_notice(notice);
        presenter.request(false);
    }
    requeue_after_suspend_timeout(pending, request);
}

fn finish_interactive_outcome(
    app: &mut TuiApp,
    terminal: &mut terminal::TerminalSession,
    outcome: InteractiveOutcome,
) -> anyhow::Result<InteractiveOutcome> {
    // A quit/session transition can happen immediately after input, before
    // the next ordinary draw. Paint one final preview-free frame so Kitty ID 1
    // is cleared inside the synchronized writer before terminal ownership is
    // released or moved to a new runtime generation.
    app.dismiss_image_preview_for_terminal_exit();
    terminal
        .present(app)
        .context("failed to clear the CrabCode image overlay before leaving the TUI generation")?;
    Ok(outcome)
}

/// Fixed-upstream presentation transaction.
///
/// `TerminalSession` is the CrabCode adapter supplied to `try_present`: this
/// state machine remains renderer-only and cannot send or reinterpret backend
/// traffic.
#[derive(Debug)]
struct Presenter {
    dirty: bool,
    force_full_repaint: bool,
    in_flight_target: Option<u64>,
    last_draw_at: Instant,
    draw_scheduled_at: Option<Instant>,
}

impl Presenter {
    fn new() -> Self {
        Self {
            dirty: false,
            force_full_repaint: false,
            in_flight_target: None,
            last_draw_at: Instant::now(),
            draw_scheduled_at: None,
        }
    }

    fn acknowledge(&mut self, sequence: u64) {
        if self
            .in_flight_target
            .is_some_and(|target| sequence >= target)
        {
            self.in_flight_target = None;
        }
    }

    fn try_present(
        &mut self,
        queued_before: u64,
        draw: impl FnOnce(bool),
        queued_after: impl FnOnce() -> u64,
    ) -> bool {
        if self.in_flight_target.is_some() || !self.dirty {
            return false;
        }
        let force_full_repaint = std::mem::take(&mut self.force_full_repaint);
        self.dirty = false;
        draw(force_full_repaint);
        let target = queued_after();
        if target > queued_before {
            self.in_flight_target = Some(target);
        }
        true
    }

    fn request(&mut self, force_full_repaint: bool) {
        self.dirty = true;
        self.force_full_repaint |= force_full_repaint;
    }

    /// Request now when cadence permits; otherwise schedule the earliest draw.
    fn request_throttled(&mut self, now: Instant, min_draw_interval: Duration) -> bool {
        if now.duration_since(self.last_draw_at) < min_draw_interval {
            if self.draw_scheduled_at.is_none() {
                self.draw_scheduled_at = Some(self.last_draw_at + min_draw_interval);
            }
            return false;
        }
        self.request(false);
        true
    }

    fn mark_drawn(&mut self, now: Instant) {
        self.last_draw_at = now;
        self.draw_scheduled_at = None;
    }

    fn reset_after_terminal_generation_change(&mut self) {
        crate::crabcode_image_overlay::reset_terminal_image_owner();
        self.in_flight_target = None;
        self.draw_scheduled_at = None;
        self.request(true);
    }
}

fn drain_runtime_events(
    app: &mut TuiApp,
    host: &mut RuntimeHost,
    dispatcher: &mut ActionDispatcher,
    terminal_input: &terminal_input::TerminalEventSource,
    runtime_ready: &tokio::sync::Notify,
) -> bool {
    let mut progressed = false;
    let mut rearm = false;
    for index in 0..MAX_RUNTIME_EVENTS_PER_TICK {
        if progressed && terminal_input.has_pending() {
            // Terminal input preempts an otherwise-ready runtime receiver. Keep
            // its readiness level-triggered so queued runtime events are not
            // stranded after the input event is handled.
            rearm = true;
            break;
        }
        let runtime_event = match host.try_recv_event() {
            Ok(runtime_event) => runtime_event,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        };
        progressed = true;
        let actions = app.handle_runtime_event(runtime_event);
        dispatcher.enqueue(app, actions);
        let dispatch = dispatcher.drive(app, host);
        progressed |= dispatch.progressed;
        if index + 1 == MAX_RUNTIME_EVENTS_PER_TICK {
            // The bounded drain consumed its full budget. There may be more
            // queued work, so explicitly schedule another fair turn.
            rearm = true;
        }
    }
    if rearm {
        // RuntimeHost exposes readiness as a notifier rather than the fixed
        // upstream receiver. Re-arm it when a bounded/input-preempted drain may
        // have left queued events, preserving the receiver's level-triggered
        // behavior. Empty and Disconnected are both drained terminal states:
        // re-arming either would create a self-waking hot loop.
        runtime_ready.notify_one();
    }
    progressed
}

fn drain_runtime_stderr(
    app: &mut TuiApp,
    host: &RuntimeHost,
    runtime_stderr_ready: &tokio::sync::Notify,
) -> bool {
    let mut progressed = false;
    let mut rearm = false;
    for index in 0..MAX_STDERR_FRAMES_PER_TICK {
        let frame = match host.try_recv_stderr() {
            Ok(frame) => frame,
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        };
        progressed = true;
        app.push_stderr(&frame.bytes, frame.truncated);
        if index + 1 == MAX_STDERR_FRAMES_PER_TICK {
            rearm = true;
        }
    }
    if rearm {
        runtime_stderr_ready.notify_one();
    }
    progressed
}

#[derive(Debug, Default)]
struct ActionDispatcher {
    pending: VecDeque<HostAction>,
    in_flight: HashMap<OutboundDeliveryId, HostAction>,
    aborting: bool,
}

#[derive(Debug, Default)]
struct DispatchProgress {
    progressed: bool,
}

impl ActionDispatcher {
    fn enqueue(&mut self, app: &mut TuiApp, actions: Vec<HostAction>) {
        if !self.aborting {
            self.pending.extend(actions);
        }
        self.capture_fail_closed_stop(app);
    }

    fn capture_fail_closed_stop(&mut self, app: &mut TuiApp) {
        let Some(stop) = app.take_runtime_stop_action() else {
            return;
        };
        if self.aborting {
            return;
        }
        // Once the renderer has failed closed, no later queued business action
        // may cross the boundary. An already admitted frame is handled by
        // transport teardown and is never replayed.
        self.pending.clear();
        self.pending.push_front(stop);
    }

    fn drive(&mut self, app: &mut TuiApp, host: &RuntimeHost) -> DispatchProgress {
        let mut result = DispatchProgress::default();
        loop {
            let completions_progressed = self.drain_completions(app, host);
            let pending_progress = self.pump_pending(app, host);
            result.progressed |= completions_progressed || pending_progress.progressed;
            if !completions_progressed && !pending_progress.progressed {
                return result;
            }
        }
    }

    fn drain_completions(&mut self, app: &mut TuiApp, host: &RuntimeHost) -> bool {
        let mut progressed = host.progress_nonblocking_outbound();
        while let Some(completion) = host.try_recv_outbound_completion() {
            progressed = true;
            let Some(action) = self.in_flight.remove(&completion.id) else {
                if !self.aborting {
                    app.transport_failed_before_delivery(format!(
                        "outbound completion {:?} had no renderer action owner",
                        completion.id
                    ));
                    self.capture_fail_closed_stop(app);
                }
                continue;
            };
            match completion.result {
                Ok(_) => app.action_succeeded(&action),
                Err(error) => {
                    let failure = classify_send_error(error);
                    match failure {
                        DispatchFailure::Recoverable(message) => {
                            app.action_failed(&action, &message);
                            if !matches!(&action, HostAction::SendPrivateRuntimeAction { .. }) {
                                app.transport_failed_before_delivery(message);
                            }
                        }
                        DispatchFailure::DeliveryIndeterminate(message) => {
                            app.action_failed(&action, &message);
                            app.transport_failed_indeterminate(message);
                        }
                    }
                }
            }
            self.capture_fail_closed_stop(app);
        }
        progressed
    }

    fn pump_pending(&mut self, app: &mut TuiApp, host: &RuntimeHost) -> DispatchProgress {
        let mut result = DispatchProgress::default();
        loop {
            self.capture_fail_closed_stop(app);
            let Some(action) = self.pending.front().cloned() else {
                return result;
            };
            match &action {
                HostAction::StopRuntime { reason } => {
                    self.pending.pop_front();
                    host.abort_nonblocking(format!(
                        "CrabCode renderer requested fail-closed runtime stop: {reason}"
                    ));
                    app.action_succeeded(&action);
                    self.pending.clear();
                    self.in_flight.clear();
                    self.aborting = true;
                    result.progressed = true;
                    return result;
                }
                HostAction::Initialize if !app.initialize_release_allowed() => {
                    if !self.in_flight.is_empty() || host.has_nonblocking_outbound_work() {
                        // Preserve FIFO until all earlier renderer-private
                        // setup responses have completed delivery.
                        return result;
                    }
                    self.pending.pop_front();
                    let error =
                        "initialize remained locked after all prior setup deliveries completed";
                    app.action_failed(&action, error);
                    app.transport_failed_before_delivery(error);
                    self.capture_fail_closed_stop(app);
                    result.progressed = true;
                    continue;
                }
                _ => {}
            }

            match submit_outbound_action(app, host, &action) {
                Ok(delivery_id) => {
                    self.pending.pop_front();
                    if self.in_flight.insert(delivery_id, action.clone()).is_some() {
                        app.transport_failed_indeterminate(format!(
                            "outbound delivery id {delivery_id:?} was reused after admission"
                        ));
                        self.capture_fail_closed_stop(app);
                    } else {
                        // Renderer-private setup dialogs may advance only
                        // after this dispatcher owns the unique delivery id.
                        // Queue admission is not a delivery ACK; correlated
                        // completion still owns every semantic transition.
                        app.action_admitted(&action);
                    }
                    result.progressed = true;
                }
                Err(error @ OutboundSubmitError::QueueFull { .. }) => {
                    if matches!(&action, HostAction::SendPrivateRuntimeAction { .. }) {
                        // A panel action has not crossed the process boundary.
                        // Drop only this correlated request and let the open
                        // panel surface a retry instead of pinning the global
                        // dispatcher behind a full writer queue.
                        self.pending.pop_front();
                        app.action_failed(&action, error);
                        result.progressed = true;
                        continue;
                    }
                    // Preserve the exact action at the FIFO head. Consuming a
                    // completion releases both frame and byte capacity and
                    // wakes this dispatcher for a retry.
                    return result;
                }
                Err(OutboundSubmitError::Send(error)) => {
                    self.pending.pop_front();
                    match classify_send_error(error) {
                        DispatchFailure::Recoverable(message) => {
                            app.action_failed(&action, message);
                        }
                        DispatchFailure::DeliveryIndeterminate(message) => {
                            app.action_failed(&action, &message);
                            app.transport_failed_indeterminate(message);
                        }
                    }
                    self.capture_fail_closed_stop(app);
                    result.progressed = true;
                }
                Err(OutboundSubmitError::DeliveryIdExhausted) => {
                    self.pending.pop_front();
                    let error =
                        "process-local outbound delivery id space was exhausted before admission";
                    app.action_failed(&action, error);
                    if !matches!(&action, HostAction::SendPrivateRuntimeAction { .. }) {
                        app.transport_failed_before_delivery(error);
                    }
                    self.capture_fail_closed_stop(app);
                    result.progressed = true;
                }
            }
        }
    }

    fn discard_pending_before_shutdown(&mut self, app: &mut TuiApp) {
        // The terminal is already restored and the user has chosen to leave.
        // Never admit a new business action during shutdown; accepted frames
        // are settled separately before end_session is sent.
        self.pending.clear();
        let _ = app.take_runtime_stop_action();
    }
}

fn submit_outbound_action(
    app: &mut TuiApp,
    host: &RuntimeHost,
    action: &HostAction,
) -> Result<OutboundDeliveryId, OutboundSubmitError> {
    match action {
        HostAction::Initialize => host.submit_initialize().inspect(|_delivery_id| {
            // Correlation is recorded at admission, not ACK observation: the
            // child may publish its initialize response before the renderer
            // drains the writer completion queue.
            app.record_control_request(
                runtime_host::INITIALIZE_REQUEST_ID.to_string(),
                OutboundPurpose::Initialize,
            );
        }),
        HostAction::SendUser { content, priority } => {
            host.submit_user_content(content.clone(), priority.as_deref())
        }
        HostAction::SendControl { request, purpose } => {
            host.submit_control(request.clone())
                .map(|(request_id, delivery_id)| {
                    app.record_control_request(request_id, purpose.clone());
                    delivery_id
                })
        }
        HostAction::SendPrivateRuntimeAction {
            request_id,
            action,
            purpose,
        } => host
            .submit_private_runtime_action(request_id, action.clone())
            .inspect(|_delivery_id| {
                app.record_private_runtime_action(request_id.clone(), purpose.clone());
            }),
        HostAction::RespondPermission {
            request_id,
            response,
        } => host.submit_permission_response(request_id, response.clone()),
        HostAction::RespondElicitation {
            request_id,
            response,
        } => host.submit_elicitation_response(request_id, response.clone()),
        HostAction::RespondStartupInteraction {
            request_id,
            subtype,
            response,
        } => host.submit_startup_interaction_response(request_id, subtype, response.clone()),
        HostAction::Interrupt => host.submit_interrupt().map(|(request_id, delivery_id)| {
            let purpose = app.interrupt_outbound_purpose();
            app.record_control_request(request_id, purpose);
            delivery_id
        }),
        HostAction::StopRuntime { .. } => {
            unreachable!("local lifecycle actions never enter the outbound submitter")
        }
    }
}

fn synchronize_setup_presentation(
    app: &mut TuiApp,
    cli_override: Option<bool>,
    applied: &mut Option<bool>,
) {
    let Some(config_verbose) = app.setup_config_verbose() else {
        return;
    };
    let effective = effective_presentation_verbose(cli_override, config_verbose);
    if *applied != Some(effective) {
        app.set_presentation_verbose(effective);
        *applied = Some(effective);
    }
}

fn drain_after_shutdown(app: &mut TuiApp, host: &RuntimeHost) {
    while let Ok(runtime_event) = host.try_recv_event() {
        // The child is already terminal. Actions cannot be delivered now, but
        // ingesting every retained tail frame keeps the projection and recent
        // diagnostic window current through the matched end_session response.
        let _undeliverable_actions = app.handle_runtime_event(runtime_event);
    }
    while let Ok(frame) = host.try_recv_stderr() {
        app.push_stderr(&frame.bytes, frame.truncated);
    }
}

fn settle_outbound_after_terminal(
    app: &mut TuiApp,
    host: &RuntimeHost,
    dispatcher: &mut ActionDispatcher,
    event_runtime: &tokio::runtime::Runtime,
) {
    loop {
        let _progress = dispatcher.drive(app, host);
        if dispatcher.in_flight.is_empty() && !host.has_nonblocking_outbound_work() {
            return;
        }
        let Some(deadline) = host.next_outbound_deadline() else {
            app.transport_failed_before_delivery(
                "outbound work remained during shutdown without an authoritative deadline",
            );
            host.abort_nonblocking(
                "outbound shutdown settlement lost its authoritative deadline".to_string(),
            );
            return;
        };
        let outbound_ready = host.outbound_notifier();
        event_runtime.block_on(async {
            tokio::select! {
                _ = outbound_ready.notified() => {}
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
            }
        });
    }
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn publish_test_only_resume_cutover_ready() -> io::Result<()> {
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_RESUME_CUTOVER_READY_FILE";
    let Some(path) = std::env::var_os(READY_FILE_ENV) else {
        return Ok(());
    };
    std::fs::write(&path, b"ready").map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to publish test-only resume cutover readiness at {}: {error}",
                std::path::Path::new(&path).display()
            ),
        )
    })
}

#[cfg(feature = "terminal-lifecycle-tests")]
#[allow(unsafe_code)]
fn inject_test_only_fault_after_raw() -> anyhow::Result<()> {
    const FAULT_ENV: &str = "CRABCODE_TUI_TEST_ONLY_FAULT_AFTER_RAW";
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_FAULT_READY_FILE";
    if let Some(path) = std::env::var_os(READY_FILE_ENV) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::path::Path::new(&path).is_file() {
            if Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "test-only fault readiness file was not created: {}",
                    std::path::Path::new(&path).display()
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    match std::env::var(FAULT_ENV).ok().as_deref() {
        None => Ok(()),
        Some("error") => Err(anyhow::anyhow!(
            "test-only injected error after terminal ownership"
        )),
        Some("panic") => panic!("test-only injected panic after terminal ownership"),
        #[cfg(unix)]
        Some("sigbus") => {
            // SAFETY: the terminal-only production handler restores the tty and
            // re-raises this exact signal with its default disposition.
            unsafe {
                libc::raise(libc::SIGBUS);
            }
            Err(anyhow::anyhow!("test-only SIGBUS handler returned"))
        }
        #[cfg(unix)]
        Some("sigsegv") => {
            // SAFETY: this exercises the production fatal-memory-signal path in
            // a subprocess owned by the PTY lifecycle test.
            unsafe {
                libc::raise(libc::SIGSEGV);
            }
            Err(anyhow::anyhow!("test-only SIGSEGV handler returned"))
        }
        Some("block") => loop {
            // Exercise the fixed upstream signal contract with the UI thread
            // unable to poll its graceful-quit notification. The dedicated
            // signal thread must still restore and terminate on signal two.
            std::thread::park_timeout(Duration::from_secs(60));
        },
        Some(other) => Err(anyhow::anyhow!(
            "invalid {FAULT_ENV} value `{other}`; expected error, panic, sigbus, sigsegv, or block"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_never_replaces_the_primary_runtime_error() {
        let error = preserve_primary_shutdown_failure(
            anyhow::anyhow!("primary renderer failure"),
            Err(ShutdownError::SupervisorClosed),
        );
        assert_eq!(error.to_string(), "primary renderer failure");
        let chain = format!("{error:#}");
        assert!(chain.starts_with("primary renderer failure"));
        assert!(chain.contains("SDK runtime supervisor is closed"));
    }

    #[test]
    fn presentation_verbose_uses_explicit_override_or_authoritative_config() {
        assert!(effective_presentation_verbose(Some(true), false));
        assert!(!effective_presentation_verbose(Some(false), true));
        assert!(effective_presentation_verbose(None, true));
        assert!(!effective_presentation_verbose(None, false));
    }

    #[test]
    fn localized_renderer_lifecycle_statuses_preserve_dynamic_evidence() {
        let error = "签名验证失败 / signature evidence";
        assert_eq!(
            pending_relaunch_deferred(UiLanguage::ZhCn, "1.2.3", error),
            "CrabCode 1.2.3 已安装 · 自动重启已推迟：签名验证失败 / signature evidence"
        );
        assert_eq!(
            pending_relaunch_deferred(UiLanguage::EnUs, "1.2.3", error),
            "CrabCode 1.2.3 is installed · automatic restart deferred: 签名验证失败 / signature evidence"
        );
        assert_eq!(
            pending_relaunch_restarting(UiLanguage::ZhCn, "1.2.3"),
            "CrabCode 1.2.3 已安装 · 正在重启当前空闲会话"
        );
        assert_eq!(
            pending_relaunch_restarting(UiLanguage::EnUs, "1.2.3"),
            "CrabCode 1.2.3 is installed · restarting this idle session"
        );
        assert_eq!(
            terminal_terminate_status(UiLanguage::ZhCn, 15),
            "收到终止信号 15，正在恢复终端"
        );
        assert_eq!(
            terminal_terminate_status(UiLanguage::EnUs, 15),
            "Received termination signal 15; restoring the terminal"
        );
        assert_eq!(
            terminal_disconnected_status(UiLanguage::ZhCn),
            "终端已断开；正在关闭直连运行环境"
        );
        assert_eq!(
            terminal_disconnected_status(UiLanguage::EnUs),
            "Terminal disconnected; closing the direct runtime"
        );
    }

    #[test]
    fn writer_failure_event_returns_original_error() {
        let error = writer_event_sequence(terminal_writer::WriterEvent::Failed(
            std::io::Error::other("injected writer failure"),
        ))
        .expect_err("writer failure must terminate the event loop");

        assert_eq!(error.to_string(), "injected writer failure");
    }

    #[test]
    fn parked_terminal_cutover_failure_matrix_releases_input_only_on_full_success() {
        let releases = std::cell::Cell::new(0_u8);
        complete_parked_terminal_cutover(
            [
                (Ok(()), "terminal transition"),
                (Ok(()), "input generation cleanup"),
                (Ok(()), "control route restoration"),
            ],
            || releases.set(releases.get().saturating_add(1)),
        )
        .expect("complete cutover");
        assert_eq!(releases.get(), 1);

        let primary = complete_parked_terminal_cutover(
            [
                (
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "injected transition failure",
                    )),
                    "terminal transition",
                ),
                (Ok(()), "input generation cleanup"),
            ],
            || releases.set(releases.get().saturating_add(1)),
        )
        .expect_err("a failed transition keeps the reader parked");
        assert_eq!(primary.kind(), io::ErrorKind::InvalidData);
        assert!(primary.to_string().contains("injected transition failure"));
        assert_eq!(releases.get(), 1);

        let combined = complete_parked_terminal_cutover(
            [
                (
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected writer failure",
                    )),
                    "writer drain",
                ),
                (
                    Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "injected control failure",
                    )),
                    "control route restoration",
                ),
                (
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected tty flush failure",
                    )),
                    "tty generation cleanup",
                ),
            ],
            || releases.set(releases.get().saturating_add(1)),
        )
        .expect_err("all cutover failures must remain observable");
        assert_eq!(
            combined.kind(),
            io::ErrorKind::BrokenPipe,
            "later cleanup errors must not replace the primary failure kind"
        );
        for message in [
            "injected writer failure",
            "injected control failure",
            "injected tty flush failure",
        ] {
            assert!(combined.to_string().contains(message));
        }
        assert_eq!(
            releases.get(),
            1,
            "no failed cutover may release the sole terminal reader"
        );
    }

    #[test]
    fn presenter_coalesces_until_ack() {
        let mut presenter = Presenter::new();
        let mut draws = 0;

        presenter.request(false);
        assert!(presenter.try_present(0, |_| draws += 1, || 1));
        assert_eq!(presenter.in_flight_target, Some(1));
        for _ in 0..5 {
            presenter.request(false);
            assert!(!presenter.try_present(1, |_| draws += 1, || 2));
        }
        assert_eq!(draws, 1);
        assert!(presenter.dirty);

        presenter.acknowledge(1);
        assert!(presenter.try_present(1, |_| draws += 1, || 2));
        assert_eq!(draws, 2);
        assert_eq!(presenter.in_flight_target, Some(2));
    }

    #[test]
    fn presenter_no_output_does_not_wedge() {
        let mut presenter = Presenter::new();
        presenter.request(false);

        assert!(presenter.try_present(4, |_| {}, || 4));
        assert_eq!(presenter.in_flight_target, None);
        assert!(!presenter.dirty);

        presenter.request(false);
        assert!(presenter.try_present(4, |_| {}, || 5));
        assert_eq!(presenter.in_flight_target, Some(5));
    }

    #[test]
    fn presenter_keeps_forced_repaint_sticky() {
        let mut presenter = Presenter {
            in_flight_target: Some(8),
            ..Presenter::new()
        };
        presenter.request(false);
        presenter.request(true);
        let mut forced = false;

        presenter.acknowledge(8);
        assert!(presenter.try_present(8, |force| forced = force, || 9));
        assert!(forced);
        assert!(!presenter.force_full_repaint);
    }

    #[test]
    fn presenter_immediate_ack_before_request_is_not_lost() {
        let mut presenter = Presenter {
            in_flight_target: Some(3),
            ..Presenter::new()
        };
        presenter.acknowledge(3);
        presenter.request(false);

        assert!(presenter.try_present(3, |_| {}, || 4));
        assert_eq!(presenter.in_flight_target, Some(4));
    }

    #[test]
    fn presenter_later_ack_clears_target() {
        let mut presenter = Presenter {
            in_flight_target: Some(3),
            ..Presenter::new()
        };

        presenter.acknowledge(4);

        assert_eq!(presenter.in_flight_target, None);
    }

    #[test]
    fn presenter_waits_for_last_payload_in_turn() {
        let mut presenter = Presenter::new();
        presenter.request(false);
        assert!(presenter.try_present(10, |_| {}, || 13));
        presenter.request(false);

        presenter.acknowledge(11);
        assert!(!presenter.try_present(13, |_| panic!("target not acknowledged"), || 14));
        presenter.acknowledge(13);
        assert!(presenter.try_present(13, |_| {}, || 14));
        assert_eq!(presenter.in_flight_target, Some(14));
    }

    #[test]
    fn presenter_throttle_schedules_one_earliest_deferred_draw() {
        let start = Instant::now();
        let mut presenter = Presenter {
            last_draw_at: start,
            ..Presenter::new()
        };
        let interval = Duration::from_millis(16);

        assert!(!presenter.request_throttled(start, interval));
        assert_eq!(presenter.draw_scheduled_at, Some(start + interval));
        assert!(!presenter.request_throttled(start + Duration::from_millis(4), interval));
        assert_eq!(presenter.draw_scheduled_at, Some(start + interval));

        presenter.draw_scheduled_at = None;
        assert!(presenter.request_throttled(start + interval, interval));
        assert!(presenter.dirty);
    }

    #[test]
    fn presenter_generation_reset_discards_only_old_writer_ack_target() {
        let mut presenter = Presenter::new();
        presenter.request(false);
        assert!(presenter.try_present(4, |_| {}, || 5));
        presenter.request(false);
        assert!(presenter.in_flight_target.is_some());

        presenter.reset_after_terminal_generation_change();
        assert_eq!(presenter.in_flight_target, None);
        assert_eq!(presenter.draw_scheduled_at, None);
        assert!(presenter.dirty);
        assert!(presenter.force_full_repaint);
    }

    #[test]
    fn suspend_retry_gate_blocks_until_deadline() {
        let now = Instant::now();
        let mut retry_after = None;
        let mut wait_reported = false;

        assert!(defer_suspend_retry(
            &mut retry_after,
            &mut wait_reported,
            now
        ));
        assert!(!suspend_retry_ready(retry_after, now));
        assert_eq!(retry_after, Some(now + SUSPEND_RETRY_DELAY));
        assert!(suspend_retry_ready(retry_after, now + SUSPEND_RETRY_DELAY));
        assert!(wait_reported);

        // Mirrors the timer arm: expiry opens the gate for the next loop top.
        retry_after = None;
        assert!(suspend_retry_ready(retry_after, now));
        assert!(!defer_suspend_retry(
            &mut retry_after,
            &mut wait_reported,
            now
        ));
        assert_eq!(retry_after, Some(now + SUSPEND_RETRY_DELAY));
        assert!(!suspend_retry_ready(retry_after, now));
    }

    #[test]
    fn suspend_timeout_requeues_request() {
        let mut pending = None;

        requeue_after_suspend_timeout(&mut pending, "request");

        assert_eq!(pending, Some("request"));
    }

    #[test]
    fn external_editor_handoff_preserves_terminal_writer_generation() {
        let source = include_str!("external_editor.rs");
        let start = source
            .find("impl ChildTerminalHandoff for LiveChildTerminalHandoff<'_> {")
            .expect("production child-handoff adapter");
        let end = source[start..]
            .find("\n/// Suspend the native renderer")
            .map(|offset| start + offset)
            .expect("end of production child-handoff adapter");
        let handoff = &source[start..end];

        let park = handoff
            .find("self.terminal.park_control_writer_for_child()")
            .expect("control route closes before the stable writer drain");
        let drain = handoff
            .find("self.terminal.wait_writer_drained")
            .expect("writer drain after closing standalone controls");
        assert!(
            park < drain,
            "child handoff must close new control enqueues before choosing its drain frontier"
        );
        assert!(handoff.contains("self.terminal.release_tty_for_child()"));
        assert!(handoff.contains("self.terminal.reacquire_tty_after_child()"));
        assert!(handoff.contains("self.terminal.restore_control_writer_after_child()"));
        assert!(
            !handoff.contains("terminal.suspend()") && !handoff.contains("terminal.resume()"),
            "a child handoff must retain the live terminal and ordered writer generation"
        );
    }

    #[test]
    fn external_editor_handoff_timeout_requeues_once_and_deduplicates_feedback() {
        let mut app = TuiApp::new(&serde_json::json!({}), InitialSessionRequest::New, None);
        let mut presenter = Presenter::new();
        let mut pending = None;

        defer_external_handoff(
            &mut app,
            &mut presenter,
            &mut pending,
            PendingExternalHandoff::edit_prompt("draft".to_string()),
        );
        let mut request = pending.take().expect("request must be requeued");
        assert!(matches!(
            request.request,
            ExternalHandoffRequest::EditPrompt {
                ref original_text
            } if original_text == "draft"
        ));
        assert!(request.wait_reported);
        assert!(!suspend_retry_ready(request.retry_after, Instant::now()));
        let expected_notice = external_editor_handoff_wait(UiLanguage::ZhCn);
        assert_eq!(app.status, expected_notice);
        assert_eq!(
            app.renderer_notices()
                .map(|notice| notice.text(app.ui_language()))
                .collect::<Vec<_>>(),
            [expected_notice]
        );
        assert!(matches!(
            app.renderer_notices()
                .next()
                .map(|notice| notice.placement()),
            Some(tui_app::TuiRendererNoticePlacement::AfterRawSequence(_))
        ));
        assert!(presenter.dirty);

        request.retry_after = None;
        presenter.dirty = false;
        defer_external_handoff(&mut app, &mut presenter, &mut pending, request);
        assert_eq!(
            app.renderer_notices()
                .map(|notice| notice.text(app.ui_language()))
                .collect::<Vec<_>>(),
            [expected_notice],
            "the same pending handoff reports its wait only once"
        );
        assert!(
            !presenter.dirty,
            "a duplicate wait report cannot create a feedback-frame retry loop"
        );

        let canonical_path = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .expect("canonical test workspace");
        let mut file_pending = None;
        defer_external_handoff(
            &mut app,
            &mut presenter,
            &mut file_pending,
            PendingExternalHandoff::open_file(canonical_path.clone(), Some(17)),
        );
        let file_request = file_pending.expect("file request must be requeued");
        assert!(matches!(
            file_request.request,
            ExternalHandoffRequest::OpenFile {
                canonical_path: ref observed,
                line: Some(17),
            } if observed == &canonical_path
        ));
    }
}
