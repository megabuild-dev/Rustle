# Design changes against the approved SAD

Made during implementation after review by an independent senior reviewer; each one is a deviation from the approved SAD/PRD text.

| Area | SAD / PRD said | Implemented | Why |
|---|---|---|---|
| Caddy Admin API | TCP `127.0.0.1:2019` published from the Caddy container | unix socket `/run/rustle/caddy-admin.sock` in a 0750 directory | A TCP admin API inside `rustle-net` is reachable by every tenant container |
| How Caddy reaches Rustle | Rustle binds `172.30.0.1:3000` | unix socket `/run/rustle/rustle.sock` (+ optional loopback TCP for SSH tunnels) | Same tenant-reachability problem; removes the boot-time bind race on the bridge address |
| Docker client | `bollard` | small hyper-based HTTP client for the ~14 endpoints used | One HTTP stack for Docker, Caddy and GitHub, lower RSS and binary size, testable against a local mock server |
| Login protection | lock the account after 5 failures | per-IP exponential backoff, Argon2 gate | Account lockout lets anyone lock the only admin out |
| Proxy `EVENTS` | `EVENTS=1` | `EVENTS=0` | Nothing consumes events; drift is repaired by a 60 s reconcile |
| `builder prune` | weekly | dropped | Nothing is built on the VPS |
| Image prune | Docker `images/prune` | custom prune that never removes images of the last 3 successful deployments | Prune would delete rollback targets |
| Deploy conflicts | `409` while busy | latest-wins queue of depth 1 | A push during a deploy must not be lost |
| Deploy order | start new then stop old | stop-first default; old container kept stopped until the new one is healthy; start-first opt-in | 1 GB cannot hold two copies; failed stop-first restarts the old version |
| Container image reference | tag | immutable `repo@sha256:` recorded after pull | Deterministic rollback |
| `memory_swappiness=0` | required | not sent | cgroup v2 has no such control; containers get `memory_swap = memory` instead |
| Secrets | JWT secret generated at install | JWT/CSRF keys derived (HKDF) from the master key | One secret file, nothing in the process environment |
| Backup/update TLS | "no TLS client in admin" | rustls (ring) used only for GitHub | Needed for FR-22/FR-23; the cost is code size, not RSS |
| Dashboard distribution | React SPA | Leptos WebAssembly | Decided with the product owner |
| Update trust | checksum | mandatory Ed25519 signature, automatic revert after 3 failed boots | A checksum from the same origin proves nothing |
