#![cfg(all(unix, feature = "terminal-lifecycle-tests"))]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;
use unicode_width::UnicodeWidthChar as _;
use vte::{Params, Parser, Perform};

const ENTER_ALTERNATE_SCREEN: &str = "\u{1b}[?1049h";
const LEAVE_ALTERNATE_SCREEN: &str = "\u{1b}[?1049l";
const BEGIN_SYNCHRONIZED_UPDATE: &str = "\u{1b}[?2026h";
const END_SYNCHRONIZED_UPDATE: &str = "\u{1b}[?2026l";
const ENABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004l";
const HIDE_CURSOR: &str = "\u{1b}[?25l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";
const ENABLE_FOCUS_CHANGE: &str = "\u{1b}[?1004h";
const DISABLE_FOCUS_CHANGE: &str = "\u{1b}[?1004l";
const ENABLE_MOUSE_CAPTURE: &str = "\u{1b}[?1000h";
const MOUSE_PASTE_RESET: &str =
    "\u{1b}[?1000l\u{1b}[?1002l\u{1b}[?1003l\u{1b}[?1015l\u{1b}[?1006l\u{1b}[?2004l";
const FATAL_FAULT_TERMINAL_RESTORE: &str = "\u{1b}[?2026l\u{1b}[?25h\u{1b}[?1000l\u{1b}[?1002l\u{1b}[?1003l\u{1b}[?1015l\u{1b}[?1006l\u{1b}[?2004l\u{1b}[?1004l\u{1b}[<u\u{1b}[?1049l";
#[cfg(feature = "terminal-lifecycle-tests")]
const TEST_ONLY_FAULT_AFTER_RAW: &str = "CRABCODE_TUI_TEST_ONLY_FAULT_AFTER_RAW";
#[cfg(feature = "terminal-lifecycle-tests")]
const TEST_ONLY_FAULT_READY_FILE: &str = "CRABCODE_TUI_TEST_ONLY_FAULT_READY_FILE";
const TEST_ONLY_ACQUISITION_READY_FILE: &str = "CRABCODE_TUI_TEST_ONLY_ACQUISITION_READY_FILE";
const TEST_ONLY_ACQUISITION_RELEASE_FILE: &str = "CRABCODE_TUI_TEST_ONLY_ACQUISITION_RELEASE_FILE";
const TEST_ONLY_FIRST_TERMINATION_FILE: &str = "CRABCODE_TUI_TEST_ONLY_FIRST_TERMINATION_FILE";
#[cfg(target_os = "macos")]
const TEST_ONLY_BLOCK_PHYSICAL_MODIFIER_PROBE: &str =
    "CRABCODE_TUI_TEST_ONLY_BLOCK_PHYSICAL_MODIFIER_PROBE";
static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

// These tests all mutate real PTY ownership, process-wide signal handlers,
// and terminal modes. Running them concurrently creates load/order flakes
// that do not represent a supported product lifecycle.

const DIRECT_RUNTIME_FIXTURE: &str = r#"#!/bin/sh
set -eu

mode="${CRABCODE_TUI_FIXTURE_MODE}"
marker="${CRABCODE_TUI_FIXTURE_MARKER}"
pid_file="${CRABCODE_TUI_FIXTURE_PID}"
args_file="${CRABCODE_TUI_FIXTURE_ARGS}"
user_file="${CRABCODE_TUI_FIXTURE_USERS}"
env_file="${CRABCODE_TUI_FIXTURE_ENV}"
setup_file="${CRABCODE_TUI_FIXTURE_SETUP}"
copy_gate="${CRABCODE_TUI_FIXTURE_COPY_GATE}"
notification_channel="${CRABCODE_TUI_FIXTURE_NOTIFICATION_CHANNEL-auto}"

printf '%s' "$$" > "$pid_file"
: > "$args_file"
: > "$user_file"
: > "$setup_file"
printf 'CRABCODE_TEAMMATE_COMMAND=%s\n' \
    "${CRABCODE_TEAMMATE_COMMAND-}" > "$env_file"
printf 'CRABCODE_DESKTOP_AUTOMATION=%s\n' \
    "${CRABCODE_DESKTOP_AUTOMATION-}" >> "$env_file"
printf 'CRABCODE_DESKTOP_AUTOMATION_WRITES=%s\n' \
    "${CRABCODE_DESKTOP_AUTOMATION_WRITES-}" >> "$env_file"
printf 'CRABCODE_DESKTOP_CAPTURE=%s\n' \
    "${CRABCODE_DESKTOP_CAPTURE-}" >> "$env_file"
printf 'CRABCODE_DESKTOP_VISUAL_SIDECAR=%s\n' \
    "${CRABCODE_DESKTOP_VISUAL_SIDECAR-}" >> "$env_file"
for argument in "$@"; do
    printf '%s\n' "$argument" >> "$args_file"
done

# The Rust renderer emits the existing SDK initialize request exactly once before
# terminal ownership. The real TypeScript stdin router stashes that line while
# its closed renderer-only setup exchange runs. Every later setup exchange and
# the initialize response remain inside the one native terminal lifecycle.
IFS= read -r initialize
printf '{"fixture_event":"initialize_request","pid":%s,"request":%s}\n' \
    "$$" "$initialize" >> "$setup_file"
request_id=$(printf '%s\n' "$initialize" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
if [ -z "$request_id" ]; then
    printf 'fixture initialize request omitted request_id\n' >&2
    exit 21
fi
case "$initialize" in
    *'"type":"control_request"'*'"subtype":"initialize"'*) ;;
    *)
        printf 'fixture first stdin frame was not the SDK initialize request\n' >&2
        exit 18
        ;;
esac

canonical_cwd=$(pwd -P)
context_id="crabcode-tui-fixture-renderer-context"
context_request=$(printf '{"type":"control_request","request_id":"%s","request":{"subtype":"crabcode_tui_setup","protocol_version":1,"kind":"renderer_context","cwd":"%s","config_verbose":false,"theme_setting":"dark","syntax_highlighting_disabled":false,"ui_language":"zh-CN","preferred_notification_channel":"%s","message_idle_notification_threshold_ms":0}}' \
    "$context_id" "$canonical_cwd" "$notification_channel")
printf '{"fixture_event":"renderer_context_request","pid":%s,"request":%s}\n' \
    "$$" "$context_request" >> "$setup_file"
printf '%s\n' "$context_request"
IFS= read -r context_response
printf '{"fixture_event":"renderer_context_response","pid":%s,"response":%s}\n' \
    "$$" "$context_response" >> "$setup_file"
case "$context_response" in
    *"\"request_id\":\"${context_id}\""*"\"kind\":\"renderer_context\""*"\"decision\":\"received\""*) ;;
    *)
        printf 'fixture renderer-context response failed correlation\n' >&2
        exit 19
        ;;
esac

if [ "$mode" = "setup-blocked" ]; then
    # The fixed TypeScript setup router rejects every non-setup envelope until
    # finishSetup transfers stdin to StructuredIO. Keep that boundary open so
    # the PTY suite can prove an early signal reaps the child without injecting
    # end_session into the setup response lane.
    while IFS= read -r setup_message; do
        case "$setup_message" in
            *'"subtype":"end_session"'*)
                : > "$marker"
                printf 'fixture received end_session before runtime handoff\n' >&2
                exit 29
                ;;
            *)
                printf 'fixture received non-setup input before runtime handoff\n' >&2
                exit 30
                ;;
        esac
    done
    exit 0
fi

if [ "$mode" = "oauth-success" ]; then
    oauth_id="crabcode-tui-fixture-oauth-success"
    oauth_request=$(printf '{"type":"control_request","request_id":"%s","request":{"subtype":"crabcode_tui_setup","protocol_version":1,"kind":"onboarding","stage":"oauth","phase":"success","title":"AUTH_SUCCESS_FIXTURE","body":["Authentication completed successfully."]}}' \
        "$oauth_id")
    printf '{"fixture_event":"oauth_success_request","pid":%s,"request":%s}\n' \
        "$$" "$oauth_request" >> "$setup_file"
    printf '%s\n' "$oauth_request"
    IFS= read -r oauth_response
    printf '{"fixture_event":"oauth_success_response","pid":%s,"response":%s}\n' \
        "$$" "$oauth_response" >> "$setup_file"
    case "$oauth_response" in
        *"\"request_id\":\"${oauth_id}\""*"\"kind\":\"onboarding\""*"\"stage\":\"oauth\""*"\"phase\":\"success\""*"\"decision\":\"continue\""*) ;;
        *)
            printf 'fixture OAuth-success response failed correlation\n' >&2
            exit 31
            ;;
    esac
fi

if [ "$mode" = "oauth-browser-copy" ]; then
    browser_url_id="crabcode-tui-fixture-oauth-browser-url"
    browser_url_request=$(printf '{"type":"control_request","request_id":"%s","request":{"subtype":"crabcode_tui_setup","protocol_version":1,"kind":"onboarding","stage":"oauth","phase":"browser_url","title":"Opening browser to sign in…","body":["BROWSER_OPEN_FAILED_ORDER_MARKER","(c to copy)"],"url":"https://acosmi.test/login?flow=ordered-copy"}}' \
        "$browser_url_id")
    printf '{"fixture_event":"oauth_browser_url_request","pid":%s,"request":%s}\n' \
        "$$" "$browser_url_request" >> "$setup_file"
    printf '%s\n' "$browser_url_request"
    IFS= read -r browser_url_response
    printf '{"fixture_event":"oauth_browser_url_response","pid":%s,"response":%s}\n' \
        "$$" "$browser_url_response" >> "$setup_file"
    case "$browser_url_response" in
        *"\"request_id\":\"${browser_url_id}\""*"\"kind\":\"onboarding\""*"\"stage\":\"oauth\""*"\"phase\":\"browser_url\""*"\"decision\":\"rendered\""*) ;;
        *)
            printf 'fixture OAuth browser-url response failed correlation\n' >&2
            exit 32
            ;;
    esac

    browser_failed_id="crabcode-tui-fixture-oauth-browser-open-failed"
    browser_failed_request=$(printf '{"type":"control_request","request_id":"%s","request":{"subtype":"crabcode_tui_setup","protocol_version":1,"kind":"onboarding","stage":"oauth","phase":"browser_open_failed"}}' \
        "$browser_failed_id")
    printf '{"fixture_event":"oauth_browser_open_failed_request","pid":%s,"request":%s}\n' \
        "$$" "$browser_failed_request" >> "$setup_file"
    printf '%s\n' "$browser_failed_request"
    IFS= read -r browser_failed_response
    printf '{"fixture_event":"oauth_browser_open_failed_response","pid":%s,"response":%s}\n' \
        "$$" "$browser_failed_response" >> "$setup_file"
    case "$browser_failed_response" in
        *"\"request_id\":\"${browser_failed_id}\""*"\"kind\":\"onboarding\""*"\"stage\":\"oauth\""*"\"phase\":\"browser_open_failed\""*"\"decision\":\"rendered\""*) ;;
        *)
            printf 'fixture OAuth browser-open-failed response failed correlation\n' >&2
            exit 33
            ;;
    esac
    # Keep the SDK initialize response stashed exactly as the real setup
    # router does while OAuth is waiting. The PTY test opens this filesystem
    # gate only after it observes the ordered OSC52 and copied-feedback frame.
    while [ ! -f "$copy_gate" ]; do
        sleep 0.05
    done
fi

case "$mode" in
    trust-accept|trust-reject)
        trust_id="crabcode-tui-fixture-workspace-trust"
        trust_request=$(printf '{"type":"control_request","request_id":"%s","request":{"subtype":"crabcode_tui_setup","protocol_version":1,"kind":"workspace_trust"}}' \
            "$trust_id")
        printf '{"fixture_event":"workspace_trust_request","pid":%s,"request":%s}\n' \
            "$$" "$trust_request" >> "$setup_file"
        printf '%s\n' "$trust_request"
        IFS= read -r trust_response
        printf '{"fixture_event":"workspace_trust_response","pid":%s,"response":%s}\n' \
            "$$" "$trust_response" >> "$setup_file"
        expected_decision="accept"
        if [ "$mode" = "trust-reject" ]; then
            expected_decision="reject"
        fi
        case "$trust_response" in
            *"\"request_id\":\"${trust_id}\""*"\"kind\":\"workspace_trust\""*"\"decision\":\"${expected_decision}\""*) ;;
            *)
                printf 'fixture workspace-trust response failed correlation\n' >&2
                exit 20
                ;;
        esac
        if [ "$mode" = "trust-reject" ]; then
            printf 'Workspace trust was declined for %s\n' "$canonical_cwd" >&2
            exit 22
        fi
        ;;
    *)
        # Already-trusted workspaces have no workspace_trust renderer wire.
        ;;
esac

case "$mode" in
    reject)
        printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"fixture rejected initialize"}}\n' "$request_id"
        sleep 0.5
        exit 23
        ;;
    malformed)
        printf 'not-json\n'
        sleep 0.5
        exit 24
        ;;
    missing-commands)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"agents":[],"output_style":"default","available_output_styles":[],"models":[],"account":{}}}}\n' "$request_id"
        sleep 0.5
        exit 25
        ;;
    ready|crash|trust-accept|force-killed|oauth-success|oauth-browser-copy)
        initialize_response=$(printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"commands":[{"name":"help","description":"show help","argumentHint":""}],"agents":[],"output_style":"default","available_output_styles":["default"],"models":[],"account":{}}}}' "$request_id")
        printf '{"fixture_event":"initialize_response","pid":%s,"response":%s}\n' \
            "$$" "$initialize_response" >> "$setup_file"
        printf '%s\n' "$initialize_response"
        ;;
    *)
        printf 'unknown direct fixture mode: %s\n' "$mode" >&2
        exit 26
        ;;
esac

if [ "$mode" = "crash" ]; then
    sleep 0.5
    exit 27
fi

while IFS= read -r message; do
    case "$message" in
        *'"subtype":"end_session"'*)
            end_request_id=$(printf '%s\n' "$message" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
            if [ -z "$end_request_id" ]; then
                printf 'fixture end_session request omitted request_id\n' >&2
                exit 28
            fi
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$end_request_id"
            : > "$marker"
            exit 0
            ;;
        *'"type":"user"'*)
            printf '%s\n' "$message" >> "$user_file"
            ;;
        *)
            ;;
    esac
done

exit 0
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockMode {
    Ready,
    Crash,
    ForceKilled,
    SetupBlocked,
    OauthSuccess,
    OauthBrowserCopy,
    RejectInitialize,
    MalformedInitialize,
    MissingCommands,
    TrustAccept,
    TrustReject,
}

impl MockMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Crash => "crash",
            Self::ForceKilled => "force-killed",
            Self::SetupBlocked => "setup-blocked",
            Self::OauthSuccess => "oauth-success",
            Self::OauthBrowserCopy => "oauth-browser-copy",
            Self::RejectInitialize => "reject",
            Self::MalformedInitialize => "malformed",
            Self::MissingCommands => "missing-commands",
            Self::TrustAccept => "trust-accept",
            Self::TrustReject => "trust-reject",
        }
    }

    const fn expects_shutdown(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::TrustAccept | Self::OauthSuccess | Self::OauthBrowserCopy
        )
    }
}

struct MockDirectRuntime {
    directory: PathBuf,
    script_path: PathBuf,
    marker_path: PathBuf,
    pid_path: PathBuf,
    args_path: PathBuf,
    users_path: PathBuf,
    env_path: PathBuf,
    setup_path: PathBuf,
    copy_gate_path: PathBuf,
    mode: MockMode,
}

impl MockDirectRuntime {
    fn start(mode: MockMode) -> std::io::Result<Self> {
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let unique = format!(
            "cctui-direct-{}-{sequence}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        );
        let directory = PathBuf::from("/tmp").join(unique);
        let runtime_directory = directory.join("dist").join("tui-runtime");
        std::fs::create_dir_all(&runtime_directory)?;
        let script_path = runtime_directory.join("index.js");
        if let Err(error) = std::fs::write(&script_path, DIRECT_RUNTIME_FIXTURE) {
            let _cleanup = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        Ok(Self {
            marker_path: directory.join("shutdown.marker"),
            pid_path: directory.join("runtime.pid"),
            args_path: directory.join("runtime.args"),
            users_path: directory.join("runtime.users"),
            env_path: directory.join("runtime.env"),
            setup_path: directory.join("runtime.setup.jsonl"),
            copy_gate_path: directory.join("runtime.copy-gate"),
            directory,
            script_path,
            mode,
        })
    }

    fn configure(&self, command: &mut CommandBuilder) {
        command.env("CRABCODE_TUI_RUNTIME_SCRIPT", &self.script_path);
        command.env("CRABCODE_TUI_BUN", "/bin/sh");
        command.env("CRABCODE_TUI_FIXTURE_MODE", self.mode.label());
        command.env("CRABCODE_TUI_FIXTURE_MARKER", &self.marker_path);
        command.env("CRABCODE_TUI_FIXTURE_PID", &self.pid_path);
        command.env("CRABCODE_TUI_FIXTURE_ARGS", &self.args_path);
        command.env("CRABCODE_TUI_FIXTURE_USERS", &self.users_path);
        command.env("CRABCODE_TUI_FIXTURE_ENV", &self.env_path);
        command.env("CRABCODE_TUI_FIXTURE_SETUP", &self.setup_path);
        command.env("CRABCODE_TUI_FIXTURE_COPY_GATE", &self.copy_gate_path);
        command.env("CRABCODE_TUI_FIXTURE_NOTIFICATION_CHANNEL", "auto");
    }

    fn forwarded_args(&self) -> Vec<String> {
        wait_until_condition(
            Duration::from_secs(5),
            "direct runtime argv capture",
            || self.args_path.is_file(),
        );
        std::fs::read_to_string(&self.args_path)
            .expect("read direct runtime argv capture")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn user_messages(&self) -> Vec<String> {
        wait_until_condition(
            Duration::from_secs(5),
            "direct runtime user-message capture",
            || {
                self.users_path
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() > 0)
            },
        );
        std::fs::read_to_string(&self.users_path)
            .expect("read direct runtime user-message capture")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn runtime_environment(&self) -> String {
        wait_until_condition(
            Duration::from_secs(5),
            "direct runtime environment capture",
            || self.env_path.is_file(),
        );
        std::fs::read_to_string(&self.env_path).expect("read direct runtime environment capture")
    }

    fn runtime_pid(&self) -> u64 {
        wait_until_condition(Duration::from_secs(5), "direct runtime pid capture", || {
            self.pid_path.is_file()
        });
        std::fs::read_to_string(&self.pid_path)
            .expect("read direct runtime pid")
            .parse()
            .expect("parse direct runtime pid")
    }

    fn setup_transcript(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.setup_path)
            .expect("read renderer-setup fixture transcript")
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse renderer-setup fixture event"))
            .collect()
    }

    fn finish(self) {
        let expect_shutdown = self.mode.expects_shutdown();
        self.finish_with_shutdown_expectation(expect_shutdown);
    }

    fn finish_without_shutdown_ack(self) {
        self.finish_with_shutdown_expectation(false);
    }

    fn finish_with_shutdown_expectation(mut self, expect_shutdown: bool) {
        wait_until_condition(Duration::from_secs(5), "direct runtime pid capture", || {
            self.pid_path.is_file()
        });
        if expect_shutdown {
            wait_until_condition(
                Duration::from_secs(6),
                "direct runtime end_session acknowledgement",
                || self.marker_path.is_file(),
            );
        }
        let pid = std::fs::read_to_string(&self.pid_path)
            .expect("read direct runtime pid")
            .parse::<i32>()
            .expect("parse direct runtime pid");
        wait_until_condition(
            Duration::from_secs(6),
            "direct runtime process reap",
            || matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH)),
        );
        self.cleanup();
    }

    fn cleanup(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.directory);
    }
}

impl Drop for MockDirectRuntime {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn wait_until(output: &Arc<Mutex<Vec<u8>>>, timeout: Duration, predicate: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = {
            let bytes = output.lock().expect("capture lock");
            String::from_utf8_lossy(&bytes).into_owned()
        };
        if predicate(&snapshot) || predicate(&searchable_terminal_output(&snapshot)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for terminal output; captured {} bytes; visible frames: {:?}; raw: {snapshot:?}",
            snapshot.len(),
            searchable_terminal_output(&snapshot),
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Build a search-only transcript without replacing the raw PTY authority.
///
/// Ratatui's differential painter can place cursor/style escapes between two
/// adjacent visible words.  Raw protocol assertions still receive the exact
/// capture first.  This fallback appends the visible screen at every completed
/// synchronized-frame boundary, so text waits observe what a user saw while
/// frame-count, terminal-mode, and ordering checks continue to match the raw
/// control bytes at the start of the same string.
fn searchable_terminal_output(raw: &str) -> String {
    let mut searchable = raw.to_string();
    let mut parser = Parser::new();
    let mut screen = PtyVisibleScreen::default();
    let mut consumed = 0;
    for (offset, _) in raw.match_indices(END_SYNCHRONIZED_UPDATE) {
        let end = offset + END_SYNCHRONIZED_UPDATE.len();
        parser.advance(&mut screen, raw[consumed..end].as_bytes());
        searchable.push('\n');
        searchable.push_str(&screen.plain());
        consumed = end;
    }
    if consumed < raw.len() {
        parser.advance(&mut screen, raw[consumed..].as_bytes());
        searchable.push('\n');
        searchable.push_str(&screen.plain());
    }
    searchable
}

fn completed_frame_end_showing_after(
    raw: &str,
    after_offset: usize,
    expected: &str,
) -> Option<usize> {
    let mut parser = Parser::new();
    let mut screen = PtyVisibleScreen::default();
    let mut consumed = 0;
    for (offset, _) in raw.match_indices(END_SYNCHRONIZED_UPDATE) {
        let end = offset + END_SYNCHRONIZED_UPDATE.len();
        parser.advance(&mut screen, raw[consumed..end].as_bytes());
        consumed = end;
        if end > after_offset && screen.plain().contains(expected) {
            return Some(end);
        }
    }
    None
}

#[derive(Default)]
struct PtyVisibleScreen {
    rows: Vec<Vec<String>>,
    row: usize,
    column: usize,
}

impl PtyVisibleScreen {
    fn ensure_position(&mut self) {
        while self.rows.len() <= self.row {
            self.rows.push(Vec::new());
        }
        if self.rows[self.row].len() < self.column {
            self.rows[self.row].resize(self.column, " ".to_string());
        }
    }

    fn clear_from_cursor(&mut self) {
        self.ensure_position();
        self.rows[self.row].truncate(self.column);
        self.rows.truncate(self.row + 1);
    }

    fn plain(&self) -> String {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(String::as_str)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Perform for PtyVisibleScreen {
    fn print(&mut self, character: char) {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            if self.column > 0 {
                self.ensure_position();
                if let Some(cell) = self.rows[self.row].get_mut(self.column - 1) {
                    cell.push(character);
                }
            }
            return;
        }
        self.ensure_position();
        let row = &mut self.rows[self.row];
        if row.len() <= self.column {
            row.resize(self.column + 1, " ".to_string());
        }
        row[self.column] = character.to_string();
        for continuation in 1..width {
            let column = self.column + continuation;
            if row.len() <= column {
                row.resize(column + 1, " ".to_string());
            }
            row[column].clear();
        }
        self.column = self.column.saturating_add(width);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => {
                self.row = self.row.saturating_add(1);
                self.column = 0;
            }
            b'\r' => self.column = 0,
            b'\t' => self.column = (self.column / 8 + 1) * 8,
            0x08 => self.column = self.column.saturating_sub(1),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let parameter = |index: usize, default: usize| {
            params
                .iter()
                .nth(index)
                .and_then(|value| value.first())
                .copied()
                .map(usize::from)
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        match action {
            'H' | 'f' => {
                self.row = parameter(0, 1).saturating_sub(1);
                self.column = parameter(1, 1).saturating_sub(1);
            }
            'G' => self.column = parameter(0, 1).saturating_sub(1),
            'd' => self.row = parameter(0, 1).saturating_sub(1),
            'A' => self.row = self.row.saturating_sub(parameter(0, 1)),
            'B' => self.row = self.row.saturating_add(parameter(0, 1)),
            'C' => self.column = self.column.saturating_add(parameter(0, 1)),
            'D' => self.column = self.column.saturating_sub(parameter(0, 1)),
            'E' => {
                self.row = self.row.saturating_add(parameter(0, 1));
                self.column = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(parameter(0, 1));
                self.column = 0;
            }
            'J' => match parameter(0, 0) {
                0 => self.clear_from_cursor(),
                2 | 3 => {
                    self.rows.clear();
                    self.row = 0;
                    self.column = 0;
                }
                _ => {}
            },
            'K' => {
                self.ensure_position();
                match parameter(0, 0) {
                    0 => self.rows[self.row].truncate(self.column),
                    2 => self.rows[self.row].clear(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn wait_until_condition(timeout: Duration, description: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn occurrences(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

fn base_command(mode: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_crabcode-tui"));
    command.env("TERM", "xterm-256color");
    command.arg(match mode {
        "fullscreen" => "--fullscreen",
        "no-alt-screen" => "--no-alt-screen",
        "minimal" => "--minimal",
        other => panic!("unsupported terminal-lifecycle fixture mode {other}"),
    });
    command.env_remove("CRABCODE_TUI_RUNTIME_SCRIPT");
    command.env_remove("CRABCODE_TUI_BUN");
    command
}

fn run_to_pre_raw_exit(
    mode: &str,
    cwd: Option<&Path>,
    configure: impl FnOnce(&mut CommandBuilder),
) -> String {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let mut command = base_command(mode);
    configure(&mut command);
    if let Some(cwd) = cwd {
        command.cwd(cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui in PTY");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_capture = Arc::clone(&captured);
    let reader_thread = std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => reader_capture
                    .lock()
                    .expect("capture lock")
                    .extend_from_slice(&chunk[..read]),
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if child.try_wait().expect("poll PTY child").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("crabcode-tui did not return its pre-raw error");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    let bytes = captured.lock().expect("capture lock");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn rendered_terminal_text(output: &str) -> String {
    searchable_terminal_output(output)
}

fn run_to_post_raw_failure(
    mode: &str,
    cwd: &Path,
    expected: &str,
    configure: impl FnOnce(&mut CommandBuilder),
) -> (String, bool) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open failure-path PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read termios before failure-path ownership");
    let mut command = base_command(mode);
    configure(&mut command);
    command.cwd(cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn failure-path TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take failure-path PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone failure-path PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN) && rendered_terminal_text(output).contains(expected)
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read failure-path raw termios"),
        shell_termios,
        "initialize and protocol failures occur inside the one fixed terminal lifecycle"
    );
    {
        let mut writer = writer.lock().expect("failure-path PTY writer lock");
        writer
            .write_all(&[0x03, 0x03])
            .expect("write failure-path exit keys");
        writer.flush().expect("flush failure-path exit keys");
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll failure-path TUI") {
            break status;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("failure-path TUI did not exit after two Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read failure-path termios after exit"),
        shell_termios,
        "the fixed lifecycle must restore termios after a setup/initialize failure"
    );
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join failure-path PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (output, status.success())
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    captured: Arc<Mutex<Vec<u8>>>,
    cursor_writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let bytes = &chunk[..read];
                    if bytes.windows(4).any(|window| window == b"\x1b[6n")
                        && let Some(writer) = &cursor_writer
                    {
                        let mut writer = writer.lock().expect("PTY writer lock");
                        writer
                            .write_all(b"\x1b[1;1R")
                            .expect("answer cursor-position query");
                        writer.flush().expect("flush cursor-position answer");
                    }
                    captured
                        .lock()
                        .expect("capture lock")
                        .extend_from_slice(bytes);
                }
            }
        }
    })
}

fn assert_never_entered_raw_mode(output: &str) {
    assert!(!output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
    assert!(!output.contains(ENABLE_BRACKETED_PASTE), "{output:?}");
    assert!(!output.contains(HIDE_CURSOR), "{output:?}");
}

fn assert_fixture_pid(events: &[Value], expected_pid: u64) {
    for event in events {
        assert_eq!(
            event.get("pid").and_then(Value::as_u64),
            Some(expected_pid),
            "every setup phase must be emitted or observed by the same direct runtime child: {event}"
        );
    }
}

fn assert_setup_request<'a>(
    event: &'a Value,
    fixture_event: &str,
    request_id: &str,
    kind: &str,
) -> &'a Value {
    let request = event
        .pointer("/request/request")
        .expect("fixture event contains setup request payload");
    assert_eq!(
        event.get("fixture_event").and_then(Value::as_str),
        Some(fixture_event)
    );
    assert_eq!(
        event.pointer("/request/type").and_then(Value::as_str),
        Some("control_request")
    );
    assert_eq!(
        event.pointer("/request/request_id").and_then(Value::as_str),
        Some(request_id)
    );
    assert_eq!(
        request.get("subtype").and_then(Value::as_str),
        Some("crabcode_tui_setup")
    );
    assert_eq!(
        request.get("protocol_version").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(request.get("kind").and_then(Value::as_str), Some(kind));
    request
}

fn assert_setup_response<'a>(
    event: &'a Value,
    fixture_event: &str,
    request_id: &str,
    kind: &str,
    decision: &str,
) -> &'a Value {
    let wrapper = event
        .pointer("/response/response")
        .expect("fixture event contains control response wrapper");
    let response = wrapper
        .get("response")
        .expect("control response contains setup payload");
    assert_eq!(
        event.get("fixture_event").and_then(Value::as_str),
        Some(fixture_event)
    );
    assert_eq!(
        event.pointer("/response/type").and_then(Value::as_str),
        Some("control_response")
    );
    assert_eq!(
        wrapper.get("subtype").and_then(Value::as_str),
        Some("success")
    );
    assert_eq!(
        wrapper.get("request_id").and_then(Value::as_str),
        Some(request_id)
    );
    assert_eq!(
        response.get("protocol_version").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(response.get("kind").and_then(Value::as_str), Some(kind));
    assert_eq!(
        response.get("decision").and_then(Value::as_str),
        Some(decision)
    );
    response
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn conflicting_cli_modes_fail_before_runtime_and_raw_mode() {
    let output = run_to_pre_raw_exit("fullscreen", None, |command| {
        command.arg("--minimal");
    });
    assert!(
        output.contains("conflicting terminal mode arguments")
            && output.contains("fullscreen")
            && output.contains("--minimal"),
        "unexpected normal-terminal error output: {output:?}"
    );
    assert_never_entered_raw_mode(&output);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn missing_runtime_bundle_and_spawn_failure_are_reported_before_raw_mode() {
    let missing = PathBuf::from(format!(
        "/private/tmp/cctui-no-runtime-{}.js",
        std::process::id()
    ));
    let _remove_stale = std::fs::remove_file(&missing);
    let output = run_to_pre_raw_exit("fullscreen", None, |command| {
        command.env("CRABCODE_TUI_RUNTIME_SCRIPT", &missing);
        command.env("CRABCODE_TUI_BUN", "/bin/sh");
    });
    assert!(
        output.contains("runtime bundle is unavailable") && output.contains("not a regular file"),
        "unexpected missing-runtime output: {output:?}"
    );
    assert_never_entered_raw_mode(&output);

    let fixture = MockDirectRuntime::start(MockMode::RejectInitialize)
        .expect("create direct runtime spawn fixture");
    let output = run_to_pre_raw_exit("fullscreen", None, |command| {
        fixture.configure(command);
        command.env(
            "CRABCODE_TUI_BUN",
            "/private/tmp/cctui-definitely-missing-bun",
        );
    });
    assert!(
        output.contains("runtime bundle is unavailable")
            && output.contains("test runtime executable")
            && output.contains("not a regular file"),
        "unexpected runtime-executable validation output: {output:?}"
    );
    assert_never_entered_raw_mode(&output);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn direct_initialize_rejection_and_malformed_frames_fail_closed_inside_one_terminal_lifecycle() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    for mode in [
        MockMode::RejectInitialize,
        MockMode::MalformedInitialize,
        MockMode::MissingCommands,
    ] {
        let fixture = MockDirectRuntime::start(mode).expect("create initialize fixture");
        let (output, success) = run_to_post_raw_failure(
            "fullscreen",
            &canonical_cwd,
            "协议兼容性失败",
            |command| {
                fixture.configure(command);
            },
        );
        assert!(
            rendered_terminal_text(&output).contains("协议兼容性失败"),
            "unexpected initialize error output: {output:?}"
        );
        assert!(!success, "initialize failure must preserve a failed status");
        assert_eq!(occurrences(&output, ENTER_ALTERNATE_SCREEN), 1);
        assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
        assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
        let transcript = fixture.setup_transcript();
        assert_eq!(
            transcript
                .first()
                .and_then(|event| event.get("fixture_event"))
                .and_then(Value::as_str),
            Some("initialize_request"),
            "the existing SDK initialize must be emitted once and stashed"
        );
        assert_eq!(
            transcript
                .get(1)
                .and_then(|event| event.get("fixture_event"))
                .and_then(Value::as_str),
            Some("renderer_context_request"),
            "renderer_context must be the first renderer-only setup exchange"
        );
        assert!(
            transcript.iter().all(|event| {
                event
                    .pointer("/request/request/kind")
                    .and_then(Value::as_str)
                    != Some("workspace_trust")
            }),
            "the trusted failure fixture must not emit workspace-trust wire"
        );
        fixture.finish();
    }
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn untrusted_workspace_uses_one_cwd_only_decision_before_initialize_ready_boundary() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let canonical_cwd_text = canonical_cwd.to_str().expect("UTF-8 canonical test cwd");
    let fixture =
        MockDirectRuntime::start(MockMode::TrustAccept).expect("create trust-accept runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open trust-accept PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read termios before workspace-trust prompt");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn trust-accept TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take trust PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone trust PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("是否信任此工作区？") && output.contains("工作区：")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read workspace-trust prompt termios"),
        shell_termios,
        "the native confirmation prompt must own raw mode"
    );
    {
        let mut writer = writer.lock().expect("trust PTY writer lock");
        writer
            .write_all(b"y")
            .expect("accept native workspace-trust prompt");
        writer.flush().expect("flush workspace-trust acceptance");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "initialize response after accepted workspace trust",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );

    let runtime_pid = fixture.runtime_pid();
    let transcript = fixture.setup_transcript();
    let event_order = transcript
        .iter()
        .map(|event| {
            event
                .get("fixture_event")
                .and_then(Value::as_str)
                .expect("fixture event kind")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        event_order,
        [
            "initialize_request",
            "renderer_context_request",
            "renderer_context_response",
            "workspace_trust_request",
            "workspace_trust_response",
            "initialize_response",
        ],
        "the original initialize request must be stashed while renderer context and the one untrusted-workspace decision complete; its response is the only ready boundary"
    );
    assert_fixture_pid(&transcript, runtime_pid);

    let initialize_request = transcript[0]
        .get("request")
        .expect("initialize fixture event contains the request");
    assert_eq!(
        initialize_request
            .pointer("/request/subtype")
            .and_then(Value::as_str),
        Some("initialize")
    );
    let initialize_request_id = initialize_request
        .get("request_id")
        .and_then(Value::as_str)
        .expect("initialize request id");

    let context_request = assert_setup_request(
        &transcript[1],
        "renderer_context_request",
        "crabcode-tui-fixture-renderer-context",
        "renderer_context",
    );
    assert_eq!(
        context_request.get("cwd").and_then(Value::as_str),
        Some(canonical_cwd_text)
    );
    assert_eq!(
        context_request
            .get("config_verbose")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        context_request.get("theme_setting").and_then(Value::as_str),
        Some("dark")
    );
    assert_eq!(
        context_request
            .get("syntax_highlighting_disabled")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        context_request.get("ui_language").and_then(Value::as_str),
        Some("zh-CN")
    );
    assert_eq!(
        context_request
            .get("preferred_notification_channel")
            .and_then(Value::as_str),
        Some("auto")
    );
    assert_eq!(
        context_request
            .get("message_idle_notification_threshold_ms")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        context_request.as_object().map(serde_json::Map::len),
        Some(10),
        "renderer_context must use the exact closed DTO"
    );
    let context_response = assert_setup_response(
        &transcript[2],
        "renderer_context_response",
        "crabcode-tui-fixture-renderer-context",
        "renderer_context",
        "received",
    );
    assert_eq!(
        context_response.as_object().map(serde_json::Map::len),
        Some(3),
        "renderer_context acknowledgement must use the exact closed DTO"
    );

    let trust_request = assert_setup_request(
        &transcript[3],
        "workspace_trust_request",
        "crabcode-tui-fixture-workspace-trust",
        "workspace_trust",
    );
    assert!(
        trust_request.get("cwd").is_none(),
        "workspace trust must reuse the cwd already bound by renderer_context"
    );
    assert_eq!(
        trust_request.as_object().map(serde_json::Map::len),
        Some(3),
        "untrusted workspace wire is exactly the closed setup base"
    );
    let trust_response = assert_setup_response(
        &transcript[4],
        "workspace_trust_response",
        "crabcode-tui-fixture-workspace-trust",
        "workspace_trust",
        "accept",
    );
    assert_eq!(
        trust_response.as_object().map(serde_json::Map::len),
        Some(3),
        "workspace trust response is exactly protocol version, kind, and decision"
    );

    assert_eq!(
        transcript[5]
            .pointer("/response/response/request_id")
            .and_then(Value::as_str),
        Some(initialize_request_id)
    );
    assert_eq!(
        transcript[5]
            .pointer("/response/response/subtype")
            .and_then(Value::as_str),
        Some("success")
    );
    assert!(
        transcript.iter().all(|event| {
            event
                .pointer("/request/request/kind")
                .and_then(Value::as_str)
                .is_none_or(|kind| matches!(kind, "renderer_context" | "workspace_trust"))
        }),
        "fixture must not reintroduce a second completion or trust protocol"
    );

    let pid = child.process_id().expect("trust-accept TUI pid");
    kill(Pid::from_raw(pid as i32), Signal::SIGINT).expect("send trust-accept SIGINT");
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll trust-accept child") {
            assert!(status.success(), "trust-accept TUI failed: {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("trust-accept TUI did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after trust-accept exit"),
        shell_termios,
        "the confirmation and main TUI must restore the original termios"
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join trust-accept PTY reader");
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn workspace_trust_escape_exits_cleanly_without_initialize_response() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let canonical_cwd_text = canonical_cwd.to_str().expect("UTF-8 canonical test cwd");
    let fixture =
        MockDirectRuntime::start(MockMode::TrustReject).expect("create trust-reject runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open trust-reject PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read termios before workspace-trust rejection");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn trust-reject TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take trust PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone trust PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("是否信任此工作区？") && output.contains("工作区：")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read rejecting prompt termios"),
        shell_termios
    );
    {
        let mut writer = writer.lock().expect("trust PTY writer lock");
        writer
            .write_all(&[0x1b])
            .expect("escape native workspace-trust prompt");
        writer.flush().expect("flush workspace-trust escape");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "single rejected trust response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"workspace_trust_response\""))
        },
    );
    let runtime_pid = fixture.runtime_pid();
    let transcript = fixture.setup_transcript();
    assert_eq!(
        transcript
            .iter()
            .map(|event| event.get("fixture_event").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        [
            Some("initialize_request"),
            Some("renderer_context_request"),
            Some("renderer_context_response"),
            Some("workspace_trust_request"),
            Some("workspace_trust_response"),
        ],
        "rejection must not emit a second trust exchange or an initialize response"
    );
    assert_fixture_pid(&transcript, runtime_pid);
    let context_request = assert_setup_request(
        &transcript[1],
        "renderer_context_request",
        "crabcode-tui-fixture-renderer-context",
        "renderer_context",
    );
    assert_eq!(
        context_request.get("cwd").and_then(Value::as_str),
        Some(canonical_cwd_text)
    );
    assert_eq!(
        context_request
            .get("preferred_notification_channel")
            .and_then(Value::as_str),
        Some("auto")
    );
    assert_eq!(
        context_request
            .get("message_idle_notification_threshold_ms")
            .and_then(Value::as_u64),
        Some(0)
    );
    let trust_request = assert_setup_request(
        &transcript[3],
        "workspace_trust_request",
        "crabcode-tui-fixture-workspace-trust",
        "workspace_trust",
    );
    assert!(
        trust_request.get("cwd").is_none(),
        "workspace trust must reuse the cwd already bound by renderer_context"
    );
    assert_eq!(
        trust_request.as_object().map(serde_json::Map::len),
        Some(3),
        "rejected trust uses the same closed setup-base DTO"
    );
    let trust_response = assert_setup_response(
        &transcript[4],
        "workspace_trust_response",
        "crabcode-tui-fixture-workspace-trust",
        "workspace_trust",
        "reject",
    );
    assert_eq!(
        trust_response.as_object().map(serde_json::Map::len),
        Some(3)
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("poll trust-reject child") {
            assert!(
                status.success(),
                "workspace-trust escape must be a clean user-requested exit: {status:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("trust-reject TUI did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after trust rejection"),
        shell_termios,
        "rejection must restore the complete pre-prompt termios snapshot"
    );

    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join trust-reject PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let rendered = rendered_terminal_text(&output);
    assert!(!rendered.contains("协议兼容性失败"), "{output:?}");
    assert!(
        !rendered.contains("runtime exited unexpectedly"),
        "{output:?}"
    );
    // The welcome-card copy is present while the composer is locked and is
    // not a protocol-ready marker. The transcript assertion above is the
    // authoritative proof that no initialize response crossed this path.
    assert_eq!(occurrences(&output, ENTER_ALTERNATE_SCREEN), 1);
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
    assert_eq!(occurrences(&output, ENABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, ENABLE_MOUSE_CAPTURE), 1);
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn fullscreen_resize_suspend_resume_and_signal_exit_restore_terminal_protocol() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready direct runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before fullscreen ownership");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui in PTY");
    drop(pair.slave);

    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master.try_clone_reader().expect("clone PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN)
            && output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains(HIDE_CURSOR)
            && output.contains("CrabCode")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read fullscreen-owned termios"),
        shell_termios,
        "fullscreen ownership must place the PTY in raw mode"
    );
    wait_until_condition(
        Duration::from_secs(5),
        "trusted renderer initialize response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );
    let setup = fixture.setup_transcript();
    assert_eq!(
        setup
            .iter()
            .map(|event| event.get("fixture_event").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        [
            Some("initialize_request"),
            Some("renderer_context_request"),
            Some("renderer_context_response"),
            Some("initialize_response"),
        ],
        "an already-trusted workspace has renderer_context first among setup exchanges and no workspace-trust wire"
    );
    assert!(
        setup.iter().all(|event| {
            event
                .pointer("/request/request/kind")
                .and_then(Value::as_str)
                != Some("workspace_trust")
        }),
        "trusted startup must not expand the renderer protocol with a trust message"
    );

    let before_resize = captured.lock().expect("capture lock").len();
    for index in 0_u16..96 {
        pair.master
            .resize(PtySize {
                rows: 8 + (index.wrapping_mul(7) % 45),
                cols: 20 + (index.wrapping_mul(17) % 141),
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize fullscreen PTY during storm");
    }
    pair.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("set final fullscreen PTY size");
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.len() > before_resize && occurrences(output, "CrabCode") >= 2
    });

    let pid = child.process_id().expect("PTY child pid");
    kill(Pid::from_raw(pid as i32), Signal::SIGTSTP).expect("send SIGTSTP");
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(LEAVE_ALTERNATE_SCREEN)
            && output.contains(DISABLE_BRACKETED_PASTE)
            && output.contains(SHOW_CURSOR)
    });
    wait_until_condition(
        Duration::from_secs(5),
        "suspend termios restoration",
        || pair.master.get_termios().as_ref() == Some(&shell_termios),
    );

    std::thread::sleep(Duration::from_millis(100));
    kill(Pid::from_raw(pid as i32), Signal::SIGCONT).expect("send SIGCONT");
    wait_until(&captured, Duration::from_secs(5), |output| {
        occurrences(output, ENTER_ALTERNATE_SCREEN) >= 2
            && occurrences(output, ENABLE_BRACKETED_PASTE) >= 2
    });
    wait_until_condition(Duration::from_secs(5), "resumed raw mode", || {
        pair.master
            .get_termios()
            .is_some_and(|current| current != shell_termios)
    });

    kill(Pid::from_raw(pid as i32), Signal::SIGINT).expect("send SIGINT");
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll PTY child") {
            assert!(status.success(), "signal-handled TUI failed: {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("crabcode-tui did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read fullscreen termios after signal exit"),
        shell_termios,
        "signal exit must restore the complete termios snapshot"
    );
    fixture.finish();
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 2);
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 2);
    assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 2);
    assert!(
        occurrences(&output, SHOW_CURSOR) >= 2,
        "cursor must be restored on suspend and final teardown; render-time cursor placement may add frames"
    );
    assert_eq!(
        occurrences(&output, ENABLE_MOUSE_CAPTURE),
        2,
        "fullscreen setup and post-suspend resume must both enable mouse capture"
    );
    assert_eq!(
        occurrences(&output, MOUSE_PASTE_RESET),
        2,
        "suspend and final teardown must both reset mouse tracking and bracketed paste"
    );
}

struct ExternalResumeCutoverCase {
    label: &'static str,
    buffered_prefix: &'static [u8],
    fresh_prompt: &'static str,
    exercise_writer_frontier: bool,
}

impl ExternalResumeCutoverCase {
    const fn label(&self) -> &str {
        self.label
    }

    const fn buffered_prefix(&self) -> &[u8] {
        self.buffered_prefix
    }

    const fn fresh_prompt(&self) -> &str {
        self.fresh_prompt
    }

    const fn exercise_writer_frontier(&self) -> bool {
        self.exercise_writer_frontier
    }
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn external_sigstop_resume_discards_pre_cutover_tty_input() {
    for case in [
        ExternalResumeCutoverCase {
            label: "complete-event",
            buffered_prefix: b"zstale-complete-event",
            fresh_prompt: "fresh-complete-event",
            exercise_writer_frontier: true,
        },
        ExternalResumeCutoverCase {
            label: "partial-csi",
            buffered_prefix: b"z\x1b[",
            fresh_prompt: "fresh-after-partial-csi",
            exercise_writer_frontier: false,
        },
        ExternalResumeCutoverCase {
            label: "split-utf8",
            buffered_prefix: &[b'z', 0xe7, 0x95],
            fresh_prompt: "fresh-after-split-utf8",
            exercise_writer_frontier: false,
        },
        ExternalResumeCutoverCase {
            label: "open-bracketed-paste",
            buffered_prefix: b"z\x1b[200~stale-open-paste",
            fresh_prompt: "fresh-after-open-paste",
            exercise_writer_frontier: false,
        },
    ] {
        assert_external_resume_cutover(&case);
    }
}

fn assert_external_resume_cutover(case: &ExternalResumeCutoverCase) {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready direct runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open external-SIGSTOP PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before external SIGSTOP");
    let slave_control = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            pair.master
                .tty_name()
                .expect("external-SIGSTOP PTY slave path"),
        )
        .expect("open external-SIGSTOP PTY slave for termios control");
    let shell_termios_for_set = nix::sys::termios::tcgetattr(&slave_control)
        .expect("read shell termios through project nix version");
    let mut expected_raw_termios = shell_termios_for_set.clone();
    nix::sys::termios::cfmakeraw(&mut expected_raw_termios);
    let input_buffer_arm = fixture.directory.join("input-buffer.arm");
    let input_buffer_ready = fixture.directory.join("input-buffer.ready");
    let input_buffer_release = fixture.directory.join("input-buffer.release");
    let input_park_requested = fixture.directory.join("input-park-requested.ready");
    let input_event_log = fixture.directory.join("input-events.log");
    let writer_frame_arm = fixture.directory.join("writer-frame.arm");
    let writer_frame_ready = fixture.directory.join("writer-frame.ready");
    let writer_frame_release = fixture.directory.join("writer-frame.release");
    let resume_cutover_ready = fixture.directory.join("resume-cutover.ready");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    command.env(
        "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_ARM_FILE",
        &input_buffer_arm,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_READY_FILE",
        &input_buffer_ready,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_RELEASE_FILE",
        &input_buffer_release,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_INPUT_PARK_REQUESTED_FILE",
        &input_park_requested,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_INPUT_EVENT_LOG_FILE",
        &input_event_log,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_ARM_FILE",
        &writer_frame_arm,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_READY_FILE",
        &writer_frame_ready,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_RELEASE_FILE",
        &writer_frame_release,
    );
    command.env(
        "CRABCODE_TUI_TEST_ONLY_RESUME_CUTOVER_READY_FILE",
        &resume_cutover_ready,
    );
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn external-SIGSTOP TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take external-SIGSTOP PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone external-SIGSTOP PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN)
            && output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains("CrabCode")
    });
    wait_until_condition(
        Duration::from_secs(5),
        "external-SIGSTOP renderer initialize response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );

    if case.exercise_writer_frontier() {
        std::fs::write(&writer_frame_arm, b"armed").expect("arm accepted-writer-frame barrier");
        pair.master
            .resize(PtySize {
                rows: 25,
                cols: 81,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("trigger a synchronized frame behind the writer barrier");
        wait_until_condition(
            Duration::from_secs(5),
            "accepted writer frame barrier",
            || writer_frame_ready.is_file(),
        );
    }

    std::fs::write(&input_buffer_arm, b"armed").expect("arm crossterm reader-state barrier");
    {
        let mut writer = writer.lock().expect("external-SIGSTOP writer lock");
        writer
            .write_all(case.buffered_prefix())
            .expect("write old-generation crossterm parser prefix");
        writer
            .flush()
            .expect("flush old-generation crossterm parser prefix");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "buffered crossterm reader state",
        || input_buffer_ready.is_file(),
    );

    let pid = Pid::from_raw(child.process_id().expect("PTY child pid") as i32);
    kill(pid, Signal::SIGSTOP).expect("send external SIGSTOP");
    wait_until_condition(
        Duration::from_secs(5),
        "external SIGSTOP process state",
        || {
            matches!(
                nix::sys::wait::waitpid(
                    pid,
                    Some(
                        nix::sys::wait::WaitPidFlag::WUNTRACED
                            | nix::sys::wait::WaitPidFlag::WNOHANG
                    )
                ),
                Ok(nix::sys::wait::WaitStatus::Stopped(_, Signal::SIGSTOP))
            )
        },
    );
    nix::sys::termios::tcsetattr(
        &slave_control,
        nix::sys::termios::SetArg::TCSANOW,
        &shell_termios_for_set,
    )
    .expect("simulate an external owner restoring cooked shell termios");
    pair.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize the stopped terminal generation");
    {
        let mut writer = writer.lock().expect("external-SIGSTOP writer lock");
        writer
            .write_all(b"stale-tty-before-cutover")
            .expect("queue tty input while process is externally stopped");
        writer
            .flush()
            .expect("flush tty input while process is externally stopped");
    }

    let resume_output_offset = captured.lock().expect("capture lock").len();
    kill(pid, Signal::SIGCONT).expect("send external SIGCONT");
    wait_until_condition(
        Duration::from_secs(5),
        "external resume reader park request",
        || input_park_requested.is_file(),
    );
    std::fs::write(&input_buffer_release, b"released")
        .expect("release reader into the requested park");
    if case.exercise_writer_frontier() {
        std::fs::write(&writer_frame_release, b"released")
            .expect("release accepted old-generation writer frame");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "complete external resume generation cutover",
        || resume_cutover_ready.is_file(),
    );
    assert_eq!(
        nix::sys::termios::tcgetattr(&slave_control)
            .expect("read exact termios after external resume"),
        expected_raw_termios,
        "external resume must derive and reassert the exact raw termios from the saved pre-raw baseline"
    );
    wait_until(&captured, Duration::from_secs(5), |_output| {
        let bytes = captured.lock().expect("capture lock");
        let tail = String::from_utf8_lossy(bytes.get(resume_output_offset..).unwrap_or_default());
        let Some(heal) = tail.find(ENTER_ALTERNATE_SCREEN) else {
            return false;
        };
        let after_heal = &tail[heal + ENTER_ALTERNATE_SCREEN.len()..];
        after_heal
            .find(BEGIN_SYNCHRONIZED_UPDATE)
            .is_some_and(|begin| {
                after_heal[begin + BEGIN_SYNCHRONIZED_UPDATE.len()..]
                    .contains(END_SYNCHRONIZED_UPDATE)
            })
    });
    {
        let bytes = captured.lock().expect("capture lock");
        let tail = String::from_utf8_lossy(
            bytes
                .get(resume_output_offset..)
                .expect("resume output offset remains inside the byte capture"),
        );
        let heal = tail
            .find(ENTER_ALTERNATE_SCREEN)
            .expect("external resume reasserts alternate-screen ownership");
        let first_healed_frame = tail[heal..]
            .find(BEGIN_SYNCHRONIZED_UPDATE)
            .map(|offset| heal + offset)
            .expect("external resume emits a synchronized redraw");
        for (mode, sequence) in [
            ("focus", ENABLE_FOCUS_CHANGE),
            ("bracketed paste", ENABLE_BRACKETED_PASTE),
            ("mouse capture", ENABLE_MOUSE_CAPTURE),
        ] {
            let mode_offset = tail[heal..]
                .find(sequence)
                .map(|offset| heal + offset)
                .unwrap_or_else(|| {
                    panic!(
                        "external resume must reassert {mode} terminal mode for {}",
                        case.label()
                    )
                });
            assert!(
                mode_offset < first_healed_frame,
                "external resume must reassert {mode} before its first synchronized redraw for {}",
                case.label()
            );
        }
        if case.exercise_writer_frontier() {
            let old_frame_end = tail[..heal]
                .rfind(END_SYNCHRONIZED_UPDATE)
                .expect("the accepted old-generation frame must finish");
            assert!(
                old_frame_end < heal,
                "the accepted writer frontier must drain before protocol healing"
            );
        }
    }

    {
        let mut writer = writer.lock().expect("external-SIGSTOP writer lock");
        writer
            .write_all(case.fresh_prompt().as_bytes())
            .expect("write new-generation prompt");
        writer
            .write_all(b"\r")
            .expect("submit new-generation prompt");
        writer.flush().expect("flush new-generation prompt");
    }
    let user_message_deadline = Instant::now() + Duration::from_secs(5);
    while !fixture
        .users_path
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 0)
    {
        if let Some(status) = child
            .try_wait()
            .expect("poll child while waiting for fresh prompt")
        {
            let output = {
                let bytes = captured.lock().expect("capture lock");
                String::from_utf8_lossy(&bytes).into_owned()
            };
            panic!(
                "external-resume child exited before fresh prompt for {}: {status:?}; terminal={output:?}; setup={:?}; input_events={:?}",
                case.label(),
                std::fs::read_to_string(&fixture.setup_path),
                std::fs::read_to_string(&input_event_log)
            );
        }
        assert!(
            Instant::now() < user_message_deadline,
            "fresh prompt was not forwarded after external resume for {}; terminal={:?}; setup={:?}; input_events={:?}",
            case.label(),
            {
                let bytes = captured.lock().expect("capture lock");
                String::from_utf8_lossy(&bytes).into_owned()
            },
            std::fs::read_to_string(&fixture.setup_path),
            std::fs::read_to_string(&input_event_log)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let user_messages = fixture.user_messages();
    assert_eq!(
        user_messages.len(),
        1,
        "the old terminal generation must not produce its own request for {}",
        case.label()
    );
    let user_message: Value =
        serde_json::from_str(&user_messages[0]).expect("parse direct-runtime user envelope");
    assert_eq!(
        user_message
            .pointer("/message/content")
            .and_then(Value::as_str),
        Some(case.fresh_prompt()),
        "complete events, partial decoder prefixes, channel input, or tty bytes crossed the generation boundary for {}: {user_messages:?}",
        case.label()
    );
    assert!(
        !user_messages[0].contains("stale"),
        "old-generation bytes leaked into the composer for {}",
        case.label()
    );

    kill(pid, Signal::SIGINT).expect("send external-SIGSTOP SIGINT");
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll external-SIGSTOP PTY child") {
            assert!(status.success(), "external-SIGSTOP TUI failed: {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("external-SIGSTOP TUI did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after external-SIGSTOP exit"),
        shell_termios,
        "external-SIGSTOP lifecycle must restore the original termios"
    );
    fixture.finish();
    drop(writer);
    drop(slave_control);
    drop(pair.master);
    reader_thread
        .join()
        .expect("join external-SIGSTOP PTY reader");
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn external_editor_child_handoff_preserves_live_terminal_generation() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready direct runtime");
    let editor_path = fixture.directory.join("editor.sh");
    let editor_active_path = fixture.directory.join("editor.active");
    std::fs::write(
        &editor_path,
        r#"#!/bin/sh
set -eu
: > "$CRABCODE_TUI_EDITOR_ACTIVE"
printf 'CHILD_HANDOFF_MARKER\n'
sleep 1
printf 'edited-by-pty' > "$1"
"#,
    )
    .expect("write external-editor fixture");
    let mut editor_permissions = std::fs::metadata(&editor_path)
        .expect("read external-editor fixture metadata")
        .permissions();
    editor_permissions.set_mode(0o700);
    std::fs::set_permissions(&editor_path, editor_permissions)
        .expect("make external-editor fixture executable");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open external-editor PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before external-editor lifecycle");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    command.env_remove("VISUAL");
    command.env("EDITOR", &editor_path);
    command.env("CRABCODE_TUI_EDITOR_ACTIVE", &editor_active_path);
    // A real cold-start process sample found CoreGraphics permanently blocked
    // on this first Ctrl-G. Reproduce that macOS failure deterministically:
    // the sole modifier worker may remain blocked, but the 16ms TUI budget
    // must expire, retire the side channel, and let the same key reach the
    // external-editor handoff.
    #[cfg(target_os = "macos")]
    command.env(TEST_ONLY_BLOCK_PHYSICAL_MODIFIER_PROBE, "1");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn external-editor TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take external-editor PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone external-editor PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN)
            && output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains("CrabCode")
    });
    wait_until_condition(
        Duration::from_secs(5),
        "external-editor renderer initialize response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read external-editor TUI raw termios"),
        shell_termios,
        "the parent TUI must own raw mode before handing the tty to the child"
    );

    {
        let mut writer = writer.lock().expect("external-editor PTY writer lock");
        writer
            .write_all(&[0x07])
            .expect("write historical Ctrl-G editor shortcut");
        writer
            .flush()
            .expect("flush historical Ctrl-G editor shortcut");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "external-editor child ownership marker",
        || editor_active_path.is_file(),
    );
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios while external editor owns the tty"),
        shell_termios,
        "the blocking child must inherit the restored shell termios"
    );

    wait_until(&captured, Duration::from_secs(7), |output| {
        output.contains("CHILD_HANDOFF_MARKER")
            && output.contains("edited-by-pty")
            && occurrences(output, ENTER_ALTERNATE_SCREEN) == 2
    });
    wait_until_condition(
        Duration::from_secs(5),
        "raw-mode reacquisition after external editor",
        || {
            pair.master
                .get_termios()
                .is_some_and(|current| current != shell_termios)
        },
    );
    {
        let bytes = captured.lock().expect("capture lock");
        let output = String::from_utf8_lossy(&bytes);
        assert_eq!(
            occurrences(&output, ENTER_ALTERNATE_SCREEN),
            2,
            "startup and same-generation child reacquisition enter fullscreen exactly once each"
        );
        assert_eq!(
            occurrences(&output, LEAVE_ALTERNATE_SCREEN),
            1,
            "only the child handoff has left fullscreen before final shutdown"
        );
        assert_eq!(
            occurrences(&output, ENABLE_BRACKETED_PASTE),
            1,
            "child reacquisition must not construct a second terminal generation"
        );
        assert_eq!(
            occurrences(&output, ENABLE_MOUSE_CAPTURE),
            1,
            "child reacquisition must retain the existing mouse-capture generation"
        );
    }

    {
        let mut writer = writer.lock().expect("external-editor PTY writer lock");
        writer
            .write_all(&[0x03, 0x03])
            .expect("write external-editor exit keys");
        writer.flush().expect("flush external-editor exit keys");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll external-editor TUI") {
            assert!(status.success(), "external-editor TUI failed: {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("external-editor TUI did not exit after two Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after external-editor TUI exit"),
        shell_termios,
        "final teardown must restore the original termios snapshot"
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread
        .join()
        .expect("join external-editor PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(occurrences(&output, ENTER_ALTERNATE_SCREEN), 2);
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 2);
    assert_eq!(
        occurrences(&output, ENABLE_BRACKETED_PASTE),
        1,
        "the external editor must not replace the terminal/writer generation"
    );
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, ENABLE_MOUSE_CAPTURE), 1);
    assert_eq!(occurrences(&output, MOUSE_PASTE_RESET), 1);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn missing_custom_keybinding_authority_cannot_enable_quick_search_or_file_editor() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready direct runtime");
    // This is deliberately a valid historical-looking user binding. The
    // direct Rust renderer has no production authority for user keybindings,
    // GrowthBook or QUICK_SEARCH, so merely placing the file and feature
    // variables in the child environment must not change its fixed defaults.
    std::fs::write(
        fixture.directory.join("keybindings.json"),
        r#"{"bindings":[{"context":"Global","bindings":{"ctrl+shift+p":"app:quickOpen"}}]}"#,
    )
    .expect("write ignored quick-open binding fixture");
    let editor_path = fixture.directory.join("file-editor.sh");
    let editor_active_path = fixture.directory.join("file-editor.active");
    std::fs::write(
        &editor_path,
        r#"#!/bin/sh
set -eu
: > "$CRABCODE_TUI_FILE_EDITOR_ACTIVE"
printf 'UNEXPECTED_FILE_EDITOR_LAUNCH\n'
"#,
    )
    .expect("write file-editor fixture");
    let mut editor_permissions = std::fs::metadata(&editor_path)
        .expect("read file-editor fixture metadata")
        .permissions();
    editor_permissions.set_mode(0o700);
    std::fs::set_permissions(&editor_path, editor_permissions)
        .expect("make file-editor fixture executable");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open file-editor PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before file-editor lifecycle");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    // These values are poison fixtures, not supported renderer inputs. They
    // prove the pure TUI does not silently duplicate the TypeScript
    // configuration/GrowthBook authority.
    command.env("CRABCODE_CONFIG_DIR", &fixture.directory);
    command.env("USER_TYPE", "ant");
    command.env(
        "CRABCODE_INTERNAL_FC_OVERRIDES",
        r#"{"tengu_keybinding_customization_release":true}"#,
    );
    command.env_remove("VISUAL");
    command.env("EDITOR", &editor_path);
    command.env("CRABCODE_TUI_FILE_EDITOR_ACTIVE", &editor_active_path);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn file-editor TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take file-editor PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone file-editor PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN)
            && output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains("直连运行环境已初始化")
    });

    {
        let mut writer = writer.lock().expect("file-editor PTY writer lock");
        // Kitty CSI-u modifier 6 is Ctrl+Shift. The fixed terminal lifecycle
        // enables disambiguated key reporting before the input reader starts.
        // The following ordinary text is a processing barrier: seeing it in
        // the composer proves the preceding unbound chord was consumed before
        // the negative assertions run.
        writer
            .write_all(b"\x1b[112;6ufailclosed")
            .expect("write fail-closed Ctrl-Shift-P probe and processing barrier");
        writer.flush().expect("flush fail-closed quick-open probe");
    }
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("failclosed")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read termios after ignored quick-open chord"),
        shell_termios,
        "an ignored feature-owned chord must not relinquish terminal ownership"
    );
    let visible = {
        let bytes = captured.lock().expect("capture lock");
        searchable_terminal_output(&String::from_utf8_lossy(&bytes))
    };
    assert!(
        !visible.contains("Quick Open"),
        "QUICK_SEARCH=false and missing renderer authority must keep quick open closed: {visible:?}"
    );
    assert!(
        !editor_active_path.exists(),
        "ignored keybindings/config inputs must not launch the file editor"
    );
    {
        let mut writer = writer.lock().expect("file-editor PTY writer lock");
        writer
            .write_all(&[0x03, 0x03])
            .expect("write file-editor exit keys");
        writer.flush().expect("flush file-editor exit keys");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll file-editor TUI") {
            assert!(
                status.success(),
                "fail-closed quick-search TUI failed: {status:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("fail-closed quick-search TUI did not exit after two Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after file-editor TUI exit"),
        shell_termios
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join file-editor PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(occurrences(&output, ENTER_ALTERNATE_SCREEN), 1);
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
    assert_eq!(occurrences(&output, ENABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn no_alt_screen_mode_preserves_main_screen_and_forwards_runtime_arguments() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready direct runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before inline ownership");
    let mut command = base_command("no-alt-screen");
    fixture.configure(&mut command);
    command.env("CRABCODE_TEAMMATE_COMMAND", "/host/legacy-renderer");
    command.env("CRABCODE_DESKTOP_AUTOMATION", "1");
    command.env("CRABCODE_DESKTOP_AUTOMATION_WRITES", "1");
    command.env("CRABCODE_DESKTOP_CAPTURE", "1");
    command.env("CRABCODE_DESKTOP_VISUAL_SIDECAR", "1");
    // Place the positional prompt before Commander variadic options. Values
    // following --mcp-config/--add-dir are, by the unchanged backend grammar,
    // members of those options until the next flag.
    command.arg("explain this");
    command.arg("--model");
    command.arg("best");
    command.arg("--mcp-config");
    command.arg("a.json");
    command.arg("--add-dir");
    command.arg("/tmp/extra");
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui in PTY");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master.try_clone_reader().expect("clone PTY reader"),
        Arc::clone(&captured),
        Some(Arc::clone(&writer)),
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains(ENABLE_FOCUS_CHANGE)
            && output.contains(HIDE_CURSOR)
            && output.contains("CrabCode")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read inline-owned termios"),
        shell_termios
    );
    {
        let output = fixture.forwarded_args();
        let expected = [
            "--model",
            "best",
            "--mcp-config",
            "a.json",
            "--add-dir",
            "/tmp/extra",
        ];
        for pair in expected.windows(2) {
            assert!(
                output.windows(2).any(|actual| actual == pair),
                "missing forwarded pair {pair:?} in {output:?}"
            );
        }
        assert!(!output.iter().any(|argument| argument == "--"));
        assert!(!output.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--fullscreen" | "--no-alt-screen" | "--minimal"
            )
        }));
        assert!(
            !output.iter().any(|argument| argument == "explain this"),
            "the positional prompt must not be passed to the private print child"
        );
    }
    let native_tui = std::fs::canonicalize(env!("CARGO_BIN_EXE_crabcode-tui"))
        .expect("canonical native TUI test binary");
    assert_eq!(
        fixture.runtime_environment(),
        format!(
            "CRABCODE_TEAMMATE_COMMAND={}\n\
             CRABCODE_DESKTOP_AUTOMATION=0\n\
             CRABCODE_DESKTOP_AUTOMATION_WRITES=\n\
             CRABCODE_DESKTOP_CAPTURE=\n\
             CRABCODE_DESKTOP_VISUAL_SIDECAR=\n",
            native_tui.display()
        ),
        "the pure TUI child receives the canonical teammate command and cannot inherit GUI-owned desktop authority"
    );
    assert!(
        fixture
            .user_messages()
            .iter()
            .any(|message| message.contains("explain this")),
        "the positional prompt must be injected after the TUI session becomes ready"
    );
    {
        let bytes = captured.lock().expect("capture lock");
        let output = String::from_utf8_lossy(&bytes);
        assert!(!output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
    }

    let before_resize = captured.lock().expect("capture lock").len();
    pair.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize PTY");
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.len() > before_resize && occurrences(output, "CrabCode") >= 2
    });
    {
        let mut writer = writer.lock().expect("PTY writer lock");
        writer
            .write_all(&[0x03, 0x03])
            .expect("write two raw Ctrl-C key events");
        writer.flush().expect("flush two raw Ctrl-C key events");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if child.try_wait().expect("poll PTY child").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("crabcode-tui did not exit after two raw Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read inline termios after double Ctrl-C"),
        shell_termios
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert!(!output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
    assert!(!output.contains(LEAVE_ALTERNATE_SCREEN), "{output:?}");
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 1);
    assert_eq!(
        occurrences(&output, SHOW_CURSOR),
        2,
        "the composer visibility transition and final teardown each show once; repaint/resize must not reset cursor blink"
    );
    assert_eq!(
        occurrences(&output, ENABLE_MOUSE_CAPTURE),
        1,
        "inline terminal ownership must enable mouse capture"
    );
    assert_eq!(
        occurrences(&output, MOUSE_PASTE_RESET),
        1,
        "inline teardown must reset mouse tracking and bracketed paste"
    );
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn direct_runtime_crash_fail_closes_and_terminal_stays_recoverable() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Crash).expect("create crashing runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let shell_termios = pair.master.get_termios().expect("read shell termios");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui in PTY");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master.try_clone_reader().expect("clone PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN) && output.contains("CrabCode")
    });
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("协议兼容性失败") && output.contains("runtime exited unexpectedly")
    });
    assert!(
        child.try_wait().expect("poll fail-closed TUI").is_none(),
        "TUI exited before presenting the fail-closed state"
    );

    {
        let mut writer = writer.lock().expect("PTY writer lock");
        writer
            .write_all(b"x\r")
            .expect("attempt input after direct runtime crash");
        writer.flush().expect("flush disabled input attempt");
        writer
            .write_all(&[0x03, 0x03])
            .expect("write two raw Ctrl-C key events");
        writer.flush().expect("flush exit keys");
    }
    // The runtime has already failed, so the final best-effort end_session can
    // consume the independent 5s transport deadline after the terminal
    // writer's 2s bounded drain. Keep scheduling margin beyond that exact
    // product-budget sum; this does not relax either production timeout.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("poll PTY child") {
            assert!(
                !status.success(),
                "an unexpected backend exit must remain a failed process status: {status:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            let output = {
                let bytes = captured.lock().expect("capture lock");
                String::from_utf8_lossy(&bytes).into_owned()
            };
            panic!(
                "fail-closed crabcode-tui did not exit; shutdown_acknowledged={}; output={output:?}",
                fixture.marker_path.is_file()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after runtime crash exit"),
        shell_termios
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 1);
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
}

#[cfg(feature = "terminal-lifecycle-tests")]
#[test]
#[serial_test::serial(terminal_lifecycle)]
fn error_and_panic_after_raw_mode_restore_terminal_once() {
    for fault in ["error", "panic"] {
        let canonical_cwd = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .expect("canonical test cwd");
        // The injected fault occurs after the existing initialize ->
        // renderer_context prefix but before the later setup/initialize
        // response handoff, so end_session is still illegal even though both
        // paths must reap the child.
        let fixture =
            MockDirectRuntime::start(MockMode::ForceKilled).expect("create fault runtime");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let shell_termios = pair.master.get_termios().expect("read shell termios");
        let mut command = base_command("fullscreen");
        fixture.configure(&mut command);
        command.env(TEST_ONLY_FAULT_AFTER_RAW, fault);
        command.env(TEST_ONLY_FAULT_READY_FILE, &fixture.pid_path);
        command.cwd(&canonical_cwd);
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn fault-injected TUI");
        drop(pair.slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let reader_thread = spawn_reader(
            pair.master.try_clone_reader().expect("clone PTY reader"),
            Arc::clone(&captured),
            None,
        );
        // A detached PTY can make the terminal writer consume its documented
        // 2s bounded write/admission deadline before RuntimeHost begins its
        // independent 5s bounded end_session shutdown. Seven seconds is
        // therefore the sum of the two product budgets, not a valid outer
        // test deadline once process scheduling and reap are included.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().expect("poll fault-injected TUI") {
                assert!(!status.success(), "{fault} injection must fail");
                break;
            }
            if Instant::now() >= deadline {
                let _kill_result = child.kill();
                panic!("crabcode-tui did not exit after test-only {fault}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            pair.master
                .get_termios()
                .expect("read termios after injected fault"),
            shell_termios
        );
        fixture.finish();
        drop(pair.master);
        reader_thread.join().expect("join fault PTY reader");
        let output = {
            let bytes = captured.lock().expect("capture lock");
            String::from_utf8_lossy(&bytes).into_owned()
        };
        assert!(output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
        assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
        assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
        assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 1);
        assert_eq!(occurrences(&output, SHOW_CURSOR), 1);
        assert!(
            output.contains(&format!(
                "test-only injected {fault} after terminal ownership"
            )),
            "{output:?}"
        );
        if fault == "panic" {
            let teardown_at = output
                .find(LEAVE_ALTERNATE_SCREEN)
                .expect("panic output contains terminal teardown");
            let diagnostic_at = output
                .find("test-only injected panic after terminal ownership")
                .expect("panic output contains previous-hook diagnostic");
            assert!(
                teardown_at < diagnostic_at,
                "fixed lifecycle must restore the terminal before the previous panic hook writes: {output:?}"
            );
        }
    }
}

#[cfg(feature = "terminal-lifecycle-tests")]
#[test]
#[serial_test::serial(terminal_lifecycle)]
fn fatal_memory_signals_restore_terminal_and_preserve_signal_disposition() {
    for (fault, signal) in [("sigbus", libc::SIGBUS), ("sigsegv", libc::SIGSEGV)] {
        let canonical_cwd = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .expect("canonical test cwd");
        let fixture =
            MockDirectRuntime::start(MockMode::ForceKilled).expect("create fatal-fault runtime");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open fatal-fault PTY");
        let shell_termios = pair
            .master
            .get_termios()
            .expect("read shell termios before fatal fault");
        let mut command = base_command("fullscreen");
        fixture.configure(&mut command);
        command.env(TEST_ONLY_FAULT_AFTER_RAW, fault);
        command.env(TEST_ONLY_FAULT_READY_FILE, &fixture.pid_path);
        command.cwd(&canonical_cwd);
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn fatal-fault TUI");
        drop(pair.slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let reader_thread = spawn_reader(
            pair.master
                .try_clone_reader()
                .expect("clone fatal-fault PTY reader"),
            Arc::clone(&captured),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll fatal-fault TUI") {
                break status;
            }
            if Instant::now() >= deadline {
                let _kill_result = child.kill();
                panic!("crabcode-tui did not terminate after test-only {fault}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success(), "{fault} must preserve a failed status");
        let expected_signal = unsafe {
            let description = libc::strsignal(signal);
            assert!(!description.is_null(), "strsignal({signal})");
            std::ffi::CStr::from_ptr(description)
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(
            status.signal(),
            Some(expected_signal.as_str()),
            "{fault} must restore the default disposition and re-raise the same signal"
        );
        assert_eq!(
            pair.master
                .get_termios()
                .expect("read termios after fatal fault"),
            shell_termios,
            "{fault} must restore the complete pre-TUI termios snapshot"
        );

        fixture.finish();
        drop(pair.master);
        reader_thread.join().expect("join fatal-fault PTY reader");
        let output = {
            let bytes = captured.lock().expect("fatal-fault capture lock");
            String::from_utf8_lossy(&bytes).into_owned()
        };
        assert!(output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
        assert_eq!(
            occurrences(&output, FATAL_FAULT_TERMINAL_RESTORE),
            1,
            "{fault} must emit the canonical terminal-only fatal restore exactly once"
        );
        let restore_at = output
            .find(FATAL_FAULT_TERMINAL_RESTORE)
            .expect("canonical fatal restore offset");
        let restore = &output[restore_at..restore_at + FATAL_FAULT_TERMINAL_RESTORE.len()];
        assert!(
            restore.find(END_SYNCHRONIZED_UPDATE).unwrap() < restore.find(SHOW_CURSOR).unwrap()
                && restore.find(SHOW_CURSOR).unwrap()
                    < restore.find(LEAVE_ALTERNATE_SCREEN).unwrap(),
            "fatal restore ordering must end synchronization, show the cursor, then leave the alternate screen"
        );
    }
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn second_signal_restores_and_reaps_when_the_ui_thread_is_blocked() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture =
        MockDirectRuntime::start(MockMode::ForceKilled).expect("create force-killed runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let shell_termios = pair.master.get_termios().expect("read shell termios");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.env(TEST_ONLY_FAULT_AFTER_RAW, "block");
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn blocked TUI");
    drop(pair.slave);

    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master.try_clone_reader().expect("clone PTY reader"),
        Arc::clone(&captured),
        None,
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENTER_ALTERNATE_SCREEN)
            && output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains(HIDE_CURSOR)
    });
    let runtime_pid = fixture.runtime_pid();
    let tui_pid = child.process_id().expect("blocked TUI pid");

    kill(Pid::from_raw(tui_pid as i32), Signal::SIGINT).expect("send graceful SIGINT");
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        child.try_wait().expect("poll after first signal").is_none(),
        "the first signal must remain a graceful request even when the UI thread is blocked"
    );

    kill(Pid::from_raw(tui_pid as i32), Signal::SIGINT).expect("send forcing SIGINT");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll blocked TUI") {
            break status;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("blocked TUI did not force-exit after the second signal");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !status.success(),
        "forced second-signal exit must preserve a non-success signal status"
    );
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after forced exit"),
        shell_termios,
        "the force path must restore the complete shell termios snapshot"
    );
    wait_until_condition(
        Duration::from_secs(5),
        "force-killed direct runtime process reap",
        || {
            matches!(
                kill(Pid::from_raw(runtime_pid as i32), None),
                Err(Errno::ESRCH)
            )
        },
    );

    fixture.finish();
    drop(pair.master);
    reader_thread.join().expect("join blocked TUI reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(occurrences(&output, LEAVE_ALTERNATE_SCREEN), 1);
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 1);
    assert_eq!(occurrences(&output, SHOW_CURSOR), 1);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn closing_the_pty_master_reaps_the_direct_runtime_child() {
    for attempt in 1..=3 {
        let canonical_cwd = std::env::current_dir()
            .and_then(std::fs::canonicalize)
            .expect("canonical test cwd");
        let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready runtime");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY");
        let tty_name = pair.master.tty_name().expect("PTY slave path");
        let slave_monitor = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tty_name)
            .expect("open independent slave termios monitor");
        let shell_termios =
            nix::sys::termios::tcgetattr(&slave_monitor).expect("read initial slave termios");

        let mut command = base_command("fullscreen");
        fixture.configure(&mut command);
        command.cwd(&canonical_cwd);
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn crabcode-tui in PTY");
        drop(pair.slave);
        wait_until_condition(Duration::from_secs(5), "raw mode before PTY close", || {
            nix::sys::termios::tcgetattr(&slave_monitor)
                .is_ok_and(|current| current != shell_termios)
        });

        // The monitor itself is an independently open slave endpoint. Keeping
        // it alive while dropping the master does not model a detached
        // terminal on BSD/macOS and can legitimately suppress the kernel
        // hangup observed by the child. Release every out-of-process slave
        // before closing the sole master.
        drop(slave_monitor);
        drop(pair.master);
        // Product teardown has two independent, sequential upper bounds:
        // terminal output cancellation is 2s and direct-runtime shutdown is
        // 5s. A 7s test deadline equals the implementation budget exactly and
        // can expire before the final wait/reap observation. Keep an explicit
        // outer scheduling margin without changing either product timeout.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child
                .try_wait()
                .expect("poll direct TUI child after PTY close")
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                let _kill_result = child.kill();
                panic!(
                    "attempt {attempt}: direct crabcode-tui child survived PTY master close \
                     (runtime shutdown acknowledged: {})",
                    fixture.marker_path.is_file()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture.finish_without_shutdown_ack();
    }
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn help_exits_before_direct_runtime_and_terminal_ownership() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let mut command = base_command("fullscreen");
    command.arg("--help");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui help in PTY");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let mut output = Vec::new();
    reader.read_to_end(&mut output).expect("read help output");
    let status = child.wait().expect("wait for help process");
    assert!(status.success(), "help process failed: {status:?}");
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("CrabCode Rust TUI"), "{output:?}");
    assert!(
        output.contains("private direct CrabCode runtime"),
        "{output:?}"
    );
    assert!(output.contains("--fullscreen"), "{output:?}");
    assert!(output.contains("--no-alt-screen"), "{output:?}");
    assert!(output.contains("--minimal"), "{output:?}");
    assert_never_entered_raw_mode(&output);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn minimal_mode_never_enters_alt_screen_and_restores_after_signal() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::Ready).expect("create ready runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before minimal ownership");
    let mut command = base_command("minimal");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn crabcode-tui in PTY");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().expect("take PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master.try_clone_reader().expect("clone PTY reader"),
        Arc::clone(&captured),
        Some(Arc::clone(&writer)),
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(ENABLE_BRACKETED_PASTE)
            && output.contains(ENABLE_FOCUS_CHANGE)
            && output.contains(HIDE_CURSOR)
            // Setup dialogs are intentionally paintable before StructuredIO
            // owns stdin. Synchronize this post-handoff signal test on the
            // authoritative SDK initialize response, not on a generic branded
            // setup frame that can race the setup router.
            && output.contains("直连运行环境已初始化")
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read minimal-owned termios"),
        shell_termios
    );
    let pid = child.process_id().expect("PTY child pid");
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("send SIGTERM");
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if child.try_wait().expect("poll PTY child").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("minimal crabcode-tui did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read minimal termios after SIGTERM"),
        shell_termios
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert!(!output.contains(ENTER_ALTERNATE_SCREEN), "{output:?}");
    assert!(!output.contains(LEAVE_ALTERNATE_SCREEN), "{output:?}");
    assert_eq!(occurrences(&output, DISABLE_BRACKETED_PASTE), 1);
    assert_eq!(occurrences(&output, DISABLE_FOCUS_CHANGE), 1);
    assert!(
        occurrences(&output, SHOW_CURSOR) >= 1,
        "minimal teardown must restore the cursor; an earlier render-time visibility transition is scheduling-dependent"
    );
    assert!(!output.contains(ENABLE_MOUSE_CAPTURE), "{output:?}");
}

fn assert_signal_during_renderer_setup(signal_count: usize) {
    assert!(
        matches!(signal_count, 1 | 2),
        "the acquisition signal fixture covers one graceful request or one repeated-force request"
    );
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture =
        MockDirectRuntime::start(MockMode::SetupBlocked).expect("create setup-blocked runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open setup-blocked PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before setup ownership");
    let acquisition_ready = fixture.directory.join("terminal-acquisition.ready");
    let acquisition_release = fixture.directory.join("terminal-acquisition.release");
    let first_termination_observed = fixture.directory.join("first-termination.observed");
    let mut command = base_command("minimal");
    fixture.configure(&mut command);
    command.env(TEST_ONLY_ACQUISITION_READY_FILE, &acquisition_ready);
    command.env(TEST_ONLY_ACQUISITION_RELEASE_FILE, &acquisition_release);
    command.env(
        TEST_ONLY_FIRST_TERMINATION_FILE,
        &first_termination_observed,
    );
    command.cwd(&canonical_cwd);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn setup-blocked TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take setup-blocked PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone setup-blocked PTY reader"),
        Arc::clone(&captured),
        Some(Arc::clone(&writer)),
    );
    wait_until_condition(
        Duration::from_secs(5),
        "raw/protocol acquisition signal barrier",
        || acquisition_ready.is_file(),
    );
    let transcript = std::fs::read_to_string(&fixture.setup_path)
        .expect("read setup transcript at acquisition barrier");
    assert!(
        transcript.contains("\"renderer_context_response\""),
        "terminal acquisition must remain after the correlated renderer-context response: {transcript:?}"
    );
    assert_ne!(
        pair.master.get_termios().expect("read setup-owned termios"),
        shell_termios,
        "renderer setup must remain inside the one native terminal lifecycle"
    );

    let pid = child.process_id().expect("setup-blocked TUI pid");
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("send setup-phase SIGTERM");
    wait_until_condition(
        Duration::from_secs(5),
        "first setup-phase SIGTERM observed by the signal owner",
        || first_termination_observed.is_file(),
    );
    assert!(
        child
            .try_wait()
            .expect("poll setup-blocked TUI behind acquisition barrier")
            .is_none(),
        "the pre-acquisition signal owner must prevent SIGTERM's default action while writer/runtime handoff remains unpublished"
    );
    if signal_count == 1 {
        std::fs::write(&acquisition_release, b"release")
            .expect("release terminal acquisition after captured SIGTERM");
    } else {
        kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
            .expect("send repeated setup-phase SIGTERM");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll setup-blocked TUI") {
            break status;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("setup-blocked crabcode-tui did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if signal_count == 1 {
        assert!(
            status.success(),
            "one setup-phase signal is a graceful frontend exit after acquisition: {status:?}"
        );
    } else {
        assert_eq!(
            status.exit_code(),
            143,
            "the repeated setup-phase SIGTERM must use the deterministic forced-exit status"
        );
        assert!(
            !acquisition_release.exists(),
            "the repeated-signal force path must not depend on the blocked UI thread"
        );
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after setup-phase SIGTERM"),
        shell_termios,
        "setup-phase signal exit must restore the original terminal"
    );
    assert!(
        !fixture.marker_path.exists(),
        "end_session is not a legal setup-router envelope before runtime handoff"
    );
    fixture.finish_without_shutdown_ack();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join setup-blocked PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(
        occurrences(&output, DISABLE_BRACKETED_PASTE),
        1,
        "setup-phase restoration must emit one bracketed-paste teardown"
    );
    assert_eq!(
        occurrences(&output, DISABLE_FOCUS_CHANGE),
        1,
        "setup-phase restoration must emit one focus teardown"
    );
    let teardown_at = output
        .find(DISABLE_FOCUS_CHANGE)
        .expect("setup-phase restoration marker");
    let after_teardown = &output[teardown_at + DISABLE_FOCUS_CHANGE.len()..];
    for forbidden_setup in [
        ENTER_ALTERNATE_SCREEN,
        ENABLE_MOUSE_CAPTURE,
        ENABLE_BRACKETED_PASTE,
        ENABLE_FOCUS_CHANGE,
        HIDE_CURSOR,
    ] {
        assert!(
            !after_teardown.contains(forbidden_setup),
            "terminal setup escaped after restoration for signal count {signal_count}: {output:?}"
        );
    }
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn signal_during_renderer_setup_never_injects_end_session_before_runtime_handoff() {
    assert_signal_during_renderer_setup(1);
    assert_signal_during_renderer_setup(2);
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn oauth_success_notification_uses_the_ordered_writer_without_new_runtime_wire() {
    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture =
        MockDirectRuntime::start(MockMode::OauthSuccess).expect("create OAuth-success runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open OAuth-success PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before OAuth-success lifecycle");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    command.env("CRABCODE_TUI_FIXTURE_NOTIFICATION_CHANNEL", "iterm2");
    command.env("TERM_PROGRAM", "iTerm.app");
    command.env_remove("TMUX");
    command.env_remove("STY");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn OAuth-success TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take OAuth-success PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone OAuth-success PTY reader"),
        Arc::clone(&captured),
        None,
    );
    const EXPECTED_NOTIFICATION: &str = "\u{1b}]9;\n\nAUTH_SUCCESS_FIXTURE\u{7}";
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("AUTH_SUCCESS_FIXTURE") && output.contains(EXPECTED_NOTIFICATION)
    });
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read OAuth-success owned termios"),
        shell_termios,
        "the notification must be emitted inside the one terminal generation"
    );
    {
        let mut writer = writer.lock().expect("OAuth-success PTY writer lock");
        writer
            .write_all(b"\r")
            .expect("continue past OAuth-success setup notice");
        writer
            .flush()
            .expect("flush OAuth-success setup acknowledgement");
    }
    wait_until_condition(
        Duration::from_secs(5),
        "OAuth-success SDK initialize response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );
    let setup = fixture.setup_transcript();
    assert_eq!(
        setup
            .iter()
            .map(|event| event.get("fixture_event").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        [
            Some("initialize_request"),
            Some("renderer_context_request"),
            Some("renderer_context_response"),
            Some("oauth_success_request"),
            Some("oauth_success_response"),
            Some("initialize_response"),
        ],
        "terminal notification delivery must not add a setup or runtime envelope"
    );
    {
        let mut writer = writer.lock().expect("OAuth-success PTY writer lock");
        writer
            .write_all(&[0x03])
            .expect("write first OAuth-success exit key");
        writer.flush().expect("flush first OAuth-success exit key");
    }
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("再次按 Ctrl-C 即可退出")
    });
    {
        let mut writer = writer.lock().expect("OAuth-success PTY writer lock");
        writer
            .write_all(&[0x03])
            .expect("write confirmed OAuth-success exit key");
        writer
            .flush()
            .expect("flush confirmed OAuth-success exit key");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll OAuth-success TUI") {
            assert!(status.success(), "OAuth-success TUI failed: {status:?}");
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("OAuth-success TUI did not exit after two Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after OAuth-success TUI exit"),
        shell_termios,
        "OAuth notification delivery must not replace or leak terminal ownership"
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread.join().expect("join OAuth-success PTY reader");
    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(
        occurrences(&output, EXPECTED_NOTIFICATION),
        1,
        "one validated OAuth success setup event must enqueue one terminal notification"
    );
}

#[test]
#[serial_test::serial(terminal_lifecycle)]
fn oauth_copy_uses_the_resumed_generation_and_precedes_the_next_renderer_frame() {
    const EXPECTED_OSC52: &str =
        "\u{1b}]52;c;aHR0cHM6Ly9hY29zbWkudGVzdC9sb2dpbj9mbG93PW9yZGVyZWQtY29weQ==\u{7}";

    let canonical_cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .expect("canonical test cwd");
    let fixture = MockDirectRuntime::start(MockMode::OauthBrowserCopy)
        .expect("create OAuth browser-copy runtime");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open OAuth browser-copy PTY");
    let shell_termios = pair
        .master
        .get_termios()
        .expect("read shell termios before OAuth browser-copy lifecycle");
    let mut command = base_command("fullscreen");
    fixture.configure(&mut command);
    command.cwd(&canonical_cwd);
    command.env("TERM_PROGRAM", "ghostty");
    command.env_remove("SSH_CONNECTION");
    command.env_remove("SSH_TTY");
    command.env_remove("SSH_CLIENT");
    command.env("CRABCODE_TUI_TEST_ONLY_DISABLE_NATIVE_CLIPBOARD", "1");
    command.env_remove("TMUX");
    command.env_remove("STY");
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn OAuth browser-copy TUI");
    drop(pair.slave);

    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .expect("take OAuth browser-copy PTY writer"),
    ));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader_thread = spawn_reader(
        pair.master
            .try_clone_reader()
            .expect("clone OAuth browser-copy PTY reader"),
        Arc::clone(&captured),
        None,
    );
    let setup_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let transcript = std::fs::read_to_string(&fixture.setup_path).unwrap_or_default();
        if transcript.contains("\"oauth_browser_open_failed_response\"") {
            break;
        }
        let raw = {
            let bytes = captured.lock().expect("OAuth setup capture lock");
            String::from_utf8_lossy(&bytes).into_owned()
        };
        assert!(
            Instant::now() < setup_deadline,
            "timed out waiting for OAuth browser-open-failed setup acknowledgement; \
             setup transcript: {transcript:?}; terminal output: {raw:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(
        pair.master
            .get_termios()
            .expect("read OAuth browser-copy owned termios"),
        shell_termios,
        "the renderer must own raw mode before the copy lifecycle"
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("BROWSER_OPEN_FAILED_ORDER_MARKER")
            && output.contains("https://acosmi.test/login?flow=ordered-copy")
            && output.contains("(c to copy)")
    });

    // Suspend releases the registered writer generation before restoring the
    // shell. Resume creates and installs a distinct generation; the later
    // copy proves the renderer did not retain or silently replace a stale
    // stdout route.
    let pid = child.process_id().expect("OAuth browser-copy child pid");
    kill(Pid::from_raw(pid as i32), Signal::SIGTSTP).expect("suspend OAuth browser-copy TUI");
    wait_until_condition(
        Duration::from_secs(5),
        "OAuth browser-copy suspend termios restoration",
        || pair.master.get_termios().as_ref() == Some(&shell_termios),
    );
    kill(Pid::from_raw(pid as i32), Signal::SIGCONT).expect("resume OAuth browser-copy TUI");
    wait_until_condition(
        Duration::from_secs(5),
        "OAuth browser-copy resumed raw mode",
        || {
            pair.master
                .get_termios()
                .is_some_and(|current| current != shell_termios)
        },
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        let Some((resume_offset, _)) = output.match_indices(ENTER_ALTERNATE_SCREEN).nth(1) else {
            return false;
        };
        let resumed = &output[resume_offset..];
        resumed.contains(BEGIN_SYNCHRONIZED_UPDATE) && resumed.contains(END_SYNCHRONIZED_UPDATE)
    });

    {
        let mut writer = writer.lock().expect("OAuth browser-copy PTY writer lock");
        writer
            .write_all(b"c")
            .expect("invoke OAuth browser URL copy");
        writer.flush().expect("flush OAuth browser URL copy key");
    }
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains(EXPECTED_OSC52)
    });
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.find(EXPECTED_OSC52).is_some_and(|osc_offset| {
            completed_frame_end_showing_after(
                output,
                osc_offset + EXPECTED_OSC52.len(),
                "（已复制）",
            )
            .is_some()
        })
    });
    let copied_frame_end = {
        let bytes = captured.lock().expect("OAuth copied-frame capture lock");
        let output = String::from_utf8_lossy(&bytes);
        let osc_offset = output
            .find(EXPECTED_OSC52)
            .expect("ordered OSC52 before copied frame");
        completed_frame_end_showing_after(&output, osc_offset + EXPECTED_OSC52.len(), "（已复制）")
            .expect("complete copied-feedback frame after OSC52 delivery")
    };
    std::fs::write(&fixture.copy_gate_path, b"copied")
        .expect("release fixture initialize response after ordered copy");
    wait_until_condition(
        Duration::from_secs(5),
        "OAuth browser-copy SDK initialize response",
        || {
            std::fs::read_to_string(&fixture.setup_path)
                .is_ok_and(|transcript| transcript.contains("\"initialize_response\""))
        },
    );
    wait_until(&captured, Duration::from_secs(5), |output| {
        let Some(tail) = output.get(copied_frame_end..) else {
            return false;
        };
        tail.find(BEGIN_SYNCHRONIZED_UPDATE).is_some_and(|start| {
            tail[start + BEGIN_SYNCHRONIZED_UPDATE.len()..].contains(END_SYNCHRONIZED_UPDATE)
        })
    });

    let setup = fixture.setup_transcript();
    assert_eq!(
        setup
            .iter()
            .map(|event| event.get("fixture_event").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        [
            Some("initialize_request"),
            Some("renderer_context_request"),
            Some("renderer_context_response"),
            Some("oauth_browser_url_request"),
            Some("oauth_browser_url_response"),
            Some("oauth_browser_open_failed_request"),
            Some("oauth_browser_open_failed_response"),
            Some("initialize_response"),
        ],
        "clipboard delivery must add no setup or public runtime envelope"
    );

    {
        let mut writer = writer.lock().expect("OAuth browser-copy PTY writer lock");
        writer
            .write_all(&[0x03])
            .expect("write first OAuth browser-copy exit key");
        writer
            .flush()
            .expect("flush first OAuth browser-copy exit key");
    }
    wait_until(&captured, Duration::from_secs(5), |output| {
        output.contains("再次按 Ctrl-C 即可退出")
    });
    {
        let mut writer = writer.lock().expect("OAuth browser-copy PTY writer lock");
        writer
            .write_all(&[0x03])
            .expect("write confirmed OAuth browser-copy exit key");
        writer
            .flush()
            .expect("flush confirmed OAuth browser-copy exit key");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        if let Some(status) = child.try_wait().expect("poll OAuth browser-copy TUI") {
            assert!(
                status.success(),
                "OAuth browser-copy TUI failed: {status:?}"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _kill_result = child.kill();
            panic!("OAuth browser-copy TUI did not exit after two Ctrl-C key events");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.master
            .get_termios()
            .expect("read termios after OAuth browser-copy TUI exit"),
        shell_termios,
        "copy, suspend/resume, and final drop must restore the original terminal"
    );
    fixture.finish();
    drop(writer);
    drop(pair.master);
    reader_thread
        .join()
        .expect("join OAuth browser-copy PTY reader");

    let output = {
        let bytes = captured.lock().expect("capture lock");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    assert_eq!(
        occurrences(&output, EXPECTED_OSC52),
        1,
        "one copy key must enqueue exactly one OSC52 payload"
    );
    let osc_offset = output
        .find(EXPECTED_OSC52)
        .expect("ordered OSC52 payload offset");
    let resumed_terminal_offset = output[..osc_offset]
        .rfind(ENTER_ALTERNATE_SCREEN)
        .expect("resumed terminal generation before OSC52");
    let visible_manual_offset = output[resumed_terminal_offset..osc_offset]
        .find("BROWSER_OPEN_FAILED_ORDER_MARKER")
        .map(|offset| offset + resumed_terminal_offset)
        .expect("visible manual OAuth prompt in the resumed renderer generation");
    let visible_url_offset = output[visible_manual_offset..osc_offset]
        .find("https://acosmi.test/login?flow=ordered-copy")
        .map(|offset| offset + visible_manual_offset)
        .expect("visible OAuth URL before the copy key");
    let copied_frame_offset = output[..copied_frame_end]
        .rfind(BEGIN_SYNCHRONIZED_UPDATE)
        .expect("copied acknowledgement synchronized-frame start");
    let initialized_frame_offset = output[copied_frame_end..]
        .find(BEGIN_SYNCHRONIZED_UPDATE)
        .map(|offset| offset + copied_frame_end)
        .expect("initialized renderer frame after copied acknowledgement");
    assert!(
        resumed_terminal_offset < visible_manual_offset
            && visible_manual_offset < visible_url_offset
            && visible_url_offset < osc_offset
            && osc_offset < copied_frame_offset
            && copied_frame_offset < copied_frame_end
            && copied_frame_end <= initialized_frame_offset,
        "the resumed sole writer must preserve generation -> visible URL -> OSC52 -> copied acknowledgement -> initialized-frame order"
    );
}
