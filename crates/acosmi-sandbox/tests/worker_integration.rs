//! Integration tests for the persistent sandbox Worker.
//!
//! These tests verify the full end-to-end flow: launcher → Worker process →
//! IPC protocol → WorkerHandle client API.
//!
//! Tests use `launch_worker_unsandboxed` to avoid requiring sandbox privileges.
//! Sandboxed Worker tests are in `macos_worker_integration.rs` (macOS).
//!
//! **前提条件**: 需要预先编译 CLI 二进制。
//! 如果 CLI 二进制不可用，测试会优雅跳过（不 panic）。
//!
//! 运行方式:
//!   cargo build -p acosmi-runtime && cargo test --test worker_integration
//!   # 或指定二进制路径:
//!   CRABCODE_CLI_BINARY=target/debug/crabcode cargo test --test worker_integration
//!
//! # Unix-only platform gate
//!
//! 本文件 13 个 test 全部使用 Unix 字面路径（`/bin/echo` / `/bin/sh` /
//! `/bin/sleep` / `/bin/pwd`）。Windows 上 spawn 这些路径会 ENOENT，
//! worker 返 `exit_code=-1`（与 `worker_command_not_found` 行为一致），
//! 导致 `assert_eq!(exit_code, 0)` 必 fail。加 `#![cfg(unix)]` 让 Windows
//! 编译期跳过整文件。Windows 端 worker 用例由
//! `windows_worker_integration.rs` 使用 `cmd.exe /C echo` 等价命令覆盖。

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use acosmi_sandbox::worker::handle::WorkerHandle;
use acosmi_sandbox::worker::launcher::{WorkerLaunchConfig, launch_worker_unsandboxed};
use acosmi_sandbox::worker::protocol::{WorkerRequest, commands};

/// 尝试启动非沙箱 Worker 并验证其可用性。
/// 如果 CLI 二进制不可用或 Worker 无法响应，返回 None。
fn try_spawn_test_worker() -> Option<WorkerHandle> {
    try_spawn_test_worker_with_config(WorkerLaunchConfig {
        workspace: std::env::temp_dir(),
        default_timeout_secs: 30,
        ..WorkerLaunchConfig::default()
    })
}

fn try_spawn_test_worker_with_config(config: WorkerLaunchConfig) -> Option<WorkerHandle> {
    let mut handle = match launch_worker_unsandboxed(&config) {
        Ok(h) => h,
        Err(e) => {
            eprintln!(
                "SKIP: worker 集成测试需要预编译 CLI 二进制。\n\
                 运行 `cargo build -p acosmi-runtime` 或设置 CRABCODE_CLI_BINARY 环境变量。\n\
                 错误: {e}"
            );
            return None;
        }
    };

    match handle.ping() {
        Ok(()) => Some(handle),
        Err(e) => {
            eprintln!(
                "SKIP: worker 启动成功但无法通信（CLI 二进制可能不支持 sandbox worker-start）。\n\
                 错误: {e}"
            );
            None
        }
    }
}

/// 宏：如果 worker 启动失败（前提不满足），优雅跳过测试。
macro_rules! require_worker {
    () => {
        match try_spawn_test_worker() {
            Some(handle) => handle,
            None => return,
        }
    };
    ($config:expr) => {
        match try_spawn_test_worker_with_config($config) {
            Some(handle) => handle,
            None => return,
        }
    };
}

#[test]
fn worker_ping() {
    let handle = require_worker!();
    // ping 已在 require_worker 中验证通过
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_echo() {
    let mut handle = require_worker!();
    let resp = handle
        .execute("/bin/echo", &["hello", "worker"])
        .expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "hello worker");
    assert!(resp.error.is_none());
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_multiple_commands() {
    let mut handle = require_worker!();

    let r1 = handle
        .execute("/bin/echo", &["first"])
        .expect("exec 1 failed");
    assert_eq!(r1.stdout.trim(), "first");

    let r2 = handle
        .execute("/bin/echo", &["second"])
        .expect("exec 2 failed");
    assert_eq!(r2.stdout.trim(), "second");

    let r3 = handle
        .execute("/bin/echo", &["third"])
        .expect("exec 3 failed");
    assert_eq!(r3.stdout.trim(), "third");

    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_command_not_found() {
    let mut handle = require_worker!();
    let resp = handle
        .execute("/nonexistent/binary_xyz_123", &[])
        .expect("exec should succeed even for missing commands");
    assert_eq!(resp.exit_code, -1);
    assert!(resp.error.as_ref().unwrap().contains("not found"));
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_nonzero_exit() {
    let mut handle = require_worker!();
    let resp = handle
        .execute("/bin/sh", &["-c", "exit 42"])
        .expect("exec failed");
    assert_eq!(resp.exit_code, 42);
    assert!(resp.error.is_none()); // command ran, just non-zero exit
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_stderr_capture() {
    let mut handle = require_worker!();
    let resp = handle
        .execute("/bin/sh", &["-c", "echo err_msg >&2"])
        .expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stderr.trim(), "err_msg");
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_env_vars() {
    let mut handle = require_worker!();

    let req = WorkerRequest {
        id: 1,
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "echo $MY_TEST_VAR".into()],
        env: HashMap::from([("MY_TEST_VAR".into(), "hello_env".into())]),
        cwd: None,
        timeout_secs: None,
    };

    let resp = handle.exec(req).expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "hello_env");
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_custom_cwd() {
    let mut handle = require_worker!();

    let req = WorkerRequest {
        id: 1,
        command: "/bin/pwd".into(),
        args: vec![],
        env: HashMap::new(),
        cwd: Some("/tmp".into()),
        timeout_secs: None,
    };

    let resp = handle.exec(req).expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    // macOS: /tmp → /private/tmp
    assert!(
        resp.stdout.trim() == "/tmp" || resp.stdout.trim() == "/private/tmp",
        "unexpected cwd: {}",
        resp.stdout.trim()
    );
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_shutdown_command() {
    let mut handle = require_worker!();

    // Send shutdown via exec (not the shutdown() method)
    let req = WorkerRequest {
        id: 99,
        command: commands::SHUTDOWN.into(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
        timeout_secs: None,
    };
    let resp = handle.exec(req).expect("shutdown exec failed");
    assert_eq!(resp.id, 99);
    assert_eq!(resp.exit_code, 0);
}

#[test]
fn worker_drop_cleanup() {
    let handle = match try_spawn_test_worker() {
        Some(h) => h,
        None => return,
    };
    let pid = handle.pid();
    assert!(pid.is_some());
    drop(handle);

    // Give the OS a moment to clean up
    std::thread::sleep(std::time::Duration::from_millis(200));

    // On Unix, check the process is gone
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            if let Ok(p) = libc::pid_t::try_from(pid) {
                // SAFETY: Sending signal 0 to check if process exists.
                let exists = unsafe { libc::kill(p, 0) };
                assert_eq!(
                    exists, -1,
                    "worker process (pid {pid}) should not exist after drop"
                );
            }
        }
    }
}

#[test]
fn worker_duration_tracking() {
    let mut handle = require_worker!();

    let resp = handle
        .execute("/bin/sh", &["-c", "sleep 0.1 && echo done"])
        .expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "done");
    assert!(
        resp.duration_ms >= 50,
        "duration_ms should be >= 50, got {}",
        resp.duration_ms
    );

    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_command_timeout() {
    let config = WorkerLaunchConfig {
        workspace: std::env::temp_dir(),
        default_timeout_secs: 2,
        ..WorkerLaunchConfig::default()
    };
    let mut handle = require_worker!(config);

    let start = std::time::Instant::now();
    let resp = handle.execute("/bin/sleep", &["60"]).expect("exec failed");
    let elapsed = start.elapsed();

    assert_eq!(resp.exit_code, -1);
    assert!(resp.error.as_ref().unwrap().contains("timed out"));
    assert!(
        elapsed.as_secs() < 10,
        "should timeout around 2s, took {elapsed:?}"
    );

    handle
        .ping()
        .expect("worker should still be alive after timeout");
    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_per_request_timeout() {
    let mut handle = require_worker!();

    let req = WorkerRequest {
        id: 1,
        command: "/bin/sleep".into(),
        args: vec!["60".into()],
        env: HashMap::new(),
        cwd: None,
        timeout_secs: Some(1),
    };

    let start = std::time::Instant::now();
    let resp = handle.exec(req).expect("exec failed");
    let elapsed = start.elapsed();

    assert_eq!(resp.exit_code, -1);
    assert!(resp.error.as_ref().unwrap().contains("timed out"));
    assert!(elapsed.as_secs() < 5);

    handle.shutdown().expect("shutdown failed");
}

#[test]
fn worker_large_output() {
    let mut handle = require_worker!();

    let resp = handle
        .execute("/bin/sh", &["-c", "yes hello | head -1000"])
        .expect("exec failed");
    assert_eq!(resp.exit_code, 0);
    let lines: Vec<&str> = resp.stdout.lines().collect();
    assert_eq!(lines.len(), 1000);
    assert!(lines.iter().all(|l| *l == "hello"));

    handle.shutdown().expect("shutdown failed");
}
