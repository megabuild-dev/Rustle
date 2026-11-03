# Rustle – Implementation Plan (v1)

Source of truth: PRD and SAD (Claude Docs artifacts, approved 2026-11-01).
PRD: product requirements document (kept outside this repository; its decisions are summarised in this plan)
SAD: software architecture document (kept outside this repository; later changes are in DECISIONS_ADDENDUM.md)
Goal that overrides everything: **minimum RAM** (admin idle < 8 MB target, hard cap 48 MB).

## 0. Constraints of the build environment (what can and cannot be verified)

| Item | Status in CI sandbox | Consequence |
|---|---|---|
| Rust stable, wasm32 target, crates.io | available | full unit/integration tests, wasm build |
| Docker daemon, Caddy, systemd, real VPS | **not available** | Docker and Caddy are behind traits with in-memory fakes; real adapters are compiled and unit-tested against recorded/mocked HTTP only. RSS target, cgroup behaviour, install.sh on Ubuntu, proxy ACLs are listed in `docs/VERIFY_ON_VPS.md` as manual checks |
| Network to GitHub API | not exercised | GitHub client tested with a local mock HTTP server |

Everything claimed "done" must be backed by `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, `shellcheck` (if available) or be explicitly listed as unverified.

## 1. Repository layout

```
Cargo.toml                workspace, [profile.release] opt-level="z", lto="fat", codegen-units=1, panic="abort", strip=true
crates/rustle-types/      serde DTOs shared by server and UI (no tokio, wasm-safe)
crates/rustle-server/     binary `rustle` (+ lib for tests)
crates/rustle-ui/         Leptos CSR app -> wasm; excluded from default-members
ui-dist/                  built UI assets embedded by rust-embed (placeholder index.html committed)
install/install.sh        idempotent bootstrap
deploy/                   rustle.service, daemon.json, docker-compose.dev.yml, caddy base config JSON
docs/                     PRD.md, SAD.md (exported), IMPLEMENTATION_PLAN.md, VERIFY_ON_VPS.md, DEV.md
.github/workflows/        ci.yml (fmt, clippy, test, wasm build, shellcheck), release.yml (musl? gnu x86_64+aarch64, checksums)
```

## 2. Server crate: modules and contracts

Runtime: tokio `current_thread`. Single SQLite connection behind `std::sync::Mutex`, WAL, `cache_size=-2000`, `mmap_size=0`; DB calls run inline (short) or `spawn_blocking` for backup.

| Module | Responsibility | Key types |
|---|---|---|
| `config` | env parsing (`RUSTLE_BIND`, `RUSTLE_DB`, `DOCKER_HOST`, `CADDY_ADMIN_URL`, `RUSTLE_MASTER_KEY_FILE`, `RUSTLE_JWT_SECRET`, `RUSTLE_ADMIN_UPSTREAM`, `RUSTLE_PUBLIC_IP`) | `Config` |
| `crypto` | AES-256-GCM seal/open, SHA-256 token hashing, constant-time compare, random tokens, Argon2id (m=8 MiB,t=3,p=1) | `MasterKey` |
| `store` | migrations (`PRAGMA user_version`), typed queries for all tables | `Store` |
| `auth` | login with lockout (5 fails -> 15 min), JWT HS256 12 h in HttpOnly SameSite=Strict cookie, CSRF double-submit header, `token_version` revocation, deploy-token verification | `AuthLayer` |
| `docker` | `trait Runtime` (pull, create, start, stop, remove, inspect, list_by_label, logs, mem_current, prune_images, run_oneshot); `BollardRuntime`; `FakeRuntime`; **`host_config()` is the only constructor of HostConfig** | `ContainerSpec` |
| `caddy` | `trait Gateway` (ensure_base, upsert_route, remove_route, set_upstream, list_routes, admin_route); `HttpGateway` (reqwest, route `@id` ops); `FakeGateway` | `Route` |
| `sites` | create/update/delete, validation (regex names, domain, RAM 64..=512 step 32), RAM budget, DNS check (trait `Resolver`) | `SiteService` |
| `deploy` | per-site mutex, state machine pending->running/failed/superseded, strategies stop-first (default) and start-first (opt-in, only if budget allows), health check (TCP/HTTP via trait), rollback, history retention (50) | `Deployer` |
| `envvars` | encrypted CRUD, `NODE_OPTIONS` auto-tune (FR-25) | |
| `registries` | encrypted credentials, `X-Registry-Auth` builder (FR-20) | |
| `workflow` | GitHub Actions YAML and Dockerfile template (FR-11, FR-26) | |
| `scheduler` | own 5-field cron parser/matcher, 30 s tick, system jobs (prune images, journal note, backup, update-check), site jobs via `run_oneshot` (argv only) | `Scheduler` |
| `metrics` | /proc/meminfo, /proc/stat, /proc/uptime, statvfs, own RSS from /proc/self/status, per-container `memory.current` from cgroup v2 path (`/sys/fs/cgroup/system.slice/docker-<id>.scope/`) | `Sampler` |
| `stream` | SSE for stats (3 s) and logs; only samples while >=1 subscriber | |
| `backup` | `VACUUM INTO` -> AES-GCM -> GitHub Contents API (PUT), restore CLI; never touches master.key (FR-22) | `GithubClient` trait |
| `update` | GitHub Releases check, download to `/var/lib/rustle/bin/rustle.new`, verify sha256 (+ signature hook), atomic rename, keep `.prev`, exit for systemd restart (FR-23) | |
| `reconcile` | boot: base Caddy config, routes from DB, orphan cleanup by label `rustle.site`, stuck `pending` -> `failed` | |
| `api` | axum routers per SAD §5, JSON errors `{error:{code,message}}`, 64 KB body limit, login rate limit | |
| `ui_assets` | rust-embed, serves `.br` when accepted, SPA fallback, security headers | |
| `main` / CLI | `rustle` (serve), `rustle init` (create admin + secrets, prints once), `rustle restore <file>`, `rustle key export`, `rustle version` | |

### 2.1 HostConfig whitelist (security-critical)

Inputs allowed: container name, image ref (validated against `sites.image_repo`), env list, RAM limit, label set. Output always: `memory=limit`, `memory_swap=limit` (no swap), `memory_swappiness` omitted by default (cgroup v2), `pids_limit=512`, `cap_drop=[ALL]`, `security_opt=[no-new-privileges]`, `privileged=false`, no binds/devices/volumes, `NetworkMode="rustle-net"`, `restart_policy=unless-stopped`, log config json-file max-size 10m max-file 3, labels `rustle.site=<id>`, `rustle.deployment=<id>`. A unit test asserts the serialized HostConfig for forbidden keys.

### 2.2 Database schema (migration 1)

`admin`, `registries`, `sites`, `site_env`, `deploy_tokens`, `deployments`, `cron_jobs`, `cron_runs`, `audit_log`, `settings`, `backup_runs` exactly as SAD §4 plus: `sites.zero_downtime`, `sites.registry_id`, `admin.token_version`, `cron_jobs.system_key`. Foreign keys ON, delete cascades from sites.

### 2.3 Deploy state machine

```
POST /webhooks/deploy -> auth token -> site -> image repo match -> lock site (409 if busy) -> insert deployment(pending) -> 202
  background: pull(auth) -> budget? -> [start-first | stop-first]
     start-first: create new(name suffix) -> start -> health -> upsert upstream -> stop+remove old -> running
     stop-first : stop old -> create new -> start -> health -> upstream -> running ; on failure recreate old from previous deployment image
  failure: remove new, restore old if needed, deployment=failed(error)
  success: previous running => superseded; prune retention
```

Container naming: `rustle-<site>` stable alias via network alias; versioned name `rustle-<site>-<deployment8>`; Caddy upstream dials `rustle-<site>-<deployment8>:<port>` and is switched with `PATCH /id/site-<name>`.

## 3. UI crate (Leptos CSR)

Screens per PRD §7: Login, Overview, Sites, Site detail (tabs: Overview, Deploys, Env, Tokens, Logs, Workflow), Add-site wizard (4 steps), Cron, Settings (password, registries, backup, updates, prune). `gloo-net` fetch with CSRF header; SSE via `web_sys::EventSource`; no JS deps; dark theme CSS hand-written (<10 KB). Build with `cargo build --target wasm32-unknown-unknown --release` + `wasm-bindgen --target web` + optional `wasm-opt -Oz` + brotli, output to `ui-dist/`.

## 4. Milestones and acceptance

| M | Scope | Acceptance (all automated unless marked) |
|---|---|---|
| M0 | scaffold, types, CI skeleton, docs export | `cargo build`, `fmt`, `clippy` clean |
| M1 | config, crypto, store, auth | tests: crypto round trip, tamper detect, lockout, JWT expiry/revocation, CSRF, token hash |
| M2 | docker trait+fake+bollard adapter, host_config, caddy trait+fake+http adapter, reconcile | tests: HostConfig forbidden-key test, caddy request shapes against mock server, reconcile repairs drift |
| M3 | sites, env, tokens, budget, DNS, workflow, registries | API tests via `tower::ServiceExt` on in-memory DB |
| M4 | deploy, webhook, rollback, history | tests: success, health failure, stop-first restore, start-first budget refusal, 409 concurrency, wrong-repo 403, rollback |
| M5 | scheduler, system jobs, site cron, metrics, SSE | cron matcher table tests; one-shot argv; metrics parse fixtures |
| M6 | backup/restore, key export, update, IP-mode TLS config | mock GitHub server tests; restore round trip |
| M7 | UI | `cargo build --target wasm32` passes, bundle size reported; manual visual check unverified |
| M8 | install.sh, systemd, daemon.json, compose dev, release workflow, VERIFY_ON_VPS.md | `shellcheck`/`bash -n`; dry-run mode with `RUSTLE_INSTALL_DRY_RUN=1` printing steps |

## 5. Explicit non-goals in this pass
FR-27 (sleep idle sites) is documented but not implemented (Could, P3). Multi-user, volumes, managed DBs, registries other than GHCR stay out of scope.

## 6. Verification still required on a real VPS
See `docs/VERIFY_ON_VPS.md` (created in M8): RSS after 24 h, `free -m` base, proxy ACL behaviour (DELETE, prune, logs, stats), `memory_swappiness` on cgroup v2, Caddy admin API reachability, bind to `172.30.0.1` at boot, IP-mode TLS, Let's Encrypt, reboot recovery, start-first load test.

## 7. Revision 2 – outcomes of the Opus advisor review (binding, overrides sections above)

| # | Change | Reason |
|---|---|---|
| R1 | **Topology**: Caddy Admin API on unix socket `/run/rustle/caddy-admin.sock` (mode 0666 inside a dir `/run/rustle` owned `rustle:rustle` 0750), Rustle serves Caddy via unix socket `/run/rustle/rustle.sock`; nothing sensitive listens on `rustle-net`; no bind on `172.30.0.1`. Optional loopback TCP `127.0.0.1:3000` for SSH-tunnel fallback and dev. Socket proxy published on `127.0.0.1:2375` only | tenants share `rustle-net`; a TCP admin API there would let any app rewrite Caddy or reach the API; also removes the boot-time bind race |
| R2 | Own thin **HTTP/1 client on hyper** (unix, tcp, tls via tokio-rustls+ring) used for Docker (through the proxy), Caddy and GitHub. `bollard` and `reqwest` are dropped | one HTTP stack, smaller RSS/binary, reqwest cannot do unix sockets, trivially testable against a local mock server. Spec said bollard as a means; goal is RAM |
| R3 | Socket proxy limits are path+method only; a compromised Rustle is root-equivalent. Documented in `docs/THREAT_MODEL.md`. `EVENTS` dropped from proxy (unused); Caddy drift handled by a 60 s reconcile tick | least privilege |
| R4 | Login: per-IP exponential backoff (client IP from `X-Forwarded-For` last hop, only Caddy can connect) instead of account lockout; global semaphore(1) around Argon2 | avoids remote DoS lockout and N x 8 MiB heap |
| R5 | Self-update: **mandatory ed25519 signature** (ring) with compiled-in public key (update disabled when key absent); boot-attempt counter + `ExecStartPre` script reverts to `rustle.prev` after 3 failed boots; apply waits for in-flight deploys | checksum from same origin proves nothing |
| R6 | JWT and CSRF keys derived from `master.key` via HKDF; no secret env vars. Rustle runs as user `rustle`, not in docker group, writes only `/var/lib/rustle` and `/run/rustle` | no secrets in `/proc/*/environ` |
| R7 | Backup uses a **second read-only SQLite connection** in `spawn_blocking`; the main mutex is never held across blocking work. Timestamped backup files, keep 14, list+delete via Contents API | no event-loop stall; Contents API needs blob sha for updates |
| R8 | Deploy: record image **digest** after pull and run containers by `repo@digest`; keep the old container **stopped, not removed** until new is healthy; prune excludes digests of last 3 deployments; **latest-wins queue depth 1** per site instead of 409; health check via container IP from inspect; no network alias | determinism, rollback correctness |
| R9 | Caddy: site routes inserted at index 0, admin route (`@id: admin`) always last; `@id` on both route (`site-<name>`) and reverse_proxy handler (`up-<name>`); upstream switch = `PATCH /id/up-<name>/upstreams`; Caddy run with `--resume` and persistent `/config` + `/data` | route ordering, persistence |
| R10 | cgroup path autodetect (systemd scope vs cgroupfs); `cap_drop=ALL` default with per-site `compat_caps` opt-in (CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, NET_BIND_SERVICE) | nginx-like images |
| R11 | Image refs: real parser/normalizer, host must be `ghcr.io` in MVP | bypass-proof repo match |
| R12 | `POST /sites/:id/redeploy`; deployments store an **encrypted env snapshot**; rollback restores that snapshot | env/registry changes need a trigger; rollback fidelity |
| R13 | Cron: UTC, 15 s tick with per-job last-fired minute (persisted), no catch-up, overlap skipped, one-shot timeout 600 s then kill, no AutoRemove (wait -> logs -> remove), one-shot RAM checked against budget | double-fire/skip, races |
| R14 | Retention: audit 90 d, cron_runs 50/job, backup_runs 30, daily housekeeping | DB growth |
| R15 | Installer owns `rustle-net`; reconcile reports `degraded` loudly if missing. `rustle init` forces an acknowledgement that `master.key` was saved (`RUSTLE_ACK_KEY=1` for non-interactive) | NETWORKS=0; useless-backup risk |
| R16 | Graceful SIGTERM; webhook rate limit 30/min per token; tokio `max_blocking_threads=2`, small blocking stack, SQLite `cache_size=-512`, tracing without regex filter, `MALLOC_ARENA_MAX=1` | RAM |
| R17 | CI: RSS smoke gate in sandbox (`dev-fakes` feature serves with fake backends, N requests, read VmRSS) + e2e workflow with real Docker/proxy/Caddy for GitHub runners (cannot be run in the build sandbox, flagged unverified) | verify assumptions early |
