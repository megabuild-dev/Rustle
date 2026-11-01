#!/bin/sh
# Auto-revert guard for self-updates. An update arms it by creating bin/update-pending; the server
# removes that marker (and resets the counter) after 30 healthy seconds. If the new binary is
# started 4 times without getting there, the previous binary (rustle.prev) is put back.
# Without the marker nothing is ever reverted: crashes, OOM kills or manual restarts of a long-running
# binary must not silently downgrade it.
set -eu
d=/var/lib/rustle/bin
c="$d/boot_attempts"
mkdir -p "$d"
if [ ! -f "$d/update-pending" ]; then
  echo 0 > "$c"
  exit 0
fi
n=$(cat "$c" 2>/dev/null || echo 0)
case "$n" in ''|*[!0-9]*) n=0 ;; esac
n=$((n + 1))
echo "$n" > "$c"
if [ "$n" -ge 4 ] && [ -f "$d/rustle.prev" ]; then
  cp -f "$d/rustle.prev" "$d/rustle.revert"
  chmod 0755 "$d/rustle.revert"
  mv -f "$d/rustle.revert" "$d/rustle"
  rm -f "$d/update-pending"
  echo 0 > "$c"
  logger -t rustle "the updated binary failed to stay up; reverted to the previous version" || true
fi
