//! `CrabCode` 文件索引（索核模块）
//!
//! 提供三大核心功能：
//!
//! 1. **文件索引** (`file_index`) — 递归遍历目录、gitignore 过滤、增量更新、
//!    模糊搜索（nucleo 风格评分算法）
//!
//! 2. **内容/文件名搜索** (`search`) — 正则内容搜索（类 ripgrep）、glob 文件名
//!    匹配、rayon 并行搜索、memmap 大文件处理
//!
//! 3. **Native diff** (`native_diff`) — 行级 diff（Myers 算法）、unified diff
//!    格式输出、diff 解析、大文件流式处理
//!
//! # 模块结构
//!
//! - `types` — 共享类型（`FileEntry`, `SearchResult`, `ContentMatch`, `DiffHunk` 等）
//! - `file_index` — `FileIndex` 结构体
//! - `search` — `ContentSearcher`, `FileNameSearcher`
//! - `native_diff` — `NativeDiff`
//!
//! # 快速示例
//!
//! ```rust,no_run
//! use acosmi_index::FileIndex;
//!
//! let mut index = FileIndex::new(".");
//! index.build().expect("build index");
//! let results = index.search("main", 10);
//! for r in &results {
//!     println!("{} (score: {:.3})", r.path, r.score);
//! }
//! ```

pub mod file_index;
pub mod native_diff;
pub mod search;
pub mod types;

// 重新导出核心类型，方便外部使用
pub use file_index::FileIndex;
pub use native_diff::NativeDiff;
pub use search::{ContentSearcher, FileNameSearcher};
pub use types::{
    ChangeTag, ContentMatch, ContentSearchOptions, DiffChange, DiffHunk, DiffResult, FileEntry,
    FileKind, IndexError, IndexResult, SearchResult,
};
