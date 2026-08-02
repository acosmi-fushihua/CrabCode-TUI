//! W-MEMORY-SELF-EVOLVE-DGM G3-f (2026-07-16) — 派生层孤儿临时文件回收。
//!
//! `atomic_write` 的 tmp 文件（`.{name}.tmp.{pid}.{n}`）只在「create 成功、
//! rename 前崩溃」的窗口内残留；长期积累会污染 `.memory-rust-derived/`。
//! 本 GC 只回收**确定安全**的目标：文件名含 `.tmp.` 且 mtime 早于
//! `DERIVED_TMP_MAX_AGE_MS`（7 天）的文件，扫描深度 ≤ 2（根 + evolution/
//! 等一级子目录）。用户可见的 markdown / 正常 json 永不触碰（grok-build
//! `storage::gc` 的「非空工作区永不动」保守哲学）。
//!
//! 节流：`last-derived-gc.json` 标记，两次回收至少间隔
//! `DERIVED_GC_MIN_INTERVAL_MS`（24h）。全程 fail-soft。

use std::path::Path;

use crate::atomic_write::atomic_write;
use crate::daily_log::rust_derived_root;

pub const DERIVED_GC_MARKER_FILENAME: &str = "last-derived-gc.json";
pub const DERIVED_TMP_MAX_AGE_MS: u64 = 7 * 24 * 3600 * 1000;
pub const DERIVED_GC_MIN_INTERVAL_MS: u64 = 24 * 3600 * 1000;
const GC_MAX_DEPTH: usize = 2;

fn marker_path(project_state_dir: &Path) -> std::path::PathBuf {
    rust_derived_root(project_state_dir).join(DERIVED_GC_MARKER_FILENAME)
}

fn read_last_gc_ms(project_state_dir: &Path) -> u64 {
    let Ok(raw) = std::fs::read_to_string(marker_path(project_state_dir)) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("last_gc_ms").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn sweep_dir(dir: &Path, now_ms: u64, depth: usize, removed: &mut usize) {
    if depth > GC_MAX_DEPTH {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            sweep_dir(&path, now_ms, depth + 1, removed);
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.contains(".tmp.") {
            continue;
        }
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) < DERIVED_TMP_MAX_AGE_MS {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                *removed += 1;
                log::info!("[derived-gc] removed orphan tmp: {}", path.display());
            }
            Err(e) => {
                log::warn!(
                    "[derived-gc] remove failed (fail-soft) {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// 2026-07-27 §24.4 —— 日志保留期。
///
/// 日志本身是**按 UTC 日分文件**的（`logs/YYYY/MM/YYYY-MM-DD.jsonl`），所以
/// 单个文件天然有界；无界的是**文件数** —— 此前整个模块没有任何保留期
/// 常量，日志目录只增不减。派生层数据，过期即可删（真源是 transcript）。
pub const DAILY_LOG_RETENTION_MS: u64 = 90 * 24 * 3600 * 1000;

/// 按保留期清理 `logs/YYYY/MM/*.jsonl`，并顺手摘掉清空后的月 / 年目录。
///
/// 单独成函数而不复用 `sweep_dir`：后者的判据是"文件名含 `.tmp.`"且深度
/// 上限 2，够不到 `logs/YYYY/MM/` 这一层，两种回收的判据也本就不同。
fn sweep_daily_logs(derived_root: &Path, now_ms: u64, removed: &mut usize) {
    let logs_root = derived_root.join("logs");
    let Ok(years) = std::fs::read_dir(&logs_root) else {
        return;
    };
    for year in years.flatten() {
        let year_path = year.path();
        if !year_path.is_dir() {
            continue;
        }
        let Ok(months) = std::fs::read_dir(&year_path) else {
            continue;
        };
        for month in months.flatten() {
            let month_path = month.path();
            if !month_path.is_dir() {
                continue;
            }
            let Ok(days) = std::fs::read_dir(&month_path) else {
                continue;
            };
            for day in days.flatten() {
                let path = day.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(mtime_ms) = file_mtime_ms(&path) else {
                    continue;
                };
                if now_ms.saturating_sub(mtime_ms) < DAILY_LOG_RETENTION_MS {
                    continue;
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        *removed += 1;
                        log::info!("[derived-gc] removed expired daily log: {}", path.display());
                    }
                    Err(e) => log::warn!(
                        "[derived-gc] daily-log remove failed (fail-soft) {}: {e}",
                        path.display()
                    ),
                }
            }
            // 空目录顺手摘掉；非空时 remove_dir 直接失败，无需先判断。
            let _ = std::fs::remove_dir(&month_path);
        }
        let _ = std::fs::remove_dir(&year_path);
    }
}

/// 回收一个项目派生层的孤儿 tmp 文件与过期日志。节流未到期 → 直接返回 0。
/// 返回删除的文件数。
pub async fn gc_derived_tmp_files(project_state_dir: &Path, now_ms: u64) -> usize {
    let last = read_last_gc_ms(project_state_dir);
    if now_ms.saturating_sub(last) < DERIVED_GC_MIN_INTERVAL_MS {
        return 0;
    }
    let root = rust_derived_root(project_state_dir);
    if !root.exists() {
        return 0;
    }
    let mut removed = 0usize;
    sweep_dir(&root, now_ms, 0, &mut removed);
    sweep_daily_logs(&root, now_ms, &mut removed);

    let marker = serde_json::json!({ "last_gc_ms": now_ms }).to_string();
    if let Err(e) = atomic_write(&marker_path(project_state_dir), marker.as_bytes()).await {
        log::warn!("[derived-gc] marker write failed (fail-soft): {e}");
    }
    removed
}

#[cfg(test)]
mod tests {
    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn write_aged(root: &Path, name: &str, age_ms: u64) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join(name);
        std::fs::write(&path, "x").unwrap();
        let seconds = ((NOW - age_ms) / 1000) as i64;
        set_file_mtime(&path, FileTime::from_unix_time(seconds, 0)).unwrap();
        path
    }

    #[tokio::test]
    async fn gc_removes_only_old_tmp_files_and_writes_marker() {
        let dir = TempDir::new().unwrap();
        let root = rust_derived_root(dir.path());
        let old_tmp = write_aged(
            &root,
            ".accum.json.tmp.123.4",
            DERIVED_TMP_MAX_AGE_MS + 1_000,
        );
        let fresh_tmp = write_aged(&root, ".fresh.json.tmp.123.5", 60_000);
        let regular = write_aged(&root, "dream-config.json", DERIVED_TMP_MAX_AGE_MS * 4);
        let nested_old = write_aged(
            &root.join("evolution"),
            ".ledger.jsonl.tmp.9.9",
            DERIVED_TMP_MAX_AGE_MS + 1_000,
        );

        let removed = gc_derived_tmp_files(dir.path(), NOW).await;

        assert_eq!(removed, 2, "只删过期 tmp（根 + 一级子目录）");
        assert!(!old_tmp.exists());
        assert!(!nested_old.exists());
        assert!(fresh_tmp.exists(), "未过期 tmp 保留");
        assert!(regular.exists(), "正常 json 永不触碰（哪怕很老）");
        assert!(marker_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn gc_throttles_via_marker() {
        let dir = TempDir::new().unwrap();
        let root = rust_derived_root(dir.path());
        write_aged(&root, ".a.tmp.1.1", DERIVED_TMP_MAX_AGE_MS + 1_000);

        assert_eq!(gc_derived_tmp_files(dir.path(), NOW).await, 1);

        // 再造一个过期 tmp：节流窗口内的第二次调用不动它。
        let second = write_aged(&root, ".b.tmp.1.2", DERIVED_TMP_MAX_AGE_MS + 1_000);
        assert_eq!(
            gc_derived_tmp_files(dir.path(), NOW + 60_000).await,
            0,
            "24h 节流窗口内直接返回"
        );
        assert!(second.exists());

        // 窗口过后回收。
        assert_eq!(
            gc_derived_tmp_files(dir.path(), NOW + DERIVED_GC_MIN_INTERVAL_MS + 1_000).await,
            1
        );
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn gc_missing_root_is_noop() {
        let dir = TempDir::new().unwrap();
        assert_eq!(gc_derived_tmp_files(dir.path(), NOW).await, 0);
        assert!(
            !marker_path(dir.path()).exists(),
            "无派生目录时不落 marker（避免副作用）"
        );
    }
}
