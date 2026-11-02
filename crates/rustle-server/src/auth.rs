//! Authentication: admin sessions (HS256 JWT in an HttpOnly cookie + CSRF header), per-IP login
//! backoff, deploy-token verification and a tiny fixed-window rate limiter.

use crate::app::AppState;
use crate::crypto::{self, b64url, b64url_decode, ct_eq, hmac_sha256, MasterKey};
use crate::error::{AppError, AppResult};
use crate::store::TokenRow;
use crate::util::now;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// `__Host-` pins the cookie to this exact host over HTTPS: a sibling subdomain (a tenant app) cannot plant one.
pub const COOKIE: &str = "__Host-rustle_session";
pub const SESSION_SECS: i64 = 12 * 3600;
const HEADER_B64: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"; // {"alg":"HS256","typ":"JWT"}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    pub tv: i64,
    pub sid: String,
}

pub struct AuthState {
    jwt_key: [u8; 32],
    csrf_key: [u8; 32],
    backoff: Mutex<HashMap<String, (u32, i64)>>,
    /// Argon2 allocates ~8 MiB per hash; never run two at once.
    pub hash_gate: tokio::sync::Semaphore,
    /// Bounds how many requests may be waiting for `hash_gate`.
    pub hash_queue: tokio::sync::Semaphore,
    hooks: RateLimiter,
}

impl AuthState {
    pub fn new(key: &MasterKey) -> Self {
        Self {
            jwt_key: key.derive("jwt"),
            csrf_key: key.derive("csrf"),
            backoff: Mutex::new(HashMap::new()),
            hash_gate: tokio::sync::Semaphore::new(1),
            hash_queue: tokio::sync::Semaphore::new(4),
            hooks: RateLimiter::default(),
        }
    }

    pub fn issue(&self, tv: i64, at: i64) -> (String, String) {
        let claims = Claims {
            sub: "admin".into(),
            iat: at,
            exp: at + SESSION_SECS,
            tv,
            sid: crate::util::rand_hex(8),
        };
        let payload = b64url(&serde_json::to_vec(&claims).expect("claims serialize"));
        let signing = format!("{HEADER_B64}.{payload}");
        let sig = b64url(&hmac_sha256(&self.jwt_key, signing.as_bytes()));
        (format!("{signing}.{sig}"), self.csrf_for(&claims.sid))
    }

    pub fn csrf_for(&self, sid: &str) -> String {
        hex::encode(hmac_sha256(&self.csrf_key, sid.as_bytes()))
    }

    pub fn verify(&self, token: &str, at: i64) -> Option<Claims> {
        let mut it = token.split('.');
        let (h, p, s) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() || h != HEADER_B64 {
            return None; // also rejects alg=none and any other header
        }
        let want = hmac_sha256(&self.jwt_key, format!("{h}.{p}").as_bytes());
        if !ct_eq(&want, &b64url_decode(s)?) {
            return None;
        }
        let claims: Claims = serde_json::from_slice(&b64url_decode(p)?).ok()?;
        (claims.exp > at && claims.sub == "admin").then_some(claims)
    }

    /// Seconds the caller must wait before another attempt (0 = allowed now).
    pub fn login_wait(&self, ip: &str, at: i64) -> i64 {
        let m = self.backoff.lock().unwrap_or_else(|p| p.into_inner());
        match m.get(ip) {
            Some((_, next)) if *next > at => *next - at,
            _ => 0,
        }
    }

    /// Records a failure and returns how many consecutive failures this client has.
    pub fn login_failed(&self, ip: &str, at: i64) -> u32 {
        let mut m = self.backoff.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= 1024 && !m.contains_key(ip) {
            m.retain(|_, (_, next)| *next > at);
            if m.len() >= 1024 {
                // Still full of live penalties: evict the entry closest to expiry instead of
                // forgetting everyone (a flood must not reset an abuser's own penalty).
                if let Some(k) = m
                    .iter()
                    .min_by_key(|(_, (_, next))| *next)
                    .map(|(k, _)| k.clone())
                {
                    m.remove(&k);
                }
            }
        }
        let e = m.entry(ip.to_string()).or_insert((0, 0));
        e.0 += 1;
        // three free attempts, then 2,4,8… seconds up to 15 minutes
        let delay = if e.0 <= 3 {
            0
        } else {
            (1i64 << (e.0 - 3).min(10)).min(900)
        };
        e.1 = at + delay;
        e.0
    }

    pub fn login_ok(&self, ip: &str) {
        self.backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(ip);
    }

    pub fn webhook_allowed(&self, token_id: &str, at: i64) -> bool {
        self.hooks.allow(token_id, 30, 60, at)
    }
}

#[derive(Default)]
pub struct RateLimiter {
    map: Mutex<HashMap<String, (i64, u32)>>,
}

impl RateLimiter {
    pub fn allow(&self, key: &str, limit: u32, window: i64, at: i64) -> bool {
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() > 512 {
            m.retain(|_, (start, _)| at - *start < window);
        }
        let e = m.entry(key.to_string()).or_insert((at, 0));
        if at - e.0 >= window {
            *e = (at, 0);
        }
        if e.1 >= limit {
            return false;
        }
        e.1 += 1;
        true
    }
}

/// Client address as seen by Caddy (last `X-Forwarded-For` hop); `local` when called directly.
pub fn client_ip(h: &HeaderMap) -> String {
    h.get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() < 64)
        .map(|s| normalize_ip(&s))
        .unwrap_or_else(|| "local".into())
}

/// IPv6 clients rotate freely inside their /64, so rate-limit the whole prefix.
pub fn normalize_ip(ip: &str) -> String {
    if !ip.contains(':') || ip.contains('.') {
        return ip.to_string(); // IPv4, or an IPv4-mapped form
    }
    let Ok(v6) = ip.parse::<std::net::Ipv6Addr>() else {
        return ip.to_string();
    };
    let s = v6.segments();
    format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
}

pub fn cookie_value(h: &HeaderMap, name: &str) -> Option<String> {
    let raw = h.get("cookie")?.to_str().ok()?;
    raw.split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

pub fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_SECS}")
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
}

/// Extractor for authenticated admin requests. Mutating methods also require the CSRF header.
pub struct Admin {
    pub claims: Claims,
    pub csrf: String,
}

impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = cookie_value(&parts.headers, COOKIE)
            .ok_or_else(|| AppError::unauthorized("not signed in"))?;
        let claims = state
            .auth
            .verify(&token, now())
            .ok_or_else(|| AppError::unauthorized("invalid or expired session"))?;
        let (_, _, tv) = state
            .store
            .admin()?
            .ok_or_else(|| AppError::unauthorized("not initialized"))?;
        if tv != claims.tv {
            return Err(AppError::unauthorized("session revoked"));
        }
        let csrf = state.auth.csrf_for(&claims.sid);
        if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let got = parts
                .headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !ct_eq(got.as_bytes(), csrf.as_bytes()) {
                return Err(AppError::forbidden("missing or invalid CSRF token"));
            }
        }
        Ok(Admin { claims, csrf })
    }
}

pub const TOKEN_PREFIX: &str = "rt_";

pub fn new_deploy_token() -> (String, String, String) {
    let raw = format!("{TOKEN_PREFIX}{}", crypto::random_token(32));
    let hash = crypto::sha256_hex(raw.as_bytes());
    let prefix = raw.chars().take(8).collect();
    (raw, prefix, hash)
}

/// Authenticate a `Authorization: Bearer rt_…` header against active deploy tokens.
pub fn deploy_token(state: &AppState, h: &HeaderMap) -> AppResult<TokenRow> {
    let raw = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| t.starts_with(TOKEN_PREFIX) && t.len() < 200)
        .ok_or_else(|| AppError::unauthorized("missing deploy token"))?;
    let hash = crypto::sha256_hex(raw.as_bytes());
    let row = state
        .store
        .find_active_token(&hash)?
        .ok_or_else(|| AppError::unauthorized("invalid deploy token"))?;
    let at = now();
    if !state.auth.webhook_allowed(&row.id, at) {
        return Err(AppError::too_many("deploy webhook rate limit exceeded"));
    }
    state.store.touch_token(&row.id, at)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> AuthState {
        AuthState::new(&MasterKey::from_bytes([3; 32]))
    }

    #[test]
    fn jwt_roundtrip_expiry_and_tamper() {
        let a = st();
        let (t, csrf) = a.issue(4, 1000);
        let c = a.verify(&t, 1001).unwrap();
        assert_eq!(c.tv, 4);
        assert_eq!(a.csrf_for(&c.sid), csrf);
        assert!(a.verify(&t, 1000 + SESSION_SECS).is_none(), "expired");
        // flip a payload char
        let mut parts: Vec<String> = t.split('.').map(String::from).collect();
        parts[1].push('A');
        assert!(a.verify(&parts.join("."), 1001).is_none());
        // other key
        let other = AuthState::new(&MasterKey::from_bytes([4; 32]));
        assert!(other.verify(&t, 1001).is_none());
        assert!(a.verify("a.b", 1).is_none());
        assert!(a.verify("", 1).is_none());
    }

    #[test]
    fn alg_none_is_rejected() {
        let a = st();
        let payload = b64url(br#"{"sub":"admin","iat":0,"exp":99999999999,"tv":0,"sid":"x"}"#);
        let none_hdr = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        assert!(a.verify(&format!("{none_hdr}.{payload}."), 1).is_none());
        assert!(a.verify(&format!("{none_hdr}.{payload}.x"), 1).is_none());
    }

    #[test]
    fn backoff_schedule_and_reset() {
        let a = st();
        for _ in 0..3 {
            assert_eq!(a.login_wait("1.2.3.4", 100), 0);
            a.login_failed("1.2.3.4", 100);
        }
        assert_eq!(a.login_wait("1.2.3.4", 100), 0, "three free attempts");
        a.login_failed("1.2.3.4", 100); // 4th => 2s
        assert_eq!(a.login_wait("1.2.3.4", 100), 2);
        assert_eq!(a.login_wait("1.2.3.4", 102), 0);
        assert_eq!(
            a.login_wait("9.9.9.9", 100),
            0,
            "per-IP, never account-wide"
        );
        for _ in 0..30 {
            a.login_failed("1.2.3.4", 100);
        }
        assert_eq!(a.login_wait("1.2.3.4", 100), 900, "capped at 15 minutes");
        // a flood of distinct addresses cannot grow the table or reset an abuser's penalty
        for i in 0..2000 {
            a.login_failed(&format!("10.1.{}.{}", i / 250, i % 250), 100);
        }
        assert!(a.backoff.lock().unwrap().len() <= 1024);
        assert_eq!(
            a.login_wait("1.2.3.4", 100),
            900,
            "the abuser is still penalized after the flood"
        );
        a.login_ok("1.2.3.4");
        assert_eq!(a.login_wait("1.2.3.4", 100), 0);
    }

    #[test]
    fn rate_limiter_window() {
        let r = RateLimiter::default();
        for _ in 0..3 {
            assert!(r.allow("k", 3, 60, 10));
        }
        assert!(!r.allow("k", 3, 60, 10));
        assert!(r.allow("other", 3, 60, 10));
        assert!(r.allow("k", 3, 60, 71), "new window");
    }

    #[test]
    fn xff_uses_last_hop() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), "local");
        h.insert("x-forwarded-for", "6.6.6.6, 203.0.113.5".parse().unwrap());
        assert_eq!(
            client_ip(&h),
            "203.0.113.5",
            "client-supplied prefix is ignored"
        );
        h.insert(
            "x-forwarded-for",
            "2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap(),
        );
        assert_eq!(
            client_ip(&h),
            "2001:db8:1:2::/64",
            "IPv6 is limited per /64"
        );
        assert_eq!(
            normalize_ip("2001:db8:1:2:1::1"),
            normalize_ip("2001:db8:1:2:ffff::2")
        );
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            "a=1; __Host-rustle_session=tok; b=2".parse().unwrap(),
        );
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("tok"));
        assert!(cookie_value(&h, "zzz").is_none());
        let c = session_cookie("t");
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Secure"));
    }

    #[test]
    fn deploy_tokens_are_prefixed_and_hash_matches() {
        let (raw, prefix, hash) = new_deploy_token();
        assert!(raw.starts_with("rt_") && raw.len() > 40);
        assert!(raw.starts_with(&prefix));
        assert_eq!(hash, crypto::sha256_hex(raw.as_bytes()));
        assert_ne!(new_deploy_token().0, raw);
    }

    #[test]
    fn backoff_delays_are_exact() {
        // 3 free attempts, then 2, 4, 8 ... seconds, capped at 900
        let expected = [0, 0, 0, 2, 4, 8, 16, 32, 64, 128, 256, 512, 900, 900, 900];
        let a = st();
        for (i, want) in expected.iter().enumerate() {
            let n = a.login_failed("ip", 1000);
            assert_eq!(n as usize, i + 1, "failure counter");
            assert_eq!(a.login_wait("ip", 1000), *want, "after failure {}", i + 1);
        }
        // the wait shrinks as time passes and never goes negative
        assert_eq!(a.login_wait("ip", 1000 + 899), 1);
        assert_eq!(a.login_wait("ip", 1000 + 900), 0);
        assert_eq!(a.login_wait("ip", 5000), 0);
    }

    #[test]
    fn full_backoff_table_evicts_the_entry_closest_to_expiry() {
        let a = st();
        for i in 0..1024 {
            for _ in 0..5 {
                a.login_failed(&format!("ip{i}"), 1000 + i); // all still penalized at t=1000
            }
        }
        assert_eq!(a.backoff.lock().unwrap().len(), 1024);
        a.login_failed("newcomer", 1000);
        let m = a.backoff.lock().unwrap();
        assert_eq!(m.len(), 1024, "bounded");
        assert!(!m.contains_key("ip0"), "earliest expiry evicted");
        assert!(m.contains_key("ip1") && m.contains_key("ip1023") && m.contains_key("newcomer"));
        drop(m);
        // a known address is never evicted to make room for itself
        a.login_failed("ip1", 1000);
        assert_eq!(a.backoff.lock().unwrap().len(), 1024);
        // when everything has expired the table is purged first
        a.login_failed("late", 100_000);
        assert_eq!(a.backoff.lock().unwrap().len(), 1);
    }

    #[test]
    fn rate_limiter_boundaries_and_pruning() {
        let r = RateLimiter::default();
        assert!(r.allow("k", 1, 10, 100));
        assert!(!r.allow("k", 1, 10, 109), "still inside the window");
        assert!(
            r.allow("k", 1, 10, 110),
            "window is exactly `window` seconds"
        );
        assert!(!r.allow("k", 0, 10, 110), "limit 0 allows nothing");
        // stale windows are dropped once the table is large
        let r = RateLimiter::default();
        for i in 0..513 {
            r.allow(&format!("k{i}"), 5, 10, 0);
        }
        assert_eq!(r.map.lock().unwrap().len(), 513);
        r.allow("fresh", 5, 10, 5); // 513 entries but nobody is stale yet at t=5
        assert_eq!(r.map.lock().unwrap().len(), 514);
        r.allow("later", 5, 10, 50); // every k* window ended
        assert_eq!(
            r.map.lock().unwrap().len(),
            1,
            "all stale windows pruned; only `later` remains"
        );
    }

    #[test]
    fn client_ip_length_and_emptiness_rules() {
        let ip = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-forwarded-for", v.parse().unwrap());
            client_ip(&h)
        };
        assert_eq!(ip(" "), "local");
        assert_eq!(ip("1.1.1.1, "), "local", "empty last hop");
        assert_eq!(ip(&"a".repeat(63)), "a".repeat(63));
        assert_eq!(ip(&"a".repeat(64)), "local");
        assert_eq!(ip("  7.7.7.7  "), "7.7.7.7");
    }

    #[tokio::test]
    async fn deploy_token_header_rules() {
        use crate::app::TestEnv;
        let e = TestEnv::new();
        let site = crate::sites::create_site(
            &e.state,
            rustle_types::CreateSiteReq {
                name: "a".into(),
                domain: "a.example.com".into(),
                image_repo: "ghcr.io/o/a".into(),
                ram_limit_mb: 128,
                container_port: 3000,
                registry_id: None,
                env: vec![],
                skip_dns_check: true,
                override_budget: true,
            },
        )
        .await
        .unwrap();
        let tok = crate::sites::create_token(&e.state, &site.id).unwrap();
        let hdr = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert("authorization", v.parse().unwrap());
            h
        };
        assert!(deploy_token(&e.state, &hdr(&format!("Bearer {}", tok.token))).is_ok());
        assert!(
            deploy_token(&e.state, &hdr(&format!("Bearer  {} ", tok.token))).is_ok(),
            "trimmed"
        );
        // right length, wrong prefix
        assert!(deploy_token(&e.state, &hdr(&format!("Bearer xx_{}", &tok.token[3..]))).is_err());
        // right prefix, absurd length
        let long = format!("Bearer rt_{}", "a".repeat(200));
        assert!(deploy_token(&e.state, &hdr(&long)).is_err());
        assert!(deploy_token(&e.state, &hdr(&format!("Basic {}", tok.token))).is_err());
        assert!(deploy_token(&e.state, &HeaderMap::new()).is_err());
        // unknown token with the right shape
        assert!(deploy_token(&e.state, &hdr("Bearer rt_unknown")).is_err());
        // revoked tokens stop working
        crate::sites::revoke_token(&e.state, &site.id, &tok.id).unwrap();
        assert!(deploy_token(&e.state, &hdr(&format!("Bearer {}", tok.token))).is_err());
    }

    #[test]
    fn session_claims_and_csrf_are_exact() {
        assert_eq!(SESSION_SECS, 43_200);
        let a = st();
        let (t, csrf) = a.issue(7, 5000);
        let c = a.verify(&t, 5000).unwrap();
        assert_eq!(
            (c.iat, c.exp, c.tv, c.sub.as_str()),
            (5000, 5000 + 43_200, 7, "admin")
        );
        assert!(a.verify(&t, 5000 + 43_199).is_some() && a.verify(&t, 5000 + 43_200).is_none());
        assert_eq!(csrf.len(), 64);
        assert!(csrf.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a.csrf_for(&c.sid), csrf, "deterministic per session");
        assert_ne!(a.csrf_for("other-session"), csrf);
        let (t2, csrf2) = a.issue(7, 5000);
        assert_ne!(t, t2, "every login gets a fresh session id");
        assert_ne!(csrf, csrf2);
        let b = AuthState::new(&MasterKey::from_bytes([9; 32]));
        assert_ne!(b.csrf_for(&c.sid), csrf, "key-dependent");
    }

    #[test]
    fn verify_rejects_malformed_shapes() {
        let a = st();
        let (t, _) = a.issue(1, 100);
        assert!(a.verify(&t, 101).is_some());
        assert!(
            a.verify(&format!("{t}.extra"), 101).is_none(),
            "four segments"
        );
        let parts: Vec<&str> = t.split('.').collect();
        assert!(
            a.verify(&format!("{}.{}", parts[0], parts[1]), 101)
                .is_none(),
            "two segments"
        );
        let other_hdr = b64url(br#"{"alg":"HS256","typ":"JWS"}"#);
        assert!(a
            .verify(&format!("{other_hdr}.{}.{}", parts[1], parts[2]), 101)
            .is_none());
        // a validly signed token for a different subject is not an admin session
        let claims = Claims {
            sub: "user".into(),
            iat: 1,
            exp: 10_000,
            tv: 1,
            sid: "x".into(),
        };
        let payload = b64url(&serde_json::to_vec(&claims).unwrap());
        let signing = format!("{HEADER_B64}.{payload}");
        let sig = b64url(&hmac_sha256(&a.jwt_key, signing.as_bytes()));
        assert!(a.verify(&format!("{signing}.{sig}"), 5).is_none());
    }
}
