use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
            && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
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
}
