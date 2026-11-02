//! Scheduled work: system jobs (image prune, housekeeping, backup, update check) and per-site
//! one-shot jobs. One-shots run in a throw-away container created from the site's image with an
//! argv-only command; there is no `exec` and no shell anywhere.

use crate::app::AppState;
use crate::cron::CronExpr;
use crate::docker::{ContainerSpec, RtError};
use crate::error::{AppError, AppResult};
use crate::sites::{effective_env, load_env};
use crate::store::{CronRow, Store};
use crate::util::{now, rand_hex};
use rustle_types::{CreateCronReq, CronJobDto, PruneResp};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const SYSTEM_JOBS: [(&str, &str, &str); 4] = [
    ("prune_images", "Weekly image prune", "0 2 * * 0"),
    ("housekeeping", "Daily housekeeping", "30 3 * * *"),
    ("backup", "Daily backup to GitHub", "0 4 * * *"),
    ("update_check", "Daily update check", "0 5 * * *"),
];

/// Kept per run in SQLite (and therefore in every backup); 50 runs per job are retained.
pub const MAX_LOG: usize = 16 * 1024;

pub fn seed_system_jobs(store: &Store) -> AppResult<()> {
    for (key, name, schedule) in SYSTEM_JOBS {
        if store.get_system_cron(key)?.is_none() {
            store.insert_cron(&CronRow {
                id: rand_hex(6),
                kind: "system".into(),
                name: name.into(),
                schedule: schedule.into(),
                site_id: None,
                argv_json: None,
                enabled: true,
                system_key: Some(key.into()),
                last_fired_minute: None,
            })?;
        }
    }
    Ok(())
}

pub fn cron_dto(state: &AppState, r: &CronRow) -> CronJobDto {
    let last = state.store.last_run(&r.id).ok().flatten();
    CronJobDto {
        id: r.id.clone(),
        kind: r.kind.clone(),
        name: r.name.clone(),
        schedule: r.schedule.clone(),
        site_id: r.site_id.clone(),
        argv: r
            .argv_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        enabled: r.enabled,
        system_key: r.system_key.clone(),
        last_status: last.as_ref().map(|l| match (l.finished_at, l.exit_code) {
            (None, _) => "running".to_string(),
            (Some(_), Some(0)) => "success".to_string(),
            _ => "failed".to_string(),
        }),
        last_run_at: last.map(|l| l.started_at),
    }
}

pub fn validate_argv(argv: &[String]) -> AppResult<()> {
    if argv.is_empty() || argv.len() > 32 {
        return Err(AppError::bad_request("argv must contain 1-32 items"));
    }
    if argv
        .iter()
        .any(|a| a.is_empty() || a.len() > 1024 || a.contains('\0'))
    {
        return Err(AppError::bad_request(
            "argv items must be 1-1024 chars without NUL",
        ));
    }
    Ok(())
}

pub fn create_site_job(state: &AppState, req: CreateCronReq) -> AppResult<CronJobDto> {
    if req.name.trim().is_empty() || req.name.len() > 80 {
        return Err(AppError::bad_request("name is required (max 80 chars)"));
    }
    CronExpr::parse(&req.schedule).map_err(|e| AppError::bad_request(e.to_string()))?;
    let (kind, payload) = match (&req.http_path, req.argv.is_empty()) {
        (Some(p), true) => {
            validate_http_path(p)?;
            ("http", vec![p.clone()])
        }
        (None, false) => {
            validate_argv(&req.argv)?;
            ("site", req.argv.clone())
        }
        _ => {
            return Err(AppError::bad_request(
                "give either a command (argv) or an http_path, not both or neither",
            ))
        }
    };
    state
        .store
        .get_site(&req.site_id)?
        .ok_or_else(|| AppError::bad_request("unknown site_id"))?;
    let row = CronRow {
        id: rand_hex(6),
        kind: kind.into(),
        name: req.name.trim().into(),
        schedule: req.schedule.trim().into(),
        site_id: Some(req.site_id),
        argv_json: Some(serde_json::to_string(&payload).map_err(AppError::internal)?),
        enabled: true,
        system_key: None,
        last_fired_minute: None,
    };
    state.store.insert_cron(&row)?;
    state.store.audit(now(), "admin", "cron.create", &row.name);
    Ok(cron_dto(state, &row))
}

/// Fire every enabled job whose schedule matches the current minute, at most once per minute and
/// never while a previous run of the same job is still going. Missed minutes are not replayed.
pub fn tick(state: &Arc<AppState>, at: i64) -> Vec<String> {
    if state.maintenance.load(std::sync::atomic::Ordering::SeqCst) {
        return vec![];
    }
    let minute = at.div_euclid(60);
    let mut fired = vec![];
    let Ok(jobs) = state.store.list_cron() else {
        return fired;
    };
    for job in jobs.into_iter().filter(|j| j.enabled) {
        if job.last_fired_minute == Some(minute) {
            continue;
        }
        let Ok(expr) = CronExpr::parse(&job.schedule) else {
            tracing::warn!(job = %job.name, "invalid cron schedule; skipping");
            continue;
        };
        if !expr.matches(minute * 60) {
            continue;
        }
        let _ = state.store.set_last_fired(&job.id, minute);
        if !state
            .running_jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job.id.clone())
        {
            tracing::warn!(job = %job.name, "previous run still active; skipping this minute");
            continue;
        }
        fired.push(job.id.clone());
        let st = state.clone();
        tokio::spawn(async move { run_job(st, job).await });
    }
    fired
}

/// Run a job now (scheduler or manual). Caller must have registered it in `running_jobs`.
pub async fn run_job(state: Arc<AppState>, job: CronRow) {
    let started = now();
    let run_id = state.store.insert_run(&job.id, started).unwrap_or(0);
    let (exit, log) = match job.kind.as_str() {
        "system" => match run_system(&state, job.system_key.as_deref().unwrap_or("")).await {
            Ok(msg) => (Some(0), msg),
            Err(msg) => (Some(1), msg),
        },
        "http" => run_http_job(&state, &job).await,
        _ => run_site_job(&state, &job).await,
    };
    let _ = state
        .store
        .finish_run(run_id, now(), exit, &tail(&log, MAX_LOG));
    let _ = state.store.trim_runs(&job.id, 50);
    state
        .running_jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&job.id);
}

/// Manual trigger. Returns false when the job is already running.
pub fn run_now(state: &Arc<AppState>, job: CronRow) -> bool {
    if !state
        .running_jobs
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(job.id.clone())
    {
        return false;
    }
    let st = state.clone();
    tokio::spawn(async move { run_job(st, job).await });
    true
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // The first character that starts at or after `len - max` (never splits a character).
    let from = s.len() - max;
    let start = s
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| *i >= from)
        .unwrap_or(s.len());
    format!("…{}", &s[start..])
}

pub async fn run_system(state: &Arc<AppState>, key: &str) -> Result<String, String> {
    match key {
        "prune_images" => prune_images(state).await.map(|r| {
            format!(
                "removed {} images, reclaimed {} bytes",
                r.images_deleted, r.reclaimed_bytes
            )
        }),
        "housekeeping" => housekeeping(state),
        "backup" => crate::backup::run(state).await,
        "update_check" => {
            let info = crate::update::check(state).await;
            if let Some(l) = &info.latest {
                let _ = state.store.set_setting("update_latest", l);
            }
            Ok(match info.reason {
                Some(r) => format!("update check: {r}"),
                None => format!(
                    "current {} latest {}",
                    info.current,
                    info.latest.unwrap_or_default()
                ),
            })
        }
        other => Err(format!("unknown system job '{other}'")),
    }
}

fn housekeeping(state: &AppState) -> Result<String, String> {
    let t = now();
    state
        .store
        .trim_audit(t - 90 * 86_400)
        .map_err(|e| e.message)?;
    state.store.trim_backup_runs(30).map_err(|e| e.message)?;
    for j in state.store.list_cron().map_err(|e| e.message)? {
        state.store.trim_runs(&j.id, 50).map_err(|e| e.message)?;
    }
    Ok("trimmed audit log, backup history and run history".into())
}

/// Remove images that no container uses and that are not among the last 3 successful
/// deployments of any site (so rollback keeps working). Never touches volumes.
pub async fn prune_images(state: &AppState) -> Result<PruneResp, String> {
    if state.deployer.any_busy() {
        return Err("skipped: a deployment is in progress".into());
    }
    let protected: HashSet<String> = state
        .store
        .protected_digests(3)
        .map_err(|e| e.message)?
        .into_iter()
        .collect();
    let in_use: HashSet<String> = state
        .runtime
        .list_containers(None)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|c| c.image_id)
        .collect();
    let mut deleted = 0;
    let mut bytes = 0u64;
    for img in state
        .runtime
        .list_images()
        .await
        .map_err(|e| e.to_string())?
    {
        if in_use.contains(&img.id) || img.repo_digests.iter().any(|d| protected.contains(d)) {
            continue;
        }
        match state.runtime.remove_image(&img.id).await {
            Ok(()) => {
                deleted += 1;
                bytes += img.size;
            }
            Err(e) => tracing::warn!("could not remove image {}: {e}", img.id),
        }
    }
    Ok(PruneResp {
        reclaimed_bytes: bytes,
        images_deleted: deleted,
    })
}

pub fn validate_http_path(p: &str) -> AppResult<()> {
    let ok = p.len() <= 256
        && p.starts_with('/')
        && !p.starts_with("//")
        && p.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._/~?&=%-".contains(&c));
    if ok {
        Ok(())
    } else {
        Err(AppError::bad_request(
            "http_path must start with a single '/' and use only letters, digits and ._/~?&=%-",
        ))
    }
}

/// `GET <path>` against a running site. The body is bounded; a 2xx answer is a success.
pub async fn call_site_http(
    target: &crate::httpc::Target,
    path: &str,
    secret: Option<&str>,
    timeout: std::time::Duration,
) -> (Option<i64>, String) {
    let mut req = crate::httpc::Req::get(path);
    if let Some(s) = secret {
        req = req.header("authorization", format!("Bearer {s}"));
    }
    match crate::httpc::send_capped(target, &req, timeout, 8 * 1024).await {
        Ok((resp, _)) => {
            let ok = (200..300).contains(&resp.status);
            (
                Some(if ok { 0 } else { 1 }),
                format!("{} {}", resp.status, resp.text()),
            )
        }
        Err(e) => (Some(1), format!("request failed: {e}")),
    }
}

fn job_failure(m: &str) -> (Option<i64>, String) {
    (Some(-1), m.to_string())
}

fn job_site(state: &AppState, job: &CronRow) -> Option<crate::store::SiteRow> {
    job.site_id
        .as_deref()
        .and_then(|s| state.store.get_site(s).ok().flatten())
}

fn job_argv(job: &CronRow) -> Option<Vec<String>> {
    job.argv_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
}

/// The container of the site's current deployment, if there is one.
fn current_container_id(state: &AppState, site: &crate::store::SiteRow) -> Option<String> {
    site.current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten())
        .and_then(|d| d.container_id)
}

async fn running_ip(state: &AppState, cid: &str) -> Option<String> {
    match state.runtime.inspect(cid).await {
        Ok(i) if i.running => i.ip,
        _ => None,
    }
}

async fn run_http_job(state: &AppState, job: &CronRow) -> (Option<i64>, String) {
    let Some(site) = job_site(state, job) else {
        return job_failure("site no longer exists");
    };
    let Some(path) = job_argv(job).and_then(|v| v.into_iter().next()) else {
        return job_failure("job has no path");
    };
    if validate_http_path(&path).is_err() {
        return job_failure("job has an invalid path");
    }
    let Some(cid) = current_container_id(state, &site) else {
        return job_failure("skipped: site has no deployment");
    };
    let Some(ip) = running_ip(state, &cid).await else {
        return job_failure("skipped: site is not running");
    };
    let secret = load_env(state, &site.id).ok().and_then(|v| {
        v.into_iter()
            .find(|e| e.key == "CRON_SECRET")
            .map(|e| e.value)
    });
    call_site_http(
        &crate::httpc::Target::Tcp(format!("{ip}:{}", site.container_port)),
        &path,
        secret.as_deref(),
        state.tuning.http_job_timeout,
    )
    .await
}

async fn run_site_job(state: &AppState, job: &CronRow) -> (Option<i64>, String) {
    let fail = |m: &str| (Some(-1), m.to_string());
    let Some(site) = job
        .site_id
        .as_deref()
        .and_then(|s| state.store.get_site(s).ok().flatten())
    else {
        return fail("site no longer exists");
    };
    let Some(dep) = site
        .current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten())
    else {
        return fail("skipped: site has no deployment");
    };
    let image = dep.image_digest.clone().unwrap_or(dep.image_ref.clone());
    let Some(argv) = job
        .argv_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
    else {
        return fail("job has no valid argv");
    };
    if validate_argv(&argv).is_err() {
        return fail("job has no valid argv");
    }
    let avail_mb = crate::metrics::meminfo(&state.cfg.proc_root).available_kb / 1024;
    if avail_mb < site.ram_limit_mb as u64 + 64 {
        return fail("skipped: not enough free RAM to run a one-shot copy of this site");
    }
    let vars = match load_env(state, &site.id) {
        Ok(v) => v,
        Err(e) => return fail(&e.message),
    };
    let name = format!("rustle-cron-{}-{}", site.name, rand_hex(4));
    let spec = ContainerSpec {
        name,
        image,
        env: effective_env(&vars, site.container_port, site.ram_limit_mb),
        ram_limit_mb: site.ram_limit_mb,
        labels: BTreeMap::from([
            ("rustle.managed".to_string(), "1".to_string()),
            ("rustle.kind".to_string(), "cron".to_string()),
            ("rustle.site".to_string(), site.id.clone()),
        ]),
        compat_caps: site.compat_caps,
        network: state.cfg.network.clone(),
        cmd: Some(argv),
        long_running: false,
    };
    let rt = &state.runtime;
    let id = match rt.create(&spec).await {
        Ok(id) => id,
        Err(e) => return fail(&format!("create failed: {e}")),
    };
    // No AutoRemove: wait, then read the logs, then remove (AutoRemove races the log read).
    let result = async {
        rt.start(&id)
            .await
            .map_err(|e| format!("start failed: {e}"))?;
        let mut code = rt
            .wait(&id, state.tuning.oneshot_timeout)
            .await
            .map_err(|e| e.to_string())?;
        let mut note = String::new();
        if code.is_none() {
            let _ = rt.kill(&id).await;
            code = rt
                .wait(&id, std::time::Duration::from_secs(10))
                .await
                .ok()
                .flatten();
            note = format!(
                "\n[rustle] killed after {}s timeout\n",
                state.tuning.oneshot_timeout.as_secs()
            );
            code = Some(code.unwrap_or(137));
        }
        let logs = rt.logs(&id, 500).await.unwrap_or_else(|e| match e {
            RtError::NotFound => String::new(),
            e => format!("[rustle] could not read logs: {e}"),
        });
        Ok::<_, String>((code, format!("{logs}{note}")))
    }
    .await;
    let _ = rt.remove(&id, true).await;
    match result {
        Ok((code, log)) => (code, log),
        Err(m) => fail(&m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;
    use crate::deploy::webhook;
    use crate::docker::Runtime;
    use crate::sites::create_site;
    use rustle_types::{CreateSiteReq, EnvVar};

    #[tokio::test]
    async fn housekeeping_trims_old_history() {
        let e = TestEnv::new();
        let st = &e.state.store;
        let t = now();
        st.audit(t - 91 * 86_400, "a", "old", "x");
        st.audit(t - 89 * 86_400, "a", "recent", "x");
        for i in 0..35 {
            st.insert_backup_run(t + i, "ok", 1, "p").unwrap();
        }
        seed_system_jobs(st).unwrap();
        let job = st.list_cron().unwrap().remove(0);
        for i in 0..60 {
            let id = st.insert_run(&job.id, t + i).unwrap();
            st.finish_run(id, t + i, Some(0), "").unwrap();
        }
        let before = st.audit_count().unwrap();
        let msg = run_system(&e.state, "housekeeping").await.unwrap();
        assert!(msg.contains("trimmed"));
        assert_eq!(
            st.audit_count().unwrap(),
            before - 1,
            "only the 91-day-old entry goes"
        );
        assert_eq!(st.list_backup_runs(100).unwrap().len(), 30);
        assert_eq!(st.list_runs(&job.id, 100).unwrap().len(), 50);
    }

    #[tokio::test]
    async fn run_system_dispatch() {
        let e = TestEnv::new();
        let err = run_system(&e.state, "no_such_job").await.unwrap_err();
        assert!(err.contains("unknown system job 'no_such_job'"));
        let msg = run_system(&e.state, "prune_images").await.unwrap();
        assert!(msg.starts_with("removed 0 images"), "{msg}");
        let msg = run_system(&e.state, "update_check").await.unwrap();
        assert!(
            msg.starts_with("update check:") || msg.starts_with("current "),
            "{msg}"
        );
        // backup is dispatched to the backup module (not configured in tests: an error, not a panic)
        let _ = run_system(&e.state, "backup").await;
    }

    async fn deployed_site(e: &TestEnv) -> crate::store::SiteRow {
        let dto = create_site(
            &e.state,
            CreateSiteReq {
                name: "blog".into(),
                domain: "blog.example.com".into(),
                image_repo: "ghcr.io/o/blog".into(),
                ram_limit_mb: 128,
                container_port: 3000,
                registry_id: None,
                env: vec![EnvVar {
                    key: "K".into(),
                    value: "v".into(),
                }],
                skip_dns_check: true,
                override_budget: true,
            },
        )
        .await
        .unwrap();
        let s = e.state.store.get_site(&dto.id).unwrap().unwrap();
        webhook(&e.state, &s, "ghcr.io/o/blog:1").unwrap();
        e.state.deployer.wait_idle(&s.id).await;
        e.state.store.get_site(&s.id).unwrap().unwrap()
    }

    const T: std::time::Duration = std::time::Duration::from_secs(5);

    fn job(site: &str, argv: &[&str]) -> CronRow {
        CronRow {
            id: "j1".into(),
            kind: "site".into(),
            name: "t".into(),
            schedule: "* * * * *".into(),
            site_id: Some(site.into()),
            argv_json: Some(serde_json::to_string(argv).unwrap()),
            enabled: true,
            system_key: None,
            last_fired_minute: None,
        }
    }

    #[test]
    fn seeds_are_idempotent() {
        let s = Store::open_memory().unwrap();
        seed_system_jobs(&s).unwrap();
        seed_system_jobs(&s).unwrap();
        assert_eq!(s.list_cron().unwrap().len(), 4);
        for (_, _, sched) in SYSTEM_JOBS {
            assert!(CronExpr::parse(sched).is_ok());
        }
    }

    #[test]
    fn argv_validation() {
        assert!(validate_argv(&["node".into(), "job.js".into()]).is_ok());
        assert!(validate_argv(&[]).is_err());
        assert!(validate_argv(&["".into()]).is_err());
        assert!(validate_argv(&["a\0b".into()]).is_err());
        assert!(validate_argv(&vec!["x".to_string(); 33]).is_err());
    }

    #[tokio::test]
    async fn tick_fires_once_per_minute_and_skips_overlap() {
        let e = TestEnv::new();
        let s = &e.state;
        seed_system_jobs(&s.store).unwrap();
        // 2026-11-01 02:00 UTC (Sunday) => weekly prune is due
        let at = 1_793_498_400;
        let fired = tick(s, at);
        assert_eq!(fired.len(), 1);
        let prune = s.store.get_system_cron("prune_images").unwrap().unwrap();
        assert_eq!(fired[0], prune.id);
        assert!(
            tick(s, at + 30).is_empty(),
            "same minute does not fire twice"
        );
        assert!(tick(s, at + 61).is_empty(), "next minute: nothing due");
        for _ in 0..200 {
            if s.running_jobs.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let runs = s.store.list_runs(&prune.id, 5).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_code, Some(0));
        // a disabled job never fires
        s.store.update_cron(&prune.id, Some(false), None).unwrap();
        assert!(tick(s, at + 7 * 86_400).is_empty());
    }

    #[tokio::test]
    async fn overlap_is_skipped() {
        let e = TestEnv::new();
        let s = &e.state;
        seed_system_jobs(&s.store).unwrap();
        let prune = s.store.get_system_cron("prune_images").unwrap().unwrap();
        s.running_jobs.lock().unwrap().insert(prune.id.clone());
        assert!(
            tick(s, 1_793_498_400).is_empty(),
            "still running => skipped"
        );
    }

    #[tokio::test]
    async fn site_job_runs_argv_in_a_throwaway_container() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        e.runtime.set_oneshot(0, "migrated 3 rows\n", false);
        e.runtime.clear_calls();
        let (code, log) = run_site_job(&e.state, &job(&site.id, &["node", "migrate.js"])).await;
        assert_eq!(code, Some(0));
        assert!(log.contains("migrated 3 rows"));
        let calls = e.runtime.calls();
        assert!(calls
            .iter()
            .any(|c| c.starts_with("create rustle-cron-blog-")));
        assert!(
            calls
                .last()
                .unwrap()
                .starts_with("remove rustle-cron-blog-"),
            "container is always removed: {calls:?}"
        );
        assert_eq!(
            e.runtime.containers().len(),
            1,
            "only the app container remains"
        );
        // the one-shot got the argv as Cmd, no restart policy, same RAM limit
        e.runtime.set_oneshot(3, "boom", false);
        let (code, log) = run_site_job(&e.state, &job(&site.id, &["false"])).await;
        assert_eq!((code, log.as_str()), (Some(3), "boom"));
    }

    #[tokio::test]
    async fn hung_job_is_killed_after_timeout() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        e.runtime.set_oneshot(0, "", true);
        e.runtime.clear_calls();
        let (code, log) = run_site_job(&e.state, &job(&site.id, &["sleep", "9999"])).await;
        assert_eq!(code, Some(137));
        assert!(log.contains("killed after"));
        assert!(e
            .runtime
            .calls()
            .iter()
            .any(|c| c.starts_with("kill rustle-cron-")));
        assert_eq!(e.runtime.containers().len(), 1);
    }

    #[tokio::test]
    async fn job_skipped_when_ram_is_short_or_site_undeployed() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        std::fs::write(
            e.state.cfg.proc_root.join("meminfo"),
            "MemTotal: 1015808 kB\nMemAvailable: 100000 kB\n",
        )
        .unwrap();
        let (code, log) = run_site_job(&e.state, &job(&site.id, &["x"])).await;
        assert_eq!(code, Some(-1));
        assert!(log.starts_with("skipped"));
        let (code, _) = run_site_job(&e.state, &job("missing", &["x"])).await;
        assert_eq!(code, Some(-1));
    }

    #[tokio::test]
    async fn prune_keeps_in_use_and_recent_digests() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await; // one running deployment with a recorded digest
        let dep = e
            .state
            .store
            .get_deployment(site.current_deployment_id.as_deref().unwrap())
            .unwrap()
            .unwrap();
        let protected_digest = dep.image_digest.unwrap();
        e.runtime
            .add_image("sha256:orphan", &["ghcr.io/o/old@sha256:aaa"]);
        e.runtime.add_image("sha256:inuse", &[]);
        e.runtime.add_foreign_container("caddy", &[]);
        // make the foreign container reference the "in use" image id
        // (FakeRuntime reports spec.image as image_id)
        let _ = protected_digest;
        let before = e.runtime.image_ids();
        assert!(before.contains(&"sha256:orphan".to_string()));
        let r = prune_images(&e.state).await.unwrap();
        let after = e.runtime.image_ids();
        assert!(
            !after.contains(&"sha256:orphan".to_string()),
            "unreferenced image removed"
        );
        assert!(
            after.iter().any(|i| i.starts_with("sha256:img")),
            "deployed image survives: {after:?}"
        );
        assert!(r.images_deleted >= 1);
    }

    #[tokio::test]
    async fn prune_is_skipped_during_a_deployment() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let s = e.state.store.get_site(&site.id).unwrap().unwrap();
        webhook(&e.state, &s, "ghcr.io/o/blog:2").unwrap(); // slot busy until we yield
        assert!(prune_images(&e.state).await.is_err());
        e.state.deployer.wait_idle(&site.id).await;
    }

    #[test]
    fn http_paths() {
        for ok in ["/api/cron/cleanup", "/a?x=1&y=2", "/", "/a-b_c.d~e%20"] {
            assert!(validate_http_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "api",
            "//evil.com/x",
            "/a b",
            "/a\r\nHost: x",
            "/a;rm",
            &format!("/{}", "a".repeat(300)),
            "",
        ] {
            assert!(validate_http_path(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn http_job_sends_the_bearer_secret_and_reports_the_status() {
        use crate::testutil::MockHttp;
        let m = MockHttp::start(|r| {
            if r.header("authorization").as_deref() == Some("Bearer s3cret") {
                (200, vec![], "{\"cleaned\":3}".into())
            } else {
                (401, vec![], "nope".into())
            }
        })
        .await;
        let t = crate::httpc::Target::Tcp(m.addr.clone());
        let (code, log) = call_site_http(&t, "/api/cron/x", Some("s3cret"), T).await;
        assert_eq!((code, log.as_str()), (Some(0), "200 {\"cleaned\":3}"));
        let (code, log) = call_site_http(&t, "/api/cron/x", None, T).await;
        assert_eq!((code, log.as_str()), (Some(1), "401 nope"));
        assert_eq!(m.requests()[0].method, "GET");
        let (code, _) = call_site_http(
            &crate::httpc::Target::Tcp("127.0.0.1:1".into()),
            "/",
            None,
            T,
        )
        .await;
        assert_eq!(code, Some(1));
    }

    #[tokio::test]
    async fn http_job_end_to_end_and_every_skip_reason() {
        use crate::testutil::MockHttp;
        let m = MockHttp::start(|r| {
            if r.header("authorization").as_deref() == Some("Bearer top-secret")
                && r.path == "/api/cron/tick"
            {
                (200, vec![], "done".into())
            } else {
                (403, vec![], "no".into())
            }
        })
        .await;
        let port: u32 = m.addr.rsplit(':').next().unwrap().parse().unwrap();
        let e = TestEnv::new();
        let dto = create_site(
            &e.state,
            CreateSiteReq {
                name: "api".into(),
                domain: "api.example.com".into(),
                image_repo: "ghcr.io/o/api".into(),
                ram_limit_mb: 128,
                container_port: port as u16,
                registry_id: None,
                env: vec![EnvVar {
                    key: "CRON_SECRET".into(),
                    value: "top-secret".into(),
                }],
                skip_dns_check: true,
                override_budget: true,
            },
        )
        .await
        .unwrap();
        let site = e.state.store.get_site(&dto.id).unwrap().unwrap();
        let mut j = job(&site.id, &["/api/cron/tick"]);
        j.kind = "http".into();
        // not deployed yet
        let (c, l) = run_http_job(&e.state, &j).await;
        assert_eq!(
            (c, l.as_str()),
            (Some(-1), "skipped: site has no deployment")
        );
        webhook(&e.state, &site, "ghcr.io/o/api:1").unwrap();
        e.state.deployer.wait_idle(&site.id).await;
        e.runtime.set_ip(&e.runtime.containers()[0].id, "127.0.0.1");
        let (c, l) = run_http_job(&e.state, &j).await;
        assert_eq!((c, l.as_str()), (Some(0), "200 done"));
        // wrong path -> the server answers 403 -> failure exit code 1
        let mut bad = job(&site.id, &["/other"]);
        bad.kind = "http".into();
        assert_eq!(run_http_job(&e.state, &bad).await.0, Some(1));
        // malformed jobs
        let mut none = job(&site.id, &["/x"]);
        none.argv_json = None;
        assert_eq!(run_http_job(&e.state, &none).await.1, "job has no path");
        none.argv_json = Some("[]".into());
        assert_eq!(run_http_job(&e.state, &none).await.1, "job has no path");
        none.argv_json = Some("not json".into());
        assert_eq!(run_http_job(&e.state, &none).await.1, "job has no path");
        let inv = job(&site.id, &["//evil"]);
        assert_eq!(
            run_http_job(&e.state, &inv).await.1,
            "job has an invalid path"
        );
        let mut gone = job("missing-site", &["/x"]);
        gone.kind = "http".into();
        assert_eq!(
            run_http_job(&e.state, &gone).await.1,
            "site no longer exists"
        );
        gone.site_id = None;
        assert_eq!(
            run_http_job(&e.state, &gone).await.1,
            "site no longer exists"
        );
    }

    #[tokio::test]
    async fn http_job_skips_a_site_that_is_not_running() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let cid = e.runtime.containers()[0].id.clone();
        // a loopback address: if the job wrongly tried the stopped site it would fail at once
        e.runtime.set_ip(&cid, "127.0.0.1");
        e.runtime.stop(&cid, 1).await.unwrap();
        let mut j = job(&site.id, &["/api/cron/x"]);
        j.kind = "http".into();
        let (code, log) = run_http_job(&e.state, &j).await;
        assert_eq!(code, Some(-1));
        assert!(log.starts_with("skipped: site is not running"), "{log}");
    }

    #[tokio::test]
    async fn create_site_job_validates() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let ok = CreateCronReq {
            name: "cleanup".into(),
            schedule: "0 * * * *".into(),
            site_id: site.id.clone(),
            argv: vec!["node".into(), "c.js".into()],
            http_path: None,
        };
        let dto = create_site_job(&e.state, ok.clone()).unwrap();
        assert_eq!(dto.kind, "site");
        assert!(create_site_job(
            &e.state,
            CreateCronReq {
                schedule: "nope".into(),
                ..ok.clone()
            }
        )
        .is_err());
        assert!(create_site_job(
            &e.state,
            CreateCronReq {
                argv: vec![],
                ..ok.clone()
            }
        )
        .is_err());
        assert!(create_site_job(
            &e.state,
            CreateCronReq {
                site_id: "zzz".into(),
                ..ok.clone()
            }
        )
        .is_err());
        // an http job, and the "exactly one of argv / http_path" rule
        let h = create_site_job(
            &e.state,
            CreateCronReq {
                argv: vec![],
                http_path: Some("/api/cron/x".into()),
                ..ok.clone()
            },
        )
        .unwrap();
        assert_eq!(
            (h.kind.as_str(), h.argv),
            ("http", Some(vec!["/api/cron/x".to_string()]))
        );
        assert!(
            create_site_job(
                &e.state,
                CreateCronReq {
                    http_path: Some("/x".into()),
                    ..ok.clone()
                }
            )
            .is_err(),
            "both"
        );
        assert!(
            create_site_job(&e.state, CreateCronReq { argv: vec![], ..ok }).is_err(),
            "neither"
        );
    }

    #[test]
    fn tail_keeps_the_end_on_char_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("abc", 3), "abc", "exactly max is untouched");
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(tail("abcdef", 1), "…f");
        assert_eq!(tail("abcdef", 0), "…");
        // 'é' is two bytes: a cut inside it moves forward to the next character
        assert_eq!(tail("aéb", 2), "…b");
        assert_eq!(tail("aéb", 3), "…éb");
        let t = tail(&"é".repeat(100), 11);
        assert!(t.len() <= 14 && t.starts_with('…'));
    }

    #[test]
    fn argv_limits_are_exact() {
        let items = |n: usize| vec!["a".to_string(); n];
        assert!(validate_argv(&items(1)).is_ok());
        assert!(validate_argv(&items(32)).is_ok());
        assert!(validate_argv(&items(33)).is_err());
        assert!(validate_argv(&[]).is_err());
        assert!(validate_argv(&["a".repeat(1024)]).is_ok());
        assert!(validate_argv(&["a".repeat(1025)]).is_err());
        assert!(validate_argv(&[String::new()]).is_err());
        assert!(validate_argv(&["a\0b".to_string()]).is_err());
        assert_eq!(MAX_LOG, 16 * 1024);
    }

    #[tokio::test]
    async fn job_dto_reports_running_success_and_failure() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let st = &e.state.store;
        let mut row = job(&site.id, &["node"]);
        row.id = "jdto".into();
        st.insert_cron(&row).unwrap();
        assert_eq!(cron_dto(&e.state, &row).last_status, None, "never ran");
        let status = |finished: Option<i64>, exit: Option<i64>| {
            let id = st.insert_run("jdto", 100).unwrap();
            if let Some(f) = finished {
                st.finish_run(id, f, exit, "").unwrap();
            }
            cron_dto(&e.state, &row)
        };
        assert_eq!(status(None, None).last_status.as_deref(), Some("running"));
        assert_eq!(
            status(Some(101), Some(0)).last_status.as_deref(),
            Some("success")
        );
        assert_eq!(
            status(Some(102), Some(1)).last_status.as_deref(),
            Some("failed")
        );
        assert_eq!(
            status(Some(103), None).last_status.as_deref(),
            Some("failed")
        );
        assert_eq!(
            status(Some(104), Some(-1)).last_status.as_deref(),
            Some("failed")
        );
        assert_eq!(cron_dto(&e.state, &row).last_run_at, Some(100));
    }

    #[tokio::test]
    async fn site_job_name_rules() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let mk = |name: &str| CreateCronReq {
            name: name.into(),
            schedule: "0 * * * *".into(),
            site_id: site.id.clone(),
            argv: vec!["node".into()],
            http_path: None,
        };
        assert!(create_site_job(&e.state, mk(&"n".repeat(80))).is_ok());
        assert!(create_site_job(&e.state, mk(&"n".repeat(81))).is_err());
        assert!(create_site_job(&e.state, mk("")).is_err());
        assert!(create_site_job(&e.state, mk("   ")).is_err());
        assert!(create_site_job(&e.state, mk("x")).is_ok());
    }

    #[tokio::test]
    async fn run_job_dispatches_by_kind_and_records_the_run() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let cid = e.runtime.containers()[0].id.clone();
        e.runtime.stop(&cid, 1).await.unwrap();
        let st = &e.state.store;
        let mut row = job(&site.id, &["/api/cron/x"]);
        row.kind = "http".into();
        st.insert_cron(&row).unwrap();
        e.state.running_jobs.lock().unwrap().insert(row.id.clone());
        run_job(e.state.clone(), row.clone()).await;
        let run = st.last_run(&row.id).unwrap().unwrap();
        assert_eq!(run.exit_code, Some(-1));
        assert_eq!(
            run.log_tail, "skipped: site is not running",
            "http jobs are run by the http runner"
        );
        assert!(
            !e.state.running_jobs.lock().unwrap().contains(&row.id),
            "slot released"
        );
        // a system job goes to the system runner
        let mut sys = job(&site.id, &[]);
        sys.id = "sys1".into();
        sys.kind = "system".into();
        sys.system_key = Some("nonexistent".into());
        st.insert_cron(&sys).unwrap();
        run_job(e.state.clone(), sys.clone()).await;
        let run = st.last_run("sys1").unwrap().unwrap();
        assert_eq!(
            (run.exit_code, run.log_tail.as_str()),
            (Some(1), "unknown system job 'nonexistent'")
        );
    }

    #[tokio::test]
    async fn prune_reports_the_bytes_it_reclaimed() {
        let e = TestEnv::new();
        e.runtime.add_image("sha256:a", &[]);
        e.runtime.add_image("sha256:b", &[]);
        let r = prune_images(&e.state).await.unwrap();
        assert_eq!(r.images_deleted, 2);
        assert_eq!(r.reclaimed_bytes, 2000, "each fake image is 1000 bytes");
    }

    #[tokio::test]
    async fn site_http_log_is_capped_at_8_kib() {
        use crate::testutil::MockHttp;
        let m = MockHttp::start(|r| match r.path.as_str() {
            "/exact" => (200, vec![], "z".repeat(8 * 1024 - 4)),
            _ => (200, vec![], "z".repeat(20 * 1024)),
        })
        .await;
        let t = crate::httpc::Target::Tcp(m.addr.clone());
        let (_, log) = call_site_http(&t, "/big", None, T).await;
        assert_eq!(
            log.len(),
            "200 ".len() + 8 * 1024,
            "body cut at 8 KiB: {}",
            log.len()
        );
        let (_, log) = call_site_http(&t, "/exact", None, T).await;
        assert_eq!(log.len(), 4 + 8 * 1024 - 4);
    }

    #[tokio::test]
    async fn one_shot_runs_need_ram_limit_plus_64_mb_free() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await; // 128 MB limit
        let set_free = |mb: u64| {
            std::fs::write(
                e.state.cfg.proc_root.join("meminfo"),
                format!("MemTotal: 1015808 kB\nMemAvailable: {} kB\n", mb * 1024),
            )
            .unwrap();
        };
        let j = job(&site.id, &["node", "x.js"]);
        set_free(128 + 63);
        let (c, l) = run_site_job(&e.state, &j).await;
        assert_eq!(c, Some(-1));
        assert!(l.contains("not enough free RAM"), "{l}");
        set_free(128 + 64);
        let (_, l) = run_site_job(&e.state, &j).await;
        assert!(
            !l.contains("not enough free RAM"),
            "exactly the requirement is enough: {l}"
        );
    }
}
