//! Integration tests for Windows sandbox backend.
//!
//! These tests spawn real sandboxed child processes using Job Objects
//! and Restricted Tokens. They require Windows to run.
//!
//! # Workspaces are always `tempfile::tempdir()`
//!
//! Six of these tests used to pass `std::env::temp_dir()` — the shared `%TEMP%`
//! root — as the sandbox workspace. That is an **unbounded tree that grows with
//! the machine**, and the sandbox hands its workspace to security APIs whose
//! cost scales with it. The result was a suite whose runtime depended on how
//! long the developer's machine had been in use: 7/7 green in May 2026, 6/7 in
//! August with the same code, because `%TEMP%` had crossed ~18k objects in
//! between (2026-08-08 root-cause report, same directory as the SoT).
//!
//! Every test now owns a private empty directory. The one place a *large*
//! workspace still appears is [`sandboxed_spawn_stays_under_the_performance_gate`],
//! which builds a deterministic one on purpose — that is the regression guard,
//! and it needs a tree to be a guard at all.

#![cfg(target_os = "windows")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use acosmi_sandbox::SandboxRunner;
use acosmi_sandbox::config::{
    BackendPreference, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
};
use acosmi_sandbox::platform::WindowsCapabilities;
use acosmi_sandbox::windows::WindowsRunner;
use acosmi_sandbox::windows::exec::{ChildSpec, ChildStdio, run_child};

/// Helper to create a test config with sensible defaults.
fn make_config(
    command: &str,
    args: &[&str],
    security_level: SecurityLevel,
    workspace: &Path,
) -> SandboxConfig {
    SandboxConfig {
        security_level,
        command: command.into(),
        args: args.iter().map(|s| (*s).into()).collect(),
        workspace: workspace.to_path_buf(),
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

// ── 历史：Sprint 7 阶段 2 批量 ignore 标注（2026-04-23，**已解除**）──────────
//
// 下面这段是 `#[ignore]` 当年为什么存在、又凭什么被摘掉的完整实证链。**本文件
// 现在零 `#[ignore]`**，用例全部真跑（2026-08-08 起 windows_integration 全绿）。
// 保留这段是因为它记录了一次被推翻的归因 —— 那条错误归因活了三个月，靠的正是
// "推断写成了结论"这种写法。
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
// 代码定位：`crates/acosmi-sandbox/src/windows/token.rs`（`get_deny_only_sids` /
// `is_required_startup_group_sid` / `create_restricted_token`）。
//
// 最终修法（P3-T2 矩阵实证 2026-05-06：A/B FAIL、C/D PASS）同时保留启动必需
// well-known SID，并让 WindowsFull 默认保 Medium IL，确保 workspace 写入契约
// 也恢复常规运行。
//
// ── W-SANDBOX-ENFORCED-DEADCODE PR-3 补记（2026-08-08）────────────────────────
//
// 上面那条根因管的是**退出码**（能不能跑起来）。Windows 沙箱还有第二条、与它
// 完全无关的病征：**spawn 慢 46 秒**。同样被猜错过一轮（曾归因 LsaLookup），
// PR-3 实测定案真因是 `SetNamedSecurityInfoW` 授予 workspace ACL 时的**全子树
// 继承传播** —— 目录越大越慢，与 token / 注册表无关。修法是探测优先、按需授权
// （已有等价 ACE 就不重写），见 `src/windows/acl.rs` 的 probe-first workflow 与
// `examples/probe_acl_breakdown.rs`（复现旧代价的调查探针，非生产代码）。
//
// 两条根因的共同指纹：**一条未在目标平台实跑就写成结论的推断**。
#[test]
fn echo_hello_in_sandbox() {
    let runner = make_runner();
    if !runner.available() {
        eprintln!("SKIP: Windows sandbox not available");
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let config = make_config(
        "cmd.exe",
        &["/C", "echo hello world"],
        SecurityLevel::L1Allowlist,
        workspace.path(),
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

    let workspace = tempfile::tempdir().unwrap();
    let config = make_config(
        "cmd.exe",
        &["/C", "exit /b 42"],
        SecurityLevel::L1Allowlist,
        workspace.path(),
    );
    let output = runner.run(&config).unwrap();

    assert_eq!(output.exit_code, 42);
}

#[test]
fn command_not_found_returns_error() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let config = make_config(
        "C:\\nonexistent\\binary.exe",
        &[],
        SecurityLevel::L1Allowlist,
        workspace.path(),
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

    let workspace = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "ping -n 60 127.0.0.1 > nul".into()],
        workspace: workspace.path().to_path_buf(),
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

    let start = Instant::now();
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
    // The write is the point of the test — the exit code alone would still pass
    // if `echo` had been redirected into the void.
    assert!(
        output_file.exists(),
        "sandboxed process reported success but wrote nothing to {}\nstdout: {:?}\nstderr: {:?}",
        output_file.display(),
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

    let workspace = tempfile::tempdir().unwrap();
    let mut env = HashMap::new();
    env.insert("MY_TEST_VAR".into(), "test_value_123".into());

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "echo %MY_TEST_VAR%".into()],
        workspace: workspace.path().to_path_buf(),
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
    assert!(
        output.stdout.contains("test_value_123"),
        "env var did not reach the child; stdout: {:?}",
        output.stdout
    );
}

// ── Job Object resource limits ─────────────────────────────────────────────

#[test]
fn job_object_limits_process_count() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    // Try to spawn many processes — Job Object should limit
    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "cmd.exe".into(),
        args: vec!["/C".into(), "echo limited".into()],
        workspace: workspace.path().to_path_buf(),
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

// ── `sandbox-exec` helper relay ────────────────────────────────────────────

#[test]
fn helper_relay_propagates_the_child_exit_code() {
    let runner = make_runner();
    if !runner.available() {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();

    for expected in [0_i32, 7, 42] {
        let code = run_child(&ChildSpec {
            workspace: workspace.path(),
            security_level: SecurityLevel::L1Allowlist,
            program: &comspec(),
            args: &["/C".to_string(), format!("exit /b {expected}")],
            stdio: ChildStdio::Null,
            timeout: Some(Duration::from_secs(30)),
        })
        .unwrap();
        assert_eq!(
            code, expected,
            "the relay must hand back the child's own exit code, not its opinion of it"
        );
    }
}

// ── Performance gate (W-SANDBOX-ENFORCED-DEADCODE PR-3) ────────────────────

/// Number of objects planted in the gate's workspace.
///
/// Sized from the measured propagation cost of ~0.36–0.47 ms/object, paid twice
/// per spawn (grant + revoke): 2000 objects is ~1.5 s of DACL propagation, i.e.
/// **~3× over the budget**, so a regression that reintroduces the unconditional
/// grant fails this test rather than merely slowing it down. Small enough that
/// building the tree costs about a second.
const GATE_WORKSPACE_OBJECTS: usize = 2000;

/// Wall-clock ceiling for one sandboxed spawn.
///
/// Mirrors `acosmi_sandbox::platform::WINDOWS_SPAWN_BUDGET`, deliberately
/// duplicated as a literal: this test is the thing that would catch someone
/// "fixing" a red gate by raising the constant.
///
/// Note the division of labour with the runtime probe. The probe enforces this
/// budget **on the user's machine, at session start, absolutely** — and reports
/// the backend unavailable when it cannot be met, so a slow machine degrades
/// honestly instead of shipping 30-second commands. This test is the
/// **regression gate**: it proves the cost does not scale with the workspace.
const GATE_BUDGET: Duration = Duration::from_millis(500);

/// Samples per measured path.
///
/// Twenty, not seven, so that **p95 is a percentile rather than a synonym for
/// the maximum**. At n=7 the p95 index rounds to the last element, so a single
/// cold spawn fails the gate: measured 2026-08-08 in a full-suite run where
/// building the workspace took 16 s of contended IO and the first sample came in
/// at 1.14 s while samples 3–7 sat at 61–93 ms.
const GATE_SAMPLES: usize = 20;

fn comspec() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}

/// Nearest-rank p95 (and the median, for the failure message).
fn percentiles(samples: &mut [Duration]) -> (Duration, Duration) {
    samples.sort_unstable();
    let last = samples.len() - 1;
    let p95_idx = ((last as f64) * 0.95).round() as usize;
    (samples[last / 2], samples[p95_idx])
}

/// The same command, same workspace, **no sandbox** — the machine's own price
/// for creating a process right now.
///
/// Measured in the same run as the sandboxed samples so that both see the same
/// load, cache state and antivirus mood. Without it the tail assertion is really
/// an assertion about Windows' process-creation jitter, which this PR neither
/// causes nor can fix: on this machine under a full-suite load, individual
/// unsandboxed spawns already range across hundreds of milliseconds.
fn unsandboxed_baseline(workspace: &Path) -> Vec<Duration> {
    use std::process::{Command, Stdio};
    let mut samples = Vec::with_capacity(GATE_SAMPLES);
    for _ in 0..GATE_SAMPLES {
        let started = Instant::now();
        let status = Command::new(comspec())
            .args(["/C", "exit 0"])
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        samples.push(started.elapsed());
        assert!(status.success());
    }
    samples
}

#[test]
fn sandboxed_spawn_stays_under_the_performance_gate() {
    let runner = make_runner();
    if !runner.available() {
        eprintln!("SKIP: Windows sandbox not available");
        return;
    }

    // A workspace with a real tree under it. This is the whole point: an empty
    // directory cannot tell "we skip the DACL write" from "the DACL write got
    // cheap", and only one of those is true.
    let workspace = tempfile::tempdir().unwrap();
    let build_started = Instant::now();
    for i in 0..GATE_WORKSPACE_OBJECTS {
        let sub = workspace.path().join(format!("d{}", i % 50));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("f{i}.txt")), b"x").unwrap();
    }
    eprintln!(
        "gate workspace: {GATE_WORKSPACE_OBJECTS} objects built in {:?}",
        build_started.elapsed()
    );

    // ── The helper relay (`crabcode sandbox-exec` runs exactly this) ─────
    let mut relay: Vec<Duration> = Vec::new();
    for _ in 0..GATE_SAMPLES {
        let started = Instant::now();
        let code = run_child(&ChildSpec {
            workspace: workspace.path(),
            security_level: SecurityLevel::L1Allowlist,
            program: &comspec(),
            args: &["/C".to_string(), "exit 0".to_string()],
            stdio: ChildStdio::Null,
            timeout: Some(Duration::from_secs(60)),
        })
        .unwrap();
        relay.push(started.elapsed());
        assert_eq!(code, 0);
    }
    eprintln!("relay spawn samples: {relay:?}");

    // ── The capturing runner (supervisor path) ──────────────────────────
    let mut capture: Vec<Duration> = Vec::new();
    for _ in 0..GATE_SAMPLES {
        let config = make_config(
            "cmd.exe",
            &["/C", "echo perf-gate"],
            SecurityLevel::L1Allowlist,
            workspace.path(),
        );
        let started = Instant::now();
        let output = runner.run(&config).unwrap();
        capture.push(started.elapsed());
        assert_eq!(output.exit_code, 0);
    }
    eprintln!("capturing spawn samples: {capture:?}");

    // ── The same command with no sandbox at all ─────────────────────────
    let mut baseline = unsandboxed_baseline(workspace.path());
    eprintln!("unsandboxed baseline samples: {baseline:?}");

    let (baseline_median, baseline_p95) = percentiles(&mut baseline);
    let (relay_median, relay_p95) = percentiles(&mut relay);
    let (capture_median, capture_p95) = percentiles(&mut capture);
    eprintln!(
        "baseline: median={baseline_median:?} p95={baseline_p95:?} | \
         relay: median={relay_median:?} p95={relay_p95:?} | \
         capturing: median={capture_median:?} p95={capture_p95:?}"
    );

    // ── Claim 1: the budget itself ──────────────────────────────────────
    //
    // The product promise is per command, so it is stated absolutely. It is
    // stated on the median because that is the robust statistic: the failure
    // this guards (an unconditional DACL write, ~0.4 ms × 2000 objects × 2)
    // costs ~1.4 s on **every** spawn, so it moves the median by ~12× the
    // budget. A lone stall does not.
    for (label, median) in [
        ("helper relay", relay_median),
        ("capturing runner", capture_median),
    ] {
        assert!(
            median <= GATE_BUDGET,
            "{label} spawn median = {median:?} over the {GATE_BUDGET:?} budget on a \
             {GATE_WORKSPACE_OBJECTS}-object workspace — this is the shape of the \
             SetNamedSecurityInfoW subtree propagation coming back (see \
             acosmi-sandbox/src/windows/acl.rs). Do not raise the budget."
        );
    }

    // ── Claim 2: the tail, relative to what the machine charges anyway ──
    //
    // Asserting an absolute p95 here would mostly be asserting that Windows
    // created a process quickly, which this code neither causes nor controls —
    // measured on this machine, *unsandboxed* spawns already scatter across
    // hundreds of milliseconds under load. What is ours is the **difference**,
    // and that is what must stay inside the budget. Same 1.4 s regression fails
    // this one too, by a factor of three.
    let tail_ceiling = baseline_p95 + GATE_BUDGET;
    for (label, p95) in [
        ("helper relay", relay_p95),
        ("capturing runner", capture_p95),
    ] {
        assert!(
            p95 <= tail_ceiling,
            "{label} spawn p95 = {p95:?} exceeds unsandboxed p95 ({baseline_p95:?}) plus the \
             {GATE_BUDGET:?} budget — the sandbox is adding more than its budget to the tail."
        );
    }
}
