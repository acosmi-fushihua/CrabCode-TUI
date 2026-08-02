//! Windows Restricted Token creation.
//!
//! Creates a restricted version of the current process token with:
//! - All privileges removed
//! - All group SIDs set to deny-only (except the logon SID)
//! - Medium Integrity Level inherited from the caller
//!
//! This follows Chromium's approach for renderer process isolation, with a
//! Windows console compatibility adjustment that preserves well-known read
//! groups required by standard Win32 process startup and avoids Low IL for
//! workspace-writing command-line workloads.
//! The restricted token is used with `CreateProcessAsUserW` to spawn
//! sandboxed child processes.

use tracing::{debug, info};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetTokenInformation, IsWellKnownSid, PSID,
    SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenGroups, TokenIntegrityLevel, TokenPrivileges, WinAuthenticatedUserSid, WinBuiltinUsersSid,
    WinWorldSid,
};
use windows::Win32::System::SystemServices::{SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::SandboxError;

/// RAII guard for a restricted token handle.
pub struct RestrictedToken {
    handle: HANDLE,
}

impl RestrictedToken {
    /// Get the raw token handle (for `CreateProcessAsUserW`).
    #[must_use]
    pub const fn handle(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for RestrictedToken {
    fn drop(&mut self) {
        // SAFETY: self.handle is a valid token handle from CreateRestrictedToken.
        // CloseHandle is safe for any handle we own.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
        debug!("restricted token handle closed");
    }
}

// SAFETY: Token handles are thread-safe in Win32.
unsafe impl Send for RestrictedToken {}
unsafe impl Sync for RestrictedToken {}

/// Diagnostic matrix feature gate: whether to skip deny-only groups.
///
/// 默认 false（走严格路径，与 production 行为一致）。仅在 `sandbox-matrix`
/// feature 编入时读环境变量 `CRABCODE_SANDBOX_MATRIX_DENY_ONLY`，值为 `"0"`
/// 时返 true 跳过该步骤。
#[cfg(feature = "sandbox-matrix")]
fn matrix_skip_deny_only() -> bool {
    std::env::var("CRABCODE_SANDBOX_MATRIX_DENY_ONLY").as_deref() == Ok("0")
}
#[cfg(not(feature = "sandbox-matrix"))]
const fn matrix_skip_deny_only() -> bool {
    false
}

/// P3-T2 矩阵实证 feature gate（2026-05-06）：是否重放修复前 Low IL 行为。
///
/// Production/default builds never apply Low IL. With `sandbox-matrix`, unset
/// or non-`"0"` keeps the historical pre-fix Low IL path so the matrix remains
/// reproducible; `"0"` keeps the fixed Medium IL behavior.
#[cfg(feature = "sandbox-matrix")]
fn matrix_apply_low_il_for_repro() -> bool {
    std::env::var("CRABCODE_SANDBOX_MATRIX_LOW_IL").as_deref() != Ok("0")
}
#[cfg(not(feature = "sandbox-matrix"))]
const fn matrix_apply_low_il_for_repro() -> bool {
    false
}

/// Create a restricted token from the current process token.
///
/// The token has:
/// - All privileges stripped (`DISABLE_MAX_PRIVILEGE`)
/// - Non-essential group SIDs set to deny-only
/// - Medium Integrity Level inherited from the caller
///
/// **P3-T2 矩阵 feature**：编入 `sandbox-matrix` feature 时，
/// `CRABCODE_SANDBOX_MATRIX_DENY_ONLY=0` 跳过 deny-only 群组步骤；
/// `CRABCODE_SANDBOX_MATRIX_LOW_IL=0` 保持 Medium IL，否则重放修复前
/// Low IL 降权路径。**仅供根因实证矩阵**，production / release 构建不得启用。
pub fn create_restricted_token() -> Result<RestrictedToken, SandboxError> {
    // ── 1. Open current process token ────────────────────────────────────
    let mut process_token = HANDLE::default();
    // P3-T2 修法（2026-05-06）：production/default 路径不再调用
    // `set_low_integrity_level`，token 保持 Medium IL。仍保留
    // **TOKEN_ADJUST_DEFAULT** 是为了 `sandbox-matrix` feature 能重放修复前
    // `SetTokenInformation(..., TokenIntegrityLevel, ...)` 路径；MSDN 明示该调用
    // 要求目标 token 有 TOKEN_ADJUST_DEFAULT 访问权：
    //   https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-settokeninformation
    //   "If TokenInformationClass is TokenIntegrityLevel, the token must have
    //    the TOKEN_ADJUST_DEFAULT access right."
    //
    // `CreateRestrictedToken` 生成的 new token 默认继承 ExistingTokenHandle
    // 的访问权限集；若源 token 不含 TOKEN_ADJUST_DEFAULT，矩阵重放路径会
    // 因派生 token 缺权导致 SetTokenInformation 返 0x80070005。
    //
    // Round 2 Windows bench 盲区 + 本机 Windows 11 Home China 普通用户
    // 复合环境下这个根因才被 Sprint 7 `layer_f_repeated_full_flow` 暴露。
    //
    // SAFETY: GetCurrentProcess() returns a pseudo-handle (always valid).
    // OpenProcessToken with TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY
    // | TOKEN_ADJUST_DEFAULT is safe — we're opening our own process token.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            &raw mut process_token,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "OpenProcessToken".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    // Ensure process token is closed even on error
    let _process_token_guard = HandleGuard(process_token);

    // ── 2. Get group SIDs to set as deny-only ────────────────────────────
    //
    // Sprint 7 阶段 2 根因修复（2026-04-23）：`deny_sids_owned` 同时持有
    // PSID 指针 + 底层 buffer 所有权。**不要 destructure 或 let _ = buffer
    // 丢弃它**；SID 指针的合法期 = `_buffer` 的存活期。
    // 违反这个约束会让 `CreateRestrictedToken` 读到悬空 PSID → 0x539/0x579。
    //
    // P3-T2 矩阵 feature（2026-05-06）：`CRABCODE_SANDBOX_MATRIX_DENY_ONLY=0`
    // 时仍调 get_deny_only_sids（保 _buffer 生命周期一致），但下方调
    // CreateRestrictedToken 时传空 slice 跳过 deny-only 步骤。
    let deny_sids_owned = get_deny_only_sids(process_token)?;
    let skip_deny_only = matrix_skip_deny_only();
    if skip_deny_only {
        info!("[matrix] CRABCODE_SANDBOX_MATRIX_DENY_ONLY=0 — skipping deny-only group SIDs");
    }

    // ── 3. Enumerate privileges to DELETE (not just disable) ─────────────
    //
    // ROOT-CAUSE FIX H6: DISABLE_MAX_PRIVILEGE 的语义是"禁用所有非 SE_CHANGE_NOTIFY
    // 特权"，**是禁用不是删除** — 被禁用的 privilege 仍留在 token 里，可在
    // SeAssignPrimaryTokenPrivilege 等条件下被 re-enable。Chromium renderer 的
    // 做法是 DISABLE + DELETE 双重：CreateRestrictedToken 的第四个参数传入
    // privileges_to_delete 显式删除所有 privilege（除了 SE_CHANGE_NOTIFY，
    // 那是正常进程运作所需）。
    //
    // 之前 token.rs imports 里有 AdjustTokenPrivileges/TOKEN_PRIVILEGES/LUID
    // 但从未调用，说明这个修复本身是历史 TODO 的残留。现在补齐。
    let privileges_to_delete = enumerate_deletable_privileges(process_token)?;

    // ── 4. Create restricted token ───────────────────────────────────────
    let mut raw_token = HANDLE::default();

    // SAFETY: process_token 是上方打开的合法 token 句柄。
    // DISABLE_MAX_PRIVILEGE + privileges_to_delete：双重防御，确保 privilege
    // 真正从 token 移除。`deny_sids_owned.sids` 的 PSID 指针指向
    // `deny_sids_owned._buffer` 内部，buffer 在本 unsafe 块生命周期内保持不
    // 释放（`deny_sids_owned` 持有到本 fn 结束）。raw_token 是新句柄所有权。
    // LUID_AND_ATTRIBUTES 都是 POD（无指针），privileges_to_delete 自持有。
    unsafe {
        let priv_slice: &[windows::Win32::Security::LUID_AND_ATTRIBUTES] = &privileges_to_delete;
        let deny_param: Option<&[SID_AND_ATTRIBUTES]> = if skip_deny_only {
            None
        } else {
            Some(&deny_sids_owned.sids)
        };
        CreateRestrictedToken(
            process_token,
            DISABLE_MAX_PRIVILEGE,
            deny_param,
            if priv_slice.is_empty() {
                None
            } else {
                Some(priv_slice)
            },
            None, // no restricting SIDs (using deny-only approach like Chromium)
            &raw mut raw_token,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "CreateRestrictedToken".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    // 显式在 CreateRestrictedToken 返回后 drop —— 让读者清楚此处之后 buffer
    // 才能安全释放。（与 Rust drop order 规则等价；此 drop 语句只是显式意图。）
    drop(deny_sids_owned);

    // S-6 audit fix: Wrap in RestrictedToken immediately so Drop closes the handle
    // if the matrix-only Low IL replay path fails below.
    let token = RestrictedToken { handle: raw_token };

    // ── 5. Integrity Level policy ────────────────────────────────────────
    //
    // P3-T2：Low IL is retained only for historical matrix reproduction.
    // Default behavior keeps Medium IL so writable workspaces behave like the
    // cross-platform sandbox contract expects after privileges/groups are
    // already restricted.
    let low_il_applied = matrix_apply_low_il_for_repro();
    if low_il_applied {
        info!("[matrix] applying historical Low IL path for P3-T2 reproduction");
        set_low_integrity_level(token.handle)?;
    }

    info!(
        privileges_deleted = privileges_to_delete.len(),
        deny_only_skipped = skip_deny_only,
        low_il_applied,
        "restricted token created"
    );

    Ok(token)
}

/// Enumerate the privileges in the current process token, filter out
/// `SeChangeNotifyPrivilege` (needed for normal operation), and return
/// them as `LUID_AND_ATTRIBUTES` ready to pass to `CreateRestrictedToken`'s
/// `privileges_to_delete` parameter.
///
/// Returning an empty Vec is a valid outcome (if the token has no privileges
/// beyond `SE_CHANGE_NOTIFY`) and means `DISABLE_MAX_PRIVILEGE` alone is enough.
fn enumerate_deletable_privileges(
    process_token: HANDLE,
) -> Result<Vec<windows::Win32::Security::LUID_AND_ATTRIBUTES>, SandboxError> {
    use windows::Win32::Security::LUID_AND_ATTRIBUTES;
    use windows::Win32::Security::LookupPrivilegeNameW;

    // ── 1. Query TokenPrivileges size ─────────────────────────────────────
    let mut size_needed: u32 = 0;
    // SAFETY: GetTokenInformation with None buffer returns required size via
    // size_needed; safe to call on our own token.
    unsafe {
        let _ = GetTokenInformation(
            process_token,
            TokenPrivileges,
            None,
            0,
            &raw mut size_needed,
        );
    }
    if size_needed == 0 {
        return Ok(Vec::new());
    }

    // ── 2. Query actual data ──────────────────────────────────────────────
    let mut buf = vec![0u8; size_needed as usize];
    // SAFETY: buf is sized according to size_needed from probe above.
    // GetTokenInformation fills it with TOKEN_PRIVILEGES struct.
    unsafe {
        GetTokenInformation(
            process_token,
            TokenPrivileges,
            Some(buf.as_mut_ptr().cast()),
            size_needed,
            &raw mut size_needed,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "GetTokenInformation(TokenPrivileges)".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    // ── 3. Parse and filter ───────────────────────────────────────────────
    // SAFETY: buf holds a valid TOKEN_PRIVILEGES followed by a flex array of
    // LUID_AND_ATTRIBUTES whose count is PrivilegeCount.
    let tp_ptr = buf.as_ptr().cast::<TOKEN_PRIVILEGES>();
    let count = unsafe { (*tp_ptr).PrivilegeCount } as usize;
    if count == 0 {
        return Ok(Vec::new());
    }

    // LUID_AND_ATTRIBUTES flex array starts at offset_of!(TOKEN_PRIVILEGES, Privileges)
    let luid_array_ptr: *const LUID_AND_ATTRIBUTES = unsafe { (*tp_ptr).Privileges.as_ptr() };
    let luid_slice: &[LUID_AND_ATTRIBUTES] =
        unsafe { std::slice::from_raw_parts(luid_array_ptr, count) };

    // SE_CHANGE_NOTIFY_NAME LUID 是特殊的：传递文件系统访问需要，删掉会让
    // 合法子进程无法跑。DISABLE_MAX_PRIVILEGE 本来就会保留它，对齐行为。
    const SE_CHANGE_NOTIFY_NAME: &str = "SeChangeNotifyPrivilege";
    let mut result = Vec::with_capacity(count);
    for la in luid_slice {
        // 查询名称以便过滤
        let mut name_buf = [0u16; 256];
        let mut name_len: u32 = name_buf.len() as u32;
        // SAFETY: la.Luid 是合法 LUID，from caller 传入。name_buf 栈分配足够大。
        let ok = unsafe {
            LookupPrivilegeNameW(
                windows::core::PCWSTR::null(), // local system
                &raw const la.Luid,
                Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                &raw mut name_len,
            )
            .is_ok()
        };
        if ok {
            let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            if name == SE_CHANGE_NOTIFY_NAME {
                continue; // 保留 SeChangeNotifyPrivilege
            }
        }
        // 不管能不能拿到名字，都删除这条 privilege（保守策略）
        result.push(LUID_AND_ATTRIBUTES {
            Luid: la.Luid,
            Attributes: windows::Win32::Security::TOKEN_PRIVILEGES_ATTRIBUTES(0),
        });
    }
    debug!(
        total_in_token = count,
        deletable = result.len(),
        "enumerated privileges for deletion"
    );
    Ok(result)
}

/// Holds the deny-only SIDs together with the `TOKEN_GROUPS` buffer the `PSID`
/// pointers reference.
///
/// # Sprint 7 阶段 2 根因修复（2026-04-23）
///
/// 旧实现 `fn get_deny_only_sids(token) -> Result<Vec<SID_AND_ATTRIBUTES>>`
/// 构造 `SID_AND_ATTRIBUTES` 时直接拷贝 `group.Sid`（一个 PSID 指针），
/// **但指向的内存是函数内 `buffer: Vec<u8>`**。函数返回时 buffer drop，
/// 返回值里的 PSID 变成悬空指针。
///
/// 症状：
/// - `acosmi-sandbox/tests/windows_token_repro.rs::layer_f_repeated_full_flow`
///   第 0 次即失败，`CreateRestrictedToken` 返 `0x80070539 (ERROR_INVALID_SID)`
///   或 `0x80070579`（内存被其他分配覆盖后 Win32 观察到的混乱状态）
/// - Round 1 §4 "`WindowsRunner` ✓" 描述失效；WindowsFull 路径从未真跑过
/// - Round 2 §12 "`verify_windows_sandbox_active` 从未编译" 的 Windows CI 盲区
///   让这个 root cause 隐藏了 6+ 个月
///
/// # 修复
///
/// 返回值改成同时持有 buffer 所有权的 struct，确保 PSID 在 `CreateRestrictedToken`
/// 调用期间指向活动内存。`sids` Vec 依赖 `_buffer` 生命周期，不得先释放 `_buffer`。
pub(super) struct DenyOnlySids {
    /// 待传给 `CreateRestrictedToken` 的 `SID_AND_ATTRIBUTES` 数组。
    /// PSID 指针指向 `_buffer` 内部，不得复制到 _buffer 之外存活。
    pub(super) sids: Vec<SID_AND_ATTRIBUTES>,
    /// 拥有实际 SID 数据的 buffer；必须在 sids 被使用期间保持不释放。
    _buffer: Vec<u8>,
}

/// Get all group SIDs from the token, marking non-logon SIDs as deny-only.
///
/// The logon SID is preserved because it's needed for desktop/window station access.
fn get_deny_only_sids(token: HANDLE) -> Result<DenyOnlySids, SandboxError> {
    // First call to get required buffer size
    let mut size = 0u32;
    // SAFETY: First call with null buffer to get size. This is the documented pattern
    // for GetTokenInformation — it sets size and returns ERROR_INSUFFICIENT_BUFFER.
    unsafe {
        let _ = GetTokenInformation(token, TokenGroups, None, 0, &raw mut size);
    }

    if size == 0 {
        return Err(SandboxError::Win32 {
            operation: "GetTokenInformation(TokenGroups) size query".into(),
            error_code: 0,
        });
    }

    let mut buffer = vec![0u8; size as usize];

    // SAFETY: buffer is properly sized (from the first call). token is valid.
    // We cast the buffer to TOKEN_GROUPS after the call succeeds.
    unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &raw mut size,
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "GetTokenInformation(TokenGroups)".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    // SAFETY: GetTokenInformation succeeded and filled buffer with TOKEN_GROUPS data.
    // The buffer is properly aligned (Vec<u8> from the heap) and sized.
    let groups = unsafe { &*(buffer.as_ptr().cast::<TOKEN_GROUPS>()) };

    let group_count = groups.GroupCount as usize;
    let mut deny_sids = Vec::with_capacity(group_count);

    for i in 0..group_count {
        // SAFETY: GroupCount tells us how many SID_AND_ATTRIBUTES entries exist.
        // The Groups field is a variable-length array at the end of TOKEN_GROUPS.
        let group = unsafe { *groups.Groups.as_ptr().add(i) };

        // Skip the logon SID — it's needed for desktop/window station access.
        if (group.Attributes & SE_GROUP_LOGON_ID as u32) != 0 {
            debug!("preserving logon SID (not deny-only)");
            continue;
        }

        // P3-T2 RC-WIN-1: standard Win32 startup needs read access granted via
        // broad well-known groups on HKLM registry keys. Marking these groups
        // deny-only caused sandboxed cmd.exe to exit with STATUS_ACCESS_DENIED.
        if is_required_startup_group_sid(group.Sid) {
            debug!("preserving well-known startup SID (not deny-only)");
            continue;
        }

        // 注意：`group.Sid` 是指向 `buffer` 内部的 PSID。`DenyOnlySids._buffer`
        // 持有 `buffer` 所有权直到 `CreateRestrictedToken` 调用完成，保证指针有效。
        deny_sids.push(SID_AND_ATTRIBUTES {
            Sid: group.Sid,
            Attributes: 0, // CreateRestrictedToken sets deny-only for these
        });
    }

    debug!(
        total_groups = group_count,
        deny_only = deny_sids.len(),
        "collected group SIDs for deny-only (with owning buffer)"
    );

    Ok(DenyOnlySids {
        sids: deny_sids,
        _buffer: buffer,
    })
}

fn is_required_startup_group_sid(sid: PSID) -> bool {
    // SAFETY: `sid` comes from the TOKEN_GROUPS buffer returned by
    // GetTokenInformation and remains valid for this check.
    unsafe {
        IsWellKnownSid(sid, WinWorldSid).as_bool()
            || IsWellKnownSid(sid, WinAuthenticatedUserSid).as_bool()
            || IsWellKnownSid(sid, WinBuiltinUsersSid).as_bool()
    }
}

/// Set the integrity level of a token to Low (S-1-16-4096).
///
/// Low Integrity Level prevents the process from writing to objects at
/// Medium or higher integrity (including most user files by default).
fn set_low_integrity_level(token: HANDLE) -> Result<(), SandboxError> {
    // Low Integrity Level SID: S-1-16-4096
    let low_il_str = windows::core::w!("S-1-16-4096");

    let mut low_il_sid = PSID::default();

    // SAFETY: ConvertStringSidToSidW converts a well-known SID string to a SID.
    // "S-1-16-4096" is the documented Low Integrity Level SID.
    // The function allocates the SID — we must free it with LocalFree.
    unsafe {
        ConvertStringSidToSidW(low_il_str, &raw mut low_il_sid).map_err(|e| {
            SandboxError::Win32 {
                operation: "ConvertStringSidToSidW(Low IL)".into(),
                error_code: e.code().0 as u32,
            }
        })?;
    }

    // Ensure SID is freed even on error
    let _sid_guard = SidGuard(low_il_sid);

    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: low_il_sid,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };

    // SAFETY: token is a valid restricted token handle.
    // label contains a valid Low IL SID from ConvertStringSidToSidW.
    // SetTokenInformation with TokenIntegrityLevel sets the integrity level.
    unsafe {
        SetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::from_ref(&label).cast(),
            u32::try_from(
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>()
                    + windows::Win32::Security::GetLengthSid(low_il_sid) as usize,
            )
            .unwrap_or(u32::MAX),
        )
        .map_err(|e| SandboxError::Win32 {
            operation: "SetTokenInformation(IntegrityLevel=Low)".into(),
            error_code: e.code().0 as u32,
        })?;
    }

    debug!("token integrity level set to Low (S-1-16-4096)");
    Ok(())
}

/// RAII guard that closes a HANDLE on drop.
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: Handle was obtained from a successful Win32 API call.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// RAII guard that frees a PSID allocated by `ConvertStringSidToSidW`.
struct SidGuard(PSID);

impl Drop for SidGuard {
    fn drop(&mut self) {
        // SAFETY: PSID was allocated by ConvertStringSidToSidW.
        // LocalFree is the documented way to release it.
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(std::mem::transmute(self.0)));
        }
    }
}
