//! Minimal 5-field cron expression matcher (UTC). Supports `*`, lists, ranges and steps.
//! Day-of-month and day-of-week follow the classic OR rule when both are restricted.

use crate::util::civil;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64, // bit per minute 0..59
    hours: u32,   // 0..23
    dom: u32,     // 1..31
    months: u16,  // 1..12
    dow: u8,      // 0..6 (Sunday = 0)
    dom_star: bool,
    dow_star: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid cron expression: {0}")]
pub struct CronError(pub String);

fn field(spec: &str, min: u32, max: u32, allow_7: bool) -> Result<(u64, bool), CronError> {
    let err = |m: &str| CronError(format!("{m} in '{spec}'"));
    let mut bits = 0u64;
    let mut star = false;
    for part in spec.split(',') {
        if part.is_empty() {
            return Err(err("empty list item"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| err("bad step"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(err("zero step"));
        }
        let (lo, hi) = if range == "*" {
            if part == "*" {
                star = true;
            }
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse().map_err(|_| err("bad number"))?,
                b.parse().map_err(|_| err("bad number"))?,
            )
        } else {
            let v: u32 = range.parse().map_err(|_| err("bad number"))?;
            // `5/2` means 5-max step 2
            if part.contains('/') {
                (v, max)
            } else {
                (v, v)
            }
        };
        let top = if allow_7 { max + 1 } else { max };
        if lo < min || hi > top || lo > hi {
            return Err(err("out of range"));
        }
        let mut v = lo;
        while v <= hi {
            let vv = if allow_7 && v == 7 { 0 } else { v };
            bits |= 1u64 << vv;
            v += step;
        }
    }
    Ok((bits, star))
}

impl CronExpr {
    pub fn parse(s: &str) -> Result<CronExpr, CronError> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return Err(CronError(format!("expected 5 fields, got {}", f.len())));
        }
        let (minutes, _) = field(f[0], 0, 59, false)?;
        let (hours, _) = field(f[1], 0, 23, false)?;
        let (dom, dom_star) = field(f[2], 1, 31, false)?;
        let (months, _) = field(f[3], 1, 12, false)?;
        let (dow, dow_star) = field(f[4], 0, 6, true)?;
        Ok(CronExpr {
            minutes,
            hours: hours as u32,
            dom: dom as u32,
            months: months as u16,
            dow: dow as u8,
            dom_star,
            dow_star,
        })
    }

    /// Does the expression fire in the minute containing unix time `ts`?
    pub fn matches(&self, ts: i64) -> bool {
        let (_, mo, d, h, mi, wd) = civil(ts);
        if self.minutes & (1 << mi) == 0
            || self.hours & (1 << h) == 0
            || self.months & (1 << mo) == 0
        {
            return false;
        }
        let dom_ok = self.dom & (1 << d) != 0;
        let dow_ok = self.dow & (1 << wd) != 0;
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-01 (Sunday) 02:00 UTC
    const SUN_0200: i64 = 1_793_498_400;

    #[test]
    fn reference_timestamp_is_what_we_think() {
        let (y, m, d, h, mi, wd) = civil(SUN_0200);
        assert_eq!((y, m, d, h, mi, wd), (2026, 11, 1, 2, 0, 0));
    }

    #[test]
    fn weekly_prune() {
        let e = CronExpr::parse("0 2 * * 0").unwrap();
        assert!(e.matches(SUN_0200));
        assert!(!e.matches(SUN_0200 + 60));
        assert!(!e.matches(SUN_0200 + 86_400)); // Monday
        assert!(e.matches(SUN_0200 + 7 * 86_400));
        // 7 also means Sunday
        assert!(CronExpr::parse("0 2 * * 7").unwrap().matches(SUN_0200));
    }

    #[test]
    fn steps_ranges_lists() {
        let e = CronExpr::parse("*/15 8-10 * * *").unwrap();
        let base = SUN_0200 + 6 * 3600; // 08:00
        assert!(e.matches(base));
        assert!(e.matches(base + 15 * 60));
        assert!(!e.matches(base + 14 * 60));
        assert!(e.matches(base + 2 * 3600 + 45 * 60)); // 10:45
        assert!(!e.matches(base + 3 * 3600)); // 11:00
        let e = CronExpr::parse("5,10 * * * *").unwrap();
        assert!(
            e.matches(SUN_0200 + 5 * 60)
                && e.matches(SUN_0200 + 10 * 60)
                && !e.matches(SUN_0200 + 6 * 60)
        );
        let e = CronExpr::parse("10/20 * * * *").unwrap(); // 10, 30, 50
        assert!(e.matches(SUN_0200 + 30 * 60) && !e.matches(SUN_0200 + 20 * 60));
    }

    #[test]
    fn dom_dow_or_rule() {
        // fires on the 1st OR on Mondays
        let e = CronExpr::parse("0 2 1 * 1").unwrap();
        assert!(e.matches(SUN_0200)); // Nov 1 (a Sunday, but day-of-month matches)
        assert!(e.matches(SUN_0200 + 86_400)); // Monday Nov 2
        assert!(!e.matches(SUN_0200 + 2 * 86_400));
    }

    #[test]
    fn month_field() {
        let e = CronExpr::parse("0 2 1 12 *").unwrap();
        assert!(!e.matches(SUN_0200));
        assert!(e.matches(SUN_0200 + 30 * 86_400)); // Dec 1
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "a * * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            ",1 * * * *",
            "1,,2 * * * *",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "should reject {bad:?}");
        }
    }
}
