//! Windows async sandbox runner — Sprint 7 阶段 3 骨架（2026-04-23）
//!
//! **Windows 特殊情况**：`WindowsFull` 路径因 `STATUS_DLL_INIT_FAILED (0xC0000142)`
//! 根因（Round 3 独立立项）目前**不可用**，即便阶段 3 完整实装 async spawn，
//! 子进程也会在 DLL 加载失败。因此此模块长期 stub 返 `Ok(None)`，直到 Round 3
//! DLL preload 或 `AppContainer` 替代方案实装完成。
//!
//! `WindowsJobOnly` 模式理论上可用（不涉及 restricted token / Low IL），但其
//! "沙箱强度"只是资源限制 + 进程树 `KILL_ON_JOB_CLOSE，不构成安全边界`，
//! 跟 direct `ProcessBuilder` spawn 无本质差异。故 Windows 侧暂不实装 async
//! spawn，让 supervisor `handle_spawn_managed` opt-in 走默认 direct path。

#![cfg(target_os = "windows")]

use crate::async_runner::AsyncSandboxRunner;
use crate::config::SandboxConfig;
use crate::error::SandboxError;

// NOTE (process-tree termination): this Windows async sandbox runner is a
// pure stub returning `Ok(None)` — the supervisor `handle_spawn_managed`
// opt-in path falls through to the default direct `ProcessBuilder` spawn,
// and the live command-execution path runs through
// `acosmi-app-server::dispatcher::terminal::exec` instead. Process-*tree*
// kill on Windows (kill grandchildren, not just the direct PID) is therefore
// implemented at that exec layer via a Win32 Job Object
// (`CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` +
// `AssignProcessToJobObject`, terminated with `TerminateJobObject`).
// If/when this runner is given a real async spawn implementation, it must
// adopt the same Job Object tree-kill discipline.
pub fn try_select(
    _config: &SandboxConfig,
) -> Result<Option<Box<dyn AsyncSandboxRunner>>, SandboxError> {
    Ok(None)
}
