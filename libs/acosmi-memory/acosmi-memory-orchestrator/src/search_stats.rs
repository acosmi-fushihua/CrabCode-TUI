//! W-MEMORY-SELF-EVOLVE-DGM G1/8a (2026-07-16) — per-project 检索统计。
//!
//! `memory.search` 每次调用记一行统计：总次数、有结果次数、近期查询环形
//! 缓冲（cap 50）。落 `<project_state_dir>/.memory-rust-derived/
//! search-stats.json`。消费方：
//! - 8a 适应度：`searches_with_hits / searches_total` = 在线检索命中率
//!   （真实使用信号，非合成探针）；
//! - 8b 影子重放：`recent_queries` 是候选参数的词法重放语料。
//!
//! 派生层数据，fail-soft，损坏即重建。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::daily_log::rust_derived_root;

pub const SEARCH_STATS_FILENAME: &str = "search-stats.json";
pub const RECENT_QUERIES_CAP: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentQuery {
    pub query: String,
    pub at_ms: u64,
    pub hit_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchStats {
    #[serde(default)]
    pub searches_total: u64,
    #[serde(default)]
    pub searches_with_hits: u64,
    #[serde(default)]
    pub recent_queries: Vec<RecentQuery>,
}

impl SearchStats {
    /// 在线命中率（无样本 → None，8a 用 None 表达「数据不足」而非假 0）。
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        if self.searches_total == 0 {
            return None;
        }
        Some(self.searches_with_hits as f64 / self.searches_total as f64)
    }
}

fn stats_path(project_state_dir: &Path) -> std::path::PathBuf {
    rust_derived_root(project_state_dir).join(SEARCH_STATS_FILENAME)
}

#[must_use]
pub fn load_search_stats(project_state_dir: &Path) -> SearchStats {
    let Ok(raw) = std::fs::read_to_string(stats_path(project_state_dir)) else {
        return SearchStats::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 记录一次检索。空查询（浏览模式）不计入 —— 命中率只度量真实检索。
pub async fn record_search(project_state_dir: &Path, query: &str, hit_count: usize, now_ms: u64) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let mut stats = load_search_stats(project_state_dir);
    stats.searches_total = stats.searches_total.saturating_add(1);
    if hit_count > 0 {
        stats.searches_with_hits = stats.searches_with_hits.saturating_add(1);
    }
    stats.recent_queries.push(RecentQuery {
        query: query.to_string(),
        at_ms: now_ms,
        hit_count: hit_count as u64,
    });
    if stats.recent_queries.len() > RECENT_QUERIES_CAP {
        let overflow = stats.recent_queries.len() - RECENT_QUERIES_CAP;
        stats.recent_queries.drain(0..overflow);
    }
    let bytes = match serde_json::to_vec_pretty(&stats) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::warn!("[search-stats] serialize failed (fail-soft): {e}");
            return;
        }
    };
    if let Err(e) = atomic_write(&stats_path(project_state_dir), &bytes).await {
        log::warn!("[search-stats] write failed (fail-soft): {e}");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn records_hits_and_misses_and_hit_rate() {
        let dir = TempDir::new().unwrap();
        record_search(dir.path(), "rust async", 3, 1_000).await;
        record_search(dir.path(), "nothing here", 0, 2_000).await;

        let stats = load_search_stats(dir.path());
        assert_eq!(stats.searches_total, 2);
        assert_eq!(stats.searches_with_hits, 1);
        assert_eq!(stats.hit_rate(), Some(0.5));
        assert_eq!(stats.recent_queries.len(), 2);
        assert_eq!(stats.recent_queries[1].query, "nothing here");
    }

    #[tokio::test]
    async fn browse_mode_empty_query_is_not_counted() {
        let dir = TempDir::new().unwrap();
        record_search(dir.path(), "   ", 10, 1_000).await;
        let stats = load_search_stats(dir.path());
        assert_eq!(stats.searches_total, 0);
        assert_eq!(stats.hit_rate(), None, "无样本 = None，不是假 0");
    }

    #[tokio::test]
    async fn recent_queries_ring_buffer_caps() {
        let dir = TempDir::new().unwrap();
        for i in 0..(RECENT_QUERIES_CAP + 10) {
            record_search(dir.path(), &format!("q{i}"), 1, i as u64).await;
        }
        let stats = load_search_stats(dir.path());
        assert_eq!(stats.recent_queries.len(), RECENT_QUERIES_CAP);
        assert_eq!(stats.recent_queries[0].query, "q10", "最旧的被环形淘汰");
        assert_eq!(
            stats.searches_total,
            (RECENT_QUERIES_CAP + 10) as u64,
            "总计数不受环形缓冲影响"
        );
    }

    #[tokio::test]
    async fn corrupted_stats_rebuild_fail_soft() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(rust_derived_root(dir.path())).unwrap();
        std::fs::write(stats_path(dir.path()), "!!").unwrap();
        assert_eq!(load_search_stats(dir.path()), SearchStats::default());
        record_search(dir.path(), "q", 1, 1).await;
        assert_eq!(load_search_stats(dir.path()).searches_total, 1);
    }
}
