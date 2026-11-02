//! Deployment orchestration: per-site serialization with a latest-wins queue of depth 1, the
//! stop-first (default) and start-first (opt-in) strategies, health checks, rollback.

use crate::app::AppState;
use crate::docker::{ContainerSpec, RtError, Runtime};
use crate::error::{AppError, AppResult};
use crate::imageref::ImageRef;
use crate::metrics;
use crate::sites::{self, effective_env, load_env, registry_auth, upstream};
use crate::store::{DeploymentRow, SiteRow};
use crate::util::{now, rand_hex};
use rustle_types::{DeployAccepted, EnvVar};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Job {
    pub deployment_id: String,
    pub image_ref: String,
    pub digest_override: Option<String>,
    pub env_override: Option<Vec<EnvVar>>,
}

#[derive(Default)]
struct Slot {
    running: bool,
    queued: Option<Job>,
}

pub enum Submit {
    /// Caller must start the run loop.
    Run,
    /// Another deployment is running; `replaced` is a queued job that this one superseded.
    Queued { replaced: Option<Job> },
}

#[derive(Default)]
pub struct Deployer {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Deployer {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_busy(&self, site_id: &str) -> bool {
        self.lock().get(site_id).is_some_and(|s| s.running)
    }

    pub fn any_busy(&self) -> bool {
        self.lock().values().any(|s| s.running)
    }

    pub fn submit(&self, site_id: &str, job: Job) -> Submit {
        let mut m = self.lock();
        let slot = m.entry(site_id.to_string()).or_default();
        if slot.running {
            Submit::Queued {
                replaced: slot.queued.replace(job),
            }
        } else {
            slot.running = true;
            Submit::Run
        }
    }

    /// Next queued job, or `None` after releasing the slot.
    pub fn next(&self, site_id: &str) -> Option<Job> {
        let mut m = self.lock();
        let slot = m.get_mut(site_id)?;
        match slot.queued.take() {
            Some(j) => Some(j),
            None => {
                slot.running = false;
                None
            }
        }
    }

    /// Test helper: wait for a site's deployment to finish, failing loudly instead of hanging.
    #[cfg(test)]
    pub async fn wait_idle(&self, site_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while self.is_busy(site_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "deployment of {site_id} did not finish"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Wait until no deployment is running anywhere (used before self-update).
    pub async fn wait_all_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.any_busy() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }
}

pub fn request_deploy(
    state: &Arc<AppState>,
    site: &SiteRow,
    image_ref: String,
    trigger: &str,
    digest_override: Option<String>,
    env_override: Option<Vec<EnvVar>>,
) -> AppResult<DeployAccepted> {
    if state.maintenance.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(AppError::unavailable(
            "an update is being applied; retry in a minute",
        ));
    }
    let id = rand_hex(6);
    let t = now();
    state.store.insert_deployment(&DeploymentRow {
        id: id.clone(),
        site_id: site.id.clone(),
        image_ref: image_ref.clone(),
        image_digest: None,
        container_id: None,
        container_name: None,
        status: "pending".into(),
        trigger: trigger.into(),
        started_at: t,
        finished_at: None,
        error: None,
        env_snapshot_enc: None,
    })?;
    state.store.audit(t, trigger, "deploy.request", &site.name);
    let job = Job {
        deployment_id: id.clone(),
        image_ref,
        digest_override,
        env_override,
    };
    match state.deployer.submit(&site.id, job.clone()) {
        Submit::Run => {
            let _ = state.store.set_site_status(&site.id, "deploying");
            let st = state.clone();
            let sid = site.id.clone();
            tokio::spawn(async move { run_loop(st, sid, job).await });
            Ok(DeployAccepted {
                deployment_id: id,
                queued: false,
            })
        }
        Submit::Queued { replaced } => {
            if let Some(old) = replaced {
                if let Ok(Some(mut d)) = state.store.get_deployment(&old.deployment_id) {
                    d.status = "superseded".into();
                    d.error = Some("replaced by a newer request before it started".into());
                    d.finished_at = Some(now());
                    let _ = state.store.update_deployment(&d);
                }
            }
            Ok(DeployAccepted {
                deployment_id: id,
                queued: true,
            })
        }
    }
}

async fn run_loop(state: Arc<AppState>, site_id: String, mut job: Job) {
    loop {
        execute(&state, &site_id, &job).await;
        match state.deployer.next(&site_id) {
            Some(j) => job = j,
            None => break,
        }
    }
}

struct Failure {
    msg: String,
    /// The previously running container (if any) is serving again.
    previous_ok: bool,
}

fn trunc(s: &str) -> String {
    s.chars().take(500).collect()
}

pub async fn execute(state: &AppState, site_id: &str, job: &Job) {
    let (Ok(Some(site)), Ok(Some(mut dep))) = (
        state.store.get_site(site_id),
        state.store.get_deployment(&job.deployment_id),
    ) else {
        return; // site deleted while queued
    };
    let outcome = run_deploy(state, &site, &mut dep, job).await;
    dep.finished_at = Some(now());
    match outcome {
        Ok(()) => {
            dep.status = "running".into();
            let _ = state.store.update_deployment(&dep);
            let _ = state.store.set_current_deployment(&site.id, Some(&dep.id));
            let _ = state.store.set_site_status(&site.id, "running");
            let _ = state.store.trim_deployments(&site.id, 50);
        }
        Err(f) => {
            tracing::warn!(site = %site.name, "deployment failed: {}", f.msg);
            dep.status = "failed".into();
            dep.error = Some(trunc(&f.msg));
            let _ = state.store.update_deployment(&dep);
            let status = if f.previous_ok { "running" } else { "failed" };
            let _ = state.store.set_site_status(&site.id, status);
            // A token holder must not be able to grow the table with failing deploys.
            let _ = state.store.trim_deployments(&site.id, 50);
        }
    }
}

/// Bring the previous version back after a failed attempt (only if we had stopped it).
async fn restore_old(
    rt: &dyn crate::docker::Runtime,
    old: &Option<DeploymentRow>,
    stopped_old: bool,
    prev_ok: bool,
    msg: String,
) -> Failure {
    let mut previous_ok = prev_ok && !stopped_old;
    if stopped_old {
        if let Some(o) = old {
            previous_ok = rt
                .start(o.container_id.as_deref().unwrap_or_default())
                .await
                .is_ok();
        }
    }
    Failure { msg, previous_ok }
}

fn has_headroom(state: &AppState, ram_limit_mb: u32) -> bool {
    let m = metrics::meminfo(&state.cfg.proc_root);
    m.available_kb / 1024 >= ram_limit_mb as u64 + 64
}

/// What is needed to undo a failed attempt.
struct Undo {
    old: Option<DeploymentRow>,
    stopped_old: bool,
    prev_ok: bool,
}

impl Undo {
    async fn fail(&self, rt: &dyn Runtime, msg: String) -> Failure {
        restore_old(rt, &self.old, self.stopped_old, self.prev_ok, msg).await
    }
}

/// The previous deployment, if it still has a container.
fn previous_deployment(
    state: &AppState,
    site: &SiteRow,
    dep: &DeploymentRow,
) -> Option<DeploymentRow> {
    site.current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten())
        .filter(|d| d.id != dep.id && d.container_id.is_some())
}

/// Pull the image and pin it by digest.
async fn pull_and_pin(
    state: &AppState,
    site: &SiteRow,
    job: &Job,
    r: &ImageRef,
    prev_ok: bool,
) -> Result<String, Failure> {
    let fail = |msg: String| Failure {
        msg,
        previous_ok: prev_ok,
    };
    let rt = &state.runtime;
    let auth = match &site.registry_id {
        Some(rid) => Some(registry_auth(state, rid).map_err(|e| fail(e.message))?),
        None => None,
    };
    let pull_ref = r.canonical();
    rt.pull(&pull_ref, auth.as_ref())
        .await
        .map_err(|e| fail(format!("pull failed: {e}")))?;
    match &job.digest_override {
        Some(d) => Ok(d.clone()),
        None => rt
            .resolve_digest(&pull_ref, &r.repo_key())
            .await
            .map_err(|e| fail(format!("cannot resolve image digest: {e}"))),
    }
}

/// Environment for this deployment (override for rollbacks), with an encrypted snapshot on `dep`.
fn deployment_env(
    state: &AppState,
    site: &SiteRow,
    job: &Job,
    dep: &mut DeploymentRow,
    prev_ok: bool,
) -> Result<Vec<String>, Failure> {
    let fail = |msg: String| Failure {
        msg,
        previous_ok: prev_ok,
    };
    let vars: Vec<EnvVar> = match &job.env_override {
        Some(v) => v.clone(),
        None => load_env(state, &site.id).map_err(|e| fail(e.message))?,
    };
    let snap = serde_json::to_string(&vars).map_err(|e| fail(e.to_string()))?;
    dep.env_snapshot_enc = Some(state.key.seal_str(&snap, &format!("snap:{}", dep.id)));
    Ok(effective_env(&vars, site.container_port, site.ram_limit_mb))
}

fn container_spec(
    state: &AppState,
    site: &SiteRow,
    dep: &DeploymentRow,
    digest: String,
    env: Vec<String>,
) -> ContainerSpec {
    let labels = BTreeMap::from([
        ("rustle.managed".to_string(), "1".to_string()),
        ("rustle.kind".to_string(), "app".to_string()),
        ("rustle.site".to_string(), site.id.clone()),
        ("rustle.deployment".to_string(), dep.id.clone()),
    ]);
    ContainerSpec {
        name: format!("rustle-{}-{}", site.name, &dep.id[..dep.id.len().min(8)]),
        image: digest,
        env,
        ram_limit_mb: site.ram_limit_mb,
        labels,
        compat_caps: site.compat_caps,
        network: state.cfg.network.clone(),
        cmd: None,
        long_running: true,
    }
}

/// Wait until the container answers on its port. `Ok(ip)` when healthy, `Err(reason)` otherwise.
async fn wait_healthy(state: &AppState, id: &str, port: u16) -> Result<String, String> {
    let mut why = String::from("no response before the deadline");
    for _ in 0..state.tuning.health_attempts {
        match state.runtime.inspect(id).await {
            Ok(info) if !info.running => {
                return Err(format!(
                    "container exited with code {}{}",
                    info.exit_code,
                    if info.oom_killed {
                        " (out of memory)"
                    } else {
                        ""
                    }
                ));
            }
            Ok(info) => {
                if let Some(ip) = info.ip {
                    if state.health.probe(&ip, port).await {
                        return Ok(ip);
                    }
                }
            }
            Err(e) => {
                why = format!("inspect failed: {e}");
                break;
            }
        }
        tokio::time::sleep(state.tuning.health_interval).await;
    }
    Err(why)
}

/// Create and start the new container; on failure the half-made container is removed.
async fn create_and_start(rt: &dyn Runtime, spec: &ContainerSpec) -> Result<String, String> {
    let id = rt
        .create(spec)
        .await
        .map_err(|e| format!("create failed: {e}"))?;
    if let Err(e) = rt.start(&id).await {
        let _ = rt.remove(&id, true).await;
        return Err(format!("start failed: {e}"));
    }
    Ok(id)
}

async fn run_deploy(
    state: &AppState,
    site: &SiteRow,
    dep: &mut DeploymentRow,
    job: &Job,
) -> Result<(), Failure> {
    let rt = &state.runtime;
    let had_current = site.current_deployment_id.is_some();
    let r = ImageRef::parse(&job.image_ref).map_err(|e| Failure {
        msg: e.to_string(),
        previous_ok: had_current,
    })?;
    if r.repo_key() != site.image_repo {
        return Err(Failure {
            msg: "image repository does not match the site".into(),
            previous_ok: had_current,
        });
    }
    let old = previous_deployment(state, site, dep);
    let prev_ok = old.is_some();

    let digest = pull_and_pin(state, site, job, &r, prev_ok).await?;
    dep.image_digest = Some(digest.clone());
    let env = deployment_env(state, site, job, dep, prev_ok)?;
    let start_first = site.zero_downtime && prev_ok && has_headroom(state, site.ram_limit_mb);
    let spec = container_spec(state, site, dep, digest, env);

    // Stop-first: the old container is stopped but NOT removed, so it can be restarted if the
    // new one fails. Start-first leaves it running until the new one is healthy.
    let mut undo = Undo {
        old,
        stopped_old: false,
        prev_ok,
    };
    let mut old_gone = false;
    if let (false, Some(o)) = (start_first, &undo.old) {
        match rt
            .stop(
                o.container_id.as_deref().unwrap(),
                state.tuning.stop_timeout_secs,
            )
            .await
        {
            Ok(()) => undo.stopped_old = true,
            Err(RtError::NotFound) => old_gone = true, // vanished: nothing to restore later
            Err(e) => {
                return Err(Failure {
                    msg: format!("could not stop the running version: {e}"),
                    previous_ok: true,
                })
            }
        }
    }
    undo.prev_ok = prev_ok && !old_gone;

    let id = match create_and_start(rt.as_ref(), &spec).await {
        Ok(id) => id,
        Err(msg) => return Err(undo.fail(rt.as_ref(), msg).await),
    };
    dep.container_id = Some(id.clone());
    dep.container_name = Some(spec.name.clone());

    let ip = match wait_healthy(state, &id, site.container_port).await {
        Ok(ip) => ip,
        Err(why) => {
            let tail = rt.logs(&id, 15).await.unwrap_or_default();
            let _ = rt.remove(&id, true).await;
            let tail = tail.trim();
            let msg = if tail.is_empty() {
                format!("health check failed: {why}")
            } else {
                format!("health check failed: {why}. Last logs: {tail}")
            };
            return Err(undo.fail(rt.as_ref(), msg).await);
        }
    };

    // Switch traffic.
    let dial = upstream(state, &spec.name, Some(&ip), site.container_port);
    if let Err(e) = state
        .gateway
        .upsert_site(&site.name, &site.domain, &dial)
        .await
    {
        let _ = rt.remove(&id, true).await;
        return Err(undo
            .fail(rt.as_ref(), format!("gateway update failed: {e}"))
            .await);
    }

    // Record the new deployment as current BEFORE the old container disappears: a crash between
    // the two must never leave the database pointing at a container that no longer exists.
    dep.status = "running".into();
    dep.finished_at = Some(now());
    let _ = state.store.update_deployment(dep);
    let _ = state.store.set_current_deployment(&site.id, Some(&dep.id));
    retire_previous(state, undo.old.as_ref(), start_first).await;
    Ok(())
}

/// Success: retire the previous version.
async fn retire_previous(state: &AppState, old: Option<&DeploymentRow>, was_running: bool) {
    let Some(o) = old else { return };
    let cid = o.container_id.as_deref().unwrap();
    if was_running {
        let _ = state
            .runtime
            .stop(cid, state.tuning.stop_timeout_secs)
            .await;
    }
    let _ = state.runtime.remove(cid, true).await;
    let mut o2 = o.clone();
    o2.status = "superseded".into();
    let _ = state.store.update_deployment(&o2);
}

pub fn rollback(
    state: &Arc<AppState>,
    site: &SiteRow,
    target_id: &str,
) -> AppResult<DeployAccepted> {
    let target = state
        .store
        .get_deployment(target_id)?
        .filter(|d| d.site_id == site.id)
        .ok_or_else(|| AppError::not_found("deployment not found for this site"))?;
    if site.current_deployment_id.as_deref() == Some(target.id.as_str()) {
        return Err(AppError::conflict(
            "that deployment is already the current one",
        ));
    }
    if !matches!(target.status.as_str(), "running" | "superseded") {
        return Err(AppError::conflict(
            "only a deployment that previously ran successfully can be rolled back to",
        ));
    }
    let digest = target
        .image_digest
        .clone()
        .ok_or_else(|| AppError::conflict("deployment has no recorded image digest"))?;
    let env = match &target.env_snapshot_enc {
        Some(blob) => {
            let s = state
                .key
                .open_str(blob, &format!("snap:{}", target.id))
                .map_err(AppError::internal)?;
            Some(serde_json::from_str::<Vec<EnvVar>>(&s).map_err(AppError::internal)?)
        }
        None => None,
    };
    request_deploy(state, site, digest.clone(), "rollback", Some(digest), env)
}

pub fn redeploy(state: &Arc<AppState>, site: &SiteRow) -> AppResult<DeployAccepted> {
    let cur = site
        .current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten())
        .ok_or_else(|| AppError::conflict("nothing to redeploy yet"))?;
    let digest = cur
        .image_digest
        .clone()
        .unwrap_or_else(|| cur.image_ref.clone());
    request_deploy(state, site, digest.clone(), "redeploy", Some(digest), None)
}

pub fn site_for_webhook(
    state: &AppState,
    token_site_id: &str,
    site_name: Option<&str>,
) -> AppResult<SiteRow> {
    let site = state
        .store
        .get_site(token_site_id)?
        .ok_or_else(|| AppError::unauthorized("invalid deploy token"))?;
    if let Some(n) = site_name {
        if n != site.name {
            return Err(AppError::forbidden(
                "deploy token does not belong to that site",
            ));
        }
    }
    Ok(site)
}

pub fn webhook(state: &Arc<AppState>, site: &SiteRow, image: &str) -> AppResult<DeployAccepted> {
    let r = sites::image_matches_site(site, image)?;
    request_deploy(state, site, r.canonical(), "webhook", None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;
    use crate::sites::{create_site, update_site};
    use rustle_types::{CreateSiteReq, UpdateSiteReq};

    async fn site(e: &TestEnv, name: &str, ram: u32) -> SiteRow {
        let dto = create_site(
            &e.state,
            CreateSiteReq {
                name: name.into(),
                domain: format!("{name}.example.com"),
                image_repo: format!("ghcr.io/o/{name}"),
                ram_limit_mb: ram,
                container_port: 3000,
                registry_id: None,
                env: vec![EnvVar {
                    key: "SECRET".into(),
                    value: "v1".into(),
                }],
                skip_dns_check: true,
                override_budget: true,
            },
        )
        .await
        .unwrap();
        e.state.store.get_site(&dto.id).unwrap().unwrap()
    }

    async fn deploy(e: &TestEnv, s: &SiteRow, tag: &str) -> DeployAccepted {
        let s = e.state.store.get_site(&s.id).unwrap().unwrap();
        let acc = webhook(&e.state, &s, &format!("ghcr.io/o/{}:{tag}", s.name)).unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        acc
    }

    fn pos(calls: &[String], needle: &str) -> usize {
        calls
            .iter()
            .position(|c| c.starts_with(needle))
            .unwrap_or_else(|| panic!("{needle} not in {calls:?}"))
    }

    fn job_stub() -> Job {
        Job {
            deployment_id: "d".into(),
            image_ref: "ghcr.io/o/a:1".into(),
            digest_override: None,
            env_override: None,
        }
    }

    async fn last_deployment(e: &TestEnv, id: &str) -> crate::store::DeploymentRow {
        e.state.store.get_deployment(id).unwrap().unwrap()
    }

    #[tokio::test]
    async fn crashing_container_fails_fast_with_its_exit_code() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        e.runtime.crash_next_start(137);
        let second = deploy(&e, &s, "v2").await;
        let d = last_deployment(&e, &second.deployment_id).await;
        assert_eq!(d.status, "failed");
        let err = d.error.unwrap();
        assert!(err.contains("container exited with code 137"), "{err}");
        let site = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert_eq!(
            site.current_deployment_id.as_deref(),
            Some(first.deployment_id.as_str())
        );
        assert_eq!(site.status, "running");
        assert_eq!(
            e.runtime.containers().iter().filter(|c| c.running).count(),
            1
        );
    }

    #[tokio::test]
    async fn if_the_old_container_vanished_a_failed_deploy_leaves_the_site_failed() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        deploy(&e, &s, "v1").await;
        // somebody removed the running container behind Rustle's back
        for c in e.runtime.containers() {
            Runtime::remove(&*e.runtime, &c.id, true).await.unwrap();
        }
        e.runtime.crash_next_start(1);
        let second = deploy(&e, &s, "v2").await;
        assert_eq!(
            last_deployment(&e, &second.deployment_id).await.status,
            "failed"
        );
        let site = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert_eq!(
            site.status, "failed",
            "there is no previous version left to serve"
        );
    }

    #[tokio::test]
    async fn start_first_failure_leaves_the_old_version_untouched() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        update_site(
            &e.state,
            &s.id,
            UpdateSiteReq {
                zero_downtime: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        deploy(&e, &s, "v1").await;
        e.runtime.fail_next_starts(1);
        e.runtime.clear_calls();
        let second = deploy(&e, &s, "v2").await;
        assert_eq!(
            last_deployment(&e, &second.deployment_id).await.status,
            "failed"
        );
        let calls = e.runtime.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("stop")),
            "old never stopped: {calls:?}"
        );
        assert_eq!(
            e.state.store.get_site(&s.id).unwrap().unwrap().status,
            "running"
        );
        assert_eq!(
            e.runtime.containers().iter().filter(|c| c.running).count(),
            1
        );
    }

    #[tokio::test]
    async fn previous_deployment_must_be_another_one_with_a_container() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let acc = deploy(&e, &s, "v1").await;
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        let cur = last_deployment(&e, &acc.deployment_id).await;
        // the current deployment is not its own predecessor
        assert!(previous_deployment(&e.state, &site_row, &cur).is_none());
        // a different deployment object: the current one is the predecessor
        let mut next = cur.clone();
        next.id = "another".into();
        assert_eq!(
            previous_deployment(&e.state, &site_row, &next).unwrap().id,
            cur.id
        );
        // ...unless it has no container any more
        let mut gone = cur.clone();
        gone.container_id = None;
        e.state.store.update_deployment(&gone).unwrap();
        assert!(previous_deployment(&e.state, &site_row, &next).is_none());
    }

    #[tokio::test]
    async fn wait_all_idle_reports_whether_it_became_idle() {
        let d = Deployer::default();
        assert!(d.wait_all_idle(Duration::from_millis(10)).await);
        assert!(matches!(d.submit("s", job_stub()), Submit::Run));
        let t = std::time::Instant::now();
        assert!(
            !d.wait_all_idle(Duration::from_millis(120)).await,
            "still busy at the deadline"
        );
        assert!(t.elapsed() >= Duration::from_millis(100));
        assert!(d.is_busy("s") && d.any_busy());
        assert!(d.next("s").is_none());
        assert!(!d.is_busy("s") && !d.any_busy());
        assert!(d.wait_all_idle(Duration::from_millis(10)).await);
        // a job finishing while we wait is noticed
        d.submit("s", job_stub());
        let d = std::sync::Arc::new(d);
        let d2 = d.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            d2.next("s");
        });
        assert!(d.wait_all_idle(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn site_actions_start_stop_restart() {
        use crate::sites::{site_action, Action};
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        // nothing deployed yet: a clear conflict, not a crash
        let err = site_action(&e.state, &s.id, Action::Stop)
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::CONFLICT);
        let err = site_action(&e.state, "nope", Action::Stop)
            .await
            .unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);

        deploy(&e, &s, "v1").await;
        let running = |e: &TestEnv| e.runtime.containers().iter().filter(|c| c.running).count();
        assert_eq!(running(&e), 1);

        let dto = site_action(&e.state, &s.id, Action::Stop).await.unwrap();
        assert_eq!(dto.status, "stopped");
        assert_eq!(running(&e), 0);
        assert_eq!(
            e.state.store.get_site(&s.id).unwrap().unwrap().status,
            "stopped"
        );

        let dto = site_action(&e.state, &s.id, Action::Start).await.unwrap();
        assert_eq!(dto.status, "running");
        assert_eq!(running(&e), 1);

        e.runtime.clear_calls();
        let dto = site_action(&e.state, &s.id, Action::Restart).await.unwrap();
        assert_eq!(dto.status, "running");
        let calls = e.runtime.calls();
        assert!(pos(&calls, "stop") < pos(&calls, "start"), "{calls:?}");
        assert_eq!(running(&e), 1);
    }

    #[tokio::test]
    async fn first_deploy_happy_path() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let acc = deploy(&e, &s, "v1").await;
        assert!(!acc.queued);
        let st = &e.state.store;
        let dep = st.get_deployment(&acc.deployment_id).unwrap().unwrap();
        assert_eq!(dep.status, "running");
        assert!(dep
            .image_digest
            .as_deref()
            .unwrap()
            .starts_with("ghcr.io/o/blog@sha256:"));
        assert!(dep.env_snapshot_enc.is_some());
        let site = st.get_site(&s.id).unwrap().unwrap();
        assert_eq!(site.status, "running");
        assert_eq!(site.current_deployment_id.as_deref(), Some(dep.id.as_str()));
        // traffic goes to the new container by name, on its port
        let (_, dial) = e.gateway.sites()["blog"].clone();
        assert_eq!(dial, format!("{}:3000", dep.container_name.unwrap()));
        // container is the strict kind
        let c = &e.runtime.containers()[0];
        assert_eq!(c.spec.ram_limit_mb, 256);
        assert_eq!(c.spec.labels["rustle.site"], s.id);
        assert!(
            c.spec.image.contains("@sha256:"),
            "runs by digest, never by tag"
        );
        assert!(c.spec.env.contains(&"SECRET=v1".to_string()));
        assert!(c
            .spec
            .env
            .contains(&"NODE_OPTIONS=--max-old-space-size=192".to_string()));
        let calls = e.runtime.calls();
        assert!(
            pos(&calls, "pull") < pos(&calls, "create")
                && pos(&calls, "create") < pos(&calls, "start")
        );
    }

    #[tokio::test]
    async fn shared_caddy_gets_no_route_until_the_first_deploy_then_the_container_ip() {
        let e = TestEnv::external();
        let s = site(&e, "blog", 256).await;
        assert!(
            e.gateway.sites().is_empty(),
            "creating a site must not touch traffic that an existing site may already be serving"
        );
        deploy(&e, &s, "v1").await;
        let (domain, dial) = e.gateway.sites()["blog"].clone();
        assert_eq!(domain, "blog.example.com");
        let ip = e.runtime.containers()[0].ip.clone();
        assert_eq!(
            dial,
            format!("{ip}:3000"),
            "a host Caddy cannot resolve container names"
        );
    }

    #[tokio::test]
    async fn shared_caddy_container_on_rustle_net_is_given_container_names() {
        let e = TestEnv::external_names();
        let s = site(&e, "blog", 256).await;
        assert!(
            e.gateway.sites().is_empty(),
            "still no route before the first deploy"
        );
        deploy(&e, &s, "v1").await;
        let name = e.runtime.containers()[0].spec.name.clone();
        assert_eq!(
            e.gateway.sites()["blog"].1,
            format!("{name}:3000"),
            "a container Caddy resolves names, no IP to go stale"
        );
    }

    #[tokio::test]
    async fn second_deploy_is_stop_first_and_retires_old() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        let old_name = e
            .state
            .store
            .get_deployment(&first.deployment_id)
            .unwrap()
            .unwrap()
            .container_name
            .unwrap();
        e.runtime.clear_calls();
        let second = deploy(&e, &s, "v2").await;
        let calls = e.runtime.calls();
        let stop_old = pos(&calls, &format!("stop {old_name}"));
        assert!(
            stop_old < pos(&calls, "create"),
            "old stopped before new is created: {calls:?}"
        );
        assert!(pos(&calls, "start rustle-blog-") > stop_old);
        assert!(
            calls.iter().any(|c| c == &format!("remove {old_name}")),
            "old removed after success"
        );
        assert_eq!(
            e.state
                .store
                .get_deployment(&first.deployment_id)
                .unwrap()
                .unwrap()
                .status,
            "superseded"
        );
        assert_eq!(
            e.state
                .store
                .get_deployment(&second.deployment_id)
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(e.runtime.containers().len(), 1);
    }

    #[tokio::test]
    async fn failed_health_restores_old_and_keeps_route() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        let route_before = e.gateway.sites()["blog"].clone();
        e.health
            .default
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let second = deploy(&e, &s, "v2").await;
        let d = e
            .state
            .store
            .get_deployment(&second.deployment_id)
            .unwrap()
            .unwrap();
        assert_eq!(d.status, "failed");
        assert!(d.error.unwrap().contains("health check failed"));
        assert_eq!(e.gateway.sites()["blog"], route_before, "route untouched");
        let site = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert_eq!(site.status, "running", "previous version is serving again");
        assert_eq!(
            site.current_deployment_id.as_deref(),
            Some(first.deployment_id.as_str())
        );
        let cs = e.runtime.containers();
        assert_eq!(cs.len(), 1);
        assert!(cs[0].running, "old container restarted");
    }

    #[tokio::test]
    async fn first_deploy_failure_marks_site_failed() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        e.health
            .default
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let acc = deploy(&e, &s, "v1").await;
        assert_eq!(
            e.state
                .store
                .get_deployment(&acc.deployment_id)
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );
        assert_eq!(
            e.state.store.get_site(&s.id).unwrap().unwrap().status,
            "failed"
        );
        assert!(
            e.runtime.containers().is_empty(),
            "failed container is cleaned up"
        );
    }

    #[tokio::test]
    async fn pull_failure_creates_nothing() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        e.runtime.fail_pull_with(Some("denied: private image"));
        e.runtime.clear_calls();
        let second = deploy(&e, &s, "v2").await;
        let d = e
            .state
            .store
            .get_deployment(&second.deployment_id)
            .unwrap()
            .unwrap();
        assert!(d.error.unwrap().contains("denied"));
        assert!(
            !e.runtime
                .calls()
                .iter()
                .any(|c| c.starts_with("stop") || c.starts_with("create")),
            "old version untouched"
        );
        assert_eq!(
            e.state
                .store
                .get_site(&s.id)
                .unwrap()
                .unwrap()
                .current_deployment_id
                .as_deref(),
            Some(first.deployment_id.as_str())
        );
    }

    #[tokio::test]
    async fn start_failure_restores_old() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        deploy(&e, &s, "v1").await;
        e.runtime.fail_next_starts(1);
        let second = deploy(&e, &s, "v2").await;
        assert_eq!(
            e.state
                .store
                .get_deployment(&second.deployment_id)
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );
        assert_eq!(
            e.state.store.get_site(&s.id).unwrap().unwrap().status,
            "running"
        );
        let cs = e.runtime.containers();
        assert_eq!(cs.len(), 1);
        assert!(cs[0].running);
    }

    #[tokio::test]
    async fn gateway_failure_restores_old() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        deploy(&e, &s, "v1").await;
        e.gateway.set_down(true);
        let second = deploy(&e, &s, "v2").await;
        let d = e
            .state
            .store
            .get_deployment(&second.deployment_id)
            .unwrap()
            .unwrap();
        assert!(d.error.unwrap().contains("gateway"));
        assert_eq!(e.runtime.containers().len(), 1);
        assert!(e.runtime.containers()[0].running);
    }

    #[tokio::test]
    async fn start_first_keeps_old_until_new_is_healthy() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        update_site(
            &e.state,
            &s.id,
            UpdateSiteReq {
                zero_downtime: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = deploy(&e, &s, "v1").await;
        let old_name = e
            .state
            .store
            .get_deployment(&first.deployment_id)
            .unwrap()
            .unwrap()
            .container_name
            .unwrap();
        e.runtime.clear_calls();
        deploy(&e, &s, "v2").await;
        let calls = e.runtime.calls();
        let create = pos(&calls, "create");
        let stop_old = pos(&calls, &format!("stop {old_name}"));
        assert!(
            create < stop_old,
            "new is created before old is stopped: {calls:?}"
        );
        // and a failing new version never touches the old one
        e.runtime.clear_calls();
        e.health
            .default
            .store(false, std::sync::atomic::Ordering::SeqCst);
        deploy(&e, &s, "v3").await;
        assert!(
            !e.runtime.calls().iter().any(|c| c.starts_with("stop")),
            "old never stopped on failure: {:?}",
            e.runtime.calls()
        );
        assert_eq!(e.runtime.containers().len(), 1);
    }

    #[tokio::test]
    async fn start_first_falls_back_without_headroom() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        update_site(
            &e.state,
            &s.id,
            UpdateSiteReq {
                zero_downtime: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        deploy(&e, &s, "v1").await;
        // only 100 MB available: cannot run two 256 MB copies
        std::fs::write(
            e.state.cfg.proc_root.join("meminfo"),
            "MemTotal: 1015808 kB\nMemAvailable: 100000 kB\n",
        )
        .unwrap();
        e.runtime.clear_calls();
        deploy(&e, &s, "v2").await;
        let calls = e.runtime.calls();
        assert!(
            pos(&calls, "stop") < pos(&calls, "create"),
            "fell back to stop-first: {calls:?}"
        );
    }

    #[tokio::test]
    async fn queue_is_latest_wins_depth_one() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        // current-thread runtime: the spawned run loop cannot start until we yield, so the
        // next requests deterministically find the slot busy.
        let a = webhook(&e.state, &site_row, "ghcr.io/o/blog:a").unwrap();
        let b = webhook(&e.state, &site_row, "ghcr.io/o/blog:b").unwrap();
        let c = webhook(&e.state, &site_row, "ghcr.io/o/blog:c").unwrap();
        assert!(!a.queued && b.queued && c.queued);
        e.state.deployer.wait_idle(&s.id).await;
        let st = &e.state.store;
        let status = |id: &str| st.get_deployment(id).unwrap().unwrap().status;
        assert_eq!(
            status(&b.deployment_id),
            "superseded",
            "b was replaced before it ever ran"
        );
        // a ran and was superseded by c, c is live
        assert_eq!(status(&a.deployment_id), "superseded");
        assert_eq!(status(&c.deployment_id), "running");
        assert_eq!(e.runtime.containers().len(), 1);
    }

    #[tokio::test]
    async fn rollback_uses_recorded_digest_and_env_snapshot() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        let d1 = e
            .state
            .store
            .get_deployment(&first.deployment_id)
            .unwrap()
            .unwrap();
        // env changes, second version deploys with the new value
        crate::sites::put_env(
            &e.state,
            &s.id,
            vec![EnvVar {
                key: "SECRET".into(),
                value: "v2".into(),
            }],
        )
        .unwrap();
        let second = deploy(&e, &s, "v2").await;
        assert!(e.runtime.containers()[0]
            .spec
            .env
            .contains(&"SECRET=v2".to_string()));
        // rollback to the first deployment
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert!(
            rollback(&e.state, &site_row, &second.deployment_id).is_err(),
            "cannot roll back to the current deployment"
        );
        let rb = rollback(&e.state, &site_row, &first.deployment_id).unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        let r = e
            .state
            .store
            .get_deployment(&rb.deployment_id)
            .unwrap()
            .unwrap();
        assert_eq!(r.trigger, "rollback");
        assert_eq!(r.status, "running");
        assert_eq!(r.image_digest, d1.image_digest, "same immutable image");
        assert!(
            e.runtime.containers()[0]
                .spec
                .env
                .contains(&"SECRET=v1".to_string()),
            "env restored from snapshot"
        );
        // failed deployments and foreign deployments are not valid rollback targets
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert_eq!(
            rollback(&e.state, &site_row, "nope").unwrap_err().status,
            404
        );
    }

    #[tokio::test]
    async fn redeploy_applies_new_env_to_the_same_image() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 256).await;
        let first = deploy(&e, &s, "v1").await;
        let d1 = e
            .state
            .store
            .get_deployment(&first.deployment_id)
            .unwrap()
            .unwrap();
        crate::sites::put_env(
            &e.state,
            &s.id,
            vec![EnvVar {
                key: "SECRET".into(),
                value: "rotated".into(),
            }],
        )
        .unwrap();
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        let acc = redeploy(&e.state, &site_row).unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        let d2 = e
            .state
            .store
            .get_deployment(&acc.deployment_id)
            .unwrap()
            .unwrap();
        assert_eq!(d2.image_digest, d1.image_digest);
        assert!(e.runtime.containers()[0]
            .spec
            .env
            .contains(&"SECRET=rotated".to_string()));
    }

    #[tokio::test]
    async fn webhook_checks_site_binding() {
        let e = TestEnv::new();
        let a = site(&e, "a", 128).await;
        let b = site(&e, "b", 128).await;
        assert!(
            site_for_webhook(&e.state, &a.id, Some("b")).is_err(),
            "token of a cannot name site b"
        );
        assert!(site_for_webhook(&e.state, &a.id, Some("a")).is_ok());
        assert!(site_for_webhook(&e.state, &a.id, None).is_ok());
        // image from another repo is refused even with a valid site
        assert_eq!(
            webhook(&e.state, &b, "ghcr.io/o/a:1").unwrap_err().status,
            403
        );
    }

    #[tokio::test]
    async fn maintenance_blocks_new_deploys() {
        let e = TestEnv::new();
        let s = site(&e, "blog", 128).await;
        e.state
            .maintenance
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let site_row = e.state.store.get_site(&s.id).unwrap().unwrap();
        assert_eq!(
            webhook(&e.state, &site_row, "ghcr.io/o/blog:1")
                .unwrap_err()
                .status,
            503
        );
    }

    #[test]
    fn deployer_slot_semantics() {
        let d = Deployer::default();
        let job = |n: &str| Job {
            deployment_id: n.into(),
            image_ref: "x".into(),
            digest_override: None,
            env_override: None,
        };
        assert!(matches!(d.submit("s", job("1")), Submit::Run));
        assert!(d.is_busy("s"));
        assert!(matches!(
            d.submit("s", job("2")),
            Submit::Queued { replaced: None }
        ));
        match d.submit("s", job("3")) {
            Submit::Queued { replaced: Some(r) } => assert_eq!(r.deployment_id, "2"),
            _ => panic!("expected replacement"),
        }
        assert_eq!(d.next("s").unwrap().deployment_id, "3");
        assert!(d.is_busy("s"), "still running the dequeued job");
        assert!(d.next("s").is_none());
        assert!(!d.is_busy("s"));
        assert!(!d.is_busy("other"));
    }
}
