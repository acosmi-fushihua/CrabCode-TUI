//! W-MEMORY-SELF-EVOLVE-DGM G1 (2026-07-16) — 检索评分策略层。
//!
//! SE（BM25F 词法 / RRF hybrid）产出的是 **scope 内相对分**：跨 scope 不可
//! 比（BM25F 无界、RRF ~0.016–0.03），跨 scope 合并契约是 rank-interleave
//! （见 `ipc_handler::interleave_scope_hits`）。因此策略层在 **scope 内**做
//! 归一化 + 乘法链，interleave 之后只做可选的 MMR 多样性重排：
//!
//! ```text
//! norm    = min-max 归一到 [0,1]（单结果 = 1.0）
//! raw     = norm × temporal_decay × source_weight × access_boost
//! display = clamp(raw, 0, 1)   —— 排序用 raw（未钳），门限/展示用 display
//! ```
//!
//! 设计来源：grok-build `xai-grok-memory/src/search.rs`（temporal decay 半衰
//! 期 + evergreen 豁免 / access_boost = 1+ln(1+n)×0.05 / raw·display 双分数 /
//! MMR）按本仓类型分类净室重实现（Apache-2.0 思想吸收，零代码搬移，用户裁决
//! D4）。差异化适配：
//! - 衰减只作用于**瞬态类** memory_type（`imagined`/`feedback`/`session`/
//!   `speednote`），精选长期类（user/project/knowledge/insight/report）恒不
//!   衰减 —— 我们的索引项本身就是提炼产物，半衰期取 30 天（grok 的 7 天面向
//!   原始 session 日志，不适用）。
//! - `min_score` 默认 0（关闭）：scope 内归一分是相对量，硬门限易把小结果集
//!   清空；进化层（8b）可按适应度开启。
//! - MMR 默认关闭（上游已有去重管线，冗余度低）；作为 8b 可进化开关存在。
//!
//! 本模块是 8b 参数自调优的**参数空间**：所有旋钮经 `dream-config.json`
//! `search` 段落盘（解析期钳位），字段名即 8b 白名单键名。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::output_language::MemoryOutputLanguage;
use crate::se_integration::MemorySearchHit;

/// 衰减半衰期默认 30 天（钳位范围 [1, 365]）。
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;
/// access_boost 比例因子默认 0.05（钳位 [0, 0.5]；0 = 关闭使用强化）。
pub const DEFAULT_ACCESS_BOOST_SCALE: f64 = 0.05;
/// MMR λ 默认 0.7（1.0 = 纯相关性；钳位 [0,1]）。
pub const DEFAULT_MMR_LAMBDA: f64 = 0.7;
/// 陈旧度标注的最小年龄（天）：只有被衰减的瞬态类结果超过该年龄才在
/// snippet 上追加提醒。
pub const STALENESS_NOTE_MIN_AGE_DAYS: f64 = 30.0;
/// 默认受时间衰减的 memory_type 集合（瞬态/观点类）。
pub const DEFAULT_DECAY_TYPES: [&str; 4] = ["imagined", "feedback", "session", "speednote"];

/// `dream-config.json` `search` 段（8b 参数白名单主体）。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPolicyConfig {
    /// 时间衰减总开关（默认开；只作用于 `decay_types`）。
    pub decay_enabled: bool,
    /// 衰减半衰期（天）。
    pub half_life_days: f64,
    /// 受衰减的 memory_type 集合。
    pub decay_types: Vec<String>,
    /// 按 memory_type 的权重乘子（缺省 1.0；钳位 [0.1, 10]）。
    pub source_weights: HashMap<String, f64>,
    /// access_boost 比例因子。
    pub access_boost_scale: f64,
    /// display 分门限（默认 0 = 关闭）。
    pub min_score: f64,
    /// MMR 多样性重排开关（默认关）。
    pub mmr_enabled: bool,
    /// MMR λ。
    pub mmr_lambda: f64,
    /// W-MEMORY-KB-UPLIFT P1 — cross-encoder rerank 总开关（默认开；实际
    /// 生效还需 caller 显式 `allow_rerank`（工具/TUI 显式搜索）+ 网关存在
    /// `supports_rerank` 模型 + 无 backoff）。8b 进化白名单键
    /// `search.rerank_enabled`。
    pub rerank_enabled: bool,
}

impl Default for SearchPolicyConfig {
    fn default() -> Self {
        Self {
            decay_enabled: true,
            half_life_days: DEFAULT_HALF_LIFE_DAYS,
            decay_types: DEFAULT_DECAY_TYPES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            source_weights: HashMap::new(),
            access_boost_scale: DEFAULT_ACCESS_BOOST_SCALE,
            min_score: 0.0,
            mmr_enabled: false,
            mmr_lambda: DEFAULT_MMR_LAMBDA,
            rerank_enabled: true,
        }
    }
}

fn clamp_unit(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

impl SearchPolicyConfig {
    /// 从 dream-config.json 的 `search` 值解析（缺省/畸形字段落默认值，
    /// 数值字段解析期钳位 —— 学 grok `deserialize_clamped_unit` 的意图：
    /// 越界值静默收敛而不是让整个配置失效）。
    #[must_use]
    pub fn parse(raw: Option<&Value>) -> Self {
        let mut config = Self::default();
        let Some(value) = raw else { return config };
        if let Some(enabled) = value.get("decay_enabled").and_then(Value::as_bool) {
            config.decay_enabled = enabled;
        }
        if let Some(days) = value.get("half_life_days").and_then(Value::as_f64) {
            if days.is_finite() {
                config.half_life_days = days.clamp(1.0, 365.0);
            }
        }
        if let Some(types) = value.get("decay_types").and_then(Value::as_array) {
            let parsed: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            // 显式空数组 = 关闭全部衰减（合法配置）；非数组/缺省保持默认。
            config.decay_types = parsed;
        }
        if let Some(weights) = value.get("source_weights").and_then(Value::as_object) {
            for (key, weight) in weights {
                if let Some(w) = weight.as_f64() {
                    if w.is_finite() {
                        config
                            .source_weights
                            .insert(key.clone(), w.clamp(0.1, 10.0));
                    }
                }
            }
        }
        if let Some(scale) = value.get("access_boost_scale").and_then(Value::as_f64) {
            if scale.is_finite() {
                config.access_boost_scale = scale.clamp(0.0, 0.5);
            }
        }
        if let Some(score) = value.get("min_score").and_then(Value::as_f64) {
            if score.is_finite() {
                config.min_score = clamp_unit(score);
            }
        }
        if let Some(enabled) = value.get("mmr_enabled").and_then(Value::as_bool) {
            config.mmr_enabled = enabled;
        }
        if let Some(lambda) = value.get("mmr_lambda").and_then(Value::as_f64) {
            if lambda.is_finite() {
                config.mmr_lambda = clamp_unit(lambda);
            }
        }
        if let Some(enabled) = value.get("rerank_enabled").and_then(Value::as_bool) {
            config.rerank_enabled = enabled;
        }
        config
    }

    /// 序列化回 dream-config.json 的 `search` 值（round-trip 保真，8b 写
    /// 回时不丢字段）。
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "decay_enabled": self.decay_enabled,
            "half_life_days": self.half_life_days,
            "decay_types": self.decay_types,
            "source_weights": self.source_weights,
            "access_boost_scale": self.access_boost_scale,
            "min_score": self.min_score,
            "mmr_enabled": self.mmr_enabled,
            "mmr_lambda": self.mmr_lambda,
            "rerank_enabled": self.rerank_enabled,
        })
    }
}

/// 指数衰减乘子。`mtime_ms == 0`（无法证明年龄）或非瞬态类 → 1.0。
fn temporal_decay_multiplier(
    memory_type: Option<&str>,
    mtime_ms: u64,
    now_ms: u64,
    config: &SearchPolicyConfig,
) -> f64 {
    if !config.decay_enabled || mtime_ms == 0 || config.half_life_days <= 0.0 {
        return 1.0;
    }
    let Some(ty) = memory_type else { return 1.0 };
    if !config.decay_types.iter().any(|t| t == ty) {
        return 1.0;
    }
    let age_days = (now_ms.saturating_sub(mtime_ms)) as f64 / 86_400_000.0;
    let lambda = std::f64::consts::LN_2 / config.half_life_days;
    (-lambda * age_days).exp()
}

/// 单结果的年龄（天）；`mtime_ms == 0` → None。
fn age_days(mtime_ms: u64, now_ms: u64) -> Option<f64> {
    if mtime_ms == 0 {
        return None;
    }
    Some((now_ms.saturating_sub(mtime_ms)) as f64 / 86_400_000.0)
}

fn staleness_note(days: f64, language: MemoryOutputLanguage) -> String {
    let rounded = days.round() as i64;
    match language {
        MemoryOutputLanguage::Zh => {
            format!("（⏳ 约 {rounded} 天前的记录，采信前请核对现状）")
        }
        MemoryOutputLanguage::En => {
            format!(" (⏳ ~{rounded} days old — verify before relying on it)")
        }
    }
}

/// scope 内策略变换：归一化 → 衰减 × 来源权重 × access_boost → 按 raw 重排
/// → min_score 门限（display 分）→ 瞬态陈旧结果的 snippet 标注。
///
/// 纯函数（除 `hits` 原位改写外无副作用）；`access_counts` 键 = SE point id
/// （`hit.id`）。
pub fn apply_scope_policy(
    hits: &mut Vec<MemorySearchHit>,
    config: &SearchPolicyConfig,
    access_counts: &HashMap<String, u64>,
    now_ms: u64,
    language: MemoryOutputLanguage,
) {
    if hits.is_empty() {
        return;
    }
    // scope 内 **max 归一化**（norm = score / max）：BM25F / RRF 分数恒正，
    // 比例保持——近分结果归一后仍近分，权重/衰减能有意义地翻转次序；
    // min-max 会把两条近分结果拉成 0/1 两极（底部条目权重永远救不回），
    // 那是 grok 为 FTS5 负 rank 设计的形态，不适用于我们。max ≤ ε（全零
    // 分，理论上只在浏览模式出现且浏览不走策略）→ 整组 1.0。
    let max = hits
        .iter()
        .map(|h| f64::from(h.score))
        .fold(f64::NEG_INFINITY, f64::max);
    let degenerate = max <= f64::EPSILON;

    let mut ranked: Vec<(f64, MemorySearchHit)> = Vec::with_capacity(hits.len());
    for mut hit in hits.drain(..) {
        let norm = if degenerate {
            1.0
        } else {
            (f64::from(hit.score) / max).clamp(0.0, 1.0)
        };
        let decay =
            temporal_decay_multiplier(hit.memory_type.as_deref(), hit.mtime_ms, now_ms, config);
        let weight = hit
            .memory_type
            .as_deref()
            .and_then(|ty| config.source_weights.get(ty))
            .copied()
            .unwrap_or(1.0);
        let boost = 1.0
            + (access_counts.get(&hit.id).copied().unwrap_or(0) as f64).ln_1p()
                * config.access_boost_scale;
        let raw = norm * decay * weight * boost;
        // display 分：门限与展示用钳位分；排序用未钳 raw（access_boost 可
        // 让顶部条目 >1，raw 保留其间的次序信息 —— grok 双分数语义）。
        hit.score = clamp_unit(raw) as f32;

        // 陈旧度标注：仅「实际被衰减」且足够老的瞬态结果。
        if decay < 1.0 {
            if let Some(days) = age_days(hit.mtime_ms, now_ms) {
                if days >= STALENESS_NOTE_MIN_AGE_DAYS {
                    let note = staleness_note(days, language);
                    match hit.snippet.take() {
                        Some(snippet) => hit.snippet = Some(format!("{snippet}{note}")),
                        None => hit.snippet = Some(note.trim().to_string()),
                    }
                }
            }
        }
        ranked.push((raw, hit));
    }

    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    hits.extend(ranked.into_iter().filter_map(|(_, hit)| {
        if config.min_score > 0.0 && f64::from(hit.score) < config.min_score {
            None
        } else {
            Some(hit)
        }
    }));
}

/// wire 值（interleave 之后）的 MMR 多样性重排。
///
/// `MMR(d) = λ × relevance(d) − (1−λ) × max_sim(d, selected)`；相似度 =
/// name+snippet 词集 Jaccard（`se_integration::tokenize`，charabia 分词 —
/// CJK 正确切分）。relevance 取 `score` 字段（display 分，scope 内归一后
/// 近似可比）。返回选择序，截断 `top_k`。
#[must_use]
pub fn mmr_rerank_values(items: Vec<Value>, lambda: f64, top_k: usize) -> Vec<Value> {
    if top_k == 0 {
        return Vec::new();
    }
    if items.len() <= 1 {
        let mut items = items;
        items.truncate(top_k);
        return items;
    }
    let lambda = clamp_unit(lambda);
    let token_sets: Vec<HashSet<String>> = items
        .iter()
        .map(|item| {
            let mut text = String::new();
            for key in ["name", "snippet"] {
                if let Some(raw) = item.get(key).and_then(Value::as_str) {
                    text.push_str(raw);
                    text.push(' ');
                }
            }
            crate::se_integration::tokenize(&text).into_iter().collect()
        })
        .collect();
    let relevance: Vec<f64> = items
        .iter()
        .map(|item| item.get("score").and_then(Value::as_f64).unwrap_or(0.0))
        .collect();

    let jaccard = |a: &HashSet<String>, b: &HashSet<String>| -> f64 {
        if a.is_empty() || b.is_empty() {
            return 0.0;
        }
        let intersection = a.intersection(b).count() as f64;
        let union = (a.len() + b.len()) as f64 - intersection;
        if union <= 0.0 {
            0.0
        } else {
            intersection / union
        }
    };

    let mut remaining: Vec<usize> = (0..items.len()).collect();
    let mut selected: Vec<usize> = Vec::with_capacity(top_k.min(items.len()));
    while !remaining.is_empty() && selected.len() < top_k {
        let mut best_pos = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for (pos, &idx) in remaining.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|&s| jaccard(&token_sets[idx], &token_sets[s]))
                .fold(0.0_f64, f64::max);
            let mmr = lambda * relevance[idx] - (1.0 - lambda) * max_sim;
            if mmr > best_score {
                best_score = mmr;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
    }

    let mut picked: Vec<Option<Value>> = items.into_iter().map(Some).collect();
    selected
        .into_iter()
        .filter_map(|idx| picked[idx].take())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 86_400_000;

    fn hit(id: &str, score: f32, ty: &str, mtime_ms: u64) -> MemorySearchHit {
        MemorySearchHit {
            id: id.to_string(),
            score,
            source_path: None,
            scope: Some("private".to_string()),
            name: Some(id.to_string()),
            memory_type: Some(ty.to_string()),
            snippet: Some(format!("snippet of {id}")),
            mtime_ms,
        }
    }

    #[test]
    fn parse_defaults_and_clamps() {
        let config = SearchPolicyConfig::parse(None);
        assert_eq!(config, SearchPolicyConfig::default());
        assert!(config.decay_enabled);
        assert_eq!(config.half_life_days, DEFAULT_HALF_LIFE_DAYS);
        assert_eq!(config.min_score, 0.0);
        assert!(!config.mmr_enabled);

        let raw = serde_json::json!({
            "half_life_days": 9999.0,
            "access_boost_scale": 3.0,
            "min_score": -2.0,
            "mmr_lambda": 5.0,
            "source_weights": { "insight": 100.0, "imagined": 0.0001 },
        });
        let config = SearchPolicyConfig::parse(Some(&raw));
        assert_eq!(config.half_life_days, 365.0, "half-life clamped to ceiling");
        assert_eq!(config.access_boost_scale, 0.5, "boost scale clamped");
        assert_eq!(config.min_score, 0.0, "negative min_score clamps to 0");
        assert_eq!(config.mmr_lambda, 1.0, "lambda clamped to unit");
        assert_eq!(config.source_weights["insight"], 10.0);
        assert_eq!(config.source_weights["imagined"], 0.1);
    }

    #[test]
    fn parse_to_value_round_trips() {
        let mut config = SearchPolicyConfig {
            half_life_days: 14.0,
            mmr_enabled: true,
            ..Default::default()
        };
        config.source_weights.insert("imagined".to_string(), 0.5);
        let round = SearchPolicyConfig::parse(Some(&config.to_value()));
        assert_eq!(round, config);
    }

    #[test]
    fn neutral_config_preserves_order_and_normalizes() {
        // 衰减关闭 + 无权重 + 无计数 = 纯归一化，次序不变。
        let config = SearchPolicyConfig {
            decay_enabled: false,
            ..Default::default()
        };
        let mut hits = vec![
            hit("a", 10.0, "project", 0),
            hit("b", 5.0, "project", 0),
            hit("c", 1.0, "project", 0),
        ];
        apply_scope_policy(
            &mut hits,
            &config,
            &HashMap::new(),
            DAY_MS * 100,
            MemoryOutputLanguage::Zh,
        );
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(hits[0].score, 1.0);
        // max 归一化保持比例：1/10 = 0.1（min-max 会压成 0，比例信息丢失）。
        assert!((f64::from(hits[2].score) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn single_hit_normalizes_to_full_score() {
        let mut hits = vec![hit("only", 0.0163, "project", 0)];
        apply_scope_policy(
            &mut hits,
            &SearchPolicyConfig::default(),
            &HashMap::new(),
            DAY_MS,
            MemoryOutputLanguage::Zh,
        );
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn decay_demotes_old_transient_but_not_evergreen() {
        let now = DAY_MS * 400;
        // 同分：老的 imagined（瞬态，90 天前）vs 老的 insight（精选，90 天前）。
        let mut hits = vec![
            hit("old-imagined", 10.0, "imagined", now - 90 * DAY_MS),
            hit("old-insight", 10.0, "insight", now - 90 * DAY_MS),
        ];
        apply_scope_policy(
            &mut hits,
            &SearchPolicyConfig::default(),
            &HashMap::new(),
            now,
            MemoryOutputLanguage::Zh,
        );
        assert_eq!(hits[0].id, "old-insight", "evergreen 不衰减，排前");
        assert!(hits[1].score < hits[0].score);
        // 半衰期 30 天、年龄 90 天 → 衰减 ≈ 1/8。
        assert!((f64::from(hits[1].score) - 0.125).abs() < 0.01);
    }

    #[test]
    fn decay_skips_hits_without_mtime() {
        let now = DAY_MS * 400;
        let mut hits = vec![
            hit("no-mtime", 10.0, "imagined", 0),
            hit("fresh", 10.0, "imagined", now),
        ];
        apply_scope_policy(
            &mut hits,
            &SearchPolicyConfig::default(),
            &HashMap::new(),
            now,
            MemoryOutputLanguage::Zh,
        );
        // 无 mtime 不衰减：两条均满分，tie-break 按 id。
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(hits[1].score, 1.0);
    }

    #[test]
    fn access_boost_is_monotonic_and_modest() {
        let now = DAY_MS;
        let counts: HashMap<String, u64> =
            [("hot".to_string(), 100), ("warm".to_string(), 1)].into();
        // 三条同分同类，无衰减差异 → boost 决定次序。
        let mut hits = vec![
            hit("cold", 10.0, "project", now),
            hit("hot", 10.0, "project", now),
            hit("warm", 10.0, "project", now),
        ];
        apply_scope_policy(
            &mut hits,
            &SearchPolicyConfig::default(),
            &counts,
            now,
            MemoryOutputLanguage::Zh,
        );
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["hot", "warm", "cold"]);
        // boost 有界温和：100 次 ≈ ×1.23（display 钳到 1.0）。
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn source_weight_reorders_within_scope() {
        let mut config = SearchPolicyConfig {
            decay_enabled: false,
            ..Default::default()
        };
        config.source_weights.insert("imagined".to_string(), 0.3);
        let mut hits = vec![
            hit("draft", 10.0, "imagined", 0),
            hit("fact", 9.0, "project", 0),
        ];
        apply_scope_policy(
            &mut hits,
            &config,
            &HashMap::new(),
            DAY_MS,
            MemoryOutputLanguage::Zh,
        );
        assert_eq!(hits[0].id, "fact", "降权后的 imagined 让位");
    }

    #[test]
    fn min_score_gate_filters_on_display_score() {
        let config = SearchPolicyConfig {
            decay_enabled: false,
            min_score: 0.5,
            ..Default::default()
        };
        let mut hits = vec![
            hit("keep", 10.0, "project", 0),
            hit("mid", 6.0, "project", 0),
            hit("drop", 1.0, "project", 0),
        ];
        apply_scope_policy(
            &mut hits,
            &config,
            &HashMap::new(),
            DAY_MS,
            MemoryOutputLanguage::Zh,
        );
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["keep", "mid"], "norm=1.0 / 0.6 留下, norm=0.1 淘汰");
    }

    #[test]
    fn staleness_note_appended_only_to_decayed_old_hits_zh_and_en() {
        let now = DAY_MS * 400;
        let mut hits = vec![
            hit("old-imagined", 10.0, "imagined", now - 60 * DAY_MS),
            hit("old-insight", 10.0, "insight", now - 60 * DAY_MS),
            hit("fresh-imagined", 10.0, "imagined", now - DAY_MS),
        ];
        apply_scope_policy(
            &mut hits,
            &SearchPolicyConfig::default(),
            &HashMap::new(),
            now,
            MemoryOutputLanguage::Zh,
        );
        let old_imagined = hits.iter().find(|h| h.id == "old-imagined").unwrap();
        assert!(
            old_imagined
                .snippet
                .as_ref()
                .unwrap()
                .contains("约 60 天前"),
            "衰减且 ≥30 天 → 中文陈旧标注: {:?}",
            old_imagined.snippet
        );
        let old_insight = hits.iter().find(|h| h.id == "old-insight").unwrap();
        assert!(
            !old_insight.snippet.as_ref().unwrap().contains("⏳"),
            "evergreen 永不标注"
        );
        let fresh = hits.iter().find(|h| h.id == "fresh-imagined").unwrap();
        assert!(
            !fresh.snippet.as_ref().unwrap().contains("⏳"),
            "新鲜瞬态结果不标注"
        );

        let mut en_hits = vec![hit("old-imagined", 10.0, "imagined", now - 60 * DAY_MS)];
        apply_scope_policy(
            &mut en_hits,
            &SearchPolicyConfig::default(),
            &HashMap::new(),
            now,
            MemoryOutputLanguage::En,
        );
        assert!(en_hits[0]
            .snippet
            .as_ref()
            .unwrap()
            .contains("~60 days old"));
    }

    fn value_item(id: &str, score: f64, text: &str) -> Value {
        serde_json::json!({
            "id": id,
            "score": score,
            "name": id,
            "snippet": text,
        })
    }

    #[test]
    fn mmr_promotes_diversity_over_redundancy() {
        // 两条近重复高分 + 一条不同主题中分：λ=0.5 应在第二位选中不同主题。
        let items = vec![
            value_item("dup-a", 1.0, "rust async runtime tokio scheduler details"),
            value_item("dup-b", 0.95, "rust async runtime tokio scheduler details"),
            value_item("other", 0.6, "postgres index vacuum autovacuum tuning"),
        ];
        let reranked = mmr_rerank_values(items, 0.5, 3);
        let ids: Vec<&str> = reranked
            .iter()
            .map(|v| v.get("id").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids[0], "dup-a", "首选最高相关");
        assert_eq!(ids[1], "other", "第二位选多样性");
        assert_eq!(ids[2], "dup-b");
    }

    #[test]
    fn mmr_lambda_one_is_pure_relevance() {
        let items = vec![
            value_item("a", 1.0, "same text"),
            value_item("b", 0.9, "same text"),
            value_item("c", 0.8, "same text"),
        ];
        let reranked = mmr_rerank_values(items, 1.0, 3);
        let ids: Vec<&str> = reranked
            .iter()
            .map(|v| v.get("id").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn mmr_truncates_and_handles_edges() {
        assert!(mmr_rerank_values(Vec::new(), 0.7, 5).is_empty());
        assert!(mmr_rerank_values(vec![value_item("a", 1.0, "x")], 0.7, 0).is_empty());
        let one = mmr_rerank_values(vec![value_item("a", 1.0, "x")], 0.7, 5);
        assert_eq!(one.len(), 1);
        let truncated = mmr_rerank_values(
            vec![
                value_item("a", 1.0, "alpha"),
                value_item("b", 0.9, "beta"),
                value_item("c", 0.8, "gamma"),
            ],
            0.7,
            2,
        );
        assert_eq!(truncated.len(), 2);
    }

    #[test]
    fn mmr_handles_cjk_similarity() {
        // charabia 分词：中文近重复也能被识别为冗余。
        let items = vec![
            value_item("a", 1.0, "记忆系统做梦整理周期与想象假设生成"),
            value_item("b", 0.95, "记忆系统做梦整理周期与想象假设生成"),
            value_item("c", 0.5, "浏览器自动化标签页嵌入与版本握手"),
        ];
        let reranked = mmr_rerank_values(items, 0.5, 2);
        let ids: Vec<&str> = reranked
            .iter()
            .map(|v| v.get("id").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids, ["a", "c"], "中文近重复应被 MMR 压制");
    }
}
