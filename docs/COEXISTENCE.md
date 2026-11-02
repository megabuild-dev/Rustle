# Installing Rustle on a server that already runs sites and Caddy

Rustle can share the Caddy that is already there. Nothing of yours is moved or restarted at install time, and you migrate sites one
at a time whenever you like. Two layouts are supported:

| Your Caddy | Rustle reaches your containers by | What you change in your stack |
|---|---|---|
| **A container** (Docker Compose, `caddy:2-alpine`) | container name, over the shared `rustle-net` network | four small edits, below |
| **A host service** (`apt install caddy`) | container IP | nothing (its admin API on `127.0.0.1:2019` is on by default) |

The installer detects which one you have: `docker ps --filter publish=443` for a container, `systemctl is-active caddy` for a service.

## A. Caddy as a container (your `~/apps` layout: Compose + `site-ctl.sh`)

Your stack, as the files show: one Compose project (`apps`) built from `compose.head.yml` + `sites.d/*.yml` + `compose.tail.yml`;
one `caddy:2-alpine` container publishing 80/443; the Caddyfile imports `caddy.d/*.caddy` (mounted at `/etc/caddy/sites.d`);
sites are reached by service name on the Compose network; `site-ctl.sh` validates and reloads Caddy.

### 1. Install Rustle (this changes none of your files)

```bash
sudo ./install/install.sh --use-existing-caddy --domain rustle.example.com --email you@example.com
```
It finds your Caddy container (the one publishing 443), leaves your swap, `ufw` and Docker alone (it does not restart Docker while it runs
containers), creates the `rustle-net` and `rustle-docker` networks, starts the socket proxy and connects your Caddy container to `rustle-net`.
Rustle then reports "gateway unreachable" until step 2 is done; it keeps retrying by itself.

### 2. Four small edits in `~/apps` (then `./site-ctl.sh regen && docker compose up -d caddy`)

Do this in a quiet moment: recreating the Caddy container interrupts every site for a second or two.

**`Caddyfile`**: a global options block, which must be the first thing in the file:
```caddyfile
{
	admin unix//run/rustle/caddy-admin.sock|0666
}

(common) {
	...
```
This moves the admin API from `localhost:2019` (inside the container) to a unix socket that only the `rustle` user and root can reach:
the other containers cannot touch it.

**`compose.head.yml`**: two additions to the `caddy` service:
```yaml
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - ./caddy.d:/etc/caddy/sites.d:ro
      - /run/rustle:/run/rustle            # new
      - caddy_data:/data
      - caddy_config:/config
    networks: [default, rustle-net]        # new
```

**`compose.tail.yml`**: a `networks:` block, placed **before** `volumes:` (`site-ctl.sh` appends the per-site volume lines right after
the tail, so `volumes:` has to stay last):
```yaml
networks:
  rustle-net:
    external: true

volumes:
  caddy_data:   # chứa chứng chỉ SSL, KHÔNG xoá
  caddy_config:
```

**`site-ctl.sh`**: in `reload_caddy()`, keep the failure status of the reload (callers such as `cmd_live` roll back on it) and then tell Rustle to
re-add its routes immediately:
```bash
reload_caddy() {
  docker compose up -d caddy >/dev/null
  local out
  if ! out=$(docker compose exec -T caddy caddy validate --config /etc/caddy/Caddyfile 2>&1); then
    echo "$out" >&2
    return 1
  fi
  docker compose exec -T caddy caddy reload --config /etc/caddy/Caddyfile || return 1
  sudo -n systemctl kill -s HUP rustle >/dev/null 2>&1 || true     # new: Rustle re-syncs its routes now
}
```
(`sudo -n` works for the default `ubuntu` user on cloud images; without it Rustle restores its routes on its own within 15 seconds.)
Apply the same four edits to the templates in your repo (`server/Caddyfile`, `compose.head.yml`, `compose.tail.yml`, `site-ctl.sh`) so
new instances get them; `setup.sh` never overwrites an existing `Caddyfile` or compose file.

Check it worked:
```bash
curl --unix-socket /run/rustle/caddy-admin.sock http://127.0.0.1/config/ | head -c 200
```

### 3. Bring your sites over

```bash
rustle import-site-ctl ~/apps --dry-run     # shows what would happen
rustle import-site-ctl ~/apps               # creates the sites in Rustle
```
For every site in `sites.d/*.meta` it takes the domain, the image repository (`ghcr.io/...`, tag dropped), the RAM (rounded up to a multiple of
32: 220 becomes 224), every variable of `env/<name>.env` (encrypted), and every line of `cron.d/<name>.cron` (as an HTTP cron job: Rustle calls
the path with `Authorization: Bearer $CRON_SECRET`, exactly as your cron does). It **does not touch traffic**: your sites keep answering
until each one's first deploy through Rustle. Re-running it is harmless.

It warns about what Rustle cannot do yet: the `www` redirect (keep it in your Caddyfile), the persistent `.next/cache` volume (`--cache`),
and health checks on a custom path (Rustle checks `/`). It skips sites whose image is not on `ghcr.io`.

Then, per site (start with the smallest, the static one, 64 MB):

1. In the dashboard open the site, create a **deploy token**, and in the CI/CD tab copy the generated workflow into the repository
   (secrets `RUSTLE_URL` and `RUSTLE_TOKEN`). The static-export `Dockerfile` of your repository (`Dockerfile.static.tmpl`: Caddy serving `out/`
   on port 3000) works unchanged; a Node site keeps its own Dockerfile.
2. Push. GitHub builds the image; Rustle pulls it, starts it next to your running container, waits until it is healthy and only then adds its route
   in front of your old one. Traffic flips in one step.
3. When satisfied: `./site-ctl.sh remove <name>` (this reloads Caddy, which triggers the resync above). Your old definition is gone, so
   from then on Rustle is the only source of that site. Rolling back means redeploying an older version in Rustle.

Memory on a 1 GB machine: during the flip both copies run, so migrate one site at a time. Rustle measured the memory in use at install time (your
sites included) and subtracts it from its budget; raise the budget in Settings after you retire the old containers.

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
* Routes Rustle adds carry the same response headers and compression as a typical `(common)` snippet (HSTS, `nosniff`, referrer policy, no `Server` header, zstd/gzip).
* Not yet verified against a real Caddyfile-built Caddy: see items 27-40 of [VERIFY_ON_VPS.md](VERIFY_ON_VPS.md).
