#!/bin/sh
# Auto-revert guard for self-updates. The server resets the counter after 30 healthy seconds; if the
# counter reaches 4 without that, the previous binary (kept as rustle.prev) is put back.
set -eu
d=/var/lib/rustle/bin
c="$d/boot_attempts"
mkdir -p "$d"
n=$(cat "$c" 2>/dev/null || echo 0)
case "$n" in ''|*[!0-9]*) n=0 ;; esac
n=$((n + 1))
echo "$n" > "$c"
if [ "$n" -ge 4 ] && [ -f "$d/rustle.prev" ]; then
  cp -f "$d/rustle.prev" "$d/rustle.revert"
  chmod 0755 "$d/rustle.revert"
  mv -f "$d/rustle.revert" "$d/rustle"
  echo 0 > "$c"
  logger -t rustle "new binary failed to stay up; reverted to the previous version" || true
fi
