# Rustle

An ultra-light, self-hosted PaaS written in Rust for small VPSs (1 vCPU / 1 GB RAM). Build on GitHub Actions, deploy to your
own server, keep the platform itself under 8 MB of RAM.

* **Tiny:** one 5 MB binary with the dashboard embedded (WebAssembly, 226 KB over the wire). Measured idle RSS 5.8 MB.
* **Safe by default:** the Docker socket is never mounted into Rustle; it talks to a filtering proxy. Every container gets a hard
  RAM limit, no capabilities, no published ports. Details in [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).
* **Simple deploys:** `git push` -> GitHub Actions builds an image -> a per-site token triggers Rustle -> pull by digest, health
  check, switch the Caddy route, keep the previous version for one-click rollback. Automatic HTTPS through Caddy.
* **Stateless apps** (Next.js / Node) from public or private GHCR images. Databases live elsewhere (Neon, Supabase, ...).

## Install (Ubuntu 22.04 / 24.04, run as root on the VPS)

```bash
curl -fsSL https://github.com/megalabs-dev/rustle/releases/latest/download/rustle-installer.tar.gz | tar xz
sudo ./install/install.sh --domain admin.example.com --email you@example.com
```

The installer creates swap, installs Docker, starts the socket proxy and Caddy, installs the signed-release binary, prints the
**master key** (save it: without it backups cannot be restored) and the admin password, and starts the service.
Without `--domain` the dashboard is served on `https://<server-ip>` with a self-signed certificate until you set one in Settings.
Already running sites behind a Caddy (host service or container)? Use `--use-existing-caddy` (see [docs/COEXISTENCE.md](docs/COEXISTENCE.md)): your sites stay untouched.
Private repository? See [docs/RELEASING.md](docs/RELEASING.md).

## Documents

| | |
|---|---|
| Product requirements | https://claude.ai/artifact/RfEb9FujTLZR3wWBfDdwwC |
| Architecture | https://claude.ai/artifact/GCo2xkuBRXUZArR8iNJSuZ |
| What was built and what is verified | [docs/STATUS.md](docs/STATUS.md) |
| What still has to be checked on a real VPS | [docs/VERIFY_ON_VPS.md](docs/VERIFY_ON_VPS.md) |
| Implementation plan and the review that changed it | [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md) |
| Design changes against the approved SAD | [docs/DECISIONS_ADDENDUM.md](docs/DECISIONS_ADDENDUM.md) |
| Development, configuration, CLI | [docs/DEV.md](docs/DEV.md) |
| Installing next to existing sites and Caddy | [docs/COEXISTENCE.md](docs/COEXISTENCE.md) |
| Threat model | [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) |
| Releasing and signing | [docs/RELEASING.md](docs/RELEASING.md) |

## Status

MVP complete in code and tested without Docker (fakes and mock servers: 142 unit/API tests, a headless-browser test of the
dashboard, an RSS gate). **Not yet run on a real VPS**, see `docs/VERIFY_ON_VPS.md`. `FR-27` (sleep idle sites) is not implemented.
