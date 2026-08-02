//! NTFS ACL management for workspace access control.
//!
//! Grants temporary file system access to the restricted sandbox process,
//! then revokes it on cleanup via RAII [`AclGuard`].
//!
//! # Workflow
//!
//! ```text
//! 1. GetNamedSecurityInfoW  →  get current DACL
//! 2. SetEntriesInAclW       →  add temporary ACE for sandbox SID
//! 3. SetNamedSecurityInfoW  →  apply modified DACL
//! 4. (on Drop)              →  restore original DACL
//! ```
//!
//! This ensures the restricted token process can access its workspace
//! without granting permanent filesystem permissions.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::PSID;
use windows::Win32::Security::{
    ACL as SEC_ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};

use crate::config::MountMode;
use crate::error::SandboxError;

/// File access rights for read-only access.
const READ_ONLY_ACCESS: u32 = 0x0012_0089; // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE

/// File access rights for read-write access.
const READ_WRITE_ACCESS: u32 = 0x001F_01FF; // FILE_ALL_ACCESS

/// RAII guard that restores the original DACL when dropped.
///
/// ROOT-CAUSE FIX H8: 原实现 Drop 里 `SetNamedSecurityInfoW` 失败只打 warn，
/// 调用方无从感知文件系统 ACL 残留（宽松 ACE 会留在磁盘直到下次手动清理，
/// 沙箱边界静默泄漏）。现在：
///   - 新增显式 `revoke()` consume self，能返回错误让调用方处理失败
///   - Drop 作为 last-resort fallback：若调用方已 revoke 则跳过；否则尝试
///     恢复并在失败时至少写一个 `<path>.acl-recovery-needed` 标记文件，
///     让下次启动能检测到残留并尝试清理
///   - 新增 `revoke_count` 字段防止 Drop 和 revoke 重复恢复
pub struct AclGuard {
    path: PathBuf,
    original_dacl: *mut SEC_ACL,
    original_descriptor: PSECURITY_DESCRIPTOR,
    revoked: bool,
}

impl AclGuard {
    /// Get the protected path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 显式恢复原 DACL 并释放资源。失败返回错误（调用方决定重试/告警）。
    /// 消费 self 避免 Drop 再次执行。
    ///
    /// ROOT-CAUSE FIX (P1-1 2026-04-23): 之前写成
    /// ```ignore
    /// self.revoked = true;
    /// self.do_revoke()   // do_revoke 首行 `if self.revoked return Ok(())`
    /// ```
    /// 结果 `do_revoke` 永远早退，**`SetNamedSecurityInfoW` 从未执行** —— `revoke()`
    /// 返回 Ok 但 DACL 根本没恢复，宽松 ACE 永久留盘。方向搞反了。
    ///
    /// 现在的状态机：
    ///   1. 若已 revoked（幂等调用）立刻返回 Ok；
    ///   2. 否则真正执行 `do_revoke（纯操作，不检查状态`）；
    ///   3. 无论成功失败都置 revoked=true，防止 Drop 再次 `SetNamedSecurityInfoW`。
    /// `do_revoke` 自身从此是纯函数：只做恢复调用，不判断 guard。Drop 分支
    /// 独立用 `!self.revoked` 判断是否介入。
    pub fn revoke(mut self) -> Result<(), SandboxError> {
        if self.revoked {
            return Ok(());
        }
        let r = self.do_revoke();
        // 无论 r 是 Ok 还是 Err 都标记 —— 显式承诺"revoke() 调用过就由调用方处理失败，
        // Drop 不再二次尝试"。状态与返回值语义一致。
        self.revoked = true;

        // ROOT-CAUSE FIX (深度复核 P1 2026-04-23): revoke() Err 时也要写
        // recovery marker，对齐 Drop 的失败兜底语义。否则调用方若 `let _ = g.revoke()`
        // 忽略返回值 —— DACL 未恢复、无磁盘 marker、下次启动无从清理，沙箱边界
        // 静默泄漏。Drop 的失败路径一直有 marker；revoke() 的失败路径之前没有。
        if let Err(ref e) = r {
            write_acl_recovery_marker(&self.path, e);
        }
        r
    }

    /// 执行 `SetNamedSecurityInfoW` 恢复原 DACL。纯操作，不检查状态；
    /// 调用方（revoke / Drop）负责保证"每个 guard 最多调用一次"。
    fn do_revoke(&mut self) -> Result<(), SandboxError> {
        let path_wide = to_wide_string(&self.path);
        // SAFETY: path_wide is a valid null-terminated wide string.
        // original_dacl points to the DACL from the original GetNamedSecurityInfoW call
        // (owned by original_descriptor). SetNamedSecurityInfoW restores it.
        let result = unsafe {
            SetNamedSecurityInfoW(
                windows::core::PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(self.original_dacl),
                None,
            )
        };
        if result.0 != 0 {
            return Err(SandboxError::Win32 {
                operation: format!(
                    "SetNamedSecurityInfoW(restore DACL for {})",
                    self.path.display()
                ),
                error_code: result.0,
            });
        }
        debug!(path = %self.path.display(), "original DACL restored");
        Ok(())
    }
}

impl Drop for AclGuard {
    fn drop(&mut self) {
        // 若调用方已通过 revoke() 显式恢复，跳过（防止二次 SetNamedSecurityInfoW）。
        if !self.revoked {
            if let Err(e) = self.do_revoke() {
                // 失败路径：没法传回错误，但要留下磁盘痕迹让启动时能检测到。
                // 这是"ACL 残留泄漏"的 last-resort 告警；用户看到 warn 日志 +
                // 磁盘 marker，能知道下次启动要清理。
                // 深度复核 P1 (2026-04-23): 抽出 write_acl_recovery_marker 共享给
                // revoke() 的失败路径，两条失败路径行为一致。
                write_acl_recovery_marker(&self.path, &e);
            }
            self.revoked = true;
        }

        // 释放 security descriptor（无论 revoke 成功与否都要做）
        // SAFETY: original_descriptor was allocated by GetNamedSecurityInfoW.
        // LocalFree is the documented way to free it.
        if !self.original_descriptor.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(std::mem::transmute(self.original_descriptor.0)));
            }
        }
    }
}

// SAFETY: The pointers in AclGuard are to heap memory allocated by Win32 APIs
// and are not shared with other threads. The guard is only used from the
// thread that created it (Drop runs on the same thread or on a single owner).
unsafe impl Send for AclGuard {}

/// 写 ACL 恢复 marker 到磁盘，供下次启动检测残留 ACE 后清理。
///
/// 深度复核 P1 (2026-04-23): `revoke()` 失败路径和 Drop 失败路径行为现在一致 —
/// 都写 `<path>.acl-recovery-needed` 磁盘 marker + warn 日志。调用方即使
/// 忽略 `revoke()` 的 Err，也能通过磁盘 marker 感知 "ACL 残留需清理"。
fn write_acl_recovery_marker(path: &Path, err: &SandboxError) {
    warn!(
        path = %path.display(),
        error = %err,
        "failed to restore original DACL — writing recovery marker"
    );
    let marker = path.with_extension("acl-recovery-needed");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Err(write_err) = std::fs::write(
        &marker,
        format!("acl restore failed: {err}\nunix_ts: {ts}\n"),
    ) {
        warn!(
            marker = %marker.display(),
            error = %write_err,
            "ALSO failed to write ACL recovery marker — permissions will leak until manual cleanup"
        );
    }
}

/// Grant workspace access to a SID (Security Identifier).
///
/// Adds an ACE (Access Control Entry) to the DACL of `path` granting
/// either read-only or read-write access to `sid`. Returns an [`AclGuard`]
/// that restores the original DACL on drop.
///
/// # Arguments
///
/// * `path` — Directory to grant access to
/// * `sid` — SID of the restricted process (from the restricted token)
/// * `mode` — Read-only or read-write access
pub fn grant_workspace_access(
    path: &Path,
    sid: PSID,
    mode: MountMode,
) -> Result<AclGuard, SandboxError> {
    let path_wide = to_wide_string(path);

    // ── 1. Get current DACL ──────────────────────────────────────────────
    let mut dacl_ptr: *mut SEC_ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();

    // SAFETY: path_wide is a valid null-terminated wide string.
    // We request DACL_SECURITY_INFORMATION which is always readable for the
    // file owner. The function allocates descriptor (freed via LocalFree).
    unsafe {
        let err = GetNamedSecurityInfoW(
            windows::core::PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&raw mut dacl_ptr),
            None,
            &raw mut descriptor,
        );
        if err.0 != 0 {
            return Err(SandboxError::Win32 {
                operation: format!("GetNamedSecurityInfoW({})", path.display()),
                error_code: err.0,
            });
        }
    }

    // ── 2. Build new ACE ─────────────────────────────────────────────────
    let access_mask = match mode {
        MountMode::ReadOnly => READ_ONLY_ACCESS,
        MountMode::ReadWrite => READ_WRITE_ACCESS,
    };

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: windows::core::PWSTR(sid.0.cast()),
            ..Default::default()
        },
    };

    // ── 3. Merge new ACE into existing DACL ──────────────────────────────
    let mut new_dacl: *mut SEC_ACL = std::ptr::null_mut();

    // SAFETY: dacl_ptr is a valid DACL from GetNamedSecurityInfoW.
    // ea is a properly initialized EXPLICIT_ACCESS_W.
    // SetEntriesInAclW merges the new ACE and allocates new_dacl.
    unsafe {
        let err = SetEntriesInAclW(Some(&[ea]), Some(dacl_ptr), &raw mut new_dacl);
        if err.0 != 0 {
            return Err(SandboxError::Win32 {
                operation: "SetEntriesInAclW".into(),
                error_code: err.0,
            });
        }
    }

    // Ensure new_dacl is freed even on error below
    let new_dacl_guard = DaclGuard(new_dacl);

    // ── 4. Apply modified DACL ───────────────────────────────────────────
    // SAFETY: path_wide is valid. new_dacl is a valid merged DACL from SetEntriesInAclW.
    unsafe {
        let err = SetNamedSecurityInfoW(
            windows::core::PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        );
        if err.0 != 0 {
            return Err(SandboxError::Win32 {
                operation: format!("SetNamedSecurityInfoW({})", path.display()),
                error_code: err.0,
            });
        }
    }

    // Don't free new_dacl yet — it's now in use by the filesystem
    std::mem::forget(new_dacl_guard);

    // Free new_dacl (the OS has copied it)
    // SAFETY: new_dacl was allocated by SetEntriesInAclW. Now that the OS has
    // applied it, we can free our copy.
    unsafe {
        let _ = LocalFree(Some(std::mem::transmute(new_dacl)));
    }

    info!(
        path = %path.display(),
        mode = ?mode,
        "workspace ACL modified — access granted"
    );

    Ok(AclGuard {
        path: path.to_path_buf(),
        original_dacl: dacl_ptr,
        original_descriptor: descriptor,
        revoked: false,
    })
}

/// RAII guard that frees a DACL allocated by `SetEntriesInAclW`.
struct DaclGuard(*mut SEC_ACL);

impl Drop for DaclGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: DACL was allocated by SetEntriesInAclW. LocalFree is documented.
            unsafe {
                let _ = LocalFree(Some(std::mem::transmute(self.0)));
            }
        }
    }
}

/// Convert a `Path` to a null-terminated UTF-16 wide string for Win32 APIs.
fn to_wide_string(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
