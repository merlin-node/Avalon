//! 到期提醒与自动续期。这里只有纯函数：日期运算和「今天该对这台机器做什么」的判断；
//! 读写数据库和发通知在 admin.rs 的巡检里。全部按 hub 所在时区的日历日计算。
//!
//! 规则：
//! - 到期前 REMIND_DAYS 天起每天提醒一次，到期当天也提醒；
//! - 跨过到期日后，节点开着自动续期、周期不是一次性、且此刻在线，就按周期往后顺延；
//! - 不在线不顺延（机器可能已经退了），一次性账单不顺延；
//! - 过期了还在线、但没法自动续期的（手动续费或一次性），继续每天提醒。

use std::fmt;

/// 提前几天开始每天提醒。
pub(super) const REMIND_DAYS: i64 = 7;
/// hub 本地时间几点之后才动作，免得半夜吵人。
pub(super) const REMIND_HOUR: i64 = 9;

/// 后台下拉框的选项，键名与主题里的 CYCLES 一致，公开页才认得。
pub(super) const CYCLES: [(&str, &str); 7] = [
    ("monthly", "月付"),
    ("quarterly", "季付"),
    ("semiannual", "半年付"),
    ("yearly", "年付"),
    ("biennial", "两年付"),
    ("triennial", "三年付"),
    ("once", "一次性"),
];

pub(super) fn cycle_label(cycle: &str) -> &str {
    CYCLES.iter().find(|(key, _)| *key == cycle).map(|(_, label)| *label).unwrap_or(cycle)
}

/// 一个周期是几个月。一次性和未知的周期返回 None，也就是不会自动续期。
pub(super) fn cycle_months(cycle: &str) -> Option<u32> {
    match cycle {
        "monthly" => Some(1),
        "quarterly" => Some(3),
        "semiannual" => Some(6),
        "yearly" => Some(12),
        "biennial" => Some(24),
        "triennial" => Some(36),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Date {
    y: i64,
    m: u32,
    d: u32,
}

impl Date {
    /// `YYYY-MM-DD`，也就是后台日期框提交的格式。
    pub(super) fn parse(text: &str) -> Option<Date> {
        let mut parts = text.trim().splitn(3, '-');
        let y: i64 = parts.next()?.parse().ok()?;
        let m: u32 = parts.next()?.parse().ok()?;
        let d: u32 = parts.next()?.parse().ok()?;
        let valid = (1970..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m);
        valid.then_some(Date { y, m, d })
    }

    /// 距 1970-01-01 的天数（Howard Hinnant 的 days_from_civil）。两个日期相减就是相差几天。
    pub(super) fn number(self) -> i64 {
        let y = if self.m <= 2 { self.y - 1 } else { self.y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.m);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.d) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// 加若干个月。落在不存在的日子上（1 月 31 日加一个月）取当月最后一天，
    /// 跟阿里云的续费规则一致；像 Go 的 AddDate 那样的实现会溢出成 3 月 3 日。
    pub(super) fn add_months(self, months: u32) -> Date {
        let total = self.y * 12 + i64::from(self.m) - 1 + i64::from(months);
        let y = total.div_euclid(12);
        let m = total.rem_euclid(12) as u32 + 1;
        Date { y, m, d: self.d.min(days_in_month(y, m)) }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// 流量周期从哪天开始。今天 ≥ 本月重置日，就从本月重置日算起；否则从上月重置日算起。
/// 重置日超过当月天数就落到当月最后一天（设 31 号，2 月落到 28 或 29 号）。
pub(super) fn period_start(today: Date, reset_day: u32) -> Date {
    let reset = reset_day.clamp(1, 31);
    let this_month = Date { y: today.y, m: today.m, d: reset.min(days_in_month(today.y, today.m)) };
    if today.d >= this_month.d {
        return this_month;
    }
    let (y, m) = if today.m == 1 { (today.y - 1, 12) } else { (today.y, today.m - 1) };
    Date { y, m, d: reset.min(days_in_month(y, m)) }
}

#[derive(Debug, PartialEq)]
pub(super) enum Action {
    Nothing,
    /// 提醒一次。数字是还剩几天：0 是今天到期，负数是已经过期几天。
    Remind(i64),
    /// 把到期日顺延到这一天。
    Renew(Date),
}

pub(super) fn decide(expires: Date, today: Date, cycle: &str, auto: bool, online: bool) -> Action {
    let left = expires.number() - today.number();
    if left >= 0 {
        return if left <= REMIND_DAYS { Action::Remind(left) } else { Action::Nothing };
    }
    if auto && online {
        if let Some(months) = cycle_months(cycle) {
            // hub 停过一阵、断了好几个周期就一次补齐。每次都从原到期日往后算，
            // 这样补齐过程中 31 号不会被一路截成 28 号。
            for periods in 1..=1200 {
                let next = expires.add_months(months * periods);
                if next.number() >= today.number() {
                    return Action::Renew(next);
                }
            }
        }
    }
    // 过期了还在线，说明机器还在用，只是没法自动续：接着提醒。不在线就别打扰了。
    if online { Action::Remind(left) } else { Action::Nothing }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Date {
        Date::parse(text).unwrap()
    }

    #[test]
    fn calendar_math() {
        assert_eq!(date("1970-01-01").number(), 0);
        assert_eq!(date("2000-03-01").number(), 11017);
        assert_eq!(date("2024-01-31").add_months(1), date("2024-02-29"), "闰年月末");
        assert_eq!(date("2023-01-31").add_months(1), date("2023-02-28"), "平年月末");
        assert_eq!(date("2024-11-15").add_months(3), date("2025-02-15"), "跨年");
        assert!(Date::parse("2025-02-29").is_none(), "平年没有 2 月 29 日");
        assert!(Date::parse("").is_none());
        assert_eq!(date("2026-04-05").to_string(), "2026-04-05");
    }

    #[test]
    fn reminds_daily_in_the_last_week() {
        let expires = date("2026-10-01");
        assert_eq!(decide(expires, date("2026-09-23"), "monthly", true, true), Action::Nothing, "还有 8 天");
        assert_eq!(decide(expires, date("2026-09-24"), "monthly", true, true), Action::Remind(7));
        assert_eq!(decide(expires, date("2026-10-01"), "monthly", true, false), Action::Remind(0), "到期当天不管在不在线都提醒");
    }

    #[test]
    fn renews_only_when_online_and_periodic() {
        let expires = date("2026-10-01");
        let next_day = date("2026-10-02");
        assert_eq!(decide(expires, next_day, "monthly", true, true), Action::Renew(date("2026-11-01")));
        assert_eq!(decide(expires, next_day, "yearly", true, true), Action::Renew(date("2027-10-01")));
        assert_eq!(decide(expires, next_day, "monthly", true, false), Action::Nothing, "离线不续，也不打扰");
        assert_eq!(decide(expires, next_day, "once", true, true), Action::Remind(-1), "一次性不续，在线就继续提醒");
        assert_eq!(decide(expires, next_day, "monthly", false, true), Action::Remind(-1), "关了自动续期");
    }

    #[test]
    fn traffic_period_follows_reset_day() {
        assert_eq!(period_start(date("2026-09-25"), 1), date("2026-09-01"));
        assert_eq!(period_start(date("2026-09-25"), 28), date("2026-08-28"), "还没到本月重置日，从上月算");
        assert_eq!(period_start(date("2026-09-28"), 28), date("2026-09-28"), "重置日当天开始新周期");
        assert_eq!(period_start(date("2026-01-05"), 10), date("2025-12-10"), "跨年");
        assert_eq!(period_start(date("2026-03-01"), 31), date("2026-02-28"), "31 号在 2 月落到月底");
        assert_eq!(period_start(date("2026-02-28"), 31), date("2026-02-28"));
    }

    #[test]
    fn catches_up_missed_periods() {
        assert_eq!(
            decide(date("2026-01-31"), date("2026-04-15"), "monthly", true, true),
            Action::Renew(date("2026-04-30")),
            "一次补齐，而且从原到期日算，不会漂成 28 号"
        );
    }
}
