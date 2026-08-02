//! 信号处理与优雅关闭协议
//!
//! 提供两项核心能力：
//! 1. **信号处理** — `install_signal_handler()` 安装全局信号监听器，
//!    将 OS 信号映射为 `CancellationToken`，第二次信号触发强制退出。
//! 2. **三级关闭协议** — `ShutdownCoordinator` 按 soft stop → drain → hard kill
//!    三阶段有序停止所有子进程，确保数据不丢失。

use crate::child::ManagedProcess;
use crate::topology::ProcessId;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// 安装全局信号处理器，返回一个 `CancellationToken`。
///
/// 当收到 SIGTERM/SIGINT/SIGHUP（Unix）或 Ctrl+C/Ctrl+Break（Windows）时，
/// token 被 cancel。
/// 第二次收到信号时进程强制退出（防止卡死）。
#[must_use]
pub fn install_signal_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let token_cloned = token.clone();

    // Step 2 Phase D.2: signal-waiting task is registered with the
    // process-global TrackedSpawn so a panic is logged at error level
    // (instead of disappearing) and the JoinHandle is owned for the
    // process's lifetime. Closes Step 1 §六 R1 ① for shutdown.rs:23.
    crate::task_registry::global().spawn("supervisor.shutdown.signal_handler", async move {
        wait_for_signal().await;
        tracing::info!("收到终止信号，开始优雅关闭");
        token_cloned.cancel();

        wait_for_signal().await;
        tracing::error!("再次收到终止信号，强制退出");
        std::process::exit(1);
    });

    token
}

/// 等待一次终止信号
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = signal(SignalKind::terminate()).expect("无法注册 SIGTERM 处理器");
        let mut sigint = signal(SignalKind::interrupt()).expect("无法注册 SIGINT 处理器");
        let mut sighup = signal(SignalKind::hangup()).expect("无法注册 SIGHUP 处理器");

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::debug!("收到 SIGTERM");
            }
            _ = sigint.recv() => {
                tracing::debug!("收到 SIGINT");
            }
            _ = sighup.recv() => {
                tracing::debug!("收到 SIGHUP");
            }
        }
    }

    #[cfg(windows)]
    {
        let mut ctrl_break =
            tokio::signal::windows::ctrl_break().expect("无法注册 Ctrl+Break 处理器");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.expect("无法注册 Ctrl+C 处理器");
                tracing::debug!("收到 Ctrl+C");
            }
            _ = ctrl_break.recv() => {
                tracing::debug!("收到 Ctrl+Break");
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        tracing::warn!("当前平台不支持信号处理");
        std::future::pending::<()>().await;
    }
}

// ── 关闭报告 ─────────────────────────────────────────────────────────

/// 关闭报告：记录每个进程的停止结果
#[derive(Debug)]
pub struct ShutdownReport {
    /// 已停止的进程列表：(标识, 退出码)
    pub processes_stopped: Vec<(ProcessId, Option<i32>)>,
    /// 被强制 kill 的进程标识列表
    pub forced_kills: Vec<ProcessId>,
    /// 整个关闭流程的总耗时
    pub total_duration: Duration,
}

impl std::fmt::Display for ShutdownReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== 关闭报告 ===")?;
        writeln!(f, "总耗时: {:?}", self.total_duration)?;
        writeln!(f, "已停止进程:")?;
        for (id, code) in &self.processes_stopped {
            writeln!(f, "  - {id}: 退出码={code:?}")?;
        }
        if !self.forced_kills.is_empty() {
            writeln!(f, "被强制 kill 的进程:")?;
            for id in &self.forced_kills {
                writeln!(f, "  - {id}")?;
            }
        }
        Ok(())
    }
}

// ── 关闭协调器 ───────────────────────────────────────────────────────

/// 关闭协调器：执行三级关闭协议
///
/// 将总超时时间三等分：
/// 1. **Soft stop**（前 1/3）：发送 SIGTERM / 关闭 stdin，等待进程自行退出
/// 2. **Drain**（中 1/3）：给进程时间 flush 数据和处理未完成请求
/// 3. **Hard kill**（后 1/3）：对仍存活的进程发送 SIGKILL / `TerminateProcess`
pub struct ShutdownCoordinator {
    timeout: Duration,
}

impl ShutdownCoordinator {
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// 执行有序关闭
    pub async fn execute(&self, processes: &mut [&mut ManagedProcess]) -> ShutdownReport {
        let start = Instant::now();
        let phase_timeout = self.timeout / 3;

        let mut report = ShutdownReport {
            processes_stopped: Vec::new(),
            forced_kills: Vec::new(),
            total_duration: Duration::ZERO,
        };

        tracing::info!(
            total_timeout = ?self.timeout,
            phase_timeout = ?phase_timeout,
            process_count = processes.len(),
            "开始三级关闭协议"
        );

        // ── 第一阶段：Soft stop ──
        tracing::info!("=== 阶段一：Soft stop ===");

        let per_process_soft = if processes.is_empty() {
            phase_timeout
        } else {
            phase_timeout / processes.len() as u32
        };

        for process in processes.iter_mut() {
            let id = process.id().clone();
            if !process.is_running() {
                tracing::debug!(%id, "进程未在运行，跳过 soft stop");
                let exit_code = match &process.state {
                    crate::child::ProcessState::Stopped { exit_code } => *exit_code,
                    _ => None,
                };
                report.processes_stopped.push((id, exit_code));
                continue;
            }

            tracing::info!(%id, timeout = ?per_process_soft, "发送优雅终止信号");
            match process.stop(per_process_soft).await {
                Ok(code) => {
                    tracing::info!(%id, ?code, "进程在 soft stop 阶段退出");
                    report.processes_stopped.push((id, code));
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "soft stop 失败");
                }
            }
        }

        // ── 第二阶段：Drain ──
        tracing::info!("=== 阶段二：Drain（等待数据刷盘）===");

        let still_running: Vec<usize> = processes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_running())
            .map(|(i, _)| i)
            .collect();

        if still_running.is_empty() {
            tracing::info!("所有进程已在 soft stop 阶段退出，跳过 drain");
        } else {
            tracing::info!(
                count = still_running.len(),
                "仍有进程在运行，等待 drain 超时"
            );

            let per_process_drain = if still_running.is_empty() {
                phase_timeout
            } else {
                phase_timeout / still_running.len() as u32
            };

            for &idx in &still_running {
                let process = &mut processes[idx];
                let id = process.id().clone();
                tracing::info!(%id, timeout = ?per_process_drain, "等待进程 drain");

                match process.stop(per_process_drain).await {
                    Ok(code) => {
                        tracing::info!(%id, ?code, "进程在 drain 阶段退出");
                        report.processes_stopped.push((id, code));
                    }
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "drain 阶段停止失败");
                    }
                }
            }
        }

        // ── 第三阶段：Hard kill ──
        tracing::info!("=== 阶段三：Hard kill ===");

        let still_alive: Vec<usize> = processes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_running())
            .map(|(i, _)| i)
            .collect();

        if still_alive.is_empty() {
            tracing::info!("所有进程已退出，无需 hard kill");
        } else {
            tracing::warn!(count = still_alive.len(), "仍有进程未退出，执行强制终止");

            for &idx in &still_alive {
                let process = &mut processes[idx];
                let id = process.id().clone();
                tracing::warn!(%id, "强制终止进程");

                match process.stop(Duration::ZERO).await {
                    Ok(code) => {
                        tracing::info!(%id, ?code, "进程已被强制终止");
                        report.processes_stopped.push((id.clone(), code));
                    }
                    Err(e) => {
                        tracing::error!(%id, error = %e, "强制终止失败");
                        report.processes_stopped.push((id.clone(), None));
                    }
                }
                report.forced_kills.push(id);
            }
        }

        report.total_duration = start.elapsed();

        tracing::info!(
            total_duration = ?report.total_duration,
            stopped = report.processes_stopped.len(),
            forced = report.forced_kills.len(),
            "关闭流程完成"
        );

        report
    }
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::child::ManagedProcess;
    use crate::topology::{
        IpcType, ProcessConfig, ProcessGroupPolicy, ProcessId, RestartPolicy, StdioPolicy,
    };
    use std::collections::HashMap;

    fn make_config(id: &str) -> ProcessConfig {
        ProcessConfig {
            id: ProcessId::new(id),
            binary: "echo".to_string(),
            binary_os: None,
            args: vec!["hello".to_string()],
            env: HashMap::new(),
            env_os: HashMap::new(),
            inherit_parent_env: true,
            cwd: None,
            restart_policy: RestartPolicy::OnFailure,
            health_check_interval: Duration::from_secs(5),
            stdio_policy: StdioPolicy::Silent,
            ipc_type: IpcType::None,
            process_group: ProcessGroupPolicy::Background,
            depends_on: vec![],
        }
    }

    #[tokio::test]
    async fn test_install_signal_handler_returns_uncancelled_token() {
        let token = install_signal_handler();
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancellation_token_propagation() {
        let token = CancellationToken::new();
        let child_token = token.child_token();
        assert!(!child_token.is_cancelled());
        token.cancel();
        assert!(child_token.is_cancelled());
    }

    #[test]
    fn test_coordinator_new() {
        let timeout = Duration::from_secs(30);
        let coord = ShutdownCoordinator::new(timeout);
        assert_eq!(coord.timeout, timeout);
    }

    #[test]
    fn test_shutdown_report_display() {
        let report = ShutdownReport {
            processes_stopped: vec![
                (ProcessId::ts_session(), Some(0)),
                (ProcessId::new("test-worker"), Some(1)),
            ],
            forced_kills: vec![ProcessId::new("test-worker")],
            total_duration: Duration::from_millis(2500),
        };
        let output = format!("{report}");
        assert!(output.contains("关闭报告"));
        assert!(output.contains("ts-session"));
        assert!(output.contains("test-worker"));
        assert!(output.contains("强制 kill"));
    }

    #[test]
    fn test_shutdown_report_display_no_forced() {
        let report = ShutdownReport {
            processes_stopped: vec![(ProcessId::ts_session(), Some(0))],
            forced_kills: vec![],
            total_duration: Duration::from_millis(500),
        };
        let output = format!("{report}");
        assert!(output.contains("ts-session"));
        assert!(!output.contains("强制 kill"));
    }

    #[tokio::test]
    async fn test_execute_with_already_stopped() {
        let mut ts = ManagedProcess::new(make_config("ts-session"));
        let mut go = ManagedProcess::new(make_config("test-worker"));

        let coord = ShutdownCoordinator::new(Duration::from_secs(5));
        let report = coord.execute(&mut [&mut ts, &mut go]).await;

        assert_eq!(report.processes_stopped.len(), 2);
        assert!(report.forced_kills.is_empty());
        assert!(report.total_duration < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_execute_with_empty() {
        let coord = ShutdownCoordinator::new(Duration::from_secs(5));
        let report = coord.execute(&mut []).await;
        assert!(report.processes_stopped.is_empty());
        assert!(report.forced_kills.is_empty());
    }

    #[tokio::test]
    async fn test_execute_shutdown_order_preserved() {
        let mut ts = ManagedProcess::new(make_config("ts-session"));
        let mut go = ManagedProcess::new(make_config("test-worker"));

        let coord = ShutdownCoordinator::new(Duration::from_secs(5));
        let report = coord.execute(&mut [&mut ts, &mut go]).await;

        assert_eq!(report.processes_stopped.len(), 2);
        assert_eq!(report.processes_stopped[0].0, ProcessId::new("ts-session"));
        assert_eq!(report.processes_stopped[1].0, ProcessId::new("test-worker"));
    }

    #[tokio::test]
    async fn test_execute_with_real_fast_exiting_process() {
        let config = ProcessConfig {
            id: ProcessId::new("fast-exit"),
            #[cfg(unix)]
            binary: "true".to_string(),
            #[cfg(windows)]
            binary: "cmd".to_string(),
            binary_os: None,
            #[cfg(unix)]
            args: vec![],
            #[cfg(windows)]
            args: vec!["/C".to_string(), "exit".to_string(), "0".to_string()],
            env: HashMap::new(),
            env_os: HashMap::new(),
            inherit_parent_env: true,
            cwd: None,
            restart_policy: RestartPolicy::Never,
            health_check_interval: Duration::from_secs(5),
            stdio_policy: StdioPolicy::Silent,
            ipc_type: IpcType::None,
            process_group: ProcessGroupPolicy::Background,
            depends_on: vec![],
        };

        let mut child = ManagedProcess::new(config);
        child.spawn().await.expect("启动进程失败");
        tokio::time::sleep(Duration::from_millis(500)).await;

        let coord = ShutdownCoordinator::new(Duration::from_secs(5));
        let report = coord.execute(&mut [&mut child]).await;

        assert_eq!(report.processes_stopped.len(), 1);
        assert_eq!(report.processes_stopped[0].0, ProcessId::new("fast-exit"));
    }

    #[tokio::test]
    async fn test_coordinator_timeout_division() {
        let timeout = Duration::from_secs(9);
        let coord = ShutdownCoordinator::new(timeout);
        let phase = coord.timeout / 3;
        assert_eq!(phase, Duration::from_secs(3));
    }

    #[tokio::test]
    async fn test_coordinator_small_timeout() {
        let coord = ShutdownCoordinator::new(Duration::from_millis(100));
        let mut proc = ManagedProcess::new(make_config("test"));
        let report = coord.execute(&mut [&mut proc]).await;
        assert_eq!(report.processes_stopped.len(), 1);
    }

    #[tokio::test]
    async fn test_coordinator_zero_timeout() {
        let coord = ShutdownCoordinator::new(Duration::ZERO);
        let mut proc = ManagedProcess::new(make_config("test"));
        let report = coord.execute(&mut [&mut proc]).await;
        assert_eq!(report.processes_stopped.len(), 1);
    }
}
