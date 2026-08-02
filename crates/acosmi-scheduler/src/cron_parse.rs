//! 5-field cron 表达式解析器 + 下次触发时间计算
//!
//! 语法: minute hour day-of-month month day-of-week
//! 支持: 通配符, N, */N (step), N-M (range), N,M (list)
//! 不支持: L, W, ?, 名称别名
//! 时区: 使用调用方的本地时区

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

/// 解析后的 cron 字段
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronFields {
    pub minute: Vec<u32>,
    pub hour: Vec<u32>,
    pub day_of_month: Vec<u32>,
    pub month: Vec<u32>,
    pub day_of_week: Vec<u32>,
}

struct FieldRange {
    min: u32,
    max: u32,
}

const FIELD_RANGES: [FieldRange; 5] = [
    FieldRange { min: 0, max: 59 }, // minute
    FieldRange { min: 0, max: 23 }, // hour
    FieldRange { min: 1, max: 31 }, // day_of_month
    FieldRange { min: 1, max: 12 }, // month
    FieldRange { min: 0, max: 6 },  // day_of_week (0=Sunday; 7 accepted as Sunday alias)
];

/// 展开单个 cron 字段为匹配值的有序数组
fn expand_field(field: &str, range: &FieldRange) -> Option<Vec<u32>> {
    let mut out = std::collections::BTreeSet::new();
    let FieldRange { min, max } = *range;

    for part in field.split(',') {
        if let Some(rest) = part.strip_prefix('*') {
            // 通配符 或 */N
            let step = if let Some(s) = rest.strip_prefix('/') {
                s.parse::<u32>().ok().filter(|&v| v >= 1)?
            } else if rest.is_empty() {
                1
            } else {
                return None;
            };
            let mut i = min;
            while i <= max {
                let _ = out.insert(i);
                i += step;
            }
        } else if part.contains('-') {
            // N-M 或 N-M/S
            let (range_part, step) = if let Some((r, s)) = part.split_once('/') {
                (r, s.parse::<u32>().ok().filter(|&v| v >= 1)?)
            } else {
                (part, 1)
            };
            let (lo_str, hi_str) = range_part.split_once('-')?;
            let lo = lo_str.parse::<u32>().ok()?;
            let hi = hi_str.parse::<u32>().ok()?;
            let is_dow = min == 0 && max == 6;
            let eff_max = if is_dow { 7 } else { max };
            if lo > hi || lo < min || hi > eff_max {
                return None;
            }
            let mut i = lo;
            while i <= hi {
                let val = if is_dow && i == 7 { 0 } else { i };
                let _ = out.insert(val);
                i += step;
            }
        } else {
            // 普通数字 N
            let mut n = part.parse::<u32>().ok()?;
            // dayOfWeek: 7 → 0 (Sunday alias)
            if min == 0 && max == 6 && n == 7 {
                n = 0;
            }
            if n < min || n > max {
                return None;
            }
            let _ = out.insert(n);
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out.into_iter().collect())
    }
}

/// 解析 5-field cron 表达式
///
/// # Errors
/// 无效或不支持的语法返回 None
#[must_use]
pub fn parse_cron(expr: &str) -> Option<CronFields> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }

    let minute = expand_field(parts[0], &FIELD_RANGES[0])?;
    let hour = expand_field(parts[1], &FIELD_RANGES[1])?;
    let day_of_month = expand_field(parts[2], &FIELD_RANGES[2])?;
    let month = expand_field(parts[3], &FIELD_RANGES[3])?;
    let day_of_week = expand_field(parts[4], &FIELD_RANGES[4])?;

    Some(CronFields {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
    })
}

/// 计算严格在 `after` 之后的下一个匹配时间点（本地时间语义）。
///
/// 标准 cron 语义：当 `day_of_month` 和 `day_of_week` 都有约束（非全集）时，
/// 日期匹配为 OR 关系（任一匹配即可）。
///
/// 最多搜索 366*24*60 分钟（1 年），无匹配返回 None。
#[must_use]
pub fn next_fire_time(fields: &CronFields, after: NaiveDateTime) -> Option<NaiveDateTime> {
    let minute_set: std::collections::HashSet<u32> = fields.minute.iter().copied().collect();
    let hour_set: std::collections::HashSet<u32> = fields.hour.iter().copied().collect();
    let dom_set: std::collections::HashSet<u32> = fields.day_of_month.iter().copied().collect();
    let month_set: std::collections::HashSet<u32> = fields.month.iter().copied().collect();
    let dow_set: std::collections::HashSet<u32> = fields.day_of_week.iter().copied().collect();

    let dom_wild = fields.day_of_month.len() == 31;
    let weekday_wild = fields.day_of_week.len() == 7;

    // Round up to the next whole minute (strictly after `after`)
    let mut t = after.with_second(0)?.with_nanosecond(0)?;
    t += chrono::Duration::minutes(1);

    let max_iter = 366 * 24 * 60;
    for _ in 0..max_iter {
        let month = t.month();
        if !month_set.contains(&month) {
            // Jump to start of next month
            let next_month = if month == 12 {
                NaiveDate::from_ymd_opt(t.year() + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(t.year(), month + 1, 1)?
            };
            t = next_month.and_hms_opt(0, 0, 0)?;
            continue;
        }

        let dom = t.day();
        let dow = t.weekday().num_days_from_sunday(); // 0=Sunday
        let day_matches = if dom_wild && weekday_wild {
            true
        } else if dom_wild {
            dow_set.contains(&dow)
        } else if weekday_wild {
            dom_set.contains(&dom)
        } else {
            dom_set.contains(&dom) || dow_set.contains(&dow)
        };

        if !day_matches {
            // Jump to start of next day
            let next_day = t.date().succ_opt()?;
            t = next_day.and_hms_opt(0, 0, 0)?;
            continue;
        }

        if !hour_set.contains(&t.hour()) {
            t = t.with_minute(0)?.with_second(0)?;
            t += chrono::Duration::hours(1);
            continue;
        }

        if !minute_set.contains(&t.minute()) {
            t += chrono::Duration::minutes(1);
            continue;
        }

        return Some(t);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every_minute() {
        let f = parse_cron("* * * * *").expect("valid");
        assert_eq!(f.minute.len(), 60);
        assert_eq!(f.hour.len(), 24);
        assert_eq!(f.day_of_month.len(), 31);
        assert_eq!(f.month.len(), 12);
        assert_eq!(f.day_of_week.len(), 7);
    }

    #[test]
    fn parse_specific_time() {
        let f = parse_cron("30 9 * * 1-5").expect("valid");
        assert_eq!(f.minute, vec![30]);
        assert_eq!(f.hour, vec![9]);
        assert_eq!(f.day_of_week, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_step() {
        let f = parse_cron("*/15 * * * *").expect("valid");
        assert_eq!(f.minute, vec![0, 15, 30, 45]);
    }

    #[test]
    fn parse_range_with_step() {
        let f = parse_cron("0 9-17/2 * * *").expect("valid");
        assert_eq!(f.hour, vec![9, 11, 13, 15, 17]);
    }

    #[test]
    fn parse_list() {
        let f = parse_cron("0 9,12,18 * * *").expect("valid");
        assert_eq!(f.hour, vec![9, 12, 18]);
    }

    #[test]
    fn parse_sunday_alias_7() {
        let f = parse_cron("0 9 * * 7").expect("valid");
        assert_eq!(f.day_of_week, vec![0]); // 7 → 0
    }

    #[test]
    fn parse_invalid_expr() {
        assert!(parse_cron("invalid").is_none());
        assert!(parse_cron("* * * *").is_none()); // only 4 fields
        assert!(parse_cron("60 * * * *").is_none()); // minute out of range
    }

    #[test]
    fn next_fire_every_minute() {
        let f = parse_cron("* * * * *").expect("valid");
        let after = NaiveDate::from_ymd_opt(2026, 4, 12)
            .expect("valid date")
            .and_hms_opt(10, 30, 0)
            .expect("valid time");
        let next = next_fire_time(&f, after).expect("should find next");
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 31);
    }

    #[test]
    fn next_fire_specific_time() {
        let f = parse_cron("30 14 * * *").expect("valid");
        let after = NaiveDate::from_ymd_opt(2026, 4, 12)
            .expect("valid date")
            .and_hms_opt(14, 30, 0)
            .expect("valid time");
        let next = next_fire_time(&f, after).expect("should find next");
        // Should be tomorrow 14:30 since we're at exactly 14:30
        assert_eq!(next.day(), 13);
        assert_eq!(next.hour(), 14);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_fire_day_of_week() {
        let f = parse_cron("0 9 * * 1").expect("valid"); // Monday 9:00
        // 2026-04-12 is Sunday
        let after = NaiveDate::from_ymd_opt(2026, 4, 12)
            .expect("valid date")
            .and_hms_opt(10, 0, 0)
            .expect("valid time");
        let next = next_fire_time(&f, after).expect("should find next");
        assert_eq!(next.day(), 13); // Monday
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_fire_month_rollover() {
        let f = parse_cron("0 0 1 * *").expect("valid"); // 1st of month midnight
        let after = NaiveDate::from_ymd_opt(2026, 4, 15)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time");
        let next = next_fire_time(&f, after).expect("should find next");
        assert_eq!(next.month(), 5);
        assert_eq!(next.day(), 1);
    }
}
