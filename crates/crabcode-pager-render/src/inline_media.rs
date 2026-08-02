//! Inline-media presentation capability and row reservation.
//!
//! Fixed-source lineage: commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`, source revision
//! `30192d2eef5d91a8fff0e53957de5bd05b43398c`, path
//! `crates/codegen/xai-grok-pager/src/inline_media_ffmpeg.rs`, SHA-256
//! `72f865f0486cf249f385523022a43f59528b0dd93839e48d775f54107da6c823`.
//! Product difference: the fixed source's config-layer executable probe is
//! replaced by the renderer-local `which` probe. It reads host capability
//! only; it does not install, execute, or configure anything.

use crate::prompt_images::InlineMediaInfo;

pub const FFMPEG_HINT_TEXT: &str = "Install ffmpeg to view inline";

/// Latches positives and re-probes negatives so a mid-session install can
/// recover video posters.
pub fn ffmpeg_available() -> bool {
    #[cfg(test)]
    if let Some(value) = TEST_FFMPEG_OVERRIDE.with(std::cell::Cell::get) {
        return value;
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    static FOUND: AtomicBool = AtomicBool::new(false);
    if FOUND.load(Ordering::Relaxed) {
        return true;
    }

    let available = which::which("ffmpeg").is_ok();
    if available {
        FOUND.store(true, Ordering::Relaxed);
    }
    available
}

#[cfg(test)]
thread_local! {
    static TEST_FFMPEG_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_ffmpeg_available_for_test(available: bool) -> FfmpegTestGuard {
    TEST_FFMPEG_OVERRIDE.with(|cell| cell.set(Some(available)));
    FfmpegTestGuard
}

#[cfg(test)]
pub(crate) struct FfmpegTestGuard;

#[cfg(test)]
impl Drop for FfmpegTestGuard {
    fn drop(&mut self) {
        TEST_FFMPEG_OVERRIDE.with(|cell| cell.set(None));
    }
}

fn ffmpeg_install_candidates() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "macos") {
        &[("brew", "! brew install ffmpeg")]
    } else if cfg!(target_os = "windows") {
        &[
            ("winget", "! winget install ffmpeg"),
            ("choco", "! choco install ffmpeg"),
            ("scoop", "! scoop install ffmpeg"),
        ]
    } else {
        &[
            ("apt", "! sudo apt install ffmpeg"),
            ("apt-get", "! sudo apt-get install ffmpeg"),
            ("dnf", "! sudo dnf install ffmpeg"),
            ("pacman", "! sudo pacman -S ffmpeg"),
            ("zypper", "! sudo zypper install ffmpeg"),
            ("apk", "! sudo apk add ffmpeg"),
        ]
    }
}

/// First package manager on `PATH`; `None` means the hint has no command row.
///
/// This returns display text only. No command is invoked.
pub fn ffmpeg_install_cmd() -> Option<&'static str> {
    #[cfg(test)]
    if let Some(value) = TEST_FFMPEG_INSTALL_CMD_OVERRIDE.with(std::cell::Cell::get) {
        return value;
    }

    use std::sync::OnceLock;

    static FOUND: OnceLock<&'static str> = OnceLock::new();
    if let Some(command) = FOUND.get() {
        return Some(*command);
    }
    for (manager, command) in ffmpeg_install_candidates() {
        if which::which(manager).is_ok() {
            let _ = FOUND.set(*command);
            return Some(*command);
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    static TEST_FFMPEG_INSTALL_CMD_OVERRIDE:
        std::cell::Cell<Option<Option<&'static str>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn set_ffmpeg_install_cmd_for_test(command: Option<&'static str>) -> FfmpegInstallCmdTestGuard {
    TEST_FFMPEG_INSTALL_CMD_OVERRIDE.with(|cell| cell.set(Some(command)));
    FfmpegInstallCmdTestGuard
}

#[cfg(test)]
struct FfmpegInstallCmdTestGuard;

#[cfg(test)]
impl Drop for FfmpegInstallCmdTestGuard {
    fn drop(&mut self) {
        TEST_FFMPEG_INSTALL_CMD_OVERRIDE.with(|cell| cell.set(None));
    }
}

fn ffmpeg_hint_banner_rows() -> u16 {
    if ffmpeg_install_cmd().is_some() { 2 } else { 1 }
}

/// `(image_area, total)` rows for an inline-media preview.
///
/// Shared by entry-height reservation and block placement so they cannot
/// drift.
pub fn inline_media_reserved_rows(info: &InlineMediaInfo, content_width: u16) -> (u16, u16) {
    if info.is_video && !ffmpeg_available() {
        let banner_rows = ffmpeg_hint_banner_rows();
        return (banner_rows, banner_rows + 1);
    }
    let max_cols = content_width.saturating_sub(2);
    let max_rows = (content_width / 2).clamp(4, 20);
    let (_cols, rows) =
        crate::terminal::image::fit_image_to_cells(info.width, info.height, max_cols, max_rows);
    (rows, rows + 3)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn media(is_video: bool) -> InlineMediaInfo {
        InlineMediaInfo {
            path: PathBuf::from("/tmp/media"),
            width: 640,
            height: 480,
            is_video,
            alt_text: String::new(),
        }
    }

    #[test]
    fn install_candidates_are_displayable_prompt_hints() {
        for (manager, command) in ffmpeg_install_candidates() {
            assert!(!manager.is_empty());
            assert!(command.starts_with("! "));
            assert!(command.contains("ffmpeg"));
        }
    }

    #[test]
    fn image_reserves_poster_and_button_rows() {
        let _ffmpeg = set_ffmpeg_available_for_test(false);
        let (poster, total) = inline_media_reserved_rows(&media(false), 80);
        assert!((4..=20).contains(&poster));
        assert_eq!(total, poster + 3);
    }

    #[test]
    fn video_without_decoder_uses_two_line_hint_when_command_exists() {
        let _ffmpeg = set_ffmpeg_available_for_test(false);
        let _command = set_ffmpeg_install_cmd_for_test(Some("! brew install ffmpeg"));
        assert_eq!(inline_media_reserved_rows(&media(true), 80), (2, 3));
    }

    #[test]
    fn video_without_decoder_uses_one_line_hint_without_command() {
        let _ffmpeg = set_ffmpeg_available_for_test(false);
        let _command = set_ffmpeg_install_cmd_for_test(None);
        assert_eq!(inline_media_reserved_rows(&media(true), 80), (1, 2));
    }

    #[test]
    fn video_with_decoder_reserves_poster_and_button_rows() {
        let _ffmpeg = set_ffmpeg_available_for_test(true);
        let (poster, total) = inline_media_reserved_rows(&media(true), 80);
        assert!((4..=20).contains(&poster));
        assert_eq!(total, poster + 3);
    }
}
