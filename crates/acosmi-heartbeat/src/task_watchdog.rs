//! 后台任务 watchdog
//!
//! `TaskWatchdog` 监控后台运行的 shell task / worker 的存活状态。
//! 面向动态注册/注销的任务，与 supervisor 的 `ProcessWatchdog`（面向固定子进程）互补。
//!
//! 每个注册的任务拥有独立的 `LivenessTracker`，可以设置任务级超时。
//! 当检测到任务异常时，通过 mpsc channel 向上层发送 `TaskWatchdogEvent`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::liveness::{LivenessConfig, LivenessState, LivenessTracker};

// ────────────────────────── 事件 ──────────────────────────

/// 任务 watchdog 事件
#[derive(Debug, Clone)]
pub enum TaskWatchdogEvent {
    /// 任务超时（运行时间超过设定上限）
    TaskTimeout { task_id: String },
    /// 任务无响应（心跳丢失 → Dead）
    TaskUnresponsive { task_id: String },
    /// 任务从异常状态恢复到 Alive
    TaskRecovered { task_id: String },
}

// ────────────────────────── 任务健康信息 ──────────────────────────

/// 单个任务的健康信息
pub struct TaskHealth {
    /// 任务唯一标识
    pub task_id: String,
    /// 存活跟踪器
    pub liveness: LivenessTracker,
    /// 任务启动时刻
    pub started_at: Instant,
    /// 任务级超时（None 表示无超时限制）
    pub timeout: Option<Duration>,
    /// 上一次检查时的状态（用于检测状态变化）
    prev_state: LivenessState,
    /// 是否已经发送过超时事件（避免重复发送）
    timeout_fired: bool,
}

// ────────────────────────── 配置 ──────────────────────────

/// 任务 watchdog 配置
#[derive(Debug, Clone)]
pub struct TaskWatchdogConfig {
    /// 默认任务心跳检测配置
    pub default_liveness: LivenessConfig,
    /// 主循环检查间隔
    pub check_interval: Duration,
}

impl Default for TaskWatchdogConfig {
    fn default() -> Self {
        let liveness = LivenessConfig::default();
        Self {
            check_interval: liveness.heartbeat_interval / 2,
            default_liveness: liveness,
        }
    }
}

// ────────────────────────── TaskWatchdog ──────────────────────────

/// 后台任务 watchdog，监控动态注册的任务心跳与超时
pub struct TaskWatchdog {
    /// `已注册的任务映射（task_id` → 健康信息）
    tasks: HashMap<String, TaskHealth>,
    /// 事件输出通道
    event_tx: mpsc::Sender<TaskWatchdogEvent>,
    /// 配置
    config: TaskWatchdogConfig,
}

impl TaskWatchdog {
    /// 创建新的任务 watchdog
    #[must_use]
    pub fn new(config: TaskWatchdogConfig, event_tx: mpsc::Sender<TaskWatchdogEvent>) -> Self {
        Self {
            tasks: HashMap::new(),
            event_tx,
            config,
        }
    }

    /// 注册新任务，使用默认心跳配置
    ///
    /// - `task_id`：任务唯一标识
    /// - `timeout`：任务级超时，None 表示无超时限制
    pub fn register_task(&mut self, task_id: String, timeout: Option<Duration>) {
        let liveness_config = self.config.default_liveness.clone();
        self.register_task_with_config(task_id, timeout, liveness_config);
    }

    /// 注册新任务，使用自定义心跳配置
    ///
    /// - `task_id`：任务唯一标识
    /// - `timeout`：任务级超时，None 表示无超时限制
    /// - `liveness_config`：自定义存活检测配置
    pub fn register_task_with_config(
        &mut self,
        task_id: String,
        timeout: Option<Duration>,
        liveness_config: LivenessConfig,
    ) {
        tracing::info!(
            task_id = %task_id,
            timeout = ?timeout,
            "注册任务到 TaskWatchdog"
        );

        let health = TaskHealth {
            task_id: task_id.clone(),
            liveness: LivenessTracker::new(liveness_config),
            started_at: Instant::now(),
            timeout,
            prev_state: LivenessState::Alive,
            timeout_fired: false,
        };

        let _ = self.tasks.insert(task_id, health);
    }

    /// 注销任务，返回是否找到并移除了该任务
    pub fn unregister_task(&mut self, task_id: &str) -> bool {
        let removed = self.tasks.remove(task_id).is_some();
        if removed {
            tracing::info!(task_id = %task_id, "从 TaskWatchdog 注销任务");
        } else {
            tracing::warn!(task_id = %task_id, "注销失败：未找到任务");
        }
        removed
    }

    /// 记录任务心跳，返回是否找到该任务
    pub fn record_task_heartbeat(&mut self, task_id: &str) -> bool {
        if let Some(health) = self.tasks.get_mut(task_id) {
            tracing::trace!(task_id = %task_id, "记录任务心跳");
            health.liveness.record_heartbeat();
            true
        } else {
            tracing::warn!(task_id = %task_id, "记录心跳失败：未找到任务");
            false
        }
    }

    /// 当前监控的任务数
    #[must_use]
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    /// 查询任务当前状态，任务不存在时返回 None
    #[must_use]
    pub fn task_state(&self, task_id: &str) -> Option<LivenessState> {
        self.tasks.get(task_id).map(|h| h.liveness.state())
    }

    /// 运行主监控循环
    ///
    /// 此方法会一直运行，直到收到取消信号（`shutdown`）。
    /// 内部会：
    /// 1. 定期调用每个任务的 `liveness.check()` 驱动状态机
    /// 2. 检测状态变化，发送对应的 `TaskWatchdogEvent`
    /// 3. 检查任务超时，发送 `TaskTimeout` 事件
    pub async fn run(&mut self, shutdown: CancellationToken) {
        tracing::info!(
            "TaskWatchdog 启动，检查间隔: {:?}",
            self.config.check_interval
        );

        let mut check_interval = tokio::time::interval(self.config.check_interval);
        // 第一次 tick 立即触发，跳过它以免启动时误报
        let _ = check_interval.tick().await;

        loop {
            tokio::select! {
                // 取消信号：优雅退出
                () = shutdown.cancelled() => {
                    tracing::info!("TaskWatchdog 收到关闭信号，退出主循环");
                    break;
                }
                // 定期存活检查
                _ = check_interval.tick() => {
                    self.do_check_cycle().await;
                }
            }
        }

        tracing::info!("TaskWatchdog 已停止");
    }

    /// 执行一次检查周期：驱动所有任务的状态机并检测变化
    async fn do_check_cycle(&mut self) {
        let now = Instant::now();

        // 收集需要发送的事件（避免在遍历 HashMap 时借用冲突）
        let mut events = Vec::new();

        for health in self.tasks.values_mut() {
            let current = health.liveness.check();
            let previous = health.prev_state;

            // 检测状态变化
            if current != previous {
                tracing::info!(
                    task_id = %health.task_id,
                    previous = %previous,
                    current = %current,
                    "任务存活状态变化: {} -> {}",
                    previous,
                    current
                );

                match current {
                    LivenessState::Dead => {
                        events.push(TaskWatchdogEvent::TaskUnresponsive {
                            task_id: health.task_id.clone(),
                        });
                    }
                    LivenessState::Alive
                        if previous == LivenessState::Unresponsive
                            || previous == LivenessState::Dead =>
                    {
                        events.push(TaskWatchdogEvent::TaskRecovered {
                            task_id: health.task_id.clone(),
                        });
                    }
                    // 显式列出所有剩余变体，避免新增枚举值被 _ 静默吞掉
                    LivenessState::Alive
                    | LivenessState::Degraded
                    | LivenessState::Unresponsive
                    | LivenessState::ShuttingDown => {}
                }
            }

            health.prev_state = current;

            // 检查任务级超时（仅在首次超时时发送事件）
            if let Some(timeout) = health.timeout
                && !health.timeout_fired
                && now.duration_since(health.started_at) > timeout
            {
                tracing::warn!(
                    task_id = %health.task_id,
                    timeout = ?timeout,
                    elapsed = ?now.duration_since(health.started_at),
                    "任务运行超时"
                );
                health.timeout_fired = true;
                events.push(TaskWatchdogEvent::TaskTimeout {
                    task_id: health.task_id.clone(),
                });
            }
        }

        // 发送收集到的所有事件
        for event in events {
            self.send_event(event).await;
        }
    }

    /// 异步发送事件（等待 channel 容量，接收端关闭时记录警告）
    async fn send_event(&self, event: TaskWatchdogEvent) {
        if let Err(e) = self.event_tx.send(event).await {
            tracing::warn!("发送 TaskWatchdogEvent 失败（接收端已关闭）: {}", e);
        }
    }
}

// ────────────────────────── 单元测试 ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 辅助：创建快速检查配置，方便测试
    fn fast_config() -> TaskWatchdogConfig {
        let liveness = LivenessConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_miss_count: 3,
            suspect_threshold: 1,
            restart_grace: Duration::from_millis(100),
        };
        TaskWatchdogConfig {
            default_liveness: liveness,
            check_interval: Duration::from_millis(25),
        }
    }

    #[test]
    fn default_config_is_sane() {
        let config = TaskWatchdogConfig::default();
        // check_interval 应为 heartbeat_interval 的一半
        assert_eq!(
            config.check_interval,
            config.default_liveness.heartbeat_interval / 2
        );
    }

    #[test]
    fn register_and_unregister_task() {
        let (tx, _rx) = mpsc::channel(16);
        let mut wd = TaskWatchdog::new(fast_config(), tx);

        assert_eq!(wd.task_count(), 0);

        wd.register_task("task-1".into(), None);
        assert_eq!(wd.task_count(), 1);
        assert_eq!(wd.task_state("task-1"), Some(LivenessState::Alive));

        wd.register_task("task-2".into(), Some(Duration::from_secs(60)));
        assert_eq!(wd.task_count(), 2);

        // 注销存在的任务
        assert!(wd.unregister_task("task-1"));
        assert_eq!(wd.task_count(), 1);
        assert_eq!(wd.task_state("task-1"), None);

        // 注销不存在的任务
        assert!(!wd.unregister_task("nonexistent"));
    }

    #[test]
    fn record_heartbeat_returns_correct_bool() {
        let (tx, _rx) = mpsc::channel(16);
        let mut wd = TaskWatchdog::new(fast_config(), tx);

        wd.register_task("task-1".into(), None);

        // 记录已注册任务的心跳
        assert!(wd.record_task_heartbeat("task-1"));

        // 记录未注册任务的心跳
        assert!(!wd.record_task_heartbeat("nonexistent"));
    }

    #[test]
    fn task_state_returns_none_for_unknown() {
        let (tx, _rx) = mpsc::channel(16);
        let wd = TaskWatchdog::new(fast_config(), tx);
        assert_eq!(wd.task_state("unknown"), None);
    }

    #[test]
    fn register_task_with_custom_config() {
        let (tx, _rx) = mpsc::channel(16);
        let mut wd = TaskWatchdog::new(fast_config(), tx);

        let custom = LivenessConfig {
            heartbeat_interval: Duration::from_millis(100),
            max_miss_count: 5,
            suspect_threshold: 2,
            restart_grace: Duration::from_millis(200),
        };
        wd.register_task_with_config("custom-task".into(), None, custom);

        assert_eq!(wd.task_count(), 1);
        assert_eq!(wd.task_state("custom-task"), Some(LivenessState::Alive));

        // 验证自定义配置已生效
        let health = wd.tasks.get("custom-task").unwrap();
        assert_eq!(
            health.liveness.config().heartbeat_interval,
            Duration::from_millis(100)
        );
        assert_eq!(health.liveness.config().max_miss_count, 5);
    }

    #[tokio::test]
    async fn watchdog_detects_task_unresponsive() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = fast_config();
        let mut wd = TaskWatchdog::new(config, tx);

        wd.register_task("slow-task".into(), None);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(async move {
            wd.run(shutdown_clone).await;
        });

        // 等待足够时间让任务进入 Dead 状态（>3*50ms = 150ms）
        tokio::time::sleep(Duration::from_millis(250)).await;

        // 应该收到 TaskUnresponsive 事件
        let mut found_unresponsive = false;
        while let Ok(event) = rx.try_recv() {
            if let TaskWatchdogEvent::TaskUnresponsive { task_id } = event {
                if task_id == "slow-task" {
                    found_unresponsive = true;
                }
            }
        }
        assert!(found_unresponsive, "应检测到任务无响应");

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn watchdog_detects_task_timeout() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = fast_config();
        let mut wd = TaskWatchdog::new(config, tx);

        // 注册一个 100ms 超时的任务，但持续发送心跳使其不会进入 Dead
        wd.register_task("short-lived".into(), Some(Duration::from_millis(100)));

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(async move {
            wd.run(shutdown_clone).await;
        });

        // 等待超过 100ms 超时
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 应该收到 TaskTimeout 事件
        let mut found_timeout = false;
        while let Ok(event) = rx.try_recv() {
            if let TaskWatchdogEvent::TaskTimeout { task_id } = event {
                if task_id == "short-lived" {
                    found_timeout = true;
                }
            }
        }
        assert!(found_timeout, "应检测到任务超时");

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn watchdog_detects_task_recovered() {
        let (tx, mut rx) = mpsc::channel(16);
        let config = fast_config();
        let mut wd = TaskWatchdog::new(config, tx);

        wd.register_task("recoverable".into(), None);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        // 需要在 watchdog 运行时也能记录心跳，
        // 所以用 Arc<Mutex> 共享或分步执行。
        // 这里我们用一种简洁方式：先让任务进入 Dead，然后停止 watchdog，
        // 记录心跳，再启动新的 watchdog 来检测恢复。
        //
        // 更好的做法：在 do_check_cycle 前手动驱动。

        // 先等待进入 Dead
        let handle = tokio::spawn(async move {
            wd.run(shutdown_clone).await;
            wd // 返回 watchdog 所有权
        });

        tokio::time::sleep(Duration::from_millis(250)).await;
        shutdown.cancel();
        let mut wd = handle.await.expect("watchdog 任务应正常完成");

        // 消费掉之前的事件
        while rx.try_recv().is_ok() {}

        // 记录心跳，使状态恢复
        wd.record_task_heartbeat("recoverable");

        // 创建新的 channel 用于接收恢复事件
        let (tx2, mut rx2) = mpsc::channel(16);
        wd.event_tx = tx2;

        let shutdown2 = CancellationToken::new();
        let shutdown2_clone = shutdown2.clone();

        let handle2 = tokio::spawn(async move {
            wd.run(shutdown2_clone).await;
        });

        // 短暂等待让 check 触发一次
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown2.cancel();
        let _ = handle2.await;

        // 应该收到 TaskRecovered 事件
        let mut found_recovered = false;
        while let Ok(event) = rx2.try_recv() {
            if let TaskWatchdogEvent::TaskRecovered { task_id } = event {
                if task_id == "recoverable" {
                    found_recovered = true;
                }
            }
        }
        assert!(found_recovered, "应检测到任务恢复");
    }

    #[tokio::test]
    async fn watchdog_shutdown_gracefully() {
        let (tx, _rx) = mpsc::channel(16);
        let config = fast_config();
        let mut wd = TaskWatchdog::new(config, tx);

        wd.register_task("some-task".into(), None);

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(async move {
            wd.run(shutdown_clone).await;
        });

        // 立即取消
        shutdown.cancel();

        // 应该很快完成
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "watchdog 应在收到关闭信号后及时退出");
    }

    #[tokio::test]
    async fn timeout_event_fires_only_once() {
        let (tx, mut rx) = mpsc::channel(64);
        let config = fast_config();
        let mut wd = TaskWatchdog::new(config, tx);

        // 注册一个 50ms 就超时的任务
        wd.register_task("once-only".into(), Some(Duration::from_millis(50)));

        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn(async move {
            wd.run(shutdown_clone).await;
        });

        // 等待足够长时间，确保多个检查周期都已过去
        tokio::time::sleep(Duration::from_millis(300)).await;

        shutdown.cancel();
        let _ = handle.await;

        // 统计 TaskTimeout 事件数量，应该恰好为 1
        let mut timeout_count = 0;
        while let Ok(event) = rx.try_recv() {
            if let TaskWatchdogEvent::TaskTimeout { task_id } = event {
                if task_id == "once-only" {
                    timeout_count += 1;
                }
            }
        }
        assert_eq!(timeout_count, 1, "TaskTimeout 应只发送一次");
    }

    #[test]
    fn multiple_tasks_independent() {
        let (tx, _rx) = mpsc::channel(16);
        let mut wd = TaskWatchdog::new(fast_config(), tx);

        wd.register_task("a".into(), None);
        wd.register_task("b".into(), None);
        wd.register_task("c".into(), None);
        assert_eq!(wd.task_count(), 3);

        // 只给 a 发心跳
        wd.record_task_heartbeat("a");

        // 注销 b
        assert!(wd.unregister_task("b"));
        assert_eq!(wd.task_count(), 2);

        // a 和 c 仍然存在
        assert_eq!(wd.task_state("a"), Some(LivenessState::Alive));
        assert_eq!(wd.task_state("c"), Some(LivenessState::Alive));
        assert_eq!(wd.task_state("b"), None);
    }
}
