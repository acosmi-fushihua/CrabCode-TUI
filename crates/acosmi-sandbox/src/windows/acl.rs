//! NTFS ACL management for workspace access control.
//!
//! Grants temporary file system access to the restricted sandbox process,
//! then revokes it on cleanup via RAII [`AclGuard`].
//!
//! # Workflow (probe-first since W-SANDBOX-ENFORCED-DEADCODE PR-3)
//!
//! ```text
//! 0. AccessCheck            →  does the restricted token ALREADY have it?
//!                              yes ⇒ stop here, zero DACL writes  ← normal path
//! 1. GetNamedSecurityInfoW  →  get current DACL
//! 2. SetEntriesInAclW       →  add temporary ACE for sandbox SID
//! 3. SetNamedSecurityInfoW  →  apply modified DACL
//! 4. (on Drop)              →  restore original DACL
//! ```
//!
//! This ensures the restricted token process can access its workspace
//! without granting permanent filesystem permissions.
//!
//! # Why step 0 exists (2026-08-08 measured, not guessed)
//!
//! Writing a DACL onto a directory that carries inheritable ACEs makes Windows
//! re-propagate inheritance across the **entire subtree**, so both step 3 and the
//! Drop-time restore cost `O(objects under the path)` — measured at
//! 0.36–0.47 ms/object, i.e. **15.7 s + 14.6 s on this machine's %TEMP%**
//! (57,257 objects). Every sandboxed spawn paid that twice.
//!
//! Two facts make step 0 correct rather than merely fast:
//!
//! - `CreateRestrictedToken` does not touch `TokenUser`, and
//!   [`token::is_required_startup_group_sid`](super::token) keeps Everyone /
//!   Authenticated Users / BUILTIN\Users out of the deny-only set — so on a
//!   workspace the user can already use, the added ACE grants rights the token
//!   **already has**. It is a no-op with an `O(tree)` price tag.
//! - The probe asks for the *exact* mask [`grant_workspace_access`] would add.
//!   So "probe passed" ⇒ the grant would have changed nothing that matters to
//!   the access check. The fast path is behaviour-preserving by construction,
//!   not by approximation. (A narrower "only what the command really needs"
//!   probe would skip the grant more often but would no longer be a strict
//!   no-op proof — deliberately not done.)
//!
//! When the probe says *no*, the original grant + revoke path runs unchanged:
//! a workspace whose DACL genuinely denies the user still gets its temporary
//! ACE, and still pays the propagation cost. Fail-closed is preserved; only the
//! pointless work is gone.
//!
//! Ruled out by measurement (do not re-try these): clearing the inheritance
//! flag on our own ACE (`NO_INHERITANCE` A/B was equally slow — the propagation
//! comes from the directory's *pre-existing* inheritable ACEs, not ours).

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use windows::Win32::Foundation::{HANDLE, LocalFree};
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::PSID;
use windows::Win32::Security::{
    ACL as SEC_ACL, AccessCheck, DACL_SECURITY_INFORMATION, DuplicateToken, GENERIC_MAPPING,
    GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PSECURITY_DESCRIPTOR,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, SecurityImpersonation,
};

use crate::config::MountMode;
use crate::error::SandboxError;

/// File access rights for read-only access.
const READ_ONLY_ACCESS: u32 = 0x0012_0089; // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE

/// File access rights for read-write access.
const READ_WRITE_ACCESS: u32 = 0x001F_01FF; // FILE_ALL_ACCESS

/// Standard NTFS generic-rights mapping, required by `AccessCheck`.
///
/// Our desired masks are already specific rights (no `GENERIC_*` bits), so this
/// is never actually consulted — but `AccessCheck` rejects a null mapping.
/// `static` rather than `const` because we pass a pointer to it: a `const` would
/// materialise a temporary whose address dies at the end of the expression.
static FILE_GENERIC_MAPPING: GENERIC_MAPPING = GENERIC_MAPPING {
    GenericRead: 0x0012_0089,
    GenericWrite: 0x0012_0116,
    GenericExecute: 0x0012_00A0,
    GenericAll: 0x001F_01FF,
};

/// The access mask a [`MountMode`] asks for. Single source for both the probe
/// and the grant — they **must** stay identical, that identity is what makes
/// "probe passed ⇒ the grant is a no-op" a proof rather than a hope.
const fn access_mask_for(mode: MountMode) -> u32 {
    match mode {
        MountMode::ReadOnly => READ_ONLY_ACCESS,
        MountMode::ReadWrite => READ_WRITE_ACCESS,
    }
}

/// RAII free for a security descriptor allocated by `GetNamedSecurityInfoW`.
struct SecurityDescriptorGuard(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptorGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: allocated by GetNamedSecurityInfoW; LocalFree is the documented release.
            unsafe {
                let _ = LocalFree(Some(std::mem::transmute(self.0.0)));
            }
        }
    }
}

/// RAII close for a token handle we duplicated ourselves.
struct TokenHandleGuard(HANDLE);

impl Drop for TokenHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: handle came from DuplicateToken; we own it.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

/// Does `token` already hold `mode`'s access on `path`?
///
/// Pure in-memory kernel access check — reads the security descriptor and asks
/// `AccessCheck`. **No filesystem mutation, no temporary file, no ACL write**,
/// so it costs microseconds regardless of how large the tree under `path` is.
/// That property is the whole point: the thing it replaces cost 15 s here.
///
/// `token` must be the token the sandboxed child will actually run under (the
/// restricted one) — checking with the *host's* token would answer a question
/// nobody asked. `AccessCheck` is restricted-token aware: for such tokens the
/// kernel runs the check twice (token SIDs, then restricting SIDs) and grants
/// the intersection, which is exactly the child's effective rights.
///
/// # Errors
///
/// Returns [`SandboxError::Win32`] when the descriptor cannot be read or the
/// check cannot be performed. Callers treat an error as "unknown" and fall back
/// to granting — never as "already fine".
pub fn token_has_workspace_access(
    path: &Path,
    token: HANDLE,
    mode: MountMode,
) -> Result<bool, SandboxError> {
    let path_wide = to_wide_string(path);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();

    // OWNER + GROUP are not optional here: `AccessCheck` fails with
    // ERROR_INVALID_SECURITY_DESCR on a descriptor missing either of them.
    // SAFETY: path_wide is a valid null-terminated wide string; `descriptor`
    // receives an allocation we free via the guard below.
    unsafe {
        let err = GetNamedSecurityInfoW(
            windows::core::PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &raw mut descriptor,
        );
        if err.0 != 0 {
            return Err(SandboxError::Win32 {
                operation: format!("GetNamedSecurityInfoW(probe {})", path.display()),
                error_code: err.0,
            });
        }
    }
    let _descriptor_guard = SecurityDescriptorGuard(descriptor);

    // `AccessCheck` insists on an *impersonation* token; ours is a primary one.
    let mut impersonation = HANDLE::default();
    // SAFETY: `token` is a live token handle opened with TOKEN_DUPLICATE
    // (see `token::create_restricted_token`); the out-param is a stack HANDLE.
    unsafe {
        DuplicateToken(token, SecurityImpersonation, &raw mut impersonation).map_err(|e| {
            SandboxError::Win32 {
                operation: "DuplicateToken(for AccessCheck)".into(),
                error_code: e.code().0 as u32,
            }
        })?;
    }
    let _impersonation_guard = TokenHandleGuard(impersonation);

    let mut privileges = PRIVILEGE_SET::default();
    let mut privileges_len =
        u32::try_from(std::mem::size_of::<PRIVILEGE_SET>()).unwrap_or(u32::MAX);
    let mut granted: u32 = 0;
    let mut allowed = windows::core::BOOL(0);

    // SAFETY: descriptor and impersonation are live for this call; every
    // out-param points at a stack local that outlives it.
    unsafe {
        AccessCheck(
            descriptor,
            impersonation,
            access_mask_for(mode),
            &raw const FILE_GENERIC_MAPPING,
            Some(&raw mut privileges),
            &raw mut privileges_len,
            &raw mut granted,
            &raw mut allowed,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: format!("AccessCheck({})", path.display()),
            error_code: e.code().0 as u32,
        })?;
    }

    // A denied check is a *successful* call with `allowed = FALSE`; only a
    // broken descriptor/token makes `AccessCheck` itself fail (handled above).
    Ok(allowed.0 != 0)
}

/// Give the sandboxed token access to `path` — **only if it does not have it**.
///
/// Returns `None` when the probe already answers yes: nothing was modified, so
/// there is nothing to revoke. Returns `Some(guard)` when an ACE was actually
/// added; dropping (or [`AclGuard::revoke`]-ing) it restores the original DACL.
///
/// # Errors
///
/// Propagates [`grant_workspace_access`]'s errors. A probe failure is **not**
/// fatal: it is logged and treated as "assume no access", which lands on the
/// original (slow but correct) grant path.
pub fn ensure_workspace_access(
    path: &Path,
    token: HANDLE,
    sid: PSID,
    mode: MountMode,
) -> Result<Option<AclGuard>, SandboxError> {
    match token_has_workspace_access(path, token, mode) {
        Ok(true) => {
            debug!(
                path = %path.display(),
                mode = ?mode,
                "workspace already accessible to the sandbox token — no DACL write"
            );
            return Ok(None);
        }
        Ok(false) => {
            info!(
                path = %path.display(),
                mode = ?mode,
                "workspace denies the sandbox token — granting a temporary ACE \
                 (cost scales with the number of objects under this path)"
            );
        }
        Err(e) => {
            // Unknown ⇒ behave exactly like the pre-PR-3 code did.
            warn!(
                path = %path.display(),
                error = %e,
                "workspace access probe failed — falling back to an unconditional grant"
            );
        }
    }
    grant_workspace_access(path, sid, mode).map(Some)
}

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

    // Until the `AclGuard` below takes ownership, nothing else frees this
    // descriptor — and the two error branches in between used to leak it.
    let descriptor_guard = SecurityDescriptorGuard(descriptor);

    // ── 2. Build new ACE ─────────────────────────────────────────────────
    let access_mask = access_mask_for(mode);

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

    // `SetNamedSecurityInfoW` copies the DACL into the object's own security
    // descriptor, so our merged copy is dead the moment it returns — free it
    // exactly once. The guard is disarmed first and the free written out by
    // hand so that this is one `LocalFree`, not two.
    //
    // (The comment that used to sit here said "Don't free new_dacl yet",
    // immediately above the code that frees it. The code was right; the comment
    // was the thing that would eventually get someone to "fix" it into a
    // double-free.)
    std::mem::forget(new_dacl_guard);
    // SAFETY: new_dacl was allocated by SetEntriesInAclW and is no longer
    // referenced by anything — the guard above was disarmed for this line.
    unsafe {
        let _ = LocalFree(Some(std::mem::transmute(new_dacl)));
    }

    info!(
        path = %path.display(),
        mode = ?mode,
        "workspace ACL modified — access granted"
    );

    // Ownership of the descriptor moves into the guard, which frees it on Drop.
    std::mem::forget(descriptor_guard);

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use windows::Win32::Security::{
        ACL_REVISION, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    /// Replace a directory's DACL with an **empty, protected** one.
    ///
    /// An ACL that exists but contains zero ACEs denies everyone — including the
    /// owner, who keeps only `READ_CONTROL`/`WRITE_DAC` implicitly (which is
    /// precisely what lets us put it back afterwards). Protected so the parent's
    /// inheritable ACEs cannot quietly re-open it.
    fn deny_everyone(path: &Path) {
        // DWORD-aligned storage: `InitializeAcl` requires it and a `Vec<u8>`
        // does not promise it.
        let mut storage = [0u32; 16];
        let acl = storage.as_mut_ptr().cast::<SEC_ACL>();
        let wide = to_wide_string(path);
        unsafe {
            InitializeAcl(
                acl,
                u32::try_from(std::mem::size_of_val(&storage)).unwrap(),
                ACL_REVISION,
            )
            .unwrap();
            let err = SetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            );
            assert_eq!(err.0, 0, "could not lock down {}", path.display());
        }
    }

    /// Put the directory back to "inherit from the parent, deny nobody" so the
    /// temp dir can be removed.
    fn undo_deny_everyone(path: &Path) {
        let wide = to_wide_string(path);
        unsafe {
            let _ = SetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
            );
        }
    }

    #[test]
    fn probe_says_yes_on_a_workspace_the_user_already_owns() {
        let token = super::super::token::create_restricted_token().unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            token_has_workspace_access(dir.path(), token.handle(), MountMode::ReadWrite).unwrap(),
            "a directory this user just created must already be writable by the \
             restricted token — if this fails, the fast path is dead and every \
             spawn is back to paying O(tree)"
        );
    }

    #[test]
    fn probe_says_no_when_the_dacl_really_denies_the_user() {
        // The negative control. Without it, "the probe said yes" proves nothing:
        // a function that returns `true` unconditionally would pass every other
        // test in this file while silently deleting the fail-closed fallback.
        let token = super::super::token::create_restricted_token().unwrap();
        let dir = tempfile::tempdir().unwrap();
        deny_everyone(dir.path());

        let verdict = token_has_workspace_access(dir.path(), token.handle(), MountMode::ReadWrite);

        undo_deny_everyone(dir.path());
        assert!(
            !verdict.unwrap(),
            "an empty protected DACL denies everyone; the probe must say so"
        );
    }

    #[test]
    fn ensure_skips_the_grant_when_access_is_already_there() {
        let token = super::super::token::create_restricted_token().unwrap();
        let sid = super::super::get_token_user_sid(token.handle()).unwrap();
        let dir = tempfile::tempdir().unwrap();

        let guard =
            ensure_workspace_access(dir.path(), token.handle(), sid.sid, MountMode::ReadWrite)
                .unwrap();
        assert!(
            guard.is_none(),
            "no guard means no DACL was written — that is the entire performance fix"
        );
    }

    #[test]
    fn ensure_grants_and_then_revokes_when_access_is_missing() {
        // The fail-closed half: a workspace that genuinely denies the token must
        // still get its temporary ACE, and that ACE must go away afterwards.
        let token = super::super::token::create_restricted_token().unwrap();
        let sid = super::super::get_token_user_sid(token.handle()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        deny_everyone(dir.path());

        let outcome = (|| {
            let guard =
                ensure_workspace_access(dir.path(), token.handle(), sid.sid, MountMode::ReadWrite)?;
            let granted = guard.is_some();
            let during =
                token_has_workspace_access(dir.path(), token.handle(), MountMode::ReadWrite)?;
            drop(guard);
            let after =
                token_has_workspace_access(dir.path(), token.handle(), MountMode::ReadWrite)?;
            Ok::<_, SandboxError>((granted, during, after))
        })();

        undo_deny_everyone(dir.path());
        let (granted, during, after) = outcome.unwrap();
        assert!(granted, "a denying workspace must receive a real grant");
        assert!(
            during,
            "the grant must actually open the access it promises"
        );
        assert!(
            !after,
            "dropping the guard must restore the original DACL — a temporary ACE \
             that outlives the command is a permanent hole"
        );
    }
}
