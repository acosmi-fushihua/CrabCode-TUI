//! FFI bindings for macOS Seatbelt (Sandbox) private API.
//!
//! These functions are not in Apple's public headers but are available in `libSystem`
//! and used by Chromium, Firefox, and Nix package manager. The kernel-level sandbox
//! enforcement mechanism is stable; only the public CLI (`sandbox-exec`) is deprecated.
//!
//! # References
//!
//! - Chromium: `sandbox/mac/seatbelt.cc` — `sandbox_init_with_parameters`
//! - Apple: `sandbox(7)` man page

use std::ffi::{CStr, CString};
use std::ptr;

use crate::error::SandboxError;

// ── Private Apple API declarations ─────────────────────────────────────────

// SAFETY: These are private Apple API functions available in libSystem.
// They are stable and used by Chromium, Firefox, and other major software.
// The function signatures match Apple's internal headers and Chromium's declarations.
unsafe extern "C" {
    /// Apply a sandbox profile to the current process.
    ///
    /// - `profile`: SBPL string (when `flags` = 0) or named profile
    /// - `flags`: 0 = profile is a string, 1 = named built-in
    /// - `parameters`: null-terminated array of alternating key-value C strings
    ///   `["KEY1", "val1", "KEY2", "val2", NULL]`
    /// - `errorbuf`: on failure, set to an error message (must free with `sandbox_free_error`)
    ///
    /// Returns 0 on success, -1 on failure.
    fn sandbox_init_with_parameters(
        profile: *const libc::c_char,
        flags: u64,
        parameters: *const *const libc::c_char,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;

    /// Free an error buffer returned by `sandbox_init_with_parameters`.
    fn sandbox_free_error(errorbuf: *mut libc::c_char);

    /// Check whether the sandbox policy denies an operation for a given pid.
    ///
    /// 私有 API。在 `sandbox.h` 内部声明但未公开文档化，Chromium sandbox
    /// 测试和其他 security-sensitive 代码广泛使用。返回非 0 = 操作被策略
    /// 拒绝（= 沙箱对该进程生效）；返回 0 = 允许（= 沙箱未生效或 profile
    /// 放行了该 op）。
    ///
    /// ROOT-CAUSE FIX (复审 C-1): 真实 C 签名是 variadic
    /// `int sandbox_check(pid_t, const char *, int, ...)`。SysV x86-64 ABI
    /// 对 variadic 和非 variadic 函数调用约定不同（variadic 要求 `%al` 载
    /// SSE 参数计数；非 variadic 不约束 `%al`）。Rust 非 variadic extern
    /// 生成的调用栈不保证 `%al=0`，在当前 SANDBOX_FILTER_PATH 纯整数参数
    /// 路径下碰巧能跑，但 FFI 声明本身是错的。现在用 Rust stable 的
    /// c_variadic（1.82+）对齐真实签名。
    ///
    /// - `pid`: 目标进程 pid（0 或 getpid() = 当前）
    /// - `operation`: 操作名 C 串，如 `"file-read-data"`、`"file-write*"`
    /// - `filter_type`: filter type flag（见下方常量）
    /// - （可变参数）后续按 filter_type 传入额外参数，如 SANDBOX_FILTER_PATH
    ///   需要一个路径 C 串
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter_type: libc::c_int,
        ...
    ) -> libc::c_int;
}

/// SANDBOX_FILTER_PATH 常量（来自 Apple 的 sandbox.h）。
/// 告诉 `sandbox_check` variadic 尾参是一个文件路径字符串。
pub const SANDBOX_FILTER_PATH: libc::c_int = 1;

/// Canary 检查：对当前进程问 kernel 是否能对特定敏感路径做 "file-write-data"。
///
/// Seatbelt profile 在 L1/L2 下有 `(deny default)`，所以对 `/etc/passwd`
/// 做 write 查询应返回非 0（拒绝）。若返回 0 意味着 sandbox 未激活或
/// profile 意外放行 —— 两种情况都应视为 canary 失败。
///
/// 用于 `platform::verify_sandbox_active` 的 macOS 分支，替代原来的
/// 双路 no-op 弱验证。
pub fn seatbelt_canary_write_denied(path: &str) -> Result<bool, SandboxError> {
    seatbelt_operation_denied(OP_FILE_WRITE_DATA, path)
}

/// `sandbox_check` 的写操作名。
pub const OP_FILE_WRITE_DATA: &str = "file-write-data";
/// `sandbox_check` 的读操作名。
pub const OP_FILE_READ_DATA: &str = "file-read-data";

/// 问 kernel：当前进程对 `path` 做 `operation` 会不会被策略拒绝？
///
/// W-SANDBOX-ENFORCED-DEADCODE PR-2 把它从 canary 专用推广成通用查询：
/// SBPL 的 deny 规则靠「后写的规则赢」压过先前的 allow，而这条**优先级假设
/// 本身**没有编译期证据。所以施加层不信它，改成施加完成后逐条问 kernel
/// ——把一个信念换成一次实测。证不出来就让整条命令失败（125），
/// 绝不放一个「以为挡住了」的沙箱出去。
pub fn seatbelt_operation_denied(operation: &str, path: &str) -> Result<bool, SandboxError> {
    let op = CString::new(operation).map_err(|_| SandboxError::Seatbelt {
        message: "canary op contains null bytes (should never happen)".into(),
    })?;
    let path_c = CString::new(path).map_err(|_| SandboxError::Seatbelt {
        message: "canary path contains null bytes".into(),
    })?;
    // SAFETY: 调 getpid() 总是成功；sandbox_check 对当前 pid 是只读查询，
    // 不会影响进程状态。op/path_c 的 C 串在整个 unsafe 块生存。
    let rc = unsafe {
        sandbox_check(
            libc::getpid(),
            op.as_ptr(),
            SANDBOX_FILTER_PATH,
            path_c.as_ptr(),
        )
    };
    // rc != 0 → 拒绝（沙箱生效）；rc == 0 → 允许（沙箱未生效或放行）
    Ok(rc != 0)
}

// ── Pre-built FFI arguments ────────────────────────────────────────────────

/// Pre-built arguments for `sandbox_init_with_parameters`.
///
/// All `CString` allocations happen at construction time (before `fork()`),
/// so [`apply()`](Self::apply) can be called in a post-fork/pre-exec context
/// without heap allocation.
pub struct SandboxArgs {
    /// The SBPL profile string as a C string.
    profile: CString,

    /// Owned storage for parameter key-value `CString`s.
    /// Must outlive `param_ptrs`.
    _param_storage: Vec<CString>,

    /// Null-terminated array of pointers into `_param_storage`:
    /// `[key0_ptr, val0_ptr, key1_ptr, val1_ptr, ..., NULL]`
    param_ptrs: Vec<*const libc::c_char>,
}

// SAFETY: The raw pointers in `param_ptrs` reference heap data owned by `_param_storage`.
// The entire struct is moved into a `pre_exec` closure and used from a single thread
// (the child process after fork). The pointed-to data does not move on the heap.
unsafe impl Send for SandboxArgs {}
// SAFETY: Same reasoning — no concurrent access occurs. The struct is consumed
// exactly once in the child's pre_exec callback.
unsafe impl Sync for SandboxArgs {}

impl SandboxArgs {
    /// Build `SandboxArgs` from a profile string and parameter key-value pairs.
    ///
    /// All `CString` conversions happen here (on the heap, before fork).
    pub fn new(profile: &str, params: &[(&str, &str)]) -> Result<Self, SandboxError> {
        let profile = CString::new(profile).map_err(|_| SandboxError::Seatbelt {
            message: "SBPL profile contains null bytes".into(),
        })?;

        let mut storage = Vec::with_capacity(params.len() * 2);
        let mut ptrs = Vec::with_capacity(params.len() * 2 + 1);

        for (key, value) in params {
            let key_cs = CString::new(*key).map_err(|_| SandboxError::Seatbelt {
                message: format!("parameter key '{key}' contains null bytes"),
            })?;
            let val_cs = CString::new(*value).map_err(|_| SandboxError::Seatbelt {
                message: format!("parameter value for '{key}' contains null bytes"),
            })?;
            // Collect pointers before pushing to storage to avoid borrow issues.
            // CString data lives on the heap; its pointer survives moves of the CString wrapper.
            ptrs.push(key_cs.as_ptr());
            ptrs.push(val_cs.as_ptr());
            storage.push(key_cs);
            storage.push(val_cs);
        }
        ptrs.push(ptr::null()); // Null terminator

        Ok(Self {
            profile,
            _param_storage: storage,
            param_ptrs: ptrs,
        })
    }

    /// Apply the sandbox profile to the current process.
    ///
    /// This is designed to be called inside a `Command::pre_exec` closure
    /// (after `fork()`, before `exec()`). No heap allocations occur here.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` (compatible with `pre_exec` return type)
    /// if `sandbox_init_with_parameters` fails.
    pub fn apply(&self) -> std::io::Result<()> {
        let mut errorbuf: *mut libc::c_char = ptr::null_mut();

        // SAFETY:
        // - `self.profile` is a valid, non-null CString; pointer is stable (heap-backed).
        // - `self.param_ptrs` is a null-terminated array of valid CString pointers.
        //   The pointed-to data is owned by `self._param_storage` which is alive.
        // - `errorbuf` is a valid output pointer on the stack.
        // - `flags = 0` means the profile is interpreted as an SBPL string.
        // - This function is called exactly once in the child process after fork().
        //   The sandbox, once applied, is irreversible and inherited by exec'd processes.
        let result = unsafe {
            sandbox_init_with_parameters(
                self.profile.as_ptr(),
                0, // profile is a string
                self.param_ptrs.as_ptr(),
                std::ptr::addr_of_mut!(errorbuf),
            )
        };

        if result != 0 {
            let message = if errorbuf.is_null() {
                format!("sandbox_init_with_parameters returned {result}")
            } else {
                // SAFETY: errorbuf was set by sandbox_init_with_parameters to a valid C string.
                let msg = unsafe { CStr::from_ptr(errorbuf) }
                    .to_string_lossy()
                    .into_owned();
                // SAFETY: errorbuf was allocated by the sandbox subsystem; must be freed
                // with sandbox_free_error (not libc free).
                unsafe { sandbox_free_error(errorbuf) };
                msg
            };
            return Err(std::io::Error::other(format!(
                "seatbelt sandbox_init failed: {message}"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_args_creation_valid() {
        let args = SandboxArgs::new(
            "(version 1)(deny default)(import \"bsd.sb\")",
            &[("WORKSPACE_DIR", "/tmp/test")],
        );
        assert!(args.is_ok());
    }

    #[test]
    fn sandbox_args_rejects_null_bytes_in_profile() {
        let args = SandboxArgs::new("profile\0with\0nulls", &[]);
        assert!(args.is_err());
    }

    #[test]
    fn sandbox_args_rejects_null_bytes_in_params() {
        let args = SandboxArgs::new("(version 1)", &[("key\0bad", "value")]);
        assert!(args.is_err());
    }
}
