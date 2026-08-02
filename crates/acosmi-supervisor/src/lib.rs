//! Acosmi Supervisor — 通用进程生命周期管理
//!
//! 核心职责：
//! - 按配置顺序启动任意数量的进程
//! - `运行主监控循环（tokio::select`! 多路复用）
//! - 处理进程退出和重启（响应 watchdog 事件）
//! - 执行优雅关闭（逆序停止）
//! - 管理 IPC 服务（后台任务）

pub mod child;
// `discipline` exports the `silent_drop!` and `must_log_err!` macros at
// crate root via `#[macro_export]`; the module itself is `pub` so doc
// tests can resolve `$crate` paths from inside the macro definition.
pub mod discipline;
pub mod ipc;
pub mod process_registry;
pub mod shutdown;
pub mod status;
pub mod task_registry;
pub mod topology;
pub mod watchdog;
pub mod windows_process_tree;

// 重导出核心类型
pub use child::{ManagedProcess, ProcessState, RestartDecision, SupervisorError};
pub use topology::{
    ConfigError, IpcType, ProcessConfig, ProcessGroupPolicy, ProcessId, RestartPolicy, StdioPolicy,
    SupervisorConfig,
};
pub use watchdog::{ProcessWatchdog, ProcessWatchdogConfig, ProcessWatchdogEvent};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use acosmi_executor::CommandExecutor;
use acosmi_heartbeat::{LivenessConfig, LivenessState};

// ────────────────────────── Supervisor 主结构体 ──────────────────────────

/// Supervisor 主结构体，管理进程的完整生命周期
pub struct Supervisor {
    /// supervisor 全局配置
    config: SupervisorConfig,
    /// 进程映射表（ProcessId -> `ManagedProcess`）
    processes: HashMap<ProcessId, ManagedProcess>,
    /// 全局取消令牌，用于协调优雅关闭
    shutdown_token: CancellationToken,
    /// watchdog 事件接收端
    watchdog_rx: mpsc::Receiver<ProcessWatchdogEvent>,
    /// 进程看门狗（通用版，基于 `acosmi_heartbeat::LivenessTracker`）
    watchdog: ProcessWatchdog,
    /// 统一命令执行器（共享引用，传给 `IpcServer` 处理 Go 的 `CapabilityReq`）
    executor: Arc<CommandExecutor>,
}

impl Supervisor {
    /// 创建新的 Supervisor 实例
    pub fn new(config: SupervisorConfig) -> Self {
        // 1. 安装信号处理器
        let shutdown_token = shutdown::install_signal_handler();

        // 2. 创建 watchdog 事件通道
        let (watchdog_tx, watchdog_rx) = mpsc::channel::<ProcessWatchdogEvent>(32);

        // 3. 创建通用进程看门狗
        let watchdog_config = ProcessWatchdogConfig::default();
        let default_liveness = watchdog_config.default_liveness.clone();
        let mut watchdog = ProcessWatchdog::new(watchdog_config, watchdog_tx);

        // 4. 从配置中创建所有 ManagedProcess 实例，并按需注册到看门狗
        //
        // 根因修（2026-04-23）：原实现对所有进程都用 default_liveness 注册，
        // `ProcessConfig.health_check_interval` 是**死字段**——改它无效果。
        // 现在把它作为 per-process LivenessConfig.heartbeat_interval 的真实驱动，
        // max_miss / suspect 继承 default；如需 per-process 调节，后续可扩充字段。
        //
        // M6.1 (2026-05-03) 后：`health_check_interval == ZERO` 作为 sentinel 表示
        // **不注册到看门狗**。原因：M6 把 Go hub 砍了，hub 时代「由 hub 代 TS 上报心跳」
        // 的责任无人接班；ts-session 单进程没有外部心跳源，watchdog 只会 35s 误杀重启。
        //
        // 进程死亡探测**不**依赖主动心跳，也**不**存在 SIGCHLD handler（曾经的注释
        // 声称的「child.rs wait()+SIGCHLD 兜底」从未实现）。真实的存活探测在 supervisor
        // 主循环（run_with_config）的 `wait_any_child` 分支：对所有在运行的 child 同时
        // `Child::wait().await`，谁先退出谁被检测——前台（TUI）退出触发整体优雅关闭，
        // 后台（如 memory-orchestrator）退出走 `should_restart` 有界重启。
        let mut processes = HashMap::new();
        for process_config in &config.processes {
            let id = process_config.id.clone();
            let managed = ManagedProcess::new(process_config.clone());

            if process_config.health_check_interval.is_zero() {
                tracing::info!(
                    %id,
                    "health_check_interval=0，跳过看门狗注册（process 死亡靠 SIGCHLD 兜底）"
                );
            } else {
                // 注册到看门狗，使用 process_config.health_check_interval 驱动心跳间隔。
                // restart_grace 继承自 default_liveness，但若 per-process interval 短于默认，
                // 用 `max(default.restart_grace, heartbeat_interval * 2)` 保证 grace 至少能
                // 容纳新进程上报 1–2 次心跳的启动窗口，避免短间隔配置下 grace 过小仍触发冗余 kill。
                let liveness_config = LivenessConfig {
                    heartbeat_interval: process_config.health_check_interval,
                    max_miss_count: default_liveness.max_miss_count,
                    suspect_threshold: default_liveness.suspect_threshold,
                    restart_grace: default_liveness
                        .restart_grace
                        .max(process_config.health_check_interval * 2),
                };
                // Phase D.3: register_with_config 现在返 Result，重复 id
                // 不再静默覆盖。复用 D.1 的 must_log_err! macro：duplicate
                // process config 已在 line 110-112 的 processes HashMap 检测，
                // 这里若仍命中 AlreadyRegisteredError 表明 supervisor 上层
                // 状态机错配，记 warn 并保留先前注册（safety: 不杀已存在的
                // tracker）。Closes Step 1 §六 R1 ③ for watchdog.rs:108/121.
                crate::must_log_err!(
                    watchdog.register_with_config(id.clone(), liveness_config),
                    "supervisor.watchdog.register_with_config"
                );
            }

            if processes.insert(id.clone(), managed).is_some() {
                tracing::warn!(%id, "重复的进程配置，后者覆盖前者");
            }
        }

        // 5. 创建统一命令执行器
        let executor = Arc::new(CommandExecutor::new());

        tracing::info!(
            process_count = processes.len(),
            shutdown_timeout = ?config.shutdown_timeout,
            max_restart_count = config.max_restart_count,
            "Supervisor 已创建"
        );

        Self {
            config,
            processes,
            shutdown_token,
            watchdog_rx,
            watchdog,
            executor,
        }
    }

    /// 按顺序启动所有进程
    async fn start(&mut self) -> Result<()> {
        #[cfg(windows)]
        {
            tracing::info!("Windows 平台：Job Object 将在进程启动时惰性初始化");
        }

        let start_order = self.config.start_order();
        let mut started: Vec<ProcessId> = Vec::new();

        tracing::info!(order = ?start_order, "开始按顺序启动进程");

        for id in &start_order {
            if let Some(process) = self.processes.get_mut(id) {
                tracing::info!(%id, "启动进程");
                match process.spawn().await {
                    Ok(()) => {
                        tracing::info!(%id, pid = ?process.pid(), "进程启动成功");
                        started.push(id.clone());
                    }
                    Err(e) => {
                        tracing::error!(%id, error = %e, "进程启动失败");

                        // 回滚：按逆序停止已启动的进程
                        tracing::warn!("正在回滚已启动的进程...");
                        for rollback_id in started.iter().rev() {
                            if let Some(rollback_proc) = self.processes.get_mut(rollback_id) {
                                let timeout = self.config.shutdown_timeout;
                                if let Err(stop_err) = rollback_proc.stop(timeout).await {
                                    tracing::error!(
                                        id = %rollback_id,
                                        error = %stop_err,
                                        "回滚停止进程失败"
                                    );
                                }
                            }
                        }

                        return Err(anyhow::anyhow!("进程 {id} 启动失败: {e}"));
                    }
                }
            } else {
                tracing::warn!(%id, "配置中未找到该进程，跳过");
            }
        }

        tracing::info!(started = ?started, "所有进程已启动");
        Ok(())
    }

    /// 处理 watchdog 事件
    async fn handle_watchdog_event(&mut self, event: ProcessWatchdogEvent) {
        match event {
            ProcessWatchdogEvent::ProcessUnresponsive { id, state } => {
                // Unresponsive 是"观察"信号：心跳丢了一次，还没达 Dead 阈值。
                // 默认不采取行动，避免心跳抖动触发重启风暴；等 Dead 再 kill+restart。
                tracing::warn!(
                    process = %id,
                    liveness_state = ?state,
                    "进程进入 Unresponsive（心跳丢失但未达 Dead 阈值），继续观察"
                );
            }
            ProcessWatchdogEvent::ProcessDead { id, state } => {
                tracing::warn!(
                    process = %id,
                    liveness_state = ?state,
                    "检测到进程心跳死亡，执行 kill-then-restart"
                );

                // 根因修（2026-04-23）重入防护：如果 tracker 当前已不是 Dead
                // （被其它路径例如 TsUnresponsive 合成事件先处理并 reset 过）,
                // 说明这是一个过期的 Dead 事件，跳过避免二次 kill-restart。
                // tokio::select 单臂串行执行，但事件队列中可能堆积了 watchdog
                // 事件与 signal_rx 合成事件交错。
                if !matches!(self.watchdog.state(&id), Some(LivenessState::Dead)) {
                    tracing::debug!(
                        process = %id,
                        current_state = ?self.watchdog.state(&id),
                        "ProcessDead 事件已过期（tracker 已非 Dead），跳过重启"
                    );
                    return;
                }

                // M6.1 (2026-05-03)：hub 系统已全归档，watchdog 仅追踪 ts-session。

                // 心跳 Dead 的语义是"需要被干掉并重启"，直接 stop(timeout):
                //   1. 先发 SIGTERM / CTRL_BREAK
                //   2. 等 shutdown_timeout 内退出
                //   3. 超时则 SIGKILL / TerminateProcess
                // 这样无论进程是自然退出还是 hang，都能在 shutdown_timeout 内收敛。
                // 对比原实现用 process.wait() 等自然退出——hang 场景下永不返回，主循环死锁。
                let shutdown_timeout = self.config.shutdown_timeout;
                let exit_code = if let Some(process) = self.processes.get_mut(&id) {
                    match process.stop(shutdown_timeout).await {
                        Ok(code) => code,
                        Err(e) => {
                            tracing::warn!(process = %id, error = %e, "停止进程失败");
                            None
                        }
                    }
                } else {
                    None
                };

                // 检查是否应该重启
                let decision = if let Some(process) = self.processes.get_mut(&id) {
                    process.should_restart(
                        exit_code,
                        self.config.max_restart_count,
                        self.config.restart_window,
                    )
                } else {
                    RestartDecision::DoNotRestart
                };

                match decision {
                    RestartDecision::RestartNow => {
                        tracing::info!(process = %id, "立即重启进程");
                        self.watchdog.reset(&id);
                        if let Some(process) = self.processes.get_mut(&id) {
                            match process.spawn().await {
                                Ok(()) => {
                                    tracing::info!(process = %id, pid = ?process.pid(), "进程重启成功");
                                }
                                Err(e) => {
                                    tracing::error!(process = %id, error = %e, "进程重启失败");
                                }
                            }
                        }
                    }
                    RestartDecision::RestartAfter(delay) => {
                        tracing::info!(process = %id, ?delay, "延迟重启进程");
                        tokio::time::sleep(delay).await;
                        self.watchdog.reset(&id);
                        if let Some(process) = self.processes.get_mut(&id) {
                            match process.spawn().await {
                                Ok(()) => {
                                    tracing::info!(process = %id, pid = ?process.pid(), "进程延迟重启成功");
                                }
                                Err(e) => {
                                    tracing::error!(process = %id, error = %e, "进程延迟重启失败");
                                }
                            }
                        }
                    }
                    RestartDecision::DoNotRestart => {
                        tracing::warn!(
                            process = %id,
                            exit_code = ?exit_code,
                            "进程不满足重启条件，标记 ShuttingDown 避免 watchdog 反复 re-fire"
                        );
                        // 标记 tracker 为 ShuttingDown，防止后续 check_cycle 反复触发 Dead
                        self.watchdog.mark_shutting_down(&id);
                    }
                }
            }
            ProcessWatchdogEvent::ProcessRecovered { id } => {
                tracing::info!(process = %id, "进程恢复正常");
            }
            ProcessWatchdogEvent::HealthReport { states } => {
                tracing::debug!(?states, "健康报告");
            }
        }
    }

    /// 执行优雅关闭
    async fn shutdown(&mut self) {
        tracing::info!("开始优雅关闭...");
        let timeout = self.config.shutdown_timeout;

        // 获取停止顺序（逆序）
        let shutdown_order: Vec<ProcessId> = self
            .config
            .shutdown_order()
            .iter()
            .map(|c| c.id.clone())
            .collect();

        for id in &shutdown_order {
            if let Some(process) = self.processes.get_mut(id) {
                if process.is_running() {
                    tracing::info!(%id, "停止进程");

                    // 通知 watchdog 该进程正在关闭
                    self.watchdog.mark_shutting_down(id);

                    match process.stop(timeout).await {
                        Ok(code) => {
                            tracing::info!(%id, exit_code = ?code, "进程已停止");
                        }
                        Err(e) => {
                            tracing::error!(%id, error = %e, "停止进程时出错");
                        }
                    }
                } else {
                    tracing::debug!(%id, "进程未在运行，跳过停止");
                }
            }
        }

        tracing::info!("优雅关闭完成");
    }
}

// ────────────────────────── 公共入口 ──────────────────────────

/// 等待**第一个**退出的被管理子进程；返回 `(id, exit_code)`。
///
/// 这是 supervisor 唯一的子进程存活探测来源（watchdog 因 `health_check_interval=ZERO`
/// 全程跳过注册，且不存在 SIGCHLD handler）。对每个 `is_running()` 的 child 起一个
/// `Child::wait().await` future，`select_all` 取最先 resolve 的那个。
///
/// `processes.iter_mut()` 给出互不相交的 `&mut ManagedProcess`，因此 `select_all`
/// 同时持有多个 per-child wait future 不冲突。若当前没有运行中的进程，返回一个
/// 永不 resolve 的 future（`pending`），让主循环的其它分支（shutdown / signal）正常
/// 工作而不会 busy-spin。
async fn wait_any_child(
    processes: &mut std::collections::HashMap<ProcessId, child::ManagedProcess>,
) -> (ProcessId, Option<i32>) {
    use futures::future::{FutureExt, select_all};
    let mut futs = Vec::new();
    for (id, proc) in processes.iter_mut() {
        if proc.is_running() {
            let id = id.clone();
            futs.push(async move { (id, proc.wait().await.ok().flatten()) }.boxed());
        }
    }
    if futs.is_empty() {
        std::future::pending::<()>().await;
        unreachable!();
    }
    let (result, _idx, _rest) = select_all(futs).await;
    result
}

/// 启动 supervisor 主循环（使用默认开发配置）
pub async fn run(bind: Option<String>) -> Result<()> {
    let config = SupervisorConfig::development();
    run_with_config(config, bind).await
}

/// 使用外部提供的配置启动 supervisor 主循环
pub async fn run_with_config(config: SupervisorConfig, bind: Option<String>) -> Result<()> {
    // Preserve the historical public contract: a naturally exited managed
    // foreground process completes the supervisor regardless of its code.
    // Callers that explicitly need the code use the opt-in API below.
    preserve_legacy_run_result(run_with_config_foreground_exit_code(config, bind).await)
}

fn preserve_legacy_run_result(result: Result<Option<i32>>) -> Result<()> {
    result.map(|_foreground_exit_code| ())
}

/// 使用外部配置运行 supervisor，并返回自然退出的前台进程退出码。
///
/// `None` 表示 supervisor 自身收到关闭信号；被信号终止或无法取得平台退出码的
/// 前台子进程规范化为 `Some(1)`。纯 TUI launcher 用该结果忠实传播前台失败，
/// 普通调用方继续使用 [`run_with_config`] 的 `Result<()>` 契约。
pub async fn run_with_config_foreground_exit_code(
    config: SupervisorConfig,
    bind: Option<String>,
) -> Result<Option<i32>> {
    tracing::info!("Acosmi Supervisor 启动");

    tracing::info!(
        processes = config.processes.len(),
        shutdown_timeout = ?config.shutdown_timeout,
        "加载 supervisor 配置"
    );

    let mut supervisor = Supervisor::new(config);

    // IPC 信号通道
    let (signal_tx, mut signal_rx) = mpsc::channel::<ipc::IpcSignal>(32);

    // 启动 IPC 服务器
    let ipc_config = {
        let mut cfg = ipc::IpcConfig::default();
        if let Some(ref addr) = bind {
            cfg.uds_path = Some(std::path::PathBuf::from(addr));
        }
        cfg
    };

    // supervisor UDS 路径通过 CRABCODE_SUPERVISOR_UDS env 传给子进程
    let supervisor_addr: Option<String> = {
        #[cfg(unix)]
        {
            ipc_config
                .uds_path
                .as_ref()
                .map(|p| p.display().to_string())
        }
        #[cfg(windows)]
        {
            ipc_config.pipe_name.clone()
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    };
    if let Some(ref addr) = supervisor_addr {
        // 2026-04-28 阶段 2 立宪 2 落点：CRABCODE_SUPERVISOR_UDS 只注入给
        // ts-session，不再喂给 hub / 任何其它子进程。
        //
        // 旧行为 `for process in supervisor.processes.values_mut()` 把 env 喂给
        // 全部 children 是 L2 单点根因之一：hub 启动时吃 env → 一辈子只跟首启
        // supervisor 说话。阶段 2 之后 hub 是独立 daemon（阶段 1 PR），多窗口
        // 多 supervisor 时每条 TS 连接通过 initialize control 消息把自己 ts-session
        // 收到的 SUPERVISOR_UDS 上报给 hub，hub 据此按 connID 路由。所以这里：
        //   • 仅给 ts_session 注 env（TS 进程把 env 透出在 initialize handshake）
        //   • 用 ProcessId::ts_session() 精准过滤而不是黑名单，避免后续新增
        //     child（cron / 其它 daemon）继续误吃 env。
        let ts_id = ProcessId::ts_session();
        // P1-2: 握手密钥与 UDS 同路径注入受信子进程（ts-session）。任何继承此
        // env 的客户端在 version_handshake 携带 secret 即可通过认证。
        let secret = ipc_config.auth_secret.clone();
        let mut injected = false;
        for process in supervisor.processes.values_mut() {
            if process.config.id != ts_id {
                continue;
            }
            let _ = process
                .config
                .env
                .insert("CRABCODE_SUPERVISOR_UDS".to_string(), addr.clone());
            let _ = process
                .config
                .env
                .insert("CRABCODE_SUPERVISOR_SECRET".to_string(), secret.clone());
            injected = true;
        }
        if injected {
            tracing::info!(
                addr = %addr,
                "注入 CRABCODE_SUPERVISOR_UDS 仅供 ts-session 使用（阶段 2: hub 改走 initialize handshake 上报）"
            );
        }
    }
    let mut ipc_server = ipc::IpcServer::new(
        ipc_config,
        Arc::clone(&supervisor.executor),
        supervisor.shutdown_token.clone(),
        signal_tx,
    );

    // 2026-04-28 阶段 3-B（立宪 3）：cron SchedulerDaemon 已搬出 supervisor。
    // 定时核心改由独立 Rust 二进制 `crabcode-cron` 承担，跟随 hub 而非跟随
    // 任何一个 supervisor。supervisor 不再做调度决策，只保留状态快照所需
    // 的 state_dir（用于 supervisor.status RPC 暴露 supervisor.state_dir）。
    let state_dir = acosmi_config::paths::resolve_state_dir();

    // ── P1.1: Supervisor Status 快照 Arc<RwLock> ──
    let supervisor_status: crate::status::SharedStatus =
        std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::status::SupervisorStatus::new_initial(state_dir.display().to_string()),
        ));
    ipc_server.set_status_provider(std::sync::Arc::clone(&supervisor_status));

    let _ipc_handle = ipc_server.start().await.context("IPC 服务器启动失败")?;
    tracing::info!("IPC 服务器已启动");

    // 启动所有进程
    supervisor.start().await.context("启动进程失败")?;

    // watchdog 检查定时器
    let mut watchdog_check_interval = tokio::time::interval(supervisor.watchdog.check_interval());
    let mut watchdog_report_interval =
        tokio::time::interval(supervisor.watchdog.health_report_interval());
    let _ = watchdog_check_interval.tick().await;
    let _ = watchdog_report_interval.tick().await;
    tracing::info!("Watchdog 定时器已初始化");

    // 进入主监控循环
    tracing::info!("进入主监控循环");
    let mut foreground_exit_code = None;
    loop {
        tokio::select! {
            // 分支 0：子进程退出探测（唯一的存活探测来源）
            //
            // 2026-06-05 发布前根因修（A1+A2+P1-3+#74）：原主循环没有任何 wait()/SIGCHLD
            // 分支，watchdog 又因 health_check_interval=ZERO 全程跳过 → 子进程崩了永不被
            // 检测/收割/重启，前台 TUI 退出后主循环还死等其它分支永不返回（孤儿
            // orchestrator）。`wait_any_child` 对所有运行中 child 并发 wait：
            //   • 前台（TUI）退出 → break 走优雅关闭，不重启 TUI
            //   • 后台（如 memory-orchestrator）退出 → should_restart 有界重启（风暴受限）
            (exited_id, exit_code) = wait_any_child(&mut supervisor.processes) => {
                let is_foreground = supervisor.processes.get(&exited_id)
                    .map(|p| p.config.process_group == crate::topology::ProcessGroupPolicy::Foreground)
                    .unwrap_or(false);
                if is_foreground {
                    foreground_exit_code = Some(exit_code.unwrap_or(1));
                    tracing::info!(process=%exited_id, ?exit_code, "前台子进程(TUI)退出 → 触发 supervisor 优雅关闭");
                    break; // 落到下方既有 shutdown_token.cancel() + shutdown()
                }
                // 后台进程（如 memory-orchestrator）：经 should_restart 有界重启
                tracing::warn!(process=%exited_id, ?exit_code, "后台子进程退出，评估有界重启");
                let max = supervisor.config.max_restart_count;
                let window = supervisor.config.restart_window;
                let decision = supervisor.processes.get_mut(&exited_id)
                    .map(|p| p.should_restart(exit_code, max, window))
                    .unwrap_or(child::RestartDecision::DoNotRestart);
                match decision {
                    child::RestartDecision::RestartNow => {
                        if let Some(p) = supervisor.processes.get_mut(&exited_id) {
                            if let Err(e) = p.spawn().await { tracing::error!(process=%exited_id, error=%e, "后台子进程重启失败，保持 dead"); }
                            else { tracing::info!(process=%exited_id, "后台子进程已重启"); }
                        }
                    }
                    child::RestartDecision::RestartAfter(delay) => {
                        tokio::time::sleep(delay).await;
                        if let Some(p) = supervisor.processes.get_mut(&exited_id) {
                            if let Err(e) = p.spawn().await { tracing::error!(process=%exited_id, error=%e, "后台子进程延迟重启失败，保持 dead"); }
                            else { tracing::info!(process=%exited_id, "后台子进程已延迟重启"); }
                        }
                    }
                    child::RestartDecision::DoNotRestart => {
                        tracing::warn!(process=%exited_id, ?exit_code, "后台子进程不满足重启条件，保持 dead");
                    }
                }
            }
            // 分支 1：收到 shutdown 信号
            () = supervisor.shutdown_token.cancelled() => {
                tracing::info!("主循环收到 shutdown 信号，开始优雅关闭");
                break;
            }
            // 分支 2：收到 watchdog 事件
            event = supervisor.watchdog_rx.recv() => {
                if let Some(evt) = event {
                    supervisor.handle_watchdog_event(evt).await;
                } else {
                    tracing::error!("Watchdog 事件通道关闭，触发 shutdown");
                    break;
                }
            }
            // 分支 3：收到 IPC 信号（心跳 + 来自 Go 的 TS 健康告警）
            //
            // 根因修（2026-04-23）：
            //   - 原来的 ts_heartbeat_suppressed 标志是对"假心跳"（try_check_alive）
            //     的临时抑制开关。真心跳上线后不再需要它。
            //   - TsUnresponsive 原处理只设标志，等 35s watchdog 自己老化到 Dead。
            //     现在把它当作"Go 已验证 TS 挂起"的 ground-truth，立即合成
            //     ProcessDead 事件，走 kill-restart 路径，节省 35s 延迟。
            Some(signal) = signal_rx.recv() => {
                match signal {
                    ipc::IpcSignal::Heartbeat(kind) => {
                        // M6.1: watchdog 仅追踪 ts-session；ProcessKind::Go 已下线，
                        // 残留 piggyback 心跳显式 ignore，避免向 watchdog 注入未追踪 id。
                        match kind {
                            acosmi_heartbeat::ProcessKind::TypeScript => {
                                supervisor
                                    .watchdog
                                    .record_heartbeat(&ProcessId::ts_session());
                            }
                            acosmi_heartbeat::ProcessKind::Go => {
                                tracing::trace!(
                                    "ignoring Go heartbeat (hub is independent daemon since 2026-04-28 阶段 1)"
                                );
                            }
                        }
                    }
                    ipc::IpcSignal::TsAlive => {
                        // record_heartbeat + clear_degraded：TS 从 Degraded 恢复到 Alive 的唯一路径。
                        // clear_degraded 对非 Degraded tracker 是 no-op。
                        let ts_id = ProcessId::ts_session();
                        supervisor.watchdog.record_heartbeat(&ts_id);
                        supervisor.watchdog.clear_degraded(&ts_id);
                    }
                    ipc::IpcSignal::TsDegraded => {
                        // Go 观测到 TS 性能降级：mark_degraded 让 tracker 进入 Degraded，
                        // 心跳继续正常接收，但日志与 HealthReport 能反映降级状态。
                        tracing::info!("收到 Go 健康告警：TS 降级，mark_degraded");
                        supervisor.watchdog.mark_degraded(&ProcessId::ts_session());
                    }
                    ipc::IpcSignal::TsUnresponsive => {
                        let ts_id = ProcessId::ts_session();
                        // Ignore stale health samples during restart grace so a freshly
                        // restarted TS process is not killed before its first heartbeat.
                        // Once grace expires, synthesize ProcessDead normally.
                        if supervisor.watchdog.force_dead_if_stale(&ts_id) {
                            tracing::warn!(
                                process = %ts_id,
                                "收到 Go 健康告警：TS 不响应（grace 已过），合成 ProcessDead 触发 kill-restart"
                            );
                            supervisor
                                .handle_watchdog_event(ProcessWatchdogEvent::ProcessDead {
                                    id: ts_id,
                                    state: LivenessState::Dead,
                                })
                                .await;
                        } else {
                            tracing::info!(
                                process = %ts_id,
                                "收到 Go 健康告警：TS 不响应（restart grace 内），忽略为陈旧信号"
                            );
                        }
                    }
                }
            }
            // 分支 4（已删除）：Cron daemon 触发事件 — 见阶段 3-B 立宪 3，
            // cron 已搬到独立 `crabcode-cron` 二进制并直连 hub，不再经过 supervisor。
            //
            // 分支 5：定时驱动 watchdog 存活检查
            //
            // 根因修（2026-04-23）：原实现对所有非 Socket 的子进程调用
            // try_check_alive（即 try_wait）并把"进程没退出"当作心跳记录。
            // 这是**假心跳**——进程 hang 但没退出时 try_wait 永远 Ok(None)，
            // tracker 永远 Alive，supervisor 无法检测 event-loop 卡死。
            //
            // 现在完全删除被动注入心跳的逻辑：所有子进程都必须通过 IPC 主动上报。
            //   - TS Session：通过 IPC 主动 heartbeat 上报到 watchdog
            // 自然退出的进程由 tracker 自然老化到 Dead 触发 ProcessDead，
            // handle_watchdog_event 用 stop(timeout) 收割僵尸进程后再 restart。
            _ = watchdog_check_interval.tick() => {
                supervisor.watchdog.do_check_cycle().await;

                // P1.1: 刷新 supervisor status 快照（timestamp + children）
                refresh_supervisor_status(&supervisor_status, &supervisor.processes).await;
            }
            // 分支 6：定时发送健康报告
            _ = watchdog_report_interval.tick() => {
                supervisor.watchdog.send_health_report().await;
            }
        }
    }

    // 执行优雅关闭
    supervisor.shutdown_token.cancel();
    supervisor.shutdown().await;

    tracing::info!("Acosmi Supervisor 已停止");
    Ok(foreground_exit_code)
}

// ────────────────────────── 辅助函数 ──────────────────────────

/// P1.1: 主循环 tick 时刷新 supervisor status 快照。
///
/// 快照字段来源：
/// - `snapshot_at_ms` / `supervisor.uptime_s`：墙钟刷新
/// - `children`：遍历 `processes`，从 `ProcessState` 映射到对外字符串
async fn refresh_supervisor_status(
    shared: &crate::status::SharedStatus,
    processes: &HashMap<ProcessId, child::ManagedProcess>,
) {
    let mut children = std::collections::BTreeMap::new();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    for (id, proc) in processes {
        let (state_label, pid, uptime_s, restart_count) = match &proc.state {
            child::ProcessState::Starting => ("starting", proc.pid(), None, 0u32),
            child::ProcessState::Running { pid, started_at } => {
                let uptime = started_at.elapsed().as_secs();
                ("alive", Some(*pid), Some(uptime), 0u32)
            }
            child::ProcessState::Stopping => ("stopping", proc.pid(), None, 0u32),
            child::ProcessState::Stopped { .. } => ("stopped", None, None, 0u32),
            child::ProcessState::Failed { restart_count, .. } => {
                ("failed", None, None, *restart_count)
            }
        };
        children.insert(
            id.as_str().to_string(),
            crate::status::ChildStatus {
                state: state_label.to_string(),
                pid,
                uptime_s,
                restart_count,
            },
        );
    }

    let mut guard = shared.write().await;
    guard.snapshot_at_ms = now_ms;
    guard.supervisor.uptime_s = now_ms.saturating_sub(guard.supervisor.started_at_ms) / 1000;
    guard.replace_children(children);
}

// ────────────────────────── 单元测试 ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{IpcType, ProcessConfig, ProcessGroupPolicy, RestartPolicy, StdioPolicy};
    use std::collections::HashMap as StdHashMap;
    use std::time::Duration;

    fn test_process_config(id: &str) -> ProcessConfig {
        ProcessConfig {
            id: ProcessId::new(id),
            binary: "echo".to_string(),
            binary_os: None,
            args: vec!["hello".to_string()],
            env: StdHashMap::new(),
            env_os: StdHashMap::new(),
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

    fn test_supervisor_config() -> SupervisorConfig {
        SupervisorConfig {
            processes: vec![
                test_process_config("test-bg"),
                test_process_config("ts-session"),
            ],
            shutdown_timeout: Duration::from_secs(5),
            max_restart_count: 3,
            restart_window: Duration::from_secs(60),
        }
    }

    #[test]
    fn legacy_run_contract_does_not_promote_foreground_exit_codes_to_errors() {
        assert!(preserve_legacy_run_result(Ok(Some(0))).is_ok());
        assert!(preserve_legacy_run_result(Ok(Some(73))).is_ok());
        assert!(preserve_legacy_run_result(Ok(None)).is_ok());
        assert!(
            preserve_legacy_run_result(Err(anyhow::anyhow!("supervisor failure"))).is_err(),
            "supervisor infrastructure failures must still propagate"
        );
    }

    #[tokio::test]
    async fn test_supervisor_new() {
        let config = test_supervisor_config();
        let supervisor = Supervisor::new(config);

        assert_eq!(supervisor.processes.len(), 2);
        assert!(
            supervisor
                .processes
                .contains_key(&ProcessId::new("test-bg"))
        );
        assert!(
            supervisor
                .processes
                .contains_key(&ProcessId::new("ts-session"))
        );
        assert_eq!(supervisor.config.shutdown_timeout, Duration::from_secs(5));
        assert_eq!(supervisor.config.max_restart_count, 3);
    }

    #[tokio::test]
    async fn test_supervisor_new_with_default_config() {
        let config = SupervisorConfig::default();
        let supervisor = Supervisor::new(config);
        assert!(supervisor.processes.is_empty());
        assert_eq!(supervisor.config.shutdown_timeout, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn test_supervisor_new_with_development_config() {
        // M6.1: development() 单进程拓扑（仅 ts-session）。
        let config = SupervisorConfig::development();
        let supervisor = Supervisor::new(config);

        assert_eq!(supervisor.processes.len(), 1);
        assert!(supervisor.processes.contains_key(&ProcessId::ts_session()));

        let start_order = supervisor.config.start_order();
        assert_eq!(start_order.len(), 1);
        assert_eq!(start_order[0], ProcessId::ts_session());
    }

    #[test]
    fn test_config_shutdown_order() {
        let config = test_supervisor_config();
        let shutdown_order = config.shutdown_order();
        assert_eq!(shutdown_order[0].id, ProcessId::new("ts-session"));
        assert_eq!(shutdown_order[1].id, ProcessId::new("test-bg"));
    }

    #[test]
    fn test_config_start_order() {
        let config = test_supervisor_config();
        let start_order = config.start_order();
        assert_eq!(start_order[0], ProcessId::new("test-bg"));
        assert_eq!(start_order[1], ProcessId::new("ts-session"));
    }

    #[tokio::test]
    async fn test_processes_initial_state() {
        let config = test_supervisor_config();
        let supervisor = Supervisor::new(config);

        for (id, process) in &supervisor.processes {
            assert!(!process.is_running(), "{id} 进程应处于未运行状态");
            assert_eq!(process.pid(), None, "{id} 进程不应有 PID");
        }
    }

    #[tokio::test]
    async fn test_supervisor_shutdown_token() {
        let config = test_supervisor_config();
        let supervisor = Supervisor::new(config);

        assert!(!supervisor.shutdown_token.is_cancelled());
        supervisor.shutdown_token.cancel();
        assert!(supervisor.shutdown_token.is_cancelled());
    }

    #[tokio::test]
    async fn test_watchdog_channel_connected() {
        let config = test_supervisor_config();
        let mut supervisor = Supervisor::new(config);

        supervisor.watchdog.send_health_report().await;

        let event = supervisor.watchdog_rx.try_recv();
        assert!(event.is_ok(), "应能从 watchdog_rx 收到健康报告事件");
    }

    #[tokio::test]
    async fn test_single_process_config() {
        let config = SupervisorConfig {
            processes: vec![test_process_config("test-bg")],
            shutdown_timeout: Duration::from_secs(5),
            max_restart_count: 3,
            restart_window: Duration::from_secs(60),
        };
        let supervisor = Supervisor::new(config);

        assert_eq!(supervisor.processes.len(), 1);
        assert!(
            supervisor
                .processes
                .contains_key(&ProcessId::new("test-bg"))
        );
        assert!(
            !supervisor
                .processes
                .contains_key(&ProcessId::new("ts-session"))
        );
    }

    /// A1+A2 回归：`wait_any_child` 必须检测到一个真实启动后快速退出的 child，
    /// 返回它的 id 与 exit_code（这是 supervisor 唯一的存活探测来源，watchdog 跳过）。
    #[tokio::test]
    async fn test_wait_any_child_detects_exited_child() {
        // 用 `true` 命令：spawn 后立刻以 0 退出。
        let mut cfg = test_process_config("fast-exit");
        cfg.binary = "true".to_string();
        cfg.args = vec![];

        let mut processes: HashMap<ProcessId, child::ManagedProcess> = HashMap::new();
        let mut managed = child::ManagedProcess::new(cfg);
        managed.spawn().await.expect("spawn 应成功");
        processes.insert(ProcessId::new("fast-exit"), managed);

        let (exited_id, exit_code) = super::wait_any_child(&mut processes).await;
        assert_eq!(exited_id, ProcessId::new("fast-exit"));
        assert_eq!(exit_code, Some(0));
        // wait() 已把状态置为 Stopped，进程不再 running。
        assert!(!processes.get(&exited_id).unwrap().is_running());
    }

    /// A1+A2 回归：模拟主循环 `wait_any_child` 分支对后台快速崩溃进程的有界重启决策——
    /// 连续 fast-crash（exit_code != 0）在 max_restart_count 次后必须 DoNotRestart，
    /// 不会无限重启（#74 storm 防护）。
    #[test]
    fn test_background_fast_crash_loop_is_bounded() {
        use crate::topology::RestartPolicy;
        let mut cfg = test_process_config("crash-loop");
        cfg.restart_policy = RestartPolicy::Immediate; // 总是想重启，靠次数上限收敛
        let mut managed = child::ManagedProcess::new(cfg);

        let max = 3;
        let window = Duration::from_secs(60);
        // 前 max 次 RestartNow（模拟主循环 RestartNow 分支），第 max+1 次 DoNotRestart。
        for _ in 0..max {
            assert_eq!(
                managed.should_restart(Some(1), max, window),
                child::RestartDecision::RestartNow
            );
        }
        assert_eq!(
            managed.should_restart(Some(1), max, window),
            child::RestartDecision::DoNotRestart,
            "fast-crash 超过 max_restart_count 必须停止重启，避免风暴"
        );
    }

    #[tokio::test]
    async fn test_three_process_config() {
        let config = SupervisorConfig {
            processes: vec![
                test_process_config("test-bg"),
                test_process_config("ts-session"),
                test_process_config("custom-worker"),
            ],
            shutdown_timeout: Duration::from_secs(5),
            max_restart_count: 3,
            restart_window: Duration::from_secs(60),
        };
        let supervisor = Supervisor::new(config);

        assert_eq!(supervisor.processes.len(), 3);
        assert!(
            supervisor
                .processes
                .contains_key(&ProcessId::new("custom-worker"))
        );
    }
}
