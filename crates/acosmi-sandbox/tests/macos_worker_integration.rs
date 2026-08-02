//! Integration tests for macOS Seatbelt-sandboxed persistent Worker.
//!
//! These tests verify that commands executed through the persistent Worker
//! inherit the Seatbelt sandbox constraints. This confirms the Skill 5
//! verification: fork()+exec() children inherit sandbox_init() restrictions.
//!
//! **前提条件**: 需要预先编译 CLI 二进制。
//! 如果 CLI 二进制不可用，测试会优雅跳过（不 panic）。
//!
//! 运行方式:
//!   cargo build -p acosmi-runtime && cargo test --test macos_worker_integration
//!   # 或指定二进制路径:
//!   CRABCODE_CLI_BINARY=target/debug/crabcode cargo test --test macos_worker_integration

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use acosmi_sandbox::config::SecurityLevel;
use acosmi_sandbox::worker::handle::WorkerHandle;
use acosmi_sandbox::worker::launcher::{WorkerLaunchConfig, launch_worker};
use acosmi_sandbox::worker::protocol::WorkerRequest;

/// 尝试启动沙箱 Worker 并验证其可用性。
/// 如果 CLI 二进制不可用或 Worker 无法正常响应，返回 None（调用方应跳过测试）。
///
/// launch_worker 可能返回 Ok（进程被成功 spawn），但实际进程立即退出
/// （例如 current_exe() 返回了测试二进制，不支持 sandbox worker-start）。
/// 因此除了检查 launch 结果，还需要验证 Worker 能响应 ping。
fn try_spawn_sandboxed_worker(
    workspace: std::path::PathBuf,
    security: SecurityLevel,
) -> Option<WorkerHandle> {
    let config = WorkerLaunchConfig {
        security_level: security,
        workspace,
        network_policy: None,
        mounts: vec![],
        default_timeout_secs: 30,
        idle_timeout_secs: 0,
    };

    let mut handle = match launch_worker(&config) {
        Ok(h) => h,
        Err(e) => {
            eprintln!(
                "SKIP: sandbox worker 启动失败（CLI 二进制不可用）。\n\
                 运行 `cargo build -p acosmi-runtime` 或设置 CRABCODE_CLI_BINARY 环境变量。\n\
                 错误: {e}"
            );
            return None;
        }
    };

    // 验证 Worker 实际可用：进程可能已启动但立即退出（参数不识别）。
    // ping 会尝试通过 stdin/stdout 与 Worker 通信，如果 Worker 已退出则 EOF。
    match handle.ping() {
        Ok(()) => Some(handle),
        Err(e) => {
            eprintln!(
                "SKIP: sandbox worker 启动成功但无法通信（CLI 二进制可能不支持 sandbox worker-start）。\n\
                 运行 `cargo build -p acosmi-runtime` 或设置 CRABCODE_CLI_BINARY 环境变量。\n\
                 错误: {e}"
            );
            None
        }
    }
}

/// 宏：如果 worker 启动失败（前提不满足），优雅跳过测试。
macro_rules! require_worker {
    ($workspace:expr, $security:expr) => {
        match try_spawn_sandboxed_worker($workspace, $security) {
            Some(handle) => handle,
            None => return, // 前提条件不满足，跳过
        }
    };
}

// ── Basic sandboxed execution ──────────────────────────────────────────

#[test]
fn sandboxed_worker_echo() {
    let workspace = std::env::temp_dir();
    let mut handle = require_worker!(workspace, SecurityLevel::L1Allowlist);

    let resp = handle
        .execute("/bin/echo", &["hello", "sandboxed"])
        .unwrap();
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "hello sandboxed");
    assert!(resp.error.is_none());

    handle.shutdown().unwrap();
}

#[test]
fn sandboxed_worker_ping() {
    let workspace = std::env::temp_dir();
    let mut handle = require_worker!(workspace, SecurityLevel::L1Allowlist);
    handle.ping().unwrap();
    handle.shutdown().unwrap();
}

#[test]
fn sandboxed_worker_multiple_commands() {
    let workspace = std::env::temp_dir();
    let mut handle = require_worker!(workspace, SecurityLevel::L1Allowlist);

    for i in 0..5 {
        let resp = handle.execute("/bin/echo", &[&format!("cmd-{i}")]).unwrap();
        assert_eq!(resp.exit_code, 0);
        assert_eq!(resp.stdout.trim(), format!("cmd-{i}"));
    }

    handle.shutdown().unwrap();
}

// ── Workspace access (sandboxed) ──────────────────────────────────────

#[test]
fn sandboxed_worker_can_read_workspace() {
    let tmpdir = tempfile::tempdir().unwrap();
    let test_file = tmpdir.path().join("worker-test.txt");
    std::fs::write(&test_file, "worker-content").unwrap();

    let mut handle = require_worker!(tmpdir.path().to_path_buf(), SecurityLevel::L1Allowlist);

    let resp = handle
        .execute("/bin/cat", &[test_file.to_str().unwrap()])
        .unwrap();
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "worker-content");

    handle.shutdown().unwrap();
}

#[test]
fn sandboxed_worker_can_write_workspace() {
    let tmpdir = tempfile::tempdir().unwrap();
    let output_file = tmpdir.path().join("output.txt");

    let mut handle = require_worker!(tmpdir.path().to_path_buf(), SecurityLevel::L1Allowlist);

    let cmd = format!("echo 'written-by-worker' > '{}'", output_file.display());
    let resp = handle.execute("/bin/sh", &["-c", &cmd]).unwrap();
    assert_eq!(resp.exit_code, 0);

    let content = std::fs::read_to_string(&output_file).unwrap();
    assert_eq!(content.trim(), "written-by-worker");

    handle.shutdown().unwrap();
}

// ── Filesystem isolation (sandboxed) ─────────────────────────────────

#[test]
fn sandboxed_worker_cannot_read_outside_workspace_l0() {
    // Create a secret file outside the workspace
    #[allow(clippy::expect_used)]
    let home = std::env::var("HOME").expect("HOME not set");
    let test_dir = std::path::PathBuf::from(&home).join(".crabcode-worker-test");
    std::fs::create_dir_all(&test_dir).unwrap();
    let secret_file = test_dir.join("secret.txt");
    std::fs::write(&secret_file, "top-secret").unwrap();

    // Use a different directory as workspace
    let workspace = tempfile::tempdir().unwrap();
    let mut handle = require_worker!(workspace.path().to_path_buf(), SecurityLevel::L0Deny);

    let resp = handle
        .execute("/bin/cat", &[secret_file.to_str().unwrap()])
        .unwrap();

    // Cleanup
    let _ = std::fs::remove_dir_all(&test_dir);

    // Command should fail — file is outside sandbox
    assert_ne!(
        resp.exit_code,
        0,
        "should not be able to read {}",
        secret_file.display()
    );
}

#[test]
fn sandboxed_worker_cannot_write_outside_workspace_l0() {
    let workspace = tempfile::tempdir().unwrap();

    // Try to write to a location outside workspace
    #[allow(clippy::expect_used)]
    let home = std::env::var("HOME").expect("HOME not set");
    let target = std::path::PathBuf::from(&home).join(".crabcode-worker-test-write");
    // Ensure cleanup
    let _ = std::fs::remove_file(&target);

    let mut handle = require_worker!(workspace.path().to_path_buf(), SecurityLevel::L0Deny);

    let cmd = format!("echo 'escape' > '{}'", target.display());
    let resp = handle.execute("/bin/sh", &["-c", &cmd]).unwrap();

    // Cleanup
    let _ = std::fs::remove_file(&target);

    // Command should fail — cannot write outside sandbox
    assert_ne!(
        resp.exit_code, 0,
        "should not be able to write outside workspace"
    );
}

// ── Multiple isolation checks in one Worker session ──────────────────

#[test]
fn sandboxed_worker_isolation_persists_across_commands() {
    let workspace = tempfile::tempdir().unwrap();
    let workspace_file = workspace.path().join("allowed.txt");
    std::fs::write(&workspace_file, "ok").unwrap();

    #[allow(clippy::expect_used)]
    let home = std::env::var("HOME").expect("HOME not set");
    let forbidden_dir = std::path::PathBuf::from(&home).join(".crabcode-worker-persist-test");
    std::fs::create_dir_all(&forbidden_dir).unwrap();
    let forbidden_file = forbidden_dir.join("forbidden.txt");
    std::fs::write(&forbidden_file, "nope").unwrap();

    let mut handle = require_worker!(workspace.path().to_path_buf(), SecurityLevel::L0Deny);

    // 1. Allowed: read workspace file
    let r1 = handle
        .execute("/bin/cat", &[workspace_file.to_str().unwrap()])
        .unwrap();
    assert_eq!(r1.exit_code, 0);
    assert_eq!(r1.stdout.trim(), "ok");

    // 2. Blocked: read outside workspace
    let r2 = handle
        .execute("/bin/cat", &[forbidden_file.to_str().unwrap()])
        .unwrap();
    assert_ne!(r2.exit_code, 0);

    // 3. Allowed: still works after blocked attempt
    let r3 = handle.execute("/bin/echo", &["still-alive"]).unwrap();
    assert_eq!(r3.exit_code, 0);
    assert_eq!(r3.stdout.trim(), "still-alive");

    // Cleanup
    let _ = std::fs::remove_dir_all(&forbidden_dir);

    handle.shutdown().unwrap();
}

// ── Environment variables in sandboxed Worker ────────────────────────

#[test]
fn sandboxed_worker_env_vars() {
    let workspace = std::env::temp_dir();
    let mut handle = require_worker!(workspace, SecurityLevel::L1Allowlist);

    let req = WorkerRequest {
        id: 1,
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "echo $SANDBOX_VAR".into()],
        env: HashMap::from([("SANDBOX_VAR".into(), "sandbox_value".into())]),
        cwd: None,
        timeout_secs: None,
    };

    let resp = handle.exec(req).unwrap();
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.trim(), "sandbox_value");

    handle.shutdown().unwrap();
}
