//! C2 startup barrier regression tests.
//!
//! A daemon must not publish `cron.pid` before its IPC listener is bound. If
//! the first bind fails, the foreground process must exit non-zero and clean
//! the scheduler lock instead of remaining alive as a PID-only "ready" daemon.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

fn cron_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crabcode-cron"))
}

fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait cron child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn first_listener_bind_failure_exits_nonzero_without_publishing_pid() {
    let home = tempfile::tempdir().expect("runtime home tempdir");
    let state = tempfile::tempdir().expect("state tempdir");
    let runtime_root = acosmi_daemon_launcher::paths::resolve_home_dir(
        None,
        Some(home.path().as_os_str()),
        home.path(),
    );
    let run_dir = runtime_root.join("run");
    std::fs::create_dir_all(&run_dir).expect("create run dir");

    // A directory at the UDS path cannot be removed by remove_file() and
    // makes UnixListener::bind fail deterministically.
    let socket_path = run_dir.join("cron.sock");
    std::fs::create_dir(&socket_path).expect("occupy cron.sock with directory");

    let mut child = Command::new(cron_binary())
        .arg("serve")
        .env("CRABCODE_HOME", home.path())
        .env("CRABCODE_STATE_DIR", state.path())
        .env_remove("CRABCODE_CONFIG_DIR")
        .env_remove("CRABCODE_PROFILE")
        .spawn()
        .expect("spawn foreground crabcode-cron");

    let status = match wait_for_exit(&mut child, Duration::from_secs(3)) {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("IPC bind failure left a live PID-only cron daemon");
        }
    };

    assert!(!status.success(), "startup bind failure must be non-zero");
    assert!(
        !run_dir.join("cron.pid").exists(),
        "PID is a ready marker and must never be published on bind failure"
    );
    assert!(
        !state.path().join("cron/scheduled_tasks.lock").exists(),
        "startup failure must release the scheduler state lock"
    );
}
