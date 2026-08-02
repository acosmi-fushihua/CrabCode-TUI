//! Clipboard capture for the native terminal composer.
//!
//! Capture runs on a worker thread owned by [`crate::tui_app::TuiApp`].  This
//! module contains no session/backend behavior: it only reads one user-invoked
//! paste payload and prepares it through the same bounded image pipeline used
//! by dropped image paths.

use std::fs::DirBuilder;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;

use crate::composer_image::{LoadedComposerImage, load_composer_image};
use crate::terminal_capabilities::{
    MultiplexerKind, TerminalContext, TerminalName, terminal_context,
};

const MAX_CLIPBOARD_TEXT_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(0);

/// Renderer-local clipboard route.
///
/// Multi-leg delivery follows the fixed Rust TUI lifecycle. The concrete
/// native/SSH/tmux/terminator choices are the fixed historical CrabCode direct
/// TUI product behavior (`ink/termio/osc.ts`), including `SSH_CONNECTION`
/// gating and iTerm2's `tmux load-buffer` exception.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClipboardRoute {
    native: bool,
    tmux_buffer: bool,
    osc52: bool,
    osc52_tmux_passthrough: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardDelivery {
    Confirmed,
    Unverified,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Osc52Capability {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClipboardWriteLegs {
    native_ok: bool,
    tmux_ok: bool,
    osc52_ok: bool,
}

pub(crate) fn set_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_CLIPBOARD_TEXT_CAPTURE_BYTES {
        return Err(format!(
            "clipboard text exceeded the {MAX_CLIPBOARD_TEXT_CAPTURE_BYTES} byte limit"
        ));
    }

    let route = clipboard_route();
    let legs = write_clipboard_legs(text, route);
    match classify_delivery(legs, terminal_context()) {
        ClipboardDelivery::Confirmed | ClipboardDelivery::Unverified => Ok(()),
        ClipboardDelivery::Failed => Err(format!(
            "all enabled clipboard routes failed ({})",
            route_label(route)
        )),
    }
}

fn clipboard_route() -> &'static ClipboardRoute {
    static ROUTE: OnceLock<ClipboardRoute> = OnceLock::new();
    ROUTE.get_or_init(|| {
        let ssh_connection = std::env::var_os("SSH_CONNECTION").is_some();
        #[cfg(feature = "terminal-lifecycle-tests")]
        let suppress_native = {
            // The local macOS SIGTSTP regression must exercise the real
            // physical-probe cutover without mutating the developer's
            // clipboard. This test-only seam disables only the native
            // clipboard leg; OSC52 and terminal detection remain production
            // exact.
            ssh_connection
                || std::env::var_os("CRABCODE_TUI_TEST_ONLY_DISABLE_NATIVE_CLIPBOARD").is_some()
        };
        #[cfg(not(feature = "terminal-lifecycle-tests"))]
        let suppress_native = ssh_connection;
        resolve_clipboard_route(terminal_context(), suppress_native)
    })
}

fn resolve_clipboard_route(context: &TerminalContext, suppress_native: bool) -> ClipboardRoute {
    let tmux = context.multiplexer == MultiplexerKind::Tmux;
    ClipboardRoute {
        native: !suppress_native,
        tmux_buffer: tmux,
        osc52: true,
        osc52_tmux_passthrough: tmux,
    }
}

fn route_label(route: &ClipboardRoute) -> String {
    [
        (route.native, "native"),
        (route.tmux_buffer, "tmux"),
        (route.osc52, "osc52"),
    ]
    .into_iter()
    .filter_map(|(enabled, label)| enabled.then_some(label))
    .collect::<Vec<_>>()
    .join("+")
}

fn write_clipboard_legs(text: &str, route: &ClipboardRoute) -> ClipboardWriteLegs {
    let native_ok = route.native && write_native_text(text);
    let tmux_ok = route.tmux_buffer && write_tmux_buffer(text);
    let osc52_ok = route.osc52
        && write_osc52(
            text,
            route.osc52_tmux_passthrough && tmux_ok,
            terminal_context().brand == TerminalName::Kitty,
        );
    ClipboardWriteLegs {
        native_ok,
        tmux_ok,
        osc52_ok,
    }
}

fn classify_delivery(legs: ClipboardWriteLegs, context: &TerminalContext) -> ClipboardDelivery {
    if legs.native_ok {
        return ClipboardDelivery::Confirmed;
    }
    if legs.tmux_ok {
        return ClipboardDelivery::Confirmed;
    }
    if legs.osc52_ok {
        return match osc52_capability(context.brand) {
            Osc52Capability::Supported => ClipboardDelivery::Confirmed,
            Osc52Capability::Unknown | Osc52Capability::Unsupported => {
                ClipboardDelivery::Unverified
            }
        };
    }
    ClipboardDelivery::Failed
}

fn osc52_capability(brand: TerminalName) -> Osc52Capability {
    if brand.supports_osc52_clipboard() {
        Osc52Capability::Supported
    } else if brand == TerminalName::Unknown {
        Osc52Capability::Unknown
    } else {
        Osc52Capability::Unsupported
    }
}

#[cfg(test)]
fn write_native_text(_text: &str) -> bool {
    // Unit tests must not mutate the developer's real clipboard. Production
    // execution evidence is collected separately from this hermetic seam.
    true
}

#[cfg(all(target_os = "macos", not(test)))]
fn write_native_text(text: &str) -> bool {
    write_child_stdin("pbcopy", &[], text, Duration::from_secs(8))
}

#[cfg(all(target_os = "linux", not(test)))]
fn write_native_text(text: &str) -> bool {
    #[derive(Clone, Copy)]
    enum LinuxCopyTool {
        Wayland,
        Xclip,
        Xsel,
    }
    impl LinuxCopyTool {
        fn command(self) -> (&'static str, &'static [&'static str]) {
            match self {
                Self::Wayland => ("wl-copy", &[]),
                Self::Xclip => ("xclip", &["-selection", "clipboard"]),
                Self::Xsel => ("xsel", &["--clipboard", "--input"]),
            }
        }

        fn write(self, text: &str) -> bool {
            let (command, arguments) = self.command();
            write_child_stdin(command, arguments, text, Duration::from_secs(2))
        }
    }

    static SELECTED: OnceLock<Option<LinuxCopyTool>> = OnceLock::new();
    if let Some(selected) = SELECTED.get() {
        return selected.is_some_and(|tool| tool.write(text));
    }
    for tool in [
        LinuxCopyTool::Wayland,
        LinuxCopyTool::Xclip,
        LinuxCopyTool::Xsel,
    ] {
        if tool.write(text) {
            let _ = SELECTED.set(Some(tool));
            return true;
        }
    }
    let _ = SELECTED.set(None);
    false
}

#[cfg(all(target_os = "windows", not(test)))]
fn write_native_text(text: &str) -> bool {
    write_child_stdin("clip.exe", &[], text, Duration::from_secs(2))
}

#[cfg(all(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    not(test)
))]
fn write_native_text(_text: &str) -> bool {
    false
}

fn write_tmux_buffer(text: &str) -> bool {
    let lc_terminal = std::env::var("LC_TERMINAL").ok();
    let arguments = tmux_load_buffer_arguments(lc_terminal.as_deref());
    write_child_stdin("tmux", arguments, text, Duration::from_secs(2))
}

fn tmux_load_buffer_arguments(lc_terminal: Option<&str>) -> &'static [&'static str] {
    if lc_terminal == Some("iTerm2") {
        &["load-buffer", "-"][..]
    } else {
        &["load-buffer", "-w", "-"][..]
    }
}

fn write_child_stdin(command: &str, arguments: &[&str], text: &str, deadline: Duration) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt as _;

    let Ok(mut child) = Command::new(command)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(text.as_bytes()).is_ok());
        let status = match child.wait_timeout(deadline) {
            Ok(Some(status)) => status.success(),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                false
            }
        };
        let wrote = writer.join().unwrap_or(false);
        status && wrote
    })
}

fn write_osc52(text: &str, tmux_passthrough: bool, kitty: bool) -> bool {
    let bytes = osc52_bytes(text, tmux_passthrough, kitty);
    crate::terminal_writer::enqueue_registered_terminal_control(&bytes).is_ok()
}

fn osc52_bytes(text: &str, tmux_passthrough: bool, kitty: bool) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let terminator = if kitty { "\u{1b}\\" } else { "\u{7}" };
    let payload = format!("\u{1b}]52;c;{encoded}{terminator}");
    if tmux_passthrough {
        // Historical CrabCode always uses BEL for the inner tmux payload,
        // independently of the outer terminal's raw OSC terminator.
        let inner = format!("\u{1b}]52;c;{encoded}\u{7}");
        let escaped = inner.replace('\u{1b}', "\u{1b}\u{1b}");
        format!("\u{1b}Ptmux;{escaped}\u{1b}\\").into_bytes()
    } else {
        payload.into_bytes()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClipboardCapture {
    pub(crate) text: Option<String>,
    pub(crate) image: Option<LoadedComposerImage>,
    pub(crate) image_error: Option<String>,
    /// The attachment probe was intentionally discarded because an Otty
    /// bracketed-paste payload did not match the system clipboard text.
    pub(crate) probe_dropped: bool,
}

pub(crate) fn capture(workspace: &Path) -> Result<ClipboardCapture, String> {
    capture_with_bracketed_origin(workspace, None)
}

/// Probe attachments for a bracketed-paste event. `verify_origin` is enabled
/// only for terminals known by the pinned terminal matrix to deliver IME
/// commits as bracketed paste. In that case an attachment is accepted only
/// when the payload matches the same clipboard generation that is probed.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn capture_bracketed(
    workspace: &Path,
    payload: &str,
    verify_origin: bool,
) -> Result<ClipboardCapture, String> {
    capture_with_bracketed_origin(workspace, verify_origin.then_some(payload))
}

fn capture_with_bracketed_origin(
    workspace: &Path,
    bracketed_payload: Option<&str>,
) -> Result<ClipboardCapture, String> {
    let first_text = if bracketed_payload.is_some() {
        platform_text().map_err(|error| format!("clipboard text origin check failed: {error}"))?
    } else {
        platform_text().ok().flatten()
    };
    if let Some(payload) = bracketed_payload
        && !bracketed_payload_matches_clipboard_text(payload, first_text.as_deref())
    {
        return Ok(ClipboardCapture {
            text: None,
            image: None,
            image_error: None,
            probe_dropped: true,
        });
    }
    let image_bytes = platform_image_png();
    let second_text = platform_text().ok().flatten();

    // Do not combine content from two pasteboard generations.  If the text
    // changed while image capture was running, keep the latest text and drop
    // the potentially unrelated raster.
    if first_text != second_text {
        return Ok(ClipboardCapture {
            text: second_text,
            image: None,
            image_error: Some(
                "clipboard changed while the image probe was running; raster ignored".to_string(),
            ),
            probe_dropped: false,
        });
    }

    let (image, image_error) = match image_bytes {
        Ok(Some(bytes)) => match prepare_png(workspace, &bytes) {
            Ok(image) => (Some(image), None),
            Err(error) => (None, Some(error)),
        },
        Ok(None) => (None, None),
        Err(error) => (None, Some(error)),
    };
    Ok(ClipboardCapture {
        text: first_text,
        image,
        image_error,
        probe_dropped: false,
    })
}

fn bracketed_payload_matches_clipboard_text(payload: &str, clipboard_text: Option<&str>) -> bool {
    fn normalized(text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim_end()
            .to_owned()
    }

    match clipboard_text {
        None => payload.trim().is_empty(),
        Some(text) => normalized(payload) == normalized(text),
    }
}

/// Exact bracketed-paste attachment-probe heuristic from the pinned renderer:
/// empty and short payloads probe, lone HTTP(S) URLs and large/multiline
/// ordinary text do not.
pub(crate) fn bracketed_paste_should_probe(payload: &str) -> bool {
    if payload.is_empty() {
        return true;
    }
    let trimmed = payload.trim();
    if is_lone_http_url(trimmed) {
        return false;
    }
    if trimmed.len() >= 4096 {
        return false;
    }
    trimmed.lines().count().max(1) <= 4 || trimmed.contains("://")
}

fn is_lone_http_url(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };
    !rest.is_empty() && !text.chars().any(char::is_whitespace)
}

fn prepare_png(workspace: &Path, bytes: &[u8]) -> Result<LoadedComposerImage, String> {
    if bytes.is_empty() {
        return Err("clipboard raster was empty".to_string());
    }
    if bytes.len() > MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES {
        return Err(format!(
            "clipboard raster exceeded the {MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES} byte capture limit"
        ));
    }
    let temporary = PrivateCaptureDirectory::create()?;
    let path = temporary.path.join("clipboard.png");
    std::fs::write(&path, bytes)
        .map_err(|error| format!("failed to materialize clipboard raster: {error}"))?;
    load_composer_image(workspace, &path.to_string_lossy())
        .map_err(|error| format!("clipboard raster was rejected: {error}"))
}

struct PrivateCaptureDirectory {
    path: PathBuf,
}

impl PrivateCaptureDirectory {
    fn create() -> Result<Self, String> {
        for _ in 0..16 {
            let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "crabcode-tui-clipboard-{}-{id}",
                std::process::id()
            ));
            match create_private_directory(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to create private clipboard directory: {error}"
                    ));
                }
            }
        }
        Err("failed to allocate a unique private clipboard directory".to_string())
    }
}

impl Drop for PrivateCaptureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().create(path)
}

#[cfg(any(test, target_os = "linux"))]
mod x11_primary {
    use super::*;

    /// Read UTF-8 text from the Linux X11 PRIMARY selection.
    ///
    /// This is renderer-local input handling for one unmodified middle-button
    /// press. It does not consult the structured backend transport. A non-empty
    /// `DISPLAY` is mandatory. X11-native tools are tried in `xclip`, then `xsel`
    /// order; pure X11 may fall back to arboard, while XWayland must not because
    /// arboard could otherwise return the Wayland PRIMARY selection.
    #[cfg(target_os = "linux")]
    pub(crate) fn system_primary_selection_get() -> Option<String> {
        #[cfg(test)]
        if let Some(available) = primary_selection_test_hook_available() {
            if !available {
                return None;
            }
            return primary_selection_test_hook_text()
                .flatten()
                .filter(|text| !text.is_empty());
        }

        let display_env_present = x11_env_present("DISPLAY");
        if !display_env_present {
            return None;
        }
        match read_x11_primary_with_tools(
            display_env_present,
            x11_primary_tool_available,
            |_, argv| {
                run_x11_capture_checked(argv, X11_PRIMARY_READ_WAIT).map_err(anyhow::Error::from)
            },
        ) {
            PrimaryCliRead::Text(text) => return Some(text),
            PrimaryCliRead::Empty => return None,
            PrimaryCliRead::Failed => {}
        }

        if x11_arboard_fallback_allowed(display_env_present, x11_env_present("WAYLAND_DISPLAY")) {
            return read_x11_primary_with_arboard()
                .map_err(|error| {
                    tracing::debug!("X11 PRIMARY arboard read failed: {error}");
                    error
                })
                .ok()
                .flatten()
                .filter(|text| !text.is_empty());
        }
        None
    }

    #[cfg(any(test, target_os = "linux"))]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ToolSpec {
        name: &'static str,
        read_primary: Option<&'static [&'static str]>,
    }

    #[cfg(any(test, target_os = "linux"))]
    const XCLIP_SPEC: ToolSpec = ToolSpec {
        name: "xclip",
        read_primary: Some(&["xclip", "-o", "-selection", "primary"]),
    };

    #[cfg(any(test, target_os = "linux"))]
    const XSEL_SPEC: ToolSpec = ToolSpec {
        name: "xsel",
        read_primary: Some(&["xsel", "--primary", "--output"]),
    };

    #[cfg(any(test, target_os = "linux"))]
    #[derive(Debug, Eq, PartialEq)]
    enum PrimaryCliRead {
        Text(String),
        Empty,
        Failed,
    }

    #[cfg(any(test, target_os = "linux"))]
    fn read_x11_primary_with_tools(
        display_env_present: bool,
        mut available: impl FnMut(&ToolSpec) -> bool,
        mut capture: impl FnMut(&ToolSpec, &[&str]) -> anyhow::Result<Vec<u8>>,
    ) -> PrimaryCliRead {
        if !display_env_present {
            return PrimaryCliRead::Failed;
        }
        for spec in [&XCLIP_SPEC, &XSEL_SPEC] {
            if !available(spec) {
                continue;
            }
            let argv = spec
                .read_primary
                .expect("X11 PRIMARY tool must define read argv");
            match capture(spec, argv) {
                Ok(bytes) if bytes.is_empty() => return PrimaryCliRead::Empty,
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => return PrimaryCliRead::Text(text),
                    Err(e) => {
                        tracing::debug!(tool = spec.name, "X11 PRIMARY text was not UTF-8: {e}")
                    }
                },
                Err(e) => {
                    tracing::debug!(tool = spec.name, "X11 PRIMARY CLI read failed: {e}")
                }
            }
        }
        PrimaryCliRead::Failed
    }

    #[cfg(any(test, target_os = "linux"))]
    fn x11_arboard_fallback_allowed(display_env_present: bool, wayland_env_present: bool) -> bool {
        display_env_present && !wayland_env_present
    }

    #[cfg(any(test, target_os = "linux"))]
    fn x11_env_value_present(value: Option<&std::ffi::OsStr>) -> bool {
        value.is_some_and(|value| !value.is_empty())
    }

    #[cfg(target_os = "linux")]
    fn x11_env_present(variable: &str) -> bool {
        x11_env_value_present(std::env::var_os(variable).as_deref())
    }

    #[cfg(target_os = "linux")]
    const X11_PRIMARY_TOOL_PROBE_WAIT: Duration = Duration::from_secs(1);
    #[cfg(target_os = "linux")]
    const X11_PRIMARY_READ_WAIT: Duration = Duration::from_secs(2);

    #[cfg(target_os = "linux")]
    fn x11_primary_tool_available(spec: &ToolSpec) -> bool {
        static XCLIP_DISCOVERED: OnceLock<()> = OnceLock::new();
        static XSEL_DISCOVERED: OnceLock<()> = OnceLock::new();
        let cache = if std::ptr::eq(spec, &XCLIP_SPEC) {
            &XCLIP_DISCOVERED
        } else {
            debug_assert!(std::ptr::eq(spec, &XSEL_SPEC));
            &XSEL_DISCOVERED
        };
        cache_successful_x11_probe(cache, || {
            let argv = spec
                .read_primary
                .expect("X11 PRIMARY tool must define read argv");
            let mut command = std::process::Command::new(argv[0]);
            command
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            detach_x11_clipboard_command(&mut command);
            let Ok(mut child) = command.spawn() else {
                return false;
            };
            wait_x11_child(&mut child, X11_PRIMARY_TOOL_PROBE_WAIT).is_ok()
        })
    }

    #[cfg(any(test, target_os = "linux"))]
    fn cache_successful_x11_probe(cache: &OnceLock<()>, probe: impl FnOnce() -> bool) -> bool {
        if cache.get().is_some() {
            return true;
        }
        if !probe() {
            return false;
        }
        let _ = cache.set(());
        true
    }

    #[cfg(target_os = "linux")]
    fn run_x11_capture_checked(argv: &[&str], deadline: Duration) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        use std::process::{Command, Stdio};

        let (program, arguments) = argv.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "clipboard argv was empty")
        })?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        detach_x11_clipboard_command(&mut command);
        let mut child = command.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("clipboard child stdout was not piped"))?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes);
            result.map(|_| bytes)
        });
        let status = wait_x11_child(&mut child, deadline);
        let stdout = reader
            .join()
            .map_err(|_| io::Error::other("clipboard stdout reader panicked"))??;
        let status = status?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "{program} exited with status {status}"
            )));
        }
        Ok(stdout)
    }

    #[cfg(target_os = "linux")]
    fn wait_x11_child(
        child: &mut std::process::Child,
        deadline: Duration,
    ) -> io::Result<std::process::ExitStatus> {
        use wait_timeout::ChildExt as _;

        match child.wait_timeout(deadline) {
            Ok(Some(status)) => Ok(status),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("clipboard command exceeded its {deadline:?} deadline"),
                ))
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn detach_x11_clipboard_command(command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt as _;

        // SAFETY: the pre-exec hook invokes only POSIX async-signal-safe
        // `setsid` and, for the process-group-leader edge case, `setpgid`.
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

    #[cfg(target_os = "linux")]
    fn read_x11_primary_with_arboard() -> Result<Option<String>, String> {
        use std::sync::mpsc::RecvTimeoutError;

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("x11-primary-read".to_owned())
            .spawn(move || {
                use arboard::{GetExtLinux as _, LinuxClipboardKind};

                let result = arboard::Clipboard::new()
                    .map_err(|error| error.to_string())
                    .and_then(|mut clipboard| {
                        match clipboard
                            .get()
                            .clipboard(LinuxClipboardKind::Primary)
                            .text()
                        {
                            Ok(text) if text.is_empty() => Ok(None),
                            Ok(text) => Ok(Some(text)),
                            Err(
                                arboard::Error::ContentNotAvailable
                                | arboard::Error::ClipboardNotSupported,
                            ) => Ok(None),
                            Err(error) => Err(error.to_string()),
                        }
                    });
                let _ = sender.send(result);
            })
            .map_err(|error| format!("failed to spawn X11 PRIMARY read worker: {error}"))?;
        match receiver.recv_timeout(X11_PRIMARY_READ_WAIT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err("X11 PRIMARY arboard read timed out".to_owned()),
            Err(RecvTimeoutError::Disconnected) => {
                Err("X11 PRIMARY arboard read worker died".to_owned())
            }
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    thread_local! {
        static PRIMARY_SELECTION_TEST_HOOK: std::cell::RefCell<Option<(bool, Option<String>)>> =
            const { std::cell::RefCell::new(None) };
        static PRIMARY_SELECTION_TEST_READS: std::cell::Cell<u32> =
            const { std::cell::Cell::new(0) };
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_primary_selection_test_hook(available: bool, text: Option<String>) {
        PRIMARY_SELECTION_TEST_HOOK.with(|hook| *hook.borrow_mut() = Some((available, text)));
        PRIMARY_SELECTION_TEST_READS.with(|count| count.set(0));
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn clear_primary_selection_test_hook() {
        PRIMARY_SELECTION_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
        PRIMARY_SELECTION_TEST_READS.with(|count| count.set(0));
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn primary_selection_read_call_count() -> u32 {
        PRIMARY_SELECTION_TEST_READS.with(std::cell::Cell::get)
    }

    #[cfg(all(test, target_os = "linux"))]
    fn primary_selection_test_hook_available() -> Option<bool> {
        PRIMARY_SELECTION_TEST_HOOK
            .with(|hook| hook.borrow().as_ref().map(|(available, _)| *available))
    }

    #[cfg(all(test, target_os = "linux"))]
    fn primary_selection_test_hook_text() -> Option<Option<String>> {
        PRIMARY_SELECTION_TEST_HOOK.with(|hook| {
            hook.borrow().as_ref().map(|(_, text)| {
                PRIMARY_SELECTION_TEST_READS.with(|count| count.set(count.get() + 1));
                text.clone()
            })
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn primary_read_argv_targets_x11_primary_exactly() {
            assert_eq!(
                XCLIP_SPEC.read_primary,
                Some(&["xclip", "-o", "-selection", "primary"][..])
            );
            assert_eq!(
                XSEL_SPEC.read_primary,
                Some(&["xsel", "--primary", "--output"][..])
            );
        }

        #[test]
        fn primary_tool_selection_requires_nonempty_display_before_probing() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                false,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"text".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Failed);
            assert!(
                calls.borrow().is_empty(),
                "non-X11 sessions must not invoke X11 tools"
            );
        }

        #[test]
        fn successful_xclip_never_discovers_xsel() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"from xclip".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xclip".to_owned()));
            assert_eq!(calls.into_inner(), vec!["discover xclip", "read xclip"]);
        }

        #[test]
        fn primary_tool_discovery_caches_only_positive_results() {
            let cache = std::sync::OnceLock::new();
            let mut probes = 0;
            assert!(!cache_successful_x11_probe(&cache, || {
                probes += 1;
                false
            }));
            assert!(cache_successful_x11_probe(&cache, || {
                probes += 1;
                true
            }));
            assert!(cache_successful_x11_probe(&cache, || {
                probes += 1;
                false
            }));
            assert_eq!(probes, 2, "the positive result must skip later probes");
        }

        #[test]
        fn primary_read_tries_xsel_after_xclip_runtime_failure() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    if spec.name == "xclip" {
                        anyhow::bail!("xclip runtime failure");
                    }
                    Ok(b"from xsel".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xsel".to_owned()));
            assert_eq!(
                calls.into_inner(),
                vec!["discover xclip", "read xclip", "discover xsel", "read xsel"]
            );
        }

        #[test]
        fn primary_read_treats_successful_empty_as_authoritative() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    true
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(Vec::new())
                },
            );

            assert_eq!(read, PrimaryCliRead::Empty);
            assert_eq!(calls.into_inner(), vec!["discover xclip", "read xclip"]);
        }

        #[test]
        fn absent_xclip_discovers_xsel_lazily() {
            let calls = std::cell::RefCell::new(Vec::new());
            let read = read_x11_primary_with_tools(
                true,
                |spec| {
                    calls.borrow_mut().push(format!("discover {}", spec.name));
                    spec.name == "xsel"
                },
                |spec, _| {
                    calls.borrow_mut().push(format!("read {}", spec.name));
                    Ok(b"from xsel".to_vec())
                },
            );

            assert_eq!(read, PrimaryCliRead::Text("from xsel".to_owned()));
            assert_eq!(
                calls.into_inner(),
                vec!["discover xclip", "discover xsel", "read xsel"]
            );
        }

        #[test]
        fn primary_read_reports_failure_after_all_backends_fail() {
            let mut calls = Vec::new();
            let read = read_x11_primary_with_tools(
                true,
                |_| true,
                |spec, _| {
                    calls.push(spec.name);
                    anyhow::bail!("runtime failure")
                },
            );

            assert_eq!(read, PrimaryCliRead::Failed);
            assert_eq!(calls, vec!["xclip", "xsel"]);
        }

        #[test]
        fn primary_arboard_fallback_is_x11_only() {
            assert!(x11_arboard_fallback_allowed(true, false));
            assert!(!x11_arboard_fallback_allowed(true, true));
            assert!(!x11_arboard_fallback_allowed(false, false));
            assert!(!x11_arboard_fallback_allowed(false, true));
        }

        #[test]
        fn display_guard_rejects_absent_and_empty_values() {
            use std::ffi::OsStr;

            assert!(!x11_env_value_present(None));
            assert!(!x11_env_value_present(Some(OsStr::new(""))));
            assert!(x11_env_value_present(Some(OsStr::new(":99"))));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn primary_test_seam_filters_empty_and_honors_availability_guard() {
            set_primary_selection_test_hook(true, Some("PRIMARY".to_owned()));
            assert_eq!(system_primary_selection_get().as_deref(), Some("PRIMARY"));
            assert_eq!(primary_selection_read_call_count(), 1);
            clear_primary_selection_test_hook();

            set_primary_selection_test_hook(true, Some(String::new()));
            assert_eq!(system_primary_selection_get(), None);
            assert_eq!(primary_selection_read_call_count(), 1);
            clear_primary_selection_test_hook();

            set_primary_selection_test_hook(false, Some("must not escape".to_owned()));
            assert_eq!(system_primary_selection_get(), None);
            assert_eq!(primary_selection_read_call_count(), 0);
            clear_primary_selection_test_hook();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use x11_primary::system_primary_selection_get;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use x11_primary::{
    clear_primary_selection_test_hook, primary_selection_read_call_count,
    set_primary_selection_test_hook,
};

#[cfg(target_os = "macos")]
fn platform_text() -> io::Result<Option<String>> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("pbpaste");
    command
        .args(["-Prefer", "txt"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = bounded_output(command, MAX_CLIPBOARD_TEXT_CAPTURE_BYTES + 1)?;
    if !output.success && output.stdout.is_empty() {
        return Err(io::Error::other("pbpaste failed"));
    }
    if output.stdout.is_empty() {
        return Ok(None);
    }
    let usable = crate::tui_input::utf8_prefix_within(
        &String::from_utf8_lossy(&output.stdout),
        MAX_CLIPBOARD_TEXT_CAPTURE_BYTES,
    )
    .to_string();
    Ok((!usable.is_empty()).then_some(usable))
}

#[cfg(target_os = "macos")]
fn platform_image_png() -> Result<Option<Vec<u8>>, String> {
    use std::process::{Command, Stdio};

    let temporary = PrivateCaptureDirectory::create()?;
    let image_path = temporary.path.join("capture.png");
    let script = "on run argv\n\
                  try\n\
                  set imageData to the clipboard as \u{00AB}class PNGf\u{00BB}\n\
                  set imagePath to POSIX file (item 1 of argv) as text\n\
                  set imageFile to open for access file imagePath with write permission\n\
                  set eof of imageFile to 0\n\
                  write imageData to imageFile\n\
                  close access imageFile\n\
                  return \"PNG\"\n\
                  on error\n\
                  return \"NONE\"\n\
                  end try\n\
                  end run";
    let mut command = Command::new("osascript");
    command
        .arg("-e")
        .arg(script)
        .arg(&image_path)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = bounded_output(command, 64 * 1024)
        .map_err(|error| format!("clipboard image probe failed: {error}"))?;
    if !output.success {
        return Err("clipboard image probe exited unsuccessfully".to_string());
    }
    if String::from_utf8_lossy(&output.stdout).trim() != "PNG" {
        return Ok(None);
    }
    let metadata = std::fs::metadata(&image_path)
        .map_err(|error| format!("clipboard image result was missing: {error}"))?;
    if metadata.len() > MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES as u64 {
        return Err(format!(
            "clipboard raster exceeded the {MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES} byte capture limit"
        ));
    }
    std::fs::read(&image_path)
        .map(Some)
        .map_err(|error| format!("failed to read clipboard raster: {error}"))
}

#[cfg(target_os = "macos")]
struct BoundedOutput {
    success: bool,
    stdout: Vec<u8>,
}

#[cfg(target_os = "macos")]
fn bounded_output(
    mut command: std::process::Command,
    max_stdout_bytes: usize,
) -> io::Result<BoundedOutput> {
    use std::io::Read as _;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    command.stdout(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("clipboard child stdout was not piped"))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(max_stdout_bytes as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "clipboard command exceeded its two second deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = reader
        .join()
        .map_err(|_| io::Error::other("clipboard stdout reader panicked"))??;
    Ok(BoundedOutput {
        success: status.success(),
        stdout,
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_text() -> io::Result<Option<String>> {
    let mut clipboard = arboard::Clipboard::new().map_err(io::Error::other)?;
    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(text)),
        Ok(_) => Ok(None),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(io::Error::other(error)),
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_image_png() -> Result<Option<Vec<u8>>, String> {
    use image::{ColorType, ImageEncoder as _};

    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(format!("clipboard image read failed: {error}")),
    };
    let width =
        u32::try_from(image.width).map_err(|_| "clipboard image width exceeds u32".to_string())?;
    let height = u32::try_from(image.height)
        .map_err(|_| "clipboard image height exceeds u32".to_string())?;
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "clipboard image dimensions overflow".to_string())?;
    if image.bytes.len() != expected {
        return Err("clipboard image RGBA length did not match its dimensions".to_string());
    }
    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded)
        .write_image(&image.bytes, width, height, ColorType::Rgba8.into())
        .map_err(|error| format!("clipboard image PNG encoding failed: {error}"))?;
    if encoded.len() > MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES {
        return Err(format!(
            "clipboard raster exceeded the {MAX_CLIPBOARD_IMAGE_CAPTURE_BYTES} byte capture limit"
        ));
    }
    Ok(Some(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(entries: &[(&str, &str)]) -> TerminalContext {
        crate::terminal_capabilities::terminal_context_from_env_for_test(entries)
    }

    #[test]
    fn historical_direct_clipboard_route_is_native_tmux_and_osc52() {
        let plain = context(&[("TERM_PROGRAM", "ghostty")]);
        assert_eq!(
            resolve_clipboard_route(&plain, false),
            ClipboardRoute {
                native: true,
                tmux_buffer: false,
                osc52: true,
                osc52_tmux_passthrough: false,
            }
        );
        assert_eq!(
            resolve_clipboard_route(&plain, true),
            ClipboardRoute {
                native: false,
                tmux_buffer: false,
                osc52: true,
                osc52_tmux_passthrough: false,
            },
            "SSH_CONNECTION suppresses only the native remote clipboard leg"
        );

        let tmux = context(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/tmux")]);
        let route = resolve_clipboard_route(&tmux, false);
        assert_eq!(
            route,
            ClipboardRoute {
                native: true,
                tmux_buffer: true,
                osc52: true,
                osc52_tmux_passthrough: true,
            }
        );
        assert_eq!(route_label(&route), "native+tmux+osc52");

        // Historical CrabCode keys solely off TMUX here. Unlike the fixed
        // upstream base, it does not suppress the wrapper for editor terminals.
        let editor_tmux = context(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TMUX", "/tmp/tmux"),
            ("NVIM", "/tmp/nvim"),
        ]);
        assert!(
            resolve_clipboard_route(&editor_tmux, false).osc52_tmux_passthrough,
            "the fixed historical direct-TUI product behavior wins this conflict"
        );
    }

    #[test]
    fn osc52_delivery_capability_is_classified_without_suppressing_historical_output() {
        for brand in [
            TerminalName::Ghostty,
            TerminalName::Kitty,
            TerminalName::WezTerm,
            TerminalName::Alacritty,
            TerminalName::Foot,
            TerminalName::Rio,
            TerminalName::WindowsTerminal,
            TerminalName::Iterm2,
            TerminalName::VsCode,
            TerminalName::Cursor,
            TerminalName::Windsurf,
            TerminalName::Zed,
        ] {
            assert_eq!(osc52_capability(brand), Osc52Capability::Supported);
        }
        assert_eq!(
            osc52_capability(TerminalName::Unknown),
            Osc52Capability::Unknown
        );
        for brand in [
            TerminalName::AppleTerminal,
            TerminalName::WarpTerminal,
            TerminalName::JetBrains,
            TerminalName::Vte,
            TerminalName::Terminator,
            TerminalName::Otty,
        ] {
            assert_eq!(osc52_capability(brand), Osc52Capability::Unsupported);
        }
    }

    #[test]
    fn clipboard_delivery_reports_exact_success_evidence() {
        let known = context(&[("TERM_PROGRAM", "ghostty")]);
        let unknown = context(&[]);
        let native = ClipboardWriteLegs {
            native_ok: true,
            tmux_ok: false,
            osc52_ok: false,
        };
        assert_eq!(
            classify_delivery(native, &known),
            ClipboardDelivery::Confirmed
        );

        let tmux = ClipboardWriteLegs {
            native_ok: false,
            tmux_ok: true,
            osc52_ok: false,
        };
        assert_eq!(
            classify_delivery(tmux, &unknown),
            ClipboardDelivery::Confirmed
        );

        let osc52 = ClipboardWriteLegs {
            native_ok: false,
            tmux_ok: false,
            osc52_ok: true,
        };
        assert_eq!(
            classify_delivery(osc52, &known),
            ClipboardDelivery::Confirmed
        );
        assert_eq!(
            classify_delivery(osc52, &unknown),
            ClipboardDelivery::Unverified
        );
        assert_eq!(
            classify_delivery(osc52, &context(&[("TERM_PROGRAM", "Apple_Terminal")])),
            ClipboardDelivery::Unverified,
            "historical output is retained, but unsupported delivery is not claimed as confirmed"
        );
    }

    #[test]
    fn historical_osc52_payload_and_tmux_passthrough_are_byte_exact() {
        assert_eq!(osc52_bytes("Crab", false, false), b"\x1b]52;c;Q3JhYg==\x07");
        assert_eq!(
            osc52_bytes("Crab", false, true),
            b"\x1b]52;c;Q3JhYg==\x1b\\"
        );
        assert_eq!(
            osc52_bytes("Crab", true, true),
            b"\x1bPtmux;\x1b\x1b]52;c;Q3JhYg==\x07\x1b\\"
        );
    }

    #[test]
    fn osc52_fails_closed_without_an_owned_terminal_writer_generation() {
        assert!(
            !write_osc52("must-not-reach-test-stdout", false, false),
            "an unowned renderer must never fall back to direct stdout"
        );
    }

    #[test]
    fn historical_iterm2_tmux_route_omits_only_write_through_flag() {
        assert_eq!(
            tmux_load_buffer_arguments(Some("iTerm2")),
            ["load-buffer", "-"]
        );
        assert_eq!(
            tmux_load_buffer_arguments(Some("Ghostty")),
            ["load-buffer", "-w", "-"]
        );
        assert_eq!(tmux_load_buffer_arguments(None), ["load-buffer", "-w", "-"]);
    }

    #[test]
    fn private_capture_paths_are_unique_and_removed() {
        let first = PrivateCaptureDirectory::create().expect("first private directory");
        let first_path = first.path.clone();
        let second = PrivateCaptureDirectory::create().expect("second private directory");
        assert_ne!(first.path, second.path);
        drop(first);
        assert!(!first_path.exists());
    }

    #[test]
    fn bracketed_origin_comparison_normalizes_terminal_newlines_and_trailing_newline() {
        assert!(bracketed_payload_matches_clipboard_text(
            "a\rb",
            Some("a\r\nb\n")
        ));
        assert!(!bracketed_payload_matches_clipboard_text(
            "中",
            Some("clipboard")
        ));
        assert!(bracketed_payload_matches_clipboard_text("", None));
        assert!(!bracketed_payload_matches_clipboard_text("中", None));
    }

    #[test]
    fn bracketed_attachment_probe_heuristic_matches_pinned_boundaries() {
        assert!(bracketed_paste_should_probe(""));
        assert!(bracketed_paste_should_probe("caption\nline2"));
        assert!(bracketed_paste_should_probe(
            "https://a.example\nhttps://b.example"
        ));
        assert!(!bracketed_paste_should_probe("https://example.com/path"));
        assert!(!bracketed_paste_should_probe(&"x".repeat(4096)));
        assert!(!bracketed_paste_should_probe("1\n2\n3\n4\n5"));
    }
}
