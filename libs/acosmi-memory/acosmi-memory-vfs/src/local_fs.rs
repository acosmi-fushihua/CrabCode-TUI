// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! `LocalFs` — concrete `FileSystem` implementation backed by local disk
//! via `tokio::fs`.
//!
//! URI convention: `viking://resources/foo` maps to `{root}/resources/foo`.
//! A plain path (no `viking://` prefix) is treated relative to `root`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use acosmi_memory_session::traits::{BoxError, FileSystem, FsEntry, FsStat, GrepMatch};

/// 原子写临时文件名的进程内单调序号。配合 `std::process::id()` 保证并发写
/// 不同目标 / 同目标重试时临时文件名唯一，互不踩踏。
static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A `FileSystem` implementation that maps Viking URIs to real paths under a
/// root directory and delegates all I/O to `tokio::fs`.
pub struct LocalFs {
    /// Root directory for all file operations.
    root: PathBuf,
}

impl LocalFs {
    /// Create a new `LocalFs` rooted at the given directory.
    ///
    /// The directory is **not** created automatically; callers should ensure
    /// it exists before issuing file operations.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a Viking URI to an absolute path under `self.root`.
    ///
    /// Strips the `viking://` prefix if present, then joins the remainder
    /// onto `self.root` after validating each path component to prevent
    /// directory traversal escape.
    ///
    /// Rejects any URI containing `..` segments or absolute path components,
    /// returning an error rather than silently allowing the resolved path
    /// to escape `self.root`. Closes audit findings:
    /// `Rust底层核心健康度审计-2026-05-05.md` §九 §9.8 #1 (HIGH) +
    /// §十三 HIGH-memvfs-1 (viking_fs URI 拼接同源外溢).
    fn resolve(&self, uri: &str) -> Result<PathBuf, BoxError> {
        let stripped = uri
            .strip_prefix("viking://")
            .unwrap_or(uri)
            .trim_start_matches('/');

        let mut buf = self.root.clone();
        for component in Path::new(stripped).components() {
            use std::path::Component;
            match component {
                Component::Normal(c) => buf.push(c),
                Component::CurDir => {} // "." 段无害，跳过
                Component::ParentDir => {
                    let msg = format!("URI contains forbidden parent reference (..): {uri}");
                    return Err(Box::<dyn std::error::Error + Send + Sync>::from(msg));
                }
                Component::RootDir | Component::Prefix(_) => {
                    let msg = format!("URI contains forbidden absolute path component: {uri}");
                    return Err(Box::<dyn std::error::Error + Send + Sync>::from(msg));
                }
            }
        }
        Ok(buf)
    }

    /// Ensure that the parent directory of `path` exists.
    async fn ensure_parent(path: &Path) -> Result<(), BoxError> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }
        Ok(())
    }

    /// 崩溃安全的原子全量写：写同目录临时文件 → `sync_all`（数据落盘）→
    /// `rename` 覆盖目标。
    ///
    /// 裸 `fs::write` = `open(O_TRUNC)+write+close`，写入中途崩溃 → 目标文件
    /// 零字节 / 截断（`.relations.json` / `messages.jsonl` / `MEMORY.md` 等
    /// 半写损坏，重启不自愈）。同目录 `rename` 在 POSIX / Windows 上对读者
    /// 原子可见：要么旧内容、要么完整新内容，永不见半写。临时文件先 `sync_all`
    /// 确保数据真正落盘后再 rename。失败路径清理临时文件，不泄漏。
    ///
    /// 注：跨设备 rename 会 `EXDEV` 失败，但临时文件与目标同父目录、必同设备，
    /// 故不触发；记忆系统不写 symlink 目标，rename 替换 symlink 本身可接受。
    async fn atomic_write_bytes(path: &Path, content: &[u8]) -> Result<(), BoxError> {
        Self::ensure_parent(path).await?;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "atomic".to_string());
        let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

        let map_err = |stage: &str, e: std::io::Error, p: &Path| -> BoxError {
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "atomic_write {stage} {}: {e}",
                p.display()
            ))
        };

        let mut file = match fs::File::create(&tmp_path).await {
            Ok(f) => f,
            Err(e) => return Err(map_err("create", e, &tmp_path)),
        };
        // 写 + sync_data；任一步失败清理临时文件后返错。
        let write_res = async {
            file.write_all(content).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await;
        if let Err(e) = write_res {
            drop(file);
            let _ = fs::remove_file(&tmp_path).await;
            return Err(map_err("write", e, &tmp_path));
        }
        drop(file);

        if let Err(e) = fs::rename(&tmp_path, path).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(map_err("rename", e, path));
        }
        Ok(())
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    // -------------------------------------------------------------------
    // read / write
    // -------------------------------------------------------------------

    async fn read(&self, uri: &str) -> Result<String, BoxError> {
        let path = self.resolve(uri)?;
        // Step 2 Phase D.7 — closes Step 1 §六 R1 / HIGH-extractor.rs:259:
        // wrap as `std::io::Error` so the original `kind()` is preserved
        // (rather than the previous `String` formatting which made
        // `is_not_found_error()` undecidable from caller's side). The
        // formatted message is kept identical for log compatibility.
        fs::read_to_string(&path).await.map_err(|e| {
            let kind = e.kind();
            let wrapped = std::io::Error::new(kind, format!("read {}: {e}", path.display()));
            Box::new(wrapped) as BoxError
        })
    }

    async fn read_bytes(&self, uri: &str) -> Result<Vec<u8>, BoxError> {
        let path = self.resolve(uri)?;
        // Same kind-preservation pattern as `read` above.
        fs::read(&path).await.map_err(|e| {
            let kind = e.kind();
            let wrapped = std::io::Error::new(kind, format!("read_bytes {}: {e}", path.display()));
            Box::new(wrapped) as BoxError
        })
    }

    async fn write(&self, uri: &str, content: &str) -> Result<(), BoxError> {
        let path = self.resolve(uri)?;
        Self::atomic_write_bytes(&path, content.as_bytes()).await
    }

    async fn write_bytes(&self, uri: &str, content: &[u8]) -> Result<(), BoxError> {
        let path = self.resolve(uri)?;
        Self::atomic_write_bytes(&path, content).await
    }

    // -------------------------------------------------------------------
    // mkdir
    // -------------------------------------------------------------------

    async fn mkdir(&self, uri: &str) -> Result<(), BoxError> {
        let path = self.resolve(uri)?;
        fs::create_dir_all(&path).await.map_err(|e| {
            let msg = format!("mkdir {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // ls
    // -------------------------------------------------------------------

    async fn ls(&self, uri: &str) -> Result<Vec<FsEntry>, BoxError> {
        let path = self.resolve(uri)?;
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(&path).await.map_err(|e| {
            let msg = format!("ls {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;

        while let Some(entry) = dir.next_entry().await? {
            let meta = entry.metadata().await?;
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }

        Ok(entries)
    }

    // -------------------------------------------------------------------
    // rm — auto-detect file vs. directory
    // -------------------------------------------------------------------

    async fn rm(&self, uri: &str) -> Result<(), BoxError> {
        let path = self.resolve(uri)?;
        let meta = fs::metadata(&path).await.map_err(|e| {
            let msg = format!("rm stat {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;

        if meta.is_dir() {
            fs::remove_dir_all(&path).await.map_err(|e| {
                let msg = format!("rm dir {}: {e}", path.display());
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })?;
        } else {
            fs::remove_file(&path).await.map_err(|e| {
                let msg = format!("rm file {}: {e}", path.display());
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // mv — rename with cross-device fallback (copy + delete)
    // -------------------------------------------------------------------

    async fn mv(&self, from_uri: &str, to_uri: &str) -> Result<(), BoxError> {
        let from = self.resolve(from_uri)?;
        let to = self.resolve(to_uri)?;

        Self::ensure_parent(&to).await?;

        // Try atomic rename first.
        match fs::rename(&from, &to).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // On cross-device links, rename returns EXDEV (errno 18 on
                // macOS/Linux). Fall back to copy + delete.
                if e.raw_os_error() == Some(libc_exdev()) {
                    let meta = fs::metadata(&from).await?;
                    if meta.is_dir() {
                        copy_dir_recursive(&from, &to).await?;
                        fs::remove_dir_all(&from).await?;
                    } else {
                        fs::copy(&from, &to).await?;
                        fs::remove_file(&from).await?;
                    }
                    Ok(())
                } else {
                    let msg = format!("mv {} → {}: {e}", from.display(), to.display());
                    Err(Box::<dyn std::error::Error + Send + Sync>::from(msg))
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // stat
    // -------------------------------------------------------------------

    async fn stat(&self, uri: &str) -> Result<FsStat, BoxError> {
        let path = self.resolve(uri)?;
        let meta = fs::metadata(&path).await.map_err(|e| {
            let msg = format!("stat {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;

        let mod_time = meta
            .modified()
            .map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.to_rfc3339()
            })
            .unwrap_or_default();

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(FsStat {
            name,
            size: meta.len(),
            is_dir: meta.is_dir(),
            mod_time,
        })
    }

    // -------------------------------------------------------------------
    // grep — line-by-line substring match (no regex crate)
    // -------------------------------------------------------------------

    async fn grep(
        &self,
        uri: &str,
        pattern: &str,
        recursive: bool,
        case_insensitive: bool,
    ) -> Result<Vec<GrepMatch>, BoxError> {
        let path = self.resolve(uri)?;
        let meta = fs::metadata(&path).await.map_err(|e| {
            let msg = format!("grep stat {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;

        let mut results = Vec::new();

        if meta.is_file() {
            grep_file(&path, uri, pattern, case_insensitive, &mut results).await?;
        } else if meta.is_dir() && recursive {
            grep_dir_recursive(&path, uri, pattern, case_insensitive, &mut results).await?;
        }

        Ok(results)
    }

    // -------------------------------------------------------------------
    // exists
    // -------------------------------------------------------------------

    async fn exists(&self, uri: &str) -> Result<bool, BoxError> {
        let path = self.resolve(uri)?;
        Ok(fs::try_exists(&path).await.unwrap_or(false))
    }

    // -------------------------------------------------------------------
    // append
    // -------------------------------------------------------------------

    async fn append(&self, uri: &str, content: &str) -> Result<(), BoxError> {
        let path = self.resolve(uri)?;
        Self::ensure_parent(&path).await?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| {
                let msg = format!("append {}: {e}", path.display());
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })?;

        file.write_all(content.as_bytes()).await.map_err(|e| {
            let msg = format!("append write {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;
        // tokio `File` drop 不保证 flush；append 必须显式 flush + sync_data
        // 落盘，否则进程崩溃 / 断电时刚 append 的记录（messages.jsonl 等）丢失。
        file.flush().await.map_err(|e| {
            let msg = format!("append flush {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;
        file.sync_data().await.map_err(|e| {
            let msg = format!("append sync {}: {e}", path.display());
            Box::<dyn std::error::Error + Send + Sync>::from(msg)
        })?;

        Ok(())
    }

    // -------------------------------------------------------------------
    // link — symbolic link (macOS / Linux)
    // -------------------------------------------------------------------

    async fn link(&self, source_uri: &str, target_uri: &str) -> Result<(), BoxError> {
        // 2026-07-04 审计（原则 C 附带发现）：原实现 non-unix 臂 `return Err` 后
        // 跟着共享的 `Ok(())`——win32 下 clippy -D warnings 报不可达代码 + source
        // 未用（mac 闸门看不见的平台性存量缺陷）。改为两臂各自终结。
        let source = self.resolve(source_uri)?;
        let target = self.resolve(target_uri)?;

        Self::ensure_parent(&target).await?;

        #[cfg(unix)]
        {
            tokio::fs::symlink(&source, &target).await.map_err(|e| {
                let msg = format!("link {} → {}: {e}", source.display(), target.display());
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let _ = source;
            Err("symlink is only supported on Unix-like systems".into())
        }
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Cross-device move errno.
#[cfg(unix)]
fn libc_exdev() -> i32 {
    18 // EXDEV on macOS and Linux
}

/// Cross-device move errno (non-Unix stub — always returns -1).
#[cfg(not(unix))]
fn libc_exdev() -> i32 {
    -1 // Windows: rename across drives returns a different error code
}

/// Recursively copy a directory.
async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), BoxError> {
    fs::create_dir_all(dst).await?;
    let mut dir = fs::read_dir(src).await?;
    while let Some(entry) = dir.next_entry().await? {
        let src_child = entry.path();
        let dst_child = dst.join(entry.file_name());
        if entry.metadata().await?.is_dir() {
            Box::pin(copy_dir_recursive(&src_child, &dst_child)).await?;
        } else {
            fs::copy(&src_child, &dst_child).await?;
        }
    }
    Ok(())
}

/// Grep a single file for `pattern`, appending matches to `out`.
async fn grep_file(
    path: &Path,
    uri: &str,
    pattern: &str,
    case_insensitive: bool,
    out: &mut Vec<GrepMatch>,
) -> Result<(), BoxError> {
    let content = match fs::read_to_string(path).await {
        Ok(c) => c,
        Err(_) => return Ok(()), // skip binary / unreadable files
    };

    let pat_lower = pattern.to_lowercase();

    for (idx, line) in content.lines().enumerate() {
        let matched = if case_insensitive {
            line.to_lowercase().contains(&pat_lower)
        } else {
            line.contains(pattern)
        };

        if matched {
            out.push(GrepMatch {
                uri: uri.to_string(),
                line: (idx + 1) as u64,
                content: line.to_string(),
            });
        }
    }
    Ok(())
}

/// Recursively grep a directory.
async fn grep_dir_recursive(
    dir_path: &Path,
    base_uri: &str,
    pattern: &str,
    case_insensitive: bool,
    out: &mut Vec<GrepMatch>,
) -> Result<(), BoxError> {
    let mut dir = fs::read_dir(dir_path).await?;
    while let Some(entry) = dir.next_entry().await? {
        let child_path = entry.path();
        let child_name = entry.file_name().to_string_lossy().into_owned();
        let child_uri = format!("{}/{}", base_uri.trim_end_matches('/'), child_name);

        let meta = entry.metadata().await?;
        if meta.is_file() {
            grep_file(&child_path, &child_uri, pattern, case_insensitive, out).await?;
        } else if meta.is_dir() {
            Box::pin(grep_dir_recursive(
                &child_path,
                &child_uri,
                pattern,
                case_insensitive,
                out,
            ))
            .await?;
        }
    }
    Ok(())
}
