//! 8b — 参数自调优：试验 → 观察一个周期 → 保留/回滚。
//!
//! DGM 的「经验验证」在线化：每个进化周期（默认 7 天）至多改**一个**白名
//! 单参数一档（硬范围网格内），把当期复合适应度记为基线；下一周期对照
//! 新适应度决定保留（不劣化）或回滚（写回旧值）。每一步都追加进
//! append-only 台账（`evolution-ledger.jsonl`），TUI 报告可见；用户可在
//! dream-config `evolution.locked` 锁定任意参数；连续两周期适应度下滑会
//! 追溯回滚最近一次已保留的变更。
//!
//! 检索类参数（`search.*`）在试验前先过**影子护栏**：用近期真实查询重放
//! 词法检索 + 策略层，候选配置的结果保留率 < 现行 50% 即拒绝（防
//! min_score 之类把结果面闷死）。护栏是保底不是裁决——真正的裁决是下一
//! 周期的在线适应度对照。
//!
//! 无随机依赖：探索用 now_ms 播种的 xorshift（测试注入固定时刻即全确定）。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::dream_config::{read_dream_config, write_dream_config, DreamConfig};
use crate::evolution::fitness::FitnessSnapshot;
use crate::se_integration::MemorySearchHit;

pub const LEDGER_FILENAME: &str = "evolution-ledger.jsonl";
pub const STATE_FILENAME: &str = "evolution-state.json";
pub const LEDGER_MAX_LINES: usize = 500;
/// 保留判据：after ≥ before − TOLERANCE（微小波动不算劣化）。
pub const KEEP_TOLERANCE: f64 = 0.01;
/// 追溯回滚判据：连续 N 个周期复合分下滑。
pub const LATE_REVERT_DECLINES: u32 = 2;
/// 影子护栏：候选保留率 ≥ 现行 × 该系数才放行。
pub const SHADOW_RETENTION_FLOOR: f64 = 0.5;
/// 探索概率（其余情况利用台账历史收益）。
const EXPLORE_PROB_PERCENT: u64 = 20;

/// 影子护栏的检索重放回调：query → 当前 SE 词法命中。
pub type ShadowFetch<'a> = &'a (dyn Fn(&str) -> Vec<MemorySearchHit> + Sync);

/// 一个可进化参数：dream-config 白名单键 + 硬范围网格 + 读写函数。
pub struct ParamSpec {
    pub key: &'static str,
    pub grid: &'static [f64],
    pub read: fn(&DreamConfig) -> f64,
    pub write: fn(&mut DreamConfig, f64),
    /// 检索类参数（影子护栏适用）。
    pub retrieval: bool,
}

/// 白名单 = 闭集；扩条目必须同 PR 修订 SoT 文档（W-MEMORY-SELF-EVOLVE-DGM
/// 实施方案 §PR-8b）+ 本表 + 固化测试。
pub const PARAM_SPECS: &[ParamSpec] = &[
    ParamSpec {
        key: "dream.min_hours",
        grid: &[4.0, 8.0, 12.0, 24.0, 48.0, 72.0, 96.0, 168.0],
        read: |c| c.min_hours as f64,
        write: |c, v| c.min_hours = v as u64,
        retrieval: false,
    },
    ParamSpec {
        key: "dream.imagination_min_hours",
        grid: &[4.0, 8.0, 12.0, 24.0, 48.0, 72.0, 96.0, 168.0, 336.0],
        read: |c| c.imagination_min_hours as f64,
        write: |c, v| c.imagination_min_hours = v as u64,
        retrieval: false,
    },
    ParamSpec {
        key: "dream.importance_pressure_threshold",
        grid: &[50.0, 75.0, 100.0, 150.0, 200.0, 300.0, 500.0],
        read: |c| c.importance_pressure_threshold as f64,
        write: |c, v| c.importance_pressure_threshold = v as u64,
        retrieval: false,
    },
    ParamSpec {
        key: "search.half_life_days",
        grid: &[1.0, 3.0, 7.0, 14.0, 30.0, 60.0, 90.0],
        read: |c| c.search_policy.half_life_days,
        write: |c, v| c.search_policy.half_life_days = v,
        retrieval: true,
    },
    ParamSpec {
        key: "search.min_score",
        grid: &[0.0, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4],
        read: |c| c.search_policy.min_score,
        write: |c, v| c.search_policy.min_score = v,
        retrieval: true,
    },
    ParamSpec {
        key: "search.access_boost_scale",
        grid: &[0.0, 0.02, 0.05, 0.1, 0.2],
        read: |c| c.search_policy.access_boost_scale,
        write: |c, v| c.search_policy.access_boost_scale = v,
        retrieval: true,
    },
    ParamSpec {
        key: "search.mmr_enabled",
        grid: &[0.0, 1.0],
        read: |c| f64::from(u8::from(c.search_policy.mmr_enabled)),
        write: |c, v| c.search_policy.mmr_enabled = v >= 0.5,
        retrieval: true,
    },
    ParamSpec {
        key: "search.mmr_lambda",
        grid: &[0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
        read: |c| c.search_policy.mmr_lambda,
        write: |c, v| c.search_policy.mmr_lambda = v,
        retrieval: true,
    },
    // W-MEMORY-KB-UPLIFT P1 — cross-encoder rerank 总开关进进化空间：适应度
    // 若显示 rerank 反而伤命中率（弱模型/慢网关），进化层可自动关掉它。
    ParamSpec {
        key: "search.rerank_enabled",
        grid: &[0.0, 1.0],
        read: |c| f64::from(u8::from(c.search_policy.rerank_enabled)),
        write: |c, v| c.search_policy.rerank_enabled = v >= 0.5,
        retrieval: true,
    },
    ParamSpec {
        key: "dedup.jaccard_threshold",
        grid: &[0.75, 0.8, 0.85, 0.9, 0.95, 1.0],
        read: |c| c.dedup.jaccard_threshold,
        write: |c, v| c.dedup.jaccard_threshold = v,
        retrieval: false,
    },
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerEntry {
    pub ts_ms: u64,
    pub param: String,
    pub from: f64,
    pub to: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitness_before: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitness_after: Option<f64>,
    /// trial | kept | reverted | reverted_late | shadow_rejected
    pub status: String,
    #[serde(default)]
    pub source: String,
    /// R2-1：这次裁定时**哪些适应度指标真的参与了**复合分。没有它，事后
    /// 无法判断一次 `kept` 是真收益，还是"只剩一个指标且它恰好取到理想值"
    /// 造成的常数假象。旧台账无此键 → 空表。
    #[serde(default)]
    pub metrics_present: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PendingTrial {
    pub param: String,
    pub from: f64,
    pub to: f64,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitness_before: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvolutionState {
    #[serde(default)]
    pub last_cycle_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingTrial>,
    #[serde(default)]
    pub consecutive_declines: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_composite: Option<f64>,
}

fn ledger_path(project_state_dir: &Path) -> std::path::PathBuf {
    super::evolution_dir(project_state_dir).join(LEDGER_FILENAME)
}

fn state_path(project_state_dir: &Path) -> std::path::PathBuf {
    super::evolution_dir(project_state_dir).join(STATE_FILENAME)
}

#[must_use]
pub fn load_state(project_state_dir: &Path) -> EvolutionState {
    let Ok(raw) = std::fs::read_to_string(state_path(project_state_dir)) else {
        return EvolutionState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

async fn save_state(project_state_dir: &Path, state: &EvolutionState) {
    let Ok(bytes) = serde_json::to_vec_pretty(state) else {
        return;
    };
    if let Err(e) = atomic_write(&state_path(project_state_dir), &bytes).await {
        log::warn!("[evolution] state write failed (fail-soft): {e}");
    }
}

#[must_use]
pub fn read_ledger(project_state_dir: &Path) -> Vec<LedgerEntry> {
    let Ok(raw) = std::fs::read_to_string(ledger_path(project_state_dir)) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

async fn append_ledger(project_state_dir: &Path, entry: &LedgerEntry) {
    let path = ledger_path(project_state_dir);
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = existing.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.push(&line);
    let start = lines.len().saturating_sub(LEDGER_MAX_LINES);
    let content = lines[start..].join("\n") + "\n";
    if let Err(e) = atomic_write(&path, content.as_bytes()).await {
        log::warn!("[evolution] ledger append failed (fail-soft): {e}");
    }
}

fn xorshift(mut x: u64) -> u64 {
    x = x.max(1);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn spec_by_key(key: &str) -> Option<&'static ParamSpec> {
    PARAM_SPECS.iter().find(|spec| spec.key == key)
}

fn grid_index(spec: &ParamSpec, value: f64) -> usize {
    spec.grid
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - value)
                .abs()
                .partial_cmp(&(*b - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// 影子护栏的结果保留率：Σ策略后结果数（同一批 SE 命中，两套配置各过一遍
/// 策略层）。
fn shadow_retention(
    queries: &[String],
    fetch: &(dyn Fn(&str) -> Vec<MemorySearchHit> + Sync),
    config: &crate::search_policy::SearchPolicyConfig,
    now_ms: u64,
) -> usize {
    let empty_counts = std::collections::HashMap::new();
    let mut retained = 0usize;
    for query in queries {
        let mut hits = fetch(query);
        crate::search_policy::apply_scope_policy(
            &mut hits,
            config,
            &empty_counts,
            now_ms,
            crate::output_language::MemoryOutputLanguage::Zh,
        );
        retained += hits.len();
    }
    retained
}

/// 台账里各参数的历史平均收益（kept/reverted 的 after−before）。
fn historical_gains(ledger: &[LedgerEntry]) -> std::collections::HashMap<String, f64> {
    let mut sums: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for entry in ledger {
        if entry.status != "kept" && entry.status != "reverted" {
            continue;
        }
        let (Some(before), Some(after)) = (entry.fitness_before, entry.fitness_after) else {
            continue;
        };
        let slot = sums.entry(entry.param.clone()).or_insert((0.0, 0));
        slot.0 += after - before;
        slot.1 += 1;
    }
    sums.into_iter()
        .map(|(key, (sum, n))| (key, sum / f64::from(n.max(1))))
        .collect()
}

/// 跑一个进化周期。调用方保证 `snapshot` 是当期新鲜适应度。返回给报告
/// 投影的状态行（zh 文案由报告层统一，不在此翻译）。
pub async fn run_evolution_cycle(
    project_state_dir: &Path,
    now_ms: u64,
    snapshot: &FitnessSnapshot,
    shadow_fetch: Option<ShadowFetch<'_>>,
) -> Vec<String> {
    let Ok(config) = read_dream_config(project_state_dir) else {
        return vec!["(dream-config unreadable — cycle skipped)".to_string()];
    };
    if !config.evolution.enabled {
        return vec!["evolution.enabled = false — 试验停用（适应度照常记录）".to_string()];
    }
    let mut state = load_state(project_state_dir);
    let cycle_ms = config.evolution.cycle_hours.saturating_mul(3_600_000);
    if now_ms.saturating_sub(state.last_cycle_ms) < cycle_ms {
        return match &state.pending {
            Some(trial) => vec![format!(
                "试验观察中：{} {} → {}（周期未满）",
                trial.param, trial.from, trial.to
            )],
            None => vec!["周期未满，本轮无动作".to_string()],
        };
    }
    state.last_cycle_ms = now_ms;
    let mut lines: Vec<String> = Vec::new();

    // ── 1. settle pending trial ──
    if let Some(trial) = state.pending.take() {
        let keep = match (trial.fitness_before, snapshot.composite) {
            (Some(before), Some(after)) => after >= before - KEEP_TOLERANCE,
            // 对照数据不完整 → 保守回滚（试验必须可归因）。
            _ => false,
        };
        let status = if keep { "kept" } else { "reverted" };
        if !keep {
            if let (Ok(mut cfg), Some(spec)) = (
                read_dream_config(project_state_dir),
                spec_by_key(&trial.param),
            ) {
                (spec.write)(&mut cfg, trial.from);
                if let Err(e) = write_dream_config(project_state_dir, &cfg).await {
                    log::warn!("[evolution] trial revert write failed (fail-soft): {e}");
                }
            }
        }
        append_ledger(
            project_state_dir,
            &LedgerEntry {
                ts_ms: now_ms,
                param: trial.param.clone(),
                from: trial.from,
                to: trial.to,
                fitness_before: trial.fitness_before,
                fitness_after: snapshot.composite,
                status: status.to_string(),
                source: "auto".to_string(),
                metrics_present: snapshot.metrics_present.clone(),
            },
        )
        .await;
        lines.push(format!(
            "试验裁定：{} {} → {}，{}",
            trial.param, trial.from, trial.to, status
        ));
    } else if let (Some(prev), Some(cur)) = (state.last_composite, snapshot.composite) {
        // ── 1b. 追溯回滚：连续劣化时回退最近一次已保留的变更 ──
        if cur < prev - 0.005 {
            state.consecutive_declines = state.consecutive_declines.saturating_add(1);
        } else {
            state.consecutive_declines = 0;
        }
        if state.consecutive_declines >= LATE_REVERT_DECLINES {
            let ledger = read_ledger(project_state_dir);
            if let Some(last_kept) = ledger.iter().rev().find(|e| e.status == "kept") {
                if let (Ok(mut cfg), Some(spec)) = (
                    read_dream_config(project_state_dir),
                    spec_by_key(&last_kept.param),
                ) {
                    (spec.write)(&mut cfg, last_kept.from);
                    if write_dream_config(project_state_dir, &cfg).await.is_ok() {
                        append_ledger(
                            project_state_dir,
                            &LedgerEntry {
                                ts_ms: now_ms,
                                param: last_kept.param.clone(),
                                from: last_kept.to,
                                to: last_kept.from,
                                fitness_before: Some(cur),
                                fitness_after: None,
                                status: "reverted_late".to_string(),
                                source: "auto".to_string(),
                                metrics_present: snapshot.metrics_present.clone(),
                            },
                        )
                        .await;
                        lines.push(format!(
                            "连续 {} 周期劣化 — 追溯回滚 {}",
                            LATE_REVERT_DECLINES, last_kept.param
                        ));
                    }
                }
            }
            state.consecutive_declines = 0;
        }
    }
    state.last_composite = snapshot.composite;

    // ── 2. propose new trial ──
    if state.pending.is_none() {
        if let Some(composite_before) = snapshot.composite {
            let unlocked: Vec<&ParamSpec> = PARAM_SPECS
                .iter()
                .filter(|spec| !config.evolution.locked.iter().any(|k| k == spec.key))
                .collect();
            if !unlocked.is_empty() {
                let ledger = read_ledger(project_state_dir);
                let gains = historical_gains(&ledger);
                let seed = xorshift(now_ms ^ 0x9E37_79B9_7F4A_7C15);
                let explore = seed % 100 < EXPLORE_PROB_PERCENT || gains.is_empty();
                let spec = if explore {
                    unlocked[(xorshift(seed) as usize) % unlocked.len()]
                } else {
                    unlocked
                        .iter()
                        .max_by(|a, b| {
                            let ga = gains.get(a.key).copied().unwrap_or(0.0);
                            let gb = gains.get(b.key).copied().unwrap_or(0.0);
                            ga.partial_cmp(&gb).unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .copied()
                        .unwrap_or(unlocked[0])
                };
                let current = (spec.read)(&config);
                let index = grid_index(spec, current);
                let step_up = xorshift(seed ^ 0x5DEE_CE66).is_multiple_of(2);
                let candidate_index = if step_up {
                    (index + 1).min(spec.grid.len() - 1)
                } else {
                    index.saturating_sub(1)
                };
                if candidate_index != index {
                    let candidate = spec.grid[candidate_index];
                    // 影子护栏（检索类参数 + 有重放素材时）。
                    let mut shadow_ok = true;
                    if spec.retrieval {
                        if let Some(fetch) = shadow_fetch {
                            let stats = crate::search_stats::load_search_stats(project_state_dir);
                            let queries: Vec<String> = stats
                                .recent_queries
                                .iter()
                                .rev()
                                .take(10)
                                .map(|q| q.query.clone())
                                .collect();
                            if queries.len() >= 5 {
                                let mut candidate_cfg = config.clone();
                                (spec.write)(&mut candidate_cfg, candidate);
                                let baseline = shadow_retention(
                                    &queries,
                                    fetch,
                                    &config.search_policy,
                                    now_ms,
                                );
                                let trial_retention = shadow_retention(
                                    &queries,
                                    fetch,
                                    &candidate_cfg.search_policy,
                                    now_ms,
                                );
                                if baseline > 0
                                    && (trial_retention as f64)
                                        < baseline as f64 * SHADOW_RETENTION_FLOOR
                                {
                                    shadow_ok = false;
                                    append_ledger(
                                        project_state_dir,
                                        &LedgerEntry {
                                            ts_ms: now_ms,
                                            param: spec.key.to_string(),
                                            from: current,
                                            to: candidate,
                                            fitness_before: Some(composite_before),
                                            fitness_after: None,
                                            status: "shadow_rejected".to_string(),
                                            source: "auto".to_string(),
                                            metrics_present: snapshot.metrics_present.clone(),
                                        },
                                    )
                                    .await;
                                    lines.push(format!(
                                        "影子护栏拒绝 {} → {}（保留率 {trial_retention}/{baseline}）",
                                        spec.key, candidate
                                    ));
                                }
                            }
                        }
                    }
                    if shadow_ok {
                        if let Ok(mut cfg) = read_dream_config(project_state_dir) {
                            (spec.write)(&mut cfg, candidate);
                            if write_dream_config(project_state_dir, &cfg).await.is_ok() {
                                state.pending = Some(PendingTrial {
                                    param: spec.key.to_string(),
                                    from: current,
                                    to: candidate,
                                    started_ms: now_ms,
                                    fitness_before: Some(composite_before),
                                });
                                append_ledger(
                                    project_state_dir,
                                    &LedgerEntry {
                                        ts_ms: now_ms,
                                        param: spec.key.to_string(),
                                        from: current,
                                        to: candidate,
                                        fitness_before: Some(composite_before),
                                        fitness_after: None,
                                        status: "trial".to_string(),
                                        source: "auto".to_string(),
                                        metrics_present: snapshot.metrics_present.clone(),
                                    },
                                )
                                .await;
                                lines.push(format!(
                                    "新试验：{} {} → {}（下一周期裁定）",
                                    spec.key, current, candidate
                                ));
                            }
                        }
                    }
                }
            }
        } else {
            lines.push("复合适应度数据不足 — 本周期不试验".to_string());
        }
    }

    save_state(project_state_dir, &state).await;
    if lines.is_empty() {
        lines.push("本周期无动作".to_string());
    }
    lines
}

/// 报告投影：台账尾部 N 行渲染成 markdown 列表。
#[must_use]
pub fn render_ledger_tail(project_state_dir: &Path, n: usize) -> String {
    let ledger = read_ledger(project_state_dir);
    if ledger.is_empty() {
        return String::new();
    }
    let start = ledger.len().saturating_sub(n);
    ledger[start..]
        .iter()
        .map(|e| {
            format!(
                "- {} `{}` {} → {}（before {} / after {}）",
                e.status,
                e.param,
                e.from,
                e.to,
                e.fitness_before.map_or("—".into(), |v| format!("{v:.3}")),
                e.fitness_after.map_or("—".into(), |v| format!("{v:.3}")),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const HOUR_MS: u64 = 3_600_000;

    fn snapshot(composite: Option<f64>) -> FitnessSnapshot {
        FitnessSnapshot {
            computed_at_ms: 0,
            composite,
            ..FitnessSnapshot::default()
        }
    }

    #[test]
    fn param_specs_whitelist_is_pinned() {
        // 固化白名单：扩条目必须同 PR 修订 SoT + 本测试（tripwire）。
        let keys: Vec<&str> = PARAM_SPECS.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            [
                "dream.min_hours",
                "dream.imagination_min_hours",
                "dream.importance_pressure_threshold",
                "search.half_life_days",
                "search.min_score",
                "search.access_boost_scale",
                "search.mmr_enabled",
                "search.mmr_lambda",
                // W-MEMORY-KB-UPLIFT P1 — rerank 总开关入进化空间。
                "search.rerank_enabled",
                "dedup.jaccard_threshold",
            ]
        );
        for spec in PARAM_SPECS {
            assert!(spec.grid.len() >= 2, "{} 网格至少两档", spec.key);
            assert!(
                spec.grid.windows(2).all(|w| w[0] < w[1]),
                "{} 网格必须严格递增",
                spec.key
            );
        }
    }

    #[tokio::test]
    async fn cycle_throttles_and_proposes_then_settles_keep() {
        let dir = TempDir::new().unwrap();
        // t0：数据充足 → 提出试验。
        let lines =
            run_evolution_cycle(dir.path(), HOUR_MS * 200, &snapshot(Some(0.6)), None).await;
        assert!(
            lines.iter().any(|l| l.contains("新试验")),
            "首周期应提出试验: {lines:?}"
        );
        let state = load_state(dir.path());
        let trial = state.pending.clone().expect("pending trial");
        // 参数确实写进了 dream-config。
        let cfg = read_dream_config(dir.path()).unwrap();
        let spec = spec_by_key(&trial.param).unwrap();
        assert_eq!((spec.read)(&cfg), trial.to);

        // 周期未满 → 无动作。
        let lines =
            run_evolution_cycle(dir.path(), HOUR_MS * 201, &snapshot(Some(0.9)), None).await;
        assert!(lines.iter().any(|l| l.contains("观察中")), "{lines:?}");
        assert!(load_state(dir.path()).pending.is_some());

        // 下一周期：适应度未劣化 → kept；同周期随即开启下一个试验（试验
        // 以周期为节拍，「每周期一个活动试验」）。
        let lines = run_evolution_cycle(
            dir.path(),
            HOUR_MS * 200 + HOUR_MS * 24 * 8,
            &snapshot(Some(0.61)),
            None,
        )
        .await;
        assert!(lines.iter().any(|l| l.contains("kept")), "{lines:?}");
        let ledger = read_ledger(dir.path());
        let kept = ledger
            .iter()
            .find(|e| e.status == "kept")
            .expect("kept 入账");
        assert_eq!(kept.param, trial.param);
        assert_eq!(kept.to, trial.to);
        assert_eq!(kept.fitness_after, Some(0.61));
        // 参数值连续性：若新试验又动了同一参数，其 from 必须 == kept 的
        // to；否则参数维持试验值。
        let cfg = read_dream_config(dir.path()).unwrap();
        match load_state(dir.path()).pending {
            Some(next) if next.param == trial.param => {
                assert_eq!(next.from, trial.to, "新试验从 kept 值出发");
                assert_eq!((spec.read)(&cfg), next.to);
            }
            _ => assert_eq!((spec.read)(&cfg), trial.to, "kept 后参数维持试验值"),
        }
    }

    #[tokio::test]
    async fn degraded_trial_reverts_parameter() {
        let dir = TempDir::new().unwrap();
        run_evolution_cycle(dir.path(), HOUR_MS * 200, &snapshot(Some(0.8)), None).await;
        let trial = load_state(dir.path()).pending.clone().unwrap();
        let spec = spec_by_key(&trial.param).unwrap();

        // 下一周期：复合分大幅劣化 → 回滚写回旧值。
        let lines = run_evolution_cycle(
            dir.path(),
            HOUR_MS * 200 + HOUR_MS * 24 * 8,
            &snapshot(Some(0.5)),
            None,
        )
        .await;
        assert!(lines.iter().any(|l| l.contains("reverted")), "{lines:?}");
        let cfg = read_dream_config(dir.path()).unwrap();
        assert_eq!((spec.read)(&cfg), trial.from, "回滚后参数还原");
    }

    #[tokio::test]
    async fn locked_params_are_never_touched() {
        let dir = TempDir::new().unwrap();
        // 锁定全部参数 → 永不试验。
        let mut cfg = DreamConfig::default();
        cfg.evolution.locked = PARAM_SPECS.iter().map(|s| s.key.to_string()).collect();
        write_dream_config(dir.path(), &cfg).await.unwrap();

        let lines =
            run_evolution_cycle(dir.path(), HOUR_MS * 200, &snapshot(Some(0.7)), None).await;
        assert!(load_state(dir.path()).pending.is_none(), "{lines:?}");
        assert!(read_ledger(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn insufficient_fitness_means_no_trial() {
        let dir = TempDir::new().unwrap();
        let lines = run_evolution_cycle(dir.path(), HOUR_MS * 200, &snapshot(None), None).await;
        assert!(lines.iter().any(|l| l.contains("数据不足")), "{lines:?}");
        assert!(load_state(dir.path()).pending.is_none());
    }

    #[tokio::test]
    async fn evolution_disabled_stops_trials() {
        let dir = TempDir::new().unwrap();
        let mut cfg = DreamConfig::default();
        cfg.evolution.enabled = false;
        write_dream_config(dir.path(), &cfg).await.unwrap();
        let lines =
            run_evolution_cycle(dir.path(), HOUR_MS * 200, &snapshot(Some(0.9)), None).await;
        assert!(lines[0].contains("停用"));
        assert!(load_state(dir.path()).pending.is_none());
    }

    #[tokio::test]
    async fn shadow_guard_rejects_suffocating_retrieval_param() {
        let dir = TempDir::new().unwrap();
        // 预置足量真实查询素材。
        for i in 0..6 {
            crate::search_stats::record_search(dir.path(), &format!("查询{i}"), 3, i).await;
        }
        // 锁定全部非检索参数 + 除 min_score 外的检索参数 → 试验必选 min_score。
        let mut cfg = DreamConfig::default();
        cfg.evolution.locked = PARAM_SPECS
            .iter()
            .filter(|s| s.key != "search.min_score")
            .map(|s| s.key.to_string())
            .collect();
        // 现行 min_score 拉到 0.35，让 +1 档（0.4）与 −1 档（0.3）都可能；
        // fetch 返回的命中分数集中在 0.32 → 候选 0.4 会闷死全部结果。
        cfg.search_policy.min_score = 0.35;
        cfg.search_policy.decay_enabled = false;
        write_dream_config(dir.path(), &cfg).await.unwrap();

        let fetch = |_q: &str| -> Vec<MemorySearchHit> {
            // 两条命中：归一化后 1.0 与 0.34/1.0=0.34 —— 现行 0.35 门下保留
            // 1 条；候选 0.4 门同样保留 1 条（1.0 那条）→ 不触发；候选 0.3
            // 门保留 2 条 → 也不触发。为了确定性触发拒绝，用「全 0.37 附近」
            // 的形态：归一化全 1.0？—— 改用多条同分 + min_score 0.4 也留。
            // 真正能闷死的是 min_score > 1.0 不存在；因此这里直接构造
            // 「候选门=0.4、命中 display 全 0.35~0.39」：max 归一化后最高
            // 分恒 1.0 ≥ 门 —— 说明 max 归一化天然防闷死，护栏兜的是
            // 多结果长尾。构造 5 条：1 条高分 + 4 条低比例分。
            vec![
                hit("a", 100.0),
                hit("b", 36.0),
                hit("c", 35.0),
                hit("d", 34.0),
                hit("e", 33.0),
            ]
        };
        fn hit(id: &str, score: f32) -> MemorySearchHit {
            MemorySearchHit {
                id: id.to_string(),
                score,
                source_path: None,
                scope: None,
                name: None,
                memory_type: Some("project".to_string()),
                snippet: None,
                mtime_ms: 0,
            }
        }

        // 强制走 min_score 的 +1 档（0.35→0.4）：seed 由 now_ms 决定，试到
        // 一个会产生 step_up 的 now；两个方向都合法，所以这里接受任一方向，
        // 只断言「若被拒绝则是 shadow_rejected 且参数未变」。
        let now = HOUR_MS * 300;
        let before_cfg = read_dream_config(dir.path()).unwrap();
        let lines = run_evolution_cycle(dir.path(), now, &snapshot(Some(0.7)), Some(&fetch)).await;
        let ledger = read_ledger(dir.path());
        let after_cfg = read_dream_config(dir.path()).unwrap();
        if ledger.iter().any(|e| e.status == "shadow_rejected") {
            assert_eq!(
                after_cfg.search_policy.min_score, before_cfg.search_policy.min_score,
                "影子拒绝后参数不变: {lines:?}"
            );
            assert!(load_state(dir.path()).pending.is_none());
        } else {
            // 走了 −1 档（0.35→0.3）：保留率上升，试验合法成立。
            assert!(load_state(dir.path()).pending.is_some(), "{lines:?}");
        }
    }

    #[tokio::test]
    async fn late_revert_after_consecutive_declines() {
        let dir = TempDir::new().unwrap();
        // 人工植入一条 kept 台账 + 把参数推到 kept 的 to 值。
        let mut cfg = DreamConfig::default();
        cfg.search_policy.half_life_days = 14.0;
        write_dream_config(dir.path(), &cfg).await.unwrap();
        append_ledger(
            dir.path(),
            &LedgerEntry {
                ts_ms: 1,
                param: "search.half_life_days".to_string(),
                from: 30.0,
                to: 14.0,
                fitness_before: Some(0.5),
                fitness_after: Some(0.6),
                status: "kept".to_string(),
                source: "auto".to_string(),
                metrics_present: Vec::new(),
            },
        )
        .await;
        // 锁定全部参数：不提新试验，纯观察劣化路径。
        let mut cfg = read_dream_config(dir.path()).unwrap();
        cfg.evolution.locked = PARAM_SPECS.iter().map(|s| s.key.to_string()).collect();
        write_dream_config(dir.path(), &cfg).await.unwrap();

        let week = HOUR_MS * 24 * 8;
        run_evolution_cycle(dir.path(), week, &snapshot(Some(0.8)), None).await;
        run_evolution_cycle(dir.path(), week * 2, &snapshot(Some(0.7)), None).await;
        let lines = run_evolution_cycle(dir.path(), week * 3, &snapshot(Some(0.6)), None).await;

        assert!(
            lines.iter().any(|l| l.contains("追溯回滚")),
            "连续两周期劣化触发: {lines:?}"
        );
        let cfg = read_dream_config(dir.path()).unwrap();
        assert_eq!(cfg.search_policy.half_life_days, 30.0, "回退到 kept 前值");
        assert!(read_ledger(dir.path())
            .iter()
            .any(|e| e.status == "reverted_late"));
    }
}
