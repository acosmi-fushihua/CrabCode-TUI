//! System appearance detection for automatic day/night theming.
//!
//! Desktop APIs are safe throughout the TUI lifecycle. OSC 11 is a
//! startup-only fallback because it reads terminal input directly.

use std::time::Duration;

use tokio::sync::watch;

use super::CrabCodeThemeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAppearance {
    Light,
    Dark,
}

/// Detect via desktop APIs only.
#[must_use]
pub fn detect() -> Option<SystemAppearance> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(value) = mock_override() {
        return value;
    }

    detect_without_mock()
}

/// Detect via desktop APIs, then the startup-only OSC 11 terminal probe.
#[must_use]
pub fn detect_with_osc11_fallback() -> Option<SystemAppearance> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(value) = mock_override() {
        return value;
    }

    detect_without_mock().or_else(super::osc11::detect_via_osc11)
}

fn detect_without_mock() -> Option<SystemAppearance> {
    match dark_light::detect() {
        Ok(dark_light::Mode::Dark) => Some(SystemAppearance::Dark),
        Ok(dark_light::Mode::Light) => Some(SystemAppearance::Light),
        _ => None,
    }
}

#[must_use]
pub const fn to_theme_kind(appearance: SystemAppearance) -> CrabCodeThemeKind {
    match appearance {
        SystemAppearance::Light => CrabCodeThemeKind::Light,
        SystemAppearance::Dark => CrabCodeThemeKind::Dark,
    }
}

#[cfg(not(test))]
const POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Polls desktop appearance without mutating the theme cache.
pub struct SystemAppearanceWatcher {
    receiver: watch::Receiver<Option<SystemAppearance>>,
    handle: tokio::task::JoinHandle<()>,
}

impl SystemAppearanceWatcher {
    #[must_use]
    pub fn start_if_auto(is_auto: bool) -> Option<Self> {
        if !is_auto {
            return None;
        }

        let initial = detect();
        let (sender, receiver) = watch::channel(initial);
        let handle = tokio::spawn(async move {
            let mut current = initial;
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let detected = detect();
                if detected != current {
                    current = detected;
                    let _ = sender.send(current);
                }
            }
        });
        Some(Self { receiver, handle })
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.receiver.changed().await
    }

    #[must_use]
    pub fn current(&self) -> Option<SystemAppearance> {
        *self.receiver.borrow()
    }
}

impl Drop for SystemAppearanceWatcher {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(any(test, feature = "test-support"))]
static MOCK_APPEARANCE: std::sync::Mutex<Option<Option<SystemAppearance>>> =
    std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-support"))]
fn mock_override() -> Option<Option<SystemAppearance>> {
    *MOCK_APPEARANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_mock(value: Option<SystemAppearance>) {
    *MOCK_APPEARANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(value);
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_mock() {
    *MOCK_APPEARANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::super::cache;
    use super::*;

    #[test]
    fn appearance_mapping_has_no_config_or_backend_override() {
        assert_eq!(
            to_theme_kind(SystemAppearance::Dark),
            CrabCodeThemeKind::Dark,
        );
        assert_eq!(
            to_theme_kind(SystemAppearance::Light),
            CrabCodeThemeKind::Light,
        );
    }

    #[test]
    fn mock_round_trips_all_detection_outcomes() {
        let _guard = cache::test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for expected in [
            Some(SystemAppearance::Dark),
            Some(SystemAppearance::Light),
            None,
        ] {
            set_mock(expected);
            assert_eq!(detect(), expected);
            assert_eq!(detect_with_osc11_fallback(), expected);
        }
        clear_mock();
    }

    #[tokio::test]
    async fn watcher_is_absent_outside_auto_mode() {
        assert!(SystemAppearanceWatcher::start_if_auto(false).is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn watcher_reports_a_real_change_and_ignores_unchanged_polls() {
        let _guard = cache::test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_mock(Some(SystemAppearance::Dark));
        let mut watcher = SystemAppearanceWatcher::start_if_auto(true).expect("watcher");
        assert_eq!(watcher.current(), Some(SystemAppearance::Dark));

        let unchanged = tokio::time::timeout(Duration::from_millis(150), watcher.changed()).await;
        assert!(unchanged.is_err());

        set_mock(Some(SystemAppearance::Light));
        tokio::time::timeout(Duration::from_secs(2), watcher.changed())
            .await
            .expect("appearance change timeout")
            .expect("watch channel");
        assert_eq!(watcher.current(), Some(SystemAppearance::Light));
        clear_mock();
    }
}
