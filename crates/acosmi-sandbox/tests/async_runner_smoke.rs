//! Phase 1A-T1 smoke：`AsyncSandboxRunner::spawn` 端到端真后端验证（2026-05-06）
//!
//! `acosmi-supervisor::handle_spawn_managed_sandboxed` (ipc.rs:2759) 在 supervisor
//! 收到 `exec.spawnManaged` RPC 且 `sandbox_mode == "enforced"` 时调用
//! `acosmi_sandbox::select_async_runner` + `AsyncSandboxRunner::spawn`。本测试
//! 直接调用底层 API，覆盖：
//!
//! - `select_async_runner` 选 native backend（macOS Seatbelt / Linux Landlock+Seccomp）
//! - `spawn` 返 `SandboxedProcess` 含 stdout / wait / abort
//! - `wait` resolve 到正确 exit code
//! - `abort` 真终止长运行进程
//!
//! Windows 经 `STATUS_DLL_INIT_FAILED` 根因 `try_select` 返 None / Err，
//! 该平台用 `#[cfg(not(target_os = "windows"))]` 跳过（与 ExecBridge.ts:191 注释一致）。

#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::time::Duration;

use acosmi_sandbox::config::{
    BackendPreference, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
};
use acosmi_sandbox::error::SandboxError;
use acosmi_sandbox::{SandboxedProcess, select_async_runner};
use tokio::io::AsyncReadExt;

fn make_config(command: &str, args: &[&str]) -> SandboxConfig {
    SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: command.into(),
        args: args.iter().map(|s| (*s).into()).collect(),
        workspace: std::env::temp_dir(),
        mounts: vec![],
        resource_limits: ResourceLimits {
            timeout_secs: Some(10),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    }
}

fn host_rejected_native_spawn(error: &SandboxError) -> bool {
    matches!(
        error,
        SandboxError::Io { source, .. } if source.kind() == ErrorKind::InvalidInput
    )
}

async fn spawn_or_skip(
    runner: &dyn acosmi_sandbox::AsyncSandboxRunner,
    config: &SandboxConfig,
) -> Option<SandboxedProcess> {
    match runner.spawn(config).await {
        Ok(process) => Some(process),
        Err(error) if host_rejected_native_spawn(&error) => {
            eprintln!("SKIP: native async sandbox spawn rejected by this host: {error}");
            None
        }
        Err(error) => panic!("async sandbox spawn failed unexpectedly: {error:?}"),
    }
}

/// macOS / Linux 真后端 echo smoke：验证 spawn → stdout 抽到 → wait exit 0
#[tokio::test]
async fn async_spawn_echo_returns_zero_and_stdout() {
    let config = make_config("/bin/echo", &["sandbox-smoke"]);
    let runner = match select_async_runner(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP: select_async_runner unavailable on this host: {e}");
            return;
        }
    };
    if !runner.available() {
        eprintln!("SKIP: runner.available() = false on this host");
        return;
    }

    let Some(mut proc) = spawn_or_skip(runner.as_ref(), &config).await else {
        return;
    };

    // sandbox_backend 字段非空且符合平台命名约定
    assert!(
        proc.sandbox_backend.starts_with("macos-") || proc.sandbox_backend.starts_with("linux-"),
        "unexpected backend: {}",
        proc.sandbox_backend
    );
    assert!(proc.pid > 0, "pid should be positive");

    // 读 stdout 直到 EOF（echo 立即关闭）
    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = proc.stdout.take() {
        stdout.read_to_end(&mut stdout_buf).await.unwrap();
    }

    let exit_code = proc.wait.await.unwrap();
    assert_eq!(exit_code, 0);

    let stdout_text = String::from_utf8_lossy(&stdout_buf);
    assert!(
        stdout_text.contains("sandbox-smoke"),
        "stdout missing payload: {stdout_text:?}"
    );
}

/// 验证 nonzero exit code 真透传（sh -c 'exit 7'）
#[tokio::test]
async fn async_spawn_propagates_nonzero_exit() {
    let config = make_config("/bin/sh", &["-c", "exit 7"]);
    let runner = match select_async_runner(&config) {
        Ok(r) => r,
        Err(_) => return,
    };
    if !runner.available() {
        return;
    }

    let Some(proc) = spawn_or_skip(runner.as_ref(), &config).await else {
        return;
    };
    let exit_code = proc.wait.await.unwrap();
    assert_eq!(exit_code, 7);
}

/// **真隔离验证**：sandbox L0Deny + workspace 外的文件应被沙箱拦截读取
///
/// 关键反假阳测试 — 没这条 smoke，前两个测试只能证明 spawn API 形式 OK，
/// 不能区分"沙箱真激活"vs"沙箱 init 失败但子进程仍跑 echo"。本测试在 deny mode
/// 下 cat 一个**位于 workspace 外**的文件，期待 sandbox 真拦截 → exit != 0。
///
/// 跟 macos_integration.rs::cannot_read_outside_workspace_in_deny_mode 同策略，
/// 但走 AsyncSandboxRunner::spawn 真路径（即 supervisor handle_spawn_managed_sandboxed
/// 的真后端入口），覆盖 sync `MacosRunner::run` 触不到的 spawn+wait+stdout 链路。
#[tokio::test]
async fn async_spawn_filesystem_isolation_blocks_external_read() {
    // 受保护文件：放在 HOME/.acosmi-async-smoke/ 不在任何 SBPL 默认 allowlist
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            eprintln!("SKIP: HOME not set");
            return;
        }
    };
    let secret_dir = std::path::PathBuf::from(&home).join(".acosmi-async-smoke");
    let _ = std::fs::create_dir_all(&secret_dir);
    let secret_file = secret_dir.join("secret.txt");
    if std::fs::write(&secret_file, "should-not-leak").is_err() {
        eprintln!("SKIP: cannot prepare secret file");
        return;
    }

    let workspace = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&secret_dir);
            return;
        }
    };

    let mut config = make_config("/bin/cat", &[]);
    config.security_level = SecurityLevel::L0Deny;
    config.workspace = workspace.path().to_path_buf();
    config.network_policy = None;
    config.args = vec![secret_file.to_str().unwrap().into()];

    let runner = match select_async_runner(&config) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&secret_dir);
            eprintln!("SKIP: select_async_runner unavailable: {e}");
            return;
        }
    };
    if !runner.available() {
        let _ = std::fs::remove_dir_all(&secret_dir);
        return;
    }

    let Some(mut proc) = spawn_or_skip(runner.as_ref(), &config).await else {
        let _ = std::fs::remove_dir_all(&secret_dir);
        return;
    };

    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = proc.stdout.take() {
        let _ = stdout.read_to_end(&mut stdout_buf).await;
    }
    let exit = proc.wait.await.unwrap();

    let _ = std::fs::remove_dir_all(&secret_dir);

    // 沙箱真激活 → cat 读不到 → 非 0 exit；secret 内容不应进 stdout
    assert_ne!(
        exit,
        0,
        "L0Deny sandbox should block cat of file outside workspace (got exit=0, stdout={:?})",
        String::from_utf8_lossy(&stdout_buf)
    );
    let stdout_text = String::from_utf8_lossy(&stdout_buf);
    assert!(
        !stdout_text.contains("should-not-leak"),
        "sandbox FAILED to block read; secret leaked to stdout: {stdout_text:?}"
    );
}

/// 验证 abort closure 真杀长运行进程（sleep 60 → abort 后 wait 立即 resolve）
#[tokio::test]
async fn async_spawn_abort_kills_process() {
    let config = make_config("/bin/sleep", &["60"]);
    let runner = match select_async_runner(&config) {
        Ok(r) => r,
        Err(_) => return,
    };
    if !runner.available() {
        return;
    }

    let Some(proc) = spawn_or_skip(runner.as_ref(), &config).await else {
        return;
    };
    let pid = proc.pid;

    // 解构出 abort + wait（owning）
    let abort = proc.abort;
    let wait = proc.wait;

    // 给 sleep 100ms 起飞确保它在 wait 阶段
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 调 abort SIGKILL
    abort().await;

    // wait 必须在合理时间内 resolve（不该永挂；2s 上限远高于真 SIGKILL <100ms）
    let exit = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap_or_else(|_| panic!("wait did not resolve after abort (pid {pid})"))
        .unwrap();

    // SIGKILL 退出码 = -1 (tokio 没 status.code() 时 fallback) 或 137（128 + 9）
    // 不绑死值，只验证非 0（沙箱真 abort 了，没自然 exit 0）
    assert_ne!(exit, 0, "process unexpectedly exited 0 after abort");
}
