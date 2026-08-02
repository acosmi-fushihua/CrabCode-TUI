//! Renderer-local external prompt editor.
//!
//! The terminal owner suspends the native renderer before calling
//! [`edit_prompt`]. This module owns only the temporary prompt file and the
//! historical CrabCode editor-selection/edit semantics; it has no backend or
//! wire-protocol surface.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

const PROMPT_FILE_PREFIX: &str = "crabcode-prompt-";
const PROMPT_FILE_SUFFIX: &str = ".md";
const CHILD_INPUT_PARK_TIMEOUT: Duration = Duration::from_millis(500);
const CHILD_WRITER_DRAIN_TIMEOUT: Duration = Duration::from_millis(750);
const POSIX_EDITOR_CANDIDATES: [&str; 3] = ["code", "vi", "nano"];
const GUI_EDITORS: [&str; 9] = [
    "code",
    "cursor",
    "windsurf",
    "codium",
    "subl",
    "atom",
    "gedit",
    "notepad++",
    "notepad",
];
const VSCODE_FAMILY: [&str; 4] = ["code", "cursor", "windsurf", "codium"];

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExternalEditor {
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptEditOutcome {
    Content(String),
    NoChange,
    Failed(String),
}

pub(crate) enum PromptEditPreparation {
    Ready(PreparedPromptEdit),
    Finished(PromptEditOutcome),
}

pub(crate) struct PreparedPromptEdit {
    editor: ExternalEditor,
    file: PromptEditorFile,
}

pub(crate) enum FileOpenPreparation {
    Ready(PreparedFileOpen),
    Unavailable,
}

#[derive(Debug)]
pub(crate) struct ChildHandoffError {
    error: io::Error,
    retryable_before_child: bool,
}

impl ChildHandoffError {
    fn retryable_before_child(error: io::Error) -> Self {
        Self {
            error,
            retryable_before_child: true,
        }
    }

    fn fatal(error: io::Error) -> Self {
        Self {
            error,
            retryable_before_child: false,
        }
    }

    pub(crate) const fn is_retryable_before_child(&self) -> bool {
        self.retryable_before_child
    }

    #[cfg(test)]
    fn kind(&self) -> io::ErrorKind {
        self.error.kind()
    }
}

impl std::fmt::Display for ChildHandoffError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ChildHandoffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(crate) struct PreparedFileOpen {
    editor: ExternalEditor,
    path: PathBuf,
    line: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
struct FileEditorInvocation {
    program: String,
    args: Vec<OsString>,
    gui: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorExit {
    Success,
    Code(i32),
    UnreportedFailure,
}

struct PromptEditorFile {
    path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum PromptReadback {
    Content(String),
    TooLarge,
}

/// Renderer-local operations needed by the fixed child-TTY handoff.
///
/// The fixed upstream implementation performs these operations directly in
/// `app/event_loop.rs::suspend_for_child`. CrabCode keeps the same ordering
/// behind this narrow adapter because its terminal session additionally owns a
/// process-wide standalone-control route. That route must be closed before the
/// writer drain frontier is chosen and restored before the reader is unparked;
/// neither operation observes or changes backend/runtime protocol state.
trait ChildTerminalHandoff {
    fn park_input(&mut self, timeout: Duration) -> bool;
    fn unpark_input(&mut self);
    fn park_control_writer(&mut self) -> io::Result<()>;
    fn wait_writer_drained(
        &mut self,
        timeout: Duration,
    ) -> io::Result<crate::terminal_writer::WriterDrain>;
    fn probe_cursor_before_child(&mut self) -> io::Result<Option<(u16, u16)>>;
    fn release_tty_for_child(&mut self) -> io::Result<()>;
    fn reacquire_tty_after_child(&mut self) -> io::Result<()>;
    fn discard_child_terminal_replies(&mut self);
    fn probe_moved_cursor_after_child(
        &mut self,
        before: Option<(u16, u16)>,
    ) -> io::Result<Option<(u16, u16)>>;
    fn drain_pre_handoff_input(&mut self) -> io::Result<()>;
    fn restore_control_writer(&mut self) -> io::Result<()>;
}

struct LiveChildTerminalHandoff<'a> {
    terminal: &'a mut crate::terminal::TerminalSession,
    input: &'a mut crate::terminal_input::TerminalEventSource,
}

impl ChildTerminalHandoff for LiveChildTerminalHandoff<'_> {
    fn park_input(&mut self, timeout: Duration) -> bool {
        self.input.park_reader(timeout)
    }

    fn unpark_input(&mut self) {
        self.input.unpark_reader();
    }

    fn park_control_writer(&mut self) -> io::Result<()> {
        self.terminal.park_control_writer_for_child()
    }

    fn wait_writer_drained(
        &mut self,
        timeout: Duration,
    ) -> io::Result<crate::terminal_writer::WriterDrain> {
        self.terminal.wait_writer_drained(timeout)
    }

    fn probe_cursor_before_child(&mut self) -> io::Result<Option<(u16, u16)>> {
        self.terminal.probe_cursor_before_child()
    }

    fn release_tty_for_child(&mut self) -> io::Result<()> {
        self.terminal.release_tty_for_child()
    }

    fn reacquire_tty_after_child(&mut self) -> io::Result<()> {
        self.terminal.reacquire_tty_after_child()
    }

    fn discard_child_terminal_replies(&mut self) {
        // The input reader remains parked, so this thread is the sole
        // crossterm reader. Match the fixed upstream zero-timeout drain.
        while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            let _ = crossterm::event::read();
        }
    }

    fn probe_moved_cursor_after_child(
        &mut self,
        before: Option<(u16, u16)>,
    ) -> io::Result<Option<(u16, u16)>> {
        self.terminal.probe_moved_cursor_after_child(before)
    }

    fn drain_pre_handoff_input(&mut self) -> io::Result<()> {
        let receiver = self.input.receiver_mut()?;
        while receiver.try_recv().is_ok() {}
        Ok(())
    }

    fn restore_control_writer(&mut self) -> io::Result<()> {
        self.terminal.restore_control_writer_after_child()
    }
}

/// Suspend the native renderer, let one blocking child own the TTY, and
/// reacquire the same terminal/writer generation.
///
/// This is the fixed upstream `park input -> drain writer -> probe -> release
/// tty -> child -> reacquire -> discard replies -> probe -> drain input ->
/// unpark` lifecycle. CrabCode's only additional steps close/reopen its
/// renderer-local standalone-control route around the stable drain frontier.
pub(crate) fn suspend_for_child(
    terminal: &mut crate::terminal::TerminalSession,
    input: &mut crate::terminal_input::TerminalEventSource,
    run_child: impl FnOnce(),
) -> Result<Option<(u16, u16)>, ChildHandoffError> {
    suspend_for_child_with(&mut LiveChildTerminalHandoff { terminal, input }, run_child)
}

fn suspend_for_child_with(
    handoff: &mut impl ChildTerminalHandoff,
    run_child: impl FnOnce(),
) -> Result<Option<(u16, u16)>, ChildHandoffError> {
    if !handoff.park_input(CHILD_INPUT_PARK_TIMEOUT) {
        handoff.unpark_input();
        return Err(ChildHandoffError::retryable_before_child(io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal input reader did not park before suspend",
        )));
    }
    if let Err(error) = handoff.park_control_writer() {
        return Err(ChildHandoffError::fatal(error));
    }
    match handoff.wait_writer_drained(CHILD_WRITER_DRAIN_TIMEOUT) {
        Ok(crate::terminal_writer::WriterDrain::Drained) => {}
        Ok(crate::terminal_writer::WriterDrain::TimedOut) => {
            let restore_result = handoff.restore_control_writer();
            if let Err(restore_error) = restore_result {
                return Err(ChildHandoffError::fatal(io::Error::new(
                    restore_error.kind(),
                    format!(
                        "terminal writer did not drain before suspend; restoring the terminal \
                         control route also failed: {restore_error}"
                    ),
                )));
            }
            // This is the sole retryable failure: no terminal ownership moved
            // and the exact control route is live again.
            handoff.unpark_input();
            return Err(ChildHandoffError::retryable_before_child(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal writer did not drain before suspend",
            )));
        }
        Err(error) => return Err(ChildHandoffError::fatal(error)),
    }

    // Minimal mode's startup already established whether cursor position
    // reports are usable. The reader is parked, so the probe reply is ours.
    let before = match handoff.probe_cursor_before_child() {
        Ok(before) => before,
        Err(error) => return Err(ChildHandoffError::fatal(error)),
    };
    if let Err(error) = handoff.release_tty_for_child() {
        return Err(ChildHandoffError::fatal(error));
    }
    // The process panic hook deliberately reports while the blocking child
    // owns the cooked main screen. Catch only to complete this same handoff
    // generation before resuming the original unwind; otherwise
    // TerminalSession::Drop could clear that diagnostic from a stale
    // ChildHandoff phase.
    let child_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_child));
    let cleanup_result = (|| {
        let moved_cursor = match handoff.reacquire_tty_after_child() {
            Ok(()) => {
                handoff.discard_child_terminal_replies();
                handoff.probe_moved_cursor_after_child(before)
            }
            Err(error) => Err(error),
        }
        .map_err(ChildHandoffError::fatal)?;

        // Only the pre-park race can have reached the input channel. Restore
        // the exact live control-writer generation before returning input
        // ownership.
        handoff
            .drain_pre_handoff_input()
            .map_err(ChildHandoffError::fatal)?;
        handoff
            .restore_control_writer()
            .map_err(ChildHandoffError::fatal)?;
        handoff.unpark_input();
        Ok(moved_cursor)
    })();
    match child_result {
        Ok(()) => cleanup_result,
        Err(payload) => {
            // The original panic is authoritative even if a later cleanup
            // step failed: resuming it preserves the already-emitted
            // diagnostic and lets TerminalEventSource/TerminalSession Drop
            // finish fail-closed shutdown without inventing a second panic.
            let _cleanup_result = cleanup_result;
            std::panic::resume_unwind(payload);
        }
    }
}

impl PromptEditorFile {
    fn create(text: &str) -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "{PROMPT_FILE_PREFIX}{}{PROMPT_FILE_SUFFIX}",
            uuid::Uuid::new_v4()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let owner = Self { path };
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        Ok(owner)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read_utf8_compat(&self, max_bytes: usize) -> io::Result<PromptReadback> {
        let file = File::open(&self.path)?;
        if file.metadata()?.len() > max_bytes as u64 {
            return Ok(PromptReadback::TooLarge);
        }
        let mut bytes = Vec::new();
        // The metadata check is only a fast path: an editor can still replace
        // or grow the file between metadata and read. The max+1 reader bound
        // preserves the renderer's existing composer limit without first
        // allocating an attacker-sized temporary file.
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Ok(PromptReadback::TooLarge);
        }
        // Node's readFileSync(path, {encoding: "utf-8"}) decodes malformed
        // byte sequences with U+FFFD. Keep that fixed historical behavior.
        Ok(PromptReadback::Content(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    }
}

impl Drop for PromptEditorFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                %error,
                path = %self.path.display(),
                "external prompt editor temporary-file cleanup failed"
            );
        }
    }
}

static RESOLVED_EDITOR: OnceLock<Option<ExternalEditor>> = OnceLock::new();

fn resolve_external_editor() -> Option<ExternalEditor> {
    RESOLVED_EDITOR
        .get_or_init(|| {
            let visual = std::env::var("VISUAL").ok();
            let editor = std::env::var("EDITOR").ok();
            resolve_editor_with(
                visual.as_deref(),
                editor.as_deref(),
                cfg!(target_os = "windows"),
                |command| which::which(command).is_ok(),
            )
        })
        .clone()
}

fn resolve_editor_with(
    visual: Option<&str>,
    editor: Option<&str>,
    windows: bool,
    mut command_available: impl FnMut(&str) -> bool,
) -> Option<ExternalEditor> {
    if let Some(command) = visual.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(ExternalEditor {
            command: command.to_string(),
        });
    }
    if let Some(command) = editor.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(ExternalEditor {
            command: command.to_string(),
        });
    }
    if windows {
        return Some(ExternalEditor {
            command: "start /wait notepad".to_string(),
        });
    }
    POSIX_EDITOR_CANDIDATES
        .into_iter()
        .find(|candidate| command_available(candidate))
        .map(|command| ExternalEditor {
            command: command.to_string(),
        })
}

fn editor_command_line(editor: &ExternalEditor, file_path: &Path) -> String {
    let command = match editor.command.as_str() {
        "code" => "code -w",
        "subl" => "subl --wait",
        command => command,
    };
    format!("{command} \"{}\"", file_path.display())
}

#[cfg(target_os = "windows")]
fn launch_editor(editor: &ExternalEditor, file_path: &Path) -> io::Result<EditorExit> {
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let status = Command::new(shell)
        .args(["/D", "/S", "/C"])
        .arg(editor_command_line(editor, file_path))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(classify_exit_status(status))
}

#[cfg(not(target_os = "windows"))]
fn launch_editor(editor: &ExternalEditor, file_path: &Path) -> io::Result<EditorExit> {
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(editor_command_line(editor, file_path))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(classify_exit_status(status))
}

fn classify_exit_status(status: ExitStatus) -> EditorExit {
    if status.success() {
        EditorExit::Success
    } else if let Some(code) = status.code() {
        EditorExit::Code(code)
    } else {
        EditorExit::UnreportedFailure
    }
}

fn editor_display_name(editor: &str) -> String {
    if let Some(display) = supported_ide_display_name(editor) {
        return display.to_string();
    }
    let normalized = editor.trim().to_ascii_lowercase();
    if let Some(display) = known_editor_display_name(&normalized) {
        return display.to_string();
    }
    let command = editor.split(' ').next().unwrap_or_default();
    let basename = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    if let Some(display) = known_editor_display_name(&basename) {
        return display.to_string();
    }
    capitalize_first(&basename)
}

fn supported_ide_display_name(command: &str) -> Option<&'static str> {
    match command {
        "cursor" => Some("Cursor"),
        "windsurf" => Some("Windsurf"),
        "vscode" => Some("VS Code"),
        "intellij" => Some("IntelliJ IDEA"),
        "pycharm" => Some("PyCharm"),
        "webstorm" => Some("WebStorm"),
        "phpstorm" => Some("PhpStorm"),
        "rubymine" => Some("RubyMine"),
        "clion" => Some("CLion"),
        "goland" => Some("GoLand"),
        "rider" => Some("Rider"),
        "datagrip" => Some("DataGrip"),
        "appcode" => Some("AppCode"),
        "dataspell" => Some("DataSpell"),
        "aqua" => Some("Aqua"),
        "gateway" => Some("Gateway"),
        "fleet" => Some("Fleet"),
        "androidstudio" => Some("Android Studio"),
        _ => None,
    }
}

fn known_editor_display_name(command: &str) -> Option<&'static str> {
    match command {
        "code" => Some("VS Code"),
        "cursor" => Some("Cursor"),
        "windsurf" => Some("Windsurf"),
        "antigravity" => Some("Antigravity"),
        "vi" | "vim" => Some("Vim"),
        "nano" => Some("nano"),
        "notepad" | "start /wait notepad" => Some("Notepad"),
        "emacs" => Some("Emacs"),
        "subl" => Some("Sublime Text"),
        "atom" => Some("Atom"),
        _ => None,
    }
}

fn capitalize_first(text: &str) -> String {
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    first.to_uppercase().chain(characters).collect()
}

fn trim_one_editor_newline(mut content: String) -> String {
    if content.ends_with('\n') && !content.ends_with("\n\n") {
        content.pop();
    }
    content
}

fn finish_prompt_with_runner(
    file: PromptEditorFile,
    editor: &ExternalEditor,
    run_editor: impl FnOnce(&ExternalEditor, &Path) -> io::Result<EditorExit>,
) -> PromptEditOutcome {
    match run_editor(editor, file.path()) {
        Ok(EditorExit::Success) => {
            match file.read_utf8_compat(crate::tui_app::MAX_COMPOSER_TEXT_BYTES) {
                Ok(PromptReadback::Content(content)) => {
                    PromptEditOutcome::Content(trim_one_editor_newline(content))
                }
                Ok(PromptReadback::TooLarge) => PromptEditOutcome::Failed(format!(
                    "External editor result exceeded the {}-byte composer limit; the original draft \
                 was kept.",
                    crate::tui_app::MAX_COMPOSER_TEXT_BYTES
                )),
                // `editFileInEditor` historically caught read-back failures and
                // returned null without an error notification.
                Err(_) => PromptEditOutcome::NoChange,
            }
        }
        Ok(EditorExit::Code(code)) => PromptEditOutcome::Failed(format!(
            "{} exited with code {code}",
            editor_display_name(&editor.command)
        )),
        Ok(EditorExit::UnreportedFailure) | Err(_) => PromptEditOutcome::NoChange,
    }
}

#[cfg(test)]
fn edit_prompt_with_runner(
    current_prompt: &str,
    editor: &ExternalEditor,
    run_editor: impl FnOnce(&ExternalEditor, &Path) -> io::Result<EditorExit>,
) -> PromptEditOutcome {
    let file = match PromptEditorFile::create(current_prompt) {
        Ok(file) => file,
        Err(error) => {
            return PromptEditOutcome::Failed(format!("External editor failed: {error}"));
        }
    };
    finish_prompt_with_runner(file, editor, run_editor)
}

/// Resolve the editor and materialize the prompt immediately before the
/// terminal handoff.
///
/// Resolution happens while the renderer still owns the terminal, matching
/// the historical no-editor and preparation-failure paths. The editor itself
/// remains process-lifetime memoized like `getExternalEditor`.
pub(crate) fn prepare_prompt(current_prompt: &str) -> PromptEditPreparation {
    let Some(editor) = resolve_external_editor() else {
        return PromptEditPreparation::Finished(PromptEditOutcome::NoChange);
    };
    match PromptEditorFile::create(current_prompt) {
        Ok(file) => PromptEditPreparation::Ready(PreparedPromptEdit { editor, file }),
        Err(error) => PromptEditPreparation::Finished(PromptEditOutcome::Failed(format!(
            "External editor failed: {error}"
        ))),
    }
}

impl PreparedPromptEdit {
    /// Run only while the terminal owner has stopped input and suspended the
    /// renderer. Dropping this value cleans the prompt file on every path.
    pub(crate) fn run(self) -> PromptEditOutcome {
        let Self { editor, file } = self;
        finish_prompt_with_runner(file, &editor, launch_editor)
    }
}

/// Resolve the historical CrabCode file opener while the renderer still owns
/// the terminal. The selected catalog entry has already been canonicalized and
/// constrained to the workspace by `TuiApp`; this adapter does not broaden
/// filesystem authority.
pub(crate) fn prepare_file_open(path: PathBuf, line: Option<usize>) -> FileOpenPreparation {
    let Some(editor) = resolve_external_editor() else {
        return FileOpenPreparation::Unavailable;
    };
    FileOpenPreparation::Ready(PreparedFileOpen { editor, path, line })
}

fn classify_gui_editor(editor_program: &str) -> Option<&'static str> {
    let base = Path::new(editor_program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(editor_program);
    GUI_EDITORS
        .into_iter()
        .find(|candidate| base.contains(candidate))
}

fn terminal_editor_supports_plus_line(editor_program: &str) -> bool {
    static PLUS_LINE_EDITOR: OnceLock<regex::Regex> = OnceLock::new();
    let base = Path::new(editor_program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(editor_program);
    PLUS_LINE_EDITOR
        .get_or_init(|| {
            regex::Regex::new(r"\b(vi|vim|nvim|nano|emacs|pico|micro|helix|hx)\b")
                .expect("fixed historical editor regex")
        })
        .is_match(base)
}

fn file_editor_invocation(
    editor: &ExternalEditor,
    path: &Path,
    line: Option<usize>,
) -> FileEditorInvocation {
    // Historical `openFileInExternalEditor` deliberately uses `split(' ')`
    // rather than shell parsing. Preserve the user's actual binary and every
    // extra token exactly; POSIX launch below remains argv-only.
    let mut parts = editor.command.split(' ');
    let program = parts.next().unwrap_or(editor.command.as_str()).to_string();
    let mut args = parts.map(OsString::from).collect::<Vec<_>>();
    let gui_family = classify_gui_editor(&program);
    let line = line.filter(|line| *line > 0);
    if let Some(family) = gui_family {
        let rendered_path = path.to_string_lossy();
        if let Some(line) = line
            && VSCODE_FAMILY.contains(&family)
        {
            args.push(OsString::from("-g"));
            args.push(OsString::from(format!("{rendered_path}:{line}")));
        } else if let Some(line) = line
            && family == "subl"
        {
            args.push(OsString::from(format!("{rendered_path}:{line}")));
        } else {
            args.push(path.as_os_str().to_os_string());
        }
    } else {
        if let Some(line) = line
            && terminal_editor_supports_plus_line(&program)
        {
            args.push(OsString::from(format!("+{line}")));
        }
        args.push(path.as_os_str().to_os_string());
    }
    FileEditorInvocation {
        program,
        args,
        gui: gui_family.is_some(),
    }
}

#[cfg(not(target_os = "windows"))]
fn launch_file_editor(editor: &ExternalEditor, path: &Path, line: Option<usize>) {
    let invocation = file_editor_invocation(editor, path, line);
    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args);
    if invocation.gui {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_file_editor(&mut command);
        if let Err(error) = command.spawn() {
            // The historical Node spawn reports GUI ENOENT asynchronously
            // after returning `true`; retain that no-UI-error contract.
            tracing::warn!(%error, "external GUI file editor failed to spawn");
        }
    } else {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Err(error) = command.status() {
            tracing::warn!(%error, "external terminal file editor failed to spawn");
        }
    }
}

#[cfg(target_os = "windows")]
fn launch_file_editor(editor: &ExternalEditor, path: &Path, line: Option<usize>) {
    let invocation = file_editor_invocation(editor, path, line);
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let rendered_path = path.display();
    let line = line.filter(|line| *line > 0);
    let mut editor_parts = vec![invocation.program.clone()];
    editor_parts.extend(
        invocation
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned()),
    );
    let editor_command = editor_parts.join(" ");
    let command_line = if invocation.gui {
        let goto = match (classify_gui_editor(&invocation.program), line) {
            (Some(family), Some(line)) if VSCODE_FAMILY.contains(&family) => {
                format!("-g \"{rendered_path}:{line}\"")
            }
            (Some("subl"), Some(line)) => format!("\"{rendered_path}:{line}\""),
            _ => format!("\"{rendered_path}\""),
        };
        format!("{editor_command} {goto}")
    } else {
        let line_arg = line
            .filter(|_| terminal_editor_supports_plus_line(&invocation.program))
            .map_or_else(String::new, |line| format!("+{line} "));
        format!("{editor_command} {line_arg}\"{rendered_path}\"")
    };
    let mut command = Command::new(shell);
    command.args(["/D", "/S", "/C"]).arg(command_line);
    if invocation.gui {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach_file_editor(&mut command);
        if let Err(error) = command.spawn() {
            tracing::warn!(%error, "external GUI file editor failed to spawn");
        }
    } else {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Err(error) = command.status() {
            tracing::warn!(%error, "external terminal file editor failed to spawn");
        }
    }
}

#[cfg_attr(unix, allow(unsafe_code))]
fn detach_file_editor(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        // SAFETY: this post-fork hook performs only POSIX async-signal-safe
        // process-session operations, matching the renderer's existing
        // detached link-opener boundary.
        unsafe {
            command.pre_exec(|| {
                use nix::errno::Errno;
                use nix::unistd::{Pid, setpgid, setsid};

                match setsid() {
                    Ok(_) => Ok(()),
                    Err(Errno::EPERM) => setpgid(Pid::from_raw(0), Pid::from_raw(0))
                        .map_err(|error| io::Error::from_raw_os_error(error as i32)),
                    Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
                }
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        command.creation_flags(CREATE_NO_WINDOW);
    }
}

impl PreparedFileOpen {
    /// GUI file editors detach immediately and never take ownership of the
    /// terminal in CrabCode's historical direct adapter. Terminal editors
    /// inherit stdio and therefore require the outer suspend/reacquire cycle.
    pub(crate) fn requires_terminal_handoff(&self) -> bool {
        !file_editor_invocation(&self.editor, &self.path, self.line).gui
    }

    /// Launch a renderer-selected workspace file. Terminal editors block until
    /// exit; GUI families detach with ignored stdio exactly as in the fixed
    /// historical adapter. The outer owner supplies the safe suspend/repaint
    /// lifecycle only when [`Self::requires_terminal_handoff`] is true.
    pub(crate) fn run(self) {
        launch_file_editor(&self.editor, &self.path, self.line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn editor(command: &str) -> ExternalEditor {
        ExternalEditor {
            command: command.to_string(),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum FakeDrain {
        Drained,
        TimedOut,
        Failed(io::ErrorKind, &'static str),
    }

    struct FakeChildTerminalHandoff {
        trace: Rc<RefCell<Vec<&'static str>>>,
        input_parks: bool,
        park_control_error: Option<(io::ErrorKind, &'static str)>,
        drain: FakeDrain,
        probe_before_error: Option<(io::ErrorKind, &'static str)>,
        release_error: Option<(io::ErrorKind, &'static str)>,
        reacquire_error: Option<(io::ErrorKind, &'static str)>,
        probe_after_error: Option<(io::ErrorKind, &'static str)>,
        before: Option<(u16, u16)>,
        moved: Option<(u16, u16)>,
        drain_input_error: Option<(io::ErrorKind, &'static str)>,
        restore_control_error: Option<(io::ErrorKind, &'static str)>,
        park_timeout: Option<Duration>,
        drain_timeout: Option<Duration>,
    }

    impl Default for FakeChildTerminalHandoff {
        fn default() -> Self {
            Self {
                trace: Rc::new(RefCell::new(Vec::new())),
                input_parks: true,
                park_control_error: None,
                drain: FakeDrain::Drained,
                probe_before_error: None,
                release_error: None,
                reacquire_error: None,
                probe_after_error: None,
                before: None,
                moved: None,
                drain_input_error: None,
                restore_control_error: None,
                park_timeout: None,
                drain_timeout: None,
            }
        }
    }

    impl FakeChildTerminalHandoff {
        fn record(&self, event: &'static str) {
            self.trace.borrow_mut().push(event);
        }

        fn error(spec: (io::ErrorKind, &'static str)) -> io::Error {
            io::Error::new(spec.0, spec.1)
        }

        fn trace(&self) -> Vec<&'static str> {
            self.trace.borrow().clone()
        }
    }

    impl ChildTerminalHandoff for FakeChildTerminalHandoff {
        fn park_input(&mut self, timeout: Duration) -> bool {
            self.record("park-input");
            self.park_timeout = Some(timeout);
            self.input_parks
        }

        fn unpark_input(&mut self) {
            self.record("unpark-input");
        }

        fn park_control_writer(&mut self) -> io::Result<()> {
            self.record("park-control-writer");
            self.park_control_error
                .map_or(Ok(()), |spec| Err(Self::error(spec)))
        }

        fn wait_writer_drained(
            &mut self,
            timeout: Duration,
        ) -> io::Result<crate::terminal_writer::WriterDrain> {
            self.record("wait-writer-drained");
            self.drain_timeout = Some(timeout);
            match self.drain {
                FakeDrain::Drained => Ok(crate::terminal_writer::WriterDrain::Drained),
                FakeDrain::TimedOut => Ok(crate::terminal_writer::WriterDrain::TimedOut),
                FakeDrain::Failed(kind, message) => Err(io::Error::new(kind, message)),
            }
        }

        fn probe_cursor_before_child(&mut self) -> io::Result<Option<(u16, u16)>> {
            self.record("probe-before");
            self.probe_before_error
                .map_or(Ok(self.before), |spec| Err(Self::error(spec)))
        }

        fn release_tty_for_child(&mut self) -> io::Result<()> {
            self.record("release-tty");
            self.release_error
                .map_or(Ok(()), |spec| Err(Self::error(spec)))
        }

        fn reacquire_tty_after_child(&mut self) -> io::Result<()> {
            self.record("reacquire-tty");
            self.reacquire_error
                .map_or(Ok(()), |spec| Err(Self::error(spec)))
        }

        fn discard_child_terminal_replies(&mut self) {
            self.record("discard-child-replies");
        }

        fn probe_moved_cursor_after_child(
            &mut self,
            before: Option<(u16, u16)>,
        ) -> io::Result<Option<(u16, u16)>> {
            self.record("probe-after");
            assert_eq!(before, self.before);
            self.probe_after_error
                .map_or(Ok(self.moved), |spec| Err(Self::error(spec)))
        }

        fn drain_pre_handoff_input(&mut self) -> io::Result<()> {
            self.record("drain-pre-handoff-input");
            self.drain_input_error
                .map_or(Ok(()), |spec| Err(Self::error(spec)))
        }

        fn restore_control_writer(&mut self) -> io::Result<()> {
            self.record("restore-control-writer");
            self.restore_control_error
                .map_or(Ok(()), |spec| Err(Self::error(spec)))
        }
    }

    #[test]
    fn fixed_upstream_child_handoff_success_order_includes_only_control_route_adapter() {
        let mut handoff = FakeChildTerminalHandoff {
            before: Some((3, 5)),
            moved: Some((7, 11)),
            ..FakeChildTerminalHandoff::default()
        };
        let trace = Rc::clone(&handoff.trace);

        let moved = suspend_for_child_with(&mut handoff, || {
            trace.borrow_mut().push("child");
        })
        .expect("complete child handoff");

        assert_eq!(moved, Some((7, 11)));
        assert_eq!(handoff.park_timeout, Some(CHILD_INPUT_PARK_TIMEOUT));
        assert_eq!(handoff.drain_timeout, Some(CHILD_WRITER_DRAIN_TIMEOUT));
        assert_eq!(
            handoff.trace(),
            [
                "park-input",
                // CrabCode-only renderer-local adapter: close the standalone
                // control route before selecting the upstream drain frontier.
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
                "child",
                "reacquire-tty",
                "discard-child-replies",
                "probe-after",
                "drain-pre-handoff-input",
                // CrabCode-only inverse adapter, still before upstream unpark.
                "restore-control-writer",
                "unpark-input",
            ]
        );
    }

    #[test]
    fn child_panic_reacquires_generation_before_resuming_original_unwind() {
        let mut handoff = FakeChildTerminalHandoff {
            before: Some((3, 5)),
            moved: Some((7, 11)),
            ..FakeChildTerminalHandoff::default()
        };
        let trace = Rc::clone(&handoff.trace);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = suspend_for_child_with(&mut handoff, || {
                trace.borrow_mut().push("child");
                std::panic::panic_any("original child panic");
            });
        }))
        .expect_err("the original child panic must resume after cleanup");

        assert_eq!(panic.downcast_ref::<&str>(), Some(&"original child panic"));
        assert_eq!(
            handoff.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
                "child",
                "reacquire-tty",
                "discard-child-replies",
                "probe-after",
                "drain-pre-handoff-input",
                "restore-control-writer",
                "unpark-input",
            ],
            "cleanup must complete before the original unwind resumes"
        );

        let mut failed_cleanup = FakeChildTerminalHandoff {
            reacquire_error: Some((io::ErrorKind::BrokenPipe, "tty reacquire")),
            ..FakeChildTerminalHandoff::default()
        };
        let trace = Rc::clone(&failed_cleanup.trace);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = suspend_for_child_with(&mut failed_cleanup, || {
                trace.borrow_mut().push("child");
                std::panic::panic_any("original child panic");
            });
        }))
        .expect_err("cleanup failure must not replace the original child panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"original child panic"));
        assert_eq!(
            failed_cleanup.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
                "child",
                "reacquire-tty",
            ]
        );
    }

    #[test]
    fn fixed_upstream_child_handoff_failure_matrix_retries_only_before_child_and_fails_closed() {
        let mut input_timeout = FakeChildTerminalHandoff {
            input_parks: false,
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut input_timeout, || panic!("child must not run"))
            .expect_err("input park timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.is_retryable_before_child());
        assert_eq!(input_timeout.trace(), ["park-input", "unpark-input"]);

        let mut control_failure = FakeChildTerminalHandoff {
            park_control_error: Some((io::ErrorKind::TimedOut, "control close")),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut control_failure, || panic!("child must not run"))
            .expect_err("control close failure");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            !error.is_retryable_before_child(),
            "an error kind alone cannot make a failed control-route transition retryable"
        );
        assert_eq!(
            control_failure.trace(),
            ["park-input", "park-control-writer"]
        );

        let mut drain_timeout = FakeChildTerminalHandoff {
            drain: FakeDrain::TimedOut,
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut drain_timeout, || panic!("child must not run"))
            .expect_err("writer drain timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.is_retryable_before_child());
        assert_eq!(
            drain_timeout.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "restore-control-writer",
                "unpark-input",
            ]
        );

        let mut drain_timeout_and_restore_failure = FakeChildTerminalHandoff {
            drain: FakeDrain::TimedOut,
            restore_control_error: Some((io::ErrorKind::NotConnected, "control restore")),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut drain_timeout_and_restore_failure, || {
            panic!("child must not run")
        })
        .expect_err("writer timeout with failed ownership restoration is fatal");
        assert_eq!(
            error.kind(),
            io::ErrorKind::NotConnected,
            "a cleanup failure must not masquerade as the sole safe retryable timeout"
        );
        assert!(!error.is_retryable_before_child());
        assert!(error.to_string().contains("did not drain"));
        assert!(error.to_string().contains("control restore"));
        assert_eq!(
            drain_timeout_and_restore_failure.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "restore-control-writer",
            ],
            "failed control restoration keeps the sole reader parked"
        );

        let mut failed_drain = FakeChildTerminalHandoff {
            drain: FakeDrain::Failed(io::ErrorKind::BrokenPipe, "writer failed"),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut failed_drain, || panic!("child must not run"))
            .expect_err("writer failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert!(error.to_string().contains("writer failed"));
        assert_eq!(
            failed_drain.trace(),
            ["park-input", "park-control-writer", "wait-writer-drained",],
            "a failed writer generation must never be resurrected as a control route"
        );

        let mut probe_before_failure = FakeChildTerminalHandoff {
            probe_before_error: Some((io::ErrorKind::BrokenPipe, "emergency output stop")),
            ..FakeChildTerminalHandoff::default()
        };
        let error =
            suspend_for_child_with(&mut probe_before_failure, || panic!("child must not run"))
                .expect_err("pre-child cursor probe failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert_eq!(
            probe_before_failure.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
            ]
        );

        let mut release_failure = FakeChildTerminalHandoff {
            release_error: Some((io::ErrorKind::BrokenPipe, "emergency output stop")),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut release_failure, || panic!("child must not run"))
            .expect_err("tty release failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert_eq!(
            release_failure.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
            ]
        );

        let mut reacquire_failure = FakeChildTerminalHandoff {
            reacquire_error: Some((io::ErrorKind::BrokenPipe, "tty reacquire")),
            ..FakeChildTerminalHandoff::default()
        };
        let reacquire_child_ran = Rc::new(RefCell::new(false));
        let reacquire_child_marker = Rc::clone(&reacquire_child_ran);
        let error = suspend_for_child_with(&mut reacquire_failure, || {
            *reacquire_child_marker.borrow_mut() = true;
        })
        .expect_err("tty reacquire failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert!(*reacquire_child_ran.borrow());
        assert_eq!(
            reacquire_failure.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
                "reacquire-tty",
            ],
            "failed reacquisition must skip every live-generation cleanup and keep input parked"
        );

        let mut probe_after_failure = FakeChildTerminalHandoff {
            probe_after_error: Some((io::ErrorKind::BrokenPipe, "emergency output stop")),
            ..FakeChildTerminalHandoff::default()
        };
        let probe_after_child_ran = Rc::new(RefCell::new(false));
        let probe_after_child_marker = Rc::clone(&probe_after_child_ran);
        let error = suspend_for_child_with(&mut probe_after_failure, || {
            *probe_after_child_marker.borrow_mut() = true;
        })
        .expect_err("post-child cursor probe failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert!(*probe_after_child_ran.borrow());
        assert_eq!(
            probe_after_failure.trace(),
            [
                "park-input",
                "park-control-writer",
                "wait-writer-drained",
                "probe-before",
                "release-tty",
                "reacquire-tty",
                "discard-child-replies",
                "probe-after",
            ]
        );

        let mut post_child_input_failure = FakeChildTerminalHandoff {
            drain_input_error: Some((io::ErrorKind::BrokenPipe, "input drain")),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut post_child_input_failure, || {})
            .expect_err("post-child input drain failure");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!error.is_retryable_before_child());
        assert!(
            post_child_input_failure.trace()[9..].is_empty(),
            "a failed input-generation drain must not resurrect the control route"
        );

        let mut post_child_control_failure = FakeChildTerminalHandoff {
            restore_control_error: Some((io::ErrorKind::NotConnected, "control restore")),
            ..FakeChildTerminalHandoff::default()
        };
        let error = suspend_for_child_with(&mut post_child_control_failure, || {})
            .expect_err("post-child control restore failure");
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert!(!error.is_retryable_before_child());
        assert!(error.to_string().contains("control restore"));
        assert_eq!(
            &post_child_control_failure.trace()[9..],
            ["restore-control-writer"]
        );
    }

    #[test]
    fn editor_resolution_is_visual_then_editor_then_platform_default() {
        assert_eq!(
            resolve_editor_with(Some("  code  "), Some("vi"), false, |_| false),
            Some(editor("code"))
        );
        assert_eq!(
            resolve_editor_with(Some(" \t"), Some(" subl "), false, |_| false),
            Some(editor("subl"))
        );
        assert_eq!(
            resolve_editor_with(None, None, true, |_| {
                panic!("Windows default must not probe PATH")
            }),
            Some(editor("start /wait notepad"))
        );

        let mut probes = Vec::new();
        assert_eq!(
            resolve_editor_with(None, None, false, |candidate| {
                probes.push(candidate.to_string());
                candidate == "vi"
            }),
            Some(editor("vi"))
        );
        assert_eq!(probes, ["code", "vi"]);
        assert_eq!(resolve_editor_with(None, None, false, |_| false), None);
    }

    #[test]
    fn only_fixed_historical_editor_overrides_add_wait_flags() {
        let path = Path::new("/tmp/crabcode-prompt.md");
        assert_eq!(
            editor_command_line(&editor("code"), path),
            "code -w \"/tmp/crabcode-prompt.md\""
        );
        assert_eq!(
            editor_command_line(&editor("subl"), path),
            "subl --wait \"/tmp/crabcode-prompt.md\""
        );
        assert_eq!(
            editor_command_line(&editor("/usr/bin/code"), path),
            "/usr/bin/code \"/tmp/crabcode-prompt.md\""
        );
        assert_eq!(
            editor_command_line(&editor("code --wait"), path),
            "code --wait \"/tmp/crabcode-prompt.md\""
        );
    }

    #[test]
    fn successful_edit_uses_utf8_and_trims_exactly_one_terminal_newline() {
        for (saved, expected) in [
            ("edited\n", "edited"),
            ("edited\n\n", "edited\n\n"),
            ("edited\r\n", "edited\r"),
            ("", ""),
        ] {
            let mut materialized = None;
            let outcome = edit_prompt_with_runner("original", &editor("vi"), |_, path| {
                assert_eq!(std::fs::read_to_string(path)?, "original");
                materialized = Some(path.to_path_buf());
                std::fs::write(path, saved)?;
                Ok(EditorExit::Success)
            });
            assert_eq!(outcome, PromptEditOutcome::Content(expected.to_string()));
            assert!(
                materialized.is_some_and(|path| !path.exists()),
                "temporary prompt file must be removed after read-back"
            );
        }

        let outcome = edit_prompt_with_runner("original", &editor("vi"), |_, path| {
            std::fs::write(path, [b'e', 0xff, b'd'])?;
            Ok(EditorExit::Success)
        });
        assert_eq!(
            outcome,
            PromptEditOutcome::Content("e\u{fffd}d".to_string())
        );
    }

    #[test]
    fn prompt_readback_is_bounded_before_utf8_compat_decode() {
        let file = PromptEditorFile::create("12345").expect("temporary prompt file");
        assert_eq!(
            file.read_utf8_compat(4).expect("bounded read"),
            PromptReadback::TooLarge
        );
        assert_eq!(
            file.read_utf8_compat(5).expect("exact-limit read"),
            PromptReadback::Content("12345".to_string())
        );
    }

    #[test]
    fn launch_failure_and_nonzero_exit_keep_the_draft_and_cleanup() {
        let mut failed_path = None;
        let launch_failure = edit_prompt_with_runner("original", &editor("vi"), |_, path| {
            failed_path = Some(path.to_path_buf());
            Err(io::Error::new(io::ErrorKind::NotFound, "missing editor"))
        });
        assert_eq!(launch_failure, PromptEditOutcome::NoChange);
        assert!(failed_path.is_some_and(|path| !path.exists()));

        let mut nonzero_path = None;
        let nonzero = edit_prompt_with_runner("original", &editor("code"), |_, path| {
            nonzero_path = Some(path.to_path_buf());
            Ok(EditorExit::Code(7))
        });
        assert_eq!(
            nonzero,
            PromptEditOutcome::Failed("VS Code exited with code 7".to_string())
        );
        assert!(nonzero_path.is_some_and(|path| !path.exists()));

        let read_failure = edit_prompt_with_runner("original", &editor("vi"), |_, path| {
            std::fs::remove_file(path)?;
            Ok(EditorExit::Success)
        });
        assert_eq!(
            read_failure,
            PromptEditOutcome::NoChange,
            "historical read-back failures silently keep the original draft"
        );
    }

    #[test]
    fn editor_display_name_matches_historical_exact_and_basename_fallbacks() {
        assert_eq!(editor_display_name("start /wait notepad"), "Notepad");
        assert_eq!(editor_display_name("vscode"), "VS Code");
        assert_eq!(editor_display_name("VSCODE"), "Vscode");
        assert_eq!(editor_display_name("intellij"), "IntelliJ IDEA");
        assert_eq!(editor_display_name("/usr/bin/vscode"), "Vscode");
        assert_eq!(editor_display_name("/usr/bin/code --wait"), "VS Code");
        assert_eq!(editor_display_name("/opt/bin/kak"), "Kak");
        assert_eq!(editor_display_name(""), "");
    }

    #[test]
    fn file_open_argv_preserves_historical_gui_and_terminal_line_syntax() {
        let file = Path::new("/tmp/a file.rs");
        assert_eq!(
            file_editor_invocation(&editor("code --wait"), file, Some(42)),
            FileEditorInvocation {
                program: "code".to_string(),
                args: vec![
                    OsString::from("--wait"),
                    OsString::from("-g"),
                    OsString::from("/tmp/a file.rs:42"),
                ],
                gui: true,
            }
        );
        assert_eq!(
            file_editor_invocation(&editor("/opt/subl"), file, Some(42)),
            FileEditorInvocation {
                program: "/opt/subl".to_string(),
                args: vec![OsString::from("/tmp/a file.rs:42")],
                gui: true,
            }
        );
        assert_eq!(
            file_editor_invocation(&editor("/usr/bin/nvim -f"), file, Some(42)),
            FileEditorInvocation {
                program: "/usr/bin/nvim".to_string(),
                args: vec![
                    OsString::from("-f"),
                    OsString::from("+42"),
                    file.as_os_str().to_os_string(),
                ],
                gui: false,
            }
        );
    }

    #[test]
    fn file_open_classification_uses_only_the_editor_basename() {
        let file = Path::new("/tmp/source.rs");
        assert_eq!(
            file_editor_invocation(&editor("/home/code/bin/kak"), file, Some(9)),
            FileEditorInvocation {
                program: "/home/code/bin/kak".to_string(),
                args: vec![file.as_os_str().to_os_string()],
                gui: false,
            },
            "a directory component named code must not classify kak as a GUI editor"
        );
        assert_eq!(
            file_editor_invocation(&editor("/home/vim/bin/kak"), file, Some(9)),
            FileEditorInvocation {
                program: "/home/vim/bin/kak".to_string(),
                args: vec![file.as_os_str().to_os_string()],
                gui: false,
            },
            "a directory component named vim must not add +N"
        );
        assert_eq!(
            file_editor_invocation(&editor("gedit"), file, Some(9)),
            FileEditorInvocation {
                program: "gedit".to_string(),
                args: vec![file.as_os_str().to_os_string()],
                gui: true,
            },
            "historical GUI families without goto-line support ignore the line"
        );
        assert_eq!(
            file_editor_invocation(&editor("notepad"), file, Some(9)),
            FileEditorInvocation {
                program: "notepad".to_string(),
                args: vec![file.as_os_str().to_os_string()],
                gui: true,
            },
            "notepad must never receive a synthetic +N filename"
        );
    }

    #[test]
    fn only_terminal_file_editors_require_terminal_handoff() {
        let path = PathBuf::from("/tmp/source.rs");
        let gui = PreparedFileOpen {
            editor: editor("code --wait"),
            path: path.clone(),
            line: Some(9),
        };
        let terminal = PreparedFileOpen {
            editor: editor("/usr/bin/nvim -f"),
            path,
            line: Some(9),
        };

        assert!(!gui.requires_terminal_handoff());
        assert!(terminal.requires_terminal_handoff());
    }
}
