# Implementation status

Legend: **done** = implemented and covered by automated tests in this repository.
**unverified** = implemented but depends on real Docker/Caddy/systemd/GitHub behaviour that could not be exercised
in the build sandbox; it is listed in [VERIFY_ON_VPS.md](VERIFY_ON_VPS.md).

| ID | Requirement | Status |
|---|---|---|
| FR-01 | Idempotent `install.sh` (cgroup v2 check, swap, Docker, proxy, Caddy, systemd) | done (dry-run + shellcheck); a real run is unverified |
| FR-02 | Generated JWT/master key and admin password, shown once | done |
| FR-03 | Admin login, password change, secret never shown | done. Deviation: per-IP exponential backoff replaces account lockout (an account lockout is a remote DoS on the only admin) |
| FR-04 | Per-site deploy tokens (create/revoke, hashed, shown once) | done |
| FR-05 | Create site (name, domain, image, RAM 64-512, port, env) | done |
| FR-06 | Hard limits on every container | done. `memory_swappiness` is intentionally not sent (not supported by cgroup v2) |
| FR-07 | Caddy route + automatic TLS, DNS check | done against a mock Admin API; real Caddy unverified |
| FR-08 | Start / stop / restart / delete | done |
| FR-09 | Environment variables (encrypted, masked, editable) | done |
| FR-10 | Container logs (tail + SSE) | done (UI polls every 3 s) |
| FR-11 | GitHub Actions workflow generation | done |
| FR-12 | Webhook deploy with health check and route switch | done |
| FR-13 | Rollback (image digest + env snapshot) | done |
| FR-14 | Deployment history (50) | done |
| FR-15 | Real host stats | done |
| FR-16 | Per-container RAM from cgroup v2 | done; path detection for systemd and cgroupfs layouts, unverified on a real host |
| FR-17 | RAM budget warning / block | done |
| FR-18 | System jobs (image prune, housekeeping, backup, update check) | done. Journal size is capped by the installer (journald drop-in) instead of a job |
| FR-19 | Per-site one-shot jobs (argv only, no shell, no exec; image ENTRYPOINT is cleared) | done. Log tail per run is 16 KB (PRD said 64 KB) to bound database and backup size |
| FR-20 | Private GHCR images via registry login | done (credentials sealed; `X-Registry-Auth` verified against a mock Docker API) |
| FR-21 | Dashboard without a domain (self-signed HTTPS on the IP) | done as configuration; unverified in a real browser against real Caddy |
| FR-22 | Encrypted backup to a private GitHub repo + `rustle restore` | done against a mock GitHub API |
| FR-23 | Signed self-update with automatic revert | done (ed25519, atomic swap, boot-attempt guard); needs release signing keys, see [RELEASING.md](RELEASING.md) |
| FR-24 | Optional zero-downtime deploys | done, default off; needs a load test on a 1 GB VPS |
| FR-25 | `NODE_OPTIONS` auto-tuning | done |
| FR-26 | Dockerfile template | done |
| NFR | Coexist with an existing Caddy (`--use-existing-caddy`) | done in code and unit tests (gateway, deploy, reconcile); unverified against a real Caddyfile-built Caddy |
| FR-27 | Sleep idle sites | **not implemented** (planned P3, "Could") |

## Measured

Release build with the dashboard embedded, fake Docker/Caddy backends, after a session of 3 sites, 3 deploys and 800 API calls
(`scripts/rss_smoke.py`):

| Metric | Value | Target |
|---|---|---|
| Idle RSS | 5.8 MB | < 8 MB |
| RSS after load | 6.8 MB | < 8 MB |
| Peak RSS (during an Argon2 login) | 14.7 MB | < 48 MB hard cap |
| Binary size (with dashboard) | 5.0 MB | n/a |
| Dashboard transfer size (brotli) | 226 KB | < 1 MB |

These do not include the real Docker/Caddy clients talking to live daemons (they add buffers, not threads); re-measure on the VPS
(`docs/VERIFY_ON_VPS.md`, item 1).

## Review

An independent senior reviewer (Opus) examined the plan before coding and the finished code afterwards; all 6 HIGH and most MEDIUM
findings were fixed with tests (see `docs/DECISIONS_ADDENDUM.md`). Known leftovers:

* The installer verifies `SHA256SUMS` only; the Ed25519 signature is checked by the server when it updates itself, not by `install.sh`.
* A one-shot container orphaned by a restart is swept once it has exited; a hung one keeps its RAM until the next boot.
* `compat_caps` and per-site resource edits take effect on the next deploy (the UI says so).
* Tenant containers can still reach each other on `rustle-net`.
