//! Process wiring: listeners, background loops and graceful shutdown.

use crate::app::{AppState, FakeHealth, FakeResolver};
use crate::caddy::{FakeGateway, Gateway, HttpGateway};
use crate::config::Config;
use crate::crypto::MasterKey;
use crate::docker::{FakeRuntime, HttpRuntime, Runtime};
use crate::github::{FakeGithub, Github, HttpGithub};
use crate::httpc::Target;
use crate::store::Store;
use crate::{api, jobs, reconcile, util};
use std::sync::Arc;
use std::time::Duration;

pub fn build_state(cfg: Config, key: MasterKey, store: Store) -> Arc<AppState> {
    if cfg.fake_backends {
        tracing::warn!("running with FAKE Docker/Caddy/GitHub backends (dev-fakes)");
        let gateway: Arc<dyn Gateway> = if cfg.real_gateway {
            Arc::new(HttpGateway::with_options(
                Target::from(&cfg.caddy_admin),
                cfg.gateway_external,
                cfg.caddy_server.clone(),
            ))
        } else {
            Arc::new(FakeGateway::new())
        };
        let runtime = Arc::new(FakeRuntime::with_fixed_ip(cfg.fake_container_ip.clone()));
        return AppState::with_backends(
            cfg,
            store,
            key,
            runtime,
            gateway,
            Arc::new(FakeHealth::new(true)),
            Arc::new(FakeResolver::default()),
            Arc::new(FakeGithub::new()),
        );
    }
    let runtime: Arc<dyn Runtime> = Arc::new(HttpRuntime::new(
        Target::from(&cfg.docker),
        cfg.network.clone(),
    ));
    let gateway: Arc<dyn Gateway> = Arc::new(HttpGateway::with_options(
        Target::from(&cfg.caddy_admin),
        cfg.gateway_external,
        cfg.caddy_server.clone(),
    ));
    let github: Arc<dyn Github> = Arc::new(HttpGithub::new(cfg.github_api.clone()));
    AppState::new(cfg, store, key, runtime, gateway, github)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutdown requested");
}

pub fn log_level(v: &str) -> tracing::Level {
    match v.to_ascii_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

/// Refuse to start without a key, an initialized database, or with a key that does not belong to it.
pub fn open_checked(cfg: &Config) -> Result<(MasterKey, Store), String> {
    let key = MasterKey::load(&cfg.master_key_file, false)
        .map_err(|e| format!("{e}. Run `rustle init` first."))?;
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    if store.admin().map_err(|e| e.message)?.is_none() {
        return Err("not initialized: run `rustle init` first".into());
    }
    match store.setting("key_check").map_err(|e| e.message)? {
        Some(c) if c != key.check_value() => {
            return Err(format!(
                "the master key in {} does not match this database; refusing to start (restore the original key)",
                cfg.master_key_file.display()
            ))
        }
        Some(_) => {}
        // Databases created before the check existed: record it now.
        None => store
            .set_setting("key_check", &key.check_value())
            .map_err(|e| e.message)?,
    }
    Ok((key, store))
}

/// A shared Caddy is reloaded by its owner's own tooling, which drops Rustle's routes:
/// look more often there (a local GET per pass, cheap).
pub fn reconcile_interval(external: bool) -> Duration {
    Duration::from_secs(if external { 15 } else { 60 })
}

/// What to log when the degraded state changes (nothing when it did not).
fn degraded_transition(prev: &Option<String>, now: &Option<String>) -> Option<String> {
    if prev == now {
        return None;
    }
    Some(match now {
        Some(m) => format!("degraded: {m}"),
        None => "recovered".to_string(),
    })
}

async fn reconcile_loop(st: Arc<AppState>, mut hup: Option<tokio::signal::unix::Signal>) {
    let reason = reconcile::boot(&st).await;
    if let Some(r) = &reason {
        tracing::error!("degraded at boot: {r}");
    }
    let mut tick = tokio::time::interval(reconcile_interval(st.cfg.gateway_external));
    tick.tick().await;
    loop {
        // `systemctl kill -s HUP rustle` right after a Caddy reload restores routes at once.
        tokio::select! {
            _ = tick.tick() => {}
            _ = async { match hup.as_mut() { Some(h) => { h.recv().await; } None => std::future::pending().await } } => {
                tracing::info!("SIGHUP: re-syncing routes");
            }
        }
        let r = reconcile::sync(&st, false).await;
        match degraded_transition(&st.degraded(), &r) {
            Some(m) if r.is_some() => tracing::warn!("{m}"),
            Some(m) => tracing::info!("{m}"),
            None => {}
        }
        st.set_degraded(r);
    }
}

/// After 30 healthy seconds the boot-attempt counter used by the auto-revert guard resets and the
/// new binary is considered proven.
pub fn mark_healthy(dir: &std::path::Path) {
    let _ = std::fs::write(dir.join("boot_attempts"), "0");
    let _ = std::fs::remove_file(dir.join("update-pending"));
}

fn spawn_background(state: &Arc<AppState>) {
    // Registered first: an unhandled SIGHUP would terminate the process.
    let hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    tokio::spawn(reconcile_loop(state.clone(), hup));
    let st = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            jobs::tick(&st, util::now());
        }
    });
    let dir = state.cfg.bin_dir.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        mark_healthy(&dir);
    });
}

type Servers = tokio::task::JoinSet<std::io::Result<()>>;

async fn wait_stop(mut rx: tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

async fn listen_tcp(
    addr: &str,
    app: axum::Router,
    rx: tokio::sync::watch::Receiver<bool>,
    servers: &mut Servers,
) -> Result<(), String> {
    let l = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    tracing::info!(
        "listening on http://{}",
        l.local_addr().map(|a| a.to_string()).unwrap_or_default()
    );
    servers.spawn(async move {
        axum::serve(l, app)
            .with_graceful_shutdown(wait_stop(rx))
            .await
    });
    Ok(())
}

fn listen_unix(
    path: &std::path::Path,
    app: axum::Router,
    rx: tokio::sync::watch::Receiver<bool>,
    servers: &mut Servers,
) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let l = tokio::net::UnixListener::bind(path)
        .map_err(|e| format!("bind {}: {e}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    }
    tracing::info!("listening on unix:{}", path.display());
    servers.spawn(async move {
        axum::serve(l, app)
            .with_graceful_shutdown(wait_stop(rx))
            .await
    });
    Ok(())
}

pub async fn run(cfg: Config) -> Result<(), String> {
    run_until(cfg, shutdown_signal()).await
}

/// `run`, with the shutdown trigger injected so tests can stop it.
pub async fn run_until(
    cfg: Config,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let (key, store) = open_checked(&cfg)?;
    if cfg.local_bind.is_none() && cfg.unix_socket.is_none() {
        return Err(
            "no listener configured (RUSTLE_BIND and RUSTLE_SOCKET are both disabled)".into(),
        );
    }
    let state = build_state(cfg.clone(), key, store);
    let app = api::router(state.clone());
    spawn_background(&state);

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut servers = Servers::new();
    if let Some(addr) = &cfg.local_bind {
        listen_tcp(addr, app.clone(), stop_rx.clone(), &mut servers).await?;
    }
    if let Some(path) = &cfg.unix_socket {
        listen_unix(path, app, stop_rx, &mut servers)?;
    }
    shutdown.await;
    let _ = stop_tx.send(true);
    // Give in-flight requests a moment, then stop regardless.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while servers.join_next().await.is_some() {}
    })
    .await;
    if let Some(path) = &cfg.unix_socket {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: &std::path::Path) -> Config {
        let c = Config::for_tests(d);
        std::fs::create_dir_all(&c.proc_root).unwrap();
        std::fs::write(
            c.proc_root.join("meminfo"),
            "MemTotal: 1015808 kB\nMemAvailable: 760000 kB\n",
        )
        .unwrap();
        c
    }

    #[test]
    fn levels() {
        assert_eq!(log_level("trace"), tracing::Level::TRACE);
        assert_eq!(log_level("DEBUG"), tracing::Level::DEBUG);
        assert_eq!(log_level("warn"), tracing::Level::WARN);
        assert_eq!(log_level("error"), tracing::Level::ERROR);
        assert_eq!(log_level("info"), tracing::Level::INFO);
        assert_eq!(log_level(""), tracing::Level::INFO);
        assert_eq!(log_level("nonsense"), tracing::Level::INFO);
    }

    #[test]
    fn intervals_and_health_marker() {
        assert_eq!(reconcile_interval(true), Duration::from_secs(15));
        assert_eq!(reconcile_interval(false), Duration::from_secs(60));
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("boot_attempts"), "2").unwrap();
        std::fs::write(d.path().join("update-pending"), "x").unwrap();
        mark_healthy(d.path());
        assert_eq!(
            std::fs::read_to_string(d.path().join("boot_attempts")).unwrap(),
            "0"
        );
        assert!(!d.path().join("update-pending").exists());
        let (a, b) = (Some("x".to_string()), Some("y".to_string()));
        assert_eq!(
            degraded_transition(&None, &a).as_deref(),
            Some("degraded: x")
        );
        assert_eq!(degraded_transition(&a, &b).as_deref(), Some("degraded: y"));
        assert_eq!(degraded_transition(&a, &None).as_deref(), Some("recovered"));
        assert_eq!(degraded_transition(&None, &None), None);
        assert_eq!(degraded_transition(&a, &a), None);
    }

    #[test]
    fn open_checked_guards() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        let e = open_checked(&c).err().unwrap();
        assert!(e.contains("rustle init"), "{e}");
        crate::cli::init(&c, false).unwrap();
        let (key, store) = open_checked(&c).unwrap();
        assert_eq!(
            store.setting("key_check").unwrap().unwrap(),
            key.check_value()
        );
        // legacy database without a recorded check: it is written
        store.delete_setting("key_check").unwrap();
        drop(store);
        let (_, store) = open_checked(&c).unwrap();
        assert!(store.setting("key_check").unwrap().is_some());
        // a different key is refused
        store.set_setting("key_check", "other").unwrap();
        let e = open_checked(&c).err().unwrap();
        assert!(e.contains("does not match"), "{e}");
    }

    #[tokio::test]
    async fn run_requires_init_and_a_listener() {
        let d = tempfile::tempdir().unwrap();
        let mut c = cfg(d.path());
        let e = run_until(c.clone(), async {}).await.unwrap_err();
        assert!(e.contains("rustle init"), "{e}");
        crate::cli::init(&c, false).unwrap();
        let e = run_until(c.clone(), async {}).await.unwrap_err();
        assert!(e.contains("no listener"), "{e}");
        // a TCP port that is taken
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        c.local_bind = Some(busy.local_addr().unwrap().to_string());
        let e = run_until(c, async {}).await.unwrap_err();
        assert!(e.starts_with("bind "), "{e}");
    }

    #[tokio::test]
    async fn serves_on_unix_socket_and_cleans_up() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let d = tempfile::tempdir().unwrap();
        let mut c = cfg(d.path());
        crate::cli::init(&c, false).unwrap();
        let sock = d.path().join("run/r.sock");
        c.unix_socket = Some(sock.clone());
        c.local_bind = Some("127.0.0.1:0".into());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let h = tokio::spawn(run_until(c, async move {
            let _ = rx.await;
        }));
        let mut s = None;
        for _ in 0..250 {
            if h.is_finished() {
                break; // the server stopped on its own: report why below
            }
            match tokio::net::UnixStream::connect(&sock).await {
                Ok(c) => {
                    s = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let Some(mut s) = s else {
            panic!("server did not come up: {:?}", h.await);
        };
        s.write_all(b"GET /api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.unwrap();
        assert!(buf.starts_with("HTTP/1.1 "), "{buf}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
            0o660
        );
        tx.send(()).unwrap();
        h.await.unwrap().unwrap();
        assert!(!sock.exists(), "socket removed on shutdown");
    }

    #[tokio::test]
    async fn background_reconcile_runs_after_start() {
        let d = tempfile::tempdir().unwrap();
        let mut c = cfg(d.path());
        crate::cli::init(&c, false).unwrap();
        // a deployment left half-done by a crash
        {
            let st = Store::open(&c.db_path).unwrap();
            let t = crate::util::now();
            st.insert_site(&crate::store::SiteRow {
                id: "s1".into(),
                name: "blog".into(),
                domain: "blog.example.com".into(),
                image_repo: "ghcr.io/o/blog".into(),
                container_port: 3000,
                ram_limit_mb: 128,
                status: "deploying".into(),
                current_deployment_id: None,
                registry_id: None,
                zero_downtime: false,
                compat_caps: false,
                created_at: t,
                updated_at: t,
            })
            .unwrap();
        }
        c.local_bind = Some("127.0.0.1:0".into());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let h = tokio::spawn(run_until(c.clone(), async move {
            let _ = rx.await;
        }));
        // boot reconcile turns "deploying" with no deployment into "failed"
        let st = Store::open(&c.db_path).unwrap();
        let mut status = String::new();
        for _ in 0..250 {
            status = st.get_site("s1").unwrap().unwrap().status;
            if status == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status, "failed", "the boot reconcile ran in the background");
        tx.send(()).unwrap();
        h.await.unwrap().unwrap();
    }
}
