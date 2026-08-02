//! 8c — prompt 变体档案（DGM「变体 + 谱系 + 经验选择」的合规形态）。
//!
//! 契约 #15-8 铁律下的实现方式：**变体本体全部是编译期 Rust 常量**（人审
//! 过才进包），进化只动「选谁」的统计权重。变体做成**基线 prompt 的追加
//! 指令块**（addendum）而非整版分叉——避免模板复制漂移，且效果可被产物
//! 存活/去重信号归因。
//!
//! 选择：UCB1（`mean + sqrt(2·ln(total)/n)`，未试变体乐观优先）——确定性
//! 可测，无 RNG 依赖。胜负信号按变体族分工：
//! - **phase3 做梦**：写前近重复被拦 → 当选变体记负（产出冗余）；产物存活
//!   ≥ `SURVIVOR_AGE_MS`（14 天未被 prune/去重删除）→ 记胜（fitness 周期
//!   `survivor_sweep`，`judged` 集合防重复记账）。
//! - **hypgen 想象**：创建时按 L5 verdict 记账（`record_verdicts`）——
//!   `ReviewQueueHigh` 记胜 / `Expired` 记负 / `Pending` 中性不记。想象**不**
//!   走磁盘存活判胜：expiry=存活窗口=14 天、Expired 假设不写盘（进
//!   `refuted/`）、review-queue 无到期清除，磁盘存活对想象是"写盘几乎全胜"
//!   的废信号；而 L1–L5 置信管线（含 L3 证据验证）在创建当刻已是完整判据。
//!
//! 拉黑：样本 ≥ 10 且胜率 < 0.2 的变体退出采样（tripwire 测试钉死）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;

pub const VARIANT_ARCHIVE_FILENAME: &str = "variant-archive.json";
pub const SURVIVOR_AGE_MS: u64 = 14 * 24 * 3_600_000;
pub const JUDGED_CAP: usize = 200;
pub const BLACKLIST_MIN_SAMPLES: u64 = 10;
pub const BLACKLIST_WIN_RATE: f64 = 0.2;

/// 一个编译期变体：id 全局唯一（`<kind>/<vN>`），parent 记谱系。
pub struct PromptVariant {
    pub id: &'static str,
    pub parent: Option<&'static str>,
    /// 追加到基线 prompt 末尾的指令块（v0 基线 = 空串）。
    pub addendum: &'static str,
}

/// tier3 做梦 phase3（整理成洞见）的变体族。
pub const PHASE3_VARIANTS: &[PromptVariant] = &[
    PromptVariant {
        id: "phase3/v0",
        parent: None,
        addendum: "",
    },
    PromptVariant {
        id: "phase3/v1",
        parent: Some("phase3/v0"),
        addendum: "\n\nAdditional bar for this run: only keep facts that would CHANGE a \
                   future session's behavior — decisions, constraints, corrections, \
                   preferences. Drop narration of what merely happened.",
    },
    PromptVariant {
        id: "phase3/v2",
        parent: Some("phase3/v0"),
        addendum: "\n\nAdditional bar for this run: prefer ONE dense consolidated insight \
                   over several overlapping ones — merge aggressively; if two candidate \
                   insights share a theme, emit only the stronger, enriched by the other.",
    },
];

/// tier3 想象 hypgen（假设生成）的变体族。
pub const HYPGEN_VARIANTS: &[PromptVariant] = &[
    PromptVariant {
        id: "hypgen/v0",
        parent: None,
        addendum: "",
    },
    PromptVariant {
        id: "hypgen/v1",
        parent: Some("hypgen/v0"),
        addendum: "\nStyle emphasis for this sweep: prioritize hypotheses that CONTRADICT \
                   or stress-test an existing memory — confirmation adds little; a \
                   surviving contradiction rewrites the map.",
    },
    PromptVariant {
        id: "hypgen/v2",
        parent: Some("hypgen/v0"),
        addendum: "\nStyle emphasis for this sweep: only propose hypotheses with a CONCRETE \
                   verification probe (a file to read, a pattern to grep, a behavior to \
                   observe) — unfalsifiable speculation is queue pollution.",
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VariantStats {
    #[serde(default)]
    pub wins: u64,
    #[serde(default)]
    pub losses: u64,
    #[serde(default)]
    pub last_used_ms: u64,
}

impl VariantStats {
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.wins + self.losses
    }
    #[must_use]
    pub fn win_rate(&self) -> f64 {
        let n = self.samples();
        if n == 0 {
            return 0.5; // 无样本 = 中性先验
        }
        self.wins as f64 / n as f64
    }
    #[must_use]
    pub fn blacklisted(&self) -> bool {
        self.samples() >= BLACKLIST_MIN_SAMPLES && self.win_rate() < BLACKLIST_WIN_RATE
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VariantArchive {
    #[serde(default)]
    pub stats: BTreeMap<String, VariantStats>,
    /// 已按「存活」记过账的产物文件名（防重复记胜）。
    #[serde(default)]
    pub judged: Vec<String>,
}

fn archive_path(project_state_dir: &Path) -> std::path::PathBuf {
    super::evolution_dir(project_state_dir).join(VARIANT_ARCHIVE_FILENAME)
}

#[must_use]
pub fn load_archive(project_state_dir: &Path) -> VariantArchive {
    let Ok(raw) = std::fs::read_to_string(archive_path(project_state_dir)) else {
        return VariantArchive::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub async fn save_archive(project_state_dir: &Path, archive: &VariantArchive) {
    let Ok(bytes) = serde_json::to_vec_pretty(archive) else {
        return;
    };
    if let Err(e) = atomic_write(&archive_path(project_state_dir), &bytes).await {
        log::warn!("[evolution] variant archive write failed (fail-soft): {e}");
    }
}

/// UCB1 选择：拉黑变体出局；全拉黑时回退 v0 基线（永不无路可走）。
#[must_use]
pub fn select_variant<'a>(
    family: &'a [PromptVariant],
    archive: &VariantArchive,
) -> &'a PromptVariant {
    let total: u64 = family
        .iter()
        .map(|v| archive.stats.get(v.id).map_or(0, VariantStats::samples))
        .sum();
    let eligible: Vec<&PromptVariant> = family
        .iter()
        .filter(|v| {
            !archive
                .stats
                .get(v.id)
                .is_some_and(VariantStats::blacklisted)
        })
        .collect();
    if eligible.is_empty() {
        return &family[0];
    }
    eligible
        .into_iter()
        .max_by(|a, b| {
            let score = |v: &PromptVariant| -> f64 {
                let stats = archive.stats.get(v.id).cloned().unwrap_or_default();
                let n = stats.samples();
                if n == 0 {
                    // 未试变体：乐观无穷（用大常数），保证每个变体先被试到。
                    return 1e9;
                }
                stats.win_rate() + (2.0 * ((total.max(1)) as f64).ln() / n as f64).sqrt()
            };
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap()
}

/// 记一笔胜/负。
pub async fn record_outcome(project_state_dir: &Path, variant_id: &str, win: bool, now_ms: u64) {
    let mut archive = load_archive(project_state_dir);
    let stats = archive.stats.entry(variant_id.to_string()).or_default();
    if win {
        stats.wins = stats.wins.saturating_add(1);
    } else {
        stats.losses = stats.losses.saturating_add(1);
    }
    stats.last_used_ms = now_ms;
    save_archive(project_state_dir, &archive).await;
}

/// 想象变体判据（创建时批量记账）：一个变体一轮生成 N 个候选，其 verdict
/// 聚合成 `(wins, losses)` **单次** RMW 落盘。用批量写（而非 N 次
/// `record_outcome`）把与 detached 想象任务 / 做梦 `survivor_sweep` 的
/// fail-soft 竞态窗口缩到最小；`variant_id` 由 family 前缀命名（`hypgen/*`）
/// 与 phase3 天然隔离，不会串键。两者全零 → no-op（不空写盘）。
pub async fn record_verdicts(
    project_state_dir: &Path,
    variant_id: &str,
    wins: u64,
    losses: u64,
    now_ms: u64,
) {
    if wins == 0 && losses == 0 {
        return;
    }
    let mut archive = load_archive(project_state_dir);
    let stats = archive.stats.entry(variant_id.to_string()).or_default();
    stats.wins = stats.wins.saturating_add(wins);
    stats.losses = stats.losses.saturating_add(losses);
    stats.last_used_ms = now_ms;
    save_archive(project_state_dir, &archive).await;
}

/// 在 frontmatter 块内插入一行（紧跟开头 `---` 之后）。无 frontmatter →
/// 原样返回（产物契约上必有 frontmatter；无就不强造）。
#[must_use]
pub fn inject_frontmatter_line(content: &str, key: &str, value: &str) -> String {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content.to_string();
    };
    format!("---\n{key}: {value}\n{rest}")
}

/// 从产物 frontmatter 解析 `prompt_variant:`。
#[must_use]
pub fn variant_of_content(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        if let Some(value) = line.strip_prefix("prompt_variant:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// fitness 周期的存活 sweep：dreams/ 里带 `prompt_variant` 且年龄 ≥ 14 天、
/// 尚未记账的产物 → 为其变体记一胜（文件还在 = 挺过了 prune/去重）。
pub async fn survivor_sweep(memory_dir: &Path, project_state_dir: &Path, now_ms: u64) {
    let dreams = memory_dir.join("dreams");
    let Ok(read_dir) = std::fs::read_dir(&dreams) else {
        return;
    };
    let mut archive = load_archive(project_state_dir);
    let mut changed = false;
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !name.ends_with(".md") || archive.judged.iter().any(|j| j == &name) {
            continue;
        }
        let Some(mtime_ms) = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
        else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) < SURVIVOR_AGE_MS {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(variant_id) = variant_of_content(&content) else {
            continue;
        };
        let stats = archive.stats.entry(variant_id).or_default();
        stats.wins = stats.wins.saturating_add(1);
        stats.last_used_ms = now_ms;
        archive.judged.push(name);
        changed = true;
    }
    if archive.judged.len() > JUDGED_CAP {
        let overflow = archive.judged.len() - JUDGED_CAP;
        archive.judged.drain(0..overflow);
    }
    if changed {
        save_archive(project_state_dir, &archive).await;
    }
}

/// 报告投影：变体胜负表。
#[must_use]
pub fn render_stats(project_state_dir: &Path) -> String {
    let archive = load_archive(project_state_dir);
    if archive.stats.is_empty() {
        return String::new();
    }
    archive
        .stats
        .iter()
        .map(|(id, stats)| {
            format!(
                "- `{id}`：{}/{}（胜率 {:.2}{}）",
                stats.wins,
                stats.samples(),
                stats.win_rate(),
                if stats.blacklisted() {
                    "，已拉黑"
                } else {
                    ""
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn variant_families_are_pinned() {
        // tripwire：变体 id 闭集 + v0 基线空 addendum + 谱系指向合法。
        let ids: Vec<&str> = PHASE3_VARIANTS.iter().map(|v| v.id).collect();
        assert_eq!(ids, ["phase3/v0", "phase3/v1", "phase3/v2"]);
        let ids: Vec<&str> = HYPGEN_VARIANTS.iter().map(|v| v.id).collect();
        assert_eq!(ids, ["hypgen/v0", "hypgen/v1", "hypgen/v2"]);
        for family in [PHASE3_VARIANTS, HYPGEN_VARIANTS] {
            assert!(family[0].addendum.is_empty(), "v0 必须是空基线");
            for variant in &family[1..] {
                assert_eq!(variant.parent, Some(family[0].id), "谱系指向基线");
                assert!(!variant.addendum.is_empty());
            }
        }
    }

    #[test]
    fn ucb1_tries_untried_first_then_exploits() {
        let mut archive = VariantArchive::default();
        archive.stats.insert(
            "phase3/v0".to_string(),
            VariantStats {
                wins: 8,
                losses: 2,
                last_used_ms: 1,
            },
        );
        // v1/v2 未试 → 乐观优先（按 max_by 平局取后者语义，只断言未试者当选）。
        let chosen = select_variant(PHASE3_VARIANTS, &archive);
        assert_ne!(chosen.id, "phase3/v0", "未试变体优先");

        // 全部有样本后：高胜率 + 探索项主导。
        archive.stats.insert(
            "phase3/v1".to_string(),
            VariantStats {
                wins: 1,
                losses: 9,
                last_used_ms: 1,
            },
        );
        archive.stats.insert(
            "phase3/v2".to_string(),
            VariantStats {
                wins: 5,
                losses: 5,
                last_used_ms: 1,
            },
        );
        let chosen = select_variant(PHASE3_VARIANTS, &archive);
        assert_eq!(chosen.id, "phase3/v0", "0.8 胜率主导");
    }

    #[test]
    fn blacklisted_variant_never_sampled_and_fallback_is_baseline() {
        let mut archive = VariantArchive::default();
        for id in ["phase3/v0", "phase3/v1", "phase3/v2"] {
            archive.stats.insert(
                id.to_string(),
                VariantStats {
                    wins: 1,
                    losses: 10,
                    last_used_ms: 1,
                },
            );
        }
        // 全拉黑（胜率 1/11 < 0.2, 样本 ≥ 10）→ 回退基线。
        assert!(archive.stats.values().all(VariantStats::blacklisted));
        assert_eq!(select_variant(PHASE3_VARIANTS, &archive).id, "phase3/v0");

        // 只拉黑 v1：v1 永不当选。
        let mut archive = VariantArchive::default();
        archive.stats.insert(
            "phase3/v1".to_string(),
            VariantStats {
                wins: 0,
                losses: 12,
                last_used_ms: 1,
            },
        );
        for _ in 0..50 {
            assert_ne!(select_variant(PHASE3_VARIANTS, &archive).id, "phase3/v1");
        }
    }

    #[test]
    fn frontmatter_inject_and_parse_round_trip() {
        let content = "---\ntype: insight\nconfidence: high\n---\n正文内容\n";
        let injected = inject_frontmatter_line(content, "prompt_variant", "phase3/v1");
        assert!(injected.starts_with("---\nprompt_variant: phase3/v1\ntype: insight"));
        assert_eq!(variant_of_content(&injected).as_deref(), Some("phase3/v1"));
        // 无 frontmatter → 原样。
        assert_eq!(
            inject_frontmatter_line("plain body", "prompt_variant", "x"),
            "plain body"
        );
        assert_eq!(variant_of_content("plain body"), None);
        // 去重逻辑兼容性：注入行只改 frontmatter，正文 hash 不变。
        assert_eq!(
            crate::dedup_hash::memory_body_hash(content),
            crate::dedup_hash::memory_body_hash(&injected),
            "变体归因行不得改变正文去重 hash"
        );
    }

    #[tokio::test]
    async fn survivor_sweep_wins_once_per_file() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        let dreams = memory_dir.join("dreams");
        std::fs::create_dir_all(&dreams).unwrap();
        let content = inject_frontmatter_line(
            "---\ntype: insight\n---\n存活的洞见\n",
            "prompt_variant",
            "phase3/v2",
        );
        let path = dreams.join("insight_old.md");
        std::fs::write(&path, &content).unwrap();
        // 拨老 mtime（> 14 天）。
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1_600_000_000, 0))
            .unwrap();
        let now = 1_600_000_000_000 + SURVIVOR_AGE_MS + 1_000;

        survivor_sweep(&memory_dir, dir.path(), now).await;
        survivor_sweep(&memory_dir, dir.path(), now).await;

        let archive = load_archive(dir.path());
        assert_eq!(archive.stats["phase3/v2"].wins, 1, "judged 集合防重复记胜");
        assert!(archive.judged.contains(&"insight_old.md".to_string()));
    }

    #[tokio::test]
    async fn record_outcome_accumulates() {
        let dir = TempDir::new().unwrap();
        record_outcome(dir.path(), "hypgen/v1", false, 10).await;
        record_outcome(dir.path(), "hypgen/v1", true, 20).await;
        let archive = load_archive(dir.path());
        assert_eq!(archive.stats["hypgen/v1"].wins, 1);
        assert_eq!(archive.stats["hypgen/v1"].losses, 1);
        assert_eq!(archive.stats["hypgen/v1"].last_used_ms, 20);
    }

    #[tokio::test]
    async fn record_verdicts_batches_wins_and_losses_and_noops_on_zero() {
        let dir = TempDir::new().unwrap();
        // 全零 = no-op：不建档、不空写盘。
        record_verdicts(dir.path(), "hypgen/v2", 0, 0, 5).await;
        assert!(
            load_archive(dir.path()).stats.is_empty(),
            "全零 verdict 不应落任何 stats"
        );
        // 一轮聚合：3 胜 + 1 负单次落盘。
        record_verdicts(dir.path(), "hypgen/v2", 3, 1, 42).await;
        // 再来一轮，验证 saturating 累加。
        record_verdicts(dir.path(), "hypgen/v2", 1, 0, 99).await;
        let archive = load_archive(dir.path());
        assert_eq!(archive.stats["hypgen/v2"].wins, 4);
        assert_eq!(archive.stats["hypgen/v2"].losses, 1);
        assert_eq!(archive.stats["hypgen/v2"].last_used_ms, 99);
    }

    #[test]
    fn select_over_hypgen_family_stays_in_family() {
        // 空档案 → 未试变体乐观优先，但必须落在 hypgen 族内（不串到 phase3）。
        let archive = VariantArchive::default();
        let chosen = select_variant(HYPGEN_VARIANTS, &archive);
        assert!(
            chosen.id.starts_with("hypgen/"),
            "hypgen 族选择必须返回 hypgen/* 变体，实得 {}",
            chosen.id
        );
    }
}
