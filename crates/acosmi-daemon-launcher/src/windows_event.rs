//! Windows named-event soft-stop signaling channel (R6-T1 §A.2 选 (a))。
//!
//! Windows 没有 SIGTERM 等价物；`stop()` 早先用 `TerminateProcess` 等价 SIGKILL，
//! daemon 进程被立即终止 → R6-T1 graceful 6 步关闭路径在 Windows 下永远不会触发。
//! 本模块提供「软停优先 + 硬杀兜底」机制：
//!
//! - daemon 启动时 `create_event` 拿到一个 manual-reset event handle；
//!   `wait_for_event_or_cancelled` 分片等待 signal 或本地取消。
//! - launcher `stop()` 先 `signal_event_by_name` 把 event signal，让 daemon
//!   走 graceful 路径；超时（pid_file 没清）后才 `TerminateProcess` 兜底。
//!
//! ## Naming
//!
//! event object 名 = `Local\crabcode-<USER>-<daemon>-shutdown`。
//! - `Local\` 私有命名空间：限定到当前 logon session，避免跨 user 冲突。
//! - `<USER>` 走 `USERNAME` env，与 `paths::socket_file` Windows 分支同源；
//!   非法字符同样置 `_`，长度受限于 USERNAME 实际长度（kernel object 名上限
//!   260 chars，对一般用户名足够）。
//!
//! ## 与 unix 路径的对称
//!
//! | 平台 | launcher stop signal | daemon receive |
//! |---|---|---|
//! | unix | `kill(pid, SIGTERM)` | `tokio::signal::unix::signal(SignalKind::terminate())` |
//! | windows | `SetEvent(named event)` | `WaitForSingleObject(event, INFINITE)` (在 spawn_blocking) |
//!
//! 两个平台的 daemon 端最终都把 signal 翻译成 `shutdown_token.cancel()`，
//! 走同一条 R6-T1 6 步关闭路径。

#![cfg(windows)]

use std::io;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::HSTRING;

/// 派生 named event 的完整 object 名。
///
/// 与 daemon 端 `create_event` / launcher 端 `signal_event_by_name` 必须用
/// **同一** name。
pub fn shutdown_event_object_name(daemon_name: &str) -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    let user_safe: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(r"Local\crabcode-{user_safe}-{daemon_name}-shutdown")
}

/// 创建 manual-reset event（initial state = unsignaled），返回 handle。
///
/// daemon 进程**启动时**调用一次；同名 event 已存在则直接返回该 handle（与
/// `OpenEventW` 行为等价，不会冲突）。返回的 handle 拥有 `SYNCHRONIZE` 权限
/// 即可 `WaitForSingleObject`。
///
/// daemon 持有此 handle 直到关闭，无须显式 reset —— shutdown 后整个进程退
/// 出，handle 由 OS 回收；下次 daemon 启动时 launcher 端的旧 event 因为没人
/// 持引用早被 OS 销毁。
pub fn create_event(daemon_name: &str) -> io::Result<HANDLE> {
    let name = shutdown_event_object_name(daemon_name);
    let wname = HSTRING::from(&name);
    // SAFETY: CreateEventW 是普通 Win32 API；name 由我们构造，UTF-16 化后 NUL
    // terminate 由 HSTRING 保证；BOOL/&PCWSTR 都是简单 POD。
    let handle = unsafe { CreateEventW(None, true, false, &wname) }
        .map_err(|err| io::Error::other(format!("CreateEventW({name}): {err}")))?;
    Ok(handle)
}

/// `HANDLE` 在 windows 0.62 内部是 `*mut c_void` → `!Send`（0.52 时是 `isize`，
/// 隐式 Send）。本 wrapper 仅用于把 daemon **自己持有**的 event handle move 进
/// `spawn_blocking` 做**只读** `WaitForSingleObject`。
///
/// SAFETY 论证：event 是内核对象，handle 只是其引用；跨线程 wait 是 Win32 明确
/// 支持的用法（MSDN: object handles 可在线程间传递），且本 handle 在 wait 期间
/// 不被并发 mutate（仅 launcher 端通过**另一个** OpenEventW 拿到的独立 handle 去
/// `SetEvent`）。故跨线程发送安全。
pub struct SendableEventHandle(pub HANDLE);

// SAFETY: 见 `SendableEventHandle` 文档 —— 内核 handle 跨线程只读 wait 安全。
unsafe impl Send for SendableEventHandle {}

impl SendableEventHandle {
    /// 取出内部 `HANDLE`。**消费 `self`** 是关键：Rust 2021 disjoint closure
    /// capture 下，`move || f(sendable.0)` 只捕获 `!Send` 的 `HANDLE` 字段（仍
    /// 触发 E0277）；本方法按值 move 整个 wrapper，强制闭包整体捕获 Send 的
    /// wrapper。
    #[inline]
    pub fn into_handle(self) -> HANDLE {
        self.0
    }
}

/// Wait for a named event in bounded slices until it is signaled or the
/// caller's cancellation predicate becomes true.
///
/// This helper owns and closes the handle. It lets the daemon exit on Ctrl-C
/// or a critical startup/runtime error without leaving an infinite waiter.
pub fn wait_for_event_or_cancelled<F>(
    sendable: SendableEventHandle,
    poll_interval: Duration,
    is_cancelled: F,
) -> io::Result<bool>
where
    F: Fn() -> bool,
{
    let handle = sendable.into_handle();
    let timeout_ms = poll_interval.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
    let result = loop {
        // SAFETY: `handle` is owned by this function and remains open for the
        // entire wait. A bounded timeout makes cancellation observable.
        let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
        if wait == WAIT_OBJECT_0 {
            break Ok(true);
        }
        if wait != WAIT_TIMEOUT {
            break Err(io::Error::other(format!(
                "WaitForSingleObject returned 0x{:08X}",
                wait.0
            )));
        }
        if is_cancelled() {
            break Ok(false);
        }
    };
    // SAFETY: this function exclusively owns the handle after consuming the
    // SendableEventHandle and closes it exactly once after the wait loop.
    let _ = unsafe { CloseHandle(handle) };
    result
}

/// launcher 端：按 daemon name 打开既存 event 并 signal。
///
/// 若 event 不存在（daemon 还没起来 / 已退出）返 `NotFound`，调用方据此判断
/// 是否走 fallback。`Ok(())` 表示已 signal —— 不保证 daemon 一定收到（daemon
/// 可能正在 race 退出），调用方仍需 poll pid_file 状态。
pub fn signal_event_by_name(daemon_name: &str) -> io::Result<()> {
    let name = shutdown_event_object_name(daemon_name);
    let wname = HSTRING::from(&name);
    // SAFETY: 同 create_event；OpenEventW 失败时返 Err。
    // windows 0.62: OpenEventW 形参由 u32 收紧为 SYNCHRONIZATION_ACCESS_RIGHTS，
    // 直接传 EVENT_MODIFY_STATE（不再 `.0` 拆 u32）。
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &wname) }.map_err(|err| {
        // 区分 "event 不存在" vs 其他错误，便于调用方决定 fallback 策略。
        if err.code().0 as u32 == 0x8007_0002 {
            // E_FROM_WIN32(ERROR_FILE_NOT_FOUND)
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("OpenEventW({name}): {err}"),
            )
        } else {
            io::Error::other(format!("OpenEventW({name}): {err}"))
        }
    })?;
    // SAFETY: handle valid 由 OpenEventW 保证；SetEvent 不读不写指针。
    let set_result = unsafe { SetEvent(handle) };
    // SAFETY: handle valid; CloseHandle 仅释放 kernel ref count。
    let _ = unsafe { CloseHandle(handle) };
    set_result.map_err(|err| io::Error::other(format!("SetEvent({name}): {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_event_object_name_uses_local_namespace() {
        // SAFETY: 测试隔离 env
        unsafe { std::env::set_var("USERNAME", "alice") };
        let name = shutdown_event_object_name("cron");
        assert!(name.starts_with(r"Local\crabcode-"), "{name}");
        assert!(name.ends_with("-cron-shutdown"), "{name}");
        assert!(name.contains("alice"), "{name}");
    }

    #[test]
    fn shutdown_event_object_name_sanitizes_username() {
        // SAFETY: 测试隔离 env
        unsafe { std::env::set_var("USERNAME", "bad/user:slash") };
        let name = shutdown_event_object_name("cron");
        // 非法字符 (/, :) 被替换为 _
        assert!(!name.contains('/'), "{name}");
        assert!(
            !name.contains(':') || name.matches(':').count() == 0,
            "{name}"
        );
        assert!(name.contains("bad_user_slash"), "{name}");
    }

    #[test]
    fn create_event_returns_valid_handle_and_signal_round_trips() {
        // SAFETY: 测试隔离 env；用唯一 daemon 名避免 cross-test 干扰
        unsafe { std::env::set_var("USERNAME", "rttest") };
        let name = format!("event-rt-{}", std::process::id());
        let handle = create_event(&name).expect("create_event ok");

        // signal_event_by_name 通过 OpenEvent 连到同 name —— signal 后 wait
        // 应立即返回。
        signal_event_by_name(&name).expect("signal ok");
        assert!(
            wait_for_event_or_cancelled(
                SendableEventHandle(handle),
                Duration::from_millis(10),
                || false
            )
            .expect("wait ok after signal")
        );
    }

    #[test]
    fn signal_event_by_name_unknown_returns_not_found() {
        // SAFETY: 测试隔离 env
        unsafe { std::env::set_var("USERNAME", "missing") };
        let name = format!("event-does-not-exist-{}", std::process::id());
        let err = signal_event_by_name(&name).expect_err("must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }
}
