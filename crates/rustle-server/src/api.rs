//! HTTP API (`/api/v1`). Handlers are thin: validation and behaviour live in the service modules.

use crate::app::AppState;
use crate::auth::{self, clear_cookie, client_ip, session_cookie, Admin};
use crate::crypto;
use crate::error::{AppError, AppResult};
use crate::sites::{self, Action};
use crate::util::{now, valid_domain};
use crate::{assets, backup, deploy, jobs, metrics, update};
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::map_response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rustle_types::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

type S = Arc<AppState>;

/// JSON body extractor whose rejections use the API error format.
pub struct ApiJson<T>(pub T);

impl<T, St> FromRequest<St> for ApiJson<T>
where
    T: DeserializeOwned,
    St: Send + Sync,
{
    type Rejection = AppError;
    async fn from_request(req: Request, state: &St) -> Result<Self, AppError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(AppError::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", "request body is too large (max 64 KB)"))
            }
            Err(e) => Err(AppError::bad_request(format!("invalid request body: {}", e.body_text()))),
        }
    }
}

async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

pub fn router(state: S) -> Router {
    let api = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/password", post(change_password))
        .route("/system/stats", get(stats))
        .route("/system/stream", get(stats_stream))
        .route("/system/prune", post(prune))
        .route("/system/settings", get(get_settings).put(put_settings))
        .route("/system/backup", get(get_backup).put(put_backup))
        .route("/system/backup/run", post(run_backup))
        .route("/system/backup/runs", get(backup_runs))
        .route("/system/update", get(get_update).post(apply_update))
        .route("/sites", get(list_sites).post(create_site))
        .route("/sites/{id}", get(get_site).patch(patch_site).delete(delete_site))
        .route("/sites/{id}/start", post(|s, a, p| site_action(s, a, p, Action::Start)))
        .route("/sites/{id}/stop", post(|s, a, p| site_action(s, a, p, Action::Stop)))
        .route("/sites/{id}/restart", post(|s, a, p| site_action(s, a, p, Action::Restart)))
        .route("/sites/{id}/redeploy", post(redeploy))
        .route("/sites/{id}/logs", get(site_logs))
        .route("/sites/{id}/logs/stream", get(site_logs_stream))
        .route("/sites/{id}/env", get(get_env).put(put_env))
        .route("/sites/{id}/tokens", get(list_tokens).post(create_token))
        .route("/sites/{id}/tokens/{tid}", delete(revoke_token))
        .route("/sites/{id}/workflow", get(workflow))
        .route("/sites/{id}/deployments", get(list_deployments))
        .route("/sites/{id}/rollback", post(rollback))
        .route("/webhooks/deploy", post(webhook))
        .route("/cron", get(list_cron).post(create_cron))
        .route("/cron/{id}", patch(patch_cron).delete(delete_cron))
        .route("/cron/{id}/runs", get(cron_runs))
        .route("/cron/{id}/run", post(run_cron))
        .route("/registries", get(list_registries).post(create_registry))
        .route("/registries/{id}", delete(delete_registry));
    Router::new()
        .nest("/api/v1", api)
        .route("/healthz", get(|| async { "ok" }))
        .fallback(assets::serve)
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(map_response(security_headers))
        .with_state(state)
}

fn audit(s: &S, action: &str, target: &str) {
    s.store.audit(now(), "admin", action, target);
}

// ---- auth -----------------------------------------------------------------------------------

async fn login(State(s): State<S>, headers: HeaderMap, ApiJson(req): ApiJson<LoginReq>) -> AppResult<Response> {
    let ip = client_ip(&headers);
    let at = now();
    let wait = s.auth.login_wait(&ip, at);
    if wait > 0 {
        let mut r = AppError::too_many(format!("too many failed attempts; retry in {wait}s")).into_response();
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
        return Ok(r);
    }
    let (user, hash, tv) = s.store.admin()?.ok_or_else(|| AppError::unavailable("not initialized: run `rustle init` on the server"))?;
    if req.password.len() > 512 || req.username.len() > 128 {
        return Err(AppError::bad_request("credentials too long"));
    }
    // Always run the hash (even for a wrong username) so timing does not reveal the username.
    let _permit = s.auth.hash_gate.acquire().await.map_err(AppError::internal)?;
    let pw = req.password.clone();
    let ok_pw = tokio::task::spawn_blocking(move || crypto::verify_password(&pw, &hash)).await.map_err(AppError::internal)?;
    let ok_user = crypto::ct_eq(req.username.as_bytes(), user.as_bytes());
    drop(_permit);
    if !(ok_pw && ok_user) {
        s.auth.login_failed(&ip, at);
        s.store.audit(at, &ip, "login.failed", "-");
        return Err(AppError::unauthorized("invalid username or password"));
    }
    s.auth.login_ok(&ip);
    let (token, csrf) = s.auth.issue(tv, at);
    s.store.audit(at, &ip, "login", &user);
    let mut r = Json(LoginResp { username: user, csrf }).into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&session_cookie(&token)).map_err(AppError::internal)?);
    Ok(r)
}

async fn logout(_a: Admin) -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&clear_cookie()).expect("static cookie"));
    r
}

async fn me(State(s): State<S>, a: Admin) -> AppResult<Json<LoginResp>> {
    let (user, _, _) = s.store.admin()?.ok_or_else(|| AppError::unauthorized("not initialized"))?;
    Ok(Json(LoginResp { username: user, csrf: a.csrf }))
}

async fn change_password(State(s): State<S>, _a: Admin, ApiJson(req): ApiJson<ChangePasswordReq>) -> AppResult<Response> {
    if req.new.chars().count() < 12 || req.new.len() > 512 {
        return Err(AppError::bad_request("the new password must have at least 12 characters"));
    }
    let (_, hash, _) = s.store.admin()?.ok_or_else(|| AppError::unauthorized("not initialized"))?;
    let _permit = s.auth.hash_gate.acquire().await.map_err(AppError::internal)?;
    let cur = req.current.clone();
    if !tokio::task::spawn_blocking(move || crypto::verify_password(&cur, &hash)).await.map_err(AppError::internal)? {
        return Err(AppError::forbidden("current password is incorrect"));
    }
    let new = req.new.clone();
    let new_hash = tokio::task::spawn_blocking(move || crypto::hash_password(&new)).await.map_err(AppError::internal)?;
    drop(_permit);
    s.store.set_admin_password(&new_hash)?; // bumps token_version: every session, including this one, is revoked
    audit(&s, "password.change", "admin");
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&clear_cookie()).expect("static cookie"));
    Ok(r)
}

// ---- system ---------------------------------------------------------------------------------

pub fn collect_stats(s: &AppState) -> SystemStats {
    let mut st = SystemStats::default();
    metrics::fill_system_stats(&mut st, &s.cfg.proc_root, &s.cfg.state_dir);
    st.cpu_percent = s.sampler.lock().unwrap_or_else(|p| p.into_inner()).cpu_percent(&s.cfg.proc_root);
    let (allocated, budget) = sites::budget(s);
    st.allocated_mb = allocated as u64;
    st.budget_mb = budget as u64;
    let degraded = s.degraded();
    st.runtime_ok = degraded.as_deref().is_none_or(|d| !d.contains("runtime"));
    st.gateway_ok = degraded.as_deref().is_none_or(|d| !d.contains("gateway"));
    st.degraded_reason = degraded;
    st.public_ip_mode = sites::admin_domain(s).is_none();
    st
}

async fn stats(State(s): State<S>, _a: Admin) -> Json<SystemStats> {
    Json(collect_stats(&s))
}

struct SubGuard(S);
impl Drop for SubGuard {
    fn drop(&mut self) {
        self.0.stats_subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Stats are sampled only while at least one dashboard is open.
async fn stats_stream(State(s): State<S>, _a: Admin) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(4);
    s.stats_subscribers.fetch_add(1, Ordering::SeqCst);
    let guard = SubGuard(s.clone());
    tokio::spawn(async move {
        let _guard = guard;
        loop {
            let st = collect_stats(&_guard.0);
            let ev = Event::default().event("stats").json_data(&st).unwrap_or_default();
            if tx.send(Ok(ev)).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

async fn prune(State(s): State<S>, _a: Admin) -> AppResult<Json<PruneResp>> {
    audit(&s, "system.prune", "images");
    jobs::prune_images(&s).await.map(Json).map_err(|m| AppError::conflict(m))
}

async fn get_settings(State(s): State<S>, _a: Admin) -> AppResult<Json<SettingsDto>> {
    Ok(Json(SettingsDto {
        admin_domain: sites::admin_domain(&s).unwrap_or_default(),
        acme_email: s.store.setting("acme_email")?.unwrap_or_default(),
        budget_override_mb: s.store.setting("budget_override_mb")?.and_then(|v| v.parse().ok()),
    }))
}

async fn put_settings(State(s): State<S>, _a: Admin, ApiJson(req): ApiJson<SettingsDto>) -> AppResult<Json<SettingsDto>> {
    let domain = req.admin_domain.trim().to_ascii_lowercase();
    if !domain.is_empty() {
        if !valid_domain(&domain) {
            return Err(AppError::bad_request("invalid admin domain"));
        }
        if s.store.list_sites()?.iter().any(|x| x.domain == domain) {
            return Err(AppError::conflict("that domain is used by a site"));
        }
    }
    let email = req.acme_email.trim().to_string();
    if !email.is_empty() && (!email.contains('@') || email.len() > 200 || email.contains(char::is_whitespace)) {
        return Err(AppError::bad_request("invalid email"));
    }
    s.store.set_setting("admin_domain", &domain)?;
    s.store.set_setting("acme_email", &email)?;
    match req.budget_override_mb {
        Some(v) if (64..=65536).contains(&v) => s.store.set_setting("budget_override_mb", &v.to_string())?,
        Some(_) => return Err(AppError::bad_request("budget_override_mb must be 64-65536")),
        None => s.store.delete_setting("budget_override_mb")?,
    }
    if let Err(e) = s.gateway.set_admin(&crate::reconcile::base_params(&s)).await {
        return Err(AppError::unavailable(format!("saved, but the gateway could not be updated yet: {e}")));
    }
    audit(&s, "settings.update", &domain);
    get_settings(State(s), _a).await
}

async fn get_backup(State(s): State<S>, _a: Admin) -> Json<BackupConfig> {
    Json(backup::get_config(&s))
}

async fn put_backup(State(s): State<S>, _a: Admin, ApiJson(cfg): ApiJson<BackupConfig>) -> AppResult<Json<BackupConfig>> {
    backup::set_config(&s, cfg).await?;
    Ok(Json(backup::get_config(&s)))
}

async fn run_backup(State(s): State<S>, _a: Admin) -> AppResult<Json<serde_json::Value>> {
    let msg = backup::run(&s).await.map_err(|e| AppError::unprocessable("backup_failed", e))?;
    Ok(Json(serde_json::json!({ "message": msg })))
}

async fn backup_runs(State(s): State<S>, _a: Admin) -> AppResult<Json<Vec<BackupRunDto>>> {
    Ok(Json(backup::history(&s)?))
}

async fn get_update(State(s): State<S>, _a: Admin) -> Json<UpdateInfo> {
    Json(update::check(&s).await)
}

async fn apply_update(State(s): State<S>, _a: Admin) -> AppResult<Json<serde_json::Value>> {
    let tag = update::apply(&s).await?;
    // Give the response time to flush, then exit; systemd (Restart=always) starts the new binary.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        std::process::exit(0);
    });
    Ok(Json(serde_json::json!({ "installed": tag, "restarting": true })))
}

// ---- sites ----------------------------------------------------------------------------------

async fn list_sites(State(s): State<S>, _a: Admin) -> AppResult<Json<Vec<SiteDto>>> {
    Ok(Json(s.store.list_sites()?.iter().map(|r| sites::site_dto(&s, r)).collect()))
}

fn load_site(s: &S, id: &str) -> AppResult<crate::store::SiteRow> {
    s.store.get_site(id)?.ok_or_else(|| AppError::not_found("site not found"))
}

async fn create_site(State(s): State<S>, _a: Admin, ApiJson(req): ApiJson<CreateSiteReq>) -> AppResult<(StatusCode, Json<SiteDto>)> {
    Ok((StatusCode::CREATED, Json(sites::create_site(&s, req).await?)))
}

async fn get_site(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<Json<SiteDto>> {
    Ok(Json(sites::site_dto(&s, &load_site(&s, &id)?)))
}

async fn patch_site(State(s): State<S>, _a: Admin, Path(id): Path<String>, ApiJson(req): ApiJson<UpdateSiteReq>) -> AppResult<Json<SiteDto>> {
    Ok(Json(sites::update_site(&s, &id, req).await?))
}

async fn delete_site(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<StatusCode> {
    sites::delete_site(&s, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn site_action(State(s): State<S>, _a: Admin, Path(id): Path<String>, action: Action) -> AppResult<Json<SiteDto>> {
    Ok(Json(sites::site_action(&s, &id, action).await?))
}

async fn redeploy(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<(StatusCode, Json<DeployAccepted>)> {
    let site = load_site(&s, &id)?;
    Ok((StatusCode::ACCEPTED, Json(deploy::redeploy(&s, &site)?)))
}

#[derive(Deserialize)]
struct TailQ {
    tail: Option<u32>,
}

fn current_container(s: &S, id: &str) -> AppResult<String> {
    let site = load_site(s, id)?;
    site.current_deployment_id
        .as_deref()
        .and_then(|d| s.store.get_deployment(d).ok().flatten())
        .and_then(|d| d.container_id)
        .ok_or_else(|| AppError::conflict("site has no running container yet"))
}

async fn site_logs(State(s): State<S>, _a: Admin, Path(id): Path<String>, Query(q): Query<TailQ>) -> AppResult<Json<serde_json::Value>> {
    let cid = current_container(&s, &id)?;
    let logs = s.runtime.logs(&cid, q.tail.unwrap_or(200).min(2000)).await.map_err(|e| AppError::unavailable(e.to_string()))?;
    Ok(Json(serde_json::json!({ "logs": logs })))
}

async fn site_logs_stream(State(s): State<S>, _a: Admin, Path(id): Path<String>, Query(q): Query<TailQ>) -> AppResult<impl IntoResponse> {
    let cid = current_container(&s, &id)?;
    let mut rx = s.runtime.logs_follow(&cid, q.tail.unwrap_or(100).min(1000)).await.map_err(|e| AppError::unavailable(e.to_string()))?;
    let (tx, out) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if tx.send(Ok(Event::default().event("log").data(chunk.trim_end_matches('\n')))).await.is_err() {
                break; // browser closed: dropping `rx` ends the Docker log stream
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(out)).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct RevealQ {
    reveal: Option<bool>,
}

async fn get_env(State(s): State<S>, _a: Admin, Path(id): Path<String>, Query(q): Query<RevealQ>) -> AppResult<Json<EnvListResp>> {
    let reveal = q.reveal.unwrap_or(false);
    if reveal {
        audit(&s, "env.reveal", &id);
    }
    Ok(Json(sites::get_env(&s, &id, reveal)?))
}

async fn put_env(State(s): State<S>, _a: Admin, Path(id): Path<String>, ApiJson(req): ApiJson<PutEnvReq>) -> AppResult<StatusCode> {
    sites::put_env(&s, &id, req.vars)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_tokens(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<Json<Vec<TokenDto>>> {
    Ok(Json(sites::list_tokens(&s, &id)?))
}

async fn create_token(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<(StatusCode, Json<TokenCreated>)> {
    Ok((StatusCode::CREATED, Json(sites::create_token(&s, &id)?)))
}

async fn revoke_token(State(s): State<S>, _a: Admin, Path((id, tid)): Path<(String, String)>) -> AppResult<StatusCode> {
    sites::revoke_token(&s, &id, &tid)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn workflow(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<Json<WorkflowResp>> {
    Ok(Json(sites::workflow(&s, &id)?))
}

async fn list_deployments(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<Json<Vec<DeploymentDto>>> {
    load_site(&s, &id)?;
    Ok(Json(
        s.store
            .list_deployments(&id, 50)?
            .into_iter()
            .map(|d| DeploymentDto {
                id: d.id,
                site_id: d.site_id,
                image_ref: d.image_ref,
                image_digest: d.image_digest,
                status: d.status,
                trigger: d.trigger,
                started_at: d.started_at,
                finished_at: d.finished_at,
                error: d.error,
            })
            .collect(),
    ))
}

async fn rollback(State(s): State<S>, _a: Admin, Path(id): Path<String>, ApiJson(req): ApiJson<RollbackReq>) -> AppResult<(StatusCode, Json<DeployAccepted>)> {
    let site = load_site(&s, &id)?;
    Ok((StatusCode::ACCEPTED, Json(deploy::rollback(&s, &site, &req.deployment_id)?)))
}

async fn webhook(State(s): State<S>, headers: HeaderMap, ApiJson(req): ApiJson<DeployWebhookReq>) -> AppResult<(StatusCode, Json<DeployAccepted>)> {
    let token = auth::deploy_token(&s, &headers)?;
    let site = deploy::site_for_webhook(&s, &token.site_id, req.site_name.as_deref())?;
    Ok((StatusCode::ACCEPTED, Json(deploy::webhook(&s, &site, &req.image)?)))
}

// ---- cron -----------------------------------------------------------------------------------

async fn list_cron(State(s): State<S>, _a: Admin) -> AppResult<Json<Vec<CronJobDto>>> {
    Ok(Json(s.store.list_cron()?.iter().map(|r| jobs::cron_dto(&s, r)).collect()))
}

async fn create_cron(State(s): State<S>, _a: Admin, ApiJson(req): ApiJson<CreateCronReq>) -> AppResult<(StatusCode, Json<CronJobDto>)> {
    Ok((StatusCode::CREATED, Json(jobs::create_site_job(&s, req)?)))
}

async fn patch_cron(State(s): State<S>, _a: Admin, Path(id): Path<String>, ApiJson(req): ApiJson<UpdateCronReq>) -> AppResult<Json<CronJobDto>> {
    if let Some(sched) = &req.schedule {
        crate::cron::CronExpr::parse(sched).map_err(|e| AppError::bad_request(e.to_string()))?;
    }
    let row = s.store.get_cron(&id)?.ok_or_else(|| AppError::not_found("job not found"))?;
    if row.kind == "system" && req.schedule.is_some() {
        return Err(AppError::bad_request("system jobs can only be enabled or disabled"));
    }
    s.store.update_cron(&id, req.enabled, req.schedule.as_deref().map(str::trim))?;
    let row = s.store.get_cron(&id)?.ok_or_else(|| AppError::not_found("job not found"))?;
    Ok(Json(jobs::cron_dto(&s, &row)))
}

async fn delete_cron(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<StatusCode> {
    match s.store.get_cron(&id)? {
        None => Err(AppError::not_found("job not found")),
        Some(r) if r.kind == "system" => Err(AppError::bad_request("system jobs cannot be deleted; disable them instead")),
        Some(_) => {
            s.store.delete_cron(&id)?;
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

async fn cron_runs(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<Json<Vec<CronRunDto>>> {
    s.store.get_cron(&id)?.ok_or_else(|| AppError::not_found("job not found"))?;
    Ok(Json(
        s.store
            .list_runs(&id, 50)?
            .into_iter()
            .map(|r| CronRunDto { id: r.id, job_id: r.job_id, started_at: r.started_at, finished_at: r.finished_at, exit_code: r.exit_code, log_tail: r.log_tail })
            .collect(),
    ))
}

async fn run_cron(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<StatusCode> {
    let row = s.store.get_cron(&id)?.ok_or_else(|| AppError::not_found("job not found"))?;
    if !jobs::run_now(&s, row) {
        return Err(AppError::conflict("job is already running"));
    }
    Ok(StatusCode::ACCEPTED)
}

// ---- registries -----------------------------------------------------------------------------

async fn list_registries(State(s): State<S>, _a: Admin) -> AppResult<Json<Vec<RegistryDto>>> {
    Ok(Json(sites::list_registries(&s)?))
}

async fn create_registry(State(s): State<S>, _a: Admin, ApiJson(req): ApiJson<CreateRegistryReq>) -> AppResult<(StatusCode, Json<RegistryDto>)> {
    Ok((StatusCode::CREATED, Json(sites::create_registry(&s, req)?)))
}

async fn delete_registry(State(s): State<S>, _a: Admin, Path(id): Path<String>) -> AppResult<StatusCode> {
    sites::delete_registry(&s, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    struct Client {
        app: Router,
        cookie: Option<String>,
        csrf: Option<String>,
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn json<T: DeserializeOwned>(&self) -> T {
            serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("bad json {e}: {}", String::from_utf8_lossy(&self.body)))
        }
        fn code(&self) -> String {
            self.json::<ApiError>().error.code
        }
    }

    impl Client {
        fn new(e: &TestEnv) -> Client {
            Client { app: router(e.state.clone()), cookie: None, csrf: None }
        }

        async fn raw(&self, method: Method, path: &str, body: Option<serde_json::Value>, extra: &[(&str, &str)]) -> Reply {
            let mut b = Request::builder().method(method).uri(path);
            if let Some(c) = &self.cookie {
                b = b.header("cookie", c);
            }
            for (k, v) in extra {
                b = b.header(*k, *v);
            }
            let req = match body {
                Some(j) => b.header("content-type", "application/json").body(Body::from(j.to_string())).unwrap(),
                None => b.body(Body::empty()).unwrap(),
            };
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let (parts, body) = resp.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap().to_vec();
            Reply { status: parts.status, headers: parts.headers, body: bytes }
        }

        async fn req(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Reply {
            let csrf = self.csrf.clone().unwrap_or_default();
            self.raw(method, path, body, &[("x-csrf-token", csrf.as_str())]).await
        }

        async fn get(&self, p: &str) -> Reply {
            self.req(Method::GET, p, None).await
        }
        async fn post(&self, p: &str, b: serde_json::Value) -> Reply {
            self.req(Method::POST, p, Some(b)).await
        }

        async fn login(&mut self, user: &str, pw: &str) -> Reply {
            let r = self.raw(Method::POST, "/api/v1/auth/login", Some(serde_json::json!({"username": user, "password": pw})), &[]).await;
            if r.status == 200 {
                let c = r.headers[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string();
                self.cookie = Some(c);
                self.csrf = Some(r.json::<LoginResp>().csrf);
            }
            r
        }
    }

    fn seed_admin(e: &TestEnv) {
        e.state.store.create_admin("admin", &crypto::hash_password("correct horse battery")).unwrap();
    }

    async fn logged_in() -> (TestEnv, Client) {
        let e = TestEnv::new();
        seed_admin(&e);
        let mut c = Client::new(&e);
        assert_eq!(c.login("admin", "correct horse battery").await.status, 200);
        (e, c)
    }

    fn site_body(name: &str) -> serde_json::Value {
        serde_json::json!({"name": name, "domain": format!("{name}.example.com"), "image_repo": format!("ghcr.io/o/{name}"), "ram_limit_mb": 128, "skip_dns_check": true, "override_budget": true})
    }

    #[tokio::test]
    async fn everything_requires_login_except_health_and_assets() {
        let e = TestEnv::new();
        seed_admin(&e);
        let c = Client::new(&e);
        for (m, p) in [(Method::GET, "/api/v1/sites"), (Method::GET, "/api/v1/system/stats"), (Method::GET, "/api/v1/cron"), (Method::GET, "/api/v1/registries"), (Method::POST, "/api/v1/system/prune"), (Method::GET, "/api/v1/system/backup")] {
            let r = c.req(m.clone(), p, None).await;
            assert_eq!(r.status, 401, "{m} {p}");
        }
        assert_eq!(c.raw(Method::GET, "/healthz", None, &[]).await.status, 200);
        assert_eq!(c.raw(Method::GET, "/", None, &[]).await.status, 200);
        assert_eq!(c.raw(Method::GET, "/api/v1/nope", None, &[]).await.status, 404);
    }

    #[tokio::test]
    async fn login_failures_back_off_per_ip_without_locking_the_account() {
        let e = TestEnv::new();
        seed_admin(&e);
        let mut attacker = Client::new(&e);
        for _ in 0..3 {
            let r = attacker.login("admin", "wrong").await;
            assert_eq!(r.status, 401);
            assert_eq!(r.code(), "unauthorized");
        }
        let r = attacker.login("admin", "wrong").await; // 4th failure arms the delay
        assert_eq!(r.status, 401);
        let r = attacker.login("admin", "correct horse battery").await;
        assert_eq!(r.status, 429, "even the right password is delayed for the abusive IP");
        assert!(r.headers.contains_key(header::RETRY_AFTER));
        // the real admin, from another address, is not locked out
        let owner = Client::new(&e);
        let r = owner.raw(Method::POST, "/api/v1/auth/login", Some(serde_json::json!({"username":"admin","password":"correct horse battery"})), &[("x-forwarded-for", "198.51.100.7")]).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn uninitialized_server_says_so() {
        let e = TestEnv::new();
        let mut c = Client::new(&e);
        let r = c.login("admin", "x").await;
        assert_eq!(r.status, 503);
        assert!(r.json::<ApiError>().error.message.contains("rustle init"));
    }

    #[tokio::test]
    async fn csrf_is_required_for_mutations() {
        let (_e, c) = logged_in().await;
        assert_eq!(c.get("/api/v1/sites").await.status, 200);
        let no_csrf = c.raw(Method::POST, "/api/v1/sites", Some(site_body("a")), &[]).await;
        assert_eq!(no_csrf.status, 403);
        let wrong = c.raw(Method::POST, "/api/v1/sites", Some(site_body("a")), &[("x-csrf-token", "nope")]).await;
        assert_eq!(wrong.status, 403);
        assert_eq!(c.post("/api/v1/sites", site_body("a")).await.status, 201);
    }

    #[tokio::test]
    async fn session_cookie_is_hardened_and_logout_clears_it() {
        let (_e, c) = logged_in().await;
        let r = c.req(Method::POST, "/api/v1/auth/logout", None).await;
        assert_eq!(r.status, 204);
        let sc = r.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(sc.contains("Max-Age=0") && sc.contains("HttpOnly"));
        let me = c.get("/api/v1/auth/me").await;
        assert_eq!(me.status, 200);
        assert!(!me.json::<LoginResp>().csrf.is_empty());
    }

    #[tokio::test]
    async fn password_change_revokes_sessions() {
        let (e, mut c) = logged_in().await;
        let r = c.post("/api/v1/auth/password", serde_json::json!({"current":"wrong","new":"a-much-longer-password"})).await;
        assert_eq!(r.status, 403);
        let r = c.post("/api/v1/auth/password", serde_json::json!({"current":"correct horse battery","new":"short"})).await;
        assert_eq!(r.status, 400);
        let r = c.post("/api/v1/auth/password", serde_json::json!({"current":"correct horse battery","new":"a-much-longer-password"})).await;
        assert_eq!(r.status, 204);
        assert_eq!(c.get("/api/v1/sites").await.status, 401, "old session is dead");
        let mut c2 = Client::new(&e);
        assert_eq!(c2.login("admin", "correct horse battery").await.status, 401);
        assert_eq!(c2.login("admin", "a-much-longer-password").await.status, 200);
        c.cookie = None;
    }

    #[tokio::test]
    async fn bad_json_uses_the_error_format() {
        let (_e, c) = logged_in().await;
        let r = c.post("/api/v1/sites", serde_json::json!({"name": 5})).await;
        assert_eq!(r.status, 400);
        assert_eq!(r.code(), "bad_request");
        let huge = serde_json::json!({"name": "a".repeat(100_000)});
        let r = c.post("/api/v1/sites", huge).await;
        assert_eq!(r.status, 413, "64 KB body limit");
    }

    #[tokio::test]
    async fn site_lifecycle_through_the_api() {
        let (e, c) = logged_in().await;
        let r = c.post("/api/v1/sites", site_body("blog")).await;
        assert_eq!(r.status, 201);
        let site: SiteDto = r.json();
        assert_eq!(c.post("/api/v1/sites", site_body("blog")).await.status, 409);
        assert_eq!(c.get("/api/v1/sites").await.json::<Vec<SiteDto>>().len(), 1);
        // patch
        let r = c.req(Method::PATCH, &format!("/api/v1/sites/{}", site.id), Some(serde_json::json!({"zero_downtime": true, "ram_limit_mb": 192, "override_budget": true}))).await;
        assert_eq!(r.status, 200);
        assert!(r.json::<SiteDto>().zero_downtime);
        // env masked by default
        let r = c.req(Method::PUT, &format!("/api/v1/sites/{}/env", site.id), Some(serde_json::json!({"vars":[{"key":"API_KEY","value":"s3cret"}]}))).await;
        assert_eq!(r.status, 204);
        let masked: EnvListResp = c.get(&format!("/api/v1/sites/{}/env", site.id)).await.json();
        assert!(masked.masked && masked.vars[0].value == "********");
        let shown: EnvListResp = c.get(&format!("/api/v1/sites/{}/env?reveal=true", site.id)).await.json();
        assert_eq!(shown.vars[0].value, "s3cret");
        // workflow
        let wf: WorkflowResp = c.get(&format!("/api/v1/sites/{}/workflow", site.id)).await.json();
        assert!(wf.workflow_yaml.contains("ghcr.io/o/blog"));
        // deploy via redeploy requires a prior deployment
        assert_eq!(c.post(&format!("/api/v1/sites/{}/redeploy", site.id), serde_json::json!({})).await.status, 409);
        // delete
        let r = c.req(Method::DELETE, &format!("/api/v1/sites/{}", site.id), None).await;
        assert_eq!(r.status, 204);
        assert_eq!(c.get(&format!("/api/v1/sites/{}", site.id)).await.status, 404);
        assert!(e.gateway.sites().is_empty());
    }

    #[tokio::test]
    async fn webhook_deploys_with_a_site_token_and_nothing_else() {
        let (e, c) = logged_in().await;
        let a: SiteDto = c.post("/api/v1/sites", site_body("a")).await.json();
        let b: SiteDto = c.post("/api/v1/sites", site_body("b")).await.json();
        let tok_a: TokenCreated = c.post(&format!("/api/v1/sites/{}/tokens", a.id), serde_json::json!({})).await.json();
        let hook = |tok: &str, body: serde_json::Value| {
            let c = &c;
            let tok = tok.to_string();
            async move { c.raw(Method::POST, "/api/v1/webhooks/deploy", Some(body), &[("authorization", &format!("Bearer {tok}"))]).await }
        };
        // no CSRF needed (machine endpoint) but a valid deploy token is
        assert_eq!(c.raw(Method::POST, "/api/v1/webhooks/deploy", Some(serde_json::json!({"image":"ghcr.io/o/a:1"})), &[]).await.status, 401);
        assert_eq!(hook("rt_wrong", serde_json::json!({"image":"ghcr.io/o/a:1"})).await.status, 401);
        // the admin session cookie is not a deploy credential
        assert_eq!(c.req(Method::POST, "/api/v1/webhooks/deploy", Some(serde_json::json!({"image":"ghcr.io/o/a:1"}))).await.status, 401);
        // token of A cannot deploy B's image or name B
        assert_eq!(hook(&tok_a.token, serde_json::json!({"image":"ghcr.io/o/b:1"})).await.status, 403);
        assert_eq!(hook(&tok_a.token, serde_json::json!({"image":"ghcr.io/o/a:1","site_name":"b"})).await.status, 403);
        // happy path
        let r = hook(&tok_a.token, serde_json::json!({"image":"ghcr.io/o/a:abc123","site_name":"a"})).await;
        assert_eq!(r.status, 202);
        let acc: DeployAccepted = r.json();
        e.state.deployer.wait_idle(&a.id).await;
        let deps: Vec<DeploymentDto> = c.get(&format!("/api/v1/sites/{}/deployments", a.id)).await.json();
        assert_eq!(deps[0].id, acc.deployment_id);
        assert_eq!(deps[0].status, "running");
        assert_eq!(deps[0].trigger, "webhook");
        let sa: SiteDto = c.get(&format!("/api/v1/sites/{}", a.id)).await.json();
        assert_eq!(sa.status, "running");
        // B untouched
        let sb: SiteDto = c.get(&format!("/api/v1/sites/{}", b.id)).await.json();
        assert_eq!(sb.status, "created");
        // revoked token stops working immediately
        let tokens: Vec<TokenDto> = c.get(&format!("/api/v1/sites/{}/tokens", a.id)).await.json();
        assert_eq!(c.req(Method::DELETE, &format!("/api/v1/sites/{}/tokens/{}", a.id, tokens[0].id), None).await.status, 204);
        assert_eq!(hook(&tok_a.token, serde_json::json!({"image":"ghcr.io/o/a:2"})).await.status, 401);
        // rollback endpoint refuses the current deployment
        let r = c.post(&format!("/api/v1/sites/{}/rollback", a.id), serde_json::json!({"deployment_id": acc.deployment_id})).await;
        assert_eq!(r.status, 409);
    }

    #[tokio::test]
    async fn webhook_is_rate_limited_per_token() {
        let (_e, c) = logged_in().await;
        let a: SiteDto = c.post("/api/v1/sites", site_body("a")).await.json();
        let tok: TokenCreated = c.post(&format!("/api/v1/sites/{}/tokens", a.id), serde_json::json!({})).await.json();
        let mut last = 0;
        for _ in 0..35 {
            last = c.raw(Method::POST, "/api/v1/webhooks/deploy", Some(serde_json::json!({"image":"ghcr.io/o/other:1"})), &[("authorization", &format!("Bearer {}", tok.token))]).await.status.as_u16();
        }
        assert_eq!(last, 429);
    }

    #[tokio::test]
    async fn security_headers_on_every_response() {
        let (_e, c) = logged_in().await;
        for p in ["/", "/api/v1/sites", "/healthz"] {
            let r = c.raw(Method::GET, p, None, &[]).await;
            assert_eq!(r.headers["x-content-type-options"], "nosniff", "{p}");
            assert_eq!(r.headers["x-frame-options"], "DENY");
            assert!(r.headers["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
        }
        // API responses are never cached
        let r = c.get("/api/v1/sites").await;
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn settings_domain_switches_the_admin_route() {
        let (e, c) = logged_in().await;
        let r = c.req(Method::PUT, "/api/v1/system/settings", Some(serde_json::json!({"admin_domain":"Admin.Example.com","acme_email":"ops@example.com"}))).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json::<SettingsDto>().admin_domain, "admin.example.com");
        assert_eq!(e.gateway.base().unwrap().admin_domain.as_deref(), Some("admin.example.com"));
        assert_eq!(c.req(Method::PUT, "/api/v1/system/settings", Some(serde_json::json!({"admin_domain":"not a domain","acme_email":""}))).await.status, 400);
        c.post("/api/v1/sites", site_body("blog")).await;
        let r = c.req(Method::PUT, "/api/v1/system/settings", Some(serde_json::json!({"admin_domain":"blog.example.com","acme_email":""}))).await;
        assert_eq!(r.status, 409, "dashboard cannot steal a site's domain");
    }

    #[tokio::test]
    async fn cron_api_rules() {
        let (e, c) = logged_in().await;
        crate::jobs::seed_system_jobs(&e.state.store).unwrap();
        let list: Vec<CronJobDto> = c.get("/api/v1/cron").await.json();
        let sys = list.iter().find(|j| j.kind == "system").unwrap();
        assert_eq!(c.req(Method::DELETE, &format!("/api/v1/cron/{}", sys.id), None).await.status, 400);
        assert_eq!(c.req(Method::PATCH, &format!("/api/v1/cron/{}", sys.id), Some(serde_json::json!({"schedule":"* * * * *"}))).await.status, 400);
        let r = c.req(Method::PATCH, &format!("/api/v1/cron/{}", sys.id), Some(serde_json::json!({"enabled": false}))).await;
        assert!(!r.json::<CronJobDto>().enabled);
        let site: SiteDto = c.post("/api/v1/sites", site_body("a")).await.json();
        let r = c.post("/api/v1/cron", serde_json::json!({"name":"x","schedule":"bad","site_id":site.id,"argv":["a"]})).await;
        assert_eq!(r.status, 400);
        let r = c.post("/api/v1/cron", serde_json::json!({"name":"x","schedule":"*/5 * * * *","site_id":site.id,"argv":["node","j.js"]})).await;
        assert_eq!(r.status, 201);
        let job: CronJobDto = r.json();
        assert_eq!(c.req(Method::DELETE, &format!("/api/v1/cron/{}", job.id), None).await.status, 204);
        assert_eq!(c.get("/api/v1/cron/nope/runs").await.status, 404);
    }

    #[tokio::test]
    async fn registries_never_return_secrets() {
        let (_e, c) = logged_in().await;
        let r = c.post("/api/v1/registries", serde_json::json!({"name":"gh","username":"me","secret":"ghp_topsecret_value"})).await;
        assert_eq!(r.status, 201);
        assert!(!String::from_utf8_lossy(&r.body).contains("topsecret"));
        let list = c.get("/api/v1/registries").await;
        assert!(!String::from_utf8_lossy(&list.body).contains("topsecret"));
    }

    #[tokio::test]
    async fn stats_reflect_budget_and_degraded_state() {
        let (e, c) = logged_in().await;
        let st: SystemStats = c.get("/api/v1/system/stats").await.json();
        assert_eq!(st.mem_total_mb, 992);
        assert_eq!(st.budget_mb, 992 - 250 - 64);
        assert!(st.public_ip_mode && st.gateway_ok);
        e.state.set_degraded(Some("gateway unreachable: x".into()));
        let st: SystemStats = c.get("/api/v1/system/stats").await.json();
        assert!(!st.gateway_ok && st.runtime_ok);
    }

    #[tokio::test]
    async fn stats_stream_emits_and_releases_its_subscription() {
        use http_body_util::BodyExt;
        let (e, c) = logged_in().await;
        let req = Request::builder().method(Method::GET).uri("/api/v1/system/stream").header("cookie", c.cookie.clone().unwrap()).body(Body::empty()).unwrap();
        let resp = c.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame()).await.unwrap().unwrap().unwrap();
        let text = String::from_utf8_lossy(frame.data_ref().unwrap()).to_string();
        assert!(text.contains("event: stats") && text.contains("mem_total_mb"), "{text}");
        assert_eq!(e.state.stats_subscribers.load(Ordering::SeqCst), 1);
        drop(body); // the dashboard closed: sampling must stop
        for _ in 0..300 {
            if e.state.stats_subscribers.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(e.state.stats_subscribers.load(Ordering::SeqCst), 0, "no sampling without a viewer");
    }
}
