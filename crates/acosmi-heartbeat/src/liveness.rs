//! 统一存活判定与 kill-safe 状态机
//!
//! 提供 `LivenessTracker`，用于跟踪单个进程/任务的心跳状态。
//! 状态机保证在异步取消（cancel-safe）时不会留下不一致的中间态。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ────────────────────────── 存活状态枚举 ──────────────────────────

/// 存活状态，描述被监控实体的健康情况。
///
/// 五值枚举，与 Go (`LivenessStatus`) 和 TS (`LivenessStatus`) 合约对齐：
/// alive / degraded / unresponsive / dead / `shutting_down`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LivenessState {
    /// 健康：按时收到心跳
    Alive,
    /// 降级：组件仍在响应心跳，但自报性能下降（内存压力、事件循环延迟等）。
    /// 由外部通过 `mark_degraded()` 设置，不由 miss count 自动推导。
    Degraded,
    /// 疑似异常：错过心跳次数达到 suspect 阈值（对应 Go/TS 的 "unresponsive"）
    Unresponsive,
    /// 确认异常：连续多次心跳超时，已超过最大允许 miss 次数
    Dead,
    /// 正在关闭（由上层主动标记，不应触发重启）
    ShuttingDown,
}

impl std::fmt::Display for LivenessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alive => write!(f, "Alive"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Unresponsive => write!(f, "Unresponsive"),
            Self::Dead => write!(f, "Dead"),
            Self::ShuttingDown => write!(f, "ShuttingDown"),
        }
    }
}

// ────────────────────────── 配置 ──────────────────────────

/// 存活检测配置
#[derive(Debug, Clone)]
pub struct LivenessConfig {
    /// 心跳间隔——期望多久收到一次心跳
    pub heartbeat_interval: Duration,
    /// 允许连续错过的最大心跳次数；超过此值则判定为 Dead
    pub max_miss_count: u32,
    /// 疑似异常阈值：连续错过几次心跳进入 Unresponsive 状态。
    /// `check()` 运行时会 clamp 为 `min(suspect_threshold, max_miss_count)`，
    /// 因此配置者传入畸形值（例如 suspect > max）状态机仍保持一致语义。
    pub suspect_threshold: u32,
    /// Restart grace window: `reset()` 调用后的这段时间内，`force_dead_if_stale`
    /// 会**拒绝**把 tracker 拉回 Dead。
    ///
    /// 用途：kill-restart 完成、tracker 刚被 reset 到 Alive，新进程尚未来得及
    /// 发第一次心跳，但上游（例如 Go 的 ChannelHealth）可能仍在上报旧实例的
    /// "死亡"告警（源自 reset 之前的故障时间窗）。本 grace window 让 tracker
    /// 在这段时间内对冗余 `force_dead` 信号免疫，避免冗余 kill-restart（参见
    /// 架构文档 §六 Case A）。
    ///
    /// 默认值由 `LivenessConfig::default` 计算为 `heartbeat_interval * 2`，
    /// 保证新进程正常启动并上报 1–2 次心跳的时间里不被误杀。
    pub restart_grace: Duration,
}

impl Default for LivenessConfig {
    /// 默认配置：5s 间隔，miss 6 次判定 `Dead（≈35s），restart_grace=10s`。
    /// `max_miss_count=6` 是为容忍 Windows 系统休眠/短暂冻结，比原值 3（≈20s）更宽松。
    fn default() -> Self {
        let heartbeat_interval = Duration::from_secs(5);
        Self {
            heartbeat_interval,
            max_miss_count: 6,
            suspect_threshold: 1,
            restart_grace: heartbeat_interval * 2,
        }
    }
}

// ────────────────────────── 状态机 ──────────────────────────

/// kill-safe 存活跟踪状态机
///
/// 所有方法都是同步的 `&mut self`，保证在异步取消时不会产生半更新。
/// 外部（如 supervisor 的 `ProcessWatchdog`）负责定期调用 `check()` 驱动状态转移。
#[derive(Debug)]
pub struct LivenessTracker {
    /// 当前状态
    state: LivenessState,
    /// 上次收到心跳的时刻
    last_heartbeat: Instant,
    /// 当前连续 miss 次数
    miss_count: u32,
    /// 配置
    config: LivenessConfig,
    /// 重启代数计数（epoch）。每次 `reset()` 递增，用于让外部观察者（例如
    /// 异步事件处理链上的陈旧 `ProcessDead` 事件）判定自己手里的 epoch 是否
    /// 已过期。跨越 u64 溢出是实际不可能的（即使 1 纳秒一次也要 584 年）。
    epoch: u64,
    /// 最近一次 `reset()` 的时刻；`None` 表示尚未经历过 reset（即 tracker
    /// 从 `new` 后一直没被重启）。`force_dead_if_stale` 用它判断是否在
    /// `restart_grace` 窗口内。
    reset_at: Option<Instant>,
}

impl LivenessTracker {
    /// 创建新的存活跟踪器，初始状态为 `Alive`
    #[must_use]
    pub fn new(config: LivenessConfig) -> Self {
        Self {
            state: LivenessState::Alive,
            last_heartbeat: Instant::now(),
            miss_count: 0,
            config,
            epoch: 0,
            reset_at: None,
        }
    }

    /// 收到心跳时调用：重置 miss 计数。
    ///
    /// 状态转换规则：
    /// - `ShuttingDown` → 忽略（不应因心跳而取消优雅关闭）
    /// - `Degraded` → 保持 Degraded（组件仍在发心跳但性能降级，需外部 `clear_degraded` 恢复）
    /// - 其他 → Alive
    pub fn record_heartbeat(&mut self) {
        if self.state == LivenessState::ShuttingDown {
            tracing::debug!("收到心跳，但当前处于 ShuttingDown 状态，忽略");
            return;
        }

        let previous = self.state;
        self.last_heartbeat = Instant::now();
        self.miss_count = 0;

        // Degraded 组件仍在发心跳，但性能降级——保持 Degraded 直到外部清除
        if self.state != LivenessState::Degraded {
            self.state = LivenessState::Alive;
        }

        if previous != self.state {
            tracing::info!(
                previous = %previous,
                current = %self.state,
                "心跳恢复，状态从 {} 变为 {}",
                previous,
                self.state
            );
        }
    }

    /// 检查并更新当前状态
    ///
    /// 根据距离上次心跳的时间差和 miss 计数驱动状态转移：
    /// - 未超时 → Alive（若当前 Degraded 则保持 Degraded）
    /// - miss >= `suspect_threshold` → Unresponsive
    /// - miss > `max_miss_count` → Dead
    ///
    /// `ShuttingDown` 状态不受此方法影响。
    /// `Degraded` 状态在心跳正常时保持，心跳丢失时可转为 Unresponsive/Dead。
    pub fn check(&mut self) -> LivenessState {
        self.check_at(Instant::now())
    }

    fn check_at(&mut self, now: Instant) -> LivenessState {
        // 关闭中，直接返回，不做状态转移
        if self.state == LivenessState::ShuttingDown {
            return LivenessState::ShuttingDown;
        }

        let elapsed = now.saturating_duration_since(self.last_heartbeat);

        // Windows 休眠唤醒保护：
        // Windows 的 QueryPerformanceCounter 在系统休眠期间继续计时，
        // 唤醒后 elapsed 出现巨大跳变，所有 tracker 立即判定 Dead。
        // macOS (CLOCK_UPTIME_RAW) 和 Linux (CLOCK_MONOTONIC) 休眠时停止计时，无需此保护。
        // 检测阈值：Dead 阈值的 2 倍（正常运行不可能出现如此大的间隔）。
        #[cfg(windows)]
        {
            let dead_threshold = self.config.heartbeat_interval * (self.config.max_miss_count + 1);
            if elapsed > dead_threshold * 2 {
                tracing::info!(
                    elapsed_secs = elapsed.as_secs(),
                    threshold_secs = (dead_threshold * 2).as_secs(),
                    "检测到时间跳变（Windows 系统休眠唤醒），重置心跳计时"
                );
                self.last_heartbeat = now;
                self.miss_count = 0;
                return self.state;
            }
        }

        // 计算当前应累计的 miss 次数（向下取整）
        let interval_ms = self.config.heartbeat_interval.as_millis();
        let expected_misses = elapsed
            .as_millis()
            .checked_div(interval_ms)
            .and_then(|misses| misses.try_into().ok())
            .unwrap_or(u32::MAX);

        // 更新 miss_count 为实际 miss 次数
        self.miss_count = expected_misses;

        // 根据 miss_count 判定状态
        // Degraded 在心跳正常（miss=0）时保持，心跳丢失时可升级为 Unresponsive/Dead
        //
        // 不变量强制：若配置者把 suspect_threshold 设得 > max_miss_count，
        // clamp 为 max_miss_count 保证 Unresponsive 总能在 Dead 之前被经过。
        let effective_suspect = self
            .config
            .suspect_threshold
            .min(self.config.max_miss_count);
        let new_state = if self.miss_count > self.config.max_miss_count {
            LivenessState::Dead
        } else if self.miss_count >= effective_suspect {
            LivenessState::Unresponsive
        } else if self.state == LivenessState::Degraded {
            LivenessState::Degraded
        } else {
            LivenessState::Alive
        };

        // 仅在状态变化时记录日志
        if new_state != self.state {
            tracing::warn!(
                previous = %self.state,
                current = %new_state,
                miss_count = self.miss_count,
                elapsed_ms = elapsed.as_millis() as u64,
                "存活状态变更: {} -> {}",
                self.state,
                new_state
            );
        }

        self.state = new_state;
        self.state
    }

    /// 标记为降级运行（由外部根据 pong 报告中的内存/延迟等指标设置）
    ///
    /// 降级状态下心跳仍正常接收，但不触发重启。
    /// 当降级条件消除后，通过 `clear_degraded()` 恢复为 Alive。
    pub fn mark_degraded(&mut self) {
        if self.state == LivenessState::ShuttingDown {
            tracing::debug!("当前处于 ShuttingDown，忽略 Degraded 标记");
            return;
        }
        tracing::info!(previous = %self.state, "标记为 Degraded");
        self.state = LivenessState::Degraded;
    }

    /// 清除降级状态，恢复为 Alive
    pub fn clear_degraded(&mut self) {
        if self.state == LivenessState::Degraded {
            tracing::info!("清除 Degraded 状态，恢复为 Alive");
            self.state = LivenessState::Alive;
        }
    }

    /// 标记为正在关闭，阻止后续重启逻辑触发
    pub fn mark_shutting_down(&mut self) {
        tracing::info!(previous = %self.state, "标记为 ShuttingDown");
        self.state = LivenessState::ShuttingDown;
    }

    /// 强制标记为 Dead（外部 ground-truth 源，例如 Go 已验证 TS 挂起）
    ///
    /// 与 `check()` 基于时间窗口推导出的 Dead 语义一致，但不需要等 `miss_count`
    /// 自然累计到阈值。典型使用：Go 通过独立 ping/pong 探测到 TS event-loop
    /// 死锁，下发告警给 Rust；Rust 立即标记 Dead 并走 kill-restart 路径，
    /// 节省 `max_miss_count` × `heartbeat_interval（默认` 35s）延迟。
    ///
    /// `ShuttingDown` 状态不受此方法影响（正在关闭的进程不该被重启）。
    ///
    /// **注意**：新代码应优先使用 `force_dead_if_stale`，它在 `restart_grace`
    /// 窗口内会拒绝请求以避免冗余 kill-restart。本方法保留用于"明确知道需要
    /// 绕过 grace 立即 Dead"的极少数场景（如 shutdown 前的强制清理）。
    pub fn force_dead(&mut self) {
        if self.state == LivenessState::ShuttingDown {
            tracing::debug!("当前处于 ShuttingDown，忽略 force_dead");
            return;
        }
        tracing::warn!(previous = %self.state, "外部 force_dead（ground-truth 信号）");
        self.state = LivenessState::Dead;
    }

    /// 带 restart grace 的 `force_dead：在` `reset_at` 到当前的时间窗小于
    /// `restart_grace` 时，**拒绝**把状态拉回 Dead，返回 `false`。
    ///
    /// kill-restart 完成、tracker 刚被 `reset()` 回 Alive 时，新进程可能尚未
    /// 发出第一次心跳。此时到达的上游 `force_dead` 信号可能属于 reset 前的
    /// 旧故障窗口；在 grace 内接受它会形成冗余 kill-restart 循环。
    ///
    /// 返回 `true` 表示 grace 已过（或从未 reset 过），按 `force_dead` 语义处理；
    /// 返回 `false` 表示在 grace 内，调用方应跳过后续 kill-restart。
    ///
    /// `ShuttingDown` 状态仍然不受影响，返回 `false`（与 `force_dead` 对齐）。
    #[must_use]
    pub fn force_dead_if_stale(&mut self) -> bool {
        self.force_dead_if_stale_at(Instant::now())
    }

    fn force_dead_if_stale_at(&mut self, now: Instant) -> bool {
        if self.state == LivenessState::ShuttingDown {
            tracing::debug!("当前处于 ShuttingDown，拒绝 force_dead_if_stale");
            return false;
        }
        if let Some(reset_at) = self.reset_at {
            let elapsed = now.saturating_duration_since(reset_at);
            if elapsed < self.config.restart_grace {
                tracing::info!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    grace_ms = self.config.restart_grace.as_millis() as u64,
                    epoch = self.epoch,
                    "在 restart grace 窗口内，忽略 force_dead（视为陈旧告警）"
                );
                return false;
            }
        }
        tracing::warn!(
            previous = %self.state,
            epoch = self.epoch,
            "force_dead_if_stale：grace 已过，标记为 Dead"
        );
        self.state = LivenessState::Dead;
        true
    }

    /// 判断是否需要触发重启
    ///
    /// 仅当状态为 `Dead` 时返回 `true`；
    /// `ShuttingDown` 不触发重启。
    #[must_use]
    pub fn is_restart_needed(&self) -> bool {
        self.state == LivenessState::Dead
    }

    /// 重启后重置状态：miss 清零，状态回到 Alive，时间戳刷新，epoch 递增，
    /// `reset_at` 记录为 now（用于 `force_dead_if_stale` 的 grace 判定）。
    ///
    /// epoch 的递增让下游事件处理者可以识别"这个 `ProcessDead` 事件携带的是
    /// 旧 epoch，tracker 已经被重启过了"，配合 supervisor 主循环的事件过滤
    /// 避免重入式 kill-restart。
    pub fn reset(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        tracing::info!(
            previous = %self.state,
            new_epoch = self.epoch,
            "重置存活跟踪器"
        );
        self.state = LivenessState::Alive;
        self.last_heartbeat = Instant::now();
        self.miss_count = 0;
        self.reset_at = Some(self.last_heartbeat);
    }

    // ── 只读访问器 ──

    /// 当前状态
    #[must_use]
    pub const fn state(&self) -> LivenessState {
        self.state
    }

    /// 当前连续 miss 次数
    #[must_use]
    pub const fn miss_count(&self) -> u32 {
        self.miss_count
    }

    /// 距上次心跳的时间
    #[must_use]
    pub fn elapsed_since_last_heartbeat(&self) -> Duration {
        self.last_heartbeat.elapsed()
    }

    /// 当前重启代数（epoch），每次 `reset()` 后递增
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 距最近一次 `reset()` 的时间；尚未 reset 过时返回 `None`
    #[must_use]
    pub fn elapsed_since_reset(&self) -> Option<Duration> {
        self.reset_at.map(|t| t.elapsed())
    }

    /// 获取配置引用
    #[must_use]
    pub const fn config(&self) -> &LivenessConfig {
        &self.config
    }
}

// ────────────────────────── 单元测试 ──────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助：创建一个短间隔配置，方便测试
    fn test_config() -> LivenessConfig {
        LivenessConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_miss_count: 3,
            suspect_threshold: 1,
            restart_grace: Duration::from_millis(100),
        }
    }

    fn check_after_elapsed(tracker: &mut LivenessTracker, elapsed: Duration) -> LivenessState {
        tracker.check_at(tracker.last_heartbeat + elapsed)
    }

    fn force_dead_after_reset_elapsed(tracker: &mut LivenessTracker, elapsed: Duration) -> bool {
        let reset_at = tracker.reset_at.expect("reset_at should exist after reset");
        tracker.force_dead_if_stale_at(reset_at + elapsed)
    }

    #[test]
    fn initial_state_is_alive() {
        let tracker = LivenessTracker::new(test_config());
        assert_eq!(tracker.state(), LivenessState::Alive);
        assert_eq!(tracker.miss_count(), 0);
        assert!(!tracker.is_restart_needed());
    }

    #[test]
    fn check_stays_alive_within_interval() {
        let mut tracker = LivenessTracker::new(test_config());
        // 刚创建，尚未超时
        let state = tracker.check();
        assert_eq!(state, LivenessState::Alive);
    }

    #[test]
    fn check_transitions_to_unresponsive() {
        let mut tracker = LivenessTracker::new(test_config());
        // 模拟超过 1 个心跳间隔（>50ms）
        let state = check_after_elapsed(&mut tracker, Duration::from_millis(70));
        assert_eq!(state, LivenessState::Unresponsive);
        assert!(!tracker.is_restart_needed());
    }

    #[test]
    fn check_transitions_to_dead() {
        let mut tracker = LivenessTracker::new(test_config());
        // 模拟超过 3 个心跳间隔（>150ms），miss > max_miss_count=3
        let state = check_after_elapsed(&mut tracker, Duration::from_millis(210));
        assert_eq!(state, LivenessState::Dead);
        assert!(tracker.is_restart_needed());
    }

    #[test]
    fn record_heartbeat_resets_state() {
        let mut tracker = LivenessTracker::new(test_config());
        // 先让它变成 Unresponsive
        let _ = check_after_elapsed(&mut tracker, Duration::from_millis(70));
        assert_eq!(tracker.state(), LivenessState::Unresponsive);

        // 收到心跳，应恢复
        tracker.record_heartbeat();
        assert_eq!(tracker.state(), LivenessState::Alive);
        assert_eq!(tracker.miss_count(), 0);
    }

    #[test]
    fn degraded_preserved_on_heartbeat() {
        let mut tracker = LivenessTracker::new(test_config());
        tracker.mark_degraded();
        assert_eq!(tracker.state(), LivenessState::Degraded);

        // 收到心跳时，Degraded 应保持（组件仍在发心跳但性能降级）
        tracker.record_heartbeat();
        assert_eq!(tracker.state(), LivenessState::Degraded);
        assert_eq!(tracker.miss_count(), 0);

        // check() 也应保持 Degraded（心跳正常时）
        let state = tracker.check();
        assert_eq!(state, LivenessState::Degraded);
        assert!(!tracker.is_restart_needed());
    }

    #[test]
    fn degraded_transitions_to_dead_on_miss() {
        let mut tracker = LivenessTracker::new(test_config());
        tracker.mark_degraded();

        // 模拟超过 max_miss_count 个心跳间隔
        let state = check_after_elapsed(&mut tracker, Duration::from_millis(210));
        assert_eq!(state, LivenessState::Dead);
        assert!(tracker.is_restart_needed());
    }

    #[test]
    fn clear_degraded_restores_alive() {
        let mut tracker = LivenessTracker::new(test_config());
        tracker.mark_degraded();
        assert_eq!(tracker.state(), LivenessState::Degraded);

        tracker.clear_degraded();
        assert_eq!(tracker.state(), LivenessState::Alive);
    }

    #[test]
    fn shutting_down_blocks_restart_and_heartbeat() {
        let mut tracker = LivenessTracker::new(test_config());
        tracker.mark_shutting_down();
        assert_eq!(tracker.state(), LivenessState::ShuttingDown);
        assert!(!tracker.is_restart_needed());

        // 心跳不应改变 ShuttingDown 状态
        tracker.record_heartbeat();
        assert_eq!(tracker.state(), LivenessState::ShuttingDown);

        // check 也不应改变 ShuttingDown 状态
        let state = tracker.check();
        assert_eq!(state, LivenessState::ShuttingDown);
    }

    #[test]
    fn suspect_threshold_clamped_to_max_miss_count() {
        // 畸形配置：suspect 比 max 大，clamp 后应仍然在 max 触发 Unresponsive
        let cfg = LivenessConfig {
            heartbeat_interval: Duration::from_millis(50),
            max_miss_count: 3,
            suspect_threshold: 100, // 刻意远大于 max
            restart_grace: Duration::from_millis(100),
        };
        let mut tracker = LivenessTracker::new(cfg);

        // miss=3（刚好 ≤ max），在未 clamp 的旧实现下 100 > 3 会让 state 停留 Alive，
        // clamp 后 effective_suspect=3，所以 miss=3 ≥ 3 → Unresponsive。
        let state = check_after_elapsed(&mut tracker, Duration::from_millis(150));
        assert_eq!(state, LivenessState::Unresponsive);

        // miss=4 > max → Dead，确保 clamp 不影响 Dead 阈值本身。
        let state = check_after_elapsed(&mut tracker, Duration::from_millis(200));
        assert_eq!(state, LivenessState::Dead);
    }

    #[test]
    fn reset_after_dead() {
        let mut tracker = LivenessTracker::new(test_config());
        let _ = check_after_elapsed(&mut tracker, Duration::from_millis(210));
        assert_eq!(tracker.state(), LivenessState::Dead);

        tracker.reset();
        assert_eq!(tracker.state(), LivenessState::Alive);
        assert_eq!(tracker.miss_count(), 0);
    }

    // ── restart epoch + force_dead_if_stale 相关测试 ──

    #[test]
    fn epoch_starts_at_zero_and_increments_on_reset() {
        let mut tracker = LivenessTracker::new(test_config());
        assert_eq!(tracker.epoch(), 0);
        assert!(tracker.elapsed_since_reset().is_none());

        tracker.reset();
        assert_eq!(tracker.epoch(), 1);
        assert!(tracker.elapsed_since_reset().is_some());

        tracker.reset();
        assert_eq!(tracker.epoch(), 2);
    }

    #[test]
    fn force_dead_if_stale_without_reset_returns_true() {
        // 从未 reset 过的 tracker：force_dead_if_stale 应等价于 force_dead
        let mut tracker = LivenessTracker::new(test_config());
        assert!(tracker.force_dead_if_stale());
        assert_eq!(tracker.state(), LivenessState::Dead);
    }

    #[test]
    fn force_dead_if_stale_within_grace_is_rejected() {
        // reset 后立即调用 force_dead_if_stale：在 grace 内应拒绝（返回 false），state 保持 Alive
        let mut tracker = LivenessTracker::new(test_config());
        tracker.reset();
        assert_eq!(tracker.state(), LivenessState::Alive);

        let accepted = force_dead_after_reset_elapsed(&mut tracker, Duration::ZERO);
        assert!(!accepted, "grace 内应拒绝 force_dead");
        assert_eq!(tracker.state(), LivenessState::Alive);
    }

    #[test]
    fn force_dead_if_stale_after_grace_is_accepted() {
        // reset 后等 grace+，force_dead_if_stale 应生效
        let mut tracker = LivenessTracker::new(test_config());
        tracker.reset();

        // grace=100ms，模拟 120ms 确保越过
        let accepted = force_dead_after_reset_elapsed(&mut tracker, Duration::from_millis(120));
        assert!(accepted, "grace 过后应接受 force_dead");
        assert_eq!(tracker.state(), LivenessState::Dead);
    }

    #[test]
    fn force_dead_if_stale_blocked_by_shutting_down() {
        let mut tracker = LivenessTracker::new(test_config());
        tracker.mark_shutting_down();

        let accepted = tracker.force_dead_if_stale();
        assert!(!accepted, "ShuttingDown 状态下应始终返回 false");
        assert_eq!(tracker.state(), LivenessState::ShuttingDown);
    }
}
