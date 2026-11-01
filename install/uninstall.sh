#!/usr/bin/env bash
# Remove Rustle. Site containers are removed too; named volumes and backups are kept unless --purge.
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
PURGE=0; [ "${1:-}" = "--purge" ] && PURGE=1
systemctl disable --now rustle 2>/dev/null || true
for c in $(docker ps -aq --filter label=rustle.managed=1 2>/dev/null); do docker rm -f "$c" >/dev/null; done
docker rm -f rustle-caddy rustle-docker-proxy >/dev/null 2>&1 || true
docker network rm rustle-net >/dev/null 2>&1 || true
rm -f /etc/systemd/system/rustle.service /etc/tmpfiles.d/rustle.conf /etc/sysctl.d/99-rustle.conf
rm -rf /usr/lib/rustle
systemctl daemon-reload
if [ "$PURGE" = 1 ]; then
  rm -rf /var/lib/rustle /etc/rustle /run/rustle
  docker volume rm rustle_caddy_data rustle_caddy_config >/dev/null 2>&1 || true
  userdel rustle 2>/dev/null || true
  echo "purged (including the master key: backups can no longer be decrypted)"
else
  echo "removed services; kept /var/lib/rustle, /etc/rustle (master key) and Caddy volumes. Use --purge to delete them."
fi
