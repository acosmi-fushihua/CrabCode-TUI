use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::atomic_write::{atomic_write, BoxError};
use crate::daily_log::rust_derived_root;

/// K5 (W-MEMORY-LIFECYCLE 2026-07-09) — dreaming runs on a fixed ~2-day
/// cadence by default (was 24h). The interval is the primary throttle now
/// that the session threshold is 1 (any single new session provides corpus).
pub const DEFAULT_DREAM_MIN_HOURS: u64 = 48;
/// K5 — one touched session is enough material to dream on (was 5, which on
/// low-traffic projects deferred consolidation indefinitely; the 2-day time
/// gate above is the rate limiter).
pub const DEFAULT_DREAM_MIN_SESSIONS: usize = 1;
pub const DEFAULT_SESSION_SCAN_INTERVAL_MS: u64 = 10 * 60 * 1000;
/// K5 — imagination gets its own independent cadence (also ~2 days) instead
/// of only riding a successful dream. Consumed by the periodic tick's
/// imagination sweep (`.memory-rust-derived/last-imagination.json` marker).
pub const DEFAULT_IMAGINATION_MIN_HOURS: u64 = 48;

const CONFIG_FILE: &str = "dream-config.json";

/// W-MEMORY-SELF-EVOLUTION B1 (2026-06-11, 用户裁决④) — auto-promotion tier
/// for dream insights. Insight confidence is categorical (low/medium/high;
/// low never reaches `insight_*.md` — it becomes a fragment), so the
/// "threshold" is a tier, not a float: `High` promotes only high-confidence
/// insights into the MEMORY.md index (strong system-prompt injection
/// channel); `Medium` promotes medium+high; `Off` = fully manual.
/// Configurable via `dream-config.json` `auto_promote` ("off"|"high"|
/// "medium"); imagination drafts are NEVER auto-promoted regardless of this
/// setting (human confirm is a hard contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AutoPromoteTier {
    Off,
    #[default]
    High,
    Medium,
}

impl AutoPromoteTier {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::High => "high",
            Self::Medium => "medium",
        }
    }

    /// Whether an insight with the given categorical confidence qualifies.
    #[must_use]
    pub fn admits(self, confidence: &str) -> bool {
        match self {
            Self::Off => false,
            Self::High => confidence.eq_ignore_ascii_case("high"),
            Self::Medium => {
                confidence.eq_ignore_ascii_case("high") || confidence.eq_ignore_ascii_case("medium")
            }
        }
    }
}

/// W-MEMORY-SELF-EVOLVE-DGM G3-c (2026-07-16) — 写前去重配置（`dedup` 段）。
#[derive(Clone, Debug, PartialEq)]
pub struct DedupConfig {
    /// 梦/想象产物写前的词集 Jaccard 近重复门限（≥ 该值视为重复跳写）。
    /// 钳位 [0.5, 1.0]；1.0 = 等效关闭（只有全同词集才算重复，精确正文
    /// 去重仍由 `dedup_hash` 兜底）。
    pub jaccard_threshold: f64,
}

pub const DEFAULT_DEDUP_JACCARD_THRESHOLD: f64 = 0.85;

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            jaccard_threshold: DEFAULT_DEDUP_JACCARD_THRESHOLD,
        }
    }
}

/// W-MEMORY-SELF-EVOLVE-DGM 8b (2026-07-16) — 进化引擎配置（`evolution` 段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvolutionConfig {
    /// 参数自调优总开关（默认开 —— 关闭时 8a 适应度仍照常计算，只是
    /// 不再产生参数试验）。
    pub enabled: bool,
    /// 进化周期（小时，默认 168 = 7 天；每周期至多动一个参数一档）。
    pub cycle_hours: u64,
    /// 用户锁定的参数键（8b 永不触碰；键名 = 白名单键，如
    /// `search.half_life_days`）。用户手改过的值建议同时加锁。
    pub locked: Vec<String>,
}

pub const DEFAULT_EVOLUTION_CYCLE_HOURS: u64 = 168;

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cycle_hours: DEFAULT_EVOLUTION_CYCLE_HOURS,
            locked: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DreamConfig {
    pub enabled: bool,
    pub min_hours: u64,
    pub min_sessions: usize,
    pub session_scan_interval_ms: u64,
    /// B1 (2026-06-11): dream-insight auto-promotion tier. Default `High`.
    pub auto_promote: AutoPromoteTier,
    /// K5 (W-MEMORY-LIFECYCLE 2026-07-09): minimum interval (hours) between
    /// independent imagination sweeps (the periodic tick's
    /// `last-imagination.json` cycle — imagination no longer only rides a
    /// successful dream). Default `DEFAULT_IMAGINATION_MIN_HOURS` (48);
    /// pre-K5 `dream-config.json` files without the key deserialize to that
    /// default (backward compatible).
    pub imagination_min_hours: u64,
    /// 8b (2026-07-16): importance 积分提前做梦的压力阈值（原
    /// `importance_pressure::IMPORTANCE_PRESSURE_THRESHOLD` 常量配置化，
    /// 常量保留为默认值真源）。可进化参数。
    pub importance_pressure_threshold: u64,
    /// G1 (2026-07-16): 检索评分策略（`search` 段）——同时是 8b 参数自调优
    /// 的白名单参数空间。缺省段落 = 全默认（衰减开/MMR 关/门限关）。
    pub search_policy: crate::search_policy::SearchPolicyConfig,
    /// G3-c (2026-07-16): 写前去重（`dedup` 段）。
    pub dedup: DedupConfig,
    /// 8b (2026-07-16): 进化引擎（`evolution` 段）。
    pub evolution: EvolutionConfig,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            // W-MEMORY-DATA-COMPLETION A4 / D2 (2026-06-20, 用户裁决「实装+开启」):
            // dreaming is ON by default. This is the no-stored-config default the
            // periodic sweep `run_dream_tick` reads (`read_dream_config →
            // unwrap_or_default`), so background dream consolidation runs for
            // users who never touched the toggle. Hard prerequisite Phase 0
            // (real corpus) lands in the same change set, so a default-on dream
            // produces real insights instead of burning quota for nothing. The
            // four gates (48h / ≥1 session / 30s idle / leader lock — K5 fixed
            // 2-day cadence) keep it to an occasional idle sweep, and the TUI
            // toggle (memory.dream.set_enabled, MemoryFileSelector) persists an
            // explicit opt-out that wins over this default.
            enabled: true,
            min_hours: DEFAULT_DREAM_MIN_HOURS,
            min_sessions: DEFAULT_DREAM_MIN_SESSIONS,
            session_scan_interval_ms: DEFAULT_SESSION_SCAN_INTERVAL_MS,
            auto_promote: AutoPromoteTier::default(),
            imagination_min_hours: DEFAULT_IMAGINATION_MIN_HOURS,
            importance_pressure_threshold:
                crate::importance_pressure::IMPORTANCE_PRESSURE_THRESHOLD,
            search_policy: crate::search_policy::SearchPolicyConfig::default(),
            dedup: DedupConfig::default(),
            evolution: EvolutionConfig::default(),
        }
    }
}

impl DreamConfig {
    #[must_use]
    pub fn from_feature_flags(
        stored: Option<Self>,
        flags: &std::collections::BTreeMap<String, bool>,
    ) -> Self {
        if let Some(config) = stored {
            return config;
        }

        let mut config = Self::default();
        if let Some(enabled) = flags
            .get("auto_dream_enabled")
            .or_else(|| flags.get("AUTO_DREAM"))
        {
            config.enabled = *enabled;
        }
        config
    }
}

pub fn dream_config_path(project_state_dir: &Path) -> PathBuf {
    rust_derived_root(project_state_dir).join(CONFIG_FILE)
}

pub fn read_dream_config_optional(
    project_state_dir: &Path,
) -> Result<Option<DreamConfig>, BoxError> {
    let path = dream_config_path(project_state_dir);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(parse_dream_config(&raw)?))
}

pub fn read_dream_config(project_state_dir: &Path) -> Result<DreamConfig, BoxError> {
    Ok(read_dream_config_optional(project_state_dir)?.unwrap_or_default())
}

fn parse_dream_config(raw: &str) -> Result<DreamConfig, BoxError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let mut config = DreamConfig::default();
    if let Some(enabled) = value.get("enabled").and_then(serde_json::Value::as_bool) {
        config.enabled = enabled;
    }
    if let Some(min_hours) = value.get("min_hours").and_then(serde_json::Value::as_u64) {
        if min_hours > 0 {
            config.min_hours = min_hours;
        }
    }
    if let Some(min_sessions) = value
        .get("min_sessions")
        .and_then(serde_json::Value::as_u64)
        .and_then(|raw| usize::try_from(raw).ok())
    {
        if min_sessions > 0 {
            config.min_sessions = min_sessions;
        }
    }
    if let Some(interval) = value
        .get("session_scan_interval_ms")
        .and_then(serde_json::Value::as_u64)
    {
        if interval > 0 {
            config.session_scan_interval_ms = interval;
        }
    }
    if let Some(tier) = value
        .get("auto_promote")
        .and_then(serde_json::Value::as_str)
        .and_then(AutoPromoteTier::parse)
    {
        config.auto_promote = tier;
    }
    // K5: absent key (pre-K5 files) keeps the 48h default — backward
    // compatible; 0 is rejected like the other interval fields.
    if let Some(imagination_min_hours) = value
        .get("imagination_min_hours")
        .and_then(serde_json::Value::as_u64)
    {
        if imagination_min_hours > 0 {
            config.imagination_min_hours = imagination_min_hours;
        }
    }
    // 8b: importance 压力阈值（0 拒绝回默认，与其它数值字段一致）。
    if let Some(threshold) = value
        .get("importance_pressure_threshold")
        .and_then(serde_json::Value::as_u64)
    {
        if threshold > 0 {
            config.importance_pressure_threshold = threshold;
        }
    }
    // G1 (2026-07-16): `search` 段（缺省/畸形 → 全默认，字段级钳位在
    // SearchPolicyConfig::parse 内）。
    config.search_policy = crate::search_policy::SearchPolicyConfig::parse(value.get("search"));
    // G3-c: `dedup` 段。
    if let Some(threshold) = value
        .get("dedup")
        .and_then(|d| d.get("jaccard_threshold"))
        .and_then(serde_json::Value::as_f64)
    {
        if threshold.is_finite() {
            config.dedup.jaccard_threshold = threshold.clamp(0.5, 1.0);
        }
    }
    // 8b: `evolution` 段。
    if let Some(evolution) = value.get("evolution") {
        if let Some(enabled) = evolution
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
        {
            config.evolution.enabled = enabled;
        }
        if let Some(cycle_hours) = evolution
            .get("cycle_hours")
            .and_then(serde_json::Value::as_u64)
        {
            if cycle_hours > 0 {
                config.evolution.cycle_hours = cycle_hours;
            }
        }
        if let Some(locked) = evolution
            .get("locked")
            .and_then(serde_json::Value::as_array)
        {
            config.evolution.locked = locked
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    Ok(config)
}

pub async fn write_dream_config(
    project_state_dir: &Path,
    config: &DreamConfig,
) -> Result<(), BoxError> {
    let path = dream_config_path(project_state_dir);
    let bytes = serde_json::to_vec_pretty(&json!({
        "enabled": config.enabled,
        "min_hours": config.min_hours,
        "min_sessions": config.min_sessions,
        "session_scan_interval_ms": config.session_scan_interval_ms,
        "auto_promote": config.auto_promote.as_str(),
        "imagination_min_hours": config.imagination_min_hours,
        "importance_pressure_threshold": config.importance_pressure_threshold,
        "search": config.search_policy.to_value(),
        "dedup": { "jaccard_threshold": config.dedup.jaccard_threshold },
        "evolution": {
            "enabled": config.evolution.enabled,
            "cycle_hours": config.evolution.cycle_hours,
            "locked": config.evolution.locked,
        },
    }))?;
    atomic_write(&path, &bytes).await
}

pub async fn set_dream_enabled(
    project_state_dir: &Path,
    enabled: bool,
) -> Result<DreamConfig, BoxError> {
    let mut config = read_dream_config(project_state_dir)?;
    config.enabled = enabled;
    write_dream_config(project_state_dir, &config).await?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn dream_config_default_is_enabled_with_fixed_two_day_cadence() {
        // W-MEMORY-DATA-COMPLETION A4 / D2: dreaming defaults ON (periodic sweep
        // reads this no-stored-config default). K5 (W-MEMORY-LIFECYCLE): the
        // cadence is a fixed ~2 days (48h) with a 1-session material floor —
        // time is the throttle, not session volume.
        let config = DreamConfig::default();
        assert!(config.enabled, "A4/D2: dreaming defaults ON");
        assert_eq!(config.min_hours, 48, "K5: 2-day dream cadence");
        assert_eq!(config.min_sessions, 1, "K5: one new session suffices");
        assert_eq!(config.session_scan_interval_ms, 10 * 60 * 1000);
        assert_eq!(
            config.imagination_min_hours, 48,
            "K5: independent imagination cycle defaults to 2 days"
        );
    }

    #[test]
    fn dream_config_pre_k5_json_without_imagination_key_gets_default() {
        // A pre-K5 dream-config.json has no `imagination_min_hours` key —
        // parsing must stay compatible and fall back to the 48h default.
        let raw = r#"{
            "enabled": true,
            "min_hours": 24,
            "min_sessions": 5,
            "session_scan_interval_ms": 600000,
            "auto_promote": "high"
        }"#;
        let config = parse_dream_config(raw).unwrap();
        assert!(config.enabled);
        assert_eq!(config.min_hours, 24, "stored value wins over new default");
        assert_eq!(config.min_sessions, 5);
        assert_eq!(config.imagination_min_hours, DEFAULT_IMAGINATION_MIN_HOURS);

        // Zero is rejected like the other interval fields.
        let zeroed = parse_dream_config(r#"{"imagination_min_hours": 0}"#).unwrap();
        assert_eq!(zeroed.imagination_min_hours, DEFAULT_IMAGINATION_MIN_HOURS);
    }

    #[tokio::test]
    async fn dream_config_imagination_min_hours_round_trips() {
        let dir = TempDir::new().unwrap();
        let config = DreamConfig {
            imagination_min_hours: 72,
            ..DreamConfig::default()
        };
        write_dream_config(dir.path(), &config).await.unwrap();

        let read_back = read_dream_config(dir.path()).unwrap();
        assert_eq!(read_back, config);
        assert_eq!(read_back.imagination_min_hours, 72);
    }

    #[tokio::test]
    async fn dream_config_round_trips_enabled_to_sibling_derived_root() {
        let dir = TempDir::new().unwrap();
        let config = set_dream_enabled(dir.path(), true).await.unwrap();

        assert!(config.enabled);
        assert!(dream_config_path(dir.path()).starts_with(dir.path().join(".memory-rust-derived")));
        assert!(!dir
            .path()
            .join("memory/.rust-derived/dream-config.json")
            .exists());
        assert!(read_dream_config(dir.path()).unwrap().enabled);
    }

    #[tokio::test]
    async fn dream_config_dgm_sections_round_trip_and_clamp() {
        // W-MEMORY-SELF-EVOLVE-DGM (2026-07-16): search/dedup/evolution 三段
        // round-trip 保真（8b 读→改→写不得丢字段），越界值解析期钳位。
        let dir = TempDir::new().unwrap();
        let mut config = DreamConfig::default();
        config.search_policy.half_life_days = 14.0;
        config.search_policy.mmr_enabled = true;
        config
            .search_policy
            .source_weights
            .insert("imagined".to_string(), 0.5);
        config.dedup.jaccard_threshold = 0.9;
        config.evolution.cycle_hours = 72;
        config.evolution.locked = vec!["search.min_score".to_string()];
        write_dream_config(dir.path(), &config).await.unwrap();
        assert_eq!(read_dream_config(dir.path()).unwrap(), config);

        let parsed = parse_dream_config(
            r#"{"dedup":{"jaccard_threshold":0.1},"evolution":{"cycle_hours":0,"locked":["a",""]}}"#,
        )
        .unwrap();
        assert_eq!(parsed.dedup.jaccard_threshold, 0.5, "地板钳位");
        assert_eq!(
            parsed.evolution.cycle_hours, DEFAULT_EVOLUTION_CYCLE_HOURS,
            "0 拒绝回默认"
        );
        assert!(parsed.evolution.enabled, "缺省 enabled = true");
        assert_eq!(parsed.evolution.locked, vec!["a".to_string()], "空串滤除");
        assert_eq!(
            parsed.search_policy,
            crate::search_policy::SearchPolicyConfig::default(),
            "缺省 search 段 = 全默认"
        );

        // 旧格式（无三段）向后兼容 = 全默认。
        let legacy = parse_dream_config(r#"{"enabled":true,"min_hours":24}"#).unwrap();
        assert_eq!(legacy.search_policy, Default::default());
        assert_eq!(legacy.dedup, Default::default());
        assert_eq!(legacy.evolution, Default::default());
    }

    #[test]
    fn dream_config_feature_flags_seed_missing_config() {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert("auto_dream_enabled".to_owned(), true);

        let config = DreamConfig::from_feature_flags(None, &flags);

        assert!(config.enabled);
    }

    #[test]
    fn dream_config_stored_enabled_wins_over_feature_flags() {
        let mut flags = std::collections::BTreeMap::new();
        flags.insert("auto_dream_enabled".to_owned(), true);

        // Stored config (explicitly disabled) must win over a stale TS flag that
        // says enabled. Construct an explicit disabled config — the *default* is
        // now enabled (A4), so `DreamConfig::default()` would no longer express
        // the "stored disabled" case this asserts.
        let config = DreamConfig::from_feature_flags(
            Some(DreamConfig {
                enabled: false,
                ..DreamConfig::default()
            }),
            &flags,
        );

        assert!(!config.enabled);

        flags.insert("auto_dream_enabled".to_owned(), false);
        let config = DreamConfig::from_feature_flags(
            Some(DreamConfig {
                enabled: true,
                ..DreamConfig::default()
            }),
            &flags,
        );

        assert!(config.enabled);
    }
}
