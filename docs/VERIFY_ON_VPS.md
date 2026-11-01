# Verify on a real VPS

The build sandbox had no Docker daemon, Caddy, systemd or GitHub access, so everything below was implemented against
documented behaviour and mocks but **not observed**. Run `scripts/e2e-real.sh` (it covers items 4-9 automatically) and walk
through the rest on a fresh Ubuntu 24.04 VPS (1 vCPU / 1 GB).

| # | What to check | How | If it fails |
|---|---|---|---|
| 1 | Admin RSS < 8 MB idle, < 48 MB always | `systemctl status rustle` (Memory:), `grep VmRSS /proc/$(pgrep -x rustle)/status` after 24 h | Lower `Tokio` thread stack, check `MALLOC_ARENA_MAX`; raise the target only with evidence |
| 2 | Base RAM of OS + Docker + proxy + Caddy | `free -m` right after install with no sites | The SAD target (< 200 MB) may be unrealistic; use `--slim`, or set the budget override in Settings |
| 3 | `install.sh` end to end, twice (idempotency) | `sudo ./install.sh --domain ...` then again | Fix the failing step; the script is `shellcheck`-clean and dry-run tested only |
| 4 | Socket proxy denies `exec`, volumes, networks, system | `scripts/e2e-real.sh` ("socket proxy ACLs") | tecnativa/docker-socket-proxy maps ACLs by path: if `exec` is allowed because it lives under `/containers`, switch to `wollomatic/socket-proxy` with an explicit method+path allow-list |
| 5 | Proxy allows what Rustle needs: `DELETE /containers`, `DELETE /images`, `/images/json`, `/containers/json`, `.../logs`, `/images/create` | e2e + a manual deploy | Tune `POST`/`ALLOW_*` variables |
| 6 | Caddy Admin API on a unix socket works: `PUT .../routes/0`, `PATCH /id/up-<name>/upstreams`, `GET /id/...` for unknown ids (404 vs 500) | e2e; `journalctl -u rustle` for `gateway` errors | Adjust `HttpGateway::exists` status mapping |
| 7 | Caddy `--resume` keeps routes across a container restart | `docker restart rustle-caddy`, request a site | Reconcile re-pushes routes within 60 s regardless |
| 8 | Tenants cannot reach the admin API, proxy or Caddy admin | e2e ("isolation") | Do not enable the service until fixed |
| 9 | Containers get a hard 128 MiB limit, no swap, no caps | e2e ("container hardening") | n/a |
| 10 | `Memory` cgroup path detection (`system.slice/docker-<id>.scope`) | Dashboard shows per-site RAM | Check `docker info | grep -i cgroup` (driver must be `systemd`) |
| 11 | Hardened unit starts on 22.04 and 24.04 | `systemctl status rustle`; `systemd-analyze security rustle` | Remove the offending directive (candidates: `SystemCallFilter=~@privileged @resources`, `MemoryDenyWriteExecute`, `ProtectProc`) and report which |
| 12 | `/run/rustle` is shared correctly (Rustle creates `rustle.sock`, Caddy creates `caddy-admin.sock`, both reachable) | `ls -l /run/rustle` | Check tmpfiles and `RuntimeDirectoryPreserve` |
| 13 | IP-mode TLS (`tls internal` on a bare IP, `default_sni`) | open `https://<ip>` | Caddy may need an explicit policy matching the IP; consider a `*.sslip.io` name instead |
| 14 | Let's Encrypt issuance for a real domain; port 80 reachable | add a site, open it | Open 80/443 in the provider firewall (Lightsail blocks by default) |
| 15 | Private GHCR pull with a PAT | add a registry login, deploy a private image | Check the token scope `read:packages` |
| 16 | Backup to a private repo; restore on a clean box | Settings > Backup, then `rustle restore` | Contents API limits apply (100 MB); the DB is tiny |
| 17 | Signed self-update and automatic revert | publish two tagged releases; break the second on purpose | See RELEASING.md |
| 18 | Reboot recovery | `reboot`; sites, routes and the dashboard return unattended | Check `Restart=always`, Docker `live-restore`, `unless-stopped` |
| 19 | Zero-downtime deploy under load, with RAM headroom | enable per site, `hey`/`wrk` while deploying | Keep it off by default until this passes |
| 20 | `memory_swappiness` (omitted by design) | n/a | The original spec required `0`; cgroup v2 has no such control |
