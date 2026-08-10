//! Windows `sandbox-exec` execution core —— spawn-child relay
//! (W-SANDBOX-ENFORCED-DEADCODE PR-3).
//!
//! Unix helpers `execvp` and *become* the command: pid, stdio, process group and
//! detached semantics all survive untouched, so the host's tree-kill / abort /
//! timeout / output-capture logic keeps working with zero changes. Windows has
//! no `exec`, so the helper has to stay alive as a relay. This module is that
//! relay, and its whole job is to make the relay as close to invisible as the
//! `execvp` it cannot use:
//!
//! - **stdio is handed through, never copied.** The child gets the helper's own
//!   `STD_INPUT/OUTPUT/ERROR` handles. No pipes, no pump thread, no buffering —
//!   the host's fd plumbing (`TaskOutput` files, 64 MB truncation, streaming)
//!   sees exactly the bytes it would have seen without us. This is the one
//!   reason we do not reuse [`WindowsRunner::run`](super::WindowsRunner), which
//!   captures into a `SandboxOutput` because its callers want a value back.
//! - **the exit code is the child's**, propagated verbatim.
//! - **the helper's lifetime is the job's lifetime.** `KILL_ON_JOB_CLOSE` is
//!   already set, so if the host kills the helper — Ctrl+C, tree-kill, turn
//!   abort — the last job handle closes with it and the whole tree dies. No
//!   signal forwarding to get wrong.
//!
//! # Canary
//!
//! Unix verifies the sandbox on *itself* right before `execvp`. Here the sandbox
//! applies to the child, so the equivalent question is "is the child really in
//! the job?" — asked with `IsProcessInJob` while it is still suspended, i.e.
//! before a single instruction of user code has run. A child that is somehow
//! outside the job would be an unrestricted process wearing a sandbox's name;
//! we kill it and fail instead.
//!
//! # What this backend does and does not isolate
//!
//! It isolates by **token and job**: privileges stripped, groups deny-only
//! (minus the ones Win32 startup genuinely needs), Medium IL, resource limits,
//! kill-on-close. It does **not** filter filesystem paths — Windows has no
//! Landlock/Seatbelt equivalent here, so the four `filesystem.*` tables cannot
//! be enforced. That gap is reported as `UNENFORCEABLE(windows)` notices by
//! [`platform::run_exec_plan_as_child`](crate::platform::run_exec_plan_as_child)
//! rather than silently approximated.

#![cfg(target_os = "windows")]

use std::path::Path;
use std::time::Duration;

use tracing::debug;

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};

use super::{acl, build_command_line, job, token};
use crate::config::{MountMode, SecurityLevel};
use crate::error::SandboxError;

/// Where the child's three standard streams point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStdio {
    /// Hand the helper's own std handles to the child (the production path).
    Inherit,
    /// Point all three at `NUL`.
    ///
    /// Used by the capability probe, which must be able to run a real sandboxed
    /// spawn while keeping its own stdout byte-exact: that stdout carries a
    /// single line of JSON the TypeScript side parses, and one stray character
    /// from a probe child would fail-close the whole session into "no sandbox".
    Null,
}

/// One sandboxed child to run.
pub struct ChildSpec<'a> {
    /// Working directory for the child; also the path whose ACL is checked.
    pub workspace: &'a Path,
    pub security_level: SecurityLevel,
    pub program: &'a str,
    pub args: &'a [String],
    pub stdio: ChildStdio,
    /// Wall-clock ceiling, or `None` to wait as long as the child lives.
    ///
    /// Production passes `None` **on purpose**: lifetime belongs to the host
    /// (`Shell.ts` owns abort signal + tree-kill + the turn's own budget), and a
    /// second timeout down here would be a second source of truth for when a
    /// command is allowed to be slow. Only the probe sets it, because a probe
    /// that can hang is worse than no probe.
    pub timeout: Option<Duration>,
}

/// RAII close for handles this module owns.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: handle came from CreateProcess*/CreateFileW; we own it.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Resolve the three handles to hand the child, keeping alive anything we open.
fn child_std_handles(stdio: ChildStdio) -> Result<([HANDLE; 3], Vec<OwnedHandle>), SandboxError> {
    match stdio {
        ChildStdio::Inherit => {
            // SAFETY: GetStdHandle takes a constant and returns a borrowed
            // handle owned by the process — we must not close these.
            let handles: [HANDLE; 3] = unsafe {
                [
                    GetStdHandle(STD_INPUT_HANDLE).unwrap_or_default(),
                    GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default(),
                    GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default(),
                ]
            };
            // A handle we inherited is usually already inheritable, but "usually"
            // is not a contract: mark them explicitly so the child cannot end up
            // with a closed stdout because of how the host happened to spawn us.
            for handle in handles {
                if handle.is_invalid() || handle.0.is_null() {
                    continue;
                }
                // SAFETY: handle is a live std handle of this process.
                unsafe {
                    let _ = SetHandleInformation(
                        handle,
                        HANDLE_FLAG_INHERIT.0,
                        HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0),
                    );
                }
            }
            Ok((handles, Vec::new()))
        }
        ChildStdio::Null => {
            let read = open_nul(false)?;
            let write = open_nul(true)?;
            let write_err = open_nul(true)?;
            let handles = [read.0, write.0, write_err.0];
            Ok((handles, vec![read, write, write_err]))
        }
    }
}

/// Open `NUL` as an inheritable handle.
fn open_nul(writable: bool) -> Result<OwnedHandle, SandboxError> {
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::CreateFileW;

    let name: Vec<u16> = "NUL\0".encode_utf16().collect();
    let sa = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        bInheritHandle: true.into(),
        lpSecurityDescriptor: std::ptr::null_mut(),
    };
    // SAFETY: `name` is a null-terminated wide string; `sa` outlives the call.
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            if writable {
                FILE_GENERIC_WRITE.0
            } else {
                FILE_GENERIC_READ.0
            },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&raw const sa),
            if writable {
                CREATE_ALWAYS
            } else {
                OPEN_EXISTING
            },
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "CreateFileW(NUL)".into(),
            error_code: e.code().0 as u32,
        })?
    };
    Ok(OwnedHandle(handle))
}

/// Run one command inside a fresh restricted token + job object, and return its
/// exit code.
///
/// Blocks until the child exits (or `spec.timeout` elapses). Everything created
/// here — token, job, any temporary ACE — is released before returning.
///
/// # Errors
///
/// Returns [`SandboxError`] if any isolation step fails, if the child cannot be
/// created, or if the canary cannot confirm the child is inside the job. There
/// is deliberately **no** "run it anyway" branch: the caller's contract with the
/// user is that a command either runs sandboxed or does not run.
pub fn run_child(spec: &ChildSpec<'_>) -> Result<i32, SandboxError> {
    // ── 1. Job object first ──────────────────────────────────────────────
    // Created before the process exists so there is no window in which a
    // sandboxed child is alive but unaccounted for.
    let job_guard = job::create_job_object(&crate::config::ResourceLimits::default())?;

    // ── 2. Restricted token ──────────────────────────────────────────────
    let restricted = token::create_restricted_token()?;

    // ── 3. Workspace access — probe first, grant only if truly needed ────
    let workspace_mode = match spec.security_level {
        SecurityLevel::L0Deny => MountMode::ReadOnly,
        SecurityLevel::L1Allowlist | SecurityLevel::L2Sandboxed => MountMode::ReadWrite,
    };
    let user_sid = super::get_token_user_sid(restricted.handle())?;
    let _acl_guard = acl::ensure_workspace_access(
        spec.workspace,
        restricted.handle(),
        user_sid.sid,
        workspace_mode,
    )?;

    // ── 4. Command line + stdio ──────────────────────────────────────────
    let command_line = build_command_line(spec.program, spec.args);
    let mut command_line_wide: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let working_dir: Vec<u16> = spec
        .workspace
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let (std_handles, _owned_stdio) = child_std_handles(spec.stdio)?;

    let mut si = STARTUPINFOW::default();
    si.cb = u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(u32::MAX);
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = std_handles[0];
    si.hStdOutput = std_handles[1];
    si.hStdError = std_handles[2];

    // ── 5. Create suspended ──────────────────────────────────────────────
    // Suspended so the job assignment and the canary both happen before user
    // code runs. Environment is `None`: the child inherits ours, which is how
    // the host's env (and the TMP/TEMP the helper points at the sandbox tmp dir)
    // reaches it — a second env source in the config file would have no correct
    // merge rule against this one.
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: command line and working dir are null-terminated wide strings that
    // outlive the call; `restricted` owns a live token; si/pi are stack locals.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(restricted.handle()),
            None,
            Some(windows::core::PWSTR::from_raw(
                command_line_wide.as_mut_ptr(),
            )),
            None,
            None,
            true,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            None,
            windows::core::PCWSTR(working_dir.as_ptr()),
            &raw const si,
            &raw mut pi,
        )
    };
    created.map_err(|e| {
        if e.code().0 as u32 == 2 {
            SandboxError::CommandNotFound {
                command: spec.program.to_string(),
            }
        } else {
            SandboxError::Win32 {
                operation: format!("CreateProcessAsUserW({})", spec.program),
                error_code: e.code().0 as u32,
            }
        }
    })?;

    let _thread_guard = OwnedHandle(pi.hThread);
    let process_guard = OwnedHandle(pi.hProcess);

    // ── 6. Join the job, then prove it ───────────────────────────────────
    job_guard.assign_process(process_guard.0)?;

    let mut in_job = windows::core::BOOL(0);
    // SAFETY: both handles are live; `in_job` is a stack out-param.
    let probe =
        unsafe { IsProcessInJob(process_guard.0, Some(job_guard.handle()), &raw mut in_job) };
    if probe.is_err() || in_job.0 == 0 {
        // The child is still suspended, so nothing of it has run. Kill it rather
        // than resume something we cannot vouch for.
        let _ = job_guard.terminate(1);
        // SAFETY: process_guard owns a live, suspended process handle.
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(process_guard.0, 1);
        }
        return Err(SandboxError::InvalidConfig {
            message: format!(
                "sandbox canary failed: the child of `{}` is not inside the job object \
                 (IsProcessInJob: {probe:?}) — it would have run unrestricted",
                spec.program
            ),
        });
    }

    // ── 7. Go ────────────────────────────────────────────────────────────
    // SAFETY: pi.hThread is the live primary thread of the suspended child.
    let resume = unsafe { ResumeThread(pi.hThread) };
    if resume == u32::MAX {
        // SAFETY: reading the thread-local last error right after the failure.
        let err_code = unsafe { windows::Win32::Foundation::GetLastError().0 };
        let _ = job_guard.terminate(1);
        return Err(SandboxError::Win32 {
            operation: format!("ResumeThread({})", spec.program),
            error_code: err_code,
        });
    }
    debug!(pid = pi.dwProcessId, "sandboxed child resumed");

    // ── 8. Wait ──────────────────────────────────────────────────────────
    let wait_ms = spec.timeout.map_or(u32::MAX, |d| {
        u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
    });
    // SAFETY: process_guard owns a live process handle.
    let wait = unsafe { WaitForSingleObject(process_guard.0, wait_ms) };
    match wait {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            let _ = job_guard.terminate(1);
            return Err(SandboxError::Timeout {
                timeout_secs: spec.timeout.map_or(0, |d| d.as_secs()),
            });
        }
        other => {
            // SAFETY: reading the thread-local last error right after the failure.
            let err_code = unsafe { windows::Win32::Foundation::GetLastError().0 };
            let _ = job_guard.terminate(1);
            return Err(SandboxError::Win32 {
                operation: format!("WaitForSingleObject(returned 0x{:08x})", other.0),
                error_code: err_code,
            });
        }
    }

    // ── 9. Exit code, then reap whatever the child left behind ───────────
    let mut exit_code: u32 = 0;
    // SAFETY: the process has exited; exit_code is a stack out-param.
    unsafe {
        GetExitCodeProcess(process_guard.0, &raw mut exit_code).map_err(|e| {
            SandboxError::Win32 {
                operation: "GetExitCodeProcess".into(),
                error_code: e.code().0 as u32,
            }
        })?;
    }

    // Grandchildren outlive their parent by default; the job is what makes
    // "the command is done" also mean "its tree is done".
    let _ = job_guard.terminate(exit_code);

    debug!(exit_code, "sandboxed child completed");
    Ok(exit_code as i32)
}
