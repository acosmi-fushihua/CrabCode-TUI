//! 8a — 做梦 tick 结果计数（门效率指标数据源）。
//!
//! `dream_one_project` 每次判定后记一笔：dreamed / errored / skipped(按
//! reason 分桶)。此前 gate skip 只广播给 TUI 不落盘，进化引擎无从度量
//! 「无效唤醒率」。派生层数据，fail-soft。
//!
//! 2026-07-27 三处根因修复（§21.2 / §21.4 / §19.1-7）：
//!
//! 1. **lane 维度**（R4-2）：做梦有三条产出 lane —— Rust 自驱周期 tick、
//!    手动 `memory.dream.run_now`、TS-line runner 回执 —— 而此前**只有第一
//!    条记账**，`dreamed` 系统性漏计成功（实测：某项目磁盘上有 2 份 insight
//!    产物，`dreamed` 却停在 0 而 `errored` 是 26）。现在三条都记，并按 lane
//!    分桶。**消费侧契约**：适应度只吃**自驱 lane**（tick + ts_runner），
//!    手动 lane 仅供诊断展示 —— 否则用户为了测试连点十次 `run_now` 就会
//!    直接扰动进化引擎的输入。
//! 2. **时间窗**（R4-4）：计数器此前只有 `saturating_add`，永不衰减 / 重置 /
//!    加窗，任何以它为分母的比率都会被历史无限稀释。现在按
//!    [`GATE_STATS_WINDOW_MS`] 滚动重置。
//! 3. **值域必须能表达"不健康"**（R4-4）：旧 `waste_ratio` 的分母是
//!    `total_ticks - errored` —— 装着**无界增长的正常节流**、又**主动把错误
//!    踢出分母**，两个动作都朝"让分数变好"的方向，于是系统正常时趋 0（满
//!    分）、系统全错时分母为 0（指标消失）——**任何输入下都产不出坏读数**。
//!    现在分母收敛为"真正尝试过的 tick"，并新增 [`GateStats::error_rate`]
//!    这个专门表达"系统是否在正常工作"的维度。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

pub const GATE_STATS_FILENAME: &str = "gate-stats.json";

/// 计数窗口（30 天）。超窗即滚动重置，使比率反映"最近的系统状态"而不是
/// "自安装以来的历史平均"。
pub const GATE_STATS_WINDOW_MS: u64 = 30 * 24 * 3_600 * 1_000;

/// Rust 自驱周期 tick —— 自动 lane，进适应度。
pub const LANE_TICK: &str = "tick";
/// TS-line runner 回执（`ResultListener` 的 `kind=="dream"` 成功分支）——
/// 自动 lane，进适应度。
pub const LANE_TS_RUNNER: &str = "ts_runner";
/// 手动 `memory.dream.run_now` —— **用户触发**，仅诊断，不进适应度。
pub const LANE_MANUAL: &str = "manual";

/// 单条 lane 的成败计数。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaneCounts {
    #[serde(default)]
    pub dreamed: u64,
    #[serde(default)]
    pub errored: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateStats {
    /// 全 lane 合计成功数（保留字段名与语义位置，兼容既有磁盘文件）。
    #[serde(default)]
    pub dreamed: u64,
    /// 全 lane 合计失败数。
    #[serde(default)]
    pub errored: u64,
    /// gate skip 计数，按 reason 分桶（`corpus_empty` / `time_gate_unmet` /
    /// `session_count_unmet` / `lock_held` / `disabled` / …）。
    #[serde(default)]
    pub skipped: BTreeMap<String, u64>,
    /// 按 lane 分桶的成败计数（R4-2）。既有文件无此键 → 空表，此时
    /// [`GateStats::automatic`] 回退到扁平计数（历史上扁平计数只由自驱
    /// tick 写入，回退语义正确）。
    #[serde(default)]
    pub by_lane: BTreeMap<String, LaneCounts>,
    /// 相解析失败次数（R3-4：此前只进日志、不进任何指标 —— 无据生成对
    /// 系统的自我认知完全不可见）。
    #[serde(default)]
    pub parse_failures: u64,
    /// 因证据集为空被跳过、未发起 Phase-3 的主题数（§19.1-6）。
    #[serde(default)]
    pub themes_skipped_no_evidence: u64,
    /// 当前计数窗口起点。`0` = 尚未标注（既有文件）→ 首次写入时采用当时
    /// 时刻为窗口起点，**不清空既有计数**（不静默丢历史数据）。
    #[serde(default)]
    pub window_started_at_ms: u64,
}

impl GateStats {
    #[must_use]
    pub fn total_ticks(&self) -> u64 {
        self.dreamed + self.errored + self.skipped.values().sum::<u64>()
    }

    /// 自动 lane（tick + ts_runner）的成败合计 —— **适应度的唯一口径**。
    /// 手动 lane 被排除：它是用户点出来的，不是系统健康的证据。
    #[must_use]
    pub fn automatic(&self) -> LaneCounts {
        if self.by_lane.is_empty() {
            // 既有文件：扁平计数历史上只由自驱 tick 写入。
            return LaneCounts {
                dreamed: self.dreamed,
                errored: self.errored,
            };
        }
        let mut out = LaneCounts::default();
        for (lane, counts) in &self.by_lane {
            if lane == LANE_MANUAL {
                continue;
            }
            out.dreamed = out.dreamed.saturating_add(counts.dreamed);
            out.errored = out.errored.saturating_add(counts.errored);
        }
        out
    }

    /// **系统是否在正常工作**（R3-2 / §19.1-7）。此前整套适应度里压根没有
    /// 这一维：一个每次做梦都失败的项目，其适应度与一个完全健康的项目无法
    /// 区分。0 = 全部成功，1 = 全部失败；无样本 → None。
    #[must_use]
    pub fn error_rate(&self) -> Option<f64> {
        let auto = self.automatic();
        let attempts = auto.dreamed + auto.errored;
        if attempts == 0 {
            return None;
        }
        Some(auto.errored as f64 / attempts as f64)
    }

    /// 无效唤醒率 = `corpus_empty` 跳过 ÷ **真正尝试过的 tick**
    /// （dreamed + errored + corpus_empty）。
    ///
    /// 时间门 / 会话数门跳过是**正常节流**，绝不能进分母（它们随运行时间
    /// 无界增长，会把任何比值稀释到 0）；错误**必须**留在分母里 —— 旧实现
    /// 把 errored 从分母中剔除，等价于声明"错误不算浪费"，也正是它在全错
    /// 场景下直接消失（分母 0 → None）的原因。无样本 → None。
    #[must_use]
    pub fn waste_ratio(&self) -> Option<f64> {
        let auto = self.automatic();
        let waste = self.skipped.get("corpus_empty").copied().unwrap_or(0);
        let attempted = auto.dreamed + auto.errored + waste;
        if attempted == 0 {
            return None;
        }
        Some(waste as f64 / attempted as f64)
    }
}

fn stats_path(project_state_dir: &Path) -> std::path::PathBuf {
    super::evolution_dir(project_state_dir).join(GATE_STATS_FILENAME)
}

#[must_use]
pub fn load_gate_stats(project_state_dir: &Path) -> GateStats {
    let Ok(raw) = std::fs::read_to_string(stats_path(project_state_dir)) else {
        return GateStats::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 窗口滚动：超过 [`GATE_STATS_WINDOW_MS`] 即清空计数并重新起窗。
/// `window_started_at_ms == 0`（既有文件）只补标注，不清空。
fn roll_window(stats: &mut GateStats, now_ms: u64) {
    if stats.window_started_at_ms == 0 {
        stats.window_started_at_ms = now_ms;
        return;
    }
    if now_ms.saturating_sub(stats.window_started_at_ms) > GATE_STATS_WINDOW_MS {
        *stats = GateStats {
            window_started_at_ms: now_ms,
            ..GateStats::default()
        };
    }
}

async fn save(project_state_dir: &Path, stats: &GateStats) {
    let Ok(bytes) = serde_json::to_vec_pretty(stats) else {
        return;
    };
    if let Err(e) = atomic_write(&stats_path(project_state_dir), &bytes).await {
        log::warn!("[evolution] gate-stats write failed (fail-soft): {e}");
    }
}

/// 记一笔 tick 结果。`outcome_kind` ∈ {"dreamed","errored"}，其余字符串按
/// skip reason 分桶。`lane` 见 [`LANE_TICK`] / [`LANE_TS_RUNNER`] /
/// [`LANE_MANUAL`]。
pub async fn record_tick_outcome(
    project_state_dir: &Path,
    lane: &str,
    outcome_kind: &str,
    now_ms: u64,
) {
    let mut stats = load_gate_stats(project_state_dir);
    roll_window(&mut stats, now_ms);
    match outcome_kind {
        "dreamed" => {
            stats.dreamed = stats.dreamed.saturating_add(1);
            let entry = stats.by_lane.entry(lane.to_string()).or_default();
            entry.dreamed = entry.dreamed.saturating_add(1);
        }
        "errored" => {
            stats.errored = stats.errored.saturating_add(1);
            let entry = stats.by_lane.entry(lane.to_string()).or_default();
            entry.errored = entry.errored.saturating_add(1);
        }
        reason => {
            let bucket = stats.skipped.entry(reason.to_string()).or_default();
            *bucket = bucket.saturating_add(1);
        }
    }
    save(project_state_dir, &stats).await;
}

/// 记一次相解析失败（R3-4：让它第一次进入指标体系）。
pub async fn record_parse_failure(project_state_dir: &Path, now_ms: u64) {
    let mut stats = load_gate_stats(project_state_dir);
    roll_window(&mut stats, now_ms);
    stats.parse_failures = stats.parse_failures.saturating_add(1);
    save(project_state_dir, &stats).await;
}

/// 记一次"证据为空 → 跳过主题"（§19.1-6）。
pub async fn record_theme_skipped_no_evidence(project_state_dir: &Path, now_ms: u64) {
    let mut stats = load_gate_stats(project_state_dir);
    roll_window(&mut stats, now_ms);
    stats.themes_skipped_no_evidence = stats.themes_skipped_no_evidence.saturating_add(1);
    save(project_state_dir, &stats).await;
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const T0: u64 = 1_700_000_000_000;

    #[tokio::test]
    async fn records_and_computes_waste_ratio() {
        let dir = TempDir::new().unwrap();
        record_tick_outcome(dir.path(), LANE_TICK, "dreamed", T0).await;
        record_tick_outcome(dir.path(), LANE_TICK, "corpus_empty", T0).await;
        record_tick_outcome(dir.path(), LANE_TICK, "time_gate_unmet", T0).await;
        record_tick_outcome(dir.path(), LANE_TICK, "corpus_empty", T0).await;
        record_tick_outcome(dir.path(), LANE_TICK, "errored", T0).await;

        let stats = load_gate_stats(dir.path());
        assert_eq!(stats.dreamed, 1);
        assert_eq!(stats.errored, 1);
        assert_eq!(stats.skipped["corpus_empty"], 2);
        assert_eq!(stats.total_ticks(), 5);
        // waste = 2 / (1 dreamed + 1 errored + 2 corpus_empty) = 0.5；
        // 时间门跳过不进分母，错误**留在**分母里。
        assert_eq!(stats.waste_ratio(), Some(0.5));
    }

    #[test]
    fn empty_stats_have_no_metrics() {
        let stats = GateStats::default();
        assert_eq!(stats.waste_ratio(), None);
        assert_eq!(stats.error_rate(), None);
    }

    /// R4-4 的核心回归：一个**每次都失败**的项目，指标必须能说"不健康"。
    /// 旧实现在这里 `denominator == 0` → `waste_ratio` 返回 None → 该指标从
    /// 复合分里消失 → 复合分由剩余指标重归一接管 → 依然不是低分。
    #[tokio::test]
    async fn all_errored_project_reports_unhealthy_not_missing() {
        let dir = TempDir::new().unwrap();
        for _ in 0..5 {
            record_tick_outcome(dir.path(), LANE_TICK, "errored", T0).await;
        }
        let stats = load_gate_stats(dir.path());
        assert_eq!(stats.error_rate(), Some(1.0), "全错必须读出 1.0，不是 None");
        assert_eq!(
            stats.waste_ratio(),
            Some(0.0),
            "分母含 errored，指标存在而非消失"
        );
    }

    /// R4-2：手动 lane 不得进适应度口径，自驱两条 lane 必须合并计入。
    #[tokio::test]
    async fn manual_lane_is_excluded_from_automatic_rate() {
        let dir = TempDir::new().unwrap();
        record_tick_outcome(dir.path(), LANE_TICK, "errored", T0).await;
        record_tick_outcome(dir.path(), LANE_TS_RUNNER, "dreamed", T0).await;
        // 用户连点 8 次 run_now 全成功 —— 不许把系统健康度洗白。
        for _ in 0..8 {
            record_tick_outcome(dir.path(), LANE_MANUAL, "dreamed", T0).await;
        }

        let stats = load_gate_stats(dir.path());
        assert_eq!(stats.dreamed, 9, "扁平计数是全 lane 合计（诊断用）");
        let auto = stats.automatic();
        assert_eq!(auto.dreamed, 1);
        assert_eq!(auto.errored, 1);
        assert_eq!(stats.error_rate(), Some(0.5), "手动 lane 不参与");
    }

    /// 既有磁盘文件（无 `by_lane`、无 `window_started_at_ms`）必须能读，
    /// 且首次写入只补窗口标注、不清空历史计数。
    #[tokio::test]
    async fn legacy_file_without_lane_or_window_is_adopted_not_wiped() {
        let dir = TempDir::new().unwrap();
        let path = stats_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"dreamed":1,"errored":26,"skipped":{"scan_throttled":6}}"#,
        )
        .unwrap();

        let legacy = load_gate_stats(dir.path());
        assert_eq!(legacy.automatic().errored, 26, "无 by_lane 时回退扁平计数");

        record_tick_outcome(dir.path(), LANE_TICK, "errored", T0).await;
        let rolled = load_gate_stats(dir.path());
        assert_eq!(rolled.errored, 27, "既有计数保留，不静默清零");
        assert_eq!(rolled.window_started_at_ms, T0, "窗口起点被补标注");
    }

    /// 超窗滚动重置：比率反映近况而不是自安装以来的历史平均。
    #[tokio::test]
    async fn counters_reset_after_window_expires() {
        let dir = TempDir::new().unwrap();
        for _ in 0..4 {
            record_tick_outcome(dir.path(), LANE_TICK, "errored", T0).await;
        }
        assert_eq!(load_gate_stats(dir.path()).errored, 4);

        let after = T0 + GATE_STATS_WINDOW_MS + 1;
        record_tick_outcome(dir.path(), LANE_TICK, "dreamed", after).await;

        let stats = load_gate_stats(dir.path());
        assert_eq!(stats.errored, 0, "跨窗清空");
        assert_eq!(stats.dreamed, 1);
        assert_eq!(stats.error_rate(), Some(0.0));
        assert_eq!(stats.window_started_at_ms, after);
    }

    #[tokio::test]
    async fn parse_failures_and_no_evidence_skips_are_counted() {
        let dir = TempDir::new().unwrap();
        record_parse_failure(dir.path(), T0).await;
        record_parse_failure(dir.path(), T0).await;
        record_theme_skipped_no_evidence(dir.path(), T0).await;

        let stats = load_gate_stats(dir.path());
        assert_eq!(stats.parse_failures, 2);
        assert_eq!(stats.themes_skipped_no_evidence, 1);
    }
}
