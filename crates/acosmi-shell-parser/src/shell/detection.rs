//! Shell 检测
//!
//! 等价于 TS resolveDefaultShell.ts + powershellDetection.ts

use std::sync::OnceLock;

use crate::ShellType;

/// 解析默认 shell
///
/// Always returns Bash, even on Windows. This matches the TS behavior where
/// `resolveDefaultShell()` does NOT auto-flip to `PowerShell` on Windows.
/// The caller (or user config) is responsible for explicitly selecting `PowerShell`
/// if desired.
#[must_use]
pub const fn resolve_default_shell() -> ShellType {
    ShellType::Bash
}

/// `PowerShell` 版本
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerShellEdition {
    /// `PowerShell` Core 7+ (pwsh)
    Core,
    /// Windows `PowerShell` 5.1 (powershell.exe)
    Desktop,
}

/// Cached result of `find_powershell_sync` to avoid repeated filesystem probes.
static CACHED_PWSH_PATH: OnceLock<Option<String>> = OnceLock::new();

/// 查找 `PowerShell` 路径（同步版本）
///
/// Results are cached via `OnceLock` so the filesystem is only probed once.
pub fn find_powershell_sync() -> Option<String> {
    CACHED_PWSH_PATH
        .get_or_init(find_powershell_sync_inner)
        .clone()
}

/// Inner implementation without caching.
fn find_powershell_sync_inner() -> Option<String> {
    // 先尝试 pwsh (PowerShell Core)
    if let Ok(path) = which::which("pwsh") {
        let path_str = path.to_string_lossy().to_string();

        // Linux: 避免 snap launcher (snap shims have issues with certain operations)
        #[cfg(target_os = "linux")]
        if let Ok(real) = std::fs::canonicalize(&path) {
            if real.to_string_lossy().contains("/snap/") {
                // 直接检查常见路径
                let direct = "/opt/microsoft/powershell/7/pwsh";
                if std::path::Path::new(direct).exists() {
                    return Some(direct.to_string());
                }
            }
        }

        return Some(path_str);
    }

    // Try `powershell` on ALL platforms (not just Windows).
    // On Windows this finds powershell.exe (Windows PowerShell 5.1).
    // On other platforms it may find a symlink or wrapper.
    if let Ok(path) = which::which("powershell") {
        return Some(path.to_string_lossy().to_string());
    }

    // Linux: 直接检查常见路径
    #[cfg(target_os = "linux")]
    {
        let direct_paths = ["/opt/microsoft/powershell/7/pwsh", "/usr/bin/pwsh"];
        for direct in &direct_paths {
            if std::path::Path::new(direct).exists() {
                return Some(direct.to_string());
            }
        }
    }

    None
}

/// Reset the cached PowerShell path. Intended for testing only.
#[cfg(test)]
pub fn reset_powershell_cache() {
    // OnceLock does not support reset directly. For testing, we accept that
    // the cache persists across tests within the same process. In practice,
    // tests that need different cache states should use dependency injection
    // or call find_powershell_sync_inner directly.
    //
    // If true reset is needed, the caller should use find_powershell_sync_inner().
    // This function exists as a placeholder matching the TS resetPowershellCache().
}

/// Allow test code to call the uncached version directly.
#[cfg(test)]
pub fn find_powershell_sync_uncached() -> Option<String> {
    find_powershell_sync_inner()
}

/// 获取 `PowerShell` 版本
///
/// Extracts the basename from the path and does an exact match (case-insensitive)
/// rather than a substring `contains()` check. This prevents false positives like
/// a path containing "pwsh" in a directory name matching incorrectly.
#[must_use]
pub fn get_powershell_edition(path: &str) -> Option<PowerShellEdition> {
    // 手动按 '/' 和 '\' 分割，确保 macOS/Linux 也能解析 Windows 路径
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    // 去掉扩展名（如 .exe）
    let stem = basename.rsplit_once('.').map_or(basename, |(name, _)| name);
    let stem_lower = stem.to_lowercase();

    if stem_lower == "pwsh" {
        Some(PowerShellEdition::Core)
    } else if stem_lower == "powershell" {
        Some(PowerShellEdition::Desktop)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_default_shell_always_bash() {
        // TS behavior: always returns Bash, even on Windows
        let shell = resolve_default_shell();
        assert_eq!(shell, ShellType::Bash);
    }

    #[test]
    fn test_powershell_edition_pwsh() {
        assert_eq!(
            get_powershell_edition("/usr/bin/pwsh"),
            Some(PowerShellEdition::Core)
        );
        assert_eq!(
            get_powershell_edition("/usr/local/bin/pwsh"),
            Some(PowerShellEdition::Core)
        );
    }

    #[test]
    fn test_powershell_edition_desktop() {
        assert_eq!(
            get_powershell_edition(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            ),
            Some(PowerShellEdition::Desktop)
        );
    }

    #[test]
    fn test_powershell_edition_basename_exact_match() {
        // A path with "pwsh" in a directory name but a different binary should NOT match
        assert_eq!(get_powershell_edition("/usr/pwsh-dir/someshell"), None);
        // A path with "powershell" in a directory name but different binary should NOT match
        assert_eq!(
            get_powershell_edition("/opt/powershell/7/pwsh"),
            Some(PowerShellEdition::Core)
        );
    }

    #[test]
    fn test_powershell_edition_unknown() {
        assert_eq!(get_powershell_edition("/bin/bash"), None);
        assert_eq!(get_powershell_edition("/usr/bin/zsh"), None);
    }

    #[test]
    fn test_find_powershell_uncached() {
        // Just ensure the uncached version doesn't panic
        let _result = find_powershell_sync_uncached();
    }
}
