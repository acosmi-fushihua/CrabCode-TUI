//! 进程管理 — 启动、监控、重启被管理进程
//!
//! 实现进程的完整生命周期：spawn -> monitor -> stop/restart。
//! 平台差异通过 cfg 条件编译处理：
//! - Unix: 使用 process group 确保 kill 能递归终止
//! - Windows: 使用 Job Objects 确保子进程随 supervisor 退出

use crate::topology::{ProcessConfig, ProcessGroupPolicy, ProcessId, RestartPolicy, StdioPolicy};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

/// supervisor 专有错误类型
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("进程 {id} 启动失败: {source}")]
    SpawnFailed {
        id: ProcessId,
        source: std::io::Error,
    },

    #[error("进程 {id} 停止超时")]
    StopTimeout { id: ProcessId },

    #[error("进程 {id} 重启次数超限 ({count}/{max} in {window:?})")]
    RestartLimitExceeded {
        id: ProcessId,
        count: u32,
        max: u32,
        window: Duration,
    },

    #[error("进程 {id} 异常退出: code={code:?}")]
    AbnormalExit { id: ProcessId, code: Option<i32> },

    #[error("平台操作失败: {0}")]
    PlatformError(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// 进程状态
#[derive(Debug)]
pub enum ProcessState {
    /// 正在启动
    Starting,
    /// 运行中
    Running { pid: u32, started_at: Instant },
    /// 正在停止
    Stopping,
    /// 已停止
    Stopped { exit_code: Option<i32> },
    /// 启动/运行失败
    Failed { error: String, restart_count: u32 },
}

impl std::fmt::Display for ProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "Starting"),
            Self::Running { pid, .. } => write!(f, "Running(pid={pid})"),
            Self::Stopping => write!(f, "Stopping"),
            Self::Stopped { exit_code } => write!(f, "Stopped(code={exit_code:?})"),
            Self::Failed {
                restart_count,
                error,
            } => write!(f, "Failed(restarts={restart_count}, err={error})"),
        }
    }
}

/// 重启决策
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    /// 立即重启
    RestartNow,
    /// 延迟指定时间后重启
    RestartAfter(Duration),
    /// 不重启
    DoNotRestart,
}

/// 被管理的进程
pub struct ManagedProcess {
    /// 进程配置
    pub config: ProcessConfig,
    /// 当前状态
    pub state: ProcessState,
    /// tokio 子进程句柄
    handle: Option<Child>,
    /// 重启时间戳队列（用于频率限制）
    restart_history: VecDeque<Instant>,
}

// ── Windows Job Object 支持 ──────────────────────────────────────────

/// Windows 平台：全局 Job Object 句柄，确保子进程随 supervisor 退出
#[cfg(windows)]
#[allow(unsafe_code)]
mod job {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// 全局 Job Object 初始化结果（进程级单例）。`Err` 也被永久记住，防止
    /// 第一次权限/嵌套失败后后续 spawn 静默绕过 containment。
    static JOB_HANDLE: OnceLock<Result<JobHandle, String>> = OnceLock::new();

    /// RAII 包装，关闭时自动 `CloseHandle`
    struct JobHandle(HANDLE);

    // SAFETY: Job Object 句柄是进程级资源，可以跨线程共享
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn create_job_object() -> Result<JobHandle, String> {
        // The supervisor itself joins before any managed child is created.
        // Children therefore inherit membership in CreateProcess atomically;
        // BREAKAWAY_OK is reserved for explicit native daemon launchers.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                let _ = CloseHandle(job);
                return Err(format!("SetInformationJobObject failed: {error}"));
            }

            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                let error = std::io::Error::last_os_error();
                let _ = CloseHandle(job);
                return Err(format!(
                    "AssignProcessToJobObject(supervisor) failed: {error}"
                ));
            }

            tracing::debug!("Job Object 初始化成功（原子继承 + 显式 daemon breakaway）");
            Ok(JobHandle(job))
        }
    }

    fn job_handle() -> Result<&'static JobHandle, super::SupervisorError> {
        match JOB_HANDLE.get_or_init(create_job_object) {
            Ok(handle) => Ok(handle),
            Err(error) => Err(super::SupervisorError::PlatformError(error.clone())),
        }
    }

    /// Must run before `Command::spawn`: it binds the supervisor to the Job so
    /// the suspended child inherits containment during CreateProcess itself.
    pub fn init_job_object() -> Result<(), super::SupervisorError> {
        let _ = job_handle()?;
        Ok(())
    }

    /// Verify the membership inherited atomically by `CreateProcessW`.
    ///
    /// The supervisor deliberately does not use `CREATE_SUSPENDED`: Rust's
    /// `Command` APIs retain the exact process handle but do not expose the
    /// primary thread handle. Re-discovering a thread by PID/TID is vulnerable
    /// to identifier reuse. Containment does not require suspension because
    /// the already-job-bound parent gives every non-breakaway child membership
    /// as part of process creation, before child code can run.
    pub fn assert_child_in_job(
        child: &tokio::process::Child,
    ) -> Result<(), super::SupervisorError> {
        let job = job_handle()?;
        let process = child.raw_handle().ok_or_else(|| {
            super::SupervisorError::PlatformError(
                "spawned child has no retained Windows process handle".into(),
            )
        })? as HANDLE;
        let mut contained = 0;
        if unsafe { IsProcessInJob(process, job.0, &raw mut contained) } == 0 {
            return Err(super::SupervisorError::PlatformError(format!(
                "IsProcessInJob failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if contained == 0 {
            return Err(super::SupervisorError::PlatformError(
                "child did not atomically inherit supervisor Job".into(),
            ));
        }
        Ok(())
    }
}

impl ManagedProcess {
    /// 创建新的进程管理器（尚未启动）
    #[must_use]
    pub const fn new(config: ProcessConfig) -> Self {
        Self {
            config,
            state: ProcessState::Stopped { exit_code: None },
            handle: None,
            restart_history: VecDeque::new(),
        }
    }

    /// 获取进程标识
    #[must_use]
    pub const fn id(&self) -> &ProcessId {
        &self.config.id
    }

    /// 启动进程
    ///
    /// 根据 `ProcessConfig` 中的策略配置 I/O、进程组等行为。
    pub async fn spawn(&mut self) -> Result<(), SupervisorError> {
        let id = &self.config.id;
        let binary = self
            .config
            .binary_os
            .as_deref()
            .unwrap_or_else(|| std::ffi::OsStr::new(&self.config.binary));
        tracing::info!(%id, binary = ?binary, "启动进程");
        self.state = ProcessState::Starting;

        let mut cmd = Command::new(binary);
        // 防御性收割：tokio 在 Child 句柄被 drop 时发 SIGKILL，避免 supervisor
        // 异常路径（panic / 提前 return）漏掉显式 stop() 时遗留僵尸/孤儿子进程。
        // 正常路径仍走 stop()/wait() 的优雅信号，此处仅兜底。
        let _ = cmd.kill_on_drop(true);
        let _ = cmd.args(&self.config.args);
        if !self.config.inherit_parent_env {
            let _ = cmd.env_clear();
        }
        let _ = cmd.envs(&self.config.env);
        let _ = cmd.envs(&self.config.env_os);

        // 根据 stdio_policy 配置 I/O
        match self.config.stdio_policy {
            StdioPolicy::InheritTerminal => {
                // TUI 进程：stdin+stdout 继承终端；stderr 走 piped 由 forwarder
                // 写文件 `<config-root>/debug/ts-stderr-<pid>-<ts>.log`。
                //
                // 不能 inherit：stderr 直通终端会与 ink 的 ANSI 渲染序列冲突，
                // 把光标位置/重绘弄花。
                // 不能 null：TS 端 panic / console.error / 未捕获异常会全丢，
                // 用户屏幕看不到任何线索（2026-05-14 isModifierPressed
                // TypeError 事故指纹）。
                let _ = cmd.stdin(std::process::Stdio::inherit());
                let _ = cmd.stdout(std::process::Stdio::inherit());
                let _ = cmd.stderr(std::process::Stdio::piped());
            }
            StdioPolicy::Silent => {
                // 后台进程：全部静默
                let _ = cmd.stdin(std::process::Stdio::null());
                let _ = cmd.stdout(std::process::Stdio::null());
                let _ = cmd.stderr(std::process::Stdio::null());
            }
            StdioPolicy::Captured => {
                // PR-C / Bug 7: stdin 静默,stdout/stderr piped,spawn 后逐行推 tracing
                let _ = cmd.stdin(std::process::Stdio::null());
                let _ = cmd.stdout(std::process::Stdio::piped());
                let _ = cmd.stderr(std::process::Stdio::piped());
            }
        }

        // 设置工作目录
        if let Some(ref cwd) = self.config.cwd {
            let _ = cmd.current_dir(cwd);
        }

        // ── 平台特定设置 ──

        // Unix: 后台进程放入独立 process group，便于整组终止。
        // 前台进程（TUI）必须留在 supervisor 的前台进程组中，
        // 否则它尝试读取 stdin 时会收到 SIGTTIN 信号被内核挂起。
        #[cfg(unix)]
        if self.config.process_group == ProcessGroupPolicy::Background {
            // SAFETY: setpgid 是 async-signal-safe 的
            #[allow(unsafe_code)]
            unsafe {
                let _ = cmd.pre_exec(|| {
                    nix::unistd::setpgid(
                        nix::unistd::Pid::from_raw(0),
                        nix::unistd::Pid::from_raw(0),
                    )
                    .map_err(std::io::Error::other)?;
                    Ok(())
                });
            }
        }

        // Windows: bind the supervisor itself to the lifetime Job before
        // CreateProcess. Every managed child gets its own process-group id so
        // programmatic graceful stop can target CTRL_BREAK without pretending
        // an ordinary foreground PID is a group id. The child inherits the Job
        // atomically; no suspended/thread-rediscovery phase is needed.
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

            job::init_job_object()?;
            let _ = cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        // 启动进程
        // mut: PR-C Captured 路径需 child.stdout.take()/stderr.take()
        let mut child = cmd.spawn().map_err(|e| SupervisorError::SpawnFailed {
            id: id.clone(),
            source: e,
        })?;

        let pid = child.id().unwrap_or_else(|| {
            tracing::error!(%id, "child.id() 在 spawn 成功后返回 None，pid 记录为 0；killpg/kill 路径将拒绝信号");
            0
        });
        // Windows: membership must already exist by inheritance. Verification
        // failure is fatal and the exact retained process handle is terminated
        // and reaped; no PID/TID lookup participates in lifecycle decisions.
        #[cfg(windows)]
        {
            if let Err(error) = job::assert_child_in_job(&child) {
                let _ = child.kill().await;
                let _ = child.wait().await;
                self.state = ProcessState::Failed {
                    error: error.to_string(),
                    restart_count: self.restart_history.len() as u32,
                };
                return Err(error);
            }
        }
        tracing::info!(%id, pid, "进程已启动");

        // PR-C / Bug 7: Captured 策略下,spawn 完成后立刻 take 出 stdout/stderr 管道,
        // 各起一个 tokio task 逐行推到 tracing subscriber。take 后 child.handle
        // 上的 stdout/stderr 字段为 None,不影响 wait/kill。
        // InheritTerminal: stderr 走 piped 后必须 take + forward 到文件，
        // 否则 pipe 缓冲区写满 TS 子进程会 block 在 stderr write 上。
        if matches!(self.config.stdio_policy, StdioPolicy::InheritTerminal)
            && let Some(stderr) = child.stderr.take()
        {
            let id = self.config.id.clone();
            let log_path = stderr_log_path_for_inherit_terminal(pid);
            crate::task_registry::global().spawn(
                "supervisor.child.inherit_terminal_stderr_forwarder",
                forward_inherit_terminal_stderr_to_file(id, log_path, stderr),
            );
        }

        if matches!(self.config.stdio_policy, StdioPolicy::Captured) {
            if let Some(stdout) = child.stdout.take() {
                let id = self.config.id.clone();
                // Step 2 Phase D.2: stream forwarder is owned by the
                // process-global TrackedSpawn; panics in the forwarder
                // are now logged at error level. Closes Step 1 §六 R1
                // ① for child.rs:324.
                crate::task_registry::global().spawn(
                    "supervisor.child.stdout_forwarder",
                    forward_captured_stream(id, "stdout", stdout),
                );
            }
            if let Some(stderr) = child.stderr.take() {
                let id = self.config.id.clone();
                // Step 2 Phase D.2: same as stdout above. Closes Step 1
                // §六 R1 ① for child.rs:328.
                crate::task_registry::global().spawn(
                    "supervisor.child.stderr_forwarder",
                    forward_captured_stream(id, "stderr", stderr),
                );
            }
        }

        self.handle = Some(child);
        self.state = ProcessState::Running {
            pid,
            started_at: Instant::now(),
        };

        Ok(())
    }

    /// 优雅停止进程
    pub async fn stop(&mut self, timeout: Duration) -> Result<Option<i32>, SupervisorError> {
        let id = &self.config.id;
        let policy = self.config.process_group;
        tracing::info!(%id, ?timeout, "停止进程");

        let child = if let Some(c) = self.handle.as_mut() {
            c
        } else {
            tracing::warn!(%id, "进程句柄不存在，跳过停止");
            self.state = ProcessState::Stopped { exit_code: None };
            return Ok(None);
        };

        // Do not report a failed signal for a child that already exited, and
        // do not send a console event to a PID that may already have been
        // reused. `try_wait` uses the retained process handle, so this decision
        // is identity-safe.
        if let Some(status) = child.try_wait()? {
            let exit_code = status.code();
            tracing::info!(%id, ?exit_code, "进程在 stop 前已退出");
            self.handle = None;
            self.state = ProcessState::Stopped { exit_code };
            return Ok(exit_code);
        }

        let pid = child.id().unwrap_or_else(|| {
            tracing::error!(%id, "child.id() 返回 None（进程可能已 wait），pid=0 将触发信号路径拒绝");
            0
        });

        // Keep the state Running until the directed graceful signal is known
        // to have been queued. A failed GenerateConsoleCtrlEvent must remain a
        // visible error and keep the process eligible for retry/escalation.
        if let Err(signal_error) = Self::send_graceful_signal(pid, id, policy) {
            // The child can exit between the identity-safe `try_wait` above
            // and the directed signal. Re-check using the same retained
            // process handle: an observed exit is success, but a live child
            // plus a failed signal must remain an error rather than being
            // reported as a graceful-stop request.
            if let Some(status) = child.try_wait()? {
                let exit_code = status.code();
                tracing::info!(%id, ?exit_code, "进程在发送优雅信号竞态窗口内已退出");
                self.handle = None;
                self.state = ProcessState::Stopped { exit_code };
                return Ok(exit_code);
            }
            return Err(signal_error);
        }
        self.state = ProcessState::Stopping;

        let exit_code = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                let code = status.code();
                tracing::info!(%id, pid, ?code, "进程已优雅退出");
                code
            }
            Ok(Err(e)) => {
                tracing::warn!(%id, pid, error = %e, "等待进程退出时出错");
                None
            }
            Err(_) => {
                tracing::warn!(%id, pid, "进程未在超时内退出，强制终止");
                // P1-3 前台孙进程泄漏修：Foreground 进程**没有** setpgid（必须留在
                // supervisor 的前台进程组才能读 stdin），故 force_kill 的 kill(pid) 只杀
                // 它自己、漏掉它 fork 出的孙进程。Foreground 超时强杀走进程树整体终止
                // （kill_process_tree_force 按 ppid 链递归 SIGKILL），把孙进程一并收割。
                // Background 仍走 force_kill 的 killpg（独立 pgid，整组 SIGKILL 已覆盖孙进程）。
                if policy == ProcessGroupPolicy::Foreground && pid != 0 {
                    let result = acosmi_exec::tree_kill::kill_process_tree_force(pid).await;
                    tracing::warn!(%id, pid, killed = result.killed_pids.len(), "前台进程超时，树杀降序终止进程树");
                } else {
                    Self::force_kill(pid, id, policy)?;
                }

                if let Some(ref mut child) = self.handle {
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                }
                None
            }
        };

        self.handle = None;
        self.state = ProcessState::Stopped { exit_code };

        Ok(exit_code)
    }

    /// 等待进程退出
    pub async fn wait(&mut self) -> Result<Option<i32>, SupervisorError> {
        let child = match self.handle.as_mut() {
            Some(c) => c,
            None => return Ok(None),
        };

        let status = child.wait().await?;
        let code = status.code();
        let id = &self.config.id;

        tracing::info!(%id, ?code, "进程已退出");

        self.handle = None;
        self.state = ProcessState::Stopped { exit_code: code };

        Ok(code)
    }

    /// 判断进程是否应该重启，返回重启决策（含延迟信息）
    pub fn should_restart(
        &mut self,
        exit_code: Option<i32>,
        max_restart_count: u32,
        restart_window: Duration,
    ) -> RestartDecision {
        let id = &self.config.id;

        // 检查重启策略是否允许
        let (policy_allows, backoff_params) = match self.config.restart_policy {
            RestartPolicy::Immediate => (true, None),
            RestartPolicy::OnFailure => (exit_code != Some(0), None),
            RestartPolicy::Never => (false, None),
            RestartPolicy::ExponentialBackoff { base_ms, max_ms } => {
                (exit_code != Some(0), Some((base_ms, max_ms)))
            }
            RestartPolicy::MaxRetries(max) => {
                let count = self.restart_history.len() as u32;
                (count < max && exit_code != Some(0), None)
            }
        };

        if !policy_allows {
            tracing::debug!(%id, ?exit_code, policy = ?self.config.restart_policy,
                "重启策略不允许重启");
            return RestartDecision::DoNotRestart;
        }

        // 清理窗口期外的历史记录
        let now = Instant::now();
        while self
            .restart_history
            .front()
            .is_some_and(|t| now.duration_since(*t) > restart_window)
        {
            let _ = self.restart_history.pop_front();
        }

        // 检查窗口期内重启次数
        let count = self.restart_history.len() as u32;
        if count >= max_restart_count {
            tracing::error!(%id, count, max = max_restart_count, ?restart_window,
                "重启次数超限，放弃重启");
            self.state = ProcessState::Failed {
                error: format!("重启次数超限: {count}/{max_restart_count}"),
                restart_count: count,
            };
            return RestartDecision::DoNotRestart;
        }

        self.restart_history.push_back(now);
        tracing::info!(%id, restart_count = count + 1, max = max_restart_count, "准备重启");

        // 根据策略计算延迟
        if let Some((base_ms, max_ms)) = backoff_params {
            // 指数退避: delay = min(base_ms * 2^(count), max_ms)
            let exp = 1u64.checked_shl(count).unwrap_or(u64::MAX);
            let delay_ms = base_ms.saturating_mul(exp);
            let delay_ms = delay_ms.min(max_ms);
            let delay = Duration::from_millis(delay_ms);
            tracing::info!(%id, ?delay, "指数退避重启");
            RestartDecision::RestartAfter(delay)
        } else {
            RestartDecision::RestartNow
        }
    }

    /// 判断进程是否正在运行
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self.state, ProcessState::Running { .. })
    }

    /// 尝试获取进程的 PID
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        match &self.state {
            ProcessState::Running { pid, .. } => Some(*pid),
            _ => self.handle.as_ref().and_then(tokio::process::Child::id),
        }
    }

    /// 取出进程的 stdin/stdout 管道（供 `StdioBridge` 使用）
    pub fn take_stdio_pipes(
        &mut self,
    ) -> Option<(tokio::process::ChildStdout, tokio::process::ChildStdin)> {
        let child = self.handle.as_mut()?;
        let stdout = child.stdout.take()?;
        let stdin = child.stdin.take()?;
        Some((stdout, stdin))
    }

    fn send_graceful_signal(
        pid: u32,
        id: &ProcessId,
        policy: ProcessGroupPolicy,
    ) -> Result<(), SupervisorError> {
        // 拒绝 pid=0：POSIX 下 killpg(0)/kill(0) 会把信号发到调用进程自身的进程组，
        // 即 supervisor 自杀。spawn 路径已对 child.id()=None 做 error 日志，这里再守一次。
        if pid == 0 {
            tracing::error!(%id, ?policy, "pid=0，拒绝发送 SIGTERM (避免 killpg/kill(0) 误伤 supervisor)");
            return Err(SupervisorError::PlatformError(
                "refusing graceful signal for pid=0".into(),
            ));
        }
        tracing::debug!(%id, pid, ?policy, "发送优雅终止信号");

        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill, killpg};
            use nix::unistd::Pid;

            #[allow(clippy::cast_possible_wrap)]
            let target = Pid::from_raw(pid as i32);
            // Foreground 进程未 setpgid，其 pgid 仍是 supervisor 的 pgid；
            // 若用 killpg(pid) 在多数情况返 ESRCH，但若恰好命中 supervisor pgrp 即误伤。
            // 故 Foreground 走 kill(pid)，Background 才走 killpg(pid)。
            let result = match policy {
                ProcessGroupPolicy::Background => killpg(target, Signal::SIGTERM),
                ProcessGroupPolicy::Foreground => kill(target, Some(Signal::SIGTERM)),
            };
            result.map_err(|error| {
                tracing::error!(%id, pid, ?policy, %error, "发送 SIGTERM 失败");
                SupervisorError::PlatformError(format!(
                    "directed SIGTERM failed for pid/group {pid}: {error}"
                ))
            })
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
            use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

            // Every managed Windows child is created with
            // CREATE_NEW_PROCESS_GROUP, including foreground Bun. Therefore
            // its creation PID is a real group id rather than an ordinary PID
            // being misused as one. CTRL_BREAK is supported for targeted groups
            // even though CREATE_NEW_PROCESS_GROUP disables CTRL_C delivery.
            let _ = policy;
            #[allow(unsafe_code)]
            let ret = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            if ret == 0 {
                let error = std::io::Error::last_os_error();
                tracing::error!(%id, pid, error = %error, "GenerateConsoleCtrlEvent 失败");
                return Err(SupervisorError::PlatformError(format!(
                    "GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group={pid}) failed: {error}"
                )));
            }
            tracing::debug!(%id, pid, "已定向发送 CTRL_BREAK_EVENT");
            Ok(())
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = policy;
            Err(SupervisorError::PlatformError("不支持的平台".into()))
        }
    }

    fn force_kill(
        pid: u32,
        id: &ProcessId,
        policy: ProcessGroupPolicy,
    ) -> Result<(), SupervisorError> {
        if pid == 0 {
            tracing::error!(%id, ?policy, "pid=0，拒绝发送 SIGKILL (避免 killpg/kill(0) 误伤 supervisor)");
            return Ok(());
        }
        tracing::warn!(%id, pid, ?policy, "强制终止进程");

        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill, killpg};
            use nix::unistd::Pid;

            #[allow(clippy::cast_possible_wrap)]
            let target = Pid::from_raw(pid as i32);
            let result = match policy {
                ProcessGroupPolicy::Background => killpg(target, Signal::SIGKILL),
                ProcessGroupPolicy::Foreground => kill(target, Some(Signal::SIGKILL)),
            };
            if let Err(e) = result
                && e != nix::errno::Errno::ESRCH
            {
                tracing::error!(%id, pid, ?policy, error = %e, "发送 SIGKILL 失败");
                return Err(SupervisorError::PlatformError(format!("SIGKILL 失败: {e}")));
            }
            Ok(())
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_TERMINATE, TerminateProcess,
            };

            let _ = policy; // Windows 路径靠 Job Object 整组终止，不区分 policy
            #[allow(unsafe_code)]
            unsafe {
                let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    tracing::debug!(%id, pid, "OpenProcess 失败，进程可能已退出");
                    return Ok(());
                }

                let ret = TerminateProcess(process, 1);
                let _ = CloseHandle(process);

                if ret == 0 {
                    tracing::warn!(%id, pid, "TerminateProcess 失败");
                    return Err(SupervisorError::PlatformError(format!(
                        "TerminateProcess 失败: pid={pid}"
                    )));
                }
            }

            Ok(())
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = policy;
            Err(SupervisorError::PlatformError("不支持的平台".into()))
        }
    }
}

/// PR-C / Bug 7: 把 `StdioPolicy::Captured` 下 piped 的 stdout/stderr 逐行
/// 转发到 tracing subscriber。
///
/// 设计:
///   - 每行 `tracing::warn!` 带 `child_id` + `stream` + `message` 三字段,
///     便于下游订阅者按 id 过滤;level 用 warn 以便默认 subscriber 看见
///     (stdout 里未必是错误信息,但默认 info 级别在打包版常常关掉)
///   - EOF 正常退出;读取错误只 debug,不 panic/re-spawn (进程自身故障由
///     watchdog 管,forwarder 仅负责输出搬运)
///   - `tokio::spawn` 后此函数 move 语义独占 reader,无生命周期悬挂
async fn forward_captured_stream<R>(id: ProcessId, stream: &'static str, reader: R)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::warn!(child_id = %id, stream = stream, message = %line);
            }
            Ok(None) => {
                tracing::debug!(child_id = %id, stream = stream, "Captured 流 EOF");
                break;
            }
            Err(e) => {
                tracing::debug!(child_id = %id, stream = stream, error = %e, "读取 Captured 流失败");
                break;
            }
        }
    }
}

/// InheritTerminal 路径下的 stderr 落地目标。
///
/// 路径：`<config-root>/debug/ts-stderr-<pid>-<unix_ms>.log`。
/// `<pid>` + 启动 ms 时间戳避免同主机多 supervisor 实例覆盖；append 模式
/// 让单次启动内的所有 stderr 行连续落入同一文件，事故复盘看一份就够。
///
/// 根目录复用仓库统一配置解析器：`CRABCODE_CONFIG_DIR` 是最高优先级的完整
/// root；否则使用 `<CRABCODE_HOME-or-OS-home>/.crabcode`。不得在这里另建一套
/// HOME 解析，否则隔离运行仍会把 stderr 写回真实用户目录。
fn stderr_log_path_for_inherit_terminal(pid: u32) -> std::path::PathBuf {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    acosmi_config::paths::resolve_config_home_dir()
        .join("debug")
        .join(format!("ts-stderr-{pid}-{ts_ms}.log"))
}

/// 把 `StdioPolicy::InheritTerminal` 下 piped 的 stderr 逐行 append 到日志文件。
///
/// 设计:
///   - 不走 tracing — supervisor 自身 tracing 默认 sink 是 stderr，TUI 启动后
///     再 warn 会污染 ink 渲染（ANSI 序列冲突）
///   - 不走 Captured 路径（`forward_captured_stream`）— 该路径推 tracing，
///     与上一条同因
///   - 父目录缺失自动 mkdir -p；append 模式，多次启动同 pid 不会互相覆盖
///   - 文件打开 / 写入失败只 debug，不 panic / re-spawn（输出搬运失败不影响
///     被监控进程的生命周期）
///   - EOF 正常退出；读取错误只 debug
async fn forward_inherit_terminal_stderr_to_file<R>(
    id: ProcessId,
    log_path: std::path::PathBuf,
    reader: R,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = log_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        tracing::debug!(
            child_id = %id,
            path = %log_path.display(),
            error = %e,
            "创建 InheritTerminal stderr 日志目录失败，停止 forwarder"
        );
        return;
    }

    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(
                child_id = %id,
                path = %log_path.display(),
                error = %e,
                "打开 InheritTerminal stderr 日志文件失败，停止 forwarder"
            );
            return;
        }
    };

    tracing::info!(
        child_id = %id,
        path = %log_path.display(),
        "InheritTerminal stderr forwarder 已启动"
    );

    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let payload = format!("{line}\n");
                if let Err(e) = file.write_all(payload.as_bytes()).await {
                    tracing::debug!(
                        child_id = %id,
                        error = %e,
                        "写 InheritTerminal stderr 日志失败，停止 forwarder"
                    );
                    break;
                }
            }
            Ok(None) => {
                tracing::debug!(child_id = %id, "InheritTerminal stderr EOF");
                break;
            }
            Err(e) => {
                tracing::debug!(
                    child_id = %id,
                    error = %e,
                    "读 InheritTerminal stderr 失败"
                );
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{IpcType, ProcessGroupPolicy, StdioPolicy};
    use std::collections::HashMap;

    fn make_config(id: &str, policy: RestartPolicy) -> ProcessConfig {
        ProcessConfig {
            id: ProcessId::new(id),
            binary: "echo".to_string(),
            binary_os: None,
            args: vec!["hello".to_string()],
            env: HashMap::new(),
            env_os: HashMap::new(),
            inherit_parent_env: true,
            cwd: None,
            restart_policy: policy,
            health_check_interval: Duration::from_secs(5),
            stdio_policy: StdioPolicy::Silent,
            ipc_type: IpcType::None,
            process_group: ProcessGroupPolicy::Background,
            depends_on: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_environment_probe_child() {
        use std::os::unix::ffi::OsStrExt as _;

        if std::env::var_os("CRABCODE_ENV_OS_TEST_PROBE").is_none() {
            return;
        }
        let expected_name = b"CRABCODE_\xff_PRESERVED";
        let expected_value = b"value-\xfe";
        assert!(
            std::env::vars_os().any(|(name, value)| {
                name.as_os_str().as_bytes() == expected_name
                    && value.as_os_str().as_bytes() == expected_value
            }),
            "managed child did not receive the exact non-UTF-8 environment bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_environment_round_trips_through_managed_process() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let current_exe = std::env::current_exe().expect("current test executable");
        let mut config = make_config("env-os-round-trip", RestartPolicy::Never);
        config.binary = current_exe
            .to_str()
            .expect("Cargo test executable path must be Unicode")
            .to_owned();
        config.args = vec![
            "--exact".into(),
            "child::tests::non_utf8_environment_probe_child".into(),
            "--nocapture".into(),
        ];
        config.env = HashMap::from([("CRABCODE_ENV_OS_TEST_PROBE".into(), "1".into())]);
        config.env_os = HashMap::from([(
            OsString::from_vec(b"CRABCODE_\xff_PRESERVED".to_vec()),
            OsString::from_vec(b"value-\xfe".to_vec()),
        )]);
        config.inherit_parent_env = false;
        config.stdio_policy = StdioPolicy::Captured;

        let mut process = ManagedProcess::new(config);
        process.spawn().await.expect("spawn environment probe");
        assert_eq!(
            process.wait().await.expect("wait for environment probe"),
            Some(0)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn non_utf8_executable_path_is_spawned_without_lossy_conversion() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let temp = tempfile::tempdir().expect("temporary executable directory");
        let executable = temp
            .path()
            .join(OsString::from_vec(b"crabcode-\xff-probe".to_vec()));
        // A successful exec of this uniquely named script is the direct
        // contract: lossy conversion would address a different pathname and
        // fail before the script can return zero. Avoid current_exe() here;
        // Linux procfs is not required to preserve the spelling used at exec.
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write probe executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("probe executable metadata")
            .permissions();
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(0o700);
        }
        std::fs::set_permissions(&executable, permissions).expect("make probe executable");

        let mut config = make_config("binary-os-round-trip", RestartPolicy::Never);
        config.binary = "this-unicode-fallback-must-not-be-used".into();
        config.binary_os = Some(executable.into_os_string());
        config.args.clear();
        config.env.clear();
        config.inherit_parent_env = false;
        config.stdio_policy = StdioPolicy::Silent;

        let mut process = ManagedProcess::new(config);
        process.spawn().await.expect("spawn non-UTF-8 executable");
        assert_eq!(
            process.wait().await.expect("wait for executable probe"),
            Some(0)
        );
    }

    #[test]
    fn test_should_restart_immediate() {
        let config = make_config("test", RestartPolicy::Immediate);
        let mut proc = ManagedProcess::new(config);
        assert_eq!(
            proc.should_restart(Some(0), 5, Duration::from_secs(60)),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), 5, Duration::from_secs(60)),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(None, 5, Duration::from_secs(60)),
            RestartDecision::RestartNow
        );
    }

    #[test]
    fn test_should_restart_on_failure() {
        let config = make_config("test", RestartPolicy::OnFailure);
        let mut proc = ManagedProcess::new(config);
        assert_eq!(
            proc.should_restart(Some(0), 5, Duration::from_secs(60)),
            RestartDecision::DoNotRestart
        );
        assert_eq!(
            proc.should_restart(Some(1), 5, Duration::from_secs(60)),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(None, 5, Duration::from_secs(60)),
            RestartDecision::RestartNow
        );
    }

    #[test]
    fn test_should_restart_never() {
        let config = make_config("test", RestartPolicy::Never);
        let mut proc = ManagedProcess::new(config);
        assert_eq!(
            proc.should_restart(Some(0), 5, Duration::from_secs(60)),
            RestartDecision::DoNotRestart
        );
        assert_eq!(
            proc.should_restart(Some(1), 5, Duration::from_secs(60)),
            RestartDecision::DoNotRestart
        );
    }

    #[test]
    fn test_restart_limit() {
        let config = make_config("test", RestartPolicy::Immediate);
        let mut proc = ManagedProcess::new(config);
        let window = Duration::from_secs(60);
        let max = 3;
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::DoNotRestart
        );
    }

    #[test]
    fn test_should_restart_max_retries() {
        let config = make_config("test", RestartPolicy::MaxRetries(2));
        let mut proc = ManagedProcess::new(config);
        let window = Duration::from_secs(60);
        let max = 10;
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartNow
        );
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::DoNotRestart
        );
    }

    #[test]
    fn test_should_restart_exponential_backoff() {
        let config = make_config(
            "test",
            RestartPolicy::ExponentialBackoff {
                base_ms: 100,
                max_ms: 5000,
            },
        );
        let mut proc = ManagedProcess::new(config);
        let window = Duration::from_secs(60);
        let max = 10;
        // 第 1 次: 100ms * 2^0 = 100ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(100))
        );
        // 第 2 次: 100ms * 2^1 = 200ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(200))
        );
        // 第 3 次: 100ms * 2^2 = 400ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(400))
        );
        // exit_code=0 时不重启
        assert_eq!(
            proc.should_restart(Some(0), max, window),
            RestartDecision::DoNotRestart
        );
    }

    #[test]
    fn test_exponential_backoff_caps_at_max() {
        let config = make_config(
            "test",
            RestartPolicy::ExponentialBackoff {
                base_ms: 1000,
                max_ms: 3000,
            },
        );
        let mut proc = ManagedProcess::new(config);
        let window = Duration::from_secs(60);
        let max = 10;
        // 第 1 次: 1000ms * 2^0 = 1000ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(1000))
        );
        // 第 2 次: 1000ms * 2^1 = 2000ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(2000))
        );
        // 第 3 次: 1000ms * 2^2 = 4000ms → capped at 3000ms
        assert_eq!(
            proc.should_restart(Some(1), max, window),
            RestartDecision::RestartAfter(Duration::from_millis(3000))
        );
    }

    #[test]
    fn test_new_process_state() {
        let config = make_config("test", RestartPolicy::Immediate);
        let proc = ManagedProcess::new(config);
        assert!(!proc.is_running());
        assert_eq!(proc.pid(), None);
    }

    #[test]
    fn test_process_id_accessor() {
        let config = make_config("my-service", RestartPolicy::Never);
        let proc = ManagedProcess::new(config);
        assert_eq!(proc.id(), &ProcessId::new("my-service"));
    }

    /// 防回归 — pid=0 时 send_graceful_signal 必须短路返 Err，
    /// 绝不进入 killpg/kill 路径（POSIX 下 killpg(0)/kill(0) = 信号发到调用进程的 pgrp = supervisor 自杀）。
    #[test]
    fn test_send_graceful_signal_refuses_pid_zero() {
        let id = ProcessId::new("regression-pid0-graceful");
        assert!(
            ManagedProcess::send_graceful_signal(0, &id, ProcessGroupPolicy::Background).is_err()
        );
        assert!(
            ManagedProcess::send_graceful_signal(0, &id, ProcessGroupPolicy::Foreground).is_err()
        );
    }

    /// 防回归 — pid=0 时 force_kill 必须短路返 Ok，绝不进入 killpg/kill。
    #[test]
    fn test_force_kill_refuses_pid_zero() {
        let id = ProcessId::new("regression-pid0-force");
        assert!(ManagedProcess::force_kill(0, &id, ProcessGroupPolicy::Background).is_ok());
        assert!(ManagedProcess::force_kill(0, &id, ProcessGroupPolicy::Foreground).is_ok());
    }

    /// 防回归 — `stderr_log_path_for_inherit_terminal` 必须落到
    /// `<resolved-config-home>/debug/ts-stderr-<pid>-<ts_ms>.log` 形态，
    /// 同 pid 短时间内连续调用得到不同时间戳（避免覆盖）。
    #[test]
    #[serial_test::serial]
    fn test_stderr_log_path_for_inherit_terminal_shape() {
        let p1 = super::stderr_log_path_for_inherit_terminal(12345);
        let s1 = p1.to_string_lossy().into_owned();
        assert!(s1.contains("debug"), "路径应含 debug 段: {s1}");
        assert!(s1.contains("ts-stderr-12345-"), "路径应含 pid 段: {s1}");
        assert!(s1.ends_with(".log"), "路径应以 .log 结尾: {s1}");

        // 同 pid 二次调用应得不同时间戳（避免日志互相覆盖）。
        std::thread::sleep(Duration::from_millis(2));
        let p2 = super::stderr_log_path_for_inherit_terminal(12345);
        assert_ne!(p1, p2, "同 pid 两次调用必须不同时间戳");
    }

    #[test]
    #[serial_test::serial]
    fn test_stderr_log_path_honors_config_dir_before_home() {
        struct EnvRestore {
            config: Option<std::ffi::OsString>,
            crabcode_home: Option<std::ffi::OsString>,
            home: Option<std::ffi::OsString>,
        }

        impl Drop for EnvRestore {
            fn drop(&mut self) {
                for (name, value) in [
                    ("CRABCODE_CONFIG_DIR", self.config.take()),
                    ("CRABCODE_HOME", self.crabcode_home.take()),
                    ("HOME", self.home.take()),
                ] {
                    #[allow(unsafe_code)]
                    unsafe {
                        if let Some(value) = value {
                            std::env::set_var(name, value);
                        } else {
                            std::env::remove_var(name);
                        }
                    }
                }
            }
        }

        let config = tempfile::tempdir().expect("config tempdir");
        let other_home = tempfile::tempdir().expect("home tempdir");
        let _restore = EnvRestore {
            config: std::env::var_os("CRABCODE_CONFIG_DIR"),
            crabcode_home: std::env::var_os("CRABCODE_HOME"),
            home: std::env::var_os("HOME"),
        };
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("CRABCODE_CONFIG_DIR", config.path());
            std::env::set_var("CRABCODE_HOME", other_home.path());
            std::env::set_var("HOME", other_home.path());
        }

        let path = super::stderr_log_path_for_inherit_terminal(12345);
        let canonical_config = config
            .path()
            .canonicalize()
            .expect("canonical config tempdir");
        let canonical_home = other_home
            .path()
            .canonicalize()
            .expect("canonical home tempdir");
        assert!(
            path.starts_with(canonical_config.join("debug")),
            "path={path:?}"
        );
        assert!(
            !path.starts_with(canonical_home),
            "config override lost to home: {path:?}"
        );
    }

    /// 端到端 — `forward_inherit_terminal_stderr_to_file` 必须把读到的
    /// 行 append 到指定文件，包括缺父目录时自动 mkdir -p。
    ///
    /// 这是 InheritTerminal 路径 stderr 落地的等价验证：
    /// 真链路 spawn 时 `child.stderr.take()` 拿到的 reader 与本测试用
    /// `tokio::io::duplex` 提供的 reader 在 forwarder 端语义一致，关键
    /// 行为（按行解析 / append / EOF 退出）此 test 已覆盖。
    /// TUI 端到端复现需登录态 + 交互终端，详 audit 报告 §验证补充。
    #[tokio::test]
    async fn test_forward_inherit_terminal_stderr_writes_lines() {
        use tokio::io::AsyncWriteExt;

        // 唯一 tmpdir 避免 cargo test 并发互踩。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!(
            "acosmi-supervisor-test-stderr-fwd-{}-{}",
            std::process::id(),
            nanos
        ));
        // 故意指向不存在的嵌套子目录，验 forwarder mkdir -p 行为。
        let log_path = tmp.join("nested").join("ts-stderr-test.log");

        let (read_half, mut write_half) = tokio::io::duplex(64);
        let writer = async move {
            write_half
                .write_all(b"line1\nTEST_STDERR_FORWARD\nline3\n")
                .await
                .unwrap();
            // drop write_half 触发 EOF。
        };
        let forwarder = super::forward_inherit_terminal_stderr_to_file(
            ProcessId::new("test-inherit-stderr"),
            log_path.clone(),
            read_half,
        );

        // duplex(64) 缓冲小，必须并发跑两端否则 write_all 阻塞死锁。
        // 不能用 tokio::spawn —— supervisor crate 禁用 (走 task_registry)。
        tokio::join!(writer, forwarder);

        let contents = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            contents.contains("TEST_STDERR_FORWARD"),
            "日志缺事故复盘 marker: {contents}"
        );
        assert!(contents.contains("line1"), "缺 line1: {contents}");
        assert!(contents.contains("line3"), "缺 line3: {contents}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
