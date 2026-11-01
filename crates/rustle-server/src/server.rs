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
        return AppState::with_backends(
            cfg,
            store,
            key,
            Arc::new(FakeRuntime::new()),
            Arc::new(FakeGateway::new()),
            Arc::new(FakeHealth::new(true)),
            Arc::new(FakeResolver::default()),
            Arc::new(FakeGithub::new()),
        );
    }
    let runtime: Arc<dyn Runtime> = Arc::new(HttpRuntime::new(
        Target::from(&cfg.docker),
        cfg.network.clone(),
    ));
    let gateway: Arc<dyn Gateway> = Arc::new(HttpGateway::new(Target::from(&cfg.caddy_admin)));
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

pub async fn run(cfg: Config) -> Result<(), String> {
    let key = MasterKey::load(&cfg.master_key_file, false)
        .map_err(|e| format!("{e}. Run `rustle init` first."))?;
    let store = Store::open(&cfg.db_path).map_err(|e| e.message)?;
    if store.admin().map_err(|e| e.message)?.is_none() {
        return Err("not initialized: run `rustle init` first".into());
    }
    let state = build_state(cfg.clone(), key, store);
    let app = api::router(state.clone());

    // Background: boot reconcile, periodic reconcile, scheduler.
    {
        let st = state.clone();
        tokio::spawn(async move {
            let reason = reconcile::boot(&st).await;
            if let Some(r) = &reason {
                tracing::error!("degraded at boot: {r}");
            }
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let r = reconcile::sync(&st, false).await;
                if r != st.degraded() {
                    match &r {
                        Some(m) => tracing::warn!("degraded: {m}"),
                        None => tracing::info!("recovered"),
                    }
                }
                st.set_degraded(r);
            }
        });
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                jobs::tick(&st, util::now());
            }
        });
        // After 30 healthy seconds the boot-attempt counter used by the auto-revert guard resets.
        let dir = cfg.bin_dir.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = std::fs::write(dir.join("boot_attempts"), "0");
        });
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|v| *v).await;
    };
    let mut servers = tokio::task::JoinSet::new();
    let mut started = 0;
    if let Some(addr) = &cfg.local_bind {
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind {addr}: {e}"))?;
        tracing::info!("listening on http://{addr}");
        let app = app.clone();
        let rx = stop_rx.clone();
        servers.spawn(async move { axum::serve(l, app).with_graceful_shutdown(wait(rx)).await });
        started += 1;
    }
    if let Some(path) = &cfg.unix_socket {
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
        let app = app.clone();
        let rx = stop_rx.clone();
        servers.spawn(async move { axum::serve(l, app).with_graceful_shutdown(wait(rx)).await });
        started += 1;
    }
    if started == 0 {
        return Err(
            "no listener configured (RUSTLE_BIND and RUSTLE_SOCKET are both disabled)".into(),
        );
    }
    shutdown_signal().await;
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
