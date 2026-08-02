//! 进程看门狗 — 泛化的多进程存活监控
//!
//! 基于 `acosmi_heartbeat::LivenessTracker` 构建的通用看门狗，
//! 用 `ProcessId` 标识进程，泛化支持任意数量的受管子进程。

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

use acosmi_heartbeat::{LivenessConfig, LivenessState, LivenessTracker};

use crate::topology::ProcessId;

// ────────────────────────── 事件类型 ──────────────────────────

/// 进程看门狗事件
#[derive(Debug)]
pub enum ProcessWatchdogEvent {
    /// 进程进入 Unresponsive（心跳丢失但未达 Dead 阈值）——观察用途，
    /// supervisor 默认只记日志，不触发重启。
    ProcessUnresponsive { id: ProcessId, state: LivenessState },
    /// 进程判定为死亡
    ProcessDead { id: ProcessId, state: LivenessState },
    /// 进程从异常状态恢复
    ProcessRecovered { id: ProcessId },
    /// 定期健康报告
    HealthReport {
        states: Vec<(ProcessId, LivenessState)>,
    },
}

// ────────────────────────── 配置 ──────────────────────────

/// 看门狗配置
#[derive(Debug, Clone)]
pub struct ProcessWatchdogConfig {
    /// 每个进程的默认存活检测配置
    pub default_liveness: LivenessConfig,
    /// 存活检查间隔
    pub check_interval: Duration,
    /// 健康报告间隔
    pub health_report_interval: Duration,
}

impl Default for ProcessWatchdogConfig {
    fn default() -> Self {
        Self {
            default_liveness: LivenessConfig::default(),
            check_interval: Duration::from_millis(2500),
            health_report_interval: Duration::from_secs(30),
        }
    }
}

// ────────────────────────── 错误类型 ──────────────────────────

/// `register` / `register_with_config` 在已注册的 `ProcessId` 上再次调用时返回。
///
/// Step 2 Phase D.3 — 关闭 Step 1 §六 R1 sub-bullet ③：原实现用 `let _ =
/// trackers.insert(...)` 静默覆盖之前的 tracker，把"重复注册"伪装成"upsert"，
/// 而**业务上是 bug**（重复注册意味着配置去重失败 / supervisor 状态机错乱）。
/// 调用方现在必须显式选择处理方式（log + 跳过 / propagate / panic-on-bug）。
#[derive(Debug, thiserror::Error)]
#[error("ProcessId already registered with watchdog: {0:?}")]
pub struct AlreadyRegisteredError(pub ProcessId);

// ────────────────────────── 内部条目 ──────────────────────────

/// 单个进程的跟踪条目
struct TrackerEntry {
    tracker: LivenessTracker,
    prev_state: LivenessState,
}

// ────────────────────────── 看门狗主体 ──────────────────────────

/// 通用进程看门狗
///
/// 管理任意数量的进程存活跟踪器，通过 `ProcessId` 标识。
/// 替代早期的硬编码 Go/TS 方法。
pub struct ProcessWatchdog {
    /// 进程跟踪表
    trackers: HashMap<ProcessId, TrackerEntry>,
    /// 事件发送端
    event_tx: mpsc::Sender<ProcessWatchdogEvent>,
    /// 存活检查间隔
    check_interval: Duration,
    /// 健康报告间隔
    health_report_interval: Duration,
    /// 默认存活配置（注册新进程时使用）
    default_liveness: LivenessConfig,
}

impl ProcessWatchdog {
    /// 创建看门狗实例
    #[must_use]
    pub fn new(
        config: ProcessWatchdogConfig,
        event_tx: mpsc::Sender<ProcessWatchdogEvent>,
    ) -> Self {
        Self {
            trackers: HashMap::new(),
            event_tx,
            check_interval: config.check_interval,
            health_report_interval: config.health_report_interval,
            default_liveness: config.default_liveness,
        }
    }

    /// 注册进程（使用默认存活配置）
    ///
    /// # Errors
    /// 返回 [`AlreadyRegisteredError`] 当 `id` 已被先前的 register 注册过。
    /// 这是重复配置的指示信号，调用方决定如何处理（warn + 跳过 / propagate）。
    /// 关闭 Step 1 §六 R1 ③（原 `let _ = trackers.insert` 静默覆盖）。
    pub fn register(&mut self, id: ProcessId) -> Result<(), AlreadyRegisteredError> {
        if self.trackers.contains_key(&id) {
            return Err(AlreadyRegisteredError(id));
        }
        let tracker = LivenessTracker::new(self.default_liveness.clone());
        self.trackers.insert(
            id.clone(),
            TrackerEntry {
                tracker,
                prev_state: LivenessState::Alive,
            },
        );
        tracing::debug!(%id, "进程已注册到看门狗");
        Ok(())
    }

    /// 注册进程（使用自定义存活配置）
    ///
    /// # Errors
    /// 同 [`Self::register`]：重复 `id` 返 [`AlreadyRegisteredError`]。
    pub fn register_with_config(
        &mut self,
        id: ProcessId,
        liveness_config: LivenessConfig,
    ) -> Result<(), AlreadyRegisteredError> {
        if self.trackers.contains_key(&id) {
            return Err(AlreadyRegisteredError(id));
        }
        let tracker = LivenessTracker::new(liveness_config);
        self.trackers.insert(
            id.clone(),
            TrackerEntry {
                tracker,
                prev_state: LivenessState::Alive,
            },
        );
        tracing::debug!(%id, "进程已注册到看门狗（自定义配置）");
        Ok(())
    }

    /// 注销进程
    pub fn unregister(&mut self, id: &ProcessId) {
        if self.trackers.remove(id).is_some() {
            tracing::debug!(%id, "进程已从看门狗注销");
        }
    }

    /// 记录心跳
    pub fn record_heartbeat(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.record_heartbeat();
        }
    }

    /// 重置跟踪器（进程重启后调用）
    pub fn reset(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.reset();
            entry.prev_state = LivenessState::Alive;
            tracing::debug!(%id, "看门狗跟踪器已重置");
        }
    }

    /// 标记进程正在关闭（阻止重启逻辑）
    pub fn mark_shutting_down(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.mark_shutting_down();
        }
    }

    /// 外部 ground-truth 触发的"强制 Dead"：让 tracker 直接跳到 Dead 状态。
    ///
    /// 典型用途：Go 侧 `ChannelHealth` 已经验证 TS event-loop 死锁，下发告警给
    /// supervisor。supervisor 用 `force_dead` + 主动调用 `handle_watchdog_event`
    /// 的 `ProcessDead` 路径（kill-restart），而不用等 35s 让 tracker 自然老化。
    ///
    /// **注意**：新代码应优先使用 `force_dead_if_stale`，它在 restart grace
    /// 窗口内会拒绝请求以避免冗余 kill-restart（参见架构文档 §六 Case A）。
    pub fn force_dead(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.force_dead();
            entry.prev_state = LivenessState::Dead;
        }
    }

    /// 带 restart grace 的 `force_dead：在` tracker 最近一次 `reset()` 到当前的
    /// 时间窗小于 `LivenessConfig::restart_grace` 时拒绝请求，返回 `false`。
    ///
    /// 返回值语义：
    /// - `true`：grace 已过（或从未 reset 过），tracker 已标记为 Dead，调用方
    ///   应接着走 kill-restart 路径。
    /// - `false`：tracker 处于 restart grace 内（或正在 `ShuttingDown`），
    ///   调用方应**跳过** kill-restart，避免在新进程的启动窗口里把它误杀。
    ///
    /// 返回 `true` 时 `prev_state` 同步更新为 Dead，保证后续 `do_check_cycle`
    /// 不会再为同一次状态转移重复发 `ProcessDead` 事件。
    #[must_use]
    pub fn force_dead_if_stale(&mut self, id: &ProcessId) -> bool {
        if let Some(entry) = self.trackers.get_mut(id) {
            let accepted = entry.tracker.force_dead_if_stale();
            if accepted {
                entry.prev_state = LivenessState::Dead;
            }
            return accepted;
        }
        false
    }

    /// 查询 tracker 的当前 restart epoch（每次 `reset()` 递增）。
    ///
    /// 主要给外部事件处理者做"这个事件携带的 epoch 是否还匹配 tracker 当前
    /// epoch"的判断，识别跨 reset 边界的陈旧事件。未注册的 id 返回 `None`。
    #[must_use]
    pub fn epoch(&self, id: &ProcessId) -> Option<u64> {
        self.trackers.get(id).map(|e| e.tracker.epoch())
    }

    /// 标记进程降级运行
    ///
    /// 根因修（2026-04-23）：原 `IpcSignal::TsUnresponsive` 只能表达"死"态，
    /// 对"降级但还活着"场景无 API 支持。加入 `mark_degraded` 让 Go 的 piggyback
    /// `ts_status="degraded"` 能够如实驱动 tracker，HealthReport 可见降级状态。
    pub fn mark_degraded(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.mark_degraded();
        }
    }

    /// 清除降级状态
    pub fn clear_degraded(&mut self, id: &ProcessId) {
        if let Some(entry) = self.trackers.get_mut(id) {
            entry.tracker.clear_degraded();
        }
    }

    /// 查询进程当前存活状态
    #[must_use]
    pub fn state(&self, id: &ProcessId) -> Option<LivenessState> {
        self.trackers.get(id).map(|e| e.tracker.state())
    }

    /// 获取检查间隔
    #[must_use]
    pub const fn check_interval(&self) -> Duration {
        self.check_interval
    }

    /// 获取健康报告间隔
    #[must_use]
    pub const fn health_report_interval(&self) -> Duration {
        self.health_report_interval
    }

    /// 执行一次检查周期
    ///
    /// 驱动所有跟踪器的状态机，检测状态变化并发送相应事件。
    pub async fn do_check_cycle(&mut self) {
        // 收集事件（避免在迭代 HashMap 时 borrow 冲突）
        let mut events = Vec::new();

        for (id, entry) in &mut self.trackers {
            let new_state = entry.tracker.check();
            let prev_state = entry.prev_state;

            if new_state != prev_state {
                match new_state {
                    LivenessState::Dead => {
                        events.push(ProcessWatchdogEvent::ProcessDead {
                            id: id.clone(),
                            state: new_state,
                        });
                    }
                    LivenessState::Unresponsive
                        if prev_state == LivenessState::Alive
                            || prev_state == LivenessState::Degraded =>
                    {
                        events.push(ProcessWatchdogEvent::ProcessUnresponsive {
                            id: id.clone(),
                            state: new_state,
                        });
                    }
                    LivenessState::Alive
                        if matches!(
                            prev_state,
                            LivenessState::Unresponsive | LivenessState::Dead
                        ) =>
                    {
                        events.push(ProcessWatchdogEvent::ProcessRecovered { id: id.clone() });
                    }
                    _ => {}
                }
                entry.prev_state = new_state;
            }
        }

        for event in events {
            if self.event_tx.send(event).await.is_err() {
                tracing::warn!("看门狗事件通道已关闭");
                break;
            }
        }
    }

    /// 发送健康报告
    pub async fn send_health_report(&self) {
        let states: Vec<(ProcessId, LivenessState)> = self
            .trackers
            .iter()
            .map(|(id, entry)| (id.clone(), entry.tracker.state()))
            .collect();

        tracing::debug!(?states, "健康报告");

        let _ = self
            .event_tx
            .send(ProcessWatchdogEvent::HealthReport { states })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_config() -> ProcessWatchdogConfig {
        ProcessWatchdogConfig {
            default_liveness: LivenessConfig {
                heartbeat_interval: Duration::from_millis(100),
                max_miss_count: 3,
                suspect_threshold: 1,
                restart_grace: Duration::from_millis(200),
            },
            check_interval: Duration::from_millis(50),
            health_report_interval: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn test_register_and_heartbeat() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("test-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");

        assert_eq!(wd.state(&id), Some(LivenessState::Alive));

        wd.record_heartbeat(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::Alive));
    }

    /// Step 2 Phase D.3 regression: register on an already-registered
    /// ProcessId must return `AlreadyRegisteredError` rather than
    /// silently overwriting the existing tracker. Closes Step 1 §六
    /// R1 ③ for watchdog.rs:108/121.
    #[tokio::test]
    async fn register_duplicate_returns_err() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("dup-proc");
        wd.register(id.clone()).expect("first register succeeds");
        // Mutate the tracker so we can detect "silent overwrite" if
        // the bug regresses (overwrite would reset state to Alive).
        wd.mark_shutting_down(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::ShuttingDown));

        // Second register must fail with AlreadyRegisteredError and
        // must not overwrite the existing tracker.
        let err = wd
            .register(id.clone())
            .expect_err("duplicate register must return AlreadyRegisteredError");
        assert_eq!(err.0, id, "error must carry the duplicate id");
        assert_eq!(
            wd.state(&id),
            Some(LivenessState::ShuttingDown),
            "duplicate register must not overwrite existing tracker state",
        );

        // register_with_config has the same contract.
        let custom = LivenessConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_miss_count: 1,
            suspect_threshold: 1,
            restart_grace: Duration::from_millis(50),
        };
        let err2 = wd
            .register_with_config(id.clone(), custom)
            .expect_err("duplicate register_with_config must return AlreadyRegisteredError");
        assert_eq!(err2.0, id);
        assert_eq!(
            wd.state(&id),
            Some(LivenessState::ShuttingDown),
            "register_with_config must also not overwrite",
        );
    }

    #[tokio::test]
    async fn test_unregister() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("test-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");
        assert!(wd.state(&id).is_some());

        wd.unregister(&id);
        assert!(wd.state(&id).is_none());
    }

    #[tokio::test]
    async fn test_mark_shutting_down() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("test-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");

        wd.mark_shutting_down(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::ShuttingDown));
    }

    #[tokio::test]
    async fn test_reset_after_restart() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("test-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");

        wd.mark_shutting_down(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::ShuttingDown));

        wd.reset(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::Alive));
    }

    #[tokio::test]
    async fn test_multiple_processes() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id_a = ProcessId::new("proc-a");
        let id_b = ProcessId::new("proc-b");
        let id_c = ProcessId::new("proc-c");

        wd.register(id_a.clone())
            .expect("test setup: id_a register");
        wd.register(id_b.clone())
            .expect("test setup: id_b register");
        wd.register(id_c.clone())
            .expect("test setup: id_c register");

        wd.record_heartbeat(&id_a);
        wd.mark_shutting_down(&id_b);

        assert_eq!(wd.state(&id_a), Some(LivenessState::Alive));
        assert_eq!(wd.state(&id_b), Some(LivenessState::ShuttingDown));
        assert_eq!(wd.state(&id_c), Some(LivenessState::Alive));
    }

    #[tokio::test]
    async fn test_send_health_report() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        wd.register(ProcessId::new("proc-a"))
            .expect("test setup: proc-a register");
        wd.register(ProcessId::new("proc-b"))
            .expect("test setup: proc-b register");

        wd.send_health_report().await;

        let event = rx.try_recv();
        assert!(event.is_ok());
        if let Ok(ProcessWatchdogEvent::HealthReport { states }) = event {
            assert_eq!(states.len(), 2);
        } else {
            panic!("应收到 HealthReport 事件");
        }
    }

    #[tokio::test]
    async fn test_process_unresponsive_event_fires() {
        // Alive → Unresponsive 应发送 ProcessUnresponsive 事件
        let (tx, mut rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("no-heartbeat");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");

        // 等待 suspect_threshold=1 个间隔（100ms）使 tracker → Unresponsive
        tokio::time::sleep(Duration::from_millis(150)).await;
        wd.do_check_cycle().await;

        let mut saw_unresponsive = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, ProcessWatchdogEvent::ProcessUnresponsive { .. }) {
                saw_unresponsive = true;
            }
        }
        assert!(
            saw_unresponsive,
            "Alive→Unresponsive 转换应发 ProcessUnresponsive"
        );
    }

    #[tokio::test]
    async fn test_mark_degraded_updates_tracker() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("degraded-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");

        wd.mark_degraded(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::Degraded));

        wd.clear_degraded(&id);
        assert_eq!(wd.state(&id), Some(LivenessState::Alive));
    }

    #[tokio::test]
    async fn test_force_dead_if_stale_rejects_within_grace() {
        // reset 后立即调用 force_dead_if_stale 应被拒绝，state 保持 Alive
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("grace-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");
        wd.reset(&id); // 将 tracker 的 reset_at 置为 now

        let accepted = wd.force_dead_if_stale(&id);
        assert!(!accepted, "grace 内应拒绝 force_dead_if_stale");
        assert_eq!(
            wd.state(&id),
            Some(LivenessState::Alive),
            "grace 内 state 应保持 Alive"
        );
    }

    #[tokio::test]
    async fn test_force_dead_if_stale_accepts_after_grace() {
        // make_config 的 restart_grace=200ms，等待 250ms 越过
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("expired-grace-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");
        wd.reset(&id);
        tokio::time::sleep(Duration::from_millis(250)).await;

        let accepted = wd.force_dead_if_stale(&id);
        assert!(accepted, "grace 过后应接受 force_dead_if_stale");
        assert_eq!(wd.state(&id), Some(LivenessState::Dead));
    }

    #[tokio::test]
    async fn test_epoch_increments_on_reset() {
        let (tx, _rx) = mpsc::channel(32);
        let mut wd = ProcessWatchdog::new(make_config(), tx);

        let id = ProcessId::new("epoch-proc");
        wd.register(id.clone())
            .expect("test setup: register fresh ProcessId must succeed");
        assert_eq!(wd.epoch(&id), Some(0));

        wd.reset(&id);
        assert_eq!(wd.epoch(&id), Some(1));

        wd.reset(&id);
        assert_eq!(wd.epoch(&id), Some(2));
    }

    #[tokio::test]
    async fn test_check_interval() {
        let config = make_config();
        let expected = config.check_interval;
        let (tx, _rx) = mpsc::channel(32);
        let wd = ProcessWatchdog::new(config, tx);
        assert_eq!(wd.check_interval(), expected);
    }

    #[tokio::test]
    async fn test_health_report_interval() {
        let config = make_config();
        let expected = config.health_report_interval;
        let (tx, _rx) = mpsc::channel(32);
        let wd = ProcessWatchdog::new(config, tx);
        assert_eq!(wd.health_report_interval(), expected);
    }
}
