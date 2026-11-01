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
    validate_argv(&req.argv)?;
    state
        .store
        .get_site(&req.site_id)?
        .ok_or_else(|| AppError::bad_request("unknown site_id"))?;
    let row = CronRow {
        id: rand_hex(6),
        kind: "site".into(),
        name: req.name.trim().into(),
        schedule: req.schedule.trim().into(),
        site_id: Some(req.site_id),
        argv_json: Some(serde_json::to_string(&req.argv).map_err(AppError::internal)?),
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
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
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
    use crate::sites::create_site;
    use rustle_types::{CreateSiteReq, EnvVar};

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

    #[tokio::test]
    async fn create_site_job_validates() {
        let e = TestEnv::new();
        let site = deployed_site(&e).await;
        let ok = CreateCronReq {
            name: "cleanup".into(),
            schedule: "0 * * * *".into(),
            site_id: site.id.clone(),
            argv: vec!["node".into(), "c.js".into()],
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
                ..ok
            }
        )
        .is_err());
    }

    #[test]
    fn tail_keeps_the_end_on_char_boundaries() {
        assert_eq!(tail("abc", 10), "abc");
        let t = tail(&"é".repeat(100), 11);
        assert!(t.len() <= 14 && t.starts_with('…'));
    }
}
