//! Bring Docker and Caddy back in line with the database: at boot and every minute. SQLite is the
//! source of truth; whatever else exists is repaired or removed.

use crate::app::AppState;
use crate::caddy::BaseParams;
use crate::sites::{admin_domain, dial, placeholder_dial, public_ip};
use std::collections::HashSet;

pub fn base_params(state: &AppState) -> BaseParams {
    BaseParams {
        admin_domain: admin_domain(state),
        acme_email: state.store.setting("acme_email").ok().flatten().filter(|e| !e.is_empty()),
        public_ip: public_ip(state),
        admin_upstream: state.cfg.admin_upstream.clone(),
        admin_listen: String::new(),
    }
}

/// Everything that must exist after a restart. Returns the reason when the system is degraded.
pub async fn boot(state: &AppState) -> Option<String> {
    let t = crate::util::now();
    match state.store.fail_stuck_deployments(t) {
        Ok(n) if n > 0 => tracing::warn!("marked {n} interrupted deployment(s) as failed"),
        _ => {}
    }
    let _ = state.store.fail_stuck_runs(t);
    if let Err(e) = crate::jobs::seed_system_jobs(&state.store) {
        tracing::error!("seeding system jobs failed: {}", e.message);
    }
    // Sites stuck in "deploying" after a crash: derive the real status from the current deployment.
    for s in state.store.list_sites().unwrap_or_default() {
        if s.status == "deploying" {
            let st = if s.current_deployment_id.is_some() { "running" } else { "failed" };
            let _ = state.store.set_site_status(&s.id, st);
        }
    }
    let reason = sync(state, true).await;
    state.set_degraded(reason.clone());
    reason
}

/// One reconcile pass. `full` re-pushes every route (boot); otherwise only drift is repaired.
pub async fn sync(state: &AppState, full: bool) -> Option<String> {
    if let Err(e) = state.runtime.ping().await {
        return Some(format!("container runtime unreachable: {e}"));
    }
    if let Err(e) = state.gateway.ensure_base(&base_params(state)).await {
        return Some(format!("gateway unreachable: {e}"));
    }
    let sites = state.store.list_sites().unwrap_or_default();
    let present: HashSet<String> = match state.gateway.site_ids().await {
        Ok(v) => v.into_iter().collect(),
        Err(e) => return Some(format!("gateway unreachable: {e}")),
    };
    for s in &sites {
        let id = format!("site-{}", s.name);
        if !full && present.contains(&id) {
            continue;
        }
        let upstream = s
            .current_deployment_id
            .as_deref()
            .and_then(|d| state.store.get_deployment(d).ok().flatten())
            .and_then(|d| d.container_name)
            .map(|n| dial(&n, s.container_port))
            .unwrap_or_else(|| placeholder_dial(&s.name, s.container_port));
        if let Err(e) = state.gateway.upsert_site(&s.name, &s.domain, &upstream).await {
            return Some(format!("could not configure route for {}: {e}", s.name));
        }
    }
    let known: HashSet<String> = sites.iter().map(|s| format!("site-{}", s.name)).collect();
    for id in present.difference(&known) {
        tracing::warn!("removing orphan gateway route {id}");
        let _ = state.gateway.remove_site(id.trim_start_matches("site-")).await;
    }
    if full {
        remove_orphan_containers(state).await;
    }
    None
}

/// Containers we created that no deployment refers to. Skipped while any deployment runs, since
/// a half-finished deployment legitimately has a container the database does not point at yet.
async fn remove_orphan_containers(state: &AppState) {
    if state.deployer.any_busy() {
        return;
    }
    let Ok(list) = state.runtime.list_containers(Some("rustle.managed")).await else { return };
    let mut keep: HashSet<String> = HashSet::new();
    for s in state.store.list_sites().unwrap_or_default() {
        if let Some(c) = s
            .current_deployment_id
            .as_deref()
            .and_then(|d| state.store.get_deployment(d).ok().flatten())
            .and_then(|d| d.container_id)
        {
            keep.insert(c);
        }
    }
    for c in list {
        let kind = c.labels.get("rustle.kind").map(String::as_str).unwrap_or("");
        let owned = c.labels.get("rustle.managed").map(String::as_str) == Some("1");
        if owned && kind == "app" && !keep.contains(&c.id) {
            tracing::warn!("removing orphan container {}", c.name);
            let _ = state.runtime.stop(&c.id, state.tuning.stop_timeout_secs).await;
            let _ = state.runtime.remove(&c.id, true).await;
        } else if owned && kind == "cron" && c.state != "running" {
            let _ = state.runtime.remove(&c.id, true).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;
    use crate::caddy::Gateway;
    use crate::deploy::webhook;
    use crate::sites::create_site;
    use rustle_types::CreateSiteReq;

    async fn mk(e: &TestEnv, name: &str) -> crate::store::SiteRow {
        let d = create_site(
            &e.state,
            CreateSiteReq {
                name: name.into(), domain: format!("{name}.example.com"), image_repo: format!("ghcr.io/o/{name}"), ram_limit_mb: 128,
                container_port: 3000, registry_id: None, env: vec![], skip_dns_check: true, override_budget: true,
            },
        )
        .await
        .unwrap();
        e.state.store.get_site(&d.id).unwrap().unwrap()
    }

    #[tokio::test]
    async fn boot_restores_routes_after_caddy_lost_its_config() {
        let e = TestEnv::new();
        let s = mk(&e, "blog").await;
        webhook(&e.state, &s, "ghcr.io/o/blog:1").unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        let good = e.gateway.sites()["blog"].clone();
        e.gateway.wipe();
        assert!(e.gateway.sites().is_empty());
        assert!(boot(&e.state).await.is_none());
        assert_eq!(e.gateway.sites()["blog"], good, "route points at the live container again");
        assert!(e.gateway.base().is_some());
    }

    #[tokio::test]
    async fn periodic_pass_repairs_only_drift_and_removes_orphans() {
        let e = TestEnv::new();
        mk(&e, "a").await;
        e.gateway.upsert_site("ghost", "ghost.example.com", "x:1").await.unwrap();
        e.gateway.remove_site("a").await.unwrap();
        e.gateway.clear_calls();
        assert!(sync(&e.state, false).await.is_none());
        let sites = e.gateway.sites();
        assert!(sites.contains_key("a") && !sites.contains_key("ghost"));
    }

    #[tokio::test]
    async fn gateway_down_means_degraded_not_a_crash() {
        let e = TestEnv::new();
        e.gateway.set_down(true);
        let reason = boot(&e.state).await.unwrap();
        assert!(reason.contains("gateway"));
        assert_eq!(e.state.degraded().as_deref(), Some(reason.as_str()));
        e.gateway.set_down(false);
        assert!(sync(&e.state, false).await.is_none());
    }

    #[tokio::test]
    async fn orphan_containers_are_removed_but_current_and_foreign_are_kept() {
        let e = TestEnv::new();
        let s = mk(&e, "blog").await;
        webhook(&e.state, &s, "ghcr.io/o/blog:1").unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        let orphan = e.runtime.add_foreign_container("rustle-blog-dead", &[("rustle.managed", "1"), ("rustle.kind", "app"), ("rustle.site", &s.id)]);
        let foreign = e.runtime.add_foreign_container("someone-elses", &[]);
        boot(&e.state).await;
        let ids: Vec<String> = e.runtime.containers().into_iter().map(|c| c.id).collect();
        assert!(!ids.contains(&orphan));
        assert!(ids.contains(&foreign), "containers Rustle did not create are never touched");
        assert_eq!(ids.len(), 2, "current app container + foreign");
    }

    #[tokio::test]
    async fn interrupted_work_is_closed_at_boot() {
        let e = TestEnv::new();
        let s = mk(&e, "blog").await;
        e.state.store.set_site_status(&s.id, "deploying").unwrap();
        let acc = {
            let row = e.state.store.get_site(&s.id).unwrap().unwrap();
            // insert a pending deployment directly, as if the process died mid-deploy
            crate::deploy::request_deploy(&e.state, &row, "ghcr.io/o/blog:1".into(), "webhook", None, None).unwrap()
        };
        e.state.deployer.wait_idle(&s.id).await;
        // simulate crash leftovers
        let mut d = e.state.store.get_deployment(&acc.deployment_id).unwrap().unwrap();
        d.status = "pending".into();
        e.state.store.update_deployment(&d).unwrap();
        e.state.store.set_site_status(&s.id, "deploying").unwrap();
        boot(&e.state).await;
        assert_eq!(e.state.store.get_deployment(&acc.deployment_id).unwrap().unwrap().status, "failed");
        assert_ne!(e.state.store.get_site(&s.id).unwrap().unwrap().status, "deploying");
    }
}
