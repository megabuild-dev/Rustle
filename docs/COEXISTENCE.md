# Installing Rustle on a server that already runs sites and Caddy

Rustle can share the Caddy that is already there. Nothing of yours is moved or restarted at install time, and you migrate sites one
at a time whenever you like. Two layouts are supported:

| Your Caddy | Rustle reaches your containers by | What you change in your stack |
|---|---|---|
| **A container** (Docker Compose, `caddy:2-alpine`) | container name, over the shared `rustle-net` network | three small edits, below |
| **A host service** (`apt install caddy`) | container IP | nothing (its admin API on `127.0.0.1:2019` is on by default) |

The installer detects which one you have: `docker ps --filter publish=443` for a container, `systemctl is-active caddy` for a service.

## A. Caddy as a container (Compose + `site-ctl.sh`)

### 1. Install

```bash
sudo ./install/install.sh --use-existing-caddy --domain rustle.example.com --email you@example.com
```
Without `--use-existing-caddy` the installer notices your Caddy and stops before changing anything. It never installs Docker again,
never touches your swap or `ufw` rules, and does **not** restart Docker if it is already running containers (it only prints the
recommended `daemon.json`). It adds: the `rustle` service, a socket-proxy container, the `rustle-net` and `rustle-docker` networks,
and connects your Caddy container to `rustle-net` right away.

### 2. The three changes in your stack

Rustle shows "gateway unreachable" and keeps retrying until these are applied.

```yaml
# docker compose, caddy service
    volumes:
      - /run/rustle:/run/rustle          # Rustle's socket, and Caddy's admin socket for Rustle
    networks: [default, rustle-net]
# top level
networks:
  rustle-net:
    external: true
```
```caddyfile
# global options block of the generated Caddyfile (merge with the one you have)
{
    admin unix//run/rustle/caddy-admin.sock|0666
}
```
The admin API moves from `localhost:2019` (inside the container) to a unix socket that only the `rustle` user (and root) can reach:
containers on `rustle-net` cannot touch it. Then apply as usual (`docker compose up -d caddy`) and check:
```bash
curl --unix-socket /run/rustle/caddy-admin.sock http://127.0.0.1/config/ | head -c 200
```

### 3. Keep Rustle's routes across your reloads

`site-ctl.sh` runs `caddy reload`, which rebuilds the configuration from your Caddyfile and therefore drops the routes Rustle added
through the API. Rustle puts them back by itself within 15 seconds; to make it immediate add one line after the reload in `site-ctl.sh`:
```bash
docker compose exec -T caddy caddy reload --config /etc/caddy/Caddyfile
systemctl kill -s HUP rustle 2>/dev/null || true     # Rustle re-syncs its routes now
```
Only sites managed by Rustle are affected by that window; your own sites are served by the Caddyfile and never interrupted.

### 4. Move a site into Rustle (zero downtime)

For each site:

1. **Add site** in Rustle with the site's real domain and a GHCR image repository (`ghcr.io/you/app`). Creating the site does **not**
   touch traffic: your current site keeps answering.
2. Open the site's CI/CD tab. Replace your deploy workflow with the generated one, add the secrets `RUSTLE_URL` and `RUSTLE_TOKEN`.
   Dockerfile:
   * **Static export** (`output: "export"`): your `Dockerfile.static.tmpl` already works as is (Caddy serving `out/` on port 3000, which is
     Rustle's default port). The tab also offers an equivalent. Give the site **64 MB** of RAM.
   * **Next.js with a Node server** (`output: "standalone"`): use the other Dockerfile; 256 MB is a good start.
3. Push. GitHub builds and pushes the image; Rustle pulls it, starts it, waits until it is healthy and only then adds its route in
   front of your old one. Traffic flips in a single step.
4. When satisfied, remove the old site from `caddy.d` and its container, and reload as usual. (If you keep the old definition it
   simply takes over again if you delete the site in Rustle: that is your rollback.)

Memory: Rustle subtracts what is already in use at install time (your existing sites included) from the budget for new sites. After
you retire the old containers, raise the budget in Settings > RAM budget.

## B. Caddy as a host service (apt)

```bash
sudo ./install/install.sh --use-existing-caddy --domain rustle.example.com
```
Nothing to change in your stack. Rustle talks to `http://127.0.0.1:2019` (change with `--caddy-admin`) and gives Caddy each container's
IP. A `caddy reload` drops Rustle's routes just like above: they return within 15 seconds, or immediately after
`systemctl kill -s HUP rustle`. For a short moment after a reboot an old address could point at another container.

## Things to know

* Rustle never loads a whole configuration into your Caddy and never touches routes it did not create (all of its routes carry ids
  `site-<name>` and `admin`).
* The dashboard is exposed through your Caddy only once it has a domain (a catch-all route would swallow your other sites). Until
  then: `ssh -L 3000:127.0.0.1:3000 you@server` and open `http://localhost:3000`.
* Several `:443` servers in your Caddy: set `RUSTLE_CADDY_SERVER=<name>` in `/etc/rustle/rustle.env`
  (`curl --unix-socket ... http://127.0.0.1/config/apps/http/servers` lists the names).
* Nginx, Apache or Traefik on 80/443 are not supported as the shared proxy; use `--own-caddy` after freeing those ports.
* Not yet verified against a real Caddyfile-built Caddy: see items 27-35 of [VERIFY_ON_VPS.md](VERIFY_ON_VPS.md).
