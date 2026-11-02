//! Site lifecycle: create/update/delete, start/stop, environment, deploy tokens, registries,
//! RAM budget and the CI templates.

use crate::app::AppState;
use crate::auth::new_deploy_token;
use crate::crypto::sha256_hex;
use crate::error::{AppError, AppResult};
use crate::imageref::{parse_repo, ImageRef};
use crate::metrics;
use crate::store::{RegistryRow, SiteRow, TokenRow};
use crate::util::{now, rand_hex, valid_domain, valid_env_key, valid_site_name};
use rustle_types::*;
use std::collections::BTreeMap;

pub const MIN_RAM: u32 = 64;
pub const MAX_RAM: u32 = 512;
pub const MAX_ENV_VARS: usize = 64;
pub const MAX_ENV_VALUE: usize = 8 * 1024;
pub const MASK: &str = "********";

/// (allocated MiB across all sites, budget MiB).
pub fn budget(state: &AppState) -> (u32, u32) {
    let allocated = state.store.sum_ram_mb(None).unwrap_or(0);
    (allocated, budget_total(state))
}

pub fn budget_total(state: &AppState) -> u32 {
    let mem = metrics::meminfo(&state.cfg.proc_root);
    let base = state
        .store
        .setting("base_used_mb")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok());
    let ov = state
        .store
        .setting("budget_override_mb")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok());
    metrics::budget_mb(&mem, base, ov)
}

pub fn public_ip(state: &AppState) -> Option<String> {
    state
        .store
        .setting("public_ip")
        .ok()
        .flatten()
        .or_else(|| state.cfg.public_ip.clone())
}

pub fn admin_domain(state: &AppState) -> Option<String> {
    state
        .store
        .setting("admin_domain")
        .ok()
        .flatten()
        .filter(|d| !d.is_empty())
}

pub fn site_dto(state: &AppState, s: &SiteRow) -> SiteDto {
    let dep = s
        .current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten());
    let ram_used = dep
        .as_ref()
        .and_then(|d| d.container_id.as_deref())
        .and_then(|c| metrics::container_mem_mb(&state.cfg.cgroup_root, c));
    SiteDto {
        id: s.id.clone(),
        name: s.name.clone(),
        domain: s.domain.clone(),
        image_repo: s.image_repo.clone(),
        container_port: s.container_port,
        ram_limit_mb: s.ram_limit_mb,
        status: s.status.clone(),
        zero_downtime: s.zero_downtime,
        compat_caps: s.compat_caps,
        registry_id: s.registry_id.clone(),
        current_deployment_id: s.current_deployment_id.clone(),
        ram_used_mb: ram_used,
        last_deployed_at: dep.and_then(|d| d.finished_at.or(Some(d.started_at))),
    }
}

fn validate_ram(ram: u32) -> AppResult<()> {
    if !(MIN_RAM..=MAX_RAM).contains(&ram) || !ram.is_multiple_of(32) {
        return Err(AppError::bad_request(format!(
            "ram_limit_mb must be {MIN_RAM}-{MAX_RAM} in steps of 32"
        )));
    }
    Ok(())
}

fn validate_env(vars: &[EnvVar]) -> AppResult<()> {
    if vars.len() > MAX_ENV_VARS {
        return Err(AppError::bad_request(format!(
            "at most {MAX_ENV_VARS} environment variables"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for v in vars {
        if !valid_env_key(&v.key) {
            return Err(AppError::bad_request(format!(
                "invalid environment variable name '{}'",
                v.key
            )));
        }
        if !seen.insert(v.key.as_str()) {
            return Err(AppError::bad_request(format!(
                "duplicate environment variable '{}'",
                v.key
            )));
        }
        if v.value.len() > MAX_ENV_VALUE || v.value.contains('\0') {
            return Err(AppError::bad_request(format!(
                "value of '{}' is too large or contains NUL",
                v.key
            )));
        }
    }
    Ok(())
}

fn aad(site_id: &str, key: &str) -> String {
    format!("env:{site_id}:{key}")
}

/// Encrypt variables for storage; each value is bound to its site and key.
pub fn seal_env(
    key: &crate::crypto::MasterKey,
    site_id: &str,
    vars: &[EnvVar],
) -> Vec<(String, Vec<u8>)> {
    vars.iter()
        .map(|v| (v.key.clone(), key.seal_str(&v.value, &aad(site_id, &v.key))))
        .collect()
}

pub fn store_env(state: &AppState, site_id: &str, vars: &[EnvVar]) -> AppResult<()> {
    state
        .store
        .replace_env(site_id, &seal_env(&state.key, site_id, vars))
}

pub fn load_env(state: &AppState, site_id: &str) -> AppResult<Vec<EnvVar>> {
    state
        .store
        .list_env(site_id)?
        .into_iter()
        .map(|(k, blob)| {
            let value = state
                .key
                .open_str(&blob, &aad(site_id, &k))
                .map_err(AppError::internal)?;
            Ok(EnvVar { key: k, value })
        })
        .collect()
}

pub async fn check_dns(state: &AppState, domain: &str) -> AppResult<()> {
    let Some(expected) = public_ip(state) else {
        tracing::warn!("public IP unknown, skipping DNS check for {domain}");
        return Ok(());
    };
    let ips = state.resolver.resolve(domain).await;
    if ips.is_empty() {
        return Err(AppError::unprocessable(
            "dns_missing",
            format!("{domain} does not resolve yet; create an A record pointing to {expected}"),
        ));
    }
    if !ips.iter().any(|ip| ip.to_string() == expected) {
        let found: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
        return Err(AppError::unprocessable(
            "dns_mismatch",
            format!(
                "{domain} resolves to {} but this server is {expected}",
                found.join(", ")
            ),
        ));
    }
    Ok(())
}

fn check_budget(
    state: &AppState,
    exclude: Option<&str>,
    new_ram: u32,
    override_ok: bool,
) -> AppResult<()> {
    let allocated = state.store.sum_ram_mb(exclude)?;
    let budget = budget_total(state);
    if allocated + new_ram > budget && !override_ok {
        return Err(AppError::unprocessable(
            "budget_exceeded",
            format!("allocating {new_ram} MB would bring sites to {} MB, above the {budget} MB RAM budget", allocated + new_ram),
        ));
    }
    Ok(())
}

pub fn dial(container_name: &str, port: u16) -> String {
    format!("{container_name}:{port}")
}

/// How Caddy reaches a container. A Caddy attached to `rustle-net` (Rustle's own, or a shared one running
/// as a container) resolves container names; a Caddy running on the host cannot, so it is given the IP.
pub fn upstream(state: &AppState, container_name: &str, ip: Option<&str>, port: u16) -> String {
    match (state.cfg.gateway_external && !state.cfg.gateway_names, ip) {
        (true, Some(ip)) => format!("{ip}:{port}"),
        _ => dial(container_name, port),
    }
}

/// Placeholder upstream used before the first deployment (Caddy answers 502).
pub fn placeholder_dial(name: &str, port: u16) -> String {
    dial(&format!("rustle-{name}-pending"), port)
}

pub async fn create_site(state: &AppState, req: CreateSiteReq) -> AppResult<SiteDto> {
    if !valid_site_name(&req.name) {
        return Err(AppError::bad_request(
            "name must be 1-32 chars: lowercase letters, digits and '-'",
        ));
    }
    let domain = req.domain.trim().to_ascii_lowercase();
    if !valid_domain(&domain) {
        return Err(AppError::bad_request("invalid domain"));
    }
    if admin_domain(state).as_deref() == Some(domain.as_str()) {
        return Err(AppError::conflict("domain is used by the dashboard"));
    }
    let repo =
        parse_repo(req.image_repo.trim()).map_err(|e| AppError::bad_request(e.to_string()))?;
    validate_ram(req.ram_limit_mb)?;
    if req.container_port == 0 {
        return Err(AppError::bad_request("container_port must be 1-65535"));
    }
    validate_env(&req.env)?;
    if let Some(rid) = &req.registry_id {
        if state.store.get_registry(rid)?.is_none() {
            return Err(AppError::bad_request("unknown registry_id"));
        }
    }
    if state.store.get_site_by_name(&req.name)?.is_some() {
        return Err(AppError::conflict("a site with this name already exists"));
    }
    if !req.skip_dns_check {
        check_dns(state, &domain).await?;
    }
    check_budget(state, None, req.ram_limit_mb, req.override_budget)?;

    let t = now();
    let row = SiteRow {
        id: rand_hex(6),
        name: req.name.clone(),
        domain: domain.clone(),
        image_repo: repo.repo_key(),
        container_port: req.container_port,
        ram_limit_mb: req.ram_limit_mb,
        status: "created".into(),
        current_deployment_id: None,
        registry_id: req.registry_id.clone(),
        zero_downtime: false,
        compat_caps: false,
        created_at: t,
        updated_at: t,
    };
    state.store.insert_site(&row)?; // unique(name, domain) violations surface as 409
    if let Err(e) = store_env(state, &row.id, &req.env) {
        let _ = state.store.delete_site(&row.id);
        return Err(e);
    }
    // With a shared Caddy the domain may already be served by an existing site: adding a placeholder
    // route now would take that traffic over and answer 502. The route is added by the first
    // successful deploy instead, which switches traffic in one step.
    if !state.cfg.gateway_external {
        if let Err(e) = state
            .gateway
            .upsert_site(
                &row.name,
                &row.domain,
                &placeholder_dial(&row.name, row.container_port),
            )
            .await
        {
            let _ = state.store.delete_site(&row.id);
            return Err(AppError::unavailable(format!(
                "could not configure the gateway: {e}"
            )));
        }
    }
    state.store.audit(t, "admin", "site.create", &row.name);
    Ok(site_dto(state, &row))
}

pub async fn update_site(state: &AppState, id: &str, req: UpdateSiteReq) -> AppResult<SiteDto> {
    let mut s = state
        .store
        .get_site(id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    if let Some(ram) = req.ram_limit_mb {
        validate_ram(ram)?;
        check_budget(
            state,
            Some(&s.id),
            ram,
            req.override_budget.unwrap_or(false),
        )?;
        s.ram_limit_mb = ram;
    }
    if let Some(p) = req.container_port {
        if p == 0 {
            return Err(AppError::bad_request("container_port must be 1-65535"));
        }
        s.container_port = p;
    }
    if let Some(z) = req.zero_downtime {
        s.zero_downtime = z;
    }
    if let Some(c) = req.compat_caps {
        s.compat_caps = c;
    }
    if let Some(r) = req.registry_id {
        if let Some(rid) = &r {
            if state.store.get_registry(rid)?.is_none() {
                return Err(AppError::bad_request("unknown registry_id"));
            }
        }
        s.registry_id = r;
    }
    s.updated_at = now();
    state.store.update_site(&s)?;
    state
        .store
        .audit(s.updated_at, "admin", "site.update", &s.name);
    Ok(site_dto(state, &s))
}

pub async fn delete_site(state: &AppState, id: &str) -> AppResult<()> {
    let s = state
        .store
        .get_site(id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    if state.deployer.is_busy(&s.id) {
        return Err(AppError::conflict("a deployment is in progress"));
    }
    state
        .gateway
        .remove_site(&s.name)
        .await
        .map_err(|e| AppError::unavailable(e.to_string()))?;
    let label = format!("rustle.site={}", s.id);
    let _ = label;
    for c in state
        .runtime
        .list_containers(Some("rustle.site"))
        .await
        .unwrap_or_default()
    {
        if c.labels.get("rustle.site") == Some(&s.id) {
            let _ = state
                .runtime
                .stop(&c.id, state.tuning.stop_timeout_secs)
                .await;
            let _ = state.runtime.remove(&c.id, true).await;
        }
    }
    state.store.delete_site(&s.id)?;
    state.store.audit(now(), "admin", "site.delete", &s.name);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

pub async fn site_action(state: &AppState, id: &str, action: Action) -> AppResult<SiteDto> {
    let mut s = state
        .store
        .get_site(id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    if state.deployer.is_busy(&s.id) {
        return Err(AppError::conflict("a deployment is in progress"));
    }
    let dep = s
        .current_deployment_id
        .as_deref()
        .and_then(|d| state.store.get_deployment(d).ok().flatten())
        .ok_or_else(|| AppError::conflict("site has no deployment yet"))?;
    let cid = dep
        .container_id
        .ok_or_else(|| AppError::conflict("deployment has no container"))?;
    let rt = &state.runtime;
    let map = |e: crate::docker::RtError| AppError::unavailable(e.to_string());
    match action {
        Action::Stop => rt
            .stop(&cid, state.tuning.stop_timeout_secs)
            .await
            .map_err(map)?,
        Action::Start => rt.start(&cid).await.map_err(map)?,
        Action::Restart => {
            rt.stop(&cid, state.tuning.stop_timeout_secs)
                .await
                .map_err(map)?;
            rt.start(&cid).await.map_err(map)?;
        }
    }
    s.status = if action == Action::Stop {
        "stopped".into()
    } else {
        "running".into()
    };
    state.store.set_site_status(&s.id, &s.status)?;
    state.store.audit(
        now(),
        "admin",
        &format!("site.{action:?}").to_lowercase(),
        &s.name,
    );
    Ok(site_dto(state, &s))
}

// ---- env ------------------------------------------------------------------------------------

pub fn get_env(state: &AppState, site_id: &str, reveal: bool) -> AppResult<EnvListResp> {
    state
        .store
        .get_site(site_id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    let mut vars = load_env(state, site_id)?;
    if !reveal {
        for v in &mut vars {
            v.value = MASK.into();
        }
    }
    Ok(EnvListResp {
        vars,
        masked: !reveal,
    })
}

/// Replace all variables. A value equal to the mask keeps the stored value, so the UI can
/// round-trip a masked list without ever holding secrets.
pub fn put_env(state: &AppState, site_id: &str, mut vars: Vec<EnvVar>) -> AppResult<()> {
    let s = state
        .store
        .get_site(site_id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    let existing: BTreeMap<String, String> = load_env(state, site_id)?
        .into_iter()
        .map(|v| (v.key, v.value))
        .collect();
    for v in &mut vars {
        if v.value == MASK {
            v.value = existing.get(&v.key).cloned().ok_or_else(|| {
                AppError::bad_request(format!("'{}' has no stored value to keep", v.key))
            })?;
        }
    }
    validate_env(&vars)?;
    store_env(state, site_id, &vars)?;
    state.store.audit(now(), "admin", "site.env", &s.name);
    Ok(())
}

/// Final environment of a container: user variables plus safe, RAM-aware defaults (FR-25).
pub fn effective_env(vars: &[EnvVar], port: u16, ram_limit_mb: u32) -> Vec<String> {
    let mut m: BTreeMap<String, String> = vars
        .iter()
        .map(|v| (v.key.clone(), v.value.clone()))
        .collect();
    m.entry("PORT".into()).or_insert_with(|| port.to_string());
    m.entry("HOSTNAME".into())
        .or_insert_with(|| "0.0.0.0".into());
    m.entry("NODE_OPTIONS".into())
        .or_insert_with(|| format!("--max-old-space-size={}", ram_limit_mb * 75 / 100));
    m.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

// ---- deploy tokens --------------------------------------------------------------------------

pub fn create_token(state: &AppState, site_id: &str) -> AppResult<TokenCreated> {
    let s = state
        .store
        .get_site(site_id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    let (raw, prefix, hash) = new_deploy_token();
    let row = TokenRow {
        id: rand_hex(6),
        site_id: s.id.clone(),
        prefix,
        token_hash: hash,
        created_at: now(),
        revoked_at: None,
        last_used_at: None,
    };
    state.store.insert_token(&row)?;
    state
        .store
        .audit(row.created_at, "admin", "token.create", &s.name);
    Ok(TokenCreated {
        id: row.id,
        token: raw,
    })
}

pub fn list_tokens(state: &AppState, site_id: &str) -> AppResult<Vec<TokenDto>> {
    state
        .store
        .get_site(site_id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    Ok(state
        .store
        .list_tokens(site_id)?
        .into_iter()
        .map(|t| TokenDto {
            id: t.id,
            prefix: t.prefix,
            created_at: t.created_at,
            revoked_at: t.revoked_at,
            last_used_at: t.last_used_at,
        })
        .collect())
}

pub fn revoke_token(state: &AppState, site_id: &str, token_id: &str) -> AppResult<()> {
    if !state.store.revoke_token(site_id, token_id, now())? {
        return Err(AppError::not_found("token not found or already revoked"));
    }
    state.store.audit(now(), "admin", "token.revoke", site_id);
    Ok(())
}

// ---- registries -----------------------------------------------------------------------------

pub fn create_registry(state: &AppState, req: CreateRegistryReq) -> AppResult<RegistryDto> {
    if req.name.trim().is_empty()
        || req.name.len() > 64
        || req.username.trim().is_empty()
        || req.secret.len() < 8
        || req.secret.len() > 512
    {
        return Err(AppError::bad_request(
            "name, username and a token are required",
        ));
    }
    let id = rand_hex(6);
    let row = RegistryRow {
        id: id.clone(),
        name: req.name.trim().to_string(),
        host: "ghcr.io".into(),
        username: req.username.trim().to_string(),
        secret_enc: state.key.seal_str(&req.secret, &format!("registry:{id}")),
        created_at: now(),
    };
    state.store.insert_registry(&row)?;
    state
        .store
        .audit(row.created_at, "admin", "registry.create", &row.name);
    Ok(RegistryDto {
        id,
        name: row.name,
        host: row.host,
        username: row.username,
    })
}

pub fn list_registries(state: &AppState) -> AppResult<Vec<RegistryDto>> {
    Ok(state
        .store
        .list_registries()?
        .into_iter()
        .map(|r| RegistryDto {
            id: r.id,
            name: r.name,
            host: r.host,
            username: r.username,
        })
        .collect())
}

pub fn delete_registry(state: &AppState, id: &str) -> AppResult<()> {
    if !state.store.delete_registry(id)? {
        return Err(AppError::not_found("registry not found"));
    }
    state.store.audit(now(), "admin", "registry.delete", id);
    Ok(())
}

pub fn registry_auth(
    state: &AppState,
    registry_id: &str,
) -> AppResult<crate::docker::RegistryAuth> {
    let r = state
        .store
        .get_registry(registry_id)?
        .ok_or_else(|| AppError::internal("registry vanished"))?;
    let secret = state
        .key
        .open_str(&r.secret_enc, &format!("registry:{}", r.id))
        .map_err(AppError::internal)?;
    Ok(crate::docker::RegistryAuth {
        username: r.username,
        password: secret,
        server: r.host,
    })
}

// ---- CI templates ---------------------------------------------------------------------------

pub fn workflow(state: &AppState, site_id: &str) -> AppResult<WorkflowResp> {
    let s = state
        .store
        .get_site(site_id)?
        .ok_or_else(|| AppError::not_found("site not found"))?;
    let ip_mode = admin_domain(state).is_none();
    Ok(WorkflowResp {
        workflow_yaml: workflow_yaml(&s.image_repo, ip_mode),
        dockerfile: dockerfile(s.container_port),
        dockerfile_static: dockerfile_static(s.container_port),
    })
}

pub fn workflow_yaml(image_repo: &str, ip_mode: bool) -> String {
    // The registered repository is validated lowercase, so it is safe to inline.
    let insecure = if ip_mode { " --insecure" } else { "" };
    let note = if ip_mode {
        "# NOTE: the dashboard uses a self-signed certificate until you set a domain, hence --insecure.\n      "
    } else {
        ""
    };
    format!(
        r#"name: Build and deploy to Rustle

on:
  push:
    branches: [main]

permissions:
  contents: read
  packages: write

env:
  IMAGE: {image_repo}

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: docker/setup-buildx-action@v3

      - uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{{{ github.actor }}}}
          password: ${{{{ secrets.GITHUB_TOKEN }}}}

      - uses: docker/build-push-action@v6
        with:
          context: .
          push: true
          tags: |
            ${{{{ env.IMAGE }}}}:${{{{ github.sha }}}}
            ${{{{ env.IMAGE }}}}:latest

      - name: Trigger Rustle deploy
        run: |
      {note}curl --fail-with-body -sS{insecure} -X POST "${{{{ secrets.RUSTLE_URL }}}}/api/v1/webhooks/deploy" \
            -H "Authorization: Bearer ${{{{ secrets.RUSTLE_TOKEN }}}}" \
            -H "Content-Type: application/json" \
            -d '{{"image": "${{{{ env.IMAGE }}}}:${{{{ github.sha }}}}"}}'
"#
    )
}

/// Static export image: the site is plain files, so the runtime image is just Caddy (about 10 MB of RAM
/// per site) instead of a Node process.
pub fn dockerfile_static(port: u16) -> String {
    r#"# Next.js static export (`output: "export"`): no Node at runtime, files served by Caddy.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm ci
COPY . .
ENV NEXT_TELEMETRY_DISABLED=1
RUN npm run build

FROM caddy:2-alpine
RUN printf '{
	admin off
	auto_https off
}
:__PORT__ {
	root * /srv
	encode zstd gzip
	try_files {path} {path}.html {path}/index.html
	file_server
}
' > /etc/caddy/Caddyfile
COPY --from=build /app/out /srv
EXPOSE __PORT__
"#
    .replace("__PORT__", &port.to_string())
}

pub fn dockerfile(port: u16) -> String {
    format!(
        r#"# Next.js standalone image, small by design (requires `output: "standalone"` in next.config).
FROM node:22-alpine AS deps
WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm ci

FROM node:22-alpine AS build
WORKDIR /app
COPY --from=deps /app/node_modules ./node_modules
COPY . .
ENV NEXT_TELEMETRY_DISABLED=1
RUN npm run build

FROM node:22-alpine AS run
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 HOSTNAME=0.0.0.0 PORT={port}
RUN addgroup -S app && adduser -S app -G app
COPY --from=build /app/.next/standalone ./
COPY --from=build /app/.next/static ./.next/static
COPY --from=build /app/public ./public
USER app
EXPOSE {port}
CMD ["node", "server.js"]
"#
    )
}

/// Does `image` (from a webhook) belong to the site's registered repository?
pub fn image_matches_site(site: &SiteRow, image: &str) -> AppResult<ImageRef> {
    let r = ImageRef::parse(image).map_err(|e| AppError::bad_request(e.to_string()))?;
    if r.repo_key() != site.image_repo {
        return Err(AppError::forbidden(
            "image repository does not match this site",
        ));
    }
    Ok(r)
}

pub fn token_hash_hex(raw: &str) -> String {
    sha256_hex(raw.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TestEnv;

    fn req(name: &str, domain: &str, ram: u32) -> CreateSiteReq {
        CreateSiteReq {
            name: name.into(),
            domain: domain.into(),
            image_repo: format!("ghcr.io/o/{name}"),
            ram_limit_mb: ram,
            container_port: 3000,
            registry_id: None,
            env: vec![EnvVar {
                key: "DATABASE_URL".into(),
                value: "postgres://u:p@h/db".into(),
            }],
            skip_dns_check: true,
            override_budget: false,
        }
    }

    fn set_mem(e: &TestEnv, total_kb: u64, avail_kb: u64) {
        std::fs::write(
            e.state.cfg.proc_root.join("meminfo"),
            format!("MemTotal: {total_kb} kB\nMemAvailable: {avail_kb} kB\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn budget_for_fresh_existing_and_upgraded_vps() {
        let e = TestEnv::new();
        let s = &*e.state;
        // 1: fresh 1 GB box, ~250 MB used by OS + stack.
        s.store.set_setting("base_used_mb", "250").unwrap();
        set_mem(&e, 1015808, 760000);
        assert_eq!(budget_total(s), 992 - 250 - 64);
        // 2: box that already runs two legacy sites + Caddy (~520 MB used at install time):
        // the budget shrinks honestly instead of promising RAM that is not there.
        s.store.set_setting("base_used_mb", "520").unwrap();
        assert_eq!(budget_total(s), 992 - 520 - 64);
        // 3: plan upgrade to 2 GB, no restart / no reconfiguration: budget follows MemTotal.
        set_mem(&e, 2031616, 1500000);
        assert_eq!(budget_total(s), 1984 - 520 - 64);
        // an explicit override still wins over the measurement
        s.store.set_setting("budget_override_mb", "900").unwrap();
        assert_eq!(budget_total(s), 900);
    }

    #[tokio::test]
    async fn upgrade_lets_a_previously_rejected_site_in() {
        let e = TestEnv::new();
        let s = &*e.state;
        s.store.set_setting("base_used_mb", "250").unwrap();
        set_mem(&e, 1015808, 760000); // budget 678
        create_site(s, req("a", "a.example.com", 384)).await.unwrap();
        let err = create_site(s, req("b", "b.example.com", 384)).await;
        assert!(err.is_err(), "768 MB does not fit a 678 MB budget");
        set_mem(&e, 2031616, 1700000); // upgraded to 2 GB
        create_site(s, req("b", "b.example.com", 384)).await.unwrap();
    }

    #[tokio::test]
    async fn create_validates_and_configures_gateway() {
        let e = TestEnv::new();
        let s = &*e.state;
        let dto = create_site(s, req("blog", "Blog.Example.com", 256))
            .await
            .unwrap();
        assert_eq!(dto.domain, "blog.example.com", "domain normalized");
        assert_eq!(dto.image_repo, "ghcr.io/o/blog");
        assert_eq!(dto.status, "created");
        assert!(e.gateway.sites().contains_key("blog"));
        for (r, why) in [
            (req("Bad_Name", "a.example.com", 256), "name"),
            (req("ok", "localhost", 256), "domain"),
            (req("ok", "a.example.com", 100), "ram step"),
            (req("ok", "a.example.com", 32), "ram min"),
            (req("ok", "a.example.com", 1024), "ram max"),
        ] {
            assert_eq!(create_site(s, r).await.unwrap_err().status, 400, "{why}");
        }
        let mut r = req("ok", "a.example.com", 256);
        r.image_repo = "docker.io/library/nginx".into();
        assert_eq!(
            create_site(s, r).await.unwrap_err().status,
            400,
            "only ghcr"
        );
        let mut r = req("ok", "a.example.com", 256);
        r.env.push(EnvVar {
            key: "1BAD".into(),
            value: "x".into(),
        });
        assert_eq!(create_site(s, r).await.unwrap_err().status, 400);
        // conflicts
        assert_eq!(
            create_site(s, req("blog", "other.example.com", 64))
                .await
                .unwrap_err()
                .status,
            409
        );
        assert_eq!(
            create_site(s, req("blog2", "blog.example.com", 64))
                .await
                .unwrap_err()
                .status,
            409
        );
    }

    #[tokio::test]
    async fn gateway_failure_rolls_back_the_site() {
        let e = TestEnv::new();
        e.gateway.set_down(true);
        let err = create_site(&e.state, req("blog", "blog.example.com", 128))
            .await
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(
            e.state.store.get_site_by_name("blog").unwrap().is_none(),
            "no half-created site"
        );
    }

    #[tokio::test]
    async fn budget_blocks_overcommit_unless_overridden() {
        let e = TestEnv::new();
        let s = &*e.state;
        let (_, b) = budget(s);
        assert_eq!(b, 992 - 250 - 64);
        create_site(s, req("a", "a.example.com", 512))
            .await
            .unwrap();
        let err = create_site(s, req("b", "b.example.com", 256))
            .await
            .unwrap_err();
        assert_eq!(err.code, "budget_exceeded");
        let mut r = req("b", "b.example.com", 256);
        r.override_budget = true;
        create_site(s, r).await.unwrap();
        // editing RAM is also checked, excluding the site itself
        let a = s.store.get_site_by_name("a").unwrap().unwrap();
        assert!(update_site(
            s,
            &a.id,
            UpdateSiteReq {
                ram_limit_mb: Some(480),
                ..Default::default()
            }
        )
        .await
        .is_err());
        assert!(update_site(
            s,
            &a.id,
            UpdateSiteReq {
                ram_limit_mb: Some(480),
                override_budget: Some(true),
                ..Default::default()
            }
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn dns_check_matches_public_ip() {
        let e = TestEnv::new();
        let s = &*e.state;
        s.store.set_setting("public_ip", "203.0.113.9").unwrap();
        let mut r = req("a", "a.example.com", 128);
        r.skip_dns_check = false;
        assert_eq!(
            create_site(s, r.clone()).await.unwrap_err().code,
            "dns_missing"
        );
        e.resolver.set("a.example.com", &["198.51.100.1"]);
        let err = create_site(s, r.clone()).await.unwrap_err();
        assert_eq!(err.code, "dns_mismatch");
        assert!(err.message.contains("198.51.100.1") && err.message.contains("203.0.113.9"));
        e.resolver
            .set("a.example.com", &["198.51.100.1", "203.0.113.9"]);
        create_site(s, r).await.unwrap();
    }

    #[tokio::test]
    async fn env_is_encrypted_masked_and_roundtrips() {
        let e = TestEnv::new();
        let s = &*e.state;
        let dto = create_site(s, req("a", "a.example.com", 128))
            .await
            .unwrap();
        // ciphertext at rest never contains the secret
        let raw = s.store.list_env(&dto.id).unwrap();
        assert!(!String::from_utf8_lossy(&raw[0].1).contains("postgres"));
        let masked = get_env(s, &dto.id, false).unwrap();
        assert!(masked.masked && masked.vars[0].value == MASK);
        let shown = get_env(s, &dto.id, true).unwrap();
        assert_eq!(shown.vars[0].value, "postgres://u:p@h/db");
        // UI sends the masked list back with one new var: the stored value is kept
        put_env(
            s,
            &dto.id,
            vec![
                EnvVar {
                    key: "DATABASE_URL".into(),
                    value: MASK.into(),
                },
                EnvVar {
                    key: "X".into(),
                    value: "1".into(),
                },
            ],
        )
        .unwrap();
        let shown = get_env(s, &dto.id, true).unwrap();
        assert_eq!(
            shown
                .vars
                .iter()
                .find(|v| v.key == "DATABASE_URL")
                .unwrap()
                .value,
            "postgres://u:p@h/db"
        );
        assert_eq!(shown.vars.len(), 2);
        // a masked value for an unknown key is refused instead of storing "********"
        assert!(put_env(
            s,
            &dto.id,
            vec![EnvVar {
                key: "NEW".into(),
                value: MASK.into()
            }]
        )
        .is_err());
        // ciphertext is bound to site and key: moving it elsewhere fails to open
        let blob = raw[0].1.clone();
        assert!(s.key.open_str(&blob, "env:other:DATABASE_URL").is_err());
    }

    #[test]
    fn effective_env_defaults_and_overrides() {
        let v = effective_env(&[], 3000, 256);
        assert!(v.contains(&"PORT=3000".to_string()));
        assert!(v.contains(&"HOSTNAME=0.0.0.0".to_string()));
        assert!(v.contains(&"NODE_OPTIONS=--max-old-space-size=192".to_string()));
        let v = effective_env(
            &[
                EnvVar {
                    key: "NODE_OPTIONS".into(),
                    value: "--max-old-space-size=64".into(),
                },
                EnvVar {
                    key: "PORT".into(),
                    value: "8080".into(),
                },
            ],
            3000,
            256,
        );
        assert!(
            v.contains(&"NODE_OPTIONS=--max-old-space-size=64".to_string()),
            "user wins"
        );
        assert!(v.contains(&"PORT=8080".to_string()));
        assert_eq!(v.iter().filter(|x| x.starts_with("PORT=")).count(), 1);
    }

    #[tokio::test]
    async fn tokens_are_per_site_and_shown_once() {
        let e = TestEnv::new();
        let s = &*e.state;
        let a = create_site(s, req("a", "a.example.com", 128))
            .await
            .unwrap();
        let b = create_site(s, req("b", "b.example.com", 128))
            .await
            .unwrap();
        let t = create_token(s, &a.id).unwrap();
        assert!(t.token.starts_with("rt_"));
        let listed = list_tokens(s, &a.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            !serde_json::to_string(&listed).unwrap().contains(&t.token),
            "raw token never listed"
        );
        assert!(
            revoke_token(s, &b.id, &t.id).is_err(),
            "site B cannot revoke site A's token"
        );
        revoke_token(s, &a.id, &t.id).unwrap();
        assert!(list_tokens(s, &a.id).unwrap()[0].revoked_at.is_some());
    }

    #[tokio::test]
    async fn registry_secret_is_sealed() {
        let e = TestEnv::new();
        let s = &*e.state;
        let r = create_registry(
            s,
            CreateRegistryReq {
                name: "gh".into(),
                username: "me".into(),
                secret: "ghp_supersecret".into(),
            },
        )
        .unwrap();
        let row = s.store.get_registry(&r.id).unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&row.secret_enc).contains("supersecret"));
        assert_eq!(registry_auth(s, &r.id).unwrap().password, "ghp_supersecret");
        assert!(!serde_json::to_string(&list_registries(s).unwrap())
            .unwrap()
            .contains("supersecret"));
        assert!(create_registry(
            s,
            CreateRegistryReq {
                name: "".into(),
                username: "me".into(),
                secret: "x".into()
            }
        )
        .is_err());
    }

    #[tokio::test]
    async fn delete_cleans_route_and_containers() {
        let e = TestEnv::new();
        let s = &*e.state;
        let a = create_site(s, req("a", "a.example.com", 128))
            .await
            .unwrap();
        e.runtime.add_foreign_container(
            "rustle-a-1",
            &[("rustle.site", a.id.as_str()), ("rustle.managed", "1")],
        );
        let other = e
            .runtime
            .add_foreign_container("rustle-b-1", &[("rustle.site", "someone-else")]);
        delete_site(s, &a.id).await.unwrap();
        assert!(!e.gateway.sites().contains_key("a"));
        let left: Vec<String> = e.runtime.containers().into_iter().map(|c| c.id).collect();
        assert_eq!(left, vec![other], "only this site's containers are removed");
        assert_eq!(delete_site(s, &a.id).await.unwrap_err().status, 404);
    }

    #[test]
    fn webhook_image_must_match_registered_repo() {
        let site = SiteRow {
            id: "1".into(),
            name: "a".into(),
            domain: "a.example.com".into(),
            image_repo: "ghcr.io/o/a".into(),
            container_port: 3000,
            ram_limit_mb: 128,
            status: "x".into(),
            current_deployment_id: None,
            registry_id: None,
            zero_downtime: false,
            compat_caps: false,
            created_at: 0,
            updated_at: 0,
        };
        assert!(image_matches_site(&site, "ghcr.io/o/a:sha1").is_ok());
        assert!(image_matches_site(&site, "GHCR.IO/o/a:latest").is_ok());
        assert_eq!(
            image_matches_site(&site, "ghcr.io/o/other:1")
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            image_matches_site(&site, "ghcr.io/attacker/a:1")
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            image_matches_site(&site, "evil.com/o/a:1")
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            image_matches_site(&site, "not an image")
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn templates_embed_the_registered_repo_and_never_a_token() {
        let y = workflow_yaml("ghcr.io/o/a", false);
        assert!(y.contains("IMAGE: ghcr.io/o/a"));
        assert!(y.contains("secrets.RUSTLE_TOKEN") && y.contains("secrets.RUSTLE_URL"));
        assert!(!y.contains("--insecure"));
        assert!(workflow_yaml("ghcr.io/o/a", true).contains("--insecure"));
        assert!(dockerfile(8080).contains("PORT=8080") && dockerfile(8080).contains("EXPOSE 8080"));
        let st = dockerfile_static(3000);
        assert!(
            st.contains(":3000 {")
                && st.contains("EXPOSE 3000")
                && st.contains("file_server")
                && st.contains("FROM caddy:2-alpine")
        );
        assert!(!st.contains("__PORT__"));
        assert!(!y.contains("rt_"));
    }
}
