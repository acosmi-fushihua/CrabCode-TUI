//! Shared URL-opening and scheme-validation utilities.
//!
//! This module is terminal presentation infrastructure only. It does not own
//! remote settings, billing destinations, telemetry, or backend routing.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::audited_terminal::hyperlinks::SchemeFilter;

/// Outcome of attempting to open a URL in the system browser or handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenUrlResult {
    Opened,
    RejectedScheme,
    BrowserUnavailable,
}

/// Pure environment check for whether launching a browser is likely to work.
pub fn browser_open_likely_available_from_env(env: &HashMap<String, String>) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    if env.get("BROWSER").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    env.get("WAYLAND_DISPLAY")
        .is_some_and(|value| !value.is_empty())
        || env.get("DISPLAY").is_some_and(|value| !value.is_empty())
}

/// Whether this process likely has a GUI browser available.
pub fn browser_open_likely_available() -> bool {
    browser_open_likely_available_from_env(&crate::audited_host::collect_unicode_env())
}

/// User-facing fallback text that preserves the full URL for manual copying.
pub fn browser_unavailable_message(url: &str) -> String {
    format!("Could not open a browser. Open this URL manually:\n{url}")
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // The child is detached before exec so a GUI helper cannot acquire the
    // direct TUI's controlling terminal. Only async-signal-safe libc work is
    // performed in the pre-exec hook.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn detach_command(_command: &mut Command) {}

/// Open a URL with the platform-native handler.
///
/// The optional test seam records the URL instead of spawning a GUI process.
pub fn open_url(url: &str) -> bool {
    if let Ok(path) = std::env::var("CRABCODE_TEST_OPEN_URL_FILE") {
        use std::io::Write;

        if let Err(error) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{url}"))
        {
            tracing::warn!(
                error = %error,
                path,
                "terminal URL test-seam write failed"
            );
            return false;
        }
        return true;
    }

    if !browser_open_likely_available() {
        tracing::info!("skipping browser open: no display server or browser override");
        return false;
    }

    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "cmd";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";

    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    command.args(["/c", "start", ""]);
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    match command.spawn() {
        Ok(_) => true,
        Err(error) => {
            let redacted = url::Url::parse(url)
                .map(|mut parsed| {
                    parsed.set_query(None);
                    parsed.set_fragment(None);
                    parsed.to_string()
                })
                .unwrap_or_else(|_| "<unparseable>".to_owned());
            tracing::warn!(url = %redacted, error = %error, "failed to open URL");
            false
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn build_open_path_command(path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("xdg-open");
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    command
}

/// Open a trusted local path with the platform-native handler.
pub fn open_path(path: &Path) -> bool {
    #[cfg(test)]
    {
        !path.as_os_str().is_empty()
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        reveal_in_explorer(path)
    }
    #[cfg(all(not(test), not(target_os = "windows")))]
    {
        match build_open_path_command(path).spawn() {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to open file natively"
                );
                false
            }
        }
    }
}

#[cfg(all(not(test), target_os = "windows"))]
fn reveal_in_explorer(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;

    let target = if path.is_file() || path.is_dir() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
        parent.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let escaped = target.display().to_string().replace('"', "\"\"");
    let mut command = Command::new("explorer");
    if target.is_file() {
        command.raw_arg(format!("/select,\"{escaped}\""));
    } else {
        command.raw_arg(format!("\"{escaped}\""));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    match command.spawn() {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                path = %target.display(),
                error = %error,
                "failed to reveal file"
            );
            false
        }
    }
}

/// Check whether a URL scheme is permitted by the terminal policy.
pub fn is_safe_to_open(url: &str, filter: SchemeFilter) -> bool {
    let url = url.trim();
    if let Ok(parsed) = url::Url::parse(url) {
        return filter.allows(parsed.scheme());
    }
    if let Some((scheme, _)) = url.split_once("://") {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    if let Some((scheme, _)) = url.split_once(':')
        && scheme.eq_ignore_ascii_case("mailto")
    {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    false
}

/// Validate and open a URL, returning only whether opening succeeded.
pub fn open_url_if_safe(url: &str, filter: SchemeFilter) -> bool {
    matches!(try_open_url(url, filter), OpenUrlResult::Opened)
}

/// Validate and open a URL while preserving rejection and unavailable states.
pub fn try_open_url(url: &str, filter: SchemeFilter) -> OpenUrlResult {
    if !is_safe_to_open(url, filter) {
        tracing::debug!(url, "URL scheme not permitted");
        return OpenUrlResult::RejectedScheme;
    }
    if open_url(url) {
        OpenUrlResult::Opened
    } else {
        OpenUrlResult::BrowserUnavailable
    }
}

/// Ensure that a URL has the named query parameter.
pub fn ensure_query_param(url: &str, key: &str, value: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.query_pairs().any(|(existing, _)| existing == key) {
        return parsed.to_string();
    }
    parsed.query_pairs_mut().append_pair(key, value);
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn open_path_command_passes_path_as_a_single_arg() {
        let path = Path::new("/tmp/crabcode session/image 1.jpg");
        let command = build_open_path_command(path);
        let args: Vec<_> = command.get_args().map(|arg| arg.to_os_string()).collect();
        assert!(args.contains(&path.as_os_str().to_os_string()));
    }

    #[test]
    fn standard_http_schemes_allowed() {
        assert!(is_safe_to_open(
            "http://example.com",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "https://example.com/path?q=1",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn mailto_allowed() {
        assert!(is_safe_to_open(
            "mailto:user@example.com",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn file_scheme_blocked_by_standard() {
        assert!(!is_safe_to_open(
            "file:///home/user/doc.pdf",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "file:///home/user/doc.pdf",
            SchemeFilter::EditorExtended
        ));
    }

    #[test]
    fn javascript_scheme_blocked() {
        assert!(!is_safe_to_open(
            "javascript:alert(1)",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn data_scheme_blocked() {
        assert!(!is_safe_to_open(
            "data:text/html,<h1>hi</h1>",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn empty_and_garbage_rejected() {
        assert!(!is_safe_to_open("", SchemeFilter::Standard));
        assert!(!is_safe_to_open("not-a-url", SchemeFilter::Standard));
        assert!(!is_safe_to_open(
            "://missing-scheme",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn editor_schemes_with_extended_filter() {
        for url in [
            "vscode://file/path",
            "cursor://open",
            "idea://open",
            "zed://open",
        ] {
            assert!(is_safe_to_open(url, SchemeFilter::EditorExtended));
        }
    }

    #[test]
    fn editor_schemes_blocked_by_standard_filter() {
        assert!(!is_safe_to_open(
            "vscode://file/path",
            SchemeFilter::Standard
        ));
        assert!(!is_safe_to_open("cursor://open", SchemeFilter::Standard));
    }

    #[test]
    fn scheme_case_sensitivity() {
        assert!(is_safe_to_open(
            "HTTP://EXAMPLE.COM",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "HTTPS://EXAMPLE.COM",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn url_with_fragment_and_query() {
        assert!(is_safe_to_open(
            "https://example.com/page?key=val#section",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn ftp_scheme_blocked() {
        assert!(!is_safe_to_open(
            "ftp://files.example.com/pub",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn fallback_colon_slash_slash_path() {
        assert!(!is_safe_to_open(
            "custom://something",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn non_mailto_colon_without_slashes_rejected() {
        assert!(!is_safe_to_open("tel:+1234567890", SchemeFilter::Standard));
    }

    #[test]
    fn whitespace_trimmed_before_parse() {
        assert!(is_safe_to_open(
            "  https://example.com  ",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "\thttps://example.com\n",
            SchemeFilter::Standard
        ));
    }

    #[test]
    fn ensure_query_param_appends_when_missing() {
        let output = ensure_query_param("https://example.com/plan", "source", "terminal");
        assert_eq!(output, "https://example.com/plan?source=terminal");
    }

    #[test]
    fn ensure_query_param_preserves_existing_value() {
        let output = ensure_query_param(
            "https://example.com/plan?source=other",
            "source",
            "terminal",
        );
        assert_eq!(output, "https://example.com/plan?source=other");
    }

    #[test]
    fn ensure_query_param_keeps_other_query_pairs() {
        let output = ensure_query_param("https://example.com/plan?mode=1", "source", "terminal");
        assert_eq!(output, "https://example.com/plan?mode=1&source=terminal");
    }

    #[test]
    fn ensure_query_param_preserves_fragment() {
        let output = ensure_query_param("https://example.com/#plan", "source", "terminal");
        assert_eq!(output, "https://example.com/?source=terminal#plan");
    }

    #[test]
    fn ensure_query_param_returns_unchanged_on_parse_failure() {
        let output = ensure_query_param("not a url", "source", "terminal");
        assert_eq!(output, "not a url");
    }

    #[test]
    fn ensure_query_param_url_encodes_value() {
        let output = ensure_query_param("https://example.com/plan", "source", "direct tui");
        assert_eq!(output, "https://example.com/plan?source=direct+tui");
    }

    #[test]
    fn fallback_scheme_case_insensitive() {
        assert!(!is_safe_to_open(
            "CUSTOM://something",
            SchemeFilter::Standard
        ));
        assert!(is_safe_to_open(
            "MAILTO:user@example.com",
            SchemeFilter::Standard
        ));
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn browser_available_with_x11_display() {
        assert!(browser_open_likely_available_from_env(&env(&[(
            "DISPLAY", ":0"
        )])));
    }

    #[test]
    fn browser_available_with_wayland() {
        assert!(browser_open_likely_available_from_env(&env(&[(
            "WAYLAND_DISPLAY",
            "wayland-0"
        )])));
    }

    #[test]
    fn browser_available_with_browser_env_override() {
        assert!(browser_open_likely_available_from_env(&env(&[(
            "BROWSER", "firefox"
        )])));
    }

    #[test]
    fn browser_unavailable_when_display_vars_empty_or_missing() {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(browser_open_likely_available_from_env(&env(&[])));
            return;
        }
        assert!(!browser_open_likely_available_from_env(&env(&[])));
        assert!(!browser_open_likely_available_from_env(&env(&[
            ("DISPLAY", ""),
            ("WAYLAND_DISPLAY", ""),
            ("BROWSER", ""),
        ])));
    }

    #[test]
    fn browser_unavailable_message_includes_full_url() {
        let url = "https://example.com/plan?source=terminal";
        let message = browser_unavailable_message(url);
        assert!(message.contains("Could not open a browser"));
        assert!(message.contains(url));
        assert!(message.lines().any(|line| line == url));
    }

    #[test]
    fn try_open_url_rejects_unsafe_scheme_without_opening() {
        assert_eq!(
            try_open_url("javascript:alert(1)", SchemeFilter::Standard),
            OpenUrlResult::RejectedScheme
        );
    }
}
