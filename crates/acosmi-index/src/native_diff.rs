//! Native diff 实现
//!
//! 功能：
//! 1. 行级别文本 diff（使用 `similar` crate 的 Myers 算法）
//! 2. Unified diff 格式输出
//! 3. 大文件（>1MB）流式处理
//! 4. 文件对比（从文件路径读取内容再 diff）
//!
//! 对标 TS 源码：
//! - `src/utils/gitDiff.ts` — parseGitDiff / parseGitNumstat
//! - `native-ts/color-diff/index.ts` — ColorDiff（语法着色 diff）

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use similar::{Algorithm, ChangeTag as SimilarTag, TextDiff};

use crate::types::{ChangeTag, DiffChange, DiffHunk, DiffResult, IndexResult};

/// 默认上下文行数（unified diff 中 @@ 前后保留的行数）
const DEFAULT_CONTEXT_LINES: usize = 3;
/// 大文件阈值（超过此大小使用流式读取）
const LARGE_FILE_THRESHOLD: u64 = 1_048_576; // 1MB
/// 单个 diff 最大处理行数（防止极大文件耗尽内存）
const MAX_DIFF_LINES: usize = 100_000;

/// Native diff 引擎
pub struct NativeDiff;

impl NativeDiff {
    /// 对两段文本进行 diff，返回结构化结果
    ///
    /// # 参数
    /// - `old_text`: 旧文本（删除侧）
    /// - `new_text`: 新文本（添加侧）
    /// - `old_label`: 旧文件标签（用于 unified diff 头部）
    /// - `new_label`: 新文件标签
    /// - `context_lines`: 上下文行数（默认 3）
    #[must_use]
    pub fn diff_text(
        old_text: &str,
        new_text: &str,
        old_label: &str,
        new_label: &str,
        context_lines: Option<usize>,
    ) -> DiffResult {
        let context = context_lines.unwrap_or(DEFAULT_CONTEXT_LINES);

        let text_diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(old_text, new_text);

        let mut hunks = Vec::new();
        let mut additions: usize = 0;
        let mut deletions: usize = 0;

        // 使用 similar 的 unified diff 分组
        for group in text_diff.grouped_ops(context) {
            let mut hunk_lines = Vec::new();
            let mut old_start = 0;
            let mut old_count = 0;
            let mut new_start = 0;
            let mut new_count = 0;
            let mut first = true;

            for op in &group {
                if first {
                    old_start = op.old_range().start + 1; // 转为 1-based
                    new_start = op.new_range().start + 1;
                    first = false;
                }

                for change in text_diff.iter_changes(op) {
                    match change.tag() {
                        SimilarTag::Equal => {
                            hunk_lines.push(format!(" {}", change.value().trim_end_matches('\n')));
                            old_count += 1;
                            new_count += 1;
                        }
                        SimilarTag::Insert => {
                            hunk_lines.push(format!("+{}", change.value().trim_end_matches('\n')));
                            new_count += 1;
                            additions += 1;
                        }
                        SimilarTag::Delete => {
                            hunk_lines.push(format!("-{}", change.value().trim_end_matches('\n')));
                            old_count += 1;
                            deletions += 1;
                        }
                    }
                }
            }

            hunks.push(DiffHunk {
                old_start,
                old_lines: old_count,
                new_start,
                new_lines: new_count,
                lines: hunk_lines,
            });
        }

        DiffResult {
            old_path: old_label.to_string(),
            new_path: new_label.to_string(),
            hunks,
            additions,
            deletions,
        }
    }

    /// 对两个文件进行 diff
    ///
    /// 自动处理大文件：超过 1MB 使用流式读取。
    pub fn diff_files(
        old_path: &Path,
        new_path: &Path,
        context_lines: Option<usize>,
    ) -> IndexResult<DiffResult> {
        let old_text = read_file_for_diff(old_path)?;
        let new_text = read_file_for_diff(new_path)?;

        let old_label = old_path.to_string_lossy();
        let new_label = new_path.to_string_lossy();

        Ok(Self::diff_text(
            &old_text,
            &new_text,
            &old_label,
            &new_label,
            context_lines,
        ))
    }

    /// 生成 unified diff 格式的字符串输出
    ///
    /// 格式与 `git diff` 输出兼容。
    #[must_use]
    pub fn format_unified(result: &DiffResult) -> String {
        if result.hunks.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        output.push_str(&format!("--- {}\n", result.old_path));
        output.push_str(&format!("+++ {}\n", result.new_path));

        for hunk in &result.hunks {
            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
            ));
            for line in &hunk.lines {
                output.push_str(line);
                output.push('\n');
            }
        }

        output
    }

    /// 获取变更列表（不按 hunk 分组）
    ///
    /// 返回每行的变更类型，适合需要逐行处理的场景。
    #[must_use]
    pub fn diff_changes(old_text: &str, new_text: &str) -> Vec<DiffChange> {
        let text_diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(old_text, new_text);

        text_diff
            .iter_all_changes()
            .map(|change| {
                let tag = match change.tag() {
                    SimilarTag::Equal => ChangeTag::Equal,
                    SimilarTag::Insert => ChangeTag::Insert,
                    SimilarTag::Delete => ChangeTag::Delete,
                };
                DiffChange {
                    tag,
                    value: change.value().to_string(),
                }
            })
            .collect()
    }

    /// 计算 diff 统计信息（添加/删除行数）
    #[must_use]
    pub fn diff_stats(old_text: &str, new_text: &str) -> (usize, usize) {
        let text_diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(old_text, new_text);

        let mut additions = 0usize;
        let mut deletions = 0usize;

        for change in text_diff.iter_all_changes() {
            match change.tag() {
                SimilarTag::Insert => additions += 1,
                SimilarTag::Delete => deletions += 1,
                SimilarTag::Equal => {}
            }
        }

        (additions, deletions)
    }

    /// 解析 unified diff 字符串为结构化结果
    ///
    /// 对标 TS 端 `parseGitDiff()`。
    #[must_use]
    pub fn parse_unified_diff(diff_text: &str) -> Vec<DiffResult> {
        let mut results = Vec::new();

        // 按 "diff --git " 分割（如果是 git diff 格式）
        // 否则按 "--- " 分割（标准 unified diff）
        if diff_text.contains("diff --git ") {
            let file_diffs: Vec<&str> = diff_text
                .split("diff --git ")
                .filter(|s| !s.is_empty())
                .collect();

            for file_diff in file_diffs {
                if let Some(result) = parse_single_git_diff(file_diff) {
                    results.push(result);
                }
            }
        } else {
            // 单文件 unified diff
            if let Some(result) = parse_single_unified_diff(diff_text) {
                results.push(result);
            }
        }

        results
    }
}

// -- 内部函数 ----------------------------------------------------------------

/// 读取文件内容用于 diff
///
/// 大文件（>1MB）使用流式读取并截断到 `MAX_DIFF_LINES` 行。
fn read_file_for_diff(path: &Path) -> IndexResult<String> {
    if !path.exists() {
        // 文件不存在视为空文件（新增文件场景）
        return Ok(String::new());
    }

    let metadata = fs::metadata(path)?;

    if metadata.len() > LARGE_FILE_THRESHOLD {
        // 大文件：流式读取，限制行数
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut total_bytes = 0usize;

        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => break, // 非 UTF-8，停止
            };
            total_bytes += line.len() + 1;
            lines.push(line);

            if lines.len() >= MAX_DIFF_LINES {
                tracing::warn!(
                    path = %path.display(),
                    lines = lines.len(),
                    bytes = total_bytes,
                    "truncated large file for diff"
                );
                break;
            }
        }

        Ok(lines.join("\n"))
    } else {
        // 正常大小文件：直接读取
        let content = fs::read_to_string(path)?;
        Ok(content)
    }
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^a/(.+?) b/(.+)$").expect("header regex"))
}

fn hunk_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("hunk regex")
    })
}

/// 解析单个 git diff 格式的文件 diff
fn parse_single_git_diff(diff_text: &str) -> Option<DiffResult> {
    let lines: Vec<&str> = diff_text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // 从 "a/path b/path" 头部提取文件路径
    let caps = header_re().captures(lines[0])?;
    let old_path = caps.get(1)?.as_str().to_string();
    let new_path = caps.get(2)?.as_str().to_string();

    let mut hunks = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut current_hunk: Option<DiffHunk> = None;

    for line in lines.iter().skip(1) {
        if let Some(caps) = hunk_re().captures(line) {
            // 保存之前的 hunk
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }

            let old_start = caps.get(1)?.as_str().parse::<usize>().ok()?;
            let old_lines = caps
                .get(2)
                .map_or(1, |m| m.as_str().parse::<usize>().unwrap_or(1));
            let new_start = caps.get(3)?.as_str().parse::<usize>().ok()?;
            let new_lines = caps
                .get(4)
                .map_or(1, |m| m.as_str().parse::<usize>().unwrap_or(1));

            current_hunk = Some(DiffHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            });
            continue;
        }

        // 跳过元数据行
        if line.starts_with("index ")
            || line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("Binary files")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("copy from")
            || line.starts_with("copy to")
            || line.starts_with("dissimilarity index")
        {
            continue;
        }

        // 添加 diff 行到当前 hunk
        if let Some(ref mut hunk) = current_hunk {
            if line.starts_with('+') {
                additions += 1;
                hunk.lines.push((*line).to_string());
            } else if line.starts_with('-') {
                deletions += 1;
                hunk.lines.push((*line).to_string());
            } else if line.starts_with(' ') || line.is_empty() {
                hunk.lines.push((*line).to_string());
            }
        }
    }

    // 别忘了最后一个 hunk
    if let Some(hunk) = current_hunk {
        hunks.push(hunk);
    }

    if hunks.is_empty() {
        return None;
    }

    Some(DiffResult {
        old_path,
        new_path,
        hunks,
        additions,
        deletions,
    })
}

/// 解析标准 unified diff
fn parse_single_unified_diff(diff_text: &str) -> Option<DiffResult> {
    let lines: Vec<&str> = diff_text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut old_path = String::from("a");
    let mut new_path = String::from("b");

    let mut hunks = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut current_hunk: Option<DiffHunk> = None;

    for line in &lines {
        if let Some(stripped) = line.strip_prefix("--- ") {
            old_path = stripped.trim().to_string();
            continue;
        }
        if let Some(stripped) = line.strip_prefix("+++ ") {
            new_path = stripped.trim().to_string();
            continue;
        }

        if let Some(caps) = hunk_re().captures(line) {
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }

            let old_start = caps.get(1)?.as_str().parse::<usize>().ok()?;
            let old_lines = caps
                .get(2)
                .map_or(1, |m| m.as_str().parse::<usize>().unwrap_or(1));
            let new_start = caps.get(3)?.as_str().parse::<usize>().ok()?;
            let new_lines = caps
                .get(4)
                .map_or(1, |m| m.as_str().parse::<usize>().unwrap_or(1));

            current_hunk = Some(DiffHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(ref mut hunk) = current_hunk {
            if line.starts_with('+') {
                additions += 1;
                hunk.lines.push((*line).to_string());
            } else if line.starts_with('-') {
                deletions += 1;
                hunk.lines.push((*line).to_string());
            } else if line.starts_with(' ') || line.is_empty() {
                hunk.lines.push((*line).to_string());
            }
        }
    }

    if let Some(hunk) = current_hunk {
        hunks.push(hunk);
    }

    if hunks.is_empty() {
        return None;
    }

    Some(DiffResult {
        old_path,
        new_path,
        hunks,
        additions,
        deletions,
    })
}

// -- 单元测试 ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_diff_identical_texts() {
        let text = "line 1\nline 2\nline 3\n";
        let result = NativeDiff::diff_text(text, text, "a.txt", "b.txt", None);

        assert!(
            result.hunks.is_empty(),
            "identical texts should produce no hunks"
        );
        assert_eq!(result.additions, 0);
        assert_eq!(result.deletions, 0);
    }

    #[test]
    fn test_diff_simple_addition() {
        let old = "line 1\nline 2\n";
        let new = "line 1\nline 2\nline 3\n";
        let result = NativeDiff::diff_text(old, new, "a.txt", "b.txt", None);

        assert_eq!(result.additions, 1);
        assert_eq!(result.deletions, 0);
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn test_diff_simple_deletion() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline 3\n";
        let result = NativeDiff::diff_text(old, new, "a.txt", "b.txt", None);

        assert_eq!(result.additions, 0);
        assert_eq!(result.deletions, 1);
    }

    #[test]
    fn test_diff_modification() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline TWO\nline 3\n";
        let result = NativeDiff::diff_text(old, new, "a.txt", "b.txt", None);

        assert_eq!(result.additions, 1);
        assert_eq!(result.deletions, 1);
    }

    #[test]
    fn test_diff_empty_to_content() {
        let old = "";
        let new = "line 1\nline 2\n";
        let result = NativeDiff::diff_text(old, new, "a.txt", "b.txt", None);

        assert_eq!(result.additions, 2);
        assert_eq!(result.deletions, 0);
    }

    #[test]
    fn test_diff_content_to_empty() {
        let old = "line 1\nline 2\n";
        let new = "";
        let result = NativeDiff::diff_text(old, new, "a.txt", "b.txt", None);

        assert_eq!(result.additions, 0);
        assert_eq!(result.deletions, 2);
    }

    #[test]
    fn test_format_unified() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline TWO\nline 3\n";
        let result = NativeDiff::diff_text(old, new, "a/file.txt", "b/file.txt", None);
        let unified = NativeDiff::format_unified(&result);

        assert!(unified.contains("--- a/file.txt"));
        assert!(unified.contains("+++ b/file.txt"));
        assert!(unified.contains("@@"));
        assert!(unified.contains("-line 2"));
        assert!(unified.contains("+line TWO"));
    }

    #[test]
    fn test_format_unified_empty_diff() {
        let text = "same\n";
        let result = NativeDiff::diff_text(text, text, "a.txt", "b.txt", None);
        let unified = NativeDiff::format_unified(&result);

        assert!(unified.is_empty(), "no-diff should produce empty output");
    }

    #[test]
    fn test_diff_changes() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline TWO\nline 3\n";
        let changes = NativeDiff::diff_changes(old, new);

        assert!(!changes.is_empty());
        // 应该有 Equal、Delete、Insert、Equal 序列
        let has_equal = changes.iter().any(|c| c.tag == ChangeTag::Equal);
        let has_insert = changes.iter().any(|c| c.tag == ChangeTag::Insert);
        let has_delete = changes.iter().any(|c| c.tag == ChangeTag::Delete);
        assert!(has_equal, "should have equal lines");
        assert!(has_insert, "should have inserted lines");
        assert!(has_delete, "should have deleted lines");
    }

    #[test]
    fn test_diff_stats() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nd\n";
        let (additions, deletions) = NativeDiff::diff_stats(old, new);

        assert_eq!(additions, 2); // "B" + "d"
        assert_eq!(deletions, 1); // "b"
    }

    #[test]
    fn test_diff_files() {
        let dir = TempDir::new().expect("temp dir");

        let old_path = dir.path().join("old.txt");
        let new_path = dir.path().join("new.txt");

        fs::write(&old_path, "hello\nworld\n").expect("write old");
        fs::write(&new_path, "hello\nrust\nworld\n").expect("write new");

        let result = NativeDiff::diff_files(&old_path, &new_path, None).expect("diff");

        assert_eq!(result.additions, 1); // "rust"
        assert!(!result.hunks.is_empty());
    }

    #[test]
    fn test_diff_files_nonexistent_old() {
        let dir = TempDir::new().expect("temp dir");

        let old_path = dir.path().join("nonexistent.txt");
        let new_path = dir.path().join("new.txt");
        fs::write(&new_path, "new content\n").expect("write");

        let result = NativeDiff::diff_files(&old_path, &new_path, None).expect("diff");
        assert_eq!(result.additions, 1);
        assert_eq!(result.deletions, 0);
    }

    #[test]
    fn test_diff_context_lines() {
        // 创建有足够行数的文件以测试上下文
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        for i in 1..=20 {
            old_lines.push(format!("line {i}"));
            if i == 10 {
                new_lines.push("line TEN".to_string());
            } else {
                new_lines.push(format!("line {i}"));
            }
        }

        let old = old_lines.join("\n") + "\n";
        let new = new_lines.join("\n") + "\n";

        // context=1
        let result = NativeDiff::diff_text(&old, &new, "a.txt", "b.txt", Some(1));
        assert!(!result.hunks.is_empty());
        // hunk 应该只包含变更行 + 前后各 1 行上下文
        let hunk = &result.hunks[0];
        assert!(
            hunk.lines.len() <= 5,
            "context=1 should have <= 5 lines in hunk, got {}",
            hunk.lines.len()
        );
    }

    #[test]
    fn test_parse_unified_diff() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line TWO
 line 3
";
        let results = NativeDiff::parse_unified_diff(diff);
        assert_eq!(results.len(), 1);

        let r = &results[0];
        assert_eq!(r.old_path, "a/file.txt");
        assert_eq!(r.new_path, "b/file.txt");
        assert_eq!(r.additions, 1);
        assert_eq!(r.deletions, 1);
        assert_eq!(r.hunks.len(), 1);
        assert_eq!(r.hunks[0].old_start, 1);
        assert_eq!(r.hunks[0].old_lines, 3);
    }

    #[test]
    fn test_parse_git_diff() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"Hello!\");
     println!(\"world\");
 }
";
        let results = NativeDiff::parse_unified_diff(diff);
        assert_eq!(results.len(), 1);

        let r = &results[0];
        assert_eq!(r.old_path, "src/main.rs");
        assert_eq!(r.new_path, "src/main.rs");
        assert_eq!(r.additions, 1);
        assert_eq!(r.deletions, 0);
    }

    #[test]
    fn test_parse_multi_file_git_diff() {
        let diff = "\
diff --git a/file1.rs b/file1.rs
index aaa..bbb 100644
--- a/file1.rs
+++ b/file1.rs
@@ -1,2 +1,3 @@
 line 1
+added line
 line 2
diff --git a/file2.rs b/file2.rs
index ccc..ddd 100644
--- a/file2.rs
+++ b/file2.rs
@@ -1,3 +1,2 @@
 line 1
-removed line
 line 3
";
        let results = NativeDiff::parse_unified_diff(diff);
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].additions, 1);
        assert_eq!(results[0].deletions, 0);
        assert_eq!(results[1].additions, 0);
        assert_eq!(results[1].deletions, 1);
    }

    #[test]
    fn test_parse_empty_diff() {
        let results = NativeDiff::parse_unified_diff("");
        assert!(results.is_empty());
    }

    #[test]
    fn test_multiline_diff() {
        let old =
            "fn foo() {\n    // old implementation\n    let x = 1;\n    let y = 2;\n    x + y\n}\n";
        let new = "fn foo() -> i32 {\n    // new implementation\n    let x = 10;\n    let y = 20;\n    x + y\n}\n";

        let result = NativeDiff::diff_text(old, new, "a.rs", "b.rs", Some(3));
        let unified = NativeDiff::format_unified(&result);

        // 验证格式正确
        assert!(unified.contains("--- a.rs"));
        assert!(unified.contains("+++ b.rs"));
        assert!(unified.contains("@@"));

        // 验证统计信息
        assert!(result.additions > 0);
        assert!(result.deletions > 0);
    }
}
