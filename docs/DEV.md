# Development

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked && cargo install wasm-bindgen-cli --version "$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/[^0-9.]/,"",$3); print $3; exit}' Cargo.lock)" --locked
pip install brotli            # or install the brotli CLI

cargo test -p rustle-types -p rustle-server      # 140+ tests, no Docker needed (fakes + mock HTTP servers)
cargo clippy -p rustle-types -p rustle-server --all-targets -- -D warnings
cargo clippy -p rustle-ui --target wasm32-unknown-unknown -- -D warnings
./scripts/build-ui.sh                            # builds the WebAssembly dashboard into ui-dist/ (embedded at compile time)
```

## Run it locally without Docker (fake backends)
```bash
cargo build --release -p rustle-server --features dev-fakes
python3 scripts/rss_smoke.py                     # RSS gate
node scripts/ui_e2e.js /tmp/shots                # headless Chromium walk-through (npm i playwright)
# or by hand:
export RUSTLE_STATE_DIR=$PWD/.dev RUSTLE_MASTER_KEY_FILE=$PWD/.dev/master.key RUSTLE_SOCKET=none RUSTLE_BIND=127.0.0.1:3000 RUSTLE_FAKE_BACKENDS=1
./target/release/rustle init && ./target/release/rustle     # then open http://127.0.0.1:3000
```

## Run it against real Docker
Use the same topology as production, never a mounted `docker.sock`:
`docker compose -f deploy/docker-compose.dev.yml up -d`, then `scripts/e2e-real.sh` shows the exact environment to export.

## Configuration (environment)
| Variable | Default | Meaning |
|---|---|---|
| `RUSTLE_BIND` | `127.0.0.1:3000` | loopback TCP listener (`none` disables) |
| `RUSTLE_SOCKET` | `/run/rustle/rustle.sock` | unix socket served to Caddy (`none` disables) |
| `RUSTLE_STATE_DIR` | `/var/lib/rustle` | database, binary directory |
| `RUSTLE_DB` | `<state>/rustle.db` | SQLite file |
| `RUSTLE_MASTER_KEY_FILE` | `/etc/rustle/master.key` | master key (hex), mode 0600 |
| `DOCKER_HOST` | `tcp://172.31.0.2:2375` | the socket proxy (also accepts `unix:///path`) |
| `CADDY_ADMIN_URL` | `unix:///run/rustle/caddy-admin.sock` | Caddy Admin API |
| `RUSTLE_ADMIN_UPSTREAM` | `unix//run/rustle/rustle.sock` | how Caddy dials Rustle (Caddy syntax) |
| `RUSTLE_GATEWAY` | `managed` | `external` shares an existing Caddy (admin API default `http://127.0.0.1:2019`, dashboard upstream `127.0.0.1:3000`) |
| `RUSTLE_CADDY_SERVER` | autodetect | Caddy HTTP server name to add routes to (external mode) |
| `RUSTLE_NETWORK` | `rustle-net` | Docker network created by the installer |
| `RUSTLE_PUBLIC_IP` / `RUSTLE_ADMIN_DOMAIN` / `RUSTLE_ACME_EMAIL` | none | read by `rustle init` |
| `RUSTLE_CGROUP_ROOT`, `RUSTLE_PROC_ROOT` | `/sys/fs/cgroup`, `/proc` | overridable for tests |
| `RUSTLE_GITHUB_API`, `RUSTLE_RELEASE_REPO` | `https://api.github.com`, `megalabs-dev/rustle` | backups and updates |
| `RUST_LOG` | `info` | `error`, `warn`, `info`, `debug`, `trace` |

## CLI
`rustle` / `rustle serve`, `rustle init [--reset-password]`, `rustle restore <file> [--force]`, `rustle key export`,
`rustle caddy-config`, `rustle version`.

## Layout
`crates/rustle-types` shared DTOs · `crates/rustle-server` binary + library (see `docs/IMPLEMENTATION_PLAN.md` for the module map) ·
`crates/rustle-ui` Leptos WebAssembly dashboard · `install/` installer · `deploy/` unit, daemon.json, compose · `scripts/` build and test helpers.
