//! Real detached-exec regression for the Unix launcher.
//!
//! This must exercise the post-fork descriptor cleanup and `execve` path.
//! A mocked child would not detect target-specific missing libc symbols.

#![cfg(unix)]

use std::ffi::OsString;
use std::time::{Duration, Instant};

use acosmi_daemon_launcher::{DetachedCommand, spawn_detached_command};

#[test]
fn detached_command_executes_exact_binary() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let marker = temp.path().join("detached-exec-marker");
    let log = temp.path().join("detached-exec.log");

    spawn_detached_command(&DetachedCommand {
        binary: "/usr/bin/touch".into(),
        args: vec![OsString::from(marker.as_os_str())],
        env_overrides: Vec::new(),
        log_file: Some(log.clone()),
    })
    .expect("handoff detached command");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        marker.exists(),
        "detached child did not exec /usr/bin/touch; log: {}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}
