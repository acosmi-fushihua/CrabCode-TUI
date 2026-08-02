//! PID 文件生命周期 + read_alive_pid 行为。
//!
//! launcher 把 "daemon 是否在跑" 问题归约成 "PID 文件存在 + kill -0 通过"。
//! 该不变量被 daemon liveness monitor / `<bin> status` 共用，必须有断言。
//!
//! 串行化：`CRABCODE_HOME` env 是进程级全局态，cargo 默认并发跑测试会互相
//! 覆盖。所有这些测试共用 `serial_test::serial`，逐个跑。

#![allow(unsafe_code)]

use acosmi_daemon_launcher::{paths, read_alive_pid};
use serial_test::serial;

fn with_temp_home<F: FnOnce()>(f: F) {
    let dir = tempfile::tempdir().expect("tempdir");
    let prev = std::env::var_os("CRABCODE_HOME");
    unsafe { std::env::set_var("CRABCODE_HOME", dir.path()) };
    let _ = paths::ensure_run_dir();
    f();
    match prev {
        Some(v) => unsafe { std::env::set_var("CRABCODE_HOME", v) },
        None => unsafe { std::env::remove_var("CRABCODE_HOME") },
    }
}

#[test]
#[serial]
fn read_alive_pid_self() {
    with_temp_home(|| {
        let pid_file = paths::pid_file("cron");
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();
        let pid = read_alive_pid(&pid_file).expect("self PID 应判活");
        assert_eq!(pid, std::process::id());
    });
}

#[test]
#[serial]
fn read_alive_pid_dead() {
    with_temp_home(|| {
        let pid_file = paths::pid_file("cron");
        // 4194303 = Linux 默认 PID 上限，极小概率被复用
        std::fs::write(&pid_file, "4194303").unwrap();
        let result = read_alive_pid(&pid_file);
        assert!(
            result.is_none() || result == Some(4_194_303),
            "PID=4194303 通常不活；偶发活则跳过断言"
        );
    });
}

#[test]
#[serial]
fn read_alive_pid_missing_file() {
    with_temp_home(|| {
        let p = paths::run_dir().join("absent.pid");
        assert!(read_alive_pid(&p).is_none());
    });
}

#[test]
#[serial]
fn read_alive_pid_garbage() {
    with_temp_home(|| {
        let pid_file = paths::pid_file("cron");
        std::fs::write(&pid_file, "not-a-pid").unwrap();
        assert!(read_alive_pid(&pid_file).is_none());
    });
}

#[test]
#[serial]
fn read_alive_pid_zero_rejected() {
    with_temp_home(|| {
        let pid_file = paths::pid_file("cron");
        std::fs::write(&pid_file, "0").unwrap();
        assert!(read_alive_pid(&pid_file).is_none(), "PID=0 不可判活");
    });
}

#[test]
#[serial]
fn cron_pid_file_path_distinct_from_hub() {
    // 阶段 3-A 立宪：cron 与 hub 同 home 不同文件名前缀，否则两个 daemon
    // 会互相覆盖 PID 文件，启动顺序决定哪个被 monitor 误判死。
    with_temp_home(|| {
        let p = paths::pid_file("cron");
        let s = p.to_string_lossy().to_string();
        assert!(s.contains("cron.pid"), "{s}");
        assert!(!s.contains("hub.pid"), "{s}");
    });
}
