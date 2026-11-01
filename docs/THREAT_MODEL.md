# Threat model

Scope: a single-tenant VPS where the owner deploys their own code. Rustle is **not** a hosting platform for mutually
untrusted users.

## Assets
Docker control (root-equivalent on the host), the master key (decrypts env vars, registry tokens, backups), admin credentials,
deploy tokens, the GitHub backup token.

## Trust boundaries
```
Internet --443/80--> Caddy (container, rustle-net) --unix socket--> rustle-admin (host, user "rustle", systemd sandbox)
                                |                                        |--127.0.0.1:2375--> docker-socket-proxy --docker.sock--> dockerd
                                +--rustle-net--> tenant containers       +--unix socket--> Caddy Admin API
```
Only Caddy listens publicly. The Docker proxy is published on host loopback only. Caddy's Admin API and Rustle's own listener
are unix sockets inside `/run/rustle` (mode 0750, owner `rustle`), not on `rustle-net`, so tenant containers cannot reach
them. A loopback TCP listener (`127.0.0.1:3000`) exists for SSH-tunnel access.

## Controls
| Threat | Control |
|---|---|
| Container escape / host takeover through Rustle | No raw `docker.sock` in Rustle; proxy denies exec, volumes, networks, system. `docker::create_body` is the only constructor of container config: no privileged, no mounts, no host namespaces, `cap_drop=ALL`, `no-new-privileges`, `pids_limit`, hard RAM, no published ports. A test asserts the forbidden keys never appear |
| Compromised admin process | systemd sandbox, non-root user, not in the `docker` group, 48 MB / 30% CPU caps. **Residual risk:** the proxy filters by path and method, not request body, so an attacker who controls the process can still ask the proxy to create a privileged container. Treat the admin process as the crown jewel |
| Stolen deploy token | Per-site, hashed (SHA-256), revocable; can only deploy images from that site's registered `ghcr.io` repository; rate limited (30/min) |
| Admin password guessing | Argon2id, per-IP exponential backoff (no account lockout), a global gate so hashing cannot exhaust RAM |
| Session theft / CSRF | HttpOnly + Secure + SameSite=Strict cookie, 12 h expiry, CSRF header derived per session, password change revokes all sessions, strict CSP |
| Secrets at rest | AES-256-GCM with the master key; ciphertext bound to its row (AAD); the master key file is 0600 and never uploaded |
| Command injection | No shell, no `std::process::Command`; cron jobs are argv arrays run in a throw-away container; names, domains, env keys, image refs are validated by strict parsers |
| Malicious update | Ed25519 signature checked against a key compiled into the binary; updating is disabled without a key; previous binary kept and restored after 3 failed boots |
| Backup leak | Backups are encrypted before upload; the target repository must be private; the master key is not included |
| Resource exhaustion | Per-container limits, RAM budget, request body limit 64 KB, bounded channels, log rotation, weekly image prune |

## Known gaps
* Single admin, no audit of container-internal activity, no network policy between tenant containers on `rustle-net`.
* Images are trusted: a deployed image runs arbitrary code inside its limits.
* `compat_caps` re-adds six capabilities for images that need them.
