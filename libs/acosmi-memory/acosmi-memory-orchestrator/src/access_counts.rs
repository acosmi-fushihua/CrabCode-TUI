//! W-MEMORY-SELF-EVOLVE-DGM G1 (2026-07-16) — per-project 检索命中计数。
//!
//! `memory.search` 每次返回的 top-k 结果对对应 SE point id 计数 +1，落
//! `<project_state_dir>/.memory-rust-derived/access-counts.json`。策略层
//! （`search_policy::apply_scope_policy`）读取计数做 access_boost（使用
//! 频率强化 —— grok-build access_count 思想的净室实现）；8a 适应度用它
//! 度量 importance 校准（高 importance 记忆是否真的高频被检索）。
//!
//! 派生层数据：可清可重建，丢失只损失排序微调，绝不 fail-hard。条目
//! 上限 `ACCESS_COUNTS_MAX_ENTRIES`，超限按 `last_ms` 最旧驱逐（LRU）。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::daily_log::rust_derived_root;

pub const ACCESS_COUNTS_FILENAME: &str = "access-counts.json";
pub const ACCESS_COUNTS_MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct AccessEntry {
    /// 累计命中次数。
    n: u64,
    /// 最近一次命中（unix ms）—— LRU 驱逐键。
    last_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccessCountsFile {
    #[serde(default)]
    counts: HashMap<String, AccessEntry>,
}

fn counts_path(project_state_dir: &Path) -> std::path::PathBuf {
    rust_derived_root(project_state_dir).join(ACCESS_COUNTS_FILENAME)
}

fn load_file(project_state_dir: &Path) -> AccessCountsFile {
    let path = counts_path(project_state_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return AccessCountsFile::default();
    };
    // 损坏文件 = 派生数据，静默重建（fail-soft）。
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 策略层视图：id → 累计命中次数。
#[must_use]
pub fn load_access_counts(project_state_dir: &Path) -> HashMap<String, u64> {
    load_file(project_state_dir)
        .counts
        .into_iter()
        .map(|(id, entry)| (id, entry.n))
        .collect()
}

/// 对本次返回结果的 id 集计数 +1 并落盘。fail-soft：任何 IO/序列化错误
/// 只 warn，不影响检索响应。
pub async fn record_access(project_state_dir: &Path, ids: &[String], now_ms: u64) {
    if ids.is_empty() {
        return;
    }
    let mut file = load_file(project_state_dir);
    for id in ids {
        let entry = file.counts.entry(id.clone()).or_default();
        entry.n = entry.n.saturating_add(1);
        entry.last_ms = now_ms;
    }
    if file.counts.len() > ACCESS_COUNTS_MAX_ENTRIES {
        let mut by_recency: Vec<(String, u64)> = file
            .counts
            .iter()
            .map(|(id, entry)| (id.clone(), entry.last_ms))
            .collect();
        by_recency.sort_by_key(|(_, last_ms)| *last_ms);
        let evict = file.counts.len() - ACCESS_COUNTS_MAX_ENTRIES;
        for (id, _) in by_recency.into_iter().take(evict) {
            file.counts.remove(&id);
        }
    }
    let bytes = match serde_json::to_vec_pretty(&file) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::warn!("[access-counts] serialize failed (fail-soft): {e}");
            return;
        }
    };
    if let Err(e) = atomic_write(&counts_path(project_state_dir), &bytes).await {
        log::warn!("[access-counts] write failed (fail-soft): {e}");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn record_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        record_access(
            dir.path(),
            &["private/a".to_string(), "global/b".to_string()],
            1_000,
        )
        .await;
        record_access(dir.path(), &["private/a".to_string()], 2_000).await;

        let counts = load_access_counts(dir.path());
        assert_eq!(counts["private/a"], 2);
        assert_eq!(counts["global/b"], 1);
    }

    #[tokio::test]
    async fn empty_ids_write_nothing() {
        let dir = TempDir::new().unwrap();
        record_access(dir.path(), &[], 1_000).await;
        assert!(!counts_path(dir.path()).exists());
        assert!(load_access_counts(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn corrupted_file_resets_fail_soft() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(rust_derived_root(dir.path())).unwrap();
        std::fs::write(counts_path(dir.path()), "{not json").unwrap();
        assert!(load_access_counts(dir.path()).is_empty());
        record_access(dir.path(), &["x".to_string()], 5).await;
        assert_eq!(load_access_counts(dir.path())["x"], 1);
    }

    #[tokio::test]
    async fn lru_eviction_keeps_most_recent_entries() {
        let dir = TempDir::new().unwrap();
        // 直接种一个超限文件（避免 2000 次真实写放大测试时长）。
        let mut file = AccessCountsFile::default();
        for i in 0..ACCESS_COUNTS_MAX_ENTRIES {
            file.counts.insert(
                format!("id-{i}"),
                AccessEntry {
                    n: 1,
                    last_ms: i as u64,
                },
            );
        }
        std::fs::create_dir_all(rust_derived_root(dir.path())).unwrap();
        std::fs::write(counts_path(dir.path()), serde_json::to_vec(&file).unwrap()).unwrap();

        record_access(dir.path(), &["newcomer".to_string()], u64::MAX).await;

        let counts = load_access_counts(dir.path());
        assert_eq!(counts.len(), ACCESS_COUNTS_MAX_ENTRIES);
        assert_eq!(counts["newcomer"], 1, "新条目保留");
        assert!(
            !counts.contains_key("id-0"),
            "last_ms 最旧的条目被 LRU 驱逐"
        );
        assert!(counts.contains_key(&format!("id-{}", ACCESS_COUNTS_MAX_ENTRIES - 1)));
    }
}
