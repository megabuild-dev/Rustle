use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn rand_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Site/container-safe names: `^[a-z0-9]([a-z0-9-]{0,30}[a-z0-9])?$`.
pub fn valid_site_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return false;
    }
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !ok(b[0]) || !ok(b[b.len() - 1]) {
        return false;
    }
    b.iter().all(|&c| ok(c) || c == b'-')
}

/// Conservative hostname check (RFC 1123 labels, at least one dot, no IP literals).
pub fn valid_domain(s: &str) -> bool {
    if s.len() > 253 || !s.contains('.') {
        return false;
    }
    let labels: Vec<&str> = s.split('.').collect();
    if labels.iter().all(|l| l.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    labels.iter().all(|l| {
        let b = l.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
            && b.iter()
                .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    })
}

/// `KEY` names for environment variables.
pub fn valid_env_key(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && !b[0].is_ascii_digit()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Civil date from unix seconds (UTC): (year, month, day, hour, minute, weekday 0=Sun).
pub fn civil(ts: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (h, mi) = ((secs / 3600) as u32, ((secs % 3600) / 60) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let wd = ((days.rem_euclid(7) + 4) % 7) as u32; // 1970-01-01 was Thursday
    (y, m, d, h, mi, wd)
}

pub fn stamp(ts: i64) -> String {
    let (y, m, d, h, mi, _) = civil(ts);
    let s = ts.rem_euclid(60);
    format!("{y:04}{m:02}{d:02}-{h:02}{mi:02}{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_site_name("blog"));
        assert!(valid_site_name("a-b-1"));
        assert!(!valid_site_name("-a"));
        assert!(!valid_site_name("a-"));
        assert!(!valid_site_name("A"));
        assert!(!valid_site_name(""));
        assert!(!valid_site_name("a_b"));
        assert!(!valid_site_name(&"a".repeat(33)));
    }

    #[test]
    fn domains() {
        assert!(valid_domain("blog.example.com"));
        assert!(!valid_domain("localhost"));
        assert!(!valid_domain("1.2.3.4"));
        assert!(!valid_domain("a..b.com"));
        assert!(!valid_domain("-a.com"));
        assert!(!valid_domain("a.com/evil"));
        assert!(!valid_domain("Upper.com"));
    }

    #[test]
    fn env_keys() {
        assert!(valid_env_key("NODE_ENV"));
        assert!(!valid_env_key("1A"));
        assert!(!valid_env_key("A=B"));
        assert!(!valid_env_key(""));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 4));
        // 2026-11-01 13:38:10 UTC is a Sunday.
        let (y, m, d, h, mi, wd) = civil(1_793_540_290);
        assert_eq!((y, m, d, h, mi, wd), (2026, 11, 1, 13, 38, 0));
        assert_eq!(stamp(0), "19700101-000000");
    }

    #[test]
    fn clock_and_length_boundaries() {
        let t = now();
        assert!(t > 1_700_000_000, "{t}");
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((real - now()).abs() <= 1);
        assert!(valid_site_name(&"a".repeat(32)) && !valid_site_name(&"a".repeat(33)));
        assert!(
            valid_site_name("a")
                && valid_site_name("a-b")
                && !valid_site_name("-a")
                && !valid_site_name("a-")
        );
        // domains: 253 total, labels of at most 63, lowercase letters, digits and inner hyphens only
        let l63 = "a".repeat(63);
        assert!(
            valid_domain(&format!("{l63}.com"))
                && !valid_domain(&format!("{}.com", "a".repeat(64)))
        );
        let long = format!("{l63}.{l63}.{l63}.{}", "b".repeat(253 - 63 * 3 - 3));
        assert_eq!(long.len(), 253);
        assert!(valid_domain(&long));
        assert!(!valid_domain(&format!("{long}c")));
        for ok in ["a.com", "a-b.example.com", "a1.b2.io", "xn--abc.com"] {
            assert!(valid_domain(ok), "{ok}");
        }
        for bad in [
            "nodot", "A.com", "a_b.com", "a.com.", ".a.com", "a..com", "-a.com", "a-.com",
            "1.2.3.4", "1.2", "a b.com",
        ] {
            assert!(!valid_domain(bad), "{bad}");
        }
    }

    #[test]
    fn civil_matches_a_day_by_day_calendar() {
        let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let (mut y, mut m, mut d, mut wd) = (1970i64, 1u32, 1u32, 4u32);
        for day in 0..=(230 * 366) {
            let ts = day * 86_400;
            assert_eq!(civil(ts), (y, m, d, 0, 0, wd), "day {day}");
            assert_eq!(
                civil(ts + 86_399),
                (y, m, d, 23, 59, wd),
                "end of day {day}"
            );
            // advance
            let dim = match m {
                2 if leap(y) => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            wd = (wd + 1) % 7;
            if d < dim {
                d += 1;
            } else {
                d = 1;
                if m == 12 {
                    m = 1;
                    y += 1;
                } else {
                    m += 1;
                }
            }
        }
        // before 1970 as well
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59, 3));
        assert_eq!(stamp(1_793_540_290), "20261101-133810");
    }
}
