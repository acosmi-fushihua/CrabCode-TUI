use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtomicWriteReport {
    pub tmp_path: PathBuf,
    pub bytes: usize,
    pub file_fsynced: bool,
    pub renamed: bool,
    pub parent_fsynced: bool,
}

pub async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BoxError> {
    atomic_write_with_report(path, bytes).await.map(|_| ())
}

pub async fn atomic_write_with_report(
    path: &Path,
    bytes: &[u8],
) -> Result<AtomicWriteReport, BoxError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("target has no parent directory: {}", path.display()),
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;

    let tmp_path = unique_tmp_path(path);
    let mut tmp = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .await?;
    tmp.write_all(bytes).await?;
    tmp.flush().await?;
    tmp.sync_all().await?;
    drop(tmp);

    if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e.into());
    }

    let parent_fsynced = fsync_parent_dir(parent).await?;
    Ok(AtomicWriteReport {
        tmp_path,
        bytes: bytes.len(),
        file_fsynced: true,
        renamed: true,
        parent_fsynced,
    })
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let count = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("atomic-write");
    path.with_file_name(format!(".{file_name}.tmp.{}.{}", std::process::id(), count))
}

#[cfg(unix)]
async fn fsync_parent_dir(parent: &Path) -> Result<bool, BoxError> {
    let dir = tokio::fs::File::open(parent).await?;
    dir.sync_all().await?;
    Ok(true)
}

#[cfg(not(unix))]
async fn fsync_parent_dir(_parent: &Path) -> Result<bool, BoxError> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_tmp_fsync_rename_replaces_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pending-triggers.json");
        tokio::fs::write(&path, br#"{"old":true}"#).await.unwrap();

        let report = atomic_write_with_report(&path, br#"{"new":true}"#)
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"new":true}"#);
        assert_eq!(report.bytes, 12);
        assert!(report.file_fsynced);
        assert!(report.renamed);
        #[cfg(unix)]
        assert!(report.parent_fsynced);
        assert!(!report.tmp_path.exists());
    }

    #[tokio::test]
    async fn test_atomic_write_creates_parent_and_leaves_no_tmp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("derived").join("logs").join("entry.jsonl");

        let report = atomic_write_with_report(&path, b"{\"ok\":true}\n")
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"ok\":true}\n");
        assert!(!report.tmp_path.exists());
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0);
    }
}
