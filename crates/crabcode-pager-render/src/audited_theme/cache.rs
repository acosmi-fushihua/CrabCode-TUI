//! Process-local theme state and automatic-appearance resolution.
//!
//! This cache deliberately performs no disk/config/backend access. The direct
//! renderer injects its private renderer-context setting, and this module keeps
//! only the concrete render kind plus the selected meta-setting.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use super::CrabCodeThemeKind;
use super::system_appearance::{self, SystemAppearance};

const fn pack(setting: CrabCodeThemeKind, concrete: CrabCodeThemeKind) -> u16 {
    ((setting as u16) << 8) | concrete as u16
}

fn decode(byte: u8) -> CrabCodeThemeKind {
    match byte {
        value if value == CrabCodeThemeKind::Dark as u8 => CrabCodeThemeKind::Dark,
        value if value == CrabCodeThemeKind::Light as u8 => CrabCodeThemeKind::Light,
        value if value == CrabCodeThemeKind::LightDaltonized as u8 => {
            CrabCodeThemeKind::LightDaltonized
        }
        value if value == CrabCodeThemeKind::DarkDaltonized as u8 => {
            CrabCodeThemeKind::DarkDaltonized
        }
        value if value == CrabCodeThemeKind::LightAnsi as u8 => CrabCodeThemeKind::LightAnsi,
        value if value == CrabCodeThemeKind::DarkAnsi as u8 => CrabCodeThemeKind::DarkAnsi,
        value if value == CrabCodeThemeKind::Auto as u8 => CrabCodeThemeKind::Auto,
        _ => CrabCodeThemeKind::Dark,
    }
}

fn unpack(state: u16) -> (CrabCodeThemeKind, CrabCodeThemeKind) {
    let setting = decode((state >> 8) as u8);
    let concrete = decode(state as u8);
    let concrete = if concrete.is_auto() {
        CrabCodeThemeKind::Dark
    } else {
        concrete
    };
    (setting, concrete)
}

/// One atomic word keeps the selected setting and concrete render kind
/// consistent for readers. `Auto` is valid only in the high byte.
static STATE: AtomicU16 = AtomicU16::new(pack(CrabCodeThemeKind::Dark, CrabCodeThemeKind::Dark));

/// Minimal mode pins presentation to the terminal-native palette while
/// preserving the selected theme for restoration after the lock is cleared.
static TERMINAL_NATIVE_LOCK: AtomicBool = AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[must_use]
pub fn current_setting() -> CrabCodeThemeKind {
    unpack(STATE.load(Ordering::Acquire)).0
}

#[must_use]
pub fn current_kind() -> CrabCodeThemeKind {
    if terminal_native_locked() {
        return CrabCodeThemeKind::Dark;
    }
    unpack(STATE.load(Ordering::Acquire)).1
}

#[must_use]
pub fn is_auto_mode() -> bool {
    current_setting().is_auto()
}

/// Whether the renderer is locked to the terminal-native minimal palette.
#[must_use]
pub fn terminal_native_locked() -> bool {
    TERMINAL_NATIVE_LOCK.load(Ordering::Relaxed)
}

/// Engage or clear the terminal-native presentation lock.
///
/// The selected theme remains in [`STATE`]. While locked, Markdown colors are
/// capped to ANSI-16 and syntax tokens use the fixed dual-polarity projection.
pub fn set_terminal_native_lock(locked: bool) {
    TERMINAL_NATIVE_LOCK.store(locked, Ordering::Relaxed);
    crabcode_markdown_renderer::set_color_level_cap(if locked {
        crabcode_markdown_renderer::ColorLevel::Basic
    } else {
        crabcode_markdown_renderer::ColorLevel::TrueColor
    });
    crabcode_markdown_renderer::set_polarity_safe_syntax(locked);
}

fn resolve_appearance(appearance: Option<SystemAppearance>) -> CrabCodeThemeKind {
    appearance
        .map(system_appearance::to_theme_kind)
        .unwrap_or(CrabCodeThemeKind::Dark)
}

/// Resolve `Auto` via desktop APIs only. Safe while a crossterm input reader is
/// active.
#[must_use]
pub fn resolve_auto() -> CrabCodeThemeKind {
    resolve_appearance(system_appearance::detect())
}

/// Resolve `Auto` before terminal input starts, including the startup-only
/// OSC 11 fallback.
#[must_use]
pub fn resolve_auto_at_startup() -> CrabCodeThemeKind {
    resolve_appearance(system_appearance::detect_with_osc11_fallback())
}

fn store(setting: CrabCodeThemeKind, concrete: CrabCodeThemeKind) -> CrabCodeThemeKind {
    debug_assert!(!concrete.is_auto());
    STATE.store(pack(setting, concrete), Ordering::Release);
    concrete
}

/// Apply an explicitly injected renderer setting without touching stdin.
pub fn apply_setting(setting: CrabCodeThemeKind) -> CrabCodeThemeKind {
    let concrete = if setting.is_auto() {
        resolve_auto()
    } else {
        setting
    };
    store(setting, concrete)
}

/// Apply an explicitly injected setting before terminal input exists.
pub fn apply_setting_at_startup(setting: CrabCodeThemeKind) -> CrabCodeThemeKind {
    let concrete = if setting.is_auto() {
        resolve_auto_at_startup()
    } else {
        setting
    };
    store(setting, concrete)
}

/// Apply a watcher observation only if `Auto` is still the selected setting.
///
/// The compare/exchange prevents a late watcher tick from overwriting a newer
/// explicit theme selection.
pub fn apply_runtime_appearance(appearance: Option<SystemAppearance>) -> Option<CrabCodeThemeKind> {
    let concrete = resolve_appearance(appearance);
    let mut observed = STATE.load(Ordering::Acquire);
    loop {
        let (setting, _) = unpack(observed);
        if !setting.is_auto() {
            return None;
        }
        let replacement = pack(CrabCodeThemeKind::Auto, concrete);
        match STATE.compare_exchange_weak(
            observed,
            replacement,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(concrete),
            Err(actual) => observed = actual,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    STATE.store(
        pack(CrabCodeThemeKind::Dark, CrabCodeThemeKind::Dark),
        Ordering::Release,
    );
    set_terminal_native_lock(false);
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_lock() -> &'static std::sync::Mutex<()> {
    &TEST_LOCK
}

/// Pin deterministic theme state for exact layout/render tests.
#[cfg(any(test, feature = "test-support"))]
pub fn pin_theme() -> std::sync::MutexGuard<'static, ()> {
    let guard = test_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_for_test();
    apply_setting(CrabCodeThemeKind::Dark);
    let _ = super::color_support::set(super::color_support::ColorLevel::TrueColor);
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_test_state(test: impl FnOnce()) {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_for_test();
        system_appearance::clear_mock();
        test();
        system_appearance::clear_mock();
        reset_for_test();
    }

    #[test]
    fn explicit_setting_round_trips_without_auto_mode() {
        with_test_state(|| {
            assert_eq!(
                apply_setting(CrabCodeThemeKind::LightDaltonized),
                CrabCodeThemeKind::LightDaltonized,
            );
            assert_eq!(current_setting(), CrabCodeThemeKind::LightDaltonized);
            assert_eq!(current_kind(), CrabCodeThemeKind::LightDaltonized);
            assert!(!is_auto_mode());
        });
    }

    #[test]
    fn runtime_auto_resolves_light_dark_and_failure_without_osc11() {
        with_test_state(|| {
            system_appearance::set_mock(Some(SystemAppearance::Light));
            assert_eq!(
                apply_setting(CrabCodeThemeKind::Auto),
                CrabCodeThemeKind::Light,
            );
            assert_eq!(current_setting(), CrabCodeThemeKind::Auto);
            assert_eq!(current_kind(), CrabCodeThemeKind::Light);
            assert!(is_auto_mode());

            system_appearance::set_mock(Some(SystemAppearance::Dark));
            assert_eq!(
                apply_runtime_appearance(system_appearance::detect()),
                Some(CrabCodeThemeKind::Dark),
            );
            assert_eq!(current_kind(), CrabCodeThemeKind::Dark);

            system_appearance::set_mock(None);
            assert_eq!(
                apply_runtime_appearance(system_appearance::detect()),
                Some(CrabCodeThemeKind::Dark),
            );
        });
    }

    #[test]
    fn late_watcher_tick_cannot_overwrite_explicit_selection() {
        with_test_state(|| {
            system_appearance::set_mock(Some(SystemAppearance::Dark));
            apply_setting(CrabCodeThemeKind::Auto);
            apply_setting(CrabCodeThemeKind::LightAnsi);
            assert_eq!(apply_runtime_appearance(Some(SystemAppearance::Dark)), None,);
            assert_eq!(current_setting(), CrabCodeThemeKind::LightAnsi);
            assert_eq!(current_kind(), CrabCodeThemeKind::LightAnsi);
        });
    }

    #[test]
    fn packed_state_never_exposes_auto_as_concrete_kind() {
        with_test_state(|| {
            STATE.store(
                pack(CrabCodeThemeKind::Auto, CrabCodeThemeKind::Auto),
                Ordering::Release,
            );
            assert_eq!(current_setting(), CrabCodeThemeKind::Auto);
            assert_eq!(current_kind(), CrabCodeThemeKind::Dark);
        });
    }

    #[test]
    fn terminal_native_lock_pins_nominal_kind_and_restores_cached_kind() {
        with_test_state(|| {
            apply_setting(CrabCodeThemeKind::LightDaltonized);
            set_terminal_native_lock(true);
            assert!(terminal_native_locked());
            assert_eq!(current_kind(), CrabCodeThemeKind::Dark);
            assert_eq!(current_setting(), CrabCodeThemeKind::LightDaltonized);
            assert!(crabcode_markdown_renderer::polarity_safe_syntax());

            set_terminal_native_lock(false);
            assert!(!terminal_native_locked());
            assert_eq!(current_kind(), CrabCodeThemeKind::LightDaltonized);
            assert!(!crabcode_markdown_renderer::polarity_safe_syntax());
        });
    }
}
