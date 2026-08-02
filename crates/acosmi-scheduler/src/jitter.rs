//! Jitter 系统 — 防雷暴群效应
//!
//! 完整移植自 TS `cronTasks.ts` 的 jitter 机制：
//! - `jitter_frac(task_id)`: taskId 前 8 hex 字符 → [0, 1) 确定性哈希
//! - `jittered_next_cron_run_ms()`: 周期任务正向延迟（按间隔比例 + 上限）
//! - `one_shot_jittered_next_cron_run_ms()`: 一次性任务反向提前（分钟模门控）
//! - `CronJitterConfig`: 6 个调优旋钮（与 TS `DEFAULT_CRON_JITTER_CONFIG` 对齐）
//! - `find_missed_jobs()`: 检测 `CronJob` missed 任务（at/cron 两种 schedule）
//! - `parse_iso8601_to_ms()`: ISO-8601 时间字符串解析

use crate::cron_parse::{next_fire_time, parse_cron};
use crate::task_store::{CronJob, ScheduleKind};
use chrono::{NaiveDateTime, TimeZone, Timelike};

/// Cron 调度器抖动配置（与 TS `CronJitterConfig` 完全对齐）
///
/// 运行时可从 `GrowthBook` `tengu_kairos_cron_config` 动态注入。
#[derive(Debug, Clone)]
pub struct CronJitterConfig {
    /// 周期任务正向延迟 = 触发间隔 × `recurring_frac（上限` `recurring_cap_ms`）
    pub recurring_frac: f64,
    /// 周期任务正向延迟上限（ms）
    pub recurring_cap_ms: i64,
    /// 一次性任务反向提前上限（ms）
    pub one_shot_max_ms: i64,
    /// 一次性任务反向提前下限（ms）；0 = taskId hash 接近 0 时直接在整点触发
    pub one_shot_floor_ms: i64,
    /// 仅当触发分钟 % `one_shot_minute_mod` == 0 时应用一次性 jitter
    /// 30 → :00/:30；15 → :00/:15/:30/:45；1 → 每分钟
    pub one_shot_minute_mod: u32,
    /// 周期任务自动过期时间（ms），0 = 永不过期
    pub recurring_max_age_ms: i64,
}

/// TS `DEFAULT_CRON_JITTER_CONFIG` 的精确镜像
impl Default for CronJitterConfig {
    fn default() -> Self {
        Self {
            recurring_frac: 0.1,
            recurring_cap_ms: 15 * 60 * 1000, // 15 min
            one_shot_max_ms: 90 * 1000,       // 90 s
            one_shot_floor_ms: 0,
            one_shot_minute_mod: 30,
            recurring_max_age_ms: 7 * 24 * 60 * 60 * 1000, // 7 天
        }
    }
}

/// taskId 前 8 hex 字符 → [0.0, 1.0) 确定性分数
///
/// 与 TS `jitterFrac()` 完全对齐：
/// `parseInt(taskId.slice(0, 8), 16) / 0x1_0000_0000`
///
/// 非 hex id（手编辑 JSON）回退到 0 = 无 jitter。
#[must_use]
// hex 前 8 位最多 32 bit，u64 → f64 精度不会损失关键位（mantissa 52 bit 足够）。
#[allow(clippy::cast_precision_loss)]
pub fn jitter_frac(task_id: &str) -> f64 {
    let hex_part: String = task_id.chars().take(8).collect();
    u64::from_str_radix(&hex_part, 16).map_or(0.0, |n| n as f64 / 0x1_0000_0000_u64 as f64)
}

/// epoch ms → 本地时间 `NaiveDateTime`
///
/// 关键：cron 表达式在本地时区解析（与 TS `new Date(ms)` 行为一致），
/// 所以必须先转为 `DateTime<Local>` 再提取 `NaiveDateTime`。
fn ms_to_naive(ms: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|utc| utc.with_timezone(&chrono::Local).naive_local())
}

/// `NaiveDateTime` → epoch ms
fn naive_to_ms(dt: NaiveDateTime) -> i64 {
    chrono::Local.from_local_datetime(&dt).single().map_or_else(
        || {
            // DST 歧义或间隙：取最早候选（与 TS Date 行为一致）
            chrono::Local
                .from_local_datetime(&dt)
                .earliest()
                .map_or(0, |dt2: chrono::DateTime<chrono::Local>| {
                    dt2.timestamp_millis()
                })
        },
        |local: chrono::DateTime<chrono::Local>| local.timestamp_millis(),
    )
}

// ============================================================================
// ISO-8601 解析
// ============================================================================

/// 解析 ISO-8601 时间字符串为 epoch ms
///
/// 支持 RFC 3339（`2026-04-12T09:00:00+08:00`）和无时区（视为本地时间）两种格式。
#[must_use]
pub fn parse_iso8601_to_ms(s: &str) -> Option<i64> {
    // RFC 3339（带时区偏移）
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // 无时区（视为本地时间）
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return chrono::Local
                .from_local_datetime(&dt)
                .single()
                .map(|l| l.timestamp_millis());
        }
    }
    None
}

// ============================================================================
// Cron 下次触发时间计算（支持 IANA 时区）
// ============================================================================

/// 原始 next fire time（epoch ms），支持 IANA 时区
///
/// `tz=None` 时使用系统本地时区（与 TS `nextCronRunMs` 对齐）。
/// `tz=Some("Asia/Shanghai")` 时在指定时区解析 cron 表达式。
#[must_use]
pub fn next_cron_run_ms(cron: &str, from_ms: i64) -> Option<i64> {
    next_cron_run_ms_tz(cron, from_ms, None)
}

/// 带时区的 cron 下次触发时间计算
#[must_use]
pub fn next_cron_run_ms_tz(cron: &str, from_ms: i64, tz: Option<&str>) -> Option<i64> {
    let fields = parse_cron(cron)?;
    if let Some(tz_name) = tz {
        let tz: chrono_tz::Tz = tz_name.parse().ok()?;
        let from_dt = chrono::DateTime::from_timestamp_millis(from_ms)?;
        let from_local = from_dt.with_timezone(&tz).naive_local();
        let next_local = next_fire_time(&fields, from_local)?;
        tz.from_local_datetime(&next_local)
            .single()
            .map(|dt| dt.timestamp_millis())
    } else {
        let from = ms_to_naive(from_ms)?;
        let next = next_fire_time(&fields, from)?;
        Some(naive_to_ms(next))
    }
}

/// 周期任务 jittered next fire time（epoch ms）
///
/// 与 TS `jitteredNextCronRunMs()` 完全对齐，新增 IANA 时区支持。
#[must_use]
pub fn jittered_next_cron_run_ms(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
) -> Option<i64> {
    jittered_next_cron_run_ms_tz(cron, from_ms, task_id, cfg, None)
}

/// 带时区的周期任务 jittered next fire time
#[must_use]
// epoch ms 数量级 (~10^13) 远低于 f64 mantissa 52-bit 上限，且 jitter 是 ms 级别
// 加减；精度损失/截断对调度结果无可观察影响。
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn jittered_next_cron_run_ms_tz(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
    tz: Option<&str>,
) -> Option<i64> {
    let t1 = next_cron_run_ms_tz(cron, from_ms, tz)?;
    let Some(t2) = next_cron_run_ms_tz(cron, t1, tz) else {
        return Some(t1); // 无第二次匹配 → 不 jitter
    };
    let interval = (t2 - t1) as f64;
    let jitter =
        (jitter_frac(task_id) * cfg.recurring_frac * interval).min(cfg.recurring_cap_ms as f64);
    Some(t1 + jitter as i64)
}

/// 一次性任务 jittered next fire time（epoch ms）
///
/// 与 TS `oneShotJitteredNextCronRunMs()` 完全对齐，新增 IANA 时区支持。
#[must_use]
pub fn one_shot_jittered_next_cron_run_ms(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
) -> Option<i64> {
    one_shot_jittered_next_cron_run_ms_tz(cron, from_ms, task_id, cfg, None)
}

/// 带时区的一次性任务 jittered next fire time
#[must_use]
// 同 `jittered_next_cron_run_ms_tz`：ms 级别 i64↔f64 互转在调度精度内完全够用。
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn one_shot_jittered_next_cron_run_ms_tz(
    cron: &str,
    from_ms: i64,
    task_id: &str,
    cfg: &CronJitterConfig,
    tz: Option<&str>,
) -> Option<i64> {
    let t1 = next_cron_run_ms_tz(cron, from_ms, tz)?;

    // 检查分钟模门控 — 使用本地时间（与 TS getMinutes() 对齐）。
    // ms_to_naive 已经把 UTC ms 转成本地 NaiveDateTime，直接读 minute 即可；
    // 早期实现错误地 `t1_naive.and_utc().with_timezone(&Local)` 又转一次，
    // 在半小时偏移时区（IST +5:30 / NPL +5:45 / NL -3:30 / ACST +9:30）会
    // 把 minute 字段二次偏移，one-shot jitter 整点门控判错。
    let t1_naive = ms_to_naive(t1)?;
    let minute = t1_naive.minute();
    if cfg.one_shot_minute_mod == 0 || minute % cfg.one_shot_minute_mod != 0 {
        return Some(t1);
    }

    let lead = jitter_frac(task_id).mul_add(
        (cfg.one_shot_max_ms - cfg.one_shot_floor_ms) as f64,
        cfg.one_shot_floor_ms as f64,
    );
    Some((t1 - lead as i64).max(from_ms))
}

/// 检查周期任务是否已过期
///
/// 与 TS `isRecurringTaskAged()` 完全对齐。
#[must_use]
pub const fn is_recurring_task_aged(
    recurring: bool,
    permanent: bool,
    created_at: i64,
    now_ms: i64,
    max_age_ms: i64,
) -> bool {
    if max_age_ms == 0 {
        return false;
    }
    recurring && !permanent && (now_ms - created_at) >= max_age_ms
}

// ============================================================================
// Missed 任务检测
// ============================================================================

/// 检测错过的旧格式任务（进程停机期间应触发的任务）
///
/// 与 TS `findMissedTasks()` 完全对齐：
/// 从 createdAt 计算下次触发，若 < now 则为 missed。
#[must_use]
pub fn find_missed_tasks(tasks: &[crate::task_store::CronTask], now_ms: i64) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| next_cron_run_ms(&t.cron, t.created_at).is_some_and(|next| next < now_ms))
        .map(|t| t.id.clone())
        .collect()
}

/// 检测错过的 `CronJob` 任务
///
/// 仅检测 enabled + delete_after_run(one-shot) 任务。
/// - kind=cron: 从 `created_at_ms` 计算下次触发
/// - kind=at: at 时间 < now
/// - kind=every: 不检测（always 有下次时间）
#[must_use]
pub fn find_missed_jobs(jobs: &[CronJob], now_ms: i64) -> Vec<String> {
    jobs.iter()
        .filter(|j| {
            if !j.enabled {
                return false;
            }
            // 仅 one-shot 任务可被 "missed"
            if !j.is_one_shot() {
                return false;
            }
            match j.schedule.kind {
                ScheduleKind::Cron => j
                    .schedule
                    .expr
                    .as_deref()
                    .and_then(|expr| {
                        next_cron_run_ms_tz(expr, j.created_at_ms, j.schedule.tz.as_deref())
                    })
                    .is_some_and(|next| next < now_ms),
                ScheduleKind::At => j
                    .schedule
                    .at
                    .as_deref()
                    .and_then(parse_iso8601_to_ms)
                    .is_some_and(|ms| ms < now_ms),
                ScheduleKind::Every => false,
            }
        })
        .map(|j| j.id.clone())
        .collect()
}

/// 检测错过的 `CronJob` 任务，并返回每个 missed 任务**原本应触发的时刻**。
///
/// 与 [`find_missed_jobs`] 同样的检出逻辑，但额外回出 scheduled-at（epoch ms）：
/// - kind=cron: 从 `created_at_ms` 算出的下次触发时刻
/// - kind=at:   at 字符串解析出的绝对时刻
/// - kind=every: 不检测（恒有下次时间）
///
/// W-CRON-AUTOMATION-E2E P7：daemon 用 scheduled-at 判定 missed 任务是否
/// 落在补投窗口内 + 写进 outbox 的 `scheduled_at_ms` 字段。
#[must_use]
pub fn find_missed_jobs_detailed(jobs: &[CronJob], now_ms: i64) -> Vec<(String, i64)> {
    jobs.iter()
        .filter_map(|j| {
            if !j.enabled || !j.is_one_shot() {
                return None;
            }
            let scheduled = match j.schedule.kind {
                ScheduleKind::Cron => j.schedule.expr.as_deref().and_then(|expr| {
                    next_cron_run_ms_tz(expr, j.created_at_ms, j.schedule.tz.as_deref())
                }),
                ScheduleKind::At => j.schedule.at.as_deref().and_then(parse_iso8601_to_ms),
                ScheduleKind::Every => None,
            }?;
            if scheduled < now_ms {
                Some((j.id.clone(), scheduled))
            } else {
                None
            }
        })
        .collect()
}

/// 计算 `CronJob` 的下次 every 触发时间（epoch ms）
///
/// anchor + N * `every_ms` > `from_ms，N` ≥ 1
#[must_use]
pub const fn next_every_run_ms(every_ms: i64, anchor_ms: i64, from_ms: i64) -> Option<i64> {
    if every_ms <= 0 {
        return None;
    }
    if from_ms < anchor_ms {
        return Some(anchor_ms + every_ms);
    }
    let elapsed = from_ms - anchor_ms;
    let periods = elapsed / every_ms;
    Some(anchor_ms + (periods + 1) * every_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_frac_known_values() {
        // "00000000" → 0 / 0x100000000 = 0.0
        assert!((jitter_frac("00000000") - 0.0).abs() < f64::EPSILON);
        // "80000000" → 2147483648 / 4294967296 = 0.5
        assert!((jitter_frac("80000000") - 0.5).abs() < f64::EPSILON);
        // "ffffffff" → 4294967295 / 4294967296 ≈ 1.0
        assert!(jitter_frac("ffffffff") > 0.99);
        assert!(jitter_frac("ffffffff") < 1.0);
        // 非 hex → 0
        assert!((jitter_frac("zzzzzzzz") - 0.0).abs() < f64::EPSILON);
        // 短 id → 正常（取前 8 字符）
        assert!(jitter_frac("abcd") >= 0.0);
    }

    #[test]
    fn jitter_frac_matches_ts() {
        let frac = jitter_frac("a1b2c3d4");
        let expected = 0xa1b2c3d4_u64 as f64 / 0x1_0000_0000_u64 as f64;
        assert!(
            (frac - expected).abs() < 1e-10,
            "frac={frac}, expected={expected}"
        );
    }

    #[test]
    fn default_config_matches_ts() {
        let cfg = CronJitterConfig::default();
        assert!((cfg.recurring_frac - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.recurring_cap_ms, 15 * 60 * 1000);
        assert_eq!(cfg.one_shot_max_ms, 90 * 1000);
        assert_eq!(cfg.one_shot_floor_ms, 0);
        assert_eq!(cfg.one_shot_minute_mod, 30);
        assert_eq!(cfg.recurring_max_age_ms, 7 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn recurring_jitter_adds_delay() {
        let cfg = CronJitterConfig::default();
        let now = chrono::Utc::now().timestamp_millis();
        let raw = next_cron_run_ms("0 * * * *", now);
        let jittered = jittered_next_cron_run_ms("0 * * * *", now, "80000000", &cfg);
        assert!(raw.is_some());
        assert!(jittered.is_some());
        assert!(jittered.expect("jittered") >= raw.expect("raw"));
    }

    #[test]
    fn recurring_jitter_capped() {
        let cfg = CronJitterConfig {
            recurring_frac: 0.5,
            recurring_cap_ms: 1000,
            ..Default::default()
        };
        let now = chrono::Utc::now().timestamp_millis();
        let raw = next_cron_run_ms("0 * * * *", now).expect("valid");
        let jittered =
            jittered_next_cron_run_ms("0 * * * *", now, "ffffffff", &cfg).expect("valid");
        assert!(jittered - raw <= 1000);
    }

    #[test]
    fn one_shot_no_jitter_on_non_hot_minute() {
        let cfg = CronJitterConfig::default();
        let now = chrono::Utc::now().timestamp_millis();
        let raw = next_cron_run_ms("15 9 * * *", now);
        let jittered = one_shot_jittered_next_cron_run_ms("15 9 * * *", now, "80000000", &cfg);
        assert_eq!(raw, jittered);
    }

    #[test]
    fn one_shot_jitter_on_hot_minute() {
        let cfg = CronJitterConfig::default();
        let now = chrono::Utc::now().timestamp_millis();
        let raw = next_cron_run_ms("0 9 * * *", now);
        let jittered = one_shot_jittered_next_cron_run_ms("0 9 * * *", now, "80000000", &cfg);
        assert!(raw.is_some());
        assert!(jittered.is_some());
        assert!(jittered.expect("jittered") <= raw.expect("raw"));
    }

    #[test]
    fn one_shot_clamped_to_from() {
        let cfg = CronJitterConfig {
            one_shot_max_ms: 999_999_999,
            one_shot_minute_mod: 1,
            ..Default::default()
        };
        let now = chrono::Utc::now().timestamp_millis();
        let jittered = one_shot_jittered_next_cron_run_ms("* * * * *", now, "ffffffff", &cfg);
        assert!(jittered.is_some());
        assert!(jittered.expect("jittered") >= now);
    }

    #[test]
    fn is_recurring_task_aged_logic() {
        let max_age = 7 * 24 * 60 * 60 * 1000_i64;
        let now = chrono::Utc::now().timestamp_millis();
        assert!(!is_recurring_task_aged(true, false, now, now, max_age));
        assert!(is_recurring_task_aged(
            true,
            false,
            now - 8 * 24 * 60 * 60 * 1000,
            now,
            max_age
        ));
        assert!(!is_recurring_task_aged(
            true,
            true,
            now - 8 * 24 * 60 * 60 * 1000,
            now,
            max_age
        ));
        assert!(!is_recurring_task_aged(
            false,
            false,
            now - 8 * 24 * 60 * 60 * 1000,
            now,
            max_age
        ));
        assert!(!is_recurring_task_aged(true, false, 0, now, 0));
    }

    #[test]
    fn find_missed_detects_overdue() {
        let ancient = 1_000_i64;
        let tasks = vec![
            crate::task_store::CronTask {
                id: "missed-1".into(),
                cron: "0 9 * * *".into(),
                prompt: "p".into(),
                created_at: ancient,
                last_fired_at: None,
                recurring: None,
                permanent: None,
            },
            crate::task_store::CronTask {
                id: "future-1".into(),
                cron: "0 9 * * *".into(),
                prompt: "p".into(),
                created_at: chrono::Utc::now().timestamp_millis(),
                last_fired_at: None,
                recurring: None,
                permanent: None,
            },
        ];
        let now = chrono::Utc::now().timestamp_millis();
        let missed = find_missed_tasks(&tasks, now);
        assert_eq!(missed, vec!["missed-1".to_string()]);
    }

    // --- 新增测试 ---

    #[test]
    fn parse_iso8601_rfc3339() {
        let ms = parse_iso8601_to_ms("2026-04-12T09:00:00+08:00");
        assert!(ms.is_some());
        let ms = ms.unwrap();
        // 2026-04-12T01:00:00Z in epoch ms
        let expected = chrono::DateTime::parse_from_rfc3339("2026-04-12T09:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(ms, expected);
    }

    #[test]
    fn parse_iso8601_naive() {
        let ms = parse_iso8601_to_ms("2026-04-12T09:00:00");
        assert!(ms.is_some());
    }

    #[test]
    fn parse_iso8601_invalid() {
        assert!(parse_iso8601_to_ms("not-a-date").is_none());
        assert!(parse_iso8601_to_ms("").is_none());
    }

    #[test]
    fn next_every_run_basic() {
        let anchor = 1000_i64;
        let every = 60_000_i64; // 60s
        // from_ms = anchor + 30s → next = anchor + 60s
        assert_eq!(next_every_run_ms(every, anchor, 31_000), Some(61_000));
        // from_ms = anchor + 120s → next = anchor + 180s
        assert_eq!(next_every_run_ms(every, anchor, 121_000), Some(181_000));
        // from_ms < anchor → next = anchor + every
        assert_eq!(next_every_run_ms(every, anchor, 500), Some(61_000));
        // invalid
        assert_eq!(next_every_run_ms(0, anchor, 1000), None);
        assert_eq!(next_every_run_ms(-1, anchor, 1000), None);
    }

    #[test]
    fn cron_run_ms_with_tz() {
        // 使用 UTC 时区，结果应与直接计算一致
        let now = chrono::Utc::now().timestamp_millis();
        let result_tz = next_cron_run_ms_tz("0 * * * *", now, Some("UTC"));
        assert!(result_tz.is_some());
        // UTC 时区下的结果应该是某个整点
        let fire_ms = result_tz.unwrap();
        assert!(fire_ms > now);
    }

    #[test]
    fn cron_run_ms_with_invalid_tz_returns_none() {
        let now = chrono::Utc::now().timestamp_millis();
        let result = next_cron_run_ms_tz("0 * * * *", now, Some("Invalid/TZ"));
        assert!(result.is_none());
    }

    #[test]
    fn find_missed_jobs_cron() {
        use crate::task_store::*;
        let ancient = 1_000_i64;
        let now = chrono::Utc::now().timestamp_millis();
        let jobs = vec![
            CronJob {
                id: "missed-cron".into(),
                agent_id: None,
                name: "m".into(),
                description: None,
                owner: None,
                enabled: true,
                delete_after_run: Some(true),
                created_at_ms: ancient,
                updated_at_ms: ancient,
                schedule: CronSchedule {
                    kind: ScheduleKind::Cron,
                    at: None,
                    every_ms: None,
                    anchor_ms: None,
                    expr: Some("0 9 * * *".into()),
                    tz: None,
                },
                session_target: SessionTarget::Main,
                wake_mode: WakeMode::NextHeartbeat,
                payload: CronPayload {
                    kind: PayloadKind::SystemEvent,
                    text: Some("test".into()),
                    message: None,
                    model: None,
                    thinking: None,
                    timeout_seconds: None,
                    allow_unsafe_external_content: None,
                    deliver: None,
                    channel: None,
                    to: None,
                    best_effort_deliver: None,
                },
                delivery: None,
                state: CronJobState::default(),
                permanent: None,
                session_key: None,
                channel_id: None,
                continuation_kind: None,
            },
            // Recurring task → not missed (not one-shot)
            CronJob {
                id: "recurring-ok".into(),
                agent_id: None,
                name: "r".into(),
                description: None,
                owner: None,
                enabled: true,
                delete_after_run: Some(false),
                created_at_ms: ancient,
                updated_at_ms: ancient,
                schedule: CronSchedule {
                    kind: ScheduleKind::Cron,
                    at: None,
                    every_ms: None,
                    anchor_ms: None,
                    expr: Some("0 9 * * *".into()),
                    tz: None,
                },
                session_target: SessionTarget::Main,
                wake_mode: WakeMode::NextHeartbeat,
                payload: CronPayload {
                    kind: PayloadKind::SystemEvent,
                    text: Some("test".into()),
                    message: None,
                    model: None,
                    thinking: None,
                    timeout_seconds: None,
                    allow_unsafe_external_content: None,
                    deliver: None,
                    channel: None,
                    to: None,
                    best_effort_deliver: None,
                },
                delivery: None,
                state: CronJobState::default(),
                permanent: None,
                session_key: None,
                channel_id: None,
                continuation_kind: None,
            },
        ];
        let missed = find_missed_jobs(&jobs, now);
        assert_eq!(missed, vec!["missed-cron".to_string()]);
    }

    #[test]
    fn find_missed_jobs_at() {
        use crate::task_store::*;
        let now = chrono::Utc::now().timestamp_millis();
        let jobs = vec![CronJob {
            id: "missed-at".into(),
            agent_id: None,
            name: "a".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2020-01-01T00:00:00+00:00".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("test".into()),
                message: None,
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }];
        let missed = find_missed_jobs(&jobs, now);
        assert_eq!(missed, vec!["missed-at".to_string()]);
    }

    /// `find_missed_jobs_detailed`：检出与 `find_missed_jobs` 一致，且回出
    /// 每个 missed 任务的计划触发时刻。
    #[test]
    fn find_missed_jobs_detailed_returns_scheduled_at() {
        use crate::task_store::*;
        let now = chrono::Utc::now().timestamp_millis();
        let at_str = "2020-01-01T00:00:00+00:00";
        let at_ms = parse_iso8601_to_ms(at_str).unwrap();
        let jobs = vec![CronJob {
            id: "missed-at".into(),
            agent_id: None,
            name: "a".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some(at_str.into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("t".into()),
                message: None,
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }];
        let detailed = find_missed_jobs_detailed(&jobs, now);
        assert_eq!(detailed.len(), 1);
        assert_eq!(detailed[0].0, "missed-at");
        assert_eq!(
            detailed[0].1, at_ms,
            "scheduled_at 应是 at 字符串解析的时刻"
        );
    }

    /// `At` 任务 `delete_after_run == None`（入口省略 deleteAfterRun）。
    /// `At` 天然 one-shot：overdue 时必须被检测为 missed，不能因字段缺失漏检。
    #[test]
    fn find_missed_jobs_at_with_none_delete_after_run() {
        use crate::task_store::*;
        let now = chrono::Utc::now().timestamp_millis();
        let jobs = vec![CronJob {
            id: "missed-at-none".into(),
            agent_id: None,
            name: "a".into(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None, // 入口省略 deleteAfterRun
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2020-01-01T00:00:00+00:00".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: Some("test".into()),
                message: None,
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }];
        let missed = find_missed_jobs(&jobs, now);
        assert_eq!(missed, vec!["missed-at-none".to_string()]);
    }
}
