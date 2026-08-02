//! W-MEMORY-LIFECYCLE K10 (2026-07-09) — dream-watch (专项检测) configuration
//! plus the orchestrator-local `<base>` config-root resolution.
//!
//! # Storage
//!
//! Watch targets persist in `<base>/dream-watch.json` where `<base>` follows
//! the repo-wide TS↔Rust env contract (CLAUDE.md §硬约束 #4):
//! `CRABCODE_CONFIG_DIR` (explicit full override) → `<CRABCODE_HOME>/.crabcode`
//! (home base) → `<os home>/.crabcode`. The TUI client dispatcher resolves the
//! same base on its side when it injects `memory_dir` / `project_state_dir`
//! into `memory.watch.upsert`; the orchestrator resolves it independently so
//! the periodic tick can run watches with **no session context at all**.
//!
//! # Durability
//!
//! Saves go through [`crate::atomic_write::atomic_write`] (tmp + fsync +
//! rename). Loads are corrupt-tolerant: an unreadable or unparseable file
//! degrades to the empty default (fail-soft — a broken config must never
//! wedge the tick), and every field beyond the required identity/path set
//! carries a serde default so configs written by older builds keep loading.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// File name of the watch config inside `<base>`.
pub const WATCH_CONFIG_FILENAME: &str = "dream-watch.json";

/// Default watch cadence (hours) — mirrors the K5 "fixed 2-day rhythm".
pub const DEFAULT_WATCH_INTERVAL_HOURS: u64 = 48;

const CONFIG_DIR_ENV: &str = "CRABCODE_CONFIG_DIR";
const HOME_BASE_ENV: &str = "CRABCODE_HOME";
const STATE_DIRNAME: &str = ".crabcode";

fn default_version() -> u32 {
    1
}

fn default_interval_hours() -> u64 {
    DEFAULT_WATCH_INTERVAL_HOURS
}

fn default_enabled() -> bool {
    true
}

/// Top-level watch configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub targets: Vec<WatchTarget>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            targets: Vec::new(),
        }
    }
}

/// One watched path (专项检测 target). `memory_dir` / `project_state_dir`
/// are resolved and injected by the TUI client dispatcher at upsert time so
/// the tick needs no session context; the orchestrator only validates them
/// (non-empty + absolute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTarget {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Absolute path of the watched directory (the dream/imagination corpus
    /// root for this target).
    pub path: String,
    /// Absolute memory dir the watch dream chain anchors to.
    pub memory_dir: String,
    /// Absolute project-state dir (dream-config / derived-state parent).
    pub project_state_dir: String,
    #[serde(default = "default_interval_hours")]
    pub interval_hours: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
}

impl WatchTarget {
    /// Whether this target is due at `now_ms`. Disabled targets are never
    /// due; a target that never ran (`last_run_ms` absent or 0) is due
    /// immediately (mirrors the dream time-gate's `prior_mtime_ms == 0`
    /// pass-through). `interval_hours == 0` is defensively clamped to 1h so
    /// a hand-edited zero can't turn the 10-min tick into a hot loop.
    #[must_use]
    pub fn is_due(&self, now_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        match self.last_run_ms {
            None | Some(0) => true,
            Some(last) => {
                now_ms.saturating_sub(last) >= self.interval_hours.max(1).saturating_mul(3_600_000)
            }
        }
    }
}

/// `<base>/dream-watch.json` path for a given base dir.
#[must_use]
pub fn watch_config_path(base_dir: &Path) -> PathBuf {
    base_dir.join(WATCH_CONFIG_FILENAME)
}

/// Load the watch config from `<base>/dream-watch.json`. Missing file →
/// default (empty). Unreadable / corrupt file → default with a warn log
/// (fail-soft: a broken config never wedges the periodic tick or the IPC
/// management surface; the next successful save rewrites it whole).
#[must_use]
pub fn load_watch_config(base_dir: &Path) -> WatchConfig {
    let path = watch_config_path(base_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WatchConfig::default(),
        Err(e) => {
            log::warn!(
                "[watch] read {} failed (fail-soft, using empty config): {e}",
                path.display()
            );
            return WatchConfig::default();
        }
    };
    match serde_json::from_str::<WatchConfig>(&raw) {
        Ok(config) => config,
        Err(e) => {
            log::warn!(
                "[watch] parse {} failed (fail-soft, using empty config): {e}",
                path.display()
            );
            WatchConfig::default()
        }
    }
}

/// Persist the watch config atomically (tmp + fsync + rename; parent dirs
/// created as needed).
pub async fn save_watch_config(base_dir: &Path, config: &WatchConfig) -> Result<(), BoxError> {
    let bytes = serde_json::to_vec_pretty(config)?;
    atomic_write(&watch_config_path(base_dir), &bytes).await
}

/// Generate a fresh watch-target id: 16 random-ish hex chars derived from
/// (time, pid, per-process counter, entropy hint). No `uuid` dependency in
/// this crate; collision resistance across a single user's watch list is
/// ample.
#[must_use]
pub fn generate_watch_id(entropy_hint: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(
        format!("{now_ns}:{}:{count}:{entropy_hint}", std::process::id()).as_bytes(),
    );
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Resolve the CrabCode config base dir (`<base>`), mirroring the repo-wide
/// TS↔Rust contract (CLAUDE.md §硬约束 #4):
///
/// 1. `CRABCODE_CONFIG_DIR` — explicit **full** override, used verbatim.
/// 2. `CRABCODE_HOME` — home **base**; the config dir is
///    `<CRABCODE_HOME>/.crabcode`.
/// 3. OS home (`USERPROFILE` on Windows, `HOME` elsewhere — the same
///    variables Node's `os.homedir()` reads) joined with `.crabcode`.
///
/// Never fails: with no resolvable home at all it degrades to a relative
/// `.crabcode` (matching the honest-fallback style of
/// `dream_gate::project_state_dir_from_memory_dir`).
#[must_use]
pub fn resolve_base_dir() -> PathBuf {
    if let Some(dir) = non_empty_env(CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Some(home_base) = non_empty_env(HOME_BASE_ENV) {
        return PathBuf::from(home_base).join(STATE_DIRNAME);
    }
    os_home_dir()
        .map(|home| home.join(STATE_DIRNAME))
        .unwrap_or_else(|| PathBuf::from(STATE_DIRNAME))
}

fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// OS home directory from the platform env var (`USERPROFILE` on Windows,
/// `HOME` elsewhere) — the primary sources `os.homedir()` / `dirs::home_dir`
/// read, without pulling a new dependency into this workspace.
fn os_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let raw = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let raw = std::env::var_os("HOME");
    raw.filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// Restore-on-drop guard so env mutations can't leak across tests (also
    /// restores on panic).
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prior }
        }

        fn unset(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn sample_target(id: &str) -> WatchTarget {
        WatchTarget {
            id: id.to_string(),
            label: Some("repo watch".to_string()),
            path: "D:/somewhere/project".to_string(),
            memory_dir: "D:/somewhere/state/memory".to_string(),
            project_state_dir: "D:/somewhere/state".to_string(),
            interval_hours: 48,
            focus: Some("架构演进".to_string()),
            enabled: true,
            last_run_ms: Some(1_700_000_000_000),
            last_status: Some("ok: 2 themes".to_string()),
        }
    }

    #[tokio::test]
    async fn save_load_roundtrip_preserves_targets() {
        let base = TempDir::new().unwrap();
        let config = WatchConfig {
            version: 1,
            targets: vec![sample_target("aa11"), {
                let mut second = sample_target("bb22");
                second.label = None;
                second.focus = None;
                second.last_run_ms = None;
                second.last_status = None;
                second.enabled = false;
                second
            }],
        };

        save_watch_config(base.path(), &config).await.unwrap();
        let loaded = load_watch_config(base.path());

        assert_eq!(loaded, config);
        assert!(watch_config_path(base.path()).is_file());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let base = TempDir::new().unwrap();
        assert_eq!(load_watch_config(base.path()), WatchConfig::default());
    }

    #[test]
    fn load_corrupt_file_fail_softs_to_default() {
        let base = TempDir::new().unwrap();
        std::fs::write(watch_config_path(base.path()), "{not valid json!").unwrap();
        assert_eq!(load_watch_config(base.path()), WatchConfig::default());
    }

    /// Serde backward compatibility: a minimal target written by an older /
    /// hand-edited config (only identity + the three paths) loads with
    /// defaults for everything else.
    #[test]
    fn minimal_target_json_loads_with_defaults() {
        let raw = r#"{
            "targets": [{
                "id": "cc33",
                "path": "D:/w",
                "memory_dir": "D:/w/.state/memory",
                "project_state_dir": "D:/w/.state"
            }]
        }"#;
        let config: WatchConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.version, 1, "version defaults");
        let target = &config.targets[0];
        assert_eq!(target.interval_hours, DEFAULT_WATCH_INTERVAL_HOURS);
        assert!(target.enabled, "enabled defaults true");
        assert_eq!(target.label, None);
        assert_eq!(target.focus, None);
        assert_eq!(target.last_run_ms, None);
        assert_eq!(target.last_status, None);
    }

    #[test]
    fn is_due_honors_enabled_interval_and_never_ran() {
        let mut target = sample_target("dd44");
        let hour_ms = 3_600_000_u64;

        // Never ran → due immediately.
        target.last_run_ms = None;
        assert!(target.is_due(1));
        target.last_run_ms = Some(0);
        assert!(target.is_due(1));

        // Ran recently → not due until interval_hours elapse.
        target.interval_hours = 48;
        target.last_run_ms = Some(1_700_000_000_000);
        assert!(!target.is_due(1_700_000_000_000 + 47 * hour_ms));
        assert!(target.is_due(1_700_000_000_000 + 48 * hour_ms));

        // Disabled → never due.
        target.enabled = false;
        assert!(!target.is_due(u64::MAX));

        // Zero interval is clamped to 1h (defensive against hand edits).
        target.enabled = true;
        target.interval_hours = 0;
        target.last_run_ms = Some(1_700_000_000_000);
        assert!(!target.is_due(1_700_000_000_000 + hour_ms - 1));
        assert!(target.is_due(1_700_000_000_000 + hour_ms));
    }

    #[test]
    fn generate_watch_id_is_hex_and_unique_across_calls() {
        let a = generate_watch_id("D:/w");
        let b = generate_watch_id("D:/w");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "per-process counter must differentiate calls");
    }

    /// Base-dir resolution priority (§硬约束 #4). One combined test so the
    /// env mutations stay serialized within a single test body; the guards
    /// restore prior values (even on panic) so parallel tests reading OTHER
    /// vars are unaffected — nothing else in this crate reads these two.
    #[test]
    fn resolve_base_dir_priority_config_dir_then_home_then_os_home() {
        // 1. CRABCODE_CONFIG_DIR wins outright (full override, no suffix).
        {
            let _config = EnvGuard::set("CRABCODE_CONFIG_DIR", "D:/isolated/config-root");
            let _home = EnvGuard::set("CRABCODE_HOME", "D:/isolated/home-base");
            assert_eq!(
                resolve_base_dir(),
                PathBuf::from("D:/isolated/config-root"),
                "CRABCODE_CONFIG_DIR is the full config dir"
            );
        }

        // 2. CRABCODE_HOME is a home *base*: `.crabcode` is appended.
        {
            let _config = EnvGuard::unset("CRABCODE_CONFIG_DIR");
            let _home = EnvGuard::set("CRABCODE_HOME", "D:/isolated/home-base");
            assert_eq!(
                resolve_base_dir(),
                PathBuf::from("D:/isolated/home-base").join(".crabcode"),
            );
        }

        // 3. Neither set → OS home + `.crabcode` (do not mutate the real OS
        //    home var — just assert the suffix shape).
        {
            let _config = EnvGuard::unset("CRABCODE_CONFIG_DIR");
            let _home = EnvGuard::unset("CRABCODE_HOME");
            let resolved = resolve_base_dir();
            assert_eq!(
                resolved.file_name().and_then(|n| n.to_str()),
                Some(".crabcode"),
                "fallback lands in <home>/.crabcode, got {resolved:?}"
            );
        }

        // 4. Whitespace-only values are treated as unset.
        {
            let _config = EnvGuard::set("CRABCODE_CONFIG_DIR", "   ");
            let _home = EnvGuard::set("CRABCODE_HOME", "D:/isolated/home-base");
            assert_eq!(
                resolve_base_dir(),
                PathBuf::from("D:/isolated/home-base").join(".crabcode"),
            );
        }
    }
}
