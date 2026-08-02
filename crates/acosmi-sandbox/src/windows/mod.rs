//! Windows sandbox backend.
//!
//! Provides process isolation using:
//!
//! | Layer | Required? | What it does |
//! |-------|-----------|-------------|
//! | **Job Object** | Yes | Resource limits (memory, CPU, PIDs) + kill-on-close |
//! | **Restricted Token** | Yes* | Stripped privileges, deny-only groups, Medium IL |
//! | **NTFS ACL** | Yes* | Temporary workspace access grant, auto-revoked |
//!
//! *Only with `WindowsFull` backend. `WindowsJobOnly` uses Job Objects alone.
//!
//! # Execution flow
//!
//! ```text
//! WindowsRunner::run(config)
//!        │
//!        ├── 1. Create Job Object (resource limits + KILL_ON_JOB_CLOSE)
//!        ├── 2. Create Restricted Token (strip privileges, Medium IL)
//!        ├── 3. Grant workspace NTFS ACL access to restricted SID
//!        ├── 4. CreateProcessAsUserW with restricted token
//!        ├── 5. AssignProcessToJobObject
//!        ├── 6. ResumeThread (process starts suspended)
//!        ├── 7. Timeout thread + WaitForSingleObject
//!        │
//!        ▼
//!        Drop: AclGuard revokes ACL, JobGuard kills processes
//! ```
//!
//! # Degradation chain
//!
//! ```text
//! RestrictedToken + JobObject  →  JobObject only  →  Docker fallback
//! (full isolation)                (resource limits)
//! ```
//!
//! # Key decisions (from Chromium/Electron analysis)
//!
//! - Default to Restricted Token + Job Object (proven in Chromium renderer)
//! - Process created SUSPENDED to assign Job Object before any code runs
//!
//! # Removed: `AppContainer` (P5.5, 2026-04-23)
//!
//! Previously had `windows/appcontainer.rs` with a full
//! `CreateAppContainerProfile` wrapper, but no callsite in the crate ever
//! used it. Removed to avoid "enticing dead code" —— future `AppContainer`
//! opt-in should implement fresh with explicit capabilities wiring into
//! `WindowsRunner::run`, not resurrect the stub.

pub mod acl;
pub mod async_runner;
pub mod job;
pub mod token;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{PSID, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};

use crate::SandboxRunner;
use crate::config::{MountMode, SandboxConfig, SecurityLevel};
use crate::error::SandboxError;
use crate::output::SandboxOutput;
use crate::platform::{SandboxBackend, WindowsCapabilities};

/// Windows sandbox runner.
pub struct WindowsRunner {
    backend: SandboxBackend,
    capabilities: WindowsCapabilities,
}

impl WindowsRunner {
    /// Create a new Windows runner with the given backend selection and capabilities.
    #[must_use]
    pub const fn new(backend: SandboxBackend, capabilities: WindowsCapabilities) -> Self {
        Self {
            backend,
            capabilities,
        }
    }
}

impl SandboxRunner for WindowsRunner {
    fn name(&self) -> &'static str {
        self.backend.name()
    }

    fn available(&self) -> bool {
        self.capabilities.has_job_objects
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, config: &SandboxConfig) -> Result<SandboxOutput, SandboxError> {
        let start = Instant::now();
        let use_restricted_token = self.backend == SandboxBackend::WindowsFull;

        info!(
            backend = self.name(),
            use_restricted_token,
            command = %config.command,
            "starting sandboxed execution"
        );

        // ── 1. Create Job Object with resource limits ────────────────────
        let job_guard = job::create_job_object(&config.resource_limits)?;

        // ── 2. Create Restricted Token (if full isolation) ───────────────
        let restricted_token = if use_restricted_token {
            Some(token::create_restricted_token()?)
        } else {
            None
        };

        // ── 3. Grant workspace ACL access ────────────────────────────────
        // The restricted token has stripped privileges and deny-only groups, so
        // workspace ACL access is still granted explicitly and then revoked.
        let _acl_guards: Vec<acl::AclGuard> = if use_restricted_token {
            let mut guards = Vec::new();

            // Workspace: read-only for L0, read-write for L1+
            let workspace_mode = match config.security_level {
                SecurityLevel::L0Deny => MountMode::ReadOnly,
                SecurityLevel::L1Allowlist | SecurityLevel::L2Sandboxed => MountMode::ReadWrite,
            };

            // Get the token's user SID for ACL entries.
            // For restricted tokens, we use the original user SID (not the logon SID)
            // because the DACL check uses the enabled SIDs from the token.
            let token_handle = restricted_token
                .as_ref()
                .map_or(HANDLE::default(), token::RestrictedToken::handle);
            let user_sid = get_token_user_sid(token_handle)?;

            guards.push(acl::grant_workspace_access(
                &config.workspace,
                user_sid.sid,
                workspace_mode,
            )?);

            // Additional mounts
            for mount in &config.mounts {
                if mount.host_path.exists() {
                    guards.push(acl::grant_workspace_access(
                        &mount.host_path,
                        user_sid.sid,
                        mount.mode,
                    )?);
                }
            }

            guards
        } else {
            Vec::new()
        };

        // ── 4. Build command line ────────────────────────────────────────
        let command_line = build_command_line(&config.command, &config.args);
        let mut command_line_wide: Vec<u16> = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // S-7 audit fix: When no custom env vars, pass None to inherit parent environment.
        // Previously, empty env_vars produced an empty env block (double-null), which
        // gave the child process NO environment variables at all.
        let env_block = if config.env_vars.is_empty() {
            None
        } else {
            Some(build_environment_block(&config.env_vars))
        };

        // Working directory
        let working_dir: Vec<u16> = config
            .workspace
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // ── 4b. Create stdout/stderr pipes ───────────────────────────────
        // Write ends are inheritable (child process writes to them).
        // Read ends are non-inheritable (parent reads from them after child exits).
        let sa = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
            bInheritHandle: true.into(),
            lpSecurityDescriptor: std::ptr::null_mut(),
        };

        let mut stdout_read = HANDLE::default();
        let mut stdout_write = HANDLE::default();
        let mut stderr_read = HANDLE::default();
        let mut stderr_write = HANDLE::default();

        // SAFETY: CreatePipe with valid pointers and inheritable security attributes.
        unsafe {
            CreatePipe(
                &raw mut stdout_read,
                &raw mut stdout_write,
                Some(&raw const sa),
                0,
            )
            .map_err(|e| SandboxError::Win32 {
                operation: "CreatePipe(stdout)".into(),
                error_code: e.code().0 as u32,
            })?;
            CreatePipe(
                &raw mut stderr_read,
                &raw mut stderr_write,
                Some(&raw const sa),
                0,
            )
            .map_err(|e| {
                let _ = CloseHandle(stdout_read);
                let _ = CloseHandle(stdout_write);
                SandboxError::Win32 {
                    operation: "CreatePipe(stderr)".into(),
                    error_code: e.code().0 as u32,
                }
            })?;

            // Make read ends non-inheritable so child doesn't hold them open.
            let _ = windows::Win32::Foundation::SetHandleInformation(
                stdout_read,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(0),
            );
            let _ = windows::Win32::Foundation::SetHandleInformation(
                stderr_read,
                HANDLE_FLAG_INHERIT.0,
                HANDLE_FLAGS(0),
            );
        }

        // RAII guards for pipe handles — prevents leaks on error paths.
        let _stdout_read_guard = HandleGuard(stdout_read);
        let stdout_write_guard = HandleGuard(stdout_write);
        let _stderr_read_guard = HandleGuard(stderr_read);
        let stderr_write_guard = HandleGuard(stderr_write);

        // ── 5. Create process (suspended) ────────────────────────────────
        let mut si = STARTUPINFOW::default();
        si.cb = u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(u32::MAX);
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdOutput = stdout_write;
        si.hStdError = stderr_write;

        let mut pi = PROCESS_INFORMATION::default();

        let creation_flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;

        // ROOT-CAUSE FIX H7: WindowsJobOnly 时 restricted_token=None，旧实现把
        // HANDLE::default()（NULL）包进 Some 传给 CreateProcessAsUserW，MSDN 对
        // NULL hToken 的行为是"使用调用方 token"，但不同 Windows 版本行为可能不
        // 一致，且违反"受限运行"承诺。现分两条路径：
        //   - 有 restricted_token → CreateProcessAsUserW
        //   - 无 restricted_token → CreateProcessW（明确用父进程 token）
        // 两者都 suspend + JobObject 隔离，只是 token 严格程度不同。
        let env_ptr = env_block
            .as_ref()
            .map(|b| b.as_ptr().cast::<std::ffi::c_void>());
        let create_result: windows::core::Result<()> = if let Some(ref rt) = restricted_token {
            // SAFETY: All strings are valid null-terminated UTF-16.
            // rt.handle() is a valid restricted token from CreateRestrictedToken.
            // CREATE_SUSPENDED ensures the process doesn't run until ResumeThread.
            unsafe {
                CreateProcessAsUserW(
                    Some(rt.handle()),
                    None,
                    Some(windows::core::PWSTR::from_raw(
                        command_line_wide.as_mut_ptr(),
                    )),
                    None,
                    None,
                    true,
                    creation_flags,
                    env_ptr,
                    windows::core::PCWSTR(working_dir.as_ptr()),
                    &raw const si,
                    &raw mut pi,
                )
            }
        } else {
            // JobOnly: 用 CreateProcessW，不涉及 token 参数
            use windows::Win32::System::Threading::CreateProcessW;
            // SAFETY: 同上，只是 API 少一个 token 参数。
            unsafe {
                CreateProcessW(
                    None,
                    Some(windows::core::PWSTR::from_raw(
                        command_line_wide.as_mut_ptr(),
                    )),
                    None,
                    None,
                    true,
                    creation_flags,
                    env_ptr,
                    windows::core::PCWSTR(working_dir.as_ptr()),
                    &raw const si,
                    &raw mut pi,
                )
            }
        };
        create_result.map_err(|e| {
            if e.code().0 as u32 == 2 {
                SandboxError::CommandNotFound {
                    command: config.command.clone(),
                }
            } else {
                let op = if restricted_token.is_some() {
                    format!("CreateProcessAsUserW({})", config.command)
                } else {
                    format!("CreateProcessW({})", config.command)
                };
                SandboxError::Win32 {
                    operation: op,
                    error_code: e.code().0 as u32,
                }
            }
        })?;

        // S-1 audit fix: Wrap both handles in RAII guards immediately.
        // This prevents handle leaks on any error path below.
        let _thread_guard = HandleGuard(pi.hThread);
        let process_guard = HandleGuard(pi.hProcess);

        debug!(
            pid = pi.dwProcessId,
            "sandboxed process created (suspended)"
        );

        // ── 6. Assign to Job Object (before resuming) ────────────────────
        job_guard.assign_process(process_guard.0)?;

        // ── 7. Resume the process ────────────────────────────────────────
        // ROOT-CAUSE FIX H5: ResumeThread 返回 u32 之前的 suspend count；
        // 失败时返回 0xFFFFFFFF (-1 as u32)。旧实现丢返回值 → 进程永远挂起，
        // WaitForSingleObject 卡到用户 timeout 才报错。现在检查返回值，失败
        // 立刻 Terminate 进程并返回错误，调用方立即可感知启动失败。
        // SAFETY: pi.hThread is a valid thread handle from CreateProcess*W.
        // ResumeThread decrements the suspend count; the thread starts running.
        let resume_result = unsafe { ResumeThread(pi.hThread) };
        if resume_result == u32::MAX {
            // 获取 Win32 last error 用于诊断
            let err_code = unsafe { windows::Win32::Foundation::GetLastError().0 };
            // 尝试 Terminate 防止进程悬挂
            unsafe {
                let _ = TerminateProcess(process_guard.0, 1);
            }
            return Err(SandboxError::Win32 {
                operation: format!("ResumeThread({})", config.command),
                error_code: err_code,
            });
        }

        debug!(pid = pi.dwProcessId, "sandboxed process resumed");

        // ── 8. Timeout + wait ────────────────────────────────────────────
        let timeout_ms = config
            .resource_limits
            .timeout_secs
            .map_or(u64::MAX, |s| s.saturating_mul(1000));

        let done = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));

        // S-2 audit fix: Copy the raw handle value for the timeout thread.
        // The HandleGuard on the main thread owns the handle and will close it
        // only after joining the timeout thread, preventing use-after-close.
        let timeout_handle = config.resource_limits.timeout_secs.map(|secs| {
            let done = done.clone();
            let timed_out = timed_out.clone();
            // SAFETY: Process handles are safe to use from another thread.
            // We cast to usize to cross the Send boundary, then reconstruct
            // HANDLE on the other side. The handle is valid until process_guard
            // is dropped (after thread join below).
            let proc_handle_raw = process_guard.0.0 as usize;
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(secs);
                while Instant::now() < deadline {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !done.load(Ordering::SeqCst) {
                    warn!(timeout_secs = secs, "killing timed out process");
                    timed_out.store(true, Ordering::SeqCst);
                    // SAFETY: proc_handle_raw was a valid process handle cast to usize.
                    // TerminateProcess with exit code 1 is safe for our child.
                    unsafe {
                        let _ = TerminateProcess(HANDLE(proc_handle_raw as *mut _), 1);
                    }
                }
            })
        });

        // Wait for process completion
        // SAFETY: process_guard.0 is a valid handle from CreateProcessAsUserW.
        // WaitForSingleObject blocks until the process exits or timeout.
        let wait_result = unsafe {
            WaitForSingleObject(
                process_guard.0,
                u32::try_from(timeout_ms).unwrap_or(u32::MAX),
            )
        };

        done.store(true, Ordering::SeqCst);
        if let Some(handle) = timeout_handle {
            let _ = handle.join();
        }

        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        // ROOT-CAUSE FIX (verify WaitForSingleObject 返回值 2026-04-23):
        //
        // 旧代码 `let wait_result = unsafe { WaitForSingleObject(...) }` 拿到
        // 返回值后**完全不检查** —— Rust 编译器会发出 `unused variable` warning
        // 提醒，配合 Line 61 同时 import 但未用的 `WAIT_OBJECT_0` / `WAIT_TIMEOUT`
        // 一起暴露这里是半成品代码。后果：
        //   - 句柄无效 / 权限错 / 等其他 API 失败 → `wait_result = WAIT_FAILED (0xFFFFFFFF)`
        //     → 代码继续走到 `GetExitCodeProcess` → 拿到的 exit_code 不是真实结果
        //     （可能是 STILL_ACTIVE=259，或因进程已被杀而是 0），静默失败伪装成 "正常退出"
        //   - 外部看不到 Win32 error code，诊断路径断裂
        // 现在显式分派 wait_result：
        //   WAIT_OBJECT_0 → 正常退出，继续 GetExitCodeProcess
        //   WAIT_TIMEOUT  → 交给 timed_out atomic 路径（下方分支处理）
        //   其他          → WAIT_FAILED 或未预期值，拉 GetLastError 返回 Win32 错误
        match wait_result {
            WAIT_OBJECT_0 => {
                // 进程正常退出；继续下方 GetExitCodeProcess
            }
            WAIT_TIMEOUT => {
                timed_out.store(true, Ordering::SeqCst);
                // SAFETY: process_guard owns a valid child process handle.
                unsafe {
                    let _ = TerminateProcess(process_guard.0, 1);
                }
            }
            other => {
                let err = unsafe { windows::Win32::Foundation::GetLastError().0 };
                // 尝试 Terminate 避免进程悬挂
                unsafe {
                    let _ = TerminateProcess(process_guard.0, 1);
                }
                return Err(SandboxError::Win32 {
                    operation: format!(
                        "WaitForSingleObject(returned 0x{:08x}, expected WAIT_OBJECT_0/WAIT_TIMEOUT)",
                        other.0
                    ),
                    error_code: err,
                });
            }
        }

        // Check for timeout
        // S-3 audit fix: No manual CloseHandle — process_guard Drop handles it.
        if timed_out.load(Ordering::SeqCst) {
            let timeout_secs = config.resource_limits.timeout_secs.unwrap_or(0);
            info!(timeout_secs, duration_ms, "process timed out");
            return Err(SandboxError::Timeout { timeout_secs });
        }

        // ── 9. Get exit code ─────────────────────────────────────────────
        let mut exit_code: u32 = 0;
        // SAFETY: process_guard.0 is valid and the process has exited
        // (WaitForSingleObject returned). exit_code receives the value.
        unsafe {
            GetExitCodeProcess(process_guard.0, &raw mut exit_code).map_err(|e| {
                SandboxError::Win32 {
                    operation: "GetExitCodeProcess".into(),
                    error_code: e.code().0 as u32,
                }
            })?;
        }
        // S-3 audit fix: No manual CloseHandle — process_guard Drop handles it.

        info!(exit_code, duration_ms, "sandboxed process completed");

        // ── 10. Read captured stdout/stderr from pipes ───────────────────
        // Close the write ends first so ReadFile will return EOF after
        // all data is consumed (child process has already exited).
        drop(stdout_write_guard);
        drop(stderr_write_guard);

        let stdout = read_pipe_to_string(stdout_read);
        let stderr = read_pipe_to_string(stderr_read);

        Ok(SandboxOutput {
            stdout,
            stderr,
            exit_code: exit_code as i32,
            error: None,
            duration_ms,
            sandbox_backend: self.name().into(),
        })
    }
}

/// Build a Windows command line string from command and arguments.
///
/// Windows command lines require proper quoting for arguments with spaces.
fn build_command_line(command: &str, args: &[String]) -> String {
    let mut cmd = quote_arg(command);
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&quote_arg(arg));
    }
    cmd
}

/// Quote a single command-line argument for Windows.
///
/// If the argument contains spaces, quotes, or is empty, wraps it in double quotes
/// and escapes internal backslashes and quotes per Windows conventions.
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".into();
    }

    if !arg.contains(' ') && !arg.contains('"') && !arg.contains('\t') {
        return arg.into();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');

    let mut backslash_count = 0u32;
    for c in arg.chars() {
        match c {
            '\\' => backslash_count += 1,
            '"' => {
                // Double the backslashes before a quote
                for _ in 0..backslash_count {
                    quoted.push('\\');
                }
                backslash_count = 0;
                quoted.push('\\');
                quoted.push('"');
            }
            _ => {
                backslash_count = 0;
                quoted.push(c);
            }
        }
    }

    // Double backslashes at the end (before closing quote)
    for _ in 0..backslash_count {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

/// Build a Windows environment block (null-separated, double-null terminated).
///
/// Caller should pass `None` to `CreateProcessAsUserW` for empty `env_vars`
/// (to inherit parent environment). This function is only called with non-empty maps.
fn build_environment_block(env_vars: &std::collections::HashMap<String, String>) -> Vec<u16> {
    let mut block = Vec::new();

    for (key, value) in env_vars {
        let entry = format!("{key}={value}");
        block.extend(entry.encode_utf16());
        block.push(0);
    }
    block.push(0); // Double null terminator

    block
}

/// Token user SID extracted from a token.
struct TokenUserSid {
    sid: PSID,
    _buffer: Vec<u8>,
}

/// Get the user SID from a token handle.
fn get_token_user_sid(token: HANDLE) -> Result<TokenUserSid, SandboxError> {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_USER, TokenUser};

    let mut size = 0u32;
    // SAFETY: First call with null buffer to get required size.
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &raw mut size);
    }

    if size == 0 {
        return Err(SandboxError::Win32 {
            operation: "GetTokenInformation(TokenUser) size query".into(),
            error_code: 0,
        });
    }

    let mut buffer = vec![0u8; size as usize];

    // SAFETY: buffer is properly sized. token is a valid token handle.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &raw mut size,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "GetTokenInformation(TokenUser)".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    // SAFETY: GetTokenInformation succeeded and buffer contains TOKEN_USER.
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };

    Ok(TokenUserSid {
        sid: user.User.Sid,
        _buffer: buffer, // Keep buffer alive — SID points into it
    })
}

/// RAII guard that closes a HANDLE on drop.
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Handle is valid — from CreateProcessAsUserW or CreatePipe.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Read all data from a pipe handle and return as a String.
///
/// Reads in 4KB chunks until EOF. Lossy UTF-8 conversion for non-UTF-8 output.
fn read_pipe_to_string(handle: HANDLE) -> String {
    let mut result = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut bytes_read = 0u32;
        // SAFETY: handle is a valid read end of a pipe from CreatePipe.
        // buf is a valid buffer. ReadFile returns FALSE on EOF or error.
        let ok = unsafe { ReadFile(handle, Some(&mut buf), Some(&raw mut bytes_read), None) };
        if ok.is_err() || bytes_read == 0 {
            break;
        }
        result.extend_from_slice(&buf[..bytes_read as usize]);
    }
    String::from_utf8_lossy(&result).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_arg_simple() {
        assert_eq!(quote_arg("hello"), "hello");
    }

    #[test]
    fn test_quote_arg_with_spaces() {
        assert_eq!(quote_arg("hello world"), "\"hello world\"");
    }

    #[test]
    fn test_quote_arg_empty() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn test_quote_arg_with_quotes() {
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn test_build_command_line() {
        let cmd = build_command_line("cmd.exe", &["/C".into(), "echo hello".into()]);
        assert_eq!(cmd, "cmd.exe /C \"echo hello\"");
    }

    #[test]
    fn test_build_environment_block_with_vars() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("FOO".into(), "bar".into());
        let block = build_environment_block(&vars);
        // Should contain "FOO=bar\0\0"
        let expected: Vec<u16> = "FOO=bar"
            .encode_utf16()
            .chain(std::iter::once(0)) // null after entry
            .chain(std::iter::once(0)) // double null terminator
            .collect();
        assert_eq!(block, expected);
    }
}
