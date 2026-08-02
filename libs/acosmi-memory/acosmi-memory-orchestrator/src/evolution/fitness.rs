//! 8a — 适应度基线：没有可信的适应度函数，进化 = 随机漂移。
//!
//! 六个指标全部**离线可算、零新增 LLM 成本**，复用既有账本：
//! 1. 检索命中率 — `search_stats`（在线真实使用信号，非合成探针）；
//! 2. 产物重复率 — dreams/ 抽样两两词集 Jaccard；
//! 3. 存活效率 — meta-review 聚合（进入队列的假设 ÷ 生成的候选）；
//! 4. 过期率 — meta-review 聚合（expired ÷ candidates）；
//! 5. 门浪费率 — `gate_stats::waste_ratio`（corpus_empty 无效唤醒）；
//! 6. importance 校准 — frontmatter importance 与检索命中数的 Pearson 相关
//!    （高分记忆是否真的高频被用）。
//!
//! 复合分 = 可得指标的加权平均（缺失指标按剩余权重重归一，绝不用假 0
//! 污染）。快照追加进 `fitness-history.json`（cap 52 ≈ 一年周历史），并
//! 投影成 report 类 markdown（`<memory_dir>/reports/evolution-fitness.md`）
//! 进现有「进化报告」tab —— 零协议改动。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::output_language::MemoryOutputLanguage;

pub const FITNESS_HISTORY_FILENAME: &str = "fitness-history.json";
pub const FITNESS_HISTORY_CAP: usize = 52;
/// 重复率抽样上限（dreams/ 最新 N 个文件两两比较，O(N²) 有界）。
const DUP_SAMPLE_CAP: usize = 24;
/// 重复判定 Jaccard 阈值（与 G3-c 写前门独立——这里是度量不是拦截）。
const DUP_JACCARD: f64 = 0.8;
/// 校准相关所需最小样本对。
const CALIBRATION_MIN_SAMPLES: usize = 5;
/// 命中率样本地板（低于此不进复合分）。
const HIT_RATE_MIN_SAMPLES: u64 = 5;

/// 快照 schema 版本。2026-07-27 起 = 2：`composite` 语义变了（新增双下限
/// 守卫 + 新增 `system_error_rate` 维度 + `gate_waste_ratio` 分母口径修正），
/// **旧记录与新记录不可直接比较**。报告层据此标注"旧口径"。
pub const FITNESS_SCHEMA_VERSION: u32 = 2;

// ── 指标权重（复合分）。缺失指标剔除后按**剩余**权重重归一。 ──
const W_HIT: f64 = 0.35;
const W_DUP: f64 = 0.20;
const W_SURVIVOR: f64 = 0.20;
const W_WASTE: f64 = 0.10;
const W_CALIBRATION: f64 = 0.15;
/// §19.1-7：「系统是否在正常工作」这一维的权重**不低于现有任何单项**。
const W_ERROR: f64 = 0.35;
/// 全量权重（覆盖率分母）。
const W_TOTAL: f64 = W_HIT + W_DUP + W_SURVIVOR + W_WASTE + W_CALIBRATION + W_ERROR;

/// 复合分参与门槛之一：至少要有这么多项指标有值。
///
/// R2-1 根因：「缺失即剔除 + 剩余权重重归一」这个**优雅降级**，与 tuner 的
/// 接受判据 `after >= before - KEEP_TOLERANCE` 相乘，会产生一个**恒真的
/// 自我肯定循环** —— 当只剩一个指标、且它恰好取到理想值时，复合分被推到
/// 1.0（理论最大值），而 1.0 又让保留判据永真，于是任何试验都会被保留。
/// 实测本机：六项指标只剩一项有值、复合分恒 1.0、三条台账的
/// `fitness_before` 全是 1.0。两个各自合理的设计相乘 = 一个恒真的门。
const MIN_METRICS_PRESENT: usize = 3;

/// 复合分参与门槛之二：剔除缺失项后，剩余权重占全量权重的比例下限。
/// 单靠"指标数"挡不住三个小权重指标凑数的情形。
const MIN_WEIGHT_COVERAGE: f64 = 0.5;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FitnessSnapshot {
    pub computed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_hit_rate: Option<f64>,
    #[serde(default)]
    pub retrieval_samples: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplication_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survivor_efficiency: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_waste_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance_calibration: Option<f64>,
    /// **系统是否在正常工作**（自驱 lane 的 errored ÷ 尝试数，§19.1-7）。
    /// 此前整套适应度里没有这一维：一个每次做梦都失败的项目，与一个完全
    /// 健康的项目在适应度上无法区分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_error_rate: Option<f64>,
    /// 加权复合分 [0,1]；指标数或权重覆盖率不足 → None（数据不足，8b 不
    /// 动作）。见 [`MIN_METRICS_PRESENT`] / [`MIN_WEIGHT_COVERAGE`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<f64>,
    /// 本次复合分**实际参与**了哪些指标。没有它，事后无法判断一次 `kept`
    /// 是真收益还是"只剩一个指标且它取到理想值"的常数假象（R2-1）。
    #[serde(default)]
    pub metrics_present: Vec<String>,
    /// 快照 schema 版本，见 [`FITNESS_SCHEMA_VERSION`]。缺失（旧记录）= 1。
    #[serde(default)]
    pub schema_version: u32,
}

impl FitnessSnapshot {
    /// 复合分：hit .35 / (1-dup) .20 / survivor .20 / (1-waste) .10 /
    /// 校准((c+1)/2) .15 / (1-error) .35。缺失指标剔除后按剩余权重重归一，
    /// **但必须先过双下限**（[`MIN_METRICS_PRESENT`] +
    /// [`MIN_WEIGHT_COVERAGE`]），否则返回 None —— 见两个常量的根因说明。
    fn compute_composite(&mut self) {
        let mut weighted: Vec<(f64, f64)> = Vec::new();
        let mut present: Vec<String> = Vec::new();
        let mut push = |weight: f64, value: f64, name: &str| {
            weighted.push((weight, value));
            present.push(name.to_string());
        };
        if let Some(hit) = self.retrieval_hit_rate {
            push(W_HIT, hit, "retrieval_hit_rate");
        }
        if let Some(dup) = self.duplication_rate {
            push(W_DUP, 1.0 - dup, "duplication_rate");
        }
        if let Some(survivor) = self.survivor_efficiency {
            push(W_SURVIVOR, survivor, "survivor_efficiency");
        }
        if let Some(waste) = self.gate_waste_ratio {
            push(W_WASTE, 1.0 - waste, "gate_waste_ratio");
        }
        if let Some(calibration) = self.importance_calibration {
            push(
                W_CALIBRATION,
                (calibration + 1.0) / 2.0,
                "importance_calibration",
            );
        }
        if let Some(error_rate) = self.system_error_rate {
            push(W_ERROR, 1.0 - error_rate, "system_error_rate");
        }
        let total_weight: f64 = weighted.iter().map(|(w, _)| w).sum();
        self.metrics_present = present;
        self.composite = if weighted.len() < MIN_METRICS_PRESENT
            || total_weight / W_TOTAL < MIN_WEIGHT_COVERAGE
        {
            // 度量不可信 → 没有分，而不是给一个好看的分。
            None
        } else {
            Some(
                (weighted
                    .iter()
                    .map(|(w, v)| w * v.clamp(0.0, 1.0))
                    .sum::<f64>()
                    / total_weight)
                    .clamp(0.0, 1.0),
            )
        };
    }
}

/// meta-review.jsonl 全量聚合（cap 100 行，读整文件可接受）。
fn aggregate_meta_review(memory_dir: &Path) -> (u64, u64, u64) {
    let path = memory_dir.join("imagination").join("meta-review.jsonl");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (0, 0, 0);
    };
    let (mut candidates, mut queued, mut expired) = (0u64, 0u64, 0u64);
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let get = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        candidates += get("candidates");
        queued += get("queued_high") + get("queued_medium");
        expired += get("expired");
    }
    (candidates, queued, expired)
}

/// dreams/ 最新 `DUP_SAMPLE_CAP` 个 md 两两 Jaccard，返回重复对占比。
fn sample_duplication_rate(memory_dir: &Path) -> Option<f64> {
    let dreams = memory_dir.join("dreams");
    let Ok(read_dir) = std::fs::read_dir(&dreams) else {
        return None;
    };
    let mut files: Vec<(u64, std::path::PathBuf)> = read_dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                return None;
            }
            let mtime = entry
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            Some((mtime, path))
        })
        .collect();
    if files.len() < 4 {
        return None;
    }
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    files.truncate(DUP_SAMPLE_CAP);

    let token_sets: Vec<std::collections::HashSet<String>> = files
        .iter()
        .filter_map(|(_, path)| std::fs::read_to_string(path).ok())
        .map(|content| {
            crate::se_integration::tokenize(crate::dedup_hash::memory_body(&content))
                .into_iter()
                .collect()
        })
        .collect();
    if token_sets.len() < 4 {
        return None;
    }
    let mut pairs = 0u64;
    let mut duplicated = 0u64;
    for i in 0..token_sets.len() {
        for j in (i + 1)..token_sets.len() {
            let (a, b) = (&token_sets[i], &token_sets[j]);
            if a.is_empty() || b.is_empty() {
                continue;
            }
            pairs += 1;
            let intersection = a.intersection(b).count() as f64;
            let union = (a.len() + b.len()) as f64 - intersection;
            if union > 0.0 && intersection / union >= DUP_JACCARD {
                duplicated += 1;
            }
        }
    }
    if pairs == 0 {
        None
    } else {
        Some(duplicated as f64 / pairs as f64)
    }
}

/// importance（frontmatter）× 检索命中数（access-counts）的 Pearson 相关。
fn importance_calibration(memory_dir: &Path, project_state_dir: &Path) -> Option<f64> {
    let access = crate::access_counts::load_access_counts(project_state_dir);
    let Ok(read_dir) = std::fs::read_dir(memory_dir) else {
        return None;
    };
    let mut pairs: Vec<(f64, f64)> = Vec::new();
    for entry in read_dir.flatten().take(200) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".md") || name == "MEMORY.md" || name == "SESSION.md" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let importance = crate::importance_pressure::parse_importance(&content);
        if importance == 0 {
            continue;
        }
        // SE point id = `<scope>/<relative_path_no_ext>`；项目根 scope=private。
        let stem = name.trim_end_matches(".md");
        let hits = access.get(&format!("private/{stem}")).copied().unwrap_or(0);
        pairs.push((importance as f64, (hits as f64).ln_1p()));
    }
    if pairs.len() < CALIBRATION_MIN_SAMPLES {
        return None;
    }
    let n = pairs.len() as f64;
    let mean_x = pairs.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = pairs.iter().map(|(_, y)| y).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for (x, y) in &pairs {
        cov += (x - mean_x) * (y - mean_y);
        var_x += (x - mean_x).powi(2);
        var_y += (y - mean_y).powi(2);
    }
    if var_x <= f64::EPSILON || var_y <= f64::EPSILON {
        // 零方差（全同分/全同命中）时相关无定义 —— 返回 None 而非假 0。
        return None;
    }
    Some((cov / (var_x.sqrt() * var_y.sqrt())).clamp(-1.0, 1.0))
}

/// 计算一份适应度快照（纯读，不落盘）。
#[must_use]
pub fn compute_snapshot(
    memory_dir: &Path,
    project_state_dir: &Path,
    now_ms: u64,
) -> FitnessSnapshot {
    let stats = crate::search_stats::load_search_stats(project_state_dir);
    let (candidates, queued, expired) = aggregate_meta_review(memory_dir);
    let gate = crate::evolution::gate_stats::load_gate_stats(project_state_dir);

    let mut snapshot = FitnessSnapshot {
        computed_at_ms: now_ms,
        retrieval_hit_rate: if stats.searches_total >= HIT_RATE_MIN_SAMPLES {
            stats.hit_rate()
        } else {
            None
        },
        retrieval_samples: stats.searches_total,
        duplication_rate: sample_duplication_rate(memory_dir),
        survivor_efficiency: if candidates > 0 {
            Some((queued as f64 / candidates as f64).clamp(0.0, 1.0))
        } else {
            None
        },
        expired_rate: if candidates > 0 {
            Some((expired as f64 / candidates as f64).clamp(0.0, 1.0))
        } else {
            None
        },
        gate_waste_ratio: gate.waste_ratio(),
        importance_calibration: importance_calibration(memory_dir, project_state_dir),
        system_error_rate: gate.error_rate(),
        composite: None,
        metrics_present: Vec::new(),
        schema_version: FITNESS_SCHEMA_VERSION,
    };
    snapshot.compute_composite();
    snapshot
}

fn history_path(project_state_dir: &Path) -> std::path::PathBuf {
    super::evolution_dir(project_state_dir).join(FITNESS_HISTORY_FILENAME)
}

#[must_use]
pub fn load_history(project_state_dir: &Path) -> Vec<FitnessSnapshot> {
    let Ok(raw) = std::fs::read_to_string(history_path(project_state_dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 计算 + 追加历史（cap 52）。返回快照。fail-soft。
pub async fn compute_and_record(
    memory_dir: &Path,
    project_state_dir: &Path,
    now_ms: u64,
) -> FitnessSnapshot {
    let snapshot = compute_snapshot(memory_dir, project_state_dir, now_ms);
    let mut history = load_history(project_state_dir);
    history.push(snapshot.clone());
    if history.len() > FITNESS_HISTORY_CAP {
        let overflow = history.len() - FITNESS_HISTORY_CAP;
        history.drain(0..overflow);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&history) {
        if let Err(e) = atomic_write(&history_path(project_state_dir), &bytes).await {
            log::warn!("[evolution] fitness history write failed (fail-soft): {e}");
        }
    }
    snapshot
}

fn fmt_opt(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_string(), |v| format!("{v:.3}"))
}

/// 参与复合分的指标清单（空 = 一个都没有）。用语言中立的破折号，避免
/// 中文占位串漏进英文报告。
fn fmt_present(names: &[String]) -> String {
    if names.is_empty() {
        "—".to_string()
    } else {
        names.join(" · ")
    }
}

/// 8a 投影：把进化态势写成 report 类 markdown（固定文件名滚动覆盖），
/// 直接出现在现有「进化报告」tab —— 零协议改动。`extra_sections` 由
/// tuner/variants/proposals 追加（ledger 尾部、变体胜负、待审提案）。
pub async fn write_fitness_report(
    memory_dir: &Path,
    snapshot: &FitnessSnapshot,
    extra_sections: &str,
    language: MemoryOutputLanguage,
) {
    let reports_dir = memory_dir.join("reports");
    if let Err(e) = tokio::fs::create_dir_all(&reports_dir).await {
        log::warn!("[evolution] reports dir create failed (fail-soft): {e}");
        return;
    }
    let body = match language {
        MemoryOutputLanguage::Zh => format!(
            "---\ntype: report\nname: 进化引擎态势\nabstract: 记忆系统自我进化的适应度指标与参数试验台账（滚动更新）\n---\n\
             # 进化引擎态势\n\n\
             ## 适应度快照\n\n\
             | 指标 | 值 |\n|---|---|\n\
             | 检索命中率 | {hit}（样本 {samples}） |\n\
             | 产物重复率 | {dup} |\n\
             | 假设存活效率 | {survivor} |\n\
             | 假设过期率 | {expired} |\n\
             | 门浪费率 | {waste} |\n\
             | **做梦失败率（自驱 lane）** | **{error_rate}** |\n\
             | importance 校准（Pearson） | {calibration} |\n\
             | **复合适应度** | **{composite}** |\n\n\
             > “—” = 数据尚不足，该指标不进复合分；进化器只在复合分可算时才试验参数。\n\
             > 复合分需同时满足「≥{min_metrics} 项指标有值」与「权重覆盖率 ≥{min_coverage:.0}%」，\n\
             > 否则视为**度量不可信**而非满分。本次参与的指标：{present}。\n\
             {extra}",
            hit = fmt_opt(snapshot.retrieval_hit_rate),
            samples = snapshot.retrieval_samples,
            dup = fmt_opt(snapshot.duplication_rate),
            survivor = fmt_opt(snapshot.survivor_efficiency),
            expired = fmt_opt(snapshot.expired_rate),
            waste = fmt_opt(snapshot.gate_waste_ratio),
            error_rate = fmt_opt(snapshot.system_error_rate),
            calibration = fmt_opt(snapshot.importance_calibration),
            composite = fmt_opt(snapshot.composite),
            min_metrics = MIN_METRICS_PRESENT,
            min_coverage = MIN_WEIGHT_COVERAGE * 100.0,
            present = fmt_present(&snapshot.metrics_present),
            extra = extra_sections,
        ),
        MemoryOutputLanguage::En => format!(
            "---\ntype: report\nname: Evolution engine status\nabstract: Fitness metrics and parameter-trial ledger of memory self-evolution (rolling)\n---\n\
             # Evolution engine status\n\n\
             ## Fitness snapshot\n\n\
             | Metric | Value |\n|---|---|\n\
             | Retrieval hit rate | {hit} ({samples} samples) |\n\
             | Artifact duplication | {dup} |\n\
             | Hypothesis survival | {survivor} |\n\
             | Hypothesis expiry | {expired} |\n\
             | Gate waste | {waste} |\n\
             | **Dream failure rate (automatic lanes)** | **{error_rate}** |\n\
             | Importance calibration (Pearson) | {calibration} |\n\
             | **Composite fitness** | **{composite}** |\n\n\
             > \"—\" = not enough data yet; the tuner only runs trials once the composite is computable.\n\
             > The composite requires both \"≥{min_metrics} metrics present\" and \"weight coverage ≥{min_coverage:.0}%\";\n\
             > otherwise it reads as **untrustworthy measurement**, not a perfect score. Metrics in play: {present}.\n\
             {extra}",
            hit = fmt_opt(snapshot.retrieval_hit_rate),
            samples = snapshot.retrieval_samples,
            dup = fmt_opt(snapshot.duplication_rate),
            survivor = fmt_opt(snapshot.survivor_efficiency),
            expired = fmt_opt(snapshot.expired_rate),
            waste = fmt_opt(snapshot.gate_waste_ratio),
            error_rate = fmt_opt(snapshot.system_error_rate),
            calibration = fmt_opt(snapshot.importance_calibration),
            composite = fmt_opt(snapshot.composite),
            min_metrics = MIN_METRICS_PRESENT,
            min_coverage = MIN_WEIGHT_COVERAGE * 100.0,
            present = fmt_present(&snapshot.metrics_present),
            extra = extra_sections,
        ),
    };
    if let Err(e) = atomic_write(&reports_dir.join("evolution-fitness.md"), body.as_bytes()).await {
        log::warn!("[evolution] fitness report write failed (fail-soft): {e}");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_meta_review(memory_dir: &Path, lines: &[&str]) {
        let dir = memory_dir.join("imagination");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta-review.jsonl"), lines.join("\n")).unwrap();
    }

    #[tokio::test]
    async fn empty_project_yields_data_insufficient_snapshot() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let snapshot = compute_snapshot(&memory_dir, dir.path(), 1_000);
        assert_eq!(snapshot.retrieval_hit_rate, None);
        assert_eq!(snapshot.duplication_rate, None);
        assert_eq!(snapshot.survivor_efficiency, None);
        assert_eq!(snapshot.gate_waste_ratio, None);
        assert_eq!(snapshot.importance_calibration, None);
        assert_eq!(snapshot.system_error_rate, None);
        assert_eq!(snapshot.composite, None, "全缺失 = None，不造假 0");
        assert!(snapshot.metrics_present.is_empty());
    }

    #[tokio::test]
    async fn meta_review_and_stats_feed_composite() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        write_meta_review(
            &memory_dir,
            &[
                r#"{"ts_ms":1,"candidates":10,"queued_high":3,"queued_medium":2,"expired":1}"#,
                r#"{"ts_ms":2,"candidates":10,"queued_high":5,"queued_medium":0,"expired":3}"#,
            ],
        );
        for i in 0..6 {
            crate::search_stats::record_search(dir.path(), &format!("q{i}"), 1, i).await;
        }
        let lane = crate::evolution::gate_stats::LANE_TICK;
        crate::evolution::gate_stats::record_tick_outcome(dir.path(), lane, "dreamed", 1).await;
        crate::evolution::gate_stats::record_tick_outcome(dir.path(), lane, "corpus_empty", 1)
            .await;

        let snapshot = compute_snapshot(&memory_dir, dir.path(), 1_000);
        assert_eq!(snapshot.retrieval_hit_rate, Some(1.0));
        assert_eq!(snapshot.survivor_efficiency, Some(0.5), "10/20 存活");
        assert_eq!(snapshot.expired_rate, Some(0.2), "4/20 过期");
        // 新口径：waste = corpus_empty(1) ÷ 尝试过的 tick(1 dreamed + 1 waste)。
        assert_eq!(snapshot.gate_waste_ratio, Some(0.5));
        assert_eq!(snapshot.system_error_rate, Some(0.0), "1 次尝试 0 次失败");
        let composite = snapshot.composite.expect("四指标可得 → 复合分可算");
        // hit(.35×1.0) + survivor(.20×0.5) + (1-waste)(.10×0.5) + (1-error)(.35×1.0) ÷ 1.00
        assert!((composite - (0.35 + 0.10 + 0.05 + 0.35) / 1.00).abs() < 1e-9);
        assert_eq!(snapshot.schema_version, FITNESS_SCHEMA_VERSION);
        assert!(snapshot
            .metrics_present
            .contains(&"system_error_rate".to_string()));
    }

    /// R2-1 的核心回归：只剩一项指标、且它取到理想值时，**绝不能**产出
    /// 满分复合分（旧实现会给 1.0，而 1.0 让 tuner 的接受判据永真）。
    #[tokio::test]
    async fn single_ideal_metric_must_not_yield_a_perfect_composite() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        // 只喂一个 corpus_empty：唯一有值的指标是 gate_waste_ratio。
        crate::evolution::gate_stats::record_tick_outcome(
            dir.path(),
            crate::evolution::gate_stats::LANE_TICK,
            "corpus_empty",
            1,
        )
        .await;

        let snapshot = compute_snapshot(&memory_dir, dir.path(), 1_000);
        assert!(snapshot.gate_waste_ratio.is_some(), "该指标本身有值");
        assert_eq!(
            snapshot.composite, None,
            "指标数 1 < {MIN_METRICS_PRESENT} → 度量不可信，必须是 None 而不是 1.0"
        );
    }

    /// 权重覆盖率下限：指标数够了但全是小权重项，同样不出复合分。
    #[test]
    fn low_weight_coverage_blocks_composite_even_with_enough_metrics() {
        let mut snapshot = FitnessSnapshot {
            duplication_rate: Some(0.0),       // .20
            gate_waste_ratio: Some(0.0),       // .10
            importance_calibration: Some(1.0), // .15
            ..FitnessSnapshot::default()
        };
        snapshot.compute_composite();
        assert_eq!(snapshot.metrics_present.len(), 3, "指标数达标");
        // 0.45 / 1.35 = 0.333 < 0.5
        assert_eq!(snapshot.composite, None, "权重覆盖率不足 → None");
    }

    #[tokio::test]
    async fn duplication_rate_detects_near_identical_dreams() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        let dreams = memory_dir.join("dreams");
        std::fs::create_dir_all(&dreams).unwrap();
        let base = "---\ntype: insight\n---\n用户偏好暗色主题 交付前先跑完整测试 数据层在 Rust 业务在 TS 侧";
        std::fs::write(dreams.join("insight_a.md"), base).unwrap();
        std::fs::write(dreams.join("insight_b.md"), format!("{base}！")).unwrap();
        std::fs::write(
            dreams.join("insight_c.md"),
            "---\ntype: insight\n---\n浏览器扩展站点策略 可信 host 白名单 急停开关持久化 版本握手",
        )
        .unwrap();
        std::fs::write(
            dreams.join("insight_d.md"),
            "---\ntype: insight\n---\n定时守护跨平台互斥 socket lock 双锁真值表 PID 身份层防误杀",
        )
        .unwrap();

        let rate = sample_duplication_rate(&memory_dir).expect("4 文件足够采样");
        // 6 对里恰 1 对近重复。
        assert!((rate - 1.0 / 6.0).abs() < 1e-9, "rate={rate}");
    }

    #[tokio::test]
    async fn history_caps_and_report_written_zh() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        for i in 0..(FITNESS_HISTORY_CAP + 3) {
            compute_and_record(&memory_dir, dir.path(), i as u64).await;
        }
        let history = load_history(dir.path());
        assert_eq!(history.len(), FITNESS_HISTORY_CAP);
        assert_eq!(
            history.last().unwrap().computed_at_ms,
            (FITNESS_HISTORY_CAP + 2) as u64
        );

        let snapshot = compute_snapshot(&memory_dir, dir.path(), 9);
        write_fitness_report(
            &memory_dir,
            &snapshot,
            "\n## 附加段\n\n- x\n",
            MemoryOutputLanguage::Zh,
        )
        .await;
        let report =
            std::fs::read_to_string(memory_dir.join("reports").join("evolution-fitness.md"))
                .unwrap();
        assert!(report.contains("type: report"));
        assert!(report.contains("进化引擎态势"));
        assert!(report.contains("复合适应度"));
        assert!(report.contains("附加段"));
    }
}
