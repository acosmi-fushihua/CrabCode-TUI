//! Windows CreateRestrictedToken 0x80070579 根因分层复现测试（Sprint 7 阶段 2，2026-04-23）
//!
//! # 背景
//!
//! Sprint 7 bench (`benches/cold_start.rs`) 在本机 Windows 11 Home China 普通用户
//! 环境下跑 `native_cold_start/true/windows-restricted-token+job` panic：
//!
//! ```text
//! benchmark run failed: Err(Win32 {
//!     operation: "CreateRestrictedToken",
//!     error_code: 2147943737,  // 0x80070579 = HRESULT_FROM_WIN32(0x579) = HRESULT_FROM_WIN32(1401)
//! })
//! ```
//!
//! Round 1 §4 矩阵宣称 "WindowsRunner::run (spawn-child) Token+JobObject+ACL ✓"
//! 是 CI 盲区造成的文档 vs 实装脱节（同类 Round 2 #12 "verify_windows_sandbox_active
//! 从未编译" 问题），WindowsFull 路径从未真跑过。
//!
//! # 本 test 文件目标
//!
//! 按 MSDN `CreateRestrictedToken` 参数分层，每层加一组参数，定位**首次失败**的
//! 具体参数，为根因修复提供证据。不依赖 admin 权限，普通用户本机即可复现。
//!
//! # 调用形式（MSDN `CreateRestrictedToken`）
//!
//! ```c
//! BOOL CreateRestrictedToken(
//!   HANDLE ExistingTokenHandle,
//!   DWORD  Flags,
//!   DWORD  DisableSidCount,   PSID_AND_ATTRIBUTES SidsToDisable,
//!   DWORD  DeletePrivilegeCount, PLUID_AND_ATTRIBUTES PrivilegesToDelete,
//!   DWORD  RestrictedSidCount, PSID_AND_ATTRIBUTES SidsToRestrict,
//!   PHANDLE NewTokenHandle
//! );
//! ```
//!
//! 本 test 不用高层 `create_restricted_token()`，而是直接调 Win32 API，
//! 以便精确控制每个参数的空/非空状态。
//!
//! # 运行方式
//!
//! ```powershell
//! cargo test --manifest-path .../acosmi-sandbox/Cargo.toml --test windows_token_repro -- --nocapture
//! ```

#![cfg(windows)]

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetTokenInformation, SID_AND_ATTRIBUTES,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_QUERY, TokenGroups,
};
use windows::Win32::System::SystemServices::SE_GROUP_LOGON_ID;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 打开当前进程 token（所有 test case 共享）
fn open_current_process_token() -> HANDLE {
    let mut tok = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut tok,
        )
        .expect("OpenProcessToken on current process should succeed");
    }
    assert!(!tok.is_invalid(), "process token handle invalid");
    tok
}

fn close(h: HANDLE) {
    if !h.is_invalid() {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
}

/// Layer A：最小调用（所有可选参数皆 None / 0 flag），只是 Duplicate 一个 token
#[test]
fn layer_a_minimal_createrestrictedtoken() {
    let tok = open_current_process_token();
    let mut out = HANDLE::default();
    let r = unsafe {
        CreateRestrictedToken(
            tok,
            windows::Win32::Security::CREATE_RESTRICTED_TOKEN_FLAGS(0),
            None,
            None,
            None,
            &mut out,
        )
    };
    let err_code = r.as_ref().err().map(|e| e.code().0 as u32);
    eprintln!("Layer A (minimal) result: {r:?}, err_code = {err_code:?}");
    close(out);
    close(tok);
    r.expect("Layer A: 最小调用应成功（不加任何限制）");
}

/// Layer B：只加 DISABLE_MAX_PRIVILEGE flag，其他参数仍 None
#[test]
fn layer_b_with_disable_max_privilege() {
    let tok = open_current_process_token();
    let mut out = HANDLE::default();
    let r =
        unsafe { CreateRestrictedToken(tok, DISABLE_MAX_PRIVILEGE, None, None, None, &mut out) };
    let err_code = r.as_ref().err().map(|e| e.code().0 as u32);
    eprintln!("Layer B (DISABLE_MAX_PRIVILEGE) result: {r:?}, err_code = {err_code:?}");
    close(out);
    close(tok);
    r.expect("Layer B: 仅 DISABLE_MAX_PRIVILEGE 应成功");
}

/// Layer C：加 deny_sids（复现 get_deny_only_sids 的逻辑）
#[test]
fn layer_c_with_deny_sids() {
    let tok = open_current_process_token();

    // 查询 TOKEN_GROUPS
    let mut size = 0u32;
    unsafe {
        let _ = GetTokenInformation(tok, TokenGroups, None, 0, &mut size);
    }
    assert!(size > 0, "TOKEN_GROUPS size query failed");

    let mut buf = vec![0u8; size as usize];
    unsafe {
        GetTokenInformation(
            tok,
            TokenGroups,
            Some(buf.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .expect("GetTokenInformation(TokenGroups) failed");
    }

    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    let mut deny_sids = Vec::with_capacity(count);
    let mut logon_sids = 0;
    for i in 0..count {
        let g = unsafe { *groups.Groups.as_ptr().add(i) };
        if (g.Attributes & SE_GROUP_LOGON_ID as u32) != 0 {
            logon_sids += 1;
            continue;
        }
        deny_sids.push(SID_AND_ATTRIBUTES {
            Sid: g.Sid,
            Attributes: 0,
        });
    }
    eprintln!(
        "Layer C: TOKEN_GROUPS count = {count}, logon = {logon_sids}, deny = {}",
        deny_sids.len()
    );

    let mut out = HANDLE::default();
    let r = unsafe {
        CreateRestrictedToken(
            tok,
            DISABLE_MAX_PRIVILEGE,
            Some(&deny_sids),
            None,
            None,
            &mut out,
        )
    };
    let err_code = r.as_ref().err().map(|e| e.code().0 as u32);
    eprintln!("Layer C (+deny_sids) result: {r:?}, err_code = {err_code:?}");
    close(out);
    close(tok);
    r.expect("Layer C: DISABLE_MAX_PRIVILEGE + deny_sids 应成功");
}

/// Layer D：加 privileges_to_delete（复现 enumerate_deletable_privileges 逻辑）
///
/// 这是 create_restricted_token() 的完整参数组合。如果 bench panic 的根因
/// 在这一层，Layer A/B/C 会通过而 Layer D 失败。
#[test]
fn layer_d_with_privileges_to_delete() {
    use windows::Win32::Security::{
        LUID_AND_ATTRIBUTES, LookupPrivilegeNameW, TOKEN_PRIVILEGES, TOKEN_PRIVILEGES_ATTRIBUTES,
        TokenPrivileges,
    };

    let tok = open_current_process_token();

    // Query deny_sids（重用 Layer C 逻辑）
    let mut size_g = 0u32;
    unsafe {
        let _ = GetTokenInformation(tok, TokenGroups, None, 0, &mut size_g);
    }
    let mut buf_g = vec![0u8; size_g as usize];
    unsafe {
        GetTokenInformation(
            tok,
            TokenGroups,
            Some(buf_g.as_mut_ptr().cast()),
            size_g,
            &mut size_g,
        )
        .expect("GetTokenInformation(TokenGroups)");
    }
    let groups = unsafe { &*(buf_g.as_ptr().cast::<TOKEN_GROUPS>()) };
    let mut deny_sids = Vec::new();
    for i in 0..groups.GroupCount as usize {
        let g = unsafe { *groups.Groups.as_ptr().add(i) };
        if (g.Attributes & SE_GROUP_LOGON_ID as u32) != 0 {
            continue;
        }
        deny_sids.push(SID_AND_ATTRIBUTES {
            Sid: g.Sid,
            Attributes: 0,
        });
    }

    // Query privileges
    let mut size_p = 0u32;
    unsafe {
        let _ = GetTokenInformation(tok, TokenPrivileges, None, 0, &mut size_p);
    }
    let mut buf_p = vec![0u8; size_p as usize];
    if size_p > 0 {
        unsafe {
            GetTokenInformation(
                tok,
                TokenPrivileges,
                Some(buf_p.as_mut_ptr().cast()),
                size_p,
                &mut size_p,
            )
            .expect("GetTokenInformation(TokenPrivileges)");
        }
    }

    let mut privs: Vec<LUID_AND_ATTRIBUTES> = Vec::new();
    if size_p > 0 {
        let tp = buf_p.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let count = unsafe { (*tp).PrivilegeCount } as usize;
        let luid_ptr: *const LUID_AND_ATTRIBUTES = unsafe { (*tp).Privileges.as_ptr() };
        let slice: &[LUID_AND_ATTRIBUTES] = unsafe { std::slice::from_raw_parts(luid_ptr, count) };
        for la in slice {
            let mut name = [0u16; 256];
            let mut len: u32 = name.len() as u32;
            let ok = unsafe {
                LookupPrivilegeNameW(
                    windows::core::PCWSTR::null(),
                    &la.Luid,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut len,
                )
                .is_ok()
            };
            let name_str = if ok {
                String::from_utf16_lossy(&name[..len as usize])
            } else {
                "<unknown>".to_string()
            };
            eprintln!(
                "  privilege [{}]: LUID={{low={}, high={}}} attr=0x{:08x}",
                name_str, la.Luid.LowPart, la.Luid.HighPart, la.Attributes.0
            );
            if name_str == "SeChangeNotifyPrivilege" {
                continue;
            }
            privs.push(LUID_AND_ATTRIBUTES {
                Luid: la.Luid,
                Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(0),
            });
        }
    }
    eprintln!(
        "Layer D: deny_sids = {}, privileges_to_delete = {}",
        deny_sids.len(),
        privs.len()
    );

    let mut out = HANDLE::default();
    let priv_slice: &[LUID_AND_ATTRIBUTES] = &privs;
    let r = unsafe {
        CreateRestrictedToken(
            tok,
            DISABLE_MAX_PRIVILEGE,
            Some(&deny_sids),
            if priv_slice.is_empty() {
                None
            } else {
                Some(priv_slice)
            },
            None,
            &mut out,
        )
    };
    let err_code = r.as_ref().err().map(|e| e.code().0 as u32);
    eprintln!("Layer D (+privileges_to_delete) result: {r:?}, err_code = {err_code:?}");
    close(out);
    close(tok);
    match r {
        Ok(_) => eprintln!("Layer D 成功 → 根因不在参数级别"),
        Err(e) => panic!(
            "Layer D 失败: {:?} (err_code = 0x{:08x}). 这是 bench panic 的同一层。",
            e,
            e.code().0 as u32
        ),
    }
}

/// Layer F：**循环调用**（复现 bench warming up 的重复 CreateRestrictedToken）
///
/// bench 第一次调用通常 OK，从某一次开始 panic 0x80070579。如果根因是
/// handle leak / 资源耗尽，循环到某次会失败；如果是瞬态状态，则循环全通。
///
/// Note：每次循环调用 `create_restricted_token()` 完整流程，依赖 RestrictedToken
/// Drop 正确 CloseHandle。若 Drop 未清理某个 handle，多次调用后内核资源耗尽。
#[test]
fn layer_f_repeated_full_flow() {
    const ITERATIONS: usize = 50;
    let mut first_fail: Option<(usize, String, u32)> = None;

    for i in 0..ITERATIONS {
        match acosmi_sandbox::windows::token::create_restricted_token() {
            Ok(_token) => {
                // RestrictedToken Drop 时 CloseHandle
            }
            Err(acosmi_sandbox::error::SandboxError::Win32 {
                operation,
                error_code,
            }) => {
                first_fail = Some((i, operation.clone(), error_code));
                eprintln!("Layer F: 第 {i} 次调用失败: op='{operation}', code=0x{error_code:08x}");
                break;
            }
            Err(e) => {
                eprintln!("Layer F: 第 {i} 次调用未知错误: {e:?}");
                break;
            }
        }
        if i % 10 == 0 {
            eprintln!("Layer F: 完成 {}/{} 次", i + 1, ITERATIONS);
        }
    }

    match first_fail {
        None => eprintln!("Layer F: {} 次循环全部成功", ITERATIONS),
        Some((i, op, code)) => panic!(
            "Layer F: 第 {i} 次调用 {op} 失败，code=0x{code:08x} —— 这是 bench panic 的根因路径"
        ),
    }
}

/// Layer E：不加 deny_sids（传 None），仅加 privileges_to_delete
/// 如果 Layer C pass + Layer D fail + Layer E pass，根因在 deny_sids 组合里
#[test]
fn layer_e_privs_only() {
    use windows::Win32::Security::{
        LUID_AND_ATTRIBUTES, TOKEN_PRIVILEGES, TOKEN_PRIVILEGES_ATTRIBUTES, TokenPrivileges,
    };

    let tok = open_current_process_token();

    // Query privileges
    let mut size_p = 0u32;
    unsafe {
        let _ = GetTokenInformation(tok, TokenPrivileges, None, 0, &mut size_p);
    }
    let mut buf_p = vec![0u8; size_p as usize];
    if size_p > 0 {
        unsafe {
            GetTokenInformation(
                tok,
                TokenPrivileges,
                Some(buf_p.as_mut_ptr().cast()),
                size_p,
                &mut size_p,
            )
            .expect("GetTokenInformation(TokenPrivileges)");
        }
    }

    let mut privs: Vec<LUID_AND_ATTRIBUTES> = Vec::new();
    if size_p > 0 {
        let tp = buf_p.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let count = unsafe { (*tp).PrivilegeCount } as usize;
        let luid_ptr: *const LUID_AND_ATTRIBUTES = unsafe { (*tp).Privileges.as_ptr() };
        let slice: &[LUID_AND_ATTRIBUTES] = unsafe { std::slice::from_raw_parts(luid_ptr, count) };
        // 全部 LUID 都尝试删除（不过滤 SeChangeNotify）
        for la in slice {
            privs.push(LUID_AND_ATTRIBUTES {
                Luid: la.Luid,
                Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(0),
            });
        }
    }

    let mut out = HANDLE::default();
    let priv_slice: &[LUID_AND_ATTRIBUTES] = &privs;
    let r = unsafe {
        CreateRestrictedToken(
            tok,
            DISABLE_MAX_PRIVILEGE,
            None,
            if priv_slice.is_empty() {
                None
            } else {
                Some(priv_slice)
            },
            None,
            &mut out,
        )
    };
    eprintln!(
        "Layer E (privileges_to_delete only, no deny_sids): result = {r:?}, err_code = {:?}",
        r.as_ref()
            .err()
            .map(|e| format!("0x{:08x}", e.code().0 as u32))
    );
    close(out);
    close(tok);
    // 不 assert 成功或失败，只打印 —— 用于分层诊断
}
