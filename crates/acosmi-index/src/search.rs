//! 内容搜索与文件名搜索
//!
//! 功能：
//! 1. 文件内容正则搜索（对标 ripgrep 核心功能）
//! 2. 文件名 glob 模式匹配
//! 3. 并行搜索（使用 rayon）
//! 4. 大文件流式处理（memmap2）
//!
//! 对标 TS 源码：
//! - `src/utils/ripgrep.ts` — 内容搜索（调用 rg 子进程）
//! - `src/utils/glob.ts` — 文件名 glob 匹配

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use tracing;

use crate::types::{ContentMatch, ContentSearchOptions, IndexError, IndexResult};

/// 大文件阈值：超过 1MB 使用 mmap
const MMAP_THRESHOLD: u64 = 1_048_576;
/// 单行最大长度（超长行截断，避免极端性能问题）
const MAX_LINE_LEN: usize = 8192;

/// 内容搜索引擎
pub struct ContentSearcher;

impl ContentSearcher {
    /// 在指定目录下搜索文件内容
    ///
    /// 使用 rayon 并行搜索。遵循 .gitignore 规则。
    /// 返回所有匹配行，按文件路径排序。
    pub fn search(root: &Path, options: &ContentSearchOptions) -> IndexResult<Vec<ContentMatch>> {
        if !root.exists() {
            return Err(IndexError::PathNotFound(root.to_path_buf()));
        }
        if options.pattern.is_empty() {
            return Ok(Vec::new());
        }

        // 编译正则
        let regex = build_regex(&options.pattern, options.case_sensitive, options.literal)?;

        // 构建 include/exclude glob
        let include_glob = if options.include_globs.is_empty() {
            None
        } else {
            Some(build_glob_set(&options.include_globs)?)
        };
        let exclude_glob = if options.exclude_globs.is_empty() {
            None
        } else {
            Some(build_glob_set(&options.exclude_globs)?)
        };

        // 收集要搜索的文件
        let files = collect_files(
            root,
            include_glob.as_ref(),
            exclude_glob.as_ref(),
            options.include_hidden,
        );

        let max_results = if options.max_results == 0 {
            usize::MAX
        } else {
            options.max_results
        };

        // 使用原子计数器实现全局结果上限
        // Design note: result_count and limit_reached use Ordering::Relaxed intentionally.
        // Parallel workers may briefly overshoot max_results, which is acceptable because
        // final_results.truncate(max_results) enforces the exact limit. The trade-off is
        // lower synchronization overhead in the hot parallel loop.
        let result_count = AtomicUsize::new(0);
        let limit_reached = AtomicBool::new(false);
        let results = Mutex::new(Vec::<ContentMatch>::new());

        files.par_iter().for_each(|file_path| {
            if limit_reached.load(Ordering::Relaxed) {
                return;
            }

            let matches = match search_file(file_path, &regex) {
                Ok(m) => m,
                Err(err) => {
                    tracing::debug!("search error for {:?}: {err}", file_path);
                    return;
                }
            };

            if matches.is_empty() {
                return;
            }

            let prev_count = result_count.fetch_add(matches.len(), Ordering::Relaxed);
            if prev_count >= max_results {
                limit_reached.store(true, Ordering::Relaxed);
                return;
            }

            let take = (max_results - prev_count).min(matches.len());
            if let Ok(mut locked) = results.lock() {
                locked.extend(matches.into_iter().take(take));
            }

            if prev_count + take >= max_results {
                limit_reached.store(true, Ordering::Relaxed);
            }
        });

        let mut final_results = results.into_inner().unwrap_or_default();
        // 按文件路径 + 行号排序
        final_results.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
        final_results.truncate(max_results);
        Ok(final_results)
    }

    /// 在单个文件中搜索
    pub fn search_file(
        file_path: &Path,
        pattern: &str,
        case_sensitive: bool,
    ) -> IndexResult<Vec<ContentMatch>> {
        let regex = build_regex(pattern, case_sensitive, false)?;
        search_file(file_path, &regex).map_err(IndexError::Io)
    }
}

/// 文件名搜索（glob 模式匹配）
pub struct FileNameSearcher;

impl FileNameSearcher {
    /// 使用 glob 模式在目录下搜索文件
    ///
    /// 对标 TS 端 `glob()` 函数（底层调 `rg --files --glob`）。
    pub fn search(
        root: &Path,
        pattern: &str,
        include_hidden: bool,
        limit: usize,
        offset: usize,
    ) -> IndexResult<(Vec<PathBuf>, bool)> {
        if !root.exists() {
            return Err(IndexError::PathNotFound(root.to_path_buf()));
        }

        let glob = Glob::new(pattern)?.compile_matcher();

        let mut all_matches: Vec<PathBuf> = Vec::new();

        let walker = WalkBuilder::new(root)
            .hidden(!include_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .sort_by_file_path(std::cmp::Ord::cmp)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.path() == root {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.is_dir() {
                continue;
            }

            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");

            if glob.is_match(&relative) {
                all_matches.push(entry.path().to_path_buf());
            }
        }

        let truncated = all_matches.len() > offset + limit;
        let result: Vec<PathBuf> = all_matches.into_iter().skip(offset).take(limit).collect();

        Ok((result, truncated))
    }

    /// 使用 glob 模式列表在目录下搜索文件
    pub fn search_multi(
        root: &Path,
        patterns: &[String],
        include_hidden: bool,
    ) -> IndexResult<Vec<PathBuf>> {
        if !root.exists() {
            return Err(IndexError::PathNotFound(root.to_path_buf()));
        }

        let glob_set = build_glob_set(patterns)?;

        let mut matches: Vec<PathBuf> = Vec::new();

        let walker = WalkBuilder::new(root)
            .hidden(!include_hidden)
            .git_ignore(true)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.path() == root {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.is_dir() {
                continue;
            }

            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");

            if glob_set.is_match(&relative) {
                matches.push(entry.path().to_path_buf());
            }
        }

        Ok(matches)
    }
}

// -- 内部函数 ----------------------------------------------------------------

/// 构建正则表达式
fn build_regex(pattern: &str, case_sensitive: bool, literal: bool) -> IndexResult<Regex> {
    let actual_pattern = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };

    let regex = RegexBuilder::new(&actual_pattern)
        .case_insensitive(!case_sensitive)
        .build()?;
    Ok(regex)
}

/// 构建 `GlobSet`
fn build_glob_set(patterns: &[String]) -> IndexResult<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let _ = builder.add(Glob::new(p)?);
    }
    Ok(builder.build()?)
}

/// 收集要搜索的文件列表
fn collect_files(
    root: &Path,
    include_glob: Option<&GlobSet>,
    exclude_glob: Option<&GlobSet>,
    include_hidden: bool,
) -> Vec<PathBuf> {
    let walker = WalkBuilder::new(root)
        .hidden(!include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut files = Vec::new();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.path() == root {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");

        // include 过滤
        if let Some(inc) = include_glob
            && !inc.is_match(&relative)
        {
            continue;
        }
        // exclude 过滤
        if let Some(exc) = exclude_glob
            && exc.is_match(&relative)
        {
            continue;
        }

        files.push(entry.path().to_path_buf());
    }

    files
}

/// 在单个文件中搜索匹配行
fn search_file(file_path: &Path, regex: &Regex) -> std::io::Result<Vec<ContentMatch>> {
    let metadata = fs::metadata(file_path)?;
    let file_size = metadata.len();

    if file_size == 0 {
        return Ok(Vec::new());
    }

    // 大文件使用 mmap
    if file_size >= MMAP_THRESHOLD {
        return search_file_mmap(file_path, regex);
    }

    // 小文件使用 BufReader
    search_file_buffered(file_path, regex)
}

/// Check if data looks like binary content (contains NUL byte in probe region).
/// `str::floor_char_boundary` shim — that std method requires Rust 1.91 but
/// Returns the largest valid char boundary `<= idx` without relying on newer
/// standard-library helpers.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        s.len()
    } else {
        let mut i = idx;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

fn is_likely_binary(data: &[u8]) -> bool {
    let probe_len = data.len().min(8192);
    data[..probe_len].contains(&0)
}

/// 使用 `BufReader` 搜索文件（小文件）
fn search_file_buffered(file_path: &Path, regex: &Regex) -> std::io::Result<Vec<ContentMatch>> {
    // Binary detection: read first 8192 bytes and check for NUL
    let mut probe_buf = [0u8; 8192];
    let probe_len = {
        use std::io::Read;
        let mut probe_file = std::fs::File::open(file_path)?;
        probe_file.read(&mut probe_buf)?
    };
    if is_likely_binary(&probe_buf[..probe_len]) {
        return Ok(Vec::new());
    }

    let file = fs::File::open(file_path)?;
    let reader = BufReader::new(file);
    let mut matches = Vec::new();

    for (line_idx, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue, // 跳过非 UTF-8 行
        };

        // 截断超长行（UTF-8 安全截断）
        let search_line = if line.len() > MAX_LINE_LEN {
            &line[..floor_char_boundary(&line, MAX_LINE_LEN)]
        } else {
            &line
        };

        if let Some(m) = regex.find(search_line) {
            // 先提取匹配区间，释放对 line 的借用，再 move line
            let match_start = m.start();
            let match_end = m.end();
            matches.push(ContentMatch {
                path: file_path.to_path_buf(),
                line_number: line_idx + 1,
                line_content: line,
                match_range: (match_start, match_end),
            });
        }
    }

    Ok(matches)
}

/// 使用 mmap 搜索文件（大文件 >1MB）
fn search_file_mmap(file_path: &Path, regex: &Regex) -> std::io::Result<Vec<ContentMatch>> {
    let file = fs::File::open(file_path)?;

    // SAFETY: The file is memory-mapped read-only for the duration of this search.
    // If another process truncates the file concurrently, the OS may deliver SIGBUS
    // (on Linux/macOS) when accessing pages beyond the new file size. This is an
    // inherent risk of mmap and cannot be prevented without file locking. In practice,
    // source files are rarely truncated during search. Callers that need stronger
    // guarantees should use the BufReader path (files < MMAP_THRESHOLD).
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(&file)? };

    // 检查是否可能是二进制文件（前 8192 字节含 NUL）
    if is_likely_binary(&mmap) {
        return Ok(Vec::new());
    }

    let content = match std::str::from_utf8(&mmap) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()), // 非 UTF-8 文件，跳过
    };

    let mut matches = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let search_line = if line.len() > MAX_LINE_LEN {
            &line[..floor_char_boundary(line, MAX_LINE_LEN)]
        } else {
            line
        };

        if let Some(m) = regex.find(search_line) {
            matches.push(ContentMatch {
                path: file_path.to_path_buf(),
                line_number: line_idx + 1,
                line_content: line.to_string(),
                match_range: (m.start(), m.end()),
            });
        }
    }

    Ok(matches)
}

// -- 单元测试 ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_search_tree() -> TempDir {
        let dir = TempDir::new().expect("create temp dir");
        let root = dir.path();

        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::create_dir_all(root.join("tests")).expect("mkdir");

        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"Hello, world!\");\n}\n",
        )
        .expect("write");

        fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
        )
        .expect("write");

        fs::write(
            root.join("tests/test_lib.rs"),
            "#[test]\nfn test_add() {\n    assert_eq!(add(1, 2), 3);\n}\n",
        )
        .expect("write");

        fs::write(root.join("README.md"), "# My Project\n\nA Rust project.\n").expect("write");
        fs::write(
            root.join("data.txt"),
            "line 1\nline 2\nfoo bar baz\nline 4\n",
        )
        .expect("write");

        dir
    }

    #[test]
    fn test_content_search_basic() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "fn main".to_string(),
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(!results.is_empty(), "should find 'fn main'");
        assert!(results[0].line_content.contains("fn main"));
        assert_eq!(results[0].line_number, 1);
    }

    #[test]
    fn test_content_search_regex() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: r"fn \w+\(a: i32".to_string(),
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(
            results.len() >= 2,
            "should find at least 2 function definitions"
        );
    }

    #[test]
    fn test_content_search_case_insensitive() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "hello".to_string(),
            case_sensitive: false,
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(!results.is_empty(), "case-insensitive should find 'Hello'");
    }

    #[test]
    fn test_content_search_literal() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "a + b".to_string(),
            literal: true,
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(!results.is_empty(), "literal search should find 'a + b'");
    }

    #[test]
    fn test_content_search_max_results() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "fn".to_string(),
            max_results: 2,
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(results.len() <= 2, "should respect max_results=2");
    }

    #[test]
    fn test_content_search_include_glob() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "fn".to_string(),
            include_globs: vec!["*.rs".to_string()],
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        for r in &results {
            let ext = r.path.extension().and_then(|e| e.to_str()).unwrap_or("");
            assert_eq!(ext, "rs", "should only search .rs files");
        }
    }

    #[test]
    fn test_content_search_exclude_glob() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "fn".to_string(),
            exclude_globs: vec!["tests/**".to_string()],
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        for r in &results {
            let path_str = r.path.to_string_lossy();
            assert!(
                !path_str.contains("tests"),
                "should exclude tests directory: {path_str}"
            );
        }
    }

    #[test]
    fn test_content_search_empty_pattern() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: String::new(),
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(results.is_empty(), "empty pattern should return no results");
    }

    #[test]
    fn test_content_search_no_match() {
        let dir = create_search_tree();
        let options = ContentSearchOptions {
            pattern: "zzzznonexistent".to_string(),
            ..Default::default()
        };

        let results = ContentSearcher::search(dir.path(), &options).expect("search");
        assert!(results.is_empty());
    }

    #[test]
    fn test_content_search_path_not_found() {
        let options = ContentSearchOptions {
            pattern: "test".to_string(),
            ..Default::default()
        };

        let result = ContentSearcher::search(Path::new("/nonexistent/path/for/test"), &options);
        assert!(result.is_err());
    }

    #[test]
    fn test_filename_search_glob() {
        let dir = create_search_tree();

        let (files, truncated) =
            FileNameSearcher::search(dir.path(), "*.rs", false, 100, 0).expect("search");
        assert!(!files.is_empty(), "should find .rs files");
        assert!(!truncated);
        for f in &files {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
            assert_eq!(ext, "rs");
        }
    }

    #[test]
    fn test_filename_search_recursive_glob() {
        let dir = create_search_tree();

        let (files, _truncated) =
            FileNameSearcher::search(dir.path(), "**/*.rs", false, 100, 0).expect("search");
        assert!(
            files.len() >= 3,
            "should find at least 3 .rs files recursively"
        );
    }

    #[test]
    fn test_filename_search_limit_and_offset() {
        let dir = create_search_tree();

        let (all_files, _) = FileNameSearcher::search(dir.path(), "*", false, 100, 0).expect("all");
        let total = all_files.len();

        if total >= 3 {
            let (limited, truncated) =
                FileNameSearcher::search(dir.path(), "*", false, 2, 0).expect("limited");
            assert_eq!(limited.len(), 2);
            assert!(truncated, "should be truncated with limit < total");

            let (offset_files, _) =
                FileNameSearcher::search(dir.path(), "*", false, 100, 1).expect("offset");
            assert_eq!(offset_files.len(), total - 1);
        }
    }

    #[test]
    fn test_filename_search_multi() {
        let dir = create_search_tree();

        let patterns = vec!["*.rs".to_string(), "*.md".to_string()];
        let files = FileNameSearcher::search_multi(dir.path(), &patterns, false).expect("multi");
        assert!(files.len() >= 4, "should find .rs and .md files");
    }

    #[test]
    fn test_search_single_file() {
        let dir = create_search_tree();

        let results = ContentSearcher::search_file(&dir.path().join("data.txt"), "foo bar", true)
            .expect("search file");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_number, 3);
        assert!(results[0].line_content.contains("foo bar baz"));
    }

    #[test]
    fn test_binary_file_detection() {
        let dir = TempDir::new().expect("temp");
        let bin_path = dir.path().join("binary.dat");
        // 写入含 NUL 的内容
        fs::write(&bin_path, b"hello\x00world\nfoo\n").expect("write");

        // 由于文件很小 (<1MB), 走 BufReader 路径
        // BufReader 按行读，NUL 不会被特殊处理但不应 panic
        let regex = Regex::new("foo").expect("regex");
        let results = search_file(&bin_path, &regex);
        // 不 panic 就好
        let _ = results;
    }
}
