//! 并发抢锁：5 个 launcher 同时起，断言只有 1 个能拿到锁。
//!
//! spawn 互斥锁是 daemon spawn 唯一性的根因防御 —— 多 supervisor / 多 CLI
//! 拼一份 `<name>.lock`，第一个胜出后剩余的进 `LockOutcome::Contended` 分支
//! 等 PID 文件，绝不并发 spawn 多个 daemon。
//!
//! W-CRON-RELEASE-REOPEN P0-2（2026-07-16）：此前 `#![cfg(unix)]` + 直接
//! `libc::flock`——Windows 根本没有锁可测。现在改走跨平台
//! `socket_lock::acquire`（Unix flock / Windows 独占共享句柄），同一断言
//! 双平台生效。
//!
//! 注：Windows 实现的互斥单位是**句柄**（进程退出自动放锁），与 flock 的
//! fd 单位一致，线程级并发即可覆盖「同一时刻仅一个 Acquired」的契约。

#![allow(unsafe_code)]

use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;

use acosmi_daemon_launcher::{paths, socket_lock};
use serial_test::serial;

#[test]
#[serial]
fn spawn_lock_serializes_concurrent_acquires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prev = std::env::var_os("CRABCODE_HOME");
    // SAFETY: 单进程串行测试，仅一个 #[test]
    unsafe { std::env::set_var("CRABCODE_HOME", dir.path()) };

    paths::ensure_run_dir().expect("run dir");
    let lock_path = paths::lock_file("cron");

    const N: usize = 5;
    let barrier = Arc::new(Barrier::new(N));
    let acquired = Arc::new(AtomicUsize::new(0));
    let contended = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..N {
        let b = Arc::clone(&barrier);
        let a = Arc::clone(&acquired);
        let c = Arc::clone(&contended);
        let lp = lock_path.clone();
        handles.push(thread::spawn(move || {
            b.wait();
            match socket_lock::acquire(&lp).expect("acquire") {
                socket_lock::LockOutcome::Acquired(_guard) => {
                    a.fetch_add(1, Ordering::SeqCst);
                    // 持锁窗口内其余线程必须 Contended（guard 存活期间锁被持有）。
                    thread::sleep(std::time::Duration::from_millis(50));
                }
                socket_lock::LockOutcome::Contended => {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    match prev {
        Some(v) => unsafe { std::env::set_var("CRABCODE_HOME", v) },
        None => unsafe { std::env::remove_var("CRABCODE_HOME") },
    }

    assert_eq!(acquired.load(Ordering::SeqCst), 1, "应仅 1 个抢到锁");
    assert_eq!(
        contended.load(Ordering::SeqCst),
        N - 1,
        "其余应 Contended（Unix EWOULDBLOCK / Windows ERROR_SHARING_VIOLATION）"
    );
}
