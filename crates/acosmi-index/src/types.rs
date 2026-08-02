//! 共享类型定义
//!
//! 索核模块对外暴露的核心数据结构。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// 文件类型分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// 普通文件
    File,
    /// 目录
    Directory,
    /// 符号链接
    Symlink,
}

/// 文件元数据条目（索引中的一条记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 文件绝对路径
    pub path: PathBuf,
    /// 相对于索引根目录的路径
    pub relative_path: String,
    /// 文件大小（字节）
    pub size: u64,
    /// 最后修改时间（用于增量更新判断）
    #[serde(
        serialize_with = "serialize_system_time",
        deserialize_with = "deserialize_system_time"
    )]
    pub modified: SystemTime,
    /// 文件类型
    pub kind: FileKind,
}

/// 模糊搜索结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// 匹配的文件路径（相对路径）
    pub path: String,
    /// 匹配分数（0.0 = 最佳匹配，1.0 = 最差）
    pub score: f64,
}

/// 内容搜索匹配项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentMatch {
    /// 文件路径
    pub path: PathBuf,
    /// 行号（从 1 开始）
    pub line_number: usize,
    /// 匹配行的完整内容
    pub line_content: String,
    /// 匹配区间在行内的起止字节偏移 (start, end)
    pub match_range: (usize, usize),
}

/// 内容搜索选项
#[derive(Debug, Clone)]
pub struct ContentSearchOptions {
    /// 正则表达式模式
    pub pattern: String,
    /// 是否区分大小写（默认 true）
    pub case_sensitive: bool,
    /// 是否将模式视为字面量（而非正则）
    pub literal: bool,
    /// 限制返回结果数量（0 = 不限制）
    pub max_results: usize,
    /// 限制搜索的文件 glob 模式（空 = 搜索所有文件）
    pub include_globs: Vec<String>,
    /// 排除的文件 glob 模式
    pub exclude_globs: Vec<String>,
    /// 是否搜索隐藏文件
    pub include_hidden: bool,
}

impl Default for ContentSearchOptions {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            case_sensitive: true,
            literal: false,
            max_results: 0,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            include_hidden: false,
        }
    }
}

/// Diff 变更类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeTag {
    /// 两侧相同（上下文行）
    Equal,
    /// 仅在新文本中出现（添加行）
    Insert,
    /// 仅在旧文本中出现（删除行）
    Delete,
}

/// Diff 中的单个变更操作
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffChange {
    /// 变更类型
    pub tag: ChangeTag,
    /// 变更的文本内容
    pub value: String,
}

/// Unified diff 中的一个 hunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    /// 旧文件起始行号（从 1 开始）
    pub old_start: usize,
    /// 旧文件行数
    pub old_lines: usize,
    /// 新文件起始行号（从 1 开始）
    pub new_start: usize,
    /// 新文件行数
    pub new_lines: usize,
    /// hunk 中的各行（含 +/-/空格 前缀）
    pub lines: Vec<String>,
}

/// 完整 diff 结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    /// 旧文件路径标签
    pub old_path: String,
    /// 新文件路径标签
    pub new_path: String,
    /// hunks 列表
    pub hunks: Vec<DiffHunk>,
    /// 添加行数
    pub additions: usize,
    /// 删除行数
    pub deletions: usize,
}

/// 索核模块统一错误类型
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// IO 错误
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// 正则表达式编译错误
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    /// Glob 模式错误
    #[error("glob error: {0}")]
    Glob(#[from] globset::Error),

    /// 路径不存在
    #[error("path not found: {0}")]
    PathNotFound(PathBuf),

    /// 索引尚未构建
    #[error("index not built yet")]
    IndexNotBuilt,
}

/// 索核模块统一 Result 类型
pub type IndexResult<T> = Result<T, IndexError>;

// -- SystemTime serde helpers ------------------------------------------------

fn serialize_system_time<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    serializer.serialize_u64(duration.as_secs())
}

fn deserialize_system_time<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}
