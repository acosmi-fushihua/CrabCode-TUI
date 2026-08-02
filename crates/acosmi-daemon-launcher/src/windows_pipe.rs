//! Windows cron Named Pipe server 创建 helper（per-user DACL 收紧）。
//!
//! ## 为什么需要这个 helper
//!
//! Unix cron 使用仅限本用户运行目录的 UDS。Windows Named Pipe 的默认
//! security descriptor 给 Everyone 读 + 给
//! creator/Administrators/SYSTEM 全权 —— 比 UDS 0600 宽。本 helper 把 pipe 的
//! DACL 显式收紧到「SYSTEM / Administrators / 当前用户」三主体（SDDL
//! `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;<user-sid>)`，protected DACL 不继承），
//! 与本地用户隔离语义对齐：同用户必通，异用户（非管理员）必拒。
//!
//! 配合 `reject_remote_clients(true)`（拒绝 SMB 远程 client），等价于
//! `crabcode-cron` 的 Windows IPC 直接复用此 helper。
//!
//! ## unsafe 说明
//!
//! 这是 crate 内少数 unsafe 点：取当前进程 token 用户 SID +
//! SDDL → SECURITY_DESCRIPTOR 转换 + `create_with_security_attributes_raw`，全部
//! 是文档化的 Win32 调用；每个 unsafe 块带逐条 `// SAFETY:` 论证。所有 Win32
//! 分配（StringSid / SECURITY_DESCRIPTOR）由 RAII guard `LocalFree`，错误早退
//! 路径不泄漏。

#![cfg(windows)]
#![allow(unsafe_code)]

use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

/// RAII guard：drop 时 `LocalFree` 一块 Win32 LocalAlloc 内存
/// （`ConvertSidToStringSidW` 的 StringSid / `ConvertStringSecurityDescriptor*`
/// 的 SECURITY_DESCRIPTOR 都由系统 LocalAlloc，调用方负责 LocalFree）。
/// 用 guard 而非手工释放，保证错误早退路径（`?`）也不泄漏。
struct LocalFreeGuard(HLOCAL);

impl Drop for LocalFreeGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: self.0 是 Win32 API LocalAlloc 出来、所有权移交给本 guard
            // 的合法 HLOCAL；只 free 一次（guard 不可 clone）。
            unsafe {
                let _ = LocalFree(Some(self.0));
            }
        }
    }
}

/// RAII guard：drop 时 `CloseHandle` 一个 kernel handle（process token）。
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 是 OpenProcessToken 返回的合法 handle，仅本 guard
            // 持有；CloseHandle 只减 kernel 引用计数。
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// 取当前进程 token 的用户 SID，转 SDDL 字符串形态（如 `S-1-5-21-...`）。
fn current_process_user_sid_string() -> io::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess 返回伪 handle（无需 close）；OpenProcessToken
    // 以 TOKEN_QUERY 打开自身 token，out 参数是合法的 &mut HANDLE。
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|err| io::Error::other(format!("OpenProcessToken: {err}")))?;
    let _token_guard = HandleGuard(token);

    // 两段式查询：先拿所需 buffer 长度（首调预期失败 + 回填 needed），再真查。
    let mut needed: u32 = 0;
    // SAFETY: tokeninformation 传 None / 长度 0 是 GetTokenInformation 文档化的
    // 「探长度」用法，必失败并回填 needed；out 参数为合法 &mut u32。
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::other(
            "GetTokenInformation(TokenUser) length probe returned 0",
        ));
    }

    // 用 u64 backing 保证 buffer 8 字节对齐（TOKEN_USER 内含指针，Vec<u8> 不保证）。
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: buf 容量 ≥ needed 字节且 8 字节对齐；指针在调用期间有效；
    // needed 即首调回填的所需长度。
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|err| io::Error::other(format!("GetTokenInformation(TokenUser): {err}")))?;

    // SAFETY: GetTokenInformation 成功后 buf 起始处是合法 TOKEN_USER 结构
    // （buffer 对齐已由 u64 backing 保证）；只读借用，生命周期不超出 buf。
    let token_user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };

    let mut sid_string = PWSTR::null();
    // SAFETY: token_user.User.Sid 指向 buf 内的合法 SID（与 TOKEN_USER 同
    // buffer，仍存活）；out 参数为合法 &mut PWSTR，成功后由系统 LocalAlloc。
    unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_string) }
        .map_err(|err| io::Error::other(format!("ConvertSidToStringSidW: {err}")))?;
    // StringSid 是 LocalAlloc 内存，guard 负责 LocalFree（含下方 to_string 失败早退）。
    let _sid_guard = LocalFreeGuard(HLOCAL(sid_string.0.cast()));

    // SAFETY: sid_string 是 ConvertSidToStringSidW 刚写入的 NUL 结尾 UTF-16
    // 字符串，仍由 _sid_guard 持有未释放。
    unsafe { sid_string.to_string() }
        .map_err(|err| io::Error::other(format!("SID string is not valid UTF-16: {err}")))
}

/// 创建一个带 per-user DACL 的 Named Pipe server instance。
///
/// - `first_instance = true`：第一个 instance（`FILE_FLAG_FIRST_PIPE_INSTANCE`），
///   pipe 名已被其他进程占用时返 `Err` —— 等价 UDS bind 的 `AddrInUse` 语义；
/// - `first_instance = false`：accept loop 内重建后续 instance。
///
/// 安全属性（对齐 UDS 0600 语义，无 Everyone）：
/// - DACL = SDDL `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;<user-sid>)`
///   （SYSTEM / Administrators / 当前用户全权；`P` = protected，不继承）；
/// - `reject_remote_clients(true)`：拒 SMB 远程 client；
/// - `bInheritHandle = false`：handle 不被子进程继承。
pub fn create_pipe_server(pipe_name: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
    let user_sid = current_process_user_sid_string()?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})");
    let sddl_w = HSTRING::from(sddl.as_str());

    let mut security_descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: sddl_w 是 HSTRING（NUL 结尾 UTF-16，调用期间存活）；out 参数为合法
    // &mut PSECURITY_DESCRIPTOR，成功后由系统 LocalAlloc；size out 参数允许 None。
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl_w,
            SDDL_REVISION_1,
            &mut security_descriptor,
            None,
        )
    }
    .map_err(|err| {
        io::Error::other(format!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW({sddl}): {err}"
        ))
    })?;
    // SECURITY_DESCRIPTOR 是 LocalAlloc 内存；guard 覆盖 create 失败早退路径。
    let _sd_guard = LocalFreeGuard(HLOCAL(security_descriptor.0));

    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| io::Error::other("SECURITY_ATTRIBUTES size exceeds u32"))?,
        lpSecurityDescriptor: security_descriptor.0,
        bInheritHandle: false.into(),
    };

    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true);
    // SAFETY: attrs 指向栈上合法的 SECURITY_ATTRIBUTES，其 lpSecurityDescriptor
    // 指向 _sd_guard 持有（尚未释放）的 SD；CreateNamedPipeW 在调用内拷贝
    // security 信息，不会在返回后继续引用这两块内存。
    unsafe {
        options.create_with_security_attributes_raw(
            pipe_name,
            std::ptr::from_mut(&mut security_attributes).cast(),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;

    fn test_pipe_name(tag: &str) -> String {
        // 进程 pid 后缀防并行测试冲突。
        format!(r"\\.\pipe\crabcode-test-pr1-{tag}-{}", std::process::id())
    }

    /// (a) create 成功 + 同用户 self-connect 必须允许（DACL 含当前用户全权）；
    /// (c) SD 构造成功由本测试间接覆盖（构造失败 create 即 Err）。
    #[tokio::test]
    async fn create_pipe_server_allows_same_user_self_connect() {
        let name = test_pipe_name("selfconnect");
        let server = create_pipe_server(&name, true).expect("first instance creates");

        let _client = ClientOptions::new()
            .open(&name)
            .expect("same-user client must be allowed by per-user DACL");
        server.connect().await.expect("server observes the client");
    }

    /// (b) 双实例竞争：第一个 instance 存活时，第二个 `first_instance=true`
    /// create 必须失败（等价 UDS AddrInUse 语义）。
    #[tokio::test]
    async fn second_first_instance_create_fails_while_pipe_alive() {
        let name = test_pipe_name("dup");
        let _server = create_pipe_server(&name, true).expect("first instance creates");

        create_pipe_server(&name, true)
            .expect_err("second first_instance=true must fail while pipe is alive");
    }

    #[test]
    fn current_process_user_sid_string_looks_like_a_sid() {
        let sid = current_process_user_sid_string().expect("query own token user SID");
        assert!(sid.starts_with("S-1-"), "unexpected SID format: {sid}");
    }
}
