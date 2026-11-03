# Changelog

All notable changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project follows [Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may change behaviour).

## [Unreleased]

### Added
- First public code: admin server (axum, SQLite, current-thread tokio), embedded Leptos/WebAssembly dashboard, installer,
  hardened systemd unit, signed self-update, encrypted GitHub backups, cron (system, one-shot container, HTTP).
- Deploy pipeline: pull by digest, health check, stop-first or optional start-first (zero downtime), rollback with the
  environment of that deployment, latest-wins queue per site.
- Caddy gateway through the Admin API, in two modes: Rustle's own Caddy, or sharing an existing Caddy
  (`--use-existing-caddy`), including `rustle import-site-ctl` for `site-ctl.sh` setups.
- MIT licence, security policy, contributing guide, code of conduct, issue and pull request templates.
- Quality gates: CRAP score below 30 for every function (CI), mutation testing over the server crate (weekly workflow).

### Known limitations
- Not yet verified on a real VPS; see [docs/VERIFY_ON_VPS.md](docs/VERIFY_ON_VPS.md) and [docs/STATUS.md](docs/STATUS.md).
