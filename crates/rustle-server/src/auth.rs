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
}
