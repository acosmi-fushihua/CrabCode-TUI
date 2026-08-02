//! F1 daemon/产物代际握手 (2026-06-11) — `ensure_running` build-id 换代行为。
//!
//! 风格参考 `pid_file_lifecycle.rs`：`CRABCODE_HOME` 指向 tempdir + `serial`
//! 串行化。用真实子进程（`sleep` / TERM-ignoring `sh`）扮演旧代 daemon：
//! 写 pid 文件 + （可选）build-id 文件，然后调 `ensure_running_with` 注入
//! 自定 self build-id 与 stop 超时，断言换代 / 跳过 / fail-open 三类语义。
//!
//! 身份策略注入 `PidIdentityPolicy::TrustPidFile`：假扮 daemon 的是 `sleep`/
//! `sh`，生产的 `Expect(binary)` 身份核实会（正确地）拒认它们——这里测的是
//! build-id 换代语义而非身份层，身份层由 lib.rs 单测独立覆盖。
//!
//! 仅 Unix：Windows 路径共享同一 `needs_generation_swap` 判定 +
//! `swap_stop_old_generation`（soft-stop event + TerminateProcess 兜底），
//! 判定语义由 `build_id::tests` 纯函数单测覆盖。

#![cfg(unix)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use acosmi_daemon_launcher::{PidIdentityPolicy, build_id, ensure_running_with, paths, pid_alive};
use serial_test::serial;

const AUTHORITATIVE_SELF: &str = "9.9.9+aaaaaaaaaaaa";
const AUTHORITATIVE_OTHER: &str = "1.0.0+bbbbbbbbbbbb";
const SWAP_TIMEOUT: Duration = Duration::from_secs(5);

fn with_temp_home<F: FnOnce()>(f: F) {
    let dir = tempfile::tempdir().expect("tempdir");
    let prev = std::env::var_os("CRABCODE_HOME");
    // SAFETY: serial_test 串行化，无并发 env 读写
    unsafe { std::env::set_var("CRABCODE_HOME", dir.path()) };
    let _ = paths::ensure_run_dir();
    f();
    match prev {
        Some(v) => unsafe { std::env::set_var("CRABCODE_HOME", v) },
        None => unsafe { std::env::remove_var("CRABCODE_HOME") },
    }
}

/// 扮演「旧代 daemon」：spawn 长睡眠子进程，写 pid 文件（+ 可选 build-id），
/// 并起 reaper 线程在子进程退出后立即 wait 回收（避免 zombie 让
/// `pid_alive` 假阳性，模拟真实 detached daemon 被 init 即时收养回收）。
fn fake_daemon(name: &str, argv: &[&str], recorded_build_id: Option<&str>) -> (u32, Child) {
    let child = Command::new(argv[0])
        .args(&argv[1..])
        .spawn()
        .expect("spawn fake daemon");
    let pid = child.id();
    std::fs::write(paths::pid_file(name), pid.to_string()).unwrap();
    if let Some(id) = recorded_build_id {
        std::fs::write(build_id::build_id_file(name), id).unwrap();
    }
    (pid, child)
}

fn spawn_reaper(mut child: Child) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = child.wait();
    })
}

fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !pid_alive(pid)
}

#[test]
#[serial]
fn mismatch_triggers_generation_swap() {
    with_temp_home(|| {
        let (pid, child) = fake_daemon("cron", &["sleep", "30"], Some(AUTHORITATIVE_OTHER));
        let reaper = spawn_reaper(child);

        // binary 不存在：换代 stop 成功后走 spawn 路径 → BinaryMissing。
        // 这正好把「stop 被触发且完成」与「重新拉起」两个阶段拆开断言。
        let err = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            AUTHORITATIVE_SELF,
            SWAP_TIMEOUT,
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect_err("stop 成功后 spawn 缺 binary 应报 BinaryMissing");
        assert!(
            matches!(err, acosmi_daemon_launcher::LauncherError::BinaryMissing(_)),
            "实际: {err:?}"
        );
        assert!(
            wait_until_dead(pid, Duration::from_secs(3)),
            "旧代 daemon 应被 SIGTERM 停掉"
        );
        reaper.join().unwrap();
    });
}

#[test]
#[serial]
fn missing_build_id_file_is_treated_as_mismatch() {
    with_temp_home(|| {
        // 老代 daemon 从未写过 build-id 文件 → 视为不一致，换代一次即自愈
        let (pid, child) = fake_daemon("cron", &["sleep", "30"], None);
        let reaper = spawn_reaper(child);

        let err = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            AUTHORITATIVE_SELF,
            SWAP_TIMEOUT,
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect_err("缺 build-id 文件应触发换代（stop 后 BinaryMissing）");
        assert!(
            matches!(err, acosmi_daemon_launcher::LauncherError::BinaryMissing(_)),
            "实际: {err:?}"
        );
        assert!(wait_until_dead(pid, Duration::from_secs(3)));
        reaper.join().unwrap();
    });
}

#[test]
#[serial]
fn unknown_recorded_id_skips_swap() {
    with_temp_home(|| {
        let (pid, mut child) = fake_daemon("cron", &["sleep", "30"], Some("1.0.0+unknown"));

        let handle = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            AUTHORITATIVE_SELF,
            SWAP_TIMEOUT,
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect("unknown 记录应跳过换代，复用旧 daemon");
        assert_eq!(handle.pid, pid);
        assert!(pid_alive(pid), "旧 daemon 不应被停");

        let _ = child.kill();
        let _ = child.wait();
    });
}

#[test]
#[serial]
fn unknown_self_id_skips_swap() {
    with_temp_home(|| {
        let (pid, mut child) = fake_daemon("cron", &["sleep", "30"], Some(AUTHORITATIVE_OTHER));

        let handle = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            "9.9.9+unknown",
            SWAP_TIMEOUT,
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect("自身非权威应跳过换代，复用旧 daemon");
        assert_eq!(handle.pid, pid);
        assert!(pid_alive(pid));

        let _ = child.kill();
        let _ = child.wait();
    });
}

#[test]
#[serial]
fn skip_env_disables_swap() {
    with_temp_home(|| {
        let prev = std::env::var_os(build_id::SKIP_BUILD_HANDSHAKE_ENV);
        // SAFETY: serial_test 串行化
        unsafe { std::env::set_var(build_id::SKIP_BUILD_HANDSHAKE_ENV, "1") };

        let (pid, mut child) = fake_daemon("cron", &["sleep", "30"], Some(AUTHORITATIVE_OTHER));

        let handle = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            AUTHORITATIVE_SELF,
            SWAP_TIMEOUT,
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect("CRABCODE_SKIP_BUILD_HANDSHAKE=1 应跳过换代");
        assert_eq!(handle.pid, pid);
        assert!(pid_alive(pid));

        let _ = child.kill();
        let _ = child.wait();
        match prev {
            Some(v) => unsafe { std::env::set_var(build_id::SKIP_BUILD_HANDSHAKE_ENV, v) },
            None => unsafe { std::env::remove_var(build_id::SKIP_BUILD_HANDSHAKE_ENV) },
        }
    });
}

#[test]
#[serial]
fn swap_stop_timeout_fails_open_to_old_daemon() {
    with_temp_home(|| {
        // TERM-ignoring 旧代 daemon：stop 超时 → fail-open 返回旧 handle
        let (pid, mut child) = fake_daemon(
            "cron",
            &["sh", "-c", "trap '' TERM; while :; do sleep 0.2; done"],
            Some(AUTHORITATIVE_OTHER),
        );

        let handle = ensure_running_with(
            "cron",
            "sock",
            Path::new("/nonexistent/crabcode-cron"),
            AUTHORITATIVE_SELF,
            Duration::from_millis(600),
            &PidIdentityPolicy::TrustPidFile,
        )
        .expect("stop 超时应 fail-open 返回旧 handle（宁可旧不可无）");
        assert_eq!(handle.pid, pid);
        assert!(pid_alive(pid), "fail-open 语义：旧 daemon 仍在");

        let _ = child.kill();
        let _ = child.wait();
    });
}
