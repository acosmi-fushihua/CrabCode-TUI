//! OS handoff for an explicitly selected semantic terminal link.
//!
//! This terminal interaction starts the operating system's existing URL
//! handler or file application as a detached, stdio-free process after the TUI
//! has released raw/screen ownership. The direct backend connection is not
//! involved.

use std::collections::HashMap;
#[cfg(test)]
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::tui_links::{LinkTarget, resolve_link_open_target, unicode_environment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenLinkResult {
    Opened,
    HandlerUnavailable(String),
    Rejected,
}

/// Attempt one already user-selected semantic handoff.
///
/// Validation is repeated at the process boundary. A successful `spawn` means
/// only that the platform handler accepted the request; it does not assert
/// that a browser or file application later rendered the target.
pub(crate) fn try_open_target(target: &LinkTarget) -> OpenLinkResult {
    let Some(target) = resolve_link_open_target(target) else {
        return OpenLinkResult::Rejected;
    };
    if matches!(target, LinkTarget::Url(_))
        && !browser_open_likely_available_from_env(&unicode_environment())
    {
        return OpenLinkResult::HandlerUnavailable(
            "no graphical URL handler is advertised by this environment".to_string(),
        );
    }

    let Some(mut command) = platform_open_command(&target) else {
        return OpenLinkResult::Rejected;
    };
    match command.spawn() {
        Ok(_child) => OpenLinkResult::Opened,
        Err(error) => OpenLinkResult::HandlerUnavailable(error.to_string()),
    }
}

/// Linux/BSD needs either a display server or an explicit browser override.
/// macOS and Windows have platform URL handlers; spawn failure is still
/// reported to the caller.
fn browser_open_likely_available_from_env(environment: &HashMap<String, String>) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    ["BROWSER", "WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| {
            environment
                .get(*name)
                .is_some_and(|value| !value.is_empty())
        })
}

fn platform_open_command(target: &LinkTarget) -> Option<Command> {
    let mut command = match target {
        LinkTarget::Url(url) => platform_url_open_command(url),
        LinkTarget::File(path) if !path.as_os_str().is_empty() => platform_path_open_command(path),
        LinkTarget::File(_) => return None,
        LinkTarget::Mermaid { .. } => return None,
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_from_terminal(&mut command);
    Some(command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum OpenPlatform {
    MacOs,
    Windows,
    OtherUnix,
}

const fn current_open_platform() -> OpenPlatform {
    #[cfg(target_os = "macos")]
    {
        OpenPlatform::MacOs
    }
    #[cfg(target_os = "windows")]
    {
        OpenPlatform::Windows
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        OpenPlatform::OtherUnix
    }
}

fn platform_url_open_command(target: &str) -> Command {
    url_open_command_for_platform(current_open_platform(), target)
}

fn url_open_command_for_platform(platform: OpenPlatform, target: &str) -> Command {
    let mut command = match platform {
        OpenPlatform::MacOs => Command::new("open"),
        OpenPlatform::Windows => {
            // CrabCode's established argv-only Windows contract deliberately
            // avoids `cmd /c start`: cmd.exe would reinterpret URL characters
            // such as `&|><`. This is an intentional command-level safety
            // difference from the pinned upstream with the same user-visible
            // default-handler capability.
            let mut command = Command::new("rundll32");
            command.arg("url,OpenURL");
            command
        }
        OpenPlatform::OtherUnix => Command::new("xdg-open"),
    };
    command.arg(target);
    command
}

#[cfg(not(target_os = "windows"))]
fn platform_path_open_command(path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");

    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("xdg-open");

    // Path is a single OS-native argument, never a shell-interpolated string.
    command.arg(path);
    command
}

/// Reveal an existing Windows file with Explorer selection. Directories and
/// missing files with an existing parent open as folders. This direct process
/// launch preserves percent characters because no command shell participates.
#[cfg(target_os = "windows")]
fn platform_path_open_command(path: &Path) -> Command {
    use std::os::windows::process::CommandExt as _;

    let target = if path.is_file() || path.is_dir() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
        parent.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let select_file = target.is_file();
    let escaped = target.display().to_string().replace('"', "\"\"");
    let mut command = Command::new("explorer.exe");
    if select_file {
        command.raw_arg(format!("/select,\"{escaped}\""));
    } else {
        command.raw_arg(format!("\"{escaped}\""));
    }
    command
}

#[cfg_attr(unix, allow(unsafe_code))]
fn detach_from_terminal(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        // SAFETY: the hook invokes only POSIX async-signal-safe `setsid` and,
        // for the process-group-leader edge case, `setpgid`.
        unsafe {
            command.pre_exec(|| {
                use nix::errno::Errno;
                use nix::unistd::{Pid, setpgid, setsid};

                match setsid() {
                    Ok(_) => Ok(()),
                    Err(Errno::EPERM) => setpgid(Pid::from_raw(0), Pid::from_raw(0))
                        .map_err(|error| std::io::Error::from_raw_os_error(error as i32)),
                    Err(error) => Err(std::io::Error::from_raw_os_error(error as i32)),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn headless_detection_requires_real_nonempty_signal() {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(browser_open_likely_available_from_env(&HashMap::new()));
            return;
        }
        let mut environment = HashMap::new();
        assert!(!browser_open_likely_available_from_env(&environment));
        environment.insert("DISPLAY".to_string(), String::new());
        assert!(!browser_open_likely_available_from_env(&environment));
        environment.insert("BROWSER".to_string(), "w3m".to_string());
        assert!(browser_open_likely_available_from_env(&environment));
    }

    #[test]
    fn every_url_command_preserves_shell_metacharacters_as_one_argument() {
        let target = "https://example.test/path?q=a%20b&pipe=|&in=<&out=>&second=%24HOME";
        for (platform, program, prefix) in [
            (OpenPlatform::MacOs, "open", Vec::<&str>::new()),
            (OpenPlatform::Windows, "rundll32", vec!["url,OpenURL"]),
            (OpenPlatform::OtherUnix, "xdg-open", Vec::<&str>::new()),
        ] {
            let command = url_open_command_for_platform(platform, target);
            assert_eq!(command.get_program(), program);
            let arguments = command.get_args().map(OsString::from).collect::<Vec<_>>();
            let mut expected = prefix.into_iter().map(OsString::from).collect::<Vec<_>>();
            expected.push(OsString::from(target));
            assert_eq!(arguments, expected, "{platform:?}");
            assert_ne!(command.get_program(), "cmd", "{platform:?}");
        }

        let command = platform_open_command(&LinkTarget::Url(Arc::from(target))).expect("command");
        assert_eq!(
            command.get_args().last(),
            Some(std::ffi::OsStr::new(target))
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn path_command_preserves_spaces_as_one_os_native_argument() {
        let path = Path::new("/tmp/CrabCode session/image 1.jpg");
        let command = platform_open_command(&LinkTarget::File(Arc::from(path))).expect("command");
        let arguments = command.get_args().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(arguments, vec![path.as_os_str().to_os_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn path_command_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path =
            std::path::PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\x80/image.jpg".to_vec()));
        let command =
            platform_open_command(&LinkTarget::File(Arc::from(path.as_path()))).expect("command");
        let argument = command.get_args().next().expect("path argument");
        assert_eq!(argument.as_bytes(), path.as_os_str().as_bytes());
    }

    #[test]
    fn unsafe_url_and_empty_file_are_rejected_before_spawn() {
        assert_eq!(
            try_open_target(&LinkTarget::Url(Arc::from("javascript:alert(1)"))),
            OpenLinkResult::Rejected
        );
        assert_eq!(
            try_open_target(&LinkTarget::File(Arc::from(Path::new("")))),
            OpenLinkResult::Rejected
        );
    }

    #[test]
    fn file_url_is_not_reclassified_as_a_url_target() {
        assert_eq!(
            try_open_target(&LinkTarget::Url(Arc::from("file:///tmp/a"))),
            OpenLinkResult::Rejected
        );
        assert!(platform_open_command(&LinkTarget::File(Arc::from(Path::new("/tmp/a")))).is_some());
    }
}
