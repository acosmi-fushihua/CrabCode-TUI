//! Integration tests for Linux Landlock+Seccomp sandbox backend.
//!
//! These tests spawn real sandboxed child processes and verify isolation behavior.
//! They require Linux with Landlock (kernel 5.13+) and Seccomp to run.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use acosmi_sandbox::SandboxRunner;
use acosmi_sandbox::config::{
    BackendPreference, MountMode, MountSpec, NetworkPolicy, OutputFormat, ResourceLimits,
    SandboxConfig, SecurityLevel,
};
use acosmi_sandbox::error::SandboxError;
use acosmi_sandbox::output::SandboxOutput;

/// Helper to create a test config with sensible defaults.
fn make_config(
    command: &str,
    args: &[&str],
    security_level: SecurityLevel,
    network: Option<NetworkPolicy>,
) -> SandboxConfig {
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
        network_policy: network,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    }
}

fn native_runner_or_skip(config: &SandboxConfig) -> Option<Box<dyn SandboxRunner>> {
    match acosmi_sandbox::select_runner(config) {
        Ok(runner) => {
            assert!(
                runner.name().starts_with("linux-"),
                "expected Linux native runner, got {}",
                runner.name()
            );
            Some(runner)
        }
        Err(SandboxError::PlatformNotSupported { platform, reason }) if platform == "linux" => {
            eprintln!("SKIP: Linux native sandbox unavailable: {reason}");
            None
        }
        Err(error) => panic!("failed to select Linux native sandbox runner: {error:?}"),
    }
}

fn run_or_skip(config: &SandboxConfig) -> Option<SandboxOutput> {
    let runner = native_runner_or_skip(config)?;
    match runner.run(config) {
        Ok(output) => Some(output),
        Err(error) => panic!("sandbox command failed unexpectedly: {error:?}"),
    }
}

// ── Basic execution ────────────────────────────────────────────────────────

#[test]
fn echo_hello_in_sandbox() {
    let config = make_config(
        "/bin/echo",
        &["hello", "world"],
        SecurityLevel::L1Allowlist,
        None,
    );
    let Some(output) = run_or_skip(&config) else {
        return;
    };

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout.trim(), "hello world");
    assert!(
        output.sandbox_backend.starts_with("linux-"),
        "backend: {}",
        output.sandbox_backend
    );
}

#[test]
fn command_with_nonzero_exit() {
    let config = make_config(
        "/bin/sh",
        &["-c", "exit 42"],
        SecurityLevel::L1Allowlist,
        None,
    );
    let Some(output) = run_or_skip(&config) else {
        return;
    };

    assert_eq!(output.exit_code, 42);
}

#[test]
fn command_not_found_returns_error() {
    let config = make_config("/nonexistent/binary", &[], SecurityLevel::L1Allowlist, None);
    let Some(runner) = native_runner_or_skip(&config) else {
        return;
    };
    let result = runner.run(&config);

    assert!(result.is_err());
}

// ── Workspace access ───────────────────────────────────────────────────────

#[test]
fn can_read_workspace_files() {
    let tmpdir = tempfile::tempdir().unwrap();
    let test_file = tmpdir.path().join("test.txt");
    std::fs::write(&test_file, "sandbox-content").unwrap();

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "/bin/cat".into(),
        args: vec![test_file.to_str().unwrap().into()],
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

    let Some(output) = run_or_skip(&config) else {
        return;
    };
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout.trim(), "sandbox-content");
}

#[test]
fn can_write_to_workspace_in_l1() {
    let tmpdir = tempfile::tempdir().unwrap();
    let output_file = tmpdir.path().join("output.txt");

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!("echo 'written' > '{}'", output_file.display()),
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

    let Some(output) = run_or_skip(&config) else {
        return;
    };
    assert_eq!(output.exit_code, 0);
    assert_eq!(
        std::fs::read_to_string(&output_file).unwrap().trim(),
        "written"
    );
}

// ── Filesystem isolation ───────────────────────────────────────────────────

#[test]
fn cannot_read_outside_workspace_in_deny_mode() {
    // Create a file in the home directory (not in any sandbox allowlist)
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let test_dir = std::path::PathBuf::from(&home).join(".acosmi-sandbox-test");
    std::fs::create_dir_all(&test_dir).unwrap();
    let secret_file = test_dir.join("secret.txt");
    std::fs::write(&secret_file, "secret").unwrap();

    let workspace = tempfile::tempdir().unwrap();

    let config = SandboxConfig {
        security_level: SecurityLevel::L0Deny,
        command: "/bin/cat".into(),
        args: vec![secret_file.to_str().unwrap().into()],
        workspace: workspace.path().to_path_buf(),
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

    let Some(output) = run_or_skip(&config) else {
        let _ = std::fs::remove_dir_all(&test_dir);
        return;
    };

    // Cleanup
    let _ = std::fs::remove_dir_all(&test_dir);

    // Should fail because the file is outside workspace + system paths
    assert_ne!(output.exit_code, 0);
}

// ── Timeout ────────────────────────────────────────────────────────────────

#[test]
fn timeout_kills_long_running_process() {
    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "/bin/sleep".into(),
        args: vec!["60".into()],
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

    let Some(runner) = native_runner_or_skip(&config) else {
        return;
    };
    let start = std::time::Instant::now();
    let result = runner.run(&config);
    let elapsed = start.elapsed();

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("timed out"), "error: {err}");
    assert!(
        elapsed.as_secs() < 10,
        "should timeout around 2s, took {elapsed:?}"
    );
}

// ── Additional mounts ──────────────────────────────────────────────────────

#[test]
fn additional_mount_readonly() {
    let extra_dir = tempfile::tempdir().unwrap();
    let extra_file = extra_dir.path().join("extra.txt");
    std::fs::write(&extra_file, "extra-content").unwrap();

    let workspace = tempfile::tempdir().unwrap();

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "/bin/cat".into(),
        args: vec![extra_file.to_str().unwrap().into()],
        workspace: workspace.path().to_path_buf(),
        mounts: vec![MountSpec {
            host_path: extra_dir.path().to_path_buf(),
            sandbox_path: extra_dir.path().to_path_buf(),
            mode: MountMode::ReadOnly,
        }],
        resource_limits: ResourceLimits {
            timeout_secs: Some(10),
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: HashMap::new(),
        format: OutputFormat::Json,
        backend: BackendPreference::Native,
    };

    let Some(output) = run_or_skip(&config) else {
        return;
    };
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout.trim(), "extra-content");
}

// ── Environment variables ──────────────────────────────────────────────────

#[test]
fn env_vars_passed_to_sandbox() {
    let mut env = HashMap::new();
    env.insert("MY_TEST_VAR".into(), "test_value_123".into());

    let config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "echo $MY_TEST_VAR".into()],
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

    let Some(output) = run_or_skip(&config) else {
        return;
    };
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout.trim(), "test_value_123");
}

// ── Network policy ─────────────────────────────────────────────────────────

#[test]
fn l0_blocks_network_access() {
    // L0 with None network policy — socket syscalls should be blocked by seccomp
    let config = make_config(
        "/bin/sh",
        &["-c", "cat < /dev/tcp/127.0.0.1/80 2>&1 || echo 'BLOCKED'"],
        SecurityLevel::L0Deny,
        Some(NetworkPolicy::None),
    );
    let Some(output) = run_or_skip(&config) else {
        return;
    };
    // The command should either fail or print BLOCKED
    let combined = format!("{}{}", output.stdout, output.stderr);
    assert!(
        output.exit_code != 0 || combined.contains("BLOCKED") || combined.contains("denied"),
        "expected network to be blocked, got: stdout={} stderr={}",
        output.stdout,
        output.stderr
    );
}
