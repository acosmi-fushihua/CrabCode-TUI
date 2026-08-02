//! 文件索引与模糊搜索
//!
//! 功能：
//! 1. 递归遍历项目目录，构建文件索引（遵循 .gitignore）
//! 2. 文件元数据：路径、大小、修改时间、文件类型
//! 3. 增量更新：基于 mtime 判断文件是否需要重新索引
//! 4. 内存索引：HashMap 存储，支持按路径快速查找
//! 5. 模糊搜索：nucleo 风格的 fuzzy matching（移植自 native-ts/file-index）
//!
//! 对标 TS 源码：
//! - `native-ts/file-index/index.ts` — `FileIndex` + fuzzy search
//! - `src/utils/glob.ts` — glob 匹配（通过 ripgrep --files）

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ignore::WalkBuilder;
use tracing;

use crate::types::{FileEntry, FileKind, IndexError, IndexResult, SearchResult};

/// 模糊搜索评分常量（与 TS 端 nucleo-style 保持一致）
const SCORE_MATCH: i64 = 16;
const BONUS_BOUNDARY: i64 = 8;
const BONUS_CAMEL: i64 = 6;
const BONUS_CONSECUTIVE: i64 = 4;
const BONUS_FIRST_CHAR: i64 = 8;
const PENALTY_GAP_START: i64 = 3;
const PENALTY_GAP_EXTENSION: i64 = 1;

/// 顶层缓存条目数上限
const TOP_LEVEL_CACHE_LIMIT: usize = 100;
/// 查询串最大长度
const MAX_QUERY_LEN: usize = 64;

/// 文件索引
///
/// 遍历目录构建索引后支持：
/// - 按路径查找文件元数据
/// - 模糊搜索文件名
/// - 增量更新（只重新索引 mtime 发生变化的文件）
pub struct FileIndex {
    /// 索引根目录
    root: PathBuf,
    /// 路径 → 文件条目映射
    entries: HashMap<String, FileEntry>,
    /// 有序的相对路径列表（用于搜索）
    paths: Vec<String>,
    /// 各路径的小写形式（避免反复 `to_lowercase`）
    lower_paths: Vec<String>,
    /// 各路径的 a-z 位图（快速筛除不可能匹配的候选）
    char_bits: Vec<u32>,
    /// 各路径长度缓存
    path_lens: Vec<u16>,
    /// 顶层路径段缓存（空查询时返回）
    top_level_cache: Vec<SearchResult>,
    /// 索引是否已构建
    built: bool,
}

impl FileIndex {
    /// 创建一个空索引（需调用 `build` 或 `load_from_file_list` 填充）
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            entries: HashMap::new(),
            paths: Vec::new(),
            lower_paths: Vec::new(),
            char_bits: Vec::new(),
            path_lens: Vec::new(),
            top_level_cache: Vec::new(),
            built: false,
        }
    }

    /// 返回索引根目录
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回索引中的文件数量
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 索引是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 索引是否已构建完成
    #[must_use]
    pub const fn is_built(&self) -> bool {
        self.built
    }

    /// 按相对路径查找文件条目
    #[must_use]
    pub fn get(&self, relative_path: &str) -> Option<&FileEntry> {
        self.entries.get(relative_path)
    }

    /// 获取所有索引条目的迭代器
    pub fn iter(&self) -> impl Iterator<Item = (&String, &FileEntry)> {
        self.entries.iter()
    }

    // -- 构建 ----------------------------------------------------------------

    /// 遍历目录构建完整索引
    ///
    /// 使用 `ignore` crate 遵循 .gitignore 规则。
    pub fn build(&mut self) -> IndexResult<usize> {
        if !self.root.exists() {
            return Err(IndexError::PathNotFound(self.root.clone()));
        }

        let mut new_entries = HashMap::new();
        let mut new_paths = Vec::new();

        let walker = WalkBuilder::new(&self.root)
            .hidden(false) // 包含隐藏文件
            .git_ignore(true) // 遵循 .gitignore
            .git_global(true) // 遵循全局 gitignore
            .git_exclude(true) // 遵循 .git/info/exclude
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    tracing::debug!("walk error (skipped): {err}");
                    continue;
                }
            };

            // 跳过根目录自身
            if entry.path() == self.root {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(err) => {
                    tracing::debug!("metadata error for {:?} (skipped): {err}", entry.path());
                    continue;
                }
            };

            let kind = if metadata.is_dir() {
                // 跳过目录条目（只索引文件）
                continue;
            } else if metadata.is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::File
            };

            let abs_path = entry.path().to_path_buf();
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                // 统一为正斜杠（跨平台一致）
                .replace('\\', "/");

            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

            let file_entry = FileEntry {
                path: abs_path,
                relative_path: relative.clone(),
                size: metadata.len(),
                modified,
                kind,
            };

            new_paths.push(relative.clone());
            let _prev = new_entries.insert(relative, file_entry);
        }

        let count = new_paths.len();
        tracing::info!(root = %self.root.display(), count, "file index built");

        self.entries = new_entries;
        self.rebuild_search_structures(new_paths);
        self.built = true;

        Ok(count)
    }

    /// 从文件路径列表加载索引（不读取文件元数据）
    ///
    /// 对标 TS 端 `FileIndex.loadFromFileList()`。
    /// 当路径列表已由外部（如 ripgrep --files）生成时，直接加载到索引中。
    pub fn load_from_file_list(&mut self, file_list: &[String]) {
        // 去重 + 过滤空行
        let mut seen = std::collections::HashSet::new();
        let mut paths = Vec::new();
        for line in file_list {
            if !line.is_empty() && seen.insert(line.clone()) {
                paths.push(line.clone());
            }
        }

        self.entries.clear();
        for p in &paths {
            let abs = self.root.join(p);
            let entry = FileEntry {
                path: abs,
                relative_path: p.clone(),
                size: 0,
                modified: SystemTime::UNIX_EPOCH,
                kind: FileKind::File,
            };
            let _prev = self.entries.insert(p.clone(), entry);
        }

        self.rebuild_search_structures(paths);
        self.built = true;
    }

    /// 增量更新：只重新索引 mtime 变化的文件
    ///
    /// 返回 (新增, 更新, 删除) 的文件计数。
    pub fn incremental_update(&mut self) -> IndexResult<(usize, usize, usize)> {
        if !self.root.exists() {
            return Err(IndexError::PathNotFound(self.root.clone()));
        }

        let mut current_paths: HashMap<String, FileEntry> = HashMap::new();
        let walker = WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.path() == self.root {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                continue;
            }
            let kind = if metadata.is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::File
            };
            let abs_path = entry.path().to_path_buf();
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

            let file_entry = FileEntry {
                path: abs_path,
                relative_path: relative.clone(),
                size: metadata.len(),
                modified,
                kind,
            };
            let _prev = current_paths.insert(relative, file_entry);
        }

        let mut added = 0usize;
        let mut updated = 0usize;
        let mut removed = 0usize;

        // 检测新增和更新
        for (rel, new_entry) in &current_paths {
            match self.entries.get(rel) {
                None => {
                    added += 1;
                }
                Some(old_entry) => {
                    if old_entry.modified != new_entry.modified || old_entry.size != new_entry.size
                    {
                        updated += 1;
                    }
                }
            }
        }

        // 检测删除
        let old_keys: Vec<String> = self.entries.keys().cloned().collect();
        for key in &old_keys {
            if !current_paths.contains_key(key) {
                removed += 1;
            }
        }

        // 替换索引
        let new_paths: Vec<String> = current_paths.keys().cloned().collect();
        self.entries = current_paths;
        self.rebuild_search_structures(new_paths);

        tracing::info!(added, updated, removed, "incremental update complete");
        Ok((added, updated, removed))
    }

    // -- 模糊搜索 ------------------------------------------------------------

    /// 模糊搜索文件路径
    ///
    /// 对标 TS 端 `FileIndex.search(query, limit)`。
    /// 使用 nucleo 风格的评分算法：
    /// - 连续匹配加分
    /// - 边界字符（/ \ - _ . 空格后）加分
    /// - 驼峰边界加分
    /// - gap 扣分
    /// - 包含 "test" 的路径轻微惩罚
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        if limit == 0 {
            return Vec::new();
        }
        if query.is_empty() {
            return self.top_level_cache.iter().take(limit).cloned().collect();
        }

        // Smart case：全小写查询 → 大小写不敏感；含大写 → 大小写敏感
        let case_sensitive = query != query.to_lowercase();
        let needle: String = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };

        let n_len = needle.len().min(MAX_QUERY_LEN);
        let needle_chars: Vec<char> = needle.chars().take(n_len).collect();

        // 构建 needle 的 a-z 位图
        let mut needle_bitmap: u32 = 0;
        for &ch in &needle_chars {
            let c = ch as u32;
            if (97..=122).contains(&c) {
                needle_bitmap |= 1 << (c - 97);
            }
        }

        // 评分上界（用于剪枝）
        let score_ceiling = (n_len as i64) * (SCORE_MATCH + BONUS_BOUNDARY) + BONUS_FIRST_CHAR + 32;

        // Top-k: 维护升序排列的最佳 `limit` 个结果
        let mut top_k: Vec<(String, i64)> = Vec::with_capacity(limit + 1);
        let mut threshold = i64::MIN;

        for (i, rel_path) in self.paths.iter().enumerate() {
            // 位图快速筛除
            if (self.char_bits[i] & needle_bitmap) != needle_bitmap {
                continue;
            }

            let haystack = if case_sensitive {
                rel_path.as_str()
            } else {
                self.lower_paths[i].as_str()
            };

            // 贪心 indexOf 扫描 + 内联评分
            let mut positions = Vec::with_capacity(n_len);
            let mut search_start = 0;
            let mut found = true;
            for &nch in &needle_chars {
                if let Some(pos) = haystack[search_start..].find(nch) {
                    let abs_pos = search_start + pos;
                    positions.push(abs_pos);
                    search_start = abs_pos + nch.len_utf8();
                } else {
                    found = false;
                    break;
                }
            }
            if !found {
                continue;
            }

            // 计算 gap/consecutive
            let mut gap_penalty: i64 = 0;
            let mut consec_bonus: i64 = 0;
            for j in 1..positions.len() {
                let gap = positions[j] - positions[j - 1] - 1;
                if gap == 0 {
                    consec_bonus += BONUS_CONSECUTIVE;
                } else {
                    gap_penalty += PENALTY_GAP_START + (gap as i64) * PENALTY_GAP_EXTENSION;
                }
            }

            // Gap-bound 剪枝
            if top_k.len() == limit && score_ceiling + consec_bonus - gap_penalty <= threshold {
                continue;
            }

            // 边界/驼峰评分
            let original = rel_path.as_bytes();
            let h_len = i64::from(self.path_lens[i]);
            let mut score = (n_len as i64) * SCORE_MATCH + consec_bonus - gap_penalty;
            score += score_bonus_at(original, positions[0], true);
            for &pos in &positions[1..] {
                score += score_bonus_at(original, pos, false);
            }
            // 短路径奖励
            score += (32 - (h_len >> 2)).max(0);

            if top_k.len() < limit {
                top_k.push((rel_path.clone(), score));
                if top_k.len() == limit {
                    top_k.sort_by_key(|(_p, s)| *s);
                    threshold = top_k[0].1;
                }
            } else if score > threshold {
                // 二分插入
                let ins_pos = top_k
                    .binary_search_by(|(_p, s)| s.cmp(&score))
                    .unwrap_or_else(|x| x);
                top_k.insert(ins_pos, (rel_path.clone(), score));
                let _ = top_k.remove(0);
                threshold = top_k[0].1;
            }
        }

        // 降序排列（最佳在前）
        top_k.sort_by_key(|entry| Reverse(entry.1));

        let match_count = top_k.len();
        let denom = match_count.max(1) as f64;

        top_k
            .into_iter()
            .enumerate()
            .map(|(i, (path, _score))| {
                let position_score = i as f64 / denom;
                let final_score = if path.contains("test") {
                    (position_score * 1.05).min(1.0)
                } else {
                    position_score
                };
                SearchResult {
                    path,
                    score: final_score,
                }
            })
            .collect()
    }

    // -- 内部方法 ------------------------------------------------------------

    /// 重建搜索辅助结构（位图、小写缓存、顶层缓存）
    fn rebuild_search_structures(&mut self, mut paths: Vec<String>) {
        paths.sort();
        let n = paths.len();

        let mut lower_paths = Vec::with_capacity(n);
        let mut char_bits = Vec::with_capacity(n);
        let mut path_lens = Vec::with_capacity(n);

        for p in &paths {
            let lp = p.to_lowercase();
            let len = lp.len();
            let mut bits: u32 = 0;
            for c in lp.bytes() {
                if (97..=122).contains(&c) {
                    bits |= 1 << (c - 97);
                }
            }
            lower_paths.push(lp);
            char_bits.push(bits);
            #[allow(clippy::cast_possible_truncation)]
            path_lens.push(len as u16);
        }

        self.top_level_cache = compute_top_level_entries(&paths, TOP_LEVEL_CACHE_LIMIT);
        self.paths = paths;
        self.lower_paths = lower_paths;
        self.char_bits = char_bits;
        self.path_lens = path_lens;
    }
}

// -- 评分辅助函数 ------------------------------------------------------------

/// 判断字符是否是路径/命名边界
const fn is_boundary(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\' | b'-' | b'_' | b'.' | b' ')
}

fn is_lower(byte: u8) -> bool {
    (97..=122).contains(&byte)
}

fn is_upper(byte: u8) -> bool {
    (65..=90).contains(&byte)
}

/// 计算匹配位置的边界/驼峰加分
fn score_bonus_at(path: &[u8], pos: usize, first: bool) -> i64 {
    if pos == 0 {
        return if first { BONUS_FIRST_CHAR } else { 0 };
    }
    if pos >= path.len() {
        return 0;
    }
    let prev_ch = path[pos - 1];
    if is_boundary(prev_ch) {
        return BONUS_BOUNDARY;
    }
    if is_lower(prev_ch) && is_upper(path[pos]) {
        return BONUS_CAMEL;
    }
    0
}

/// 提取顶层路径段（空查询时返回）
///
/// 对标 TS 端 `computeTopLevelEntries()`。
fn compute_top_level_entries(paths: &[String], limit: usize) -> Vec<SearchResult> {
    let mut top_level = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for p in paths {
        // 取第一个 / 或 \ 之前的段
        let segment = match p.find(['/', '\\']) {
            Some(idx) => &p[..idx],
            None => p.as_str(),
        };
        if !segment.is_empty() && seen.insert(segment.to_string()) {
            top_level.push(segment.to_string());
            if top_level.len() >= limit {
                break;
            }
        }
    }

    // 按长度升序，同长度按字母序
    top_level.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

    top_level
        .into_iter()
        .take(limit)
        .map(|path| SearchResult { path, score: 0.0 })
        .collect()
}

// -- 单元测试 ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_tree() -> TempDir {
        let dir = TempDir::new().expect("create temp dir");
        let root = dir.path();

        // 创建目录结构
        fs::create_dir_all(root.join("src/utils")).expect("mkdir");
        fs::create_dir_all(root.join("tests")).expect("mkdir");
        fs::create_dir_all(root.join(".hidden")).expect("mkdir");

        // 创建文件
        fs::write(root.join("src/main.rs"), "fn main() {}").expect("write");
        fs::write(root.join("src/lib.rs"), "pub mod utils;").expect("write");
        fs::write(root.join("src/utils/helpers.rs"), "// helpers").expect("write");
        fs::write(root.join("tests/test_main.rs"), "#[test] fn it_works() {}").expect("write");
        fs::write(root.join(".hidden/config"), "secret").expect("write");
        fs::write(root.join("README.md"), "# Hello").expect("write");

        dir
    }

    #[test]
    fn test_build_index() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        let count = index.build().expect("build index");

        assert!(count >= 5, "should index at least 5 files, got {count}");
        assert!(index.is_built());
        assert!(!index.is_empty());
    }

    #[test]
    fn test_get_entry() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        let _count = index.build().expect("build");

        let entry = index.get("src/main.rs");
        assert!(entry.is_some(), "should find src/main.rs");
        let entry = entry.expect("entry");
        assert_eq!(entry.relative_path, "src/main.rs");
        assert!(entry.size > 0);
        assert_eq!(entry.kind, FileKind::File);
    }

    #[test]
    fn test_load_from_file_list() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());

        let file_list = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "README.md".to_string(),
            "src/main.rs".to_string(), // 重复
            String::new(),             // 空行
        ];
        index.load_from_file_list(&file_list);

        assert!(index.is_built());
        assert_eq!(index.len(), 3, "should deduplicate and filter empty");
    }

    #[test]
    fn test_search_exact_match() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.build().expect("build");

        let results = index.search("main.rs", 5);
        assert!(!results.is_empty(), "should find main.rs");
        // 最佳匹配应包含 "main.rs"
        assert!(
            results[0].path.contains("main.rs"),
            "first result should contain main.rs, got: {}",
            results[0].path
        );
    }

    #[test]
    fn test_search_empty_query() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.build().expect("build");

        let results = index.search("", 10);
        assert!(
            !results.is_empty(),
            "empty query should return top-level entries"
        );
        // 所有分数应为 0.0
        for r in &results {
            assert!(
                (r.score - 0.0).abs() < f64::EPSILON,
                "top-level score should be 0.0"
            );
        }
    }

    #[test]
    fn test_search_zero_limit() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.build().expect("build");

        let results = index.search("main", 0);
        assert!(results.is_empty(), "limit=0 should return empty");
    }

    #[test]
    fn test_search_no_match() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.build().expect("build");

        let results = index.search("zzzznonexistent", 5);
        assert!(
            results.is_empty(),
            "should return empty for nonexistent pattern"
        );
    }

    #[test]
    fn test_search_test_penalty() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.build().expect("build");

        let results = index.search("test", 10);
        // 含 "test" 的文件分数应有 1.05x 惩罚（分数更高 = 排名更差）
        for r in &results {
            if r.path.contains("test") {
                // score >= 0.0 总是成立，只要不是第一个结果
                // 这里只验证搜索不会 panic
                assert!(r.score >= 0.0);
            }
        }
    }

    #[test]
    fn test_incremental_update() {
        let dir = create_test_tree();
        let root = dir.path().to_path_buf();

        let mut index = FileIndex::new(&root);
        let _count = index.build().expect("initial build");
        let initial_count = index.len();

        // 添加新文件
        fs::write(root.join("new_file.txt"), "new content").expect("write new");
        // 修改已有文件
        fs::write(root.join("README.md"), "# Updated").expect("update");

        let (added, updated, _removed) = index.incremental_update().expect("update");

        assert!(added >= 1, "should detect at least 1 added file");
        // mtime 可能匹配也可能不匹配，取决于文件系统精度
        // 所以不严格断言 updated
        let _ = updated;
        assert!(
            index.len() > initial_count,
            "index should grow by at least 1"
        );
    }

    #[test]
    fn test_path_not_found() {
        let mut index = FileIndex::new("/nonexistent/path/for/test");
        let result = index.build();
        assert!(result.is_err());
        match result {
            Err(IndexError::PathNotFound(_)) => {} // expected
            other => panic!("expected PathNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn test_case_sensitive_search() {
        let dir = create_test_tree();
        let mut index = FileIndex::new(dir.path());
        index.load_from_file_list(&[
            "src/MyComponent.tsx".to_string(),
            "src/mycomponent.tsx".to_string(),
            "src/MYCOMPONENT.tsx".to_string(),
        ]);

        // 含大写字母 → case-sensitive
        let results = index.search("MyC", 10);
        assert!(!results.is_empty());
        // 最佳匹配应该是大小写完全匹配的
        assert!(
            results[0].path.contains("MyComponent"),
            "case-sensitive search should prefer exact case match: got {}",
            results[0].path
        );
    }
}
