#!/usr/bin/env bash
# Rustle installer: idempotent, safe to re-run. Ubuntu 22.04/24.04, x86_64 or aarch64, run as root.
#
#   sudo ./install.sh [--domain admin.example.com] [--email you@example.com] [--slim] [--dry-run]
#
# Environment overrides:
#   RUSTLE_VERSION       release tag to install (default: latest)
#   RUSTLE_REPO          GitHub repo hosting releases (default: megalabs-dev/rustle)
#   RUSTLE_BINARY        path to a local rustle binary (skips the download; for testing)
#   RUSTLE_ADMIN_DOMAIN  dashboard domain (same as --domain)   RUSTLE_ACME_EMAIL  (same as --email)
#   RUSTLE_ACK_KEY=1     confirm non-interactively that you will save the printed master key
#   PROXY_IMAGE / CADDY_IMAGE   container images for the socket proxy and Caddy
#   SWAP_MB              swap size to create when the host has none (default 1536)
#   RUSTLE_INSTALL_DRY_RUN=1    print every step, change nothing
set -euo pipefail

REPO="${RUSTLE_REPO:-megalabs-dev/rustle}"
VERSION="${RUSTLE_VERSION:-latest}"
PROXY_IMAGE="${PROXY_IMAGE:-tecnativa/docker-socket-proxy:0.3.0}"
CADDY_IMAGE="${CADDY_IMAGE:-caddy:2-alpine}"
SWAP_MB="${SWAP_MB:-1536}"
DOMAIN="${RUSTLE_ADMIN_DOMAIN:-}"
EMAIL="${RUSTLE_ACME_EMAIL:-}"
DRY="${RUSTLE_INSTALL_DRY_RUN:-0}"
SLIM=0
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN="${2:?--domain needs a value}"; shift 2 ;;
    --email) EMAIL="${2:?--email needs a value}"; shift 2 ;;
    --slim) SLIM=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

log() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
run() { if [ "$DRY" = 1 ]; then printf '+ %s\n' "$*"; else "$@"; fi; }
# write_file <path> <mode>  (content on stdin)
write_file() {
  if [ "$DRY" = 1 ]; then printf '+ write %s (mode %s)\n' "$1" "$2"; cat >/dev/null; return 0; fi
  install -d -m 0755 "$(dirname "$1")"
  cat >"$1.tmp"
  chmod "$2" "$1.tmp"
  mv -f "$1.tmp" "$1"
}
# Fetch a file shipped next to this script (when run from a checkout) or from the release.
asset() {
  if [ -f "$SRC_DIR/../deploy/$1" ]; then cat "$SRC_DIR/../deploy/$1"; else curl -fsSL "https://raw.githubusercontent.com/$REPO/main/deploy/$1"; fi
}

[ "$DRY" = 1 ] || [ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

# ---- 0. platform ------------------------------------------------------------------------------
log "Checking the platform"
if [ -r /etc/os-release ]; then
  # Read in a subshell: os-release defines VERSION, which would clobber ours.
  # shellcheck disable=SC1091
  os_id="$(. /etc/os-release; echo "${ID:-}:${VERSION_ID:-}")"
  case "$os_id" in
    ubuntu:22.04|ubuntu:24.04) ;;
    *) echo "warning: tested on Ubuntu 22.04/24.04 only (found $os_id)" ;;
  esac
fi
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported CPU architecture $(uname -m)" ;;
esac
if [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null || true)" != "cgroup2fs" ]; then
  msg="cgroup v2 is required (Ubuntu 22.04+ default). Add systemd.unified_cgroup_hierarchy=1 to the kernel command line and reboot."
  if [ "$DRY" = 1 ]; then echo "warning: $msg"; else die "$msg"; fi
fi

# ---- 1. swap ----------------------------------------------------------------------------------
log "Swap"
if [ "$(swapon --show --noheadings 2>/dev/null | wc -l)" -gt 0 ]; then
  echo "swap already present; leaving it alone"
else
  run fallocate -l "${SWAP_MB}M" /swapfile || run dd if=/dev/zero of=/swapfile bs=1M count="$SWAP_MB"
  run chmod 600 /swapfile
  run mkswap /swapfile
  run swapon /swapfile
  grep -q '^/swapfile ' /etc/fstab 2>/dev/null || { [ "$DRY" = 1 ] && echo "+ append /swapfile to /etc/fstab" || echo '/swapfile none swap sw 0 0' >> /etc/fstab; }
fi
write_file /etc/sysctl.d/99-rustle.conf 0644 <<'CONF'
# Prefer real RAM; the swapfile is a safety net, not extra capacity.
vm.swappiness=10
CONF
run sysctl -q -p /etc/sysctl.d/99-rustle.conf

# ---- 2. docker --------------------------------------------------------------------------------
log "Docker"
if ! command -v docker >/dev/null 2>&1; then
  run apt-get update -qq
  run env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io curl ca-certificates
fi
if [ -f /etc/docker/daemon.json ] && [ "$DRY" != 1 ]; then
  if ! cmp -s <(asset daemon.json) /etc/docker/daemon.json; then
    echo "warning: /etc/docker/daemon.json already exists and differs; not touching it."
    echo "         Rustle recommends: native.cgroupdriver=systemd, userland-proxy=false, live-restore=true, log limits."
  fi
else
  asset daemon.json | write_file /etc/docker/daemon.json 0644
  run systemctl restart docker
fi
run systemctl enable --now docker

# Keep the journal small: logs are a RAM/disk cost on a 1 GB box.
write_file /etc/systemd/journald.conf.d/rustle.conf 0644 <<'CONF'
[Journal]
SystemMaxUse=100M
RuntimeMaxUse=32M
CONF
run systemctl restart systemd-journald

# ---- 3. user, directories ---------------------------------------------------------------------
log "Service user and directories"
id rustle >/dev/null 2>&1 || run useradd --system --home-dir /var/lib/rustle --shell /usr/sbin/nologin rustle
run install -d -o rustle -g rustle -m 0750 /var/lib/rustle /var/lib/rustle/bin /etc/rustle
run install -d -o root -g root -m 0755 /usr/lib/rustle
asset tmpfiles-rustle.conf | write_file /etc/tmpfiles.d/rustle.conf 0644
run systemd-tmpfiles --create /etc/tmpfiles.d/rustle.conf

# ---- 4. binary --------------------------------------------------------------------------------
log "Rustle binary ($VERSION, $ARCH)"
BIN=/var/lib/rustle/bin/rustle
if [ -n "${RUSTLE_BINARY:-}" ]; then
  run install -o rustle -g rustle -m 0755 "$RUSTLE_BINARY" "$BIN"
elif [ -x "$BIN" ] && [ "$VERSION" = latest ] && [ -z "${FORCE_DOWNLOAD:-}" ]; then
  echo "binary already installed; use the dashboard (Settings > Updates) or RUSTLE_VERSION=vX.Y.Z to change it"
else
  if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"; else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi
  if [ "$DRY" = 1 ]; then
    echo "+ download $BASE/rustle-$ARCH-linux and verify against SHA256SUMS"
  else
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    curl -fsSL -o "$tmp/rustle" "$BASE/rustle-$ARCH-linux"
    curl -fsSL -o "$tmp/SHA256SUMS" "$BASE/SHA256SUMS"
    want="$(awk -v n="rustle-$ARCH-linux" '$2==n {print $1}' "$tmp/SHA256SUMS")"
    [ -n "$want" ] || die "no checksum for rustle-$ARCH-linux in SHA256SUMS"
    echo "$want  $tmp/rustle" | sha256sum -c - >/dev/null || die "checksum mismatch; refusing to install"
    install -o rustle -g rustle -m 0755 "$tmp/rustle" "$BIN"
  fi
fi

# ---- 5. initial state: master key, admin account, baseline RAM ---------------------------------
log "Initializing Rustle"
PUBLIC_IP="${RUSTLE_PUBLIC_IP:-}"
if [ -z "$PUBLIC_IP" ] && [ "$DRY" != 1 ]; then
  PUBLIC_IP="$(curl -4 -fsS --max-time 5 https://api.ipify.org 2>/dev/null || true)"
  [ -n "$PUBLIC_IP" ] || PUBLIC_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
fi
export RUSTLE_PUBLIC_IP="$PUBLIC_IP" RUSTLE_ADMIN_DOMAIN="$DOMAIN" RUSTLE_ACME_EMAIL="$EMAIL"
export RUSTLE_STATE_DIR=/var/lib/rustle RUSTLE_MASTER_KEY_FILE=/etc/rustle/master.key
if [ "$DRY" = 1 ]; then
  echo "+ $BIN init   (prints the master key and the admin password once)"
else
  INIT_OUT="$("$BIN" init)"
  printf '%s\n' "$INIT_OUT"
  chown -R rustle:rustle /var/lib/rustle /etc/rustle
  chmod 600 /etc/rustle/master.key
  if printf '%s' "$INIT_OUT" | grep -q 'MASTER KEY'; then
    if [ "${RUSTLE_ACK_KEY:-0}" != 1 ]; then
      if [ -t 0 ]; then
        printf '\nWithout the master key your backups cannot be restored. Type "saved" once you have stored it: '
        read -r ans
        [ "$ans" = saved ] || die "aborted: save the master key first (re-run the installer; nothing else was started)"
      else
        die "non-interactive run: set RUSTLE_ACK_KEY=1 to confirm you saved the master key shown above"
      fi
    fi
  fi
fi

# ---- 6. network, socket proxy, Caddy ----------------------------------------------------------
log "Docker network, socket proxy and Caddy"
docker network inspect rustle-net >/dev/null 2>&1 || run docker network create --subnet 172.30.0.0/24 rustle-net
if [ "$DRY" = 1 ]; then
  echo "+ $BIN caddy-config > /etc/rustle/caddy.json"
else
  "$BIN" caddy-config > /etc/rustle/caddy.json.tmp && mv -f /etc/rustle/caddy.json.tmp /etc/rustle/caddy.json
  chown rustle:rustle /etc/rustle/caddy.json
fi

recreate() { # name image args...
  local name="$1"; shift
  run docker rm -f "$name" >/dev/null 2>&1 || true
  run docker run -d --name "$name" --restart unless-stopped "$@"
}
# The ONLY container that sees the Docker socket. Everything dangerous (exec, volumes, networks,
# system) is disabled; it listens on the loopback interface only.
recreate rustle-docker-proxy \
  -e CONTAINERS=1 -e IMAGES=1 -e NETWORKS=0 -e VOLUMES=0 -e EXEC=0 -e SYSTEM=0 -e EVENTS=0 -e POST=1 \
  -v /var/run/docker.sock:/var/run/docker.sock:ro -p 127.0.0.1:2375:2375 --memory 32m \
  "$PROXY_IMAGE"
# Caddy: public 80/443, Admin API on a unix socket in /run/rustle (never on rustle-net).
recreate rustle-caddy \
  --network rustle-net -p 80:80 -p 443:443 --memory 96m \
  -v /run/rustle:/run/rustle -v /etc/rustle/caddy.json:/etc/caddy/caddy.json:ro \
  -v rustle_caddy_data:/data -v rustle_caddy_config:/config \
  "$CADDY_IMAGE" caddy run --config /etc/caddy/caddy.json --resume

# ---- 7. systemd service -----------------------------------------------------------------------
log "systemd service"
asset prestart.sh | write_file /usr/lib/rustle/prestart.sh 0755
asset rustle.service | write_file /etc/systemd/system/rustle.service 0644
[ -f /etc/rustle/rustle.env ] || write_file /etc/rustle/rustle.env 0640 <<'CONF'
# Extra environment for Rustle (RUST_LOG=debug, RUSTLE_BIND=127.0.0.1:3000, ...). No secrets here.
CONF
[ "$DRY" = 1 ] || chown root:rustle /etc/rustle/rustle.env
run systemctl daemon-reload
run systemctl enable rustle
run systemctl restart rustle

# ---- 8. firewall (only if ufw is already active) ----------------------------------------------
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q 'Status: active'; then
  log "Firewall"
  run ufw allow 22/tcp
  run ufw allow 80/tcp
  run ufw allow 443/tcp
fi

# ---- 9. optional: slim down the OS -------------------------------------------------------------
if [ "$SLIM" = 1 ]; then
  log "Slimming the OS (--slim)"
  run systemctl disable --now snapd.service snapd.socket snapd.seeded.service 2>/dev/null || true
  run env DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq snapd || true
  run systemctl disable --now multipathd.service ModemManager.service 2>/dev/null || true
fi

# ---- 10. done ---------------------------------------------------------------------------------
log "Waiting for the dashboard"
if [ "$DRY" != 1 ]; then
  for _ in $(seq 1 40); do curl -fsS http://127.0.0.1:3000/healthz >/dev/null 2>&1 && break; sleep 1; done
  curl -fsS http://127.0.0.1:3000/healthz >/dev/null 2>&1 || { journalctl -u rustle -n 30 --no-pager; die "Rustle did not come up"; }
fi
if [ -n "$DOMAIN" ]; then URL="https://$DOMAIN"; else URL="https://${PUBLIC_IP:-<server-ip>}"; fi
cat <<MSG

Rustle is running.

  Dashboard: $URL
  $( [ -z "$DOMAIN" ] && echo "(self-signed certificate until you set a domain: Settings > Dashboard address)" )
  Open ports 80 and 443 in your cloud firewall (Lightsail: Networking > IPv4 Firewall).
  SSH-tunnel fallback if Caddy is unreachable:  ssh -L 3000:127.0.0.1:3000 you@server   then http://localhost:3000

MSG
