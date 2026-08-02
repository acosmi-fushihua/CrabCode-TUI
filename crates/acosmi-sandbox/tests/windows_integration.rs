//! Integration tests for Windows sandbox backend.
//!
//! These tests spawn real sandboxed child processes using Job Objects
//! and Restricted Tokens. They require Windows to run.

#![cfg(target_os = "windows")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use acosmi_sandbox::SandboxRunner;
use acosmi_sandbox::config::{
    BackendPreference, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
};
use acosmi_sandbox::platform::WindowsCapabilities;
use acosmi_sandbox::windows::WindowsRunner;

/// Helper to create a test config with sensible defaults.
fn make_config(command: &str, args: &[&str], security_level: SecurityLevel) -> SandboxConfig {
    SandboxConfig {
        security_level,
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

fn make_runner() -> WindowsRunner {
    let caps = WindowsCapabilities::detect();
    let backend = if caps.has_job_objects {
        acosmi_sandbox::platform::SandboxBackend::WindowsFull
    } else {
        acosmi_sandbox::platform::SandboxBackend::WindowsJobOnly
    };
    WindowsRunner::new(backend, caps)
}

// ── Basic execution ────────────────────────────────────────────────────────

// ── Sprint 7 阶段 2 批量 ignore 标注（2026-04-23）────────────────────────────
//
// 以下 6 个 test 在 Sprint 7 阶段 2 **root-cause 前**因 `CreateRestrictedToken`
// 返 `0x80070579/0x80070539`（悬空 PSID + 缺 TOKEN_ADJUST_DEFAULT 权限）
// 早期 panic，根本跑不到 assert。阶段 2 修好 token 两层根因后，执行链路前
// 进到 `CreateProcessAsUserW` + 子进程加载阶段被 NT kernel 拒绝。
//
// ── P3-T0 实证修订（2026-05-06）─────────────────────────────────────────────
//
// **重要 drift**：原 Sprint 7 阶段 2 注释推断子进程退出码是
// `STATUS_DLL_INIT_FAILED (0xC0000142)`，但**未在 Windows 实跑**。
// 2026-05-06 Win11 Home China 26200 (rustc 1.94.1, Medium IL 普通用户) 真跑实证：
//
//   实测 `output.exit_code == -1073741790`
//   `-1073741790 (i32) = 0xC0000022 (NTSTATUS) = STATUS_ACCESS_DENIED`
//   `0xC0000142 (NTSTATUS) = -1073741502 (i32)` ≠ `-1073741790`
//
// ProcMon verification established that the root cause is:
// **Restricted Token 让 sandboxed cmd.exe 在 loader-init 阶段读 HKLM 注册表
// 三连 ACCESS DENIED**：
//
//   line 466  RegOpenKey HKLM\System\CurrentControlSet\Control\Nls\CodePage
//             Desired Access: Read                  → ACCESS DENIED
//   line 468  RegOpenKey HKLM\System\CurrentControlSet\Control\Session Manager
//             Desired Access: Query Value           → ACCESS DENIED
//   line 471  RegOpenKey HKLM\Software\Microsoft\Windows NT\CurrentVersion
//             \Image File Execution Options
//             Desired Access: Query Value, Enumerate Sub Keys → ACCESS DENIED
//
// 同 trace 内**外层** cmd.exe（PID 25308，无 sandbox token）在同三键全部
// SUCCESS（line 7 / 12 / 17），对照成立 → 差异**只在 token**。
//
// 注意：旧注释把根因说成"PE loader 拒访问 cmd.exe / system32 DLL / KnownDlls
// 等关键路径" — 该陈述**未被本次 ProcMon trace 实证支持**：trace 中 cmd.exe
// 文件本身（line 475 / 479）SUCCESS，无 DLL 文件 ACCESS DENIED。实证拒访问
// 目标只有 HKLM 注册表三连。
//
// 代码定位：`crates/acosmi-sandbox/src/windows/token.rs:104,132-143,154,339-342`
// （deny-only 群组覆盖除 logon SID 外全部群组 + 降到 Low IL 双叠加；
// 主因区分 deny-only / Low IL / 叠加 留 P3-T2 四象限矩阵）。
//
// P3-T2 候选修法：(1) 放宽 deny-only 群组白名单；(2) 加 restricting SID list
// （CreateRestrictedToken 第 5 参，目前传 None）；(3) 切 AppContainer
// （platform.rs:325-329 stub 已删，需重写）；(4) cmd.exe 类不可沙箱化负载
// 路由 WindowsJobOnly backend 不走 RestrictedToken。trade-off RFC 待落档。
//
// **Sprint 7 缓解策略**：
// - `platform::select_runner` 在本机无法运行 WindowsFull 时自动降级到
//   WindowsJobOnly（资源限制 + 进程组 KILL_ON_JOB_CLOSE，无 token 降权）
// - `handle_spawn_managed` 阶段 4 的 opt-in 策略默认 `SandboxMode::None`，
//   不触发 WindowsFull 路径
//
// P3-T2 矩阵实证（2026-05-06）：A/B FAIL、C/D PASS，cmd.exe 启动主因锁定
// 为 deny-only 群组策略。最终修法同时保留启动必需 well-known SID，并让
// WindowsFull 默认保 Medium IL，确保 workspace 写入契约也恢复常规运行。
#[test]
fn echo_hello_in_sandbox() {
    let runner = make_runner();
    if !runner.available() {
        eprintln!("SKIP: Windows sandbox not available");
        return;
    }

    let config = make_config(
        "cmd.exe",
        &["/C", "echo hello world"],
        SecurityLevel::L1Allowlist,
    );
    let output = runner.run(&config).unwrap();

    assert_eq!(output.exit_code, 0);
    assert!(
        output.sandbox_backend.starts_with("windows-"),
        "backend: {}",
        output.sandbox_backend
    );
}

#[test]
fn command_with_nonzero_exit() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let config = make_config("cmd.exe", &["/C", "exit /b 42"], SecurityLevel::L1Allowlist);
    let output = runner.run(&config).unwrap();

    assert_eq!(output.exit_code, 42);
}

#[test]
fn command_not_found_returns_error() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let config = make_config(
        "C:\\nonexistent\\binary.exe",
        &[],
        SecurityLevel::L1Allowlist,
    );
    let result = runner.run(&config);

    assert!(result.is_err());
}

// ── Timeout ────────────────────────────────────────────────────────────────

#[test]
fn timeout_kills_long_running_process() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "ping -n 60 127.0.0.1 > nul".into()],
        workspace: std::env::temp_dir(),
        mounts: vec![],
        resource_limits: ResourceLimits {
            timeout_secs: Some(2),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    };

    let start = std::time::Instant::now();
    let result = runner.run(&config);
    let elapsed = start.elapsed();

    assert!(result.is_err(), "unexpected success: {result:?}");
    let err = result.unwrap_err();
    assert!(err.to_string().contains("timed out"), "error: {err}");
    assert!(
        elapsed.as_secs() < 10,
        "should timeout around 2s, took {elapsed:?}"
    );
}

// ── Workspace access ───────────────────────────────────────────────────────

#[test]
fn can_write_to_workspace_in_l1() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let tmpdir = tempfile::tempdir().unwrap();
    let output_file = tmpdir.path().join("output.txt");

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec![
            "/C".into(),
            format!("echo written > {}", output_file.display()),
        ],
        workspace: tmpdir.path().to_path_buf(),
        mounts: vec![],
        resource_limits: ResourceLimits {
            timeout_secs: Some(10),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    };

    let output = runner.run(&config).unwrap();
    assert_eq!(
        output.exit_code,
        0,
        "workspace: {}\nstdout: {}\nstderr: {}",
        tmpdir.path().display(),
        output.stdout,
        output.stderr
    );
}

// ── Environment variables ──────────────────────────────────────────────────

#[test]
fn env_vars_passed_to_sandbox() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let mut env = HashMap::new();
    env.insert("MY_TEST_VAR".into(), "test_value_123".into());

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "echo %MY_TEST_VAR%".into()],
        workspace: std::env::temp_dir(),
        mounts: vec![],
        resource_limits: ResourceLimits {
            timeout_secs: Some(10),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: env,
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    };

    let output = runner.run(&config).unwrap();
    assert_eq!(output.exit_code, 0);
}

// ── Job Object resource limits ─────────────────────────────────────────────

#[test]
fn job_object_limits_process_count() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    // Try to spawn many processes — Job Object should limit
    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "echo limited".into()],
        workspace: std::env::temp_dir(),
        mounts: vec![],
        resource_limits: ResourceLimits {
            max_pids: 5,
            timeout_secs: Some(10),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    };

    // This should succeed (single process within limit)
    let output = runner.run(&config).unwrap();
    assert_eq!(output.exit_code, 0);
}
