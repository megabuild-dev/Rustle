//! Shared application state and the small trait seams (health, DNS) used by services.

use crate::auth::AuthState;
use crate::caddy::Gateway;
use crate::config::Config;
use crate::crypto::MasterKey;
use crate::deploy::Deployer;
use crate::docker::Runtime;
use crate::github::Github;
use crate::httpc::{self, Req, Target};
use crate::metrics::Sampler;
use crate::store::Store;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[async_trait]
pub trait Health: Send + Sync {
    /// One probe of the app on the container network. `true` = serving (HTTP status < 500).
    async fn probe(&self, ip: &str, port: u16) -> bool;
}

pub struct HttpHealth;

#[async_trait]
impl Health for HttpHealth {
    async fn probe(&self, ip: &str, port: u16) -> bool {
        let t = Target::Tcp(format!("{ip}:{port}"));
        match httpc::send(&t, &Req::get("/"), Duration::from_secs(4)).await {
            Ok(r) => r.status < 500,
            Err(_) => false,
        }
    }
}

/// Scripted health for tests: pops results; when empty returns `default`.
pub struct FakeHealth {
    script: Mutex<Vec<bool>>,
    pub default: AtomicBool,
}

impl FakeHealth {
    pub fn new(default: bool) -> Self {
        Self {
            script: Mutex::new(vec![]),
            default: AtomicBool::new(default),
        }
    }
    pub fn script(&self, v: Vec<bool>) {
        let mut s = self.script.lock().unwrap();
        *s = v;
        s.reverse();
    }
}

#[async_trait]
impl Health for FakeHealth {
    async fn probe(&self, _ip: &str, _port: u16) -> bool {
        match self.script.lock().unwrap().pop() {
            Some(v) => v,
            None => self.default.load(std::sync::atomic::Ordering::SeqCst),
        }
    }
}

#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str) -> Vec<IpAddr>;
}

pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        match tokio::net::lookup_host((host, 80)).await {
            Ok(it) => it.map(|a| a.ip()).collect(),
            Err(_) => vec![],
        }
    }
}

#[derive(Default)]
pub struct FakeResolver {
    map: Mutex<HashMap<String, Vec<IpAddr>>>,
}

impl FakeResolver {
    pub fn set(&self, host: &str, ips: &[&str]) {
        self.map.lock().unwrap().insert(
            host.into(),
            ips.iter().map(|s| s.parse().unwrap()).collect(),
        );
    }
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        self.map
            .lock()
            .unwrap()
            .get(host)
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Tuning {
    pub health_attempts: u32,
    pub health_interval: Duration,
    pub stop_timeout_secs: u32,
    pub oneshot_timeout: Duration,
    /// Whole-request limit for HTTP cron jobs.
    pub http_job_timeout: Duration,
    /// How long a self-update waits for running deployments to finish.
    pub update_idle_wait: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            health_attempts: 45,
            health_interval: Duration::from_secs(2),
            stop_timeout_secs: 10,
            oneshot_timeout: Duration::from_secs(600),
            http_job_timeout: Duration::from_secs(60),
            update_idle_wait: Duration::from_secs(120),
        }
    }
}

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    pub key: MasterKey,
    pub runtime: Arc<dyn Runtime>,
    pub gateway: Arc<dyn Gateway>,
    pub health: Arc<dyn Health>,
    pub resolver: Arc<dyn Resolver>,
    pub github: Arc<dyn Github>,
    pub auth: AuthState,
    pub deployer: Deployer,
    pub sampler: Mutex<Sampler>,
    pub stats_subscribers: AtomicUsize,
    pub degraded: Mutex<Option<String>>,
    /// Set while a self-update is being applied; new deployments are refused.
    pub maintenance: AtomicBool,
    pub tuning: Tuning,
    pub running_jobs: Mutex<HashSet<String>>,
}

impl AppState {
    pub fn new(
        cfg: Config,
        store: Store,
        key: MasterKey,
        runtime: Arc<dyn Runtime>,
        gateway: Arc<dyn Gateway>,
        github: Arc<dyn Github>,
    ) -> Arc<AppState> {
        Self::with_backends(
            cfg,
            store,
            key,
            runtime,
            gateway,
            Arc::new(HttpHealth),
            Arc::new(SystemResolver),
            github,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_backends(
        cfg: Config,
        store: Store,
        key: MasterKey,
        runtime: Arc<dyn Runtime>,
        gateway: Arc<dyn Gateway>,
        health: Arc<dyn Health>,
        resolver: Arc<dyn Resolver>,
        github: Arc<dyn Github>,
    ) -> Arc<AppState> {
        let auth = AuthState::new(&key);
        Arc::new(AppState {
            cfg,
            store,
            key,
            runtime,
            gateway,
            health,
            resolver,
            github,
            auth,
            deployer: Deployer::default(),
            sampler: Mutex::new(Sampler::default()),
            stats_subscribers: AtomicUsize::new(0),
            degraded: Mutex::new(None),
            maintenance: AtomicBool::new(false),
            tuning: Tuning::default(),
            running_jobs: Mutex::new(HashSet::new()),
        })
    }

    pub fn set_degraded(&self, reason: Option<String>) {
        *self.degraded.lock().unwrap_or_else(|p| p.into_inner()) = reason;
    }

    pub fn degraded(&self) -> Option<String> {
        self.degraded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Everything a test needs: the state plus typed handles to every fake.
#[cfg(test)]
pub struct TestEnv {
    pub state: Arc<AppState>,
    pub runtime: Arc<crate::docker::FakeRuntime>,
    pub gateway: Arc<crate::caddy::FakeGateway>,
    pub health: Arc<FakeHealth>,
    pub resolver: Arc<FakeResolver>,
    pub github: Arc<crate::github::FakeGithub>,
    pub dir: tempfile::TempDir,
}

#[cfg(test)]
#[allow(clippy::new_without_default)]
impl TestEnv {
    pub fn new() -> TestEnv {
        Self::build(false)
    }

    /// Rustle sharing a Caddy that already exists on the host.
    pub fn external() -> TestEnv {
        Self::build_with(false, true, false)
    }

    /// Rustle sharing a Caddy that runs as a container attached to `rustle-net` (resolves names).
    pub fn external_names() -> TestEnv {
        Self::build_with(false, true, true)
    }

    pub fn build(file_db: bool) -> TestEnv {
        Self::build_with(file_db, false, false)
    }

    fn build_with(file_db: bool, external: bool, names: bool) -> TestEnv {
        use crate::docker::FakeRuntime;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests(dir.path());
        cfg.gateway_external = external;
        cfg.gateway_names = names;
        let cfg_db = cfg.db_path.clone();
        let runtime = Arc::new(FakeRuntime::new());
        let gateway = Arc::new(crate::caddy::FakeGateway::new());
        let health = Arc::new(FakeHealth::new(true));
        let resolver = Arc::new(FakeResolver::default());
        let github = Arc::new(crate::github::FakeGithub::new());
        let key = MasterKey::from_bytes([9; 32]);
        let auth = AuthState::new(&key);
        let state = Arc::new(AppState {
            cfg,
            store: if file_db {
                Store::open(&cfg_db).unwrap()
            } else {
                Store::open_memory().unwrap()
            },
            key,
            runtime: runtime.clone(),
            gateway: gateway.clone(),
            health: health.clone(),
            resolver: resolver.clone(),
            github: github.clone(),
            auth,
            deployer: Deployer::default(),
            sampler: Mutex::new(Sampler::default()),
            stats_subscribers: AtomicUsize::new(0),
            degraded: Mutex::new(None),
            maintenance: AtomicBool::new(false),
            tuning: Tuning {
                health_attempts: 3,
                health_interval: Duration::from_millis(1),
                stop_timeout_secs: 1,
                oneshot_timeout: Duration::from_millis(200),
                http_job_timeout: Duration::from_secs(2),
                update_idle_wait: Duration::from_secs(1),
            },
            running_jobs: Mutex::new(HashSet::new()),
        });
        // A realistic host: 1 GiB RAM, 700 MiB available.
        let proc = state.cfg.proc_root.clone();
        std::fs::create_dir_all(&proc).unwrap();
        std::fs::write(
            proc.join("meminfo"),
            "MemTotal: 1015808 kB\nMemFree: 300000 kB\nMemAvailable: 700000 kB\nSwapTotal: 1572860 kB\nSwapFree: 1572860 kB\n",
        )
        .unwrap();
        TestEnv {
            state,
            runtime,
            gateway,
            health,
            resolver,
            github,
            dir,
        }
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::testutil::MockHttp;

    #[tokio::test]
    async fn http_health_counts_anything_below_500_as_serving() {
        for (code, want) in [
            (200u16, true),
            (301, true),
            (404, true),
            (499, true),
            (500, false),
            (503, false),
        ] {
            let m = MockHttp::start(move |_| (code, vec![], String::new())).await;
            let (ip, port) = m.addr.rsplit_once(':').unwrap();
            assert_eq!(
                HttpHealth.probe(ip, port.parse().unwrap()).await,
                want,
                "{code}"
            );
            assert_eq!(m.requests()[0].path, "/");
        }
        assert!(!HttpHealth.probe("127.0.0.1", 1).await, "nothing listening");
    }

    #[tokio::test]
    async fn fake_health_pops_the_script_then_uses_the_default() {
        let h = FakeHealth::new(true);
        h.script(vec![false, false, true]);
        assert!(!h.probe("x", 1).await);
        assert!(!h.probe("x", 1).await);
        assert!(h.probe("x", 1).await);
        assert!(h.probe("x", 1).await, "script exhausted: default");
        h.default.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!h.probe("x", 1).await);
    }

    #[tokio::test]
    async fn system_resolver_resolves_localhost_and_survives_failures() {
        let ips = SystemResolver.resolve("localhost").await;
        assert!(ips.iter().any(|ip| ip.is_loopback()), "{ips:?}");
        assert!(SystemResolver
            .resolve("does-not-exist.invalid")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn fake_resolver_returns_what_it_was_told() {
        let r = FakeResolver::default();
        assert!(r.resolve("a.example.com").await.is_empty());
        r.set("a.example.com", &["1.2.3.4", "::1"]);
        assert_eq!(r.resolve("a.example.com").await.len(), 2);
    }
}
