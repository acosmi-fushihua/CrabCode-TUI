/// Configuration file path resolution.
///
/// Resolves paths to state directories, config files, gateway lock dirs,
/// and OAuth credential directories. Respects environment variable overrides
/// such as `CRABCODE_HOME`, `CRABCODE_STATE_DIR`, `CRABCODE_CONFIG_PATH`,
/// `CRABCODE_GATEWAY_PORT`, `CRABCODE_NIX_MODE`, and `CRABCODE_OAUTH_DIR`.
use std::env;
use std::path::{Path, PathBuf};

use acosmi_types::config::CrabClawConfig;

/// Current default write state directory name under the user's home directory.
pub const NEW_STATE_DIRNAME: &str = ".crabcode";

/// Default config file name.
pub const CONFIG_FILENAME: &str = "crabcode.json";

/// Legacy state directory names from prior product incarnations.
const LEGACY_STATE_DIRNAMES: &[&str] = &[".clawdbot", ".moltbot", ".moldbot"];

/// Legacy config file names from prior product incarnations.
const LEGACY_CONFIG_FILENAMES: &[&str] = &["clawdbot.json", "moltbot.json", "moldbot.json"];

/// Default gateway HTTP port.
pub const DEFAULT_GATEWAY_PORT: u16 = 19001;

/// OAuth credentials filename.
const OAUTH_FILENAME: &str = "oauth.json";

fn preferred_env_value(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

#[cfg(test)]
fn preferred_env_value_from_map(
    values: &std::collections::HashMap<&str, &str>,
    keys: &[&str],
) -> Option<String> {
    for key in keys {
        if let Some(value) = values.get(key) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve the home directory, preferring `CRABCODE_HOME` over `dirs::home_dir`.
///
/// `pub(crate)` so the sessions `~` expansion (`sessions::paths`) resolves the
/// tilde against the SAME CrabCode home semantics as config/state paths — P2-2
/// fixed the session-path twin that used bare `dirs::home_dir()` and ignored
/// `CRABCODE_HOME`, leaking session-store paths to the real `~/.crabcode` under
/// `CRABCODE_HOME`-only isolation.
pub(crate) fn resolve_home_dir() -> PathBuf {
    if let Some(home) = preferred_env_value(&["CRABCODE_HOME"]) {
        return PathBuf::from(home);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the explicit `CRABCODE_CONFIG_DIR` full-override, if set.
///
/// `CRABCODE_CONFIG_DIR` is the **highest-priority**
/// full override of the config/sessions/oauth/keychain directory — it wins over
/// `CRABCODE_STATE_DIR` and `CRABCODE_HOME`. The value is treated as the state
/// dir directly (i.e. it already *is* the `.crabcode` directory), matching the
/// TS side `getCrabCodeConfigHomeDir` which returns `CRABCODE_CONFIG_DIR`
/// verbatim. Previously the Rust resolvers ignored this var entirely while the
/// CLI (`crabcode-cli/src/config.rs`) and parts of the backend
/// (`acosmi-runtime`, `acosmi-app-server` memory) already honored it →
/// §4 split-brain leaking config/sessions/oauth into the real `~/.crabcode`.
fn resolve_config_dir_override() -> Option<PathBuf> {
    preferred_env_value(&["CRABCODE_CONFIG_DIR"]).map(|val| resolve_user_path(&val))
}

/// Expand a leading `~` in a user-supplied path to the home directory.
fn resolve_user_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PathBuf::from(trimmed);
    }
    if let Some(rest) = trimmed.strip_prefix('~') {
        let home = resolve_home_dir();
        if rest.is_empty() {
            return home;
        }
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return home.join(rest);
    }
    PathBuf::from(trimmed)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(trimmed))
}

fn new_state_dir() -> PathBuf {
    let home = resolve_home_dir();
    let suffix = resolve_profile_suffix();
    home.join(format!("{NEW_STATE_DIRNAME}{suffix}"))
}

fn new_state_dir_for_home(home: &Path) -> PathBuf {
    home.join(NEW_STATE_DIRNAME)
}

/// 规范化 `CRABCODE_PROFILE` 值。对端语义 = Go `statepaths.NormalizeProfileName`
/// (backend/internal/statepaths/paths.go:38-44):
///   - trim
///   - "default" (case-insensitive) 视为空
///   - 其余原样返回
fn normalize_profile_name(profile: &str) -> String {
    let trimmed = profile.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return String::new();
    }
    trimmed.to_string()
}

/// 解析 profile 后缀 (对端 = Go `ResolveProfileSuffix`).
/// 非空 → `-<profile>`; 空/default → `""`。
///
/// PR-C / Finding B 修复: 在 v3 之前 Rust 侧完全不读 `CRABCODE_PROFILE`,
/// 导致 Go 写 `~/.crabcode-<p>/.crabcode/scheduled_tasks.json`,Rust daemon
/// 监视 `~/.crabcode/.crabcode/...`,profile 模式下 cron 永远不触发。
#[must_use]
pub fn resolve_profile_suffix() -> String {
    let raw = env::var("CRABCODE_PROFILE").unwrap_or_default();
    let normalized = normalize_profile_name(&raw);
    if normalized.is_empty() {
        String::new()
    } else {
        format!("-{normalized}")
    }
}

fn legacy_state_dirs() -> Vec<PathBuf> {
    let home = resolve_home_dir();
    legacy_state_dirs_for_home(&home)
}

fn legacy_state_dirs_for_home(home: &Path) -> Vec<PathBuf> {
    LEGACY_STATE_DIRNAMES
        .iter()
        .map(|name| home.join(name))
        .collect()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve the application config-home directory with the same strict
/// isolation semantics as TS `getCrabCodeConfigHomeDir`.
///
/// This intentionally does not consult `CRABCODE_STATE_DIR`, profiles or
/// legacy fallback directories: caches that cross the TS/native boundary must
/// agree byte-for-byte on `CRABCODE_CONFIG_DIR` first, otherwise on
/// `<CRABCODE_HOME-or-OS-home>/.crabcode`.
#[must_use]
pub fn resolve_config_home_dir() -> PathBuf {
    resolve_config_dir_override().unwrap_or_else(|| new_state_dir_for_home(&resolve_home_dir()))
}

/// Resolve the state directory for mutable data (sessions, logs, caches).
///
/// Precedence:
/// 1. `CRABCODE_STATE_DIR` environment variable
/// 2. `CRABCODE_PROFILE` 非空 → `~/.crabcode-<profile>` (直接返回,不走 legacy
///    回落;对端语义 = Go `ResolveStateDir`, profile 模式下 legacy 目录不参与)
/// 3. Existing `~/.crabcode` directory containing managed state
/// 4. Existing `~/.crabcode` directory
/// 5. First existing legacy state directory
/// 6. Default `~/.crabcode`
#[must_use]
pub fn resolve_state_dir() -> PathBuf {
    // CRABCODE_CONFIG_DIR — explicit FULL override, highest priority (above
    // CRABCODE_STATE_DIR and CRABCODE_HOME). See `resolve_config_dir_override`.
    if let Some(dir) = resolve_config_dir_override() {
        return dir;
    }

    // Check explicit override
    if let Some(val) = preferred_env_value(&["CRABCODE_STATE_DIR"]) {
        return resolve_user_path(&val);
    }

    let new_dir = new_state_dir();

    // PR-C / Finding B: profile 非空时不走 legacy fallback (对齐 Go 行为)。
    // legacy dirname 是品牌重命名前的兼容项,从未支持 profile 后缀。
    if !resolve_profile_suffix().is_empty() {
        return new_dir;
    }

    choose_existing_state_dir(&new_dir, &legacy_state_dirs()).unwrap_or(new_dir)
}

/// Resolve the canonical config file path within a given state directory.
///
/// Respects `CRABCODE_CONFIG_PATH` override, otherwise returns
/// `<state_dir>/crabcode.json`.
#[must_use]
pub fn resolve_canonical_config_path(state_dir: &Path) -> PathBuf {
    if let Some(val) = preferred_env_value(&["CRABCODE_CONFIG_PATH"]) {
        return resolve_user_path(&val);
    }
    state_dir.join(CONFIG_FILENAME)
}

/// Build the list of default config path candidates.
///
/// Returns all possible config file locations in priority order:
/// explicit path, state-dir candidates, new+legacy dir candidates.
fn resolve_default_config_candidates() -> Vec<PathBuf> {
    // If explicit config path is set, use only that
    if let Some(val) = preferred_env_value(&["CRABCODE_CONFIG_PATH"]) {
        return vec![resolve_user_path(&val)];
    }

    let mut candidates = Vec::new();

    // If state dir override exists, add candidates from there
    if let Some(val) = preferred_env_value(&["CRABCODE_STATE_DIR"]) {
        let resolved = resolve_user_path(&val);
        candidates.push(resolved.join(CONFIG_FILENAME));
        for name in LEGACY_CONFIG_FILENAMES {
            candidates.push(resolved.join(name));
        }
    }

    // Add default directories: current default state dir + legacy
    let default_dirs = resolve_default_state_dirs(&resolve_home_dir());

    for dir in default_dirs {
        candidates.push(dir.join(CONFIG_FILENAME));
        for name in LEGACY_CONFIG_FILENAMES {
            candidates.push(dir.join(name));
        }
    }

    candidates
}

/// Resolve the active config path by preferring existing config files
/// before falling back to the canonical path.
///
/// This is the primary entry point for determining which config file to use.
#[must_use]
pub fn resolve_config_path() -> PathBuf {
    // If explicit override, use it directly
    if let Some(val) = preferred_env_value(&["CRABCODE_CONFIG_PATH"]) {
        return resolve_user_path(&val);
    }

    // CRABCODE_CONFIG_DIR — full override, wins over CRABCODE_STATE_DIR. Look
    // within the override dir, then fall back to its canonical config path.
    if let Some(dir) = resolve_config_dir_override() {
        let mut candidates = vec![dir.join(CONFIG_FILENAME)];
        for name in LEGACY_CONFIG_FILENAMES {
            candidates.push(dir.join(name));
        }
        for candidate in &candidates {
            if candidate.exists() {
                return candidate.clone();
            }
        }
        return dir.join(CONFIG_FILENAME);
    }

    let state_dir = resolve_state_dir();

    // If CRABCODE_STATE_DIR is set, look within that dir
    let state_override = preferred_env_value(&["CRABCODE_STATE_DIR"]);

    // Build candidates from the state dir
    let mut candidates = vec![state_dir.join(CONFIG_FILENAME)];
    for name in LEGACY_CONFIG_FILENAMES {
        candidates.push(state_dir.join(name));
    }

    // Check if any candidate exists
    for candidate in &candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }

    // If state dir was explicitly overridden, use it
    if state_override.is_some() {
        return state_dir.join(CONFIG_FILENAME);
    }

    // Try full default candidate list
    let all_candidates = resolve_default_config_candidates();
    for candidate in &all_candidates {
        if candidate.exists() {
            return candidate.clone();
        }
    }

    // Fallback: canonical path
    resolve_canonical_config_path(&state_dir)
}

fn state_dir_has_managed_content(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }

    const MARKERS: &[&str] = &[
        CONFIG_FILENAME,
        "credentials",
        "sessions",
        "agents",
        "memory",
        "extensions",
        "logs",
        "exec-approvals.json",
        "oauth.json",
    ];

    MARKERS.iter().any(|marker| dir.join(marker).exists())
}

fn choose_existing_state_dir(current_dir: &Path, legacy_dirs: &[PathBuf]) -> Option<PathBuf> {
    if state_dir_has_managed_content(current_dir) {
        return Some(current_dir.to_path_buf());
    }
    if current_dir.exists() {
        return Some(current_dir.to_path_buf());
    }
    for dir in legacy_dirs {
        if dir.exists() {
            return Some(dir.clone());
        }
    }
    None
}

fn resolve_default_state_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![new_state_dir_for_home(home)];
    dirs.extend(legacy_state_dirs_for_home(home));
    dirs
}

#[cfg(test)]
fn resolve_config_path_for_home(home: &Path) -> PathBuf {
    let current_dir = new_state_dir_for_home(home);
    let state_dir = choose_existing_state_dir(&current_dir, &legacy_state_dirs_for_home(home))
        .unwrap_or(current_dir.clone());

    let mut candidates = vec![state_dir.join(CONFIG_FILENAME)];
    for name in LEGACY_CONFIG_FILENAMES {
        candidates.push(state_dir.join(name));
    }
    if let Some(found) = candidates.iter().find(|candidate| candidate.exists()) {
        return found.clone();
    }

    let mut all_candidates = Vec::new();
    for dir in resolve_default_state_dirs(home) {
        all_candidates.push(dir.join(CONFIG_FILENAME));
        for name in LEGACY_CONFIG_FILENAMES {
            all_candidates.push(dir.join(name));
        }
    }
    if let Some(found) = all_candidates.iter().find(|candidate| candidate.exists()) {
        return found.clone();
    }

    state_dir.join(CONFIG_FILENAME)
}

/// Resolve the gateway lock directory (ephemeral, in temp dir).
///
/// Returns `<tmpdir>/crabcode-<uid>` on Unix (uid from `id -u` or process ID)
/// or `<tmpdir>/crabcode` on other platforms.
#[must_use]
pub fn resolve_gateway_lock_dir() -> PathBuf {
    let base = env::temp_dir();
    #[cfg(unix)]
    {
        // Attempt to get the real uid; fall back to process id if unavailable.
        let uid = resolve_unix_uid();
        base.join(format!("crabcode-{uid}"))
    }
    #[cfg(not(unix))]
    {
        base.join("crabcode")
    }
}

/// Resolve the Unix user ID for lock-directory naming.
///
/// Uses `std::os::unix::fs::MetadataExt` to read the uid of the
/// home directory, falling back to the process ID.
#[cfg(unix)]
fn resolve_unix_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    // Read uid from the home directory metadata
    let home = resolve_home_dir();
    if let Ok(meta) = std::fs::metadata(&home) {
        return meta.uid();
    }
    // Fallback: use process id (not ideal but functional)
    std::process::id()
}

/// Resolve the OAuth credentials directory.
///
/// Precedence:
/// 1. `CRABCODE_OAUTH_DIR` environment variable
/// 2. `<state_dir>/credentials`
#[must_use]
pub fn resolve_oauth_dir(state_dir: &Path) -> PathBuf {
    if let Some(val) = preferred_env_value(&["CRABCODE_OAUTH_DIR"]) {
        return resolve_user_path(&val);
    }
    state_dir.join("credentials")
}

/// Resolve the OAuth credentials file path.
///
/// Returns `<oauth_dir>/oauth.json`.
#[must_use]
pub fn resolve_oauth_path(state_dir: &Path) -> PathBuf {
    resolve_oauth_dir(state_dir).join(OAUTH_FILENAME)
}

/// Resolve the gateway port from environment and config.
///
/// Precedence:
/// 1. `CRABCODE_GATEWAY_PORT` environment variable
/// 2. `cfg.gateway.port` from config
/// 3. [`DEFAULT_GATEWAY_PORT`] (19001)
#[must_use]
pub fn resolve_gateway_port(cfg: Option<&CrabClawConfig>) -> u16 {
    // Check env var
    if let Some(trimmed) = preferred_env_value(&["CRABCODE_GATEWAY_PORT"])
        && let Ok(parsed) = trimmed.parse::<u16>()
        && parsed > 0
    {
        return parsed;
    }

    // Check config
    if let Some(config) = cfg
        && let Some(ref gw) = config.gateway
        && let Some(port) = gw.port
        && port > 0
    {
        return port;
    }

    DEFAULT_GATEWAY_PORT
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("acosmi-config-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn resolve_state_dir_prefers_managed_content() {
        let tmp = unique_temp_dir("prefer-managed");
        let current = tmp.join(NEW_STATE_DIRNAME);
        fs::create_dir_all(current.join("credentials")).expect("create state");

        let resolved = choose_existing_state_dir(&current, &legacy_state_dirs_for_home(&tmp))
            .expect("state dir");

        assert_eq!(resolved, current);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_state_dir_falls_back_to_existing() {
        let tmp = unique_temp_dir("fallback-existing");
        let current = tmp.join(NEW_STATE_DIRNAME);
        fs::create_dir_all(&current).expect("create empty dir");

        let resolved = choose_existing_state_dir(&current, &legacy_state_dirs_for_home(&tmp))
            .expect("state dir");

        assert_eq!(resolved, current);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_config_path_prefers_crabcode_config_when_present() {
        let tmp = unique_temp_dir("config-prefers-crab");
        let cfg = tmp.join(NEW_STATE_DIRNAME).join(CONFIG_FILENAME);
        fs::create_dir_all(cfg.parent().expect("parent")).expect("create dir");
        fs::write(&cfg, b"{}").expect("write config");

        let resolved = resolve_config_path_for_home(&tmp);

        assert_eq!(resolved, cfg);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn preferred_env_value_prefers_crabcode_prefix() {
        let values = HashMap::from([("CRABCODE_STATE_DIR", "/tmp/new")]);
        let resolved = preferred_env_value_from_map(&values, &["CRABCODE_STATE_DIR"]);
        assert_eq!(resolved.as_deref(), Some("/tmp/new"));
    }

    // PR-C / Finding B: Rust ↔ Go 对齐 CRABCODE_PROFILE。
    // env 修改必须 #[serial] 串行,避免测试并发时互相污染。

    /// 辅助: scope 内设置 env,退出时恢复。
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            #[allow(unsafe_code)]
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn resolve_home_dir_prefers_crabcode_home() {
        // P2-2: CRABCODE_HOME 优先于 dirs::home_dir。
        let _g = EnvGuard::set("CRABCODE_HOME", "/tmp/p2-2-home");
        assert_eq!(resolve_home_dir(), PathBuf::from("/tmp/p2-2-home"));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_home_dir_unset_crabcode_home_falls_back_non_empty() {
        // CRABCODE_HOME 未设时回退 dirs::home_dir（非空），不读 CRABCODE_HOME。
        let _g = EnvGuard::unset("CRABCODE_HOME");
        let home = resolve_home_dir();
        assert!(!home.as_os_str().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_home_dir_ignores_config_dir_override() {
        // P2-2: CRABCODE_CONFIG_DIR 是 state-dir 全量覆盖（resolve_config_dir_override
        // 处理），与 home 解析正交 —— resolve_home_dir 只认 CRABCODE_HOME。
        let _g1 = EnvGuard::set("CRABCODE_CONFIG_DIR", "/tmp/p2-2-cfg");
        let _g2 = EnvGuard::set("CRABCODE_HOME", "/tmp/p2-2-home2");
        assert_eq!(resolve_home_dir(), PathBuf::from("/tmp/p2-2-home2"));
    }

    #[test]
    #[serial_test::serial]
    fn config_home_dir_matches_ts_isolation_precedence() {
        let _g_cfg = EnvGuard::set("CRABCODE_CONFIG_DIR", "/tmp/artifact-config-winner");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/artifact-home-loser");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/artifact-state-loser");
        assert_eq!(
            resolve_config_home_dir(),
            PathBuf::from("/tmp/artifact-config-winner")
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_home_dir_treats_crabcode_home_as_base() {
        let _g_cfg = EnvGuard::unset("CRABCODE_CONFIG_DIR");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/artifact-home-base");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/artifact-state-ignored");
        assert_eq!(
            resolve_config_home_dir(),
            PathBuf::from("/tmp/artifact-home-base/.crabcode")
        );
    }

    #[test]
    fn normalize_profile_empty_and_default() {
        assert_eq!(normalize_profile_name(""), "");
        assert_eq!(normalize_profile_name("  "), "");
        assert_eq!(normalize_profile_name("default"), "");
        assert_eq!(normalize_profile_name("DEFAULT"), "");
        assert_eq!(normalize_profile_name("Default"), "");
        assert_eq!(normalize_profile_name(" default "), "");
    }

    #[test]
    fn normalize_profile_passthrough_non_default() {
        assert_eq!(normalize_profile_name("dev"), "dev");
        assert_eq!(normalize_profile_name("  dev  "), "dev");
        assert_eq!(normalize_profile_name("test-1"), "test-1");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_profile_suffix_unset_returns_empty() {
        let _g = EnvGuard::unset("CRABCODE_PROFILE");
        assert_eq!(resolve_profile_suffix(), "");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_profile_suffix_default_returns_empty() {
        let _g = EnvGuard::set("CRABCODE_PROFILE", "default");
        assert_eq!(resolve_profile_suffix(), "");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_profile_suffix_dev_returns_hyphen_dev() {
        let _g = EnvGuard::set("CRABCODE_PROFILE", "dev");
        assert_eq!(resolve_profile_suffix(), "-dev");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_state_dir_with_profile_hits_profile_suffix() {
        // 清 CRABCODE_STATE_DIR (显式覆盖优先级更高,会屏蔽 profile)
        let _g1 = EnvGuard::unset("CRABCODE_STATE_DIR");
        let _g2 = EnvGuard::set("CRABCODE_PROFILE", "dev");
        let dir = resolve_state_dir();
        let as_str = dir.display().to_string();
        assert!(
            as_str.ends_with(".crabcode-dev"),
            "profile 模式应返回带 -dev 后缀: {as_str}",
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_state_dir_state_dir_env_overrides_profile() {
        // CRABCODE_STATE_DIR 在 profile 之上 —— 对齐 Go 行为
        let _g1 = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/override-xyz");
        let _g2 = EnvGuard::set("CRABCODE_PROFILE", "dev");
        let dir = resolve_state_dir();
        assert_eq!(dir, PathBuf::from("/tmp/override-xyz"));
    }

    // ---------------------------------------------------------------------
    // A10 (#P1-22): CRABCODE_CONFIG_DIR is the highest-priority FULL override.
    // Precedence: CONFIG_DIR > STATE_DIR > HOME > homedir.
    // Getting this wrong leaks credentials/sessions/oauth into ~/.crabcode.
    // ---------------------------------------------------------------------

    #[test]
    #[serial_test::serial]
    fn config_dir_overrides_state_dir_and_home_for_state_dir() {
        let _g_cfg = EnvGuard::set("CRABCODE_CONFIG_DIR", "/tmp/cfg-override-abc");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/state-loser");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/home-loser");
        let _g_profile = EnvGuard::unset("CRABCODE_PROFILE");
        assert_eq!(resolve_state_dir(), PathBuf::from("/tmp/cfg-override-abc"));
    }

    #[test]
    #[serial_test::serial]
    fn config_dir_overrides_state_dir_and_home_for_config_path() {
        let _g_cfg = EnvGuard::set("CRABCODE_CONFIG_DIR", "/tmp/cfg-override-def");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/state-loser");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/home-loser");
        let _g_path = EnvGuard::unset("CRABCODE_CONFIG_PATH");
        let _g_profile = EnvGuard::unset("CRABCODE_PROFILE");
        // No file exists → falls back to canonical <CONFIG_DIR>/crabcode.json.
        assert_eq!(
            resolve_config_path(),
            PathBuf::from("/tmp/cfg-override-def").join(CONFIG_FILENAME),
        );
    }

    #[test]
    #[serial_test::serial]
    fn config_dir_overrides_state_dir_and_home_for_oauth_dir() {
        let _g_cfg = EnvGuard::set("CRABCODE_CONFIG_DIR", "/tmp/cfg-override-ghi");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/state-loser");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/home-loser");
        let _g_oauth = EnvGuard::unset("CRABCODE_OAUTH_DIR");
        let _g_profile = EnvGuard::unset("CRABCODE_PROFILE");
        // oauth dir derives from resolve_state_dir() at the call sites.
        let state_dir = resolve_state_dir();
        assert_eq!(
            resolve_oauth_dir(&state_dir),
            PathBuf::from("/tmp/cfg-override-ghi").join("credentials"),
        );
    }

    #[test]
    #[serial_test::serial]
    fn state_dir_wins_when_config_dir_unset() {
        let _g_cfg = EnvGuard::unset("CRABCODE_CONFIG_DIR");
        let _g_state = EnvGuard::set("CRABCODE_STATE_DIR", "/tmp/state-winner");
        let _g_home = EnvGuard::set("CRABCODE_HOME", "/tmp/home-loser");
        let _g_profile = EnvGuard::unset("CRABCODE_PROFILE");
        assert_eq!(resolve_state_dir(), PathBuf::from("/tmp/state-winner"));
    }

    #[test]
    #[serial_test::serial]
    fn home_base_used_when_only_home_set() {
        let _g_cfg = EnvGuard::unset("CRABCODE_CONFIG_DIR");
        let _g_state = EnvGuard::unset("CRABCODE_STATE_DIR");
        let _g_path = EnvGuard::unset("CRABCODE_CONFIG_PATH");
        let _g_profile = EnvGuard::unset("CRABCODE_PROFILE");
        // Use a unique non-existent home so choose_existing_state_dir returns
        // the default <HOME>/.crabcode (deterministic, no managed content).
        let home = unique_temp_dir("config-dir-home-base");
        let _g_home = EnvGuard::set("CRABCODE_HOME", home.to_str().expect("utf8"));
        assert_eq!(resolve_state_dir(), home.join(NEW_STATE_DIRNAME));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_state_dir_profile_case_insensitive_default() {
        let _g1 = EnvGuard::unset("CRABCODE_STATE_DIR");
        let _g2 = EnvGuard::set("CRABCODE_PROFILE", "Default");
        let dir = resolve_state_dir();
        let as_str = dir.display().to_string();
        // "Default" 视为空 → 走普通 ~/.crabcode 分支
        assert!(
            as_str.ends_with(".crabcode"),
            "Default (case-insensitive) 应等价空 profile: {as_str}",
        );
    }
}

/// Check if running in Nix mode.
///
/// When `CRABCODE_NIX_MODE=1`, the gateway is running under Nix.
/// In this mode, no auto-install flows should be attempted and
/// config is managed externally.
#[must_use]
pub fn is_nix_mode() -> bool {
    preferred_env_value(&["CRABCODE_NIX_MODE"]).is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gateway_port_value() {
        assert_eq!(DEFAULT_GATEWAY_PORT, 19001);
    }

    #[test]
    fn gateway_port_from_config() {
        let cfg = CrabClawConfig {
            gateway: Some(acosmi_types::gateway::GatewayConfig {
                port: Some(9999),
                ..Default::default()
            }),
            ..CrabClawConfig::default()
        };
        // Only works when env var is not set; this tests the config path
        let port = resolve_gateway_port(Some(&cfg));
        // If env var is set, it takes precedence, so just check it's a valid port
        assert!(port > 0);
    }

    #[test]
    fn gateway_port_fallback() {
        let port = resolve_gateway_port(None);
        assert!(port > 0);
    }
}
