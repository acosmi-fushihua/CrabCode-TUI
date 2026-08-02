//! Shared `crabcode-cron` daemon ensure contract.
//!
//! W-CRON-AUTOMATION-E2E RC2 — the cron lazy-spawn used to live private to
//! `acosmi-cmd-cron::client` (CLI only), so another native caller could not
//! ensure the daemon. This module lifts the binary-resolution + ensure logic
//! into the shared launcher crate with typed errors so the CLI client and
//! native TUI reuse one contract.
//!
//! - [`is_cron_disabled`] — `CRABCODE_DISABLE_CRON` env gate.
//! - [`resolve_cron_binary`] — locate the `crabcode-cron` sibling binary.
//! - [`ensure_cron_daemon`] — SYNC; callers in async code MUST wrap it in
//!   `tokio::task::spawn_blocking` (it runs a Unix double-fork and waits up
//!   to 5s on the PID file).

use std::path::PathBuf;

use thiserror::Error;

use crate::{DaemonHandle, LauncherError};

/// Compile-time release authority for daemon-persistent cron execution.
///
/// The release gate is enabled after lifecycle cleanup, Windows spawn
/// mutual exclusion, lock liveness, and PID identity checks were unified.
/// 投递链 = daemon 单写者 delivery journal（begin/accept/abandon）+
/// 会话键路由（2026-07-06 L0-L6）+ per-thread 属主锚定（1ef2b80f）——
/// at-least-once 语义；occurrence 精确 claim 消费制（cron.claim /
/// accept / report_failure）daemon 侧已备、TS 消费端切换保持独立立项，不阻塞
/// 本发布面。Runtime environment、config 与远程 feature flag 仍然只是
/// kill switch（`CRABCODE_DISABLE_CRON` / GrowthBook），不是发布权威。
pub const DURABLE_CRON_LIVE_CONSUMER_RELEASED: bool = true;

// Type-level release tripwire (now pinning ON): flipping the authority back to
// `false` changes the right-hand array length to 0 and makes this module fail
// to compile — an un-flip must be a deliberate, reviewed act, exactly like the
// flip itself was. Stronger than a runtime test and cannot be optimized away.
const _: [(); 1] = [(); DURABLE_CRON_LIVE_CONSUMER_RELEASED as usize];

pub const DURABLE_CRON_RELEASE_BLOCK_REASON: &str = "durable cron live-consumer cutover is not released; exact execution target claim is unavailable";

/// Typed failure modes for [`ensure_cron_daemon`].
#[derive(Debug, Error)]
pub enum CronEnsureError {
    /// `CRABCODE_DISABLE_CRON` is set truthy — caller asked for no cron.
    #[error("cron 已通过 CRABCODE_DISABLE_CRON 关闭；移除该 env 后再试")]
    Disabled,
    /// `crabcode-cron` binary could not be located.
    #[error("crabcode-cron 二进制未找到: {0}")]
    BinaryMissing(String),
    /// The daemon process failed to spawn.
    #[error("cron daemon spawn 失败: {0}")]
    SpawnFailed(String),
    /// The daemon spawned but never wrote its PID file (likely crashed).
    #[error("cron daemon 未在超时内写出 PID 文件（可能启动崩溃）")]
    PidTimeout,
    /// Spawn was refused for permission reasons.
    #[error("cron daemon 拉起权限不足: {0}")]
    Permission(String),
}

/// `CRABCODE_DISABLE_CRON` environment gate (mirrors the TS
/// `isKairosCronEnabled` second-layer gate). Truthy unless empty / `0` /
/// `false` (case-insensitive).
pub fn is_cron_disabled() -> bool {
    std::env::var("CRABCODE_DISABLE_CRON")
        .ok()
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Locate the `crabcode-cron` binary: `CRABCODE_CRON_BIN` env override first,
/// else the `crabcode-cron` (`.exe` on Windows) sibling of `current_exe()`.
///
/// release packaging path: `<install_dir>/crabcode-cron` (alongside
/// `crabcode`); `release.sh` + `release.yml` copy it into the bundle.
pub fn resolve_cron_binary() -> Result<PathBuf, CronEnsureError> {
    if let Some(path) = std::env::var_os("CRABCODE_CRON_BIN") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
        return Err(CronEnsureError::BinaryMissing(format!(
            "CRABCODE_CRON_BIN 指向的文件不存在: {}",
            p.display()
        )));
    }
    let exe = std::env::current_exe()
        .map_err(|e| CronEnsureError::BinaryMissing(format!("读取 current_exe 失败: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| CronEnsureError::BinaryMissing("current_exe 无父目录".to_string()))?;
    let bin_name = if cfg!(windows) {
        "crabcode-cron.exe"
    } else {
        "crabcode-cron"
    };
    let candidate = dir.join(bin_name);
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(CronEnsureError::BinaryMissing(format!(
        "找不到 crabcode-cron 二进制（已查 {}）；release 安装目录或 target/{{debug,release}}/ 内应有此文件",
        candidate.display()
    )))
}

/// Map a low-level [`LauncherError`] onto the typed [`CronEnsureError`].
///
/// Extracted out of [`ensure_cron_daemon`] so the mapping is unit-testable
/// without a live spawn. The `match` is exhaustive over `LauncherError`
/// (compiler-checked) — any new variant forces a decision here.
fn map_launcher_error(err: LauncherError) -> CronEnsureError {
    match err {
        LauncherError::BinaryMissing(p) => CronEnsureError::BinaryMissing(p.display().to_string()),
        LauncherError::PidFileTimeout(_) => CronEnsureError::PidTimeout,
        LauncherError::SpawnFailed(s) => CronEnsureError::SpawnFailed(s),
        LauncherError::LockOrphaned => CronEnsureError::SpawnFailed(
            "lock 被持有但 PID 文件未出现：peer launcher spawn 失败".to_string(),
        ),
        LauncherError::Io(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                CronEnsureError::Permission(e.to_string())
            } else {
                CronEnsureError::SpawnFailed(e.to_string())
            }
        }
        #[cfg(unix)]
        LauncherError::Nix(e) => {
            if e == nix::errno::Errno::EACCES || e == nix::errno::Errno::EPERM {
                CronEnsureError::Permission(e.to_string())
            } else {
                CronEnsureError::SpawnFailed(e.to_string())
            }
        }
    }
}

/// Ensure the `crabcode-cron` daemon is running, returning its handle.
///
/// SYNC — runs a Unix double-fork and waits up to 5s on the PID file.
/// Callers inside an async runtime MUST invoke this via
/// `tokio::task::spawn_blocking` (both to honour fork-safety and to avoid
/// blocking the runtime on the PID wait).
///
/// `CRABCODE_DISABLE_CRON` is honoured: returns [`CronEnsureError::Disabled`]
/// without touching the filesystem or spawning anything.
pub fn ensure_cron_daemon() -> Result<DaemonHandle, CronEnsureError> {
    if is_cron_disabled() {
        return Err(CronEnsureError::Disabled);
    }
    let bin = resolve_cron_binary()?;
    let sock = crate::paths::socket_file("cron");
    let sock_str = sock.to_string_lossy().to_string();
    crate::ensure_running("cron", &sock_str, &bin).map_err(map_launcher_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durable_cron_release_block_reason_names_exact_target_gap() {
        assert!(DURABLE_CRON_RELEASE_BLOCK_REASON.contains("exact execution target"));
    }

    #[test]
    fn cron_ensure_error_display_strings() {
        assert!(format!("{}", CronEnsureError::Disabled).contains("CRABCODE_DISABLE_CRON"));
        assert!(
            format!("{}", CronEnsureError::BinaryMissing("x".to_string()))
                .contains("crabcode-cron 二进制未找到: x")
        );
        assert!(
            format!("{}", CronEnsureError::SpawnFailed("boom".to_string()))
                .contains("cron daemon spawn 失败: boom")
        );
        assert!(format!("{}", CronEnsureError::PidTimeout).contains("PID 文件"));
        assert!(
            format!("{}", CronEnsureError::Permission("eacces".to_string()))
                .contains("cron daemon 拉起权限不足: eacces")
        );
    }

    #[test]
    fn map_launcher_error_pid_timeout() {
        let mapped = map_launcher_error(LauncherError::PidFileTimeout(Duration::from_secs(5)));
        assert!(matches!(mapped, CronEnsureError::PidTimeout));
    }

    #[test]
    fn map_launcher_error_binary_missing() {
        let mapped = map_launcher_error(LauncherError::BinaryMissing(PathBuf::from(
            "/no/such/crabcode-cron",
        )));
        match mapped {
            CronEnsureError::BinaryMissing(s) => {
                assert!(s.contains("crabcode-cron"));
            }
            other => panic!("expected BinaryMissing, got {other:?}"),
        }
    }

    #[test]
    fn map_launcher_error_spawn_failed() {
        let mapped = map_launcher_error(LauncherError::SpawnFailed("fork failed".to_string()));
        match mapped {
            CronEnsureError::SpawnFailed(s) => assert_eq!(s, "fork failed"),
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn map_launcher_error_lock_orphaned_becomes_spawn_failed() {
        let mapped = map_launcher_error(LauncherError::LockOrphaned);
        match mapped {
            CronEnsureError::SpawnFailed(s) => assert!(s.contains("peer launcher")),
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn map_launcher_error_io_permission_denied_becomes_permission() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let mapped = map_launcher_error(LauncherError::Io(io));
        assert!(matches!(mapped, CronEnsureError::Permission(_)));
    }

    #[test]
    fn map_launcher_error_io_other_becomes_spawn_failed() {
        let io = std::io::Error::other("weird");
        let mapped = map_launcher_error(LauncherError::Io(io));
        assert!(matches!(mapped, CronEnsureError::SpawnFailed(_)));
    }
}
