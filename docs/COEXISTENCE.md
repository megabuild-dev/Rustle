# Installing Rustle on a server that already runs sites and Caddy

Typical case: a VPS with two Next.js sites (built by GitHub Actions) behind a Caddy installed with `apt`. Nothing
is moved at install time. Rustle shares your Caddy, and you migrate the sites one at a time whenever you like.

## 1. Before you install

```bash
curl -s http://127.0.0.1:2019/config/ | head -c 300     # Caddy's admin API must answer (it does by default)
systemctl is-active caddy; docker ps --format '{{.Names}} {{.Ports}}' | grep -i caddy
```
If the admin API does not answer, your Caddyfile probably has `admin off`, or Caddy runs in a container that does not publish
`127.0.0.1:2019` (publish it, and set `admin 0.0.0.0:2019` in the container's global options).

## 2. Install in shared mode

```bash
sudo ./install/install.sh --use-existing-caddy --domain rustle.example.com --email you@example.com
```
Without `--use-existing-caddy` the installer notices Caddy and stops before changing anything.

What it does and does not do:

| | |
|---|---|
| Your Caddy and Caddyfile | not modified, not restarted, not reloaded |
| Your two sites | keep running exactly as before |
| Docker | if it already runs containers, `daemon.json` is left alone and Docker is **not** restarted |
| New things | the `rustle` service, a socket-proxy container, the `rustle-net` and `rustle-docker` networks, swap if there is none |
| The dashboard | exposed through your Caddy only once it has a domain (point its A record at the server, then set it in Settings or pass `--domain`). Until then use `ssh -L 3000:127.0.0.1:3000 you@server` |

Rustle adds routes to Caddy through its admin API with its own ids (`site-<name>`, `admin`). It never loads a whole configuration
and never touches routes it did not create.

## 3. Move a site into Rustle (zero downtime)

For each Next.js site:

1. In Rustle: **Add site** with the site's real domain and its GHCR image repository (`ghcr.io/you/app`). Creating the site does
   **not** touch traffic: the old site keeps answering.
2. Open the site's CI/CD tab. Replace your current deploy workflow with the generated one, add the two secrets
   (`RUSTLE_URL`, `RUSTLE_TOKEN`), and use the generated Dockerfile if you do not have one (needs `output: "standalone"`).
3. Push. GitHub builds the image, Rustle pulls it, starts the container, waits until it is healthy and **then** adds the route in
   front of the old one. Traffic flips in one step.
4. When happy, remove the old site block from the Caddyfile and stop the old process (pm2/systemd/etc.). Then run
   `sudo caddy reload --config /etc/caddy/Caddyfile && sudo systemctl restart rustle`. The restart re-adds Rustle's routes
   immediately (otherwise Rustle does it within a minute).
5. Roll back at any time: delete the site in Rustle and the old block in the Caddyfile (if you kept it) serves again.

Memory: Rustle measures the memory already in use when it is installed (including your existing sites) and subtracts it from
the budget for new sites. After you stop the old processes, raise the budget in Settings > RAM budget.

## 4. Things to know

* **Caddy reloads drop Rustle's routes.** `caddy reload` rebuilds the configuration from the Caddyfile, which does not contain
  Rustle's routes. They come back within 60 seconds on their own, or at once with `systemctl restart rustle`. Same after an
  `apt upgrade` that restarts Caddy.
* **Containers are reached by IP.** A Caddy on the host cannot resolve Docker names, so Rustle gives it each container's
  address and re-checks every minute (Docker may hand out different addresses after a reboot). For a short moment after a reboot
  a stale address could point at another container; keep this in mind for sensitive sites.
* **Several `:443` servers.** If your Caddy has more than one server on :443, set `RUSTLE_CADDY_SERVER=<name>` in
  `/etc/rustle/rustle.env` (the name is in `curl -s localhost:2019/config/apps/http/servers`).
* **Nginx, Apache, Traefik** are not supported as the shared proxy; use `--own-caddy` after freeing ports 80/443.
* Unverified on a real server so far: see items 27-31 of [VERIFY_ON_VPS.md](VERIFY_ON_VPS.md).
