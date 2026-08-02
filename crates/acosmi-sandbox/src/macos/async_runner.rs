//! macOS async sandbox runner — Sprint 7 阶段 3 骨架（2026-04-23）
//!
//! 真实 spawn 实装延后到阶段 3 下一 PR（含 `tokio::process::Command::pre_exec`
//! 对接 `sandbox_init_with_parameters` FFI）。当前 stub 返 `Ok(None)`。
//!
//! # 完整实装拆解（阶段 3 next 迭代）
//!
//! 1. 新增 `MacosAsyncRunner` struct
//! 2. SBPL profile 编译在 spawn 外（避免 pre_exec 里 malloc 死锁；profile 是
//!    预计算的字符串，SandboxArgs::new 可在父进程做完）
//! 3. tokio::process::Command + pre_exec { sandbox_args.apply() }
//! 4. 其余同 Linux 模板

#![cfg(target_os = "macos")]

// 注: tokio::process::Command 在 unix 下自带 pre_exec 方法, 不再需要
// std::os::unix::process::CommandExt 引入 std::process::Command 的扩展.
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info};

use crate::async_runner::{AsyncSandboxRunner, BoxFuture, SandboxedProcess};
use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::platform::MacosCapabilities;

use super::{ffi, seatbelt};

/// Best-effort SIGKILL (or any signal) to the process *group* led by `pgid`.
///
/// Mirrors `acosmi-app-server`'s private
/// `dispatcher::terminal::exec::killpg_best_effort` (which we can't import —
/// it's `pub(super)` in a different crate). The macOS sandbox crate only
/// depends on `libc` (no `nix`), so we call `libc::killpg` directly.
///
/// `ESRCH` (no such process group — child already reaped) is treated as a
/// benign no-op, exactly like the exec.rs helper.
fn killpg_best_effort(pgid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: `killpg` only delivers a signal to an existing process group
    // we own (the sandboxed child is its own group leader). No memory is
    // touched. A `-1` return with `ESRCH` means the group already exited.
    let rc = unsafe { libc::killpg(pgid, signal) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        // ESRCH = group already empty (child reaped) — benign in our flow.
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::debug!(pgid, %err, "sandbox killpg failed (non-ESRCH)");
        }
    }
}

/// macOS 异步沙箱 runner（跟 Linux 异步 runner 同结构：tokio::process::Command
/// + Unix pre_exec 闭包调用 seatbelt FFI；SBPL profile 在 spawn 外预编译以避免
///   post-fork 里的堆分配风险）
pub struct MacosAsyncRunner {
    capabilities: MacosCapabilities,
}

impl MacosAsyncRunner {
    #[must_use]
    pub const fn new(capabilities: MacosCapabilities) -> Self {
        Self { capabilities }
    }
}

impl AsyncSandboxRunner for MacosAsyncRunner {
    fn name(&self) -> &'static str {
        "macos-seatbelt"
    }

    fn available(&self) -> bool {
        self.capabilities.has_seatbelt
    }

    fn spawn<'a>(
        &'a self,
        config: &'a SandboxConfig,
    ) -> BoxFuture<'a, Result<SandboxedProcess, SandboxError>> {
        Box::pin(async move {
            info!(
                command = %config.command,
                "async spawn: starting macos sandboxed execution"
            );

            // 1. 生成 SBPL profile（spawn 前，避免 post-fork 堆分配风险）
            let profile = seatbelt::generate_profile(config)?;
            debug!(
                profile_len = profile.sbpl.len(),
                param_count = profile.params.len(),
                "generated SBPL profile for async spawn"
            );

            let params_refs: Vec<(&str, &str)> = profile
                .params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let sandbox_args = ffi::SandboxArgs::new(&profile.sbpl, &params_refs)?;

            // 2. 构建 tokio::process::Command
            let mut cmd = Command::new(&config.command);
            cmd.args(&config.args)
                .current_dir(&config.workspace)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            for (k, v) in &config.env_vars {
                cmd.env(k, v);
            }
            if !config.env_vars.contains_key("TMPDIR")
                && let Ok(tmpdir) = std::env::var("TMPDIR")
            {
                cmd.env("TMPDIR", &tmpdir);
            }

            // 子进程自成进程组组长（pgid == pid），使 abort 时 killpg 能覆盖
            // 经 shell `-c "..."` 链派生的孙子进程，而非仅杀直接 PID。
            // `process_group(0)` 是 setpgid(0,0) 的安全稳定封装，post-fork
            // pre_exec 之前由 std 在 fork 后、exec 前注入，与下方 sandbox
            // pre_exec 闭包正交（两者都在 child 端 fork-after/exec-before 执行）。
            cmd.process_group(0);

            // SAFETY：sandbox_args 已在父进程预编译成 C 字符串 + 指针。
            // 闭包只调 sandbox_init_with_parameters FFI，无 Rust 堆分配。
            unsafe {
                cmd.pre_exec(move || sandbox_args.apply());
            }

            // 3. Spawn
            let mut child = cmd.spawn().map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SandboxError::CommandNotFound {
                        command: config.command.clone(),
                    }
                } else {
                    SandboxError::Io {
                        context: format!("macos async spawn failed: {}", config.command),
                        source: e,
                    }
                }
            })?;

            let pid = child.id().ok_or_else(|| SandboxError::InvalidConfig {
                message: "tokio child missing pid".into(),
            })?;

            debug!(pid, "macos async sandbox process spawned");

            let stdout = child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
            let stderr = child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);

            let wait: BoxFuture<'static, Result<i32, SandboxError>> = Box::pin(async move {
                match child.wait().await {
                    Ok(status) => Ok(status.code().unwrap_or(-1)),
                    Err(e) => Err(SandboxError::Io {
                        context: format!("macos async wait (pid {pid})"),
                        source: e,
                    }),
                }
            });

            let abort: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send> = Box::new(move || {
                Box::pin(async move {
                    // 子进程是其进程组组长（spawn 时 process_group(0)），故
                    // pgid == pid；killpg 整组 SIGKILL 覆盖经 shell `-c` 链
                    // 派生的孙子进程，避免只杀直接 PID 留下孤儿。
                    if let Ok(raw) = libc::pid_t::try_from(pid) {
                        killpg_best_effort(raw, libc::SIGKILL);
                    }
                })
            });

            Ok(SandboxedProcess {
                pid,
                stdout,
                stderr,
                sandbox_backend: "macos-seatbelt".to_string(),
                wait,
                abort,
                _guards: Box::new(()),
            })
        })
    }
}

pub fn try_select(
    _config: &SandboxConfig,
) -> Result<Option<Box<dyn AsyncSandboxRunner>>, SandboxError> {
    let caps = MacosCapabilities::detect();
    let runner = MacosAsyncRunner::new(caps);
    if !runner.available() {
        return Ok(None);
    }
    Ok(Some(Box::new(runner)))
}
