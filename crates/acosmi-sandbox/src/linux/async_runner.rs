//! Linux async sandbox runner — Sprint 7 阶段 3 骨架（2026-04-23）
//!
//! 真实 spawn 实装延后到阶段 3 下一 PR（含 `tokio::process::Command::pre_exec`
//! 对接 `apply_landlock_rules` + `apply_seccomp_filter`）。当前 stub 返
//! `Ok(None)`，让 `select_async_runner` 继续向下降级。
//!
//! # 完整实装拆解（阶段 3 next 迭代）
//!
//! 1. 新增 `LinuxAsyncRunner` struct（持 `SandboxBackend` + `LinuxCapabilities`）
//! 2. impl `AsyncSandboxRunner`: spawn 方法用 `tokio::process::Command`，
//!    pre_exec 闭包复用 `crate::linux::landlock::apply_landlock_rules` +
//!    `crate::linux::seccomp::apply_seccomp_filter`（已是 async-signal-safe）
//! 3. child.stdout/stderr 直接是 `tokio::io::AsyncRead`
//! 4. wait future: `async move { child.wait().await.map(...) }`
//! 5. abort closure: SIGKILL by pid
//! 6. _guards: cgroup guard（若有）+ child 本身（kill_on_drop=true 保证 drop 时清理）
//! 7. 加 integration test: landlock 生效性（写 `/etc/test.txt` 应 EACCES）

#![cfg(target_os = "linux")]

use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::async_runner::{AsyncSandboxRunner, BoxFuture, SandboxedProcess};
use crate::config::SandboxConfig;
use crate::error::SandboxError;
use crate::platform::{LinuxCapabilities, SandboxBackend};

use super::{cgroup, landlock, namespace, seccomp};

/// Best-effort signal to the process *group* led by `pgid`.
///
/// Mirrors `acosmi-app-server`'s private
/// `dispatcher::terminal::exec::killpg_best_effort` (can't be imported —
/// it's `pub(super)` in another crate). The Linux sandbox crate already
/// depends on `nix` (with `signal` + `process` features), so we reuse the
/// same `nix::sys::signal::killpg` primitive as exec.rs for parity.
///
/// `ESRCH` (group already empty — child reaped) is a benign no-op, exactly
/// like the exec.rs helper.
fn killpg_best_effort(pgid: libc::pid_t, signal: nix::sys::signal::Signal) {
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(pgid), signal) {
        Ok(()) => {}
        // ESRCH = group already empty (child reaped); benign in our flow.
        Err(nix::errno::Errno::ESRCH) => {}
        Err(err) => {
            debug!(pgid, %err, "sandbox killpg failed (non-ESRCH)");
        }
    }
}

/// Linux 异步沙箱 runner
pub struct LinuxAsyncRunner {
    backend: SandboxBackend,
    capabilities: LinuxCapabilities,
}

impl LinuxAsyncRunner {
    #[must_use]
    pub fn new(backend: SandboxBackend, capabilities: LinuxCapabilities) -> Self {
        Self {
            backend,
            capabilities,
        }
    }
}

impl AsyncSandboxRunner for LinuxAsyncRunner {
    fn name(&self) -> &'static str {
        self.backend.name()
    }

    fn available(&self) -> bool {
        self.capabilities.has_seccomp && self.capabilities.landlock_abi_version > 0
    }

    fn spawn<'a>(
        &'a self,
        config: &'a SandboxConfig,
    ) -> BoxFuture<'a, Result<SandboxedProcess, SandboxError>> {
        Box::pin(async move {
            let use_namespaces =
                self.backend == SandboxBackend::LinuxFull && self.capabilities.has_user_namespace;

            info!(
                backend = self.name(),
                use_namespaces,
                command = %config.command,
                "async spawn: starting sandboxed execution"
            );

            // ── 1. Setup cgroup resource limits（spawn 前）───────────────────
            // Cgroup guard 的 fd 在 pre_exec 里只做 write syscall，不分配；
            // guard 本体需要存活到进程退出以免被清理。
            let cgroup_guard = cgroup::setup_cgroup_limits(config)?;

            // ── 2. Build tokio::process::Command ────────────────────────────
            let mut cmd = Command::new(&config.command);
            cmd.args(&config.args)
                .current_dir(&config.workspace)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // kill_on_drop 保证 SandboxedProcess drop 时 Child 也被终止，
                // 是沙箱生命周期绑定的关键。
                .kill_on_drop(true);

            for (k, v) in &config.env_vars {
                cmd.env(k, v);
            }

            // pre_exec 闭包 move-in 的数据（都 Send + Sized，post-fork 可用）。
            // cgroup_guard 被 Arc 包装以便既给 pre_exec 又给返回 SandboxedProcess。
            let config_clone = config.clone();
            let has_user_ns = use_namespaces;
            let cgroup_arc = Arc::new(cgroup_guard);
            let cgroup_for_pre_exec = Arc::clone(&cgroup_arc);

            // SAFETY：同 sync LinuxRunner::run pre_exec（见 mod.rs:126-143）：
            //   - 闭包里不做 heap 分配（landlock/seccomp 是 syscall wrapper）
            //   - 顺序固定：setpgid → UserNS → MountNS → Cgroup → Landlock → Seccomp
            //   - tokio::process::Command::pre_exec 跟 std 语义一致
            unsafe {
                cmd.pre_exec(move || {
                    // 第一步：子进程自成进程组组长（pgid == pid）。**必须最先**
                    // 做，这样即便后续 namespace/landlock/seccomp 任一步失败导致
                    // exec 中止，进程组也已建立，abort 路径的 killpg 仍能整组清理
                    // 而非漏杀孙子进程。`setpgid(0,0)` 是纯 syscall，async-signal
                    // -safe，不依赖任何 namespace（此处只进 user/mount ns，未进
                    // PID ns，故 pgid 在父进程命名空间内可见、可被 killpg 命中）。
                    if libc::setpgid(0, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if has_user_ns {
                        namespace::apply_user_namespace()
                            .map_err(|_| std::io::Error::other("user namespace setup failed"))?;
                        namespace::apply_mount_namespace(&config_clone)
                            .map_err(|_| std::io::Error::other("mount namespace setup failed"))?;
                    }
                    if let Some(ref guard) = *cgroup_for_pre_exec {
                        guard
                            .add_self()
                            .map_err(|_| std::io::Error::other("cgroup add_self failed"))?;
                    }
                    landlock::apply_landlock_rules(&config_clone)
                        .map_err(|_| std::io::Error::other("landlock setup failed"))?;
                    seccomp::apply_seccomp_filter(&config_clone)
                        .map_err(|_| std::io::Error::other("seccomp setup failed"))?;
                    Ok(())
                });
            }

            // ── 3. Spawn ────────────────────────────────────────────────────
            let mut child = cmd.spawn().map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SandboxError::CommandNotFound {
                        command: config.command.clone(),
                    }
                } else {
                    SandboxError::Io {
                        context: format!("async spawn failed: {}", config.command),
                        source: e,
                    }
                }
            })?;

            let pid = child.id().ok_or_else(|| SandboxError::InvalidConfig {
                message: "tokio child missing pid（进程已退出？）".into(),
            })?;

            debug!(pid, "linux async sandbox process spawned");

            let stdout = child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
            let stderr = child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);

            // wait future — 消耗 child，返 exit code
            let wait: BoxFuture<'static, Result<i32, SandboxError>> = Box::pin(async move {
                match child.wait().await {
                    Ok(status) => Ok(status.code().unwrap_or(-1)),
                    Err(e) => Err(SandboxError::Io {
                        context: format!("async wait (pid {pid})"),
                        source: e,
                    }),
                }
            });

            // abort closure — SIGKILL 整个进程组（`kill_on_drop` 也兜底，
            // 但调用方可能想提前终止而不 drop SandboxedProcess）。
            // 子进程是其进程组组长（pre_exec setpgid(0,0)），pgid == pid，
            // killpg 覆盖经 shell `-c` 链派生的孙子进程，避免只杀直接 PID。
            let abort: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send> = Box::new(move || {
                Box::pin(async move {
                    if let Ok(raw) = libc::pid_t::try_from(pid) {
                        killpg_best_effort(raw, nix::sys::signal::Signal::SIGKILL);
                    }
                })
            });

            Ok(SandboxedProcess {
                pid,
                stdout,
                stderr,
                sandbox_backend: self.name().into(),
                wait,
                abort,
                // 持有 cgroup_arc 保证 SandboxedProcess 存活期间 cgroup 不被清理
                _guards: Box::new(cgroup_arc),
            })
        })
    }
}

/// `select_async_runner` 入口：Linux 侧依 capabilities 构造 `LinuxAsyncRunner`。
///
/// 返回 `Ok(None)` 让上层继续向下降级（Docker fallback）；返回 `Ok(Some(...))`
/// 表示此平台原生可用。
pub fn try_select(
    config: &SandboxConfig,
) -> Result<Option<Box<dyn AsyncSandboxRunner>>, SandboxError> {
    let caps = LinuxCapabilities::detect();
    let backend = if caps.has_user_namespace {
        SandboxBackend::LinuxFull
    } else if caps.has_seccomp && caps.landlock_abi_version > 0 {
        SandboxBackend::LinuxLandlockSeccomp
    } else {
        return Ok(None);
    };
    if let Err(err) = super::verify_native_spawn_setup(config, backend, &caps) {
        warn!(
            backend = backend.name(),
            error = %err,
            "linux native async sandbox unavailable after spawn preflight"
        );
        return Ok(None);
    }
    let runner = LinuxAsyncRunner::new(backend, caps);
    if !runner.available() {
        return Ok(None);
    }
    Ok(Some(Box::new(runner)))
}
