//! crabcode-cron daemon lifecycle e2e（阶段 4 / 立宪 3）。
//!
//! 这一组测试用 **真 crabcode-cron 二进制**（cargo 通过
//! `CARGO_BIN_EXE_crabcode-cron` 暴露 build artifact）+ **沙盒
//! `CRABCODE_HOME` & `CRABCODE_STATE_DIR`**，覆盖独立 cron daemon 的关键
//! lifecycle 不变量：
//!
//! | # | case                                | 防的根因                                       |
//! |---|-------------------------------------|------------------------------------------------|
//! | A | cold_start                          | spawn → PID 文件 → fast-path 二次复用           |
//! | B | kill_minus_9_respawn                | monitor 触发 ensure_cron_running 重建 daemon   |
//! | C | graceful_stop                       | SIGTERM 下 PID/lock 干净清理                    |
//! | D | concurrent_spawn                    | flock 互斥；5 并发只 1 真 spawn                |
//! | E | parent_dies_cron_survives           | double-fork 立宪 3：cron daemon 独立于 spawner  |
//! | F | scheduled_tasks_json_persistence    | kill -9 → respawn 后 .json 仍可见 add 的 job  |
//!
//! ## 沙盒设计
//!
//! - `tempdir/.crabcode/run/`         ← `CRABCODE_HOME` 指向 tempdir 的父，
//!   launcher::paths 把 PID/lock/socket/log 全落到 tempdir/.crabcode/run/
//! - `tempdir/state/cron/`            ← `CRABCODE_STATE_DIR=tempdir/state`，
//!   acosmi-scheduler 把 `scheduled_tasks.json` / `scheduled_tasks.lock`
//!   落到 tempdir/state/cron/（W-CRON-DIR-UNIFY 归一后单层）。
//! - 子进程通过 launcher::spawn_unix execve 时 env 全继承，沙盒 env 完整传
//!   到 daemon。
//!
//! ## 信号注入
//!
//! `nix::sys::signal::kill` 直接走 PID；信号不走 Tokio handler 而是 OS 立刻
//! 投递（kill -9 不可被屏蔽，kill -15 走 daemon 的 `spawn_signal_handler` 注
//! 册的 SIGTERM tokio::signal）。
//!
//! ## 默认 `#[ignore]`
//!
//! 每个用例需要 1-3 秒等 daemon 启动 / 信号传播 / 文件出现。CI 上跑全套
//! workspace 时把 e2e 排除（cargo test 默认跳 `#[ignore]`）；显式跑：
//! `cargo test --test lifecycle_e2e -p crabcode-cron -- --ignored --nocapture`
//!
//! ## 串行化
//!
//! `CRABCODE_HOME` / `CRABCODE_STATE_DIR` 是进程级 env，每个 #[test] 自己拿
//! 一个 tempdir 但仍共享进程 env。`#[serial_test::serial]` 让 cargo 一次只
//! 跑一个 e2e 用例，避免 env 互相污染。

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use acosmi_daemon_launcher::{ensure_running, paths, read_alive_pid, stop};
use serial_test::serial;
use tempfile::TempDir;

/// 路径解析：cargo build 时 `CARGO_BIN_EXE_crabcode-cron` 编译期注入。
fn cron_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crabcode-cron"))
}

/// 沙盒上下文：drop 时自动清理（删 tempdir + 还原 env + best-effort stop daemon）。
///
/// 注意：env 还原必须在 stop_cron 之后；否则 stop_cron 会读到原 env 指向真实
/// `~/.crabcode/run/cron.pid`，意外停掉用户开发机上的 cron。
/// 字段 `_home_dir` / `_state_dir` 仅用于 RAII —— TempDir drop 时 rm -rf
/// tempdir。下划线前缀显式告诉 rustc / 读者：这两个字段不被读取，纯为生命周
/// 期延长服务。
struct Sandbox {
    _home_dir: TempDir,
    _state_dir: TempDir,
    prev_home: Option<std::ffi::OsString>,
    prev_state: Option<std::ffi::OsString>,
}

impl Sandbox {
    fn new() -> Self {
        let home_dir = tempfile::tempdir().expect("home tempdir");
        let state_dir = tempfile::tempdir().expect("state tempdir");

        let prev_home = std::env::var_os("CRABCODE_HOME");
        let prev_state = std::env::var_os("CRABCODE_STATE_DIR");

        // SAFETY: serial_test::serial 保证测试互斥，单线程访问 env。
        unsafe {
            std::env::set_var("CRABCODE_HOME", home_dir.path());
            std::env::set_var("CRABCODE_STATE_DIR", state_dir.path());
        }

        let _ = paths::ensure_run_dir();
        Self {
            _home_dir: home_dir,
            _state_dir: state_dir,
            prev_home,
            prev_state,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // best-effort 停掉沙盒里残留的 daemon —— 必须在还原 env 之前完成，
        // 否则 paths::pid_file("cron") 会指回真实 ~/.crabcode/run/cron.pid。
        let _ = stop("cron", Duration::from_secs(3));

        // SAFETY: 同 new()，serial_test 保证互斥。
        unsafe {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("CRABCODE_HOME", v),
                None => std::env::remove_var("CRABCODE_HOME"),
            }
            match self.prev_state.take() {
                Some(v) => std::env::set_var("CRABCODE_STATE_DIR", v),
                None => std::env::remove_var("CRABCODE_STATE_DIR"),
            }
        }
        // tempdir 在字段 drop 时自动 rm -rf
    }
}

/// 走 cron daemon UDS 发一条 newline-delimited JSON 请求并读响应。
fn cron_request(method: &str, params: serde_json::Value) -> serde_json::Value {
    let sock_path = paths::socket_file("cron");
    let mut stream = UnixStream::connect(&sock_path)
        .unwrap_or_else(|e| panic!("UDS connect {}: {e}", sock_path.display()));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut params = match params {
        serde_json::Value::Object(object) => object,
        serde_json::Value::Null => serde_json::Map::new(),
        other => panic!("cron params must be object/null, got {other}"),
    };
    params.insert(
        "expectedStateIdentity".to_string(),
        serde_json::Value::String(
            acosmi_daemon_launcher::state_identity::resolve_cron_state_identity()
                .expect("expected state identity"),
        ),
    );
    let req = serde_json::json!({
        "id": "e2e",
        "method": method,
        "params": params,
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).expect("write request");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("parse response {line:?}: {e}"))
}

/// 阻塞等条件成立或超时。
fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration, label: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("等待 {label} 超时（{timeout:?}）");
}

/// 把 cron daemon 拉起来，断成功后返回 PID。
fn ensure_cron_or_panic() -> u32 {
    let sock = paths::socket_file("cron").to_string_lossy().into_owned();
    let bin = cron_binary();
    assert!(bin.exists(), "build artifact 必须存在: {}", bin.display());
    let handle = ensure_running("cron", &sock, &bin)
        .unwrap_or_else(|e| panic!("ensure_cron_running 失败: {e}"));
    handle.pid
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 A — cold_start
// ════════════════════════════════════════════════════════════════════════════

/// 冷启动 → PID 文件出现 → 二次 ensure 走 fast-path 不重 spawn。
///
/// 防的根因：launcher fast-path 必须以 PID 文件为唯一真值；如果第二次调用
/// 又重 spawn 一遍，会撞 scheduler 锁导致 daemon 自杀（exit 11）。
#[test]
#[serial]
#[ignore = "e2e: 真 daemon spawn，需 1-2s"]
fn a_cold_start() {
    let _sb = Sandbox::new();

    let pid_first = ensure_cron_or_panic();
    assert!(pid_first > 1, "PID 应有效（非 init/swapper）: {pid_first}");

    // PID 文件已经写入并通过 kill -0
    let pid_from_file = read_alive_pid(&paths::pid_file("cron")).expect("PID 文件应活");
    assert_eq!(pid_from_file, pid_first);

    // 二次 ensure → fast path，不重 spawn
    let pid_second = ensure_cron_or_panic();
    assert_eq!(pid_first, pid_second, "二次 ensure 必须复用同一 daemon");

    // 业务请求往返
    let resp = cron_request("cron.status", serde_json::Value::Null);
    assert_eq!(resp["result"]["status"], "ok", "{resp}");
    assert_eq!(resp["result"]["job_count"], 0);
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 B — kill -9 → respawn
// ════════════════════════════════════════════════════════════════════════════

/// kill -9 daemon 后，再次 ensure_cron_running 必须重建 daemon（新 PID）。
///
/// 防的根因：模拟 CronLivenessMonitor 探测到死 PID 后调 ensure_cron_running
/// 的路径。launcher 必须看到 read_alive_pid → None（因为旧 PID 已死）→ 抢锁
/// → 重 spawn → 新 PID。如果 launcher 误判旧 PID 还活（read_alive_pid bug），
/// monitor 会一直空转，永远不重建 cron daemon。
#[test]
#[serial]
#[ignore = "e2e: kill -9 + respawn，需 2-3s"]
fn b_kill_minus_9_respawn() {
    let _sb = Sandbox::new();

    let old_pid = ensure_cron_or_panic();
    kill_minus_9(old_pid);

    // wait 直到 read_alive_pid 看到 None（旧 PID 真死了）
    wait_until(
        || read_alive_pid(&paths::pid_file("cron")).is_none(),
        Duration::from_secs(3),
        "kill -9 后 read_alive_pid → None",
    );

    // 模拟 CronLivenessMonitor 重拉
    let new_pid = ensure_cron_or_panic();
    assert_ne!(old_pid, new_pid, "respawn 必须产生新 PID");

    // 新 daemon 业务可用
    let resp = cron_request("cron.status", serde_json::Value::Null);
    assert_eq!(resp["result"]["status"], "ok", "{resp}");
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 C — graceful_stop
// ════════════════════════════════════════════════════════════════════════════

/// SIGTERM 下 daemon 干净退：PID 文件清 + scheduler.lock 释放。
///
/// 防的根因：daemon 的 cleanup_runtime_files 必须 unlink PID 文件 +
/// release_lock；否则下次 ensure_cron_running 看到陈旧 PID/锁会判活失败 +
/// scheduler 启动期 try_acquire_lock 失败拒启（exit 11）。
#[test]
#[serial]
#[ignore = "e2e: SIGTERM + 等清理，需 1-2s"]
fn c_graceful_stop() {
    let _sb = Sandbox::new();

    let pid = ensure_cron_or_panic();
    assert!(pid > 1);

    let stopped = stop("cron", Duration::from_secs(5)).expect("stop_cron returned Ok");
    assert!(stopped, "stop_cron 应返回 true（5s 内退完）");

    assert!(
        !paths::pid_file("cron").exists(),
        "PID 文件应被清掉: {}",
        paths::pid_file("cron").display()
    );

    // scheduler 主锁文件路径走 cron/scheduled_tasks.lock（W-CRON-DIR-UNIFY 归一后
    // 单层）；resolve_state_dir 已被 CRABCODE_STATE_DIR 指向沙盒。
    let scheduler_lock = scheduler_lock_path();
    assert!(
        !scheduler_lock.exists(),
        "scheduler.lock 应释放: {}",
        scheduler_lock.display()
    );
}

fn scheduler_lock_path() -> PathBuf {
    acosmi_config::paths::resolve_state_dir().join("cron/scheduled_tasks.lock")
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 D — concurrent_spawn
// ════════════════════════════════════════════════════════════════════════════

/// 5 并发 ensure_cron_running 调用，仅 1 个真 spawn；其余走 contended 等
/// PID 文件。所有调用最终拿到同一 PID。
///
/// 防的根因：launcher::ensure_cron_running 内的 socket_lock::acquire 必须
/// flock(LOCK_EX | LOCK_NB) 串行化；如果两个进程同时认为 daemon 不在 → 都
/// spawn → 第二个 daemon 启动时 try_acquire_lock 失败自杀（exit 11），
/// PID 文件被反复覆盖，业务请求乱序。
#[test]
#[serial]
#[ignore = "e2e: 5 并发 spawn，需 2-3s"]
fn d_concurrent_spawn() {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    let _sb = Sandbox::new();

    const N: usize = 5;
    let barrier = Arc::new(Barrier::new(N));
    let pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::with_capacity(N)));

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let b = Arc::clone(&barrier);
        let pids = Arc::clone(&pids);
        let bin = cron_binary();
        let sock = paths::socket_file("cron").to_string_lossy().into_owned();
        handles.push(thread::spawn(move || {
            b.wait();
            let h = ensure_running("cron", &sock, &bin)
                .unwrap_or_else(|e| panic!("ensure_cron_running 并发失败: {e}"));
            pids.lock().unwrap().push(h.pid);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let pids = pids.lock().unwrap();
    assert_eq!(pids.len(), N);
    let first = pids[0];
    for (i, p) in pids.iter().enumerate() {
        assert_eq!(*p, first, "并发线程 #{i} PID={p} 与 #0 PID={first} 不一致");
    }

    // 业务一致性：所有线程拿到同一 daemon
    let resp = cron_request("cron.status", serde_json::Value::Null);
    assert_eq!(resp["result"]["status"], "ok", "{resp}");
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 E — parent_dies_cron_survives
// ════════════════════════════════════════════════════════════════════════════

/// cron daemon 的 ppid 应该是 1（init/launchd）—— double-fork 后被 init 收
/// 养，不依附于 spawner（test 进程）。spawner 死掉 cron 仍活。
///
/// 防的根因：原 supervisor-as-parent 模式下 cron 是 supervisor 子进程，
/// supervisor crash → cron 跟着退；立宪 3 阶段 1 改双 fork + setsid 后必须
/// 有此不变量。这里直接断 ppid==1 ——比模拟一个 fake supervisor 进程然后
/// kill 它更稳，因为不需要 race。
#[test]
#[serial]
#[ignore = "e2e: spawn + 读 ppid"]
fn e_parent_dies_cron_survives() {
    let _sb = Sandbox::new();

    let pid = ensure_cron_or_panic();
    let ppid = read_ppid(pid).expect("能读 ppid");

    // 立宪 3 的核心不变量：cron daemon 的父进程**绝不能**是当前 test 进程
    // —— 那是"supervisor-as-parent"旧模式，违反双 fork + setsid 设计。
    //
    // ppid==1 是常态（launchd / init / systemd），但容器/PID namespace 下
    // PID 1 可能是 tini 之类的 init 替代品；某些 sandboxed CI runner（如
    // sysbox / podman rootless）甚至可能让 reparent 目标 != 1。
    // 因此放宽断言为 "ppid != 当前测试进程"，更鲁棒；ppid==1 只在 native
    // shell 下做软断言，CI 容器下跳过。
    assert_ne!(
        ppid,
        std::process::id(),
        "cron daemon 不应是 test 进程的子进程；double-fork 后必须被 init 收养（实际 ppid={ppid}）"
    );
    if std::env::var_os("CI").is_none() {
        assert_eq!(
            ppid, 1,
            "cron daemon 在本地 shell 下 ppid 应该是 init/launchd (1)，实际 {ppid}"
        );
    }

    // 验证 cron 仍活、仍可服务
    let resp = cron_request("cron.status", serde_json::Value::Null);
    assert_eq!(resp["result"]["status"], "ok", "{resp}");
}

// ════════════════════════════════════════════════════════════════════════════
//  测试用例 F — scheduled_tasks.json 持久化跨 respawn
// ════════════════════════════════════════════════════════════════════════════

/// cron.add → kill -9 → ensure_cron_running 重拉 → cron.list 仍能看到 job。
///
/// 防的根因：daemon 重启不能丢 schedule 数据。`scheduled_tasks.json` 是
/// 唯一真值；daemon 内 in-memory state 在 respawn 时必须从文件 hydrate。
/// 如果 task_store 写入路径或读取路径不一致（如一边走 CRABCODE_STATE_DIR
/// 一边硬编码 ~/.crabcode），respawn 后看不到 add 的 job。
#[test]
#[serial]
#[ignore = "e2e: add → kill -9 → respawn → list，需 3-5s"]
fn f_scheduled_tasks_json_persistence() {
    let _sb = Sandbox::new();

    let pid_a = ensure_cron_or_panic();

    // 加一个最小可用 cron job：systemEvent payload + cron expr 每 5 分钟
    let add_resp = cron_request(
        "cron.add",
        serde_json::json!({
            "name": "e2e-persist",
            "schedule": {
                "kind": "cron",
                "expr": "*/5 * * * *"
            },
            "payload": {
                "kind": "systemEvent",
                "text": "e2e f-test"
            }
        }),
    );
    assert_eq!(add_resp["result"]["status"], "ok", "add 失败: {add_resp}");
    let job_id = add_resp["result"]["id"].as_str().expect("id").to_string();
    assert!(!job_id.is_empty());

    // .json 文件应已落盘（sched daemon 异步 flush，给 1s 余量）
    let tasks_json = acosmi_config::paths::resolve_state_dir().join("cron/scheduled_tasks.json");
    wait_until(
        || tasks_json.exists(),
        Duration::from_secs(2),
        "scheduled_tasks.json 落盘",
    );

    // kill -9 模拟硬崩
    kill_minus_9(pid_a);
    wait_until(
        || read_alive_pid(&paths::pid_file("cron")).is_none(),
        Duration::from_secs(3),
        "kill -9 后 read_alive_pid → None",
    );

    // 重拉 daemon
    let pid_b = ensure_cron_or_panic();
    assert_ne!(pid_a, pid_b);

    // list 必须仍能看到 add 的 job（hydrate from disk 不变量）
    let list_resp = cron_request("cron.list", serde_json::json!({"includeDisabled": true}));
    assert_eq!(
        list_resp["result"]["status"], "ok",
        "list 失败: {list_resp}"
    );
    let jobs = list_resp["result"]["jobs"].as_array().expect("jobs array");
    let found = jobs
        .iter()
        .any(|j| j["id"].as_str() == Some(job_id.as_str()));
    assert!(
        found,
        "respawn 后 list 应仍包含 id={job_id}; 实际 jobs={jobs:?}"
    );
}

// ════════════════════════════════════════════════════════════════════════════
//  辅助：Unix 信号注入 / proc 表读取
// ════════════════════════════════════════════════════════════════════════════

fn kill_minus_9(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Some(Signal::SIGKILL));
}

/// 读 PID 的 ppid。优先 `/proc/<pid>/stat`（Linux），失败回落 `ps -o ppid=
/// -p <pid>`（macOS / Linux 都可）。
fn read_ppid(pid: u32) -> Option<u32> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // /proc/<pid>/stat 字段 4 是 ppid；但 comm（字段 2）可能含空格
        // 必须先按 ')' 分割再 split
        if let Some(rparen) = stat.rfind(')') {
            let after = &stat[rparen + 1..];
            let fields: Vec<&str> = after.split_whitespace().collect();
            // 现在 fields[0]=state, fields[1]=ppid
            if let Some(ppid_str) = fields.get(1) {
                if let Ok(ppid) = ppid_str.parse::<u32>() {
                    return Some(ppid);
                }
            }
        }
    }
    // macOS 路径：ps -o ppid= -p <pid>
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<u32>().ok()
}
