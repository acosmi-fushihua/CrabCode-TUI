/// Browser configuration types.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserEngine {
    /// crabcode-browser (agent-browser fork) backend. The legacy `playwright`
    /// and `agent-browser` literals are accepted as aliases so old
    /// `~/.crabcode.json` configs keep deserializing and map to this backend
    /// (plan §2.13.A — PlaywrightCli runtime removed in PR-5; a serde alias
    /// avoids breaking `load_config()` on any config still carrying the old
    /// value). `rename_all = "lowercase"` would otherwise emit
    /// `crabcodebrowser`, so the wire form is pinned explicitly.
    #[serde(
        rename = "crabcode-browser",
        alias = "agent-browser",
        alias = "playwright"
    )]
    CrabcodeBrowser,
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserDriver {
    /// See `BrowserEngine::CrabcodeBrowser` — `playwright`/`agent-browser` are
    /// legacy aliases mapped here (plan §2.13.A).
    #[serde(
        rename = "crabcode-browser",
        alias = "agent-browser",
        alias = "playwright"
    )]
    CrabcodeBrowser,
    Extension,
    Gateway,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserProfileDriver {
    /// Renamed from the legacy `Playwright` variant (plan §2.13.A). Both the old
    /// `playwright` value and the older `crabcode` alias still deserialize here.
    #[serde(rename = "crabcode-browser", alias = "playwright", alias = "crabcode")]
    CrabcodeBrowser,
    Extension,
    Gateway,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<BrowserProfileDriver>,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BrowserSnapshotMode {
    Efficient,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<BrowserSnapshotMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDataDirs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts_dir: Option<String>,
}

/// Top-level browser configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConfig {
    // NB: 曾有 `enabled: Option<bool>` 死字段(从不被任何后端选择逻辑读取),给人
    // 「有开关」错觉(audit 2026-06-14 P2)。已移除以消除误导;后端选择只看
    // driver/engine。serde 忽略旧配置里残留的 "enabled" 键,无兼容性影响。
    // (与 evaluate_enabled 无关,后者是真实的 JS evaluate 能力开关,保留。)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<BrowserEngine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<BrowserDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluate_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cdp_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cdp_handshake_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_sandbox: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<HashMap<String, BrowserProfileConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dirs: Option<BrowserDataDirs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_defaults: Option<BrowserSnapshotDefaults>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_config_deserializes_engine_driver_and_dirs() {
        let raw = serde_json::json!({
            "engine": "playwright",
            "driver": "extension",
            "dataDirs": {
                "rootDir": "/tmp/browser",
                "profilesDir": "/tmp/browser/profiles",
                "cacheDir": "/tmp/browser/cache",
                "stateDir": "/tmp/browser/state",
                "artifactsDir": "/tmp/browser/artifacts"
            }
        });

        let config: BrowserConfig = serde_json::from_value(raw).expect("browser config");

        // Legacy `"playwright"` engine value maps to the crabcode-browser backend
        // (PlaywrightCli removed in PR-5; serde alias preserves old configs).
        assert_eq!(config.engine, Some(BrowserEngine::CrabcodeBrowser));
        assert_eq!(config.driver, Some(BrowserDriver::Extension));
        let dirs = config.data_dirs.expect("data dirs");
        assert_eq!(dirs.root_dir.as_deref(), Some("/tmp/browser"));
        assert_eq!(dirs.profiles_dir.as_deref(), Some("/tmp/browser/profiles"));
        assert_eq!(dirs.cache_dir.as_deref(), Some("/tmp/browser/cache"));
        assert_eq!(dirs.state_dir.as_deref(), Some("/tmp/browser/state"));
        assert_eq!(
            dirs.artifacts_dir.as_deref(),
            Some("/tmp/browser/artifacts")
        );
    }

    #[test]
    fn browser_driver_and_engine_accept_crabcode_browser_and_agent_browser_alias() {
        let driver: BrowserDriver = serde_json::from_str("\"crabcode-browser\"").expect("driver");
        assert_eq!(driver, BrowserDriver::CrabcodeBrowser);
        let aliased: BrowserDriver =
            serde_json::from_str("\"agent-browser\"").expect("driver alias");
        assert_eq!(aliased, BrowserDriver::CrabcodeBrowser);
        let engine: BrowserEngine = serde_json::from_str("\"crabcode-browser\"").expect("engine");
        assert_eq!(engine, BrowserEngine::CrabcodeBrowser);
        // Round-trips back to the pinned wire form (not `crabcodebrowser`).
        assert_eq!(
            serde_json::to_string(&BrowserDriver::CrabcodeBrowser).unwrap(),
            "\"crabcode-browser\""
        );
    }

    #[test]
    fn browser_profile_driver_accepts_legacy_crabcode_alias() {
        let driver: BrowserProfileDriver =
            serde_json::from_str("\"crabcode\"").expect("legacy driver");

        assert_eq!(driver, BrowserProfileDriver::CrabcodeBrowser);
    }

    // PR-5 §2.13.A.3 — migration regression: old `playwright`/`crabcode` config
    // literals must all keep deserializing (otherwise load_config() would fail
    // wholesale on configs carrying them) and resolve to the new variants.
    #[test]
    fn legacy_playwright_and_crabcode_config_values_still_deserialize() {
        let driver: BrowserDriver =
            serde_json::from_str("\"playwright\"").expect("legacy driver value");
        assert_eq!(driver, BrowserDriver::CrabcodeBrowser);

        let engine: BrowserEngine =
            serde_json::from_str("\"playwright\"").expect("legacy engine value");
        assert_eq!(engine, BrowserEngine::CrabcodeBrowser);

        let profile_pw: BrowserProfileDriver =
            serde_json::from_str("\"playwright\"").expect("legacy profile driver value");
        assert_eq!(profile_pw, BrowserProfileDriver::CrabcodeBrowser);

        let profile_cc: BrowserProfileDriver =
            serde_json::from_str("\"crabcode\"").expect("older profile driver alias");
        assert_eq!(profile_cc, BrowserProfileDriver::CrabcodeBrowser);
    }
}
