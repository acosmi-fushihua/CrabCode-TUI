#![allow(unsafe_code)]

//! `~/.crabcode/run/<name>.lock` spawn 互斥（跨平台）。
//!
//! - Unix：`flock(LOCK_EX | LOCK_NB)` 抢占。
//! - Windows：`CreateFile` 独占共享模式（`share_mode(0)`）——第二个打开者收到
//!   `ERROR_SHARING_VIOLATION` → [`LockOutcome::Contended`]。
//!
//! 两端同语义：持锁直到 [`LockGuard`] drop；持锁进程退出（含被 taskkill /
//! SIGKILL 硬杀）时 OS 自动放锁（Unix 关 fd 释放 flock、Windows 关句柄释放
//! 共享排斥），**无 stale lock 问题**。
//!
//! W-CRON-RELEASE-REOPEN P0-2（2026-07-16）：此前该模块 `#[cfg(unix)]`，
//! Windows ensure 路径完全无 spawn 互斥——并发 ensure 双 spawn，输家撞 daemon
//! 业务锁结构化退出，首启迁移 >5s 时双方 PidTimeout。现在 Windows 走独占句柄
//! 镜像 flock 语义，spawn 临界区两端统一由本模块串行化。
//!
//! 不同 daemon name 的 lock 文件互不干扰（如 cron.lock vs worker.lock）。

use std::fs::File;
use std::path::Path;

use crate::Result;

pub enum LockOutcome {
    Acquired(LockGuard),
    Contended,
}

pub struct LockGuard {
    /// 持有 fd / 句柄直到 drop —— OS 在 close 时自动释放锁。
    _file: File,
}

#[cfg(unix)]
pub fn acquire(path: &Path) -> Result<LockOutcome> {
    use std::os::fd::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(LockOutcome::Acquired(LockGuard { _file: file }))
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(LockOutcome::Contended)
        } else {
            Err(crate::LauncherError::Io(err))
        }
    }
}

#[cfg(windows)]
pub fn acquire(path: &Path) -> Result<LockOutcome> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `winerror.h` ERROR_SHARING_VIOLATION —— 已有独占句柄持有者时
    /// `CreateFile` 的失败码。
    const ERROR_SHARING_VIOLATION: i32 = 32;

    match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .share_mode(0) // 独占：拒绝任何并发 open（读/写/删除共享全关）
        .open(path)
    {
        Ok(file) => Ok(LockOutcome::Acquired(LockGuard { _file: file })),
        Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
            Ok(LockOutcome::Contended)
        }
        Err(err) => Err(crate::LauncherError::Io(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_then_contend() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("cron.lock");

        let r1 = acquire(&lock).unwrap();
        assert!(matches!(r1, LockOutcome::Acquired(_)));

        // 同进程二次抢锁必须 Contended：Unix 上不同 fd 的 flock LOCK_EX 互斥，
        // Windows 上独占共享模式拒绝第二个 open。
        let r2 = acquire(&lock).unwrap();
        assert!(matches!(r2, LockOutcome::Contended));

        drop(r1);
        let r3 = acquire(&lock).unwrap();
        assert!(matches!(r3, LockOutcome::Acquired(_)));
    }
}
