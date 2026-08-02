//! Dependency-closed display utilities used by scrollback blocks.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager-render/src/util.rs`, SHA-256
//! `70cebd8049934de071acc4e6f1aeef5023bf6e884329b697c9954e62c9207150`.
//! Only dependency-closed display helpers are exposed. Product config-home
//! resolution and schedule utilities remain outside this crate's presentation
//! boundary.

use std::borrow::Cow;
use std::time::Duration;

/// Abbreviate an absolute path under `$HOME` for display.
///
/// The fixed renderer also recognizes its product config root. CrabCode keeps
/// that authority in its existing backend, so this presentation-only adapter
/// intentionally applies only the fixed generic-home fallback and receives no
/// new config path or protocol field.
pub fn abbreviate_path(path: &str) -> Cow<'_, str> {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(&home)
    {
        if rest.is_empty() {
            return Cow::Borrowed("~");
        }
        if rest.starts_with('/') {
            return Cow::Owned(format!("~{rest}"));
        }
    }
    Cow::Borrowed(path)
}

/// Format a duration as a compact human-friendly string.
///
/// Uses consistent rounding for visual stability:
/// - Under 10s: `"5.2s"`
/// - 10-59s: `"32s"`
/// - 1m-59m: `"2m5s"`
/// - 1h+: `"1h2m"`
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    if total_secs < 10 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        return format!("{mins}m{secs}s");
    }
    let hours = mins / 60;
    let remaining_mins = mins % 60;
    format!("{hours}h{remaining_mins}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_duration_buckets_are_stable() {
        assert_eq!(format_duration(Duration::from_millis(5200)), "5.2s");
        assert_eq!(format_duration(Duration::from_secs(32)), "32s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h2m");
    }

    #[test]
    fn home_path_abbreviation_preserves_non_home_paths() {
        assert_eq!(
            abbreviate_path("/definitely-not-the-current-home/file"),
            "/definitely-not-the-current-home/file"
        );
    }

    #[test]
    fn home_path_abbreviation_uses_tilde_when_home_is_available() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        if home.is_empty() {
            return;
        }
        assert_eq!(
            abbreviate_path(&format!("{home}/memory/MEMORY.md")),
            "~/memory/MEMORY.md"
        );
    }
}
