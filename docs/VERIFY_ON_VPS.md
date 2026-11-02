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
| 21 | Caddy accepts the admin API call from Rustle: the unix-socket request carries `Host: 127.0.0.1` (Caddy rejects other Host values on its admin endpoint) | Create a site; `journalctl -u rustle` shows no `gateway` errors | If Caddy still answers `403 host not allowed`, adjust the host sent in `httpc.rs` |
| 22 | Port 80 never serves a site or the dashboard over plain HTTP, only the redirect and ACME challenges (`listen` is `:443` only) | `curl -i http://<ip>/` and `http://<site-domain>/` | Add an explicit redirect server on :80 |
| 23 | Bare-IP HTTPS: `certificates.automate` + `tls internal` + `default_sni` produce a working certificate | open `https://<ip>`, accept the warning once | Use a `<ip>.sslip.io` name and a normal certificate |
| 24 | The host can reach the proxy at `172.31.0.2:2375` (own bridge, nothing published) and tenants cannot | e2e "isolation" checks | Fall back to publishing on loopback plus an `iptables -t raw` DROP for non-loopback sources |
| 25 | The Docker proxy tag `tecnativa/docker-socket-proxy:0.3.0` exists and `POST=1` also permits `DELETE` | `docker pull`, then a delete through the proxy | Pin the correct tag, or use `wollomatic/socket-proxy` |
| 26 | Updating: tag `v0.0.1` then `v0.0.2`; break `v0.0.2` on purpose; the previous binary comes back after 4 failed starts and only while `bin/update-pending` exists | `ls /var/lib/rustle/bin`, `journalctl -t rustle` | n/a |
| 27 | Shared Caddy: `curl localhost:2019/config/apps/http/servers` on a Caddyfile-built Caddy shows one server (`srv0`) listening on :443 and Rustle picks it | install with `--use-existing-caddy`, then `journalctl -u rustle` | Set `RUSTLE_CADDY_SERVER` |
| 28 | A route inserted at index 0 with `@id` and a host matcher takes precedence over the Caddyfile's own route for the same host, and removing it restores the old one | create a site for a domain the old Caddyfile serves, deploy, then delete the site | Inspect the route order in `/config/apps/http/servers/<srv>/routes` |
| 29 | New hostnames added through the API get certificates from a Caddyfile-built Caddy | add a site on a new domain | Check `apps.tls.automation` in the running config |
| 30 | `caddy reload` drops Rustle's routes and Rustle restores them (within 60 s, immediately after `systemctl restart rustle`) | reload while a site is live | n/a |
| 31 | Container IP changes after `docker restart` / reboot are picked up; the old address never serves another site | reboot with 3 sites, compare each domain's content | Pin per-site container addresses |
| 32 | Installing next to a Docker that runs other containers does not restart Docker | `docker ps` uptime before/after | n/a |
| 33 | A Caddy container with `admin unix//run/rustle/caddy-admin.sock|0666` and `/run/rustle` mounted is reachable by Rustle (user `rustle`) and not by containers on `rustle-net` | `curl --unix-socket` from the host as `rustle`; from a tenant container | Check directory mode `0750` and the socket mode |
| 34 | The Caddy container, joined to `rustle-net`, resolves Rustle container names and reaches the dashboard through `unix//run/rustle/rustle.sock` | open a migrated site and the dashboard domain | n/a |
| 35 | `systemctl kill -s HUP rustle` after `caddy reload` restores routes immediately; no SIGHUP before the handler is installed can kill Rustle | reload, then request a Rustle-managed domain | n/a |
| 36 | A static-export image (`FROM caddy:2-alpine`, listens on 3000) runs under `cap_drop=ALL`, `no-new-privileges` and a 64 MB limit and passes the health check | deploy `Dockerfile.static.tmpl`'s image | Enable `compat_caps` for that site |

