//! W-MEMORY-SYNERGY W6 (2026-07-16, 6c) — 重要性积分触发（Park et al.
//! Generative Agents 的 reflection 阈值机制照搬：新记忆写入时带 1-10 的
//! importance 打分，未固化积分累计超过阈值即让下一次周期做梦**提前**——
//! 纯时间节律（48h）对高活跃期反应迟钝，重要性积分是它的事件驱动对偶）。
//!
//! 数据形态：`<project_state_dir>/.memory-rust-derived/importance-accum.json`
//! `{"sum": <u64>, "updated_at_ms": <u64>}`。
//! 生产者 = Tier-2 提取（每条新记忆的 frontmatter `importance:` 求和）；
//! 消费者 = 周期做梦 gate（sum ≥ 阈值 → 时间门豁免，其余 gate 不放宽）；
//! 复位 = 做梦成功后归零。全程 fail-soft：积分链任何 IO 失败都不影响
//! 提取与做梦本身。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::daily_log::rust_derived_root;

/// 触发阈值（Park et al. 2023 的 reflection 阈值 150 照搬；importance 1-10，
/// 约等于 15-30 条中高价值记忆的密度）。
pub const IMPORTANCE_PRESSURE_THRESHOLD: u64 = 150;

const ACCUM_FILENAME: &str = "importance-accum.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportanceAccum {
    pub sum: u64,
    pub updated_at_ms: u64,
}

fn accum_path(project_state_dir: &Path) -> PathBuf {
    rust_derived_root(project_state_dir).join(ACCUM_FILENAME)
}

/// 读当前积分（缺失/损坏 = 0，fail-soft）。
#[must_use]
pub fn read_importance_accum(project_state_dir: &Path) -> u64 {
    let raw = match std::fs::read_to_string(accum_path(project_state_dir)) {
        Ok(raw) => raw,
        Err(_) => return 0,
    };
    serde_json::from_str::<ImportanceAccum>(&raw)
        .map(|accum| accum.sum)
        .unwrap_or(0)
}

/// 累加积分（saturating）。fail-soft：写失败仅 log。
pub async fn add_importance(project_state_dir: &Path, delta: u64, now_ms: u64) {
    if delta == 0 {
        return;
    }
    let sum = read_importance_accum(project_state_dir).saturating_add(delta);
    write_accum(project_state_dir, sum, now_ms).await;
}

/// 做梦成功后归零。
pub async fn reset_importance(project_state_dir: &Path, now_ms: u64) {
    write_accum(project_state_dir, 0, now_ms).await;
}

async fn write_accum(project_state_dir: &Path, sum: u64, now_ms: u64) {
    let path = accum_path(project_state_dir);
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            log::warn!("[importance] accum dir create failed (fail-soft): {e}");
            return;
        }
    }
    let accum = ImportanceAccum {
        sum,
        updated_at_ms: now_ms,
    };
    let Ok(raw) = serde_json::to_string(&accum) else {
        return;
    };
    if let Err(e) = atomic_write(&path, raw.as_bytes()).await {
        log::warn!("[importance] accum write failed (fail-soft): {e}");
    }
}

/// 从一段记忆 markdown（frontmatter + body）里解析 `importance:` 值
/// （1-10 之外 clamp；缺失/非数字 = 0，即不计分——旧记忆与手写记忆天然
/// 兼容）。判据是 frontmatter 行级前缀，不做完整 YAML 解析（与
/// `dedup_hash::extract_frontmatter` 同一务实档位）。
#[must_use]
pub fn parse_importance(markdown: &str) -> u64 {
    let mut in_frontmatter = false;
    for (index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim();
        if index == 0 {
            if trimmed == "---" {
                in_frontmatter = true;
                continue;
            }
            return 0;
        }
        if !in_frontmatter {
            return 0;
        }
        if trimmed == "---" {
            return 0;
        }
        if let Some(raw) = trimmed.strip_prefix("importance:") {
            return raw
                .trim()
                .parse::<u64>()
                // 显式 0 = 不计分（clamp(1,10) 会把 0 抬成 1，毁灭复核修正）。
                .map(|value| if value == 0 { 0 } else { value.clamp(1, 10) })
                .unwrap_or(0);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_importance_reads_frontmatter_and_clamps() {
        assert_eq!(
            parse_importance("---\ntype: user\nimportance: 7\n---\nbody"),
            7
        );
        assert_eq!(
            parse_importance("---\nimportance: 99\n---\nbody"),
            10,
            "clamp 到 1-10"
        );
        assert_eq!(parse_importance("---\ntype: user\n---\nbody"), 0);
        assert_eq!(parse_importance("no frontmatter"), 0);
        assert_eq!(parse_importance("---\nimportance: not-a-number\n---\nx"), 0);
        assert_eq!(
            parse_importance("---\nimportance: 0\n---\nx"),
            0,
            "显式 0 不计分（不得被 clamp 抬成 1）"
        );
        assert_eq!(
            parse_importance("---\n---\nimportance: 9"),
            0,
            "正文里的 importance 不计"
        );
    }

    #[tokio::test]
    async fn accum_roundtrip_add_and_reset() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_importance_accum(dir.path()), 0);
        add_importance(dir.path(), 7, 1_000).await;
        add_importance(dir.path(), 9, 2_000).await;
        assert_eq!(read_importance_accum(dir.path()), 16);
        add_importance(dir.path(), 0, 3_000).await;
        assert_eq!(read_importance_accum(dir.path()), 16, "0 delta 不落盘");
        reset_importance(dir.path(), 4_000).await;
        assert_eq!(read_importance_accum(dir.path()), 0);
    }

    #[test]
    fn corrupt_accum_reads_as_zero() {
        let dir = TempDir::new().unwrap();
        let path = accum_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_importance_accum(dir.path()), 0);
    }
}
