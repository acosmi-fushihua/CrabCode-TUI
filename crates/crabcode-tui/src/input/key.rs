use crossterm::event::KeyModifiers;

// On Windows, AltGr arrives as Ctrl+Alt; on other platforms it's composed before reaching us.
#[cfg(target_os = "windows")]
#[inline]
pub fn is_altgr(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(not(target_os = "windows"))]
#[inline]
pub fn is_altgr(_modifiers: KeyModifiers) -> bool {
    false
}

// Renderer-private module boundary.
