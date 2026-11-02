#!/usr/bin/env bash
# shellcheck disable=SC2015  # ok()/bad() never fail, so "A && ok || bad" is a safe if/else here
# End-to-end check of "Rustle shares an existing Caddy", with a REAL Caddy and a Caddyfile shaped like a
# site-ctl.sh setup ((common) snippet, `import sites.d/*.caddy`, admin API on a unix socket).
# Docker is faked (no daemon needed) and a local Python server stands in for the deployed container.
#
#   CADDY_BIN=caddy RUSTLE_BIN=target/release/rustle ./scripts/e2e-shared-caddy.sh
#
# Build Rustle first:  cargo build --release -p rustle-server --features dev-fakes
set -euo pipefail

CADDY="${CADDY_BIN:-caddy}"
BIN="${RUSTLE_BIN:-target/release/rustle}"
D="$(mktemp -d)"
HTTPS=18443
UP=39010
RP=39003
PASS=0; FAIL=0
ok()  { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1"; }
cleanup() { set +e; kill "${RUSTLE_PID:-}" "${CADDY_PID:-}" "${UP_PID:-}" 2>/dev/null; wait 2>/dev/null; rm -rf "$D"; }
trap cleanup EXIT

page() { curl -ksS --max-time 5 --resolve "$1:$HTTPS:127.0.0.1" "https://$1:$HTTPS${2:-/}" 2>/dev/null || true; }
admin() { curl -sS --max-time 5 --unix-socket "$D/admin.sock" "$@"; }
want() { # description host expected-substring
  if page "$2" | grep -q "$3"; then ok "$1"; else bad "$1 (got: $(page "$2" | head -c 60))"; fi
}
wait_for() { # seconds description command...
  local n="$1" d="$2"; shift 2
  for _ in $(seq 1 "$n"); do "$@" >/dev/null 2>&1 && { ok "$d"; return 0; }; sleep 1; done
  bad "$d"
}

mkdir -p "$D/sites.d" "$D/state" "$D/new"
cat > "$D/Caddyfile" <<CF
{
	admin unix/$D/admin.sock|0666
	http_port 18080
	https_port $HTTPS
	local_certs
}

(common) {
	encode zstd gzip
	header {
		Strict-Transport-Security "max-age=31536000"
		X-Content-Type-Options nosniff
		Referrer-Policy strict-origin-when-cross-origin
		-Server
	}
}

import $D/sites.d/*.caddy
CF
cat > "$D/sites.d/old.caddy" <<'CF'
old.localhost {
	import common
	respond "OLD SITE"
}
keep.localhost {
	import common
	respond "KEEP SITE"
}
CF
{ echo "NEW SITE"; head -c 3000 /dev/zero | tr '\0' 'x'; } > "$D/new/index.html"

echo "== start Caddy (real) and a stand-in app"
"$CADDY" run --config "$D/Caddyfile" --adapter caddyfile >"$D/caddy.log" 2>&1 &
CADDY_PID=$!
python3 -m http.server "$UP" --bind 127.0.0.1 --directory "$D/new" >/dev/null 2>&1 &
UP_PID=$!
wait_for 20 "Caddy admin API answers on the unix socket" admin http://127.0.0.1/config/
wait_for 20 "the existing site is served" bash -c "curl -ksS --resolve old.localhost:$HTTPS:127.0.0.1 https://old.localhost:$HTTPS/ | grep -q 'OLD SITE'"

echo "== start Rustle in shared mode (Docker faked, Caddy real)"
export RUSTLE_STATE_DIR="$D/state" RUSTLE_MASTER_KEY_FILE="$D/state/master.key" RUSTLE_DB="$D/state/rustle.db"
export RUSTLE_BIND="127.0.0.1:$RP" RUSTLE_SOCKET=none RUSTLE_FAKE_BACKENDS=docker RUSTLE_FAKE_CONTAINER_IP=127.0.0.1
export RUSTLE_GATEWAY=external CADDY_ADMIN_URL="unix://$D/admin.sock" RUSTLE_ADMIN_UPSTREAM="127.0.0.1:$RP"
export RUSTLE_ADMIN_DOMAIN=admin.localhost
INIT="$("$BIN" init)"
PASSWORD="$(printf '%s' "$INIT" | sed -n 's/.*password: //p')"
"$BIN" serve >"$D/rustle.log" 2>&1 &
RUSTLE_PID=$!
wait_for 20 "Rustle is up" curl -fsS "http://127.0.0.1:$RP/healthz"

api() { curl -sS --max-time 10 -X "$1" "http://127.0.0.1:$RP$2" -H 'content-type: application/json' -H "cookie: $COOKIE" -H "x-csrf-token: $CSRF" ${3:+-d "$3"}; }
LOGIN="$(curl -sS -i -X POST "http://127.0.0.1:$RP/api/v1/auth/login" -H 'content-type: application/json' -d "{\"username\":\"admin\",\"password\":\"$PASSWORD\"}")"
COOKIE="$(printf '%s' "$LOGIN" | tr -d '\r' | sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\);.*/\1/p')"
CSRF="$(printf '%s' "$LOGIN" | tail -1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["csrf"])')"
[ -n "$COOKIE" ] && ok "admin login" || bad "admin login"

echo "== the dashboard is exposed through the shared Caddy once it has a domain"
wait_for 20 "https://admin.localhost serves the dashboard" bash -c "curl -ksS --resolve admin.localhost:$HTTPS:127.0.0.1 https://admin.localhost:$HTTPS/healthz | grep -q ok"
want "existing sites are untouched (old)" old.localhost "OLD SITE"
want "existing sites are untouched (keep)" keep.localhost "KEEP SITE"

echo "== create a site for a domain the old Caddyfile already serves"
SITE="$(api POST /api/v1/sites "{\"name\":\"blog\",\"domain\":\"old.localhost\",\"image_repo\":\"ghcr.io/acme/blog\",\"ram_limit_mb\":128,\"container_port\":$UP,\"skip_dns_check\":true,\"override_budget\":true}")"
SITE_ID="$(printf '%s' "$SITE" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
sleep 1
want "creating the site does not take over the old site's traffic" old.localhost "OLD SITE"
TOKEN="$(api POST "/api/v1/sites/$SITE_ID/tokens" '{}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')"

echo "== first deploy flips traffic"
curl -sS -o /dev/null -X POST "http://127.0.0.1:$RP/api/v1/webhooks/deploy" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"image":"ghcr.io/acme/blog:1"}'
wait_for 30 "the domain now answers from the deployed container" bash -c "curl -ksS --resolve old.localhost:$HTTPS:127.0.0.1 https://old.localhost:$HTTPS/ | grep -q 'NEW SITE'"
want "another existing site is still served by the Caddyfile" keep.localhost "KEEP SITE"
HDRS="$(curl -ksSI --max-time 5 -H 'Accept-Encoding: gzip' --resolve "old.localhost:$HTTPS:127.0.0.1" "https://old.localhost:$HTTPS/" | tr -d '\r')"
printf '%s' "$HDRS" | grep -qi '^strict-transport-security: max-age=31536000' && ok "HSTS header like (common)" || bad "HSTS header missing"
printf '%s' "$HDRS" | grep -qi '^x-content-type-options: nosniff' && ok "nosniff like (common)" || bad "nosniff missing"
printf '%s' "$HDRS" | grep -qi '^content-encoding:' && ok "responses are compressed" || bad "no compression"
printf '%s' "$HDRS" | grep -qi '^server:' && bad "Server header leaked" || ok "no Server header"

echo "== the owner reloads the Caddyfile (what site-ctl.sh does)"
reload() { admin -X POST http://127.0.0.1/load -H 'Content-Type: text/caddyfile' --data-binary @"$D/Caddyfile"; }
reload >/dev/null
want "a reload from the Caddyfile drops Rustle's route (expected)" old.localhost "OLD SITE"
kill -HUP "$RUSTLE_PID"
wait_for 10 "SIGHUP puts the route back immediately" bash -c "curl -ksS --resolve old.localhost:$HTTPS:127.0.0.1 https://old.localhost:$HTTPS/ | grep -q 'NEW SITE'"
want "the dashboard route is back too" admin.localhost "<html\|<!doctype\|Rustle"
reload >/dev/null
wait_for 25 "without SIGHUP the poll restores routes within 15 s" bash -c "curl -ksS --resolve old.localhost:$HTTPS:127.0.0.1 https://old.localhost:$HTTPS/ | grep -q 'NEW SITE'"

echo "== rollback: delete the site in Rustle"
api DELETE "/api/v1/sites/$SITE_ID" >/dev/null
sleep 1
want "the old site serves again" old.localhost "OLD SITE"
want "keep still untouched" keep.localhost "KEEP SITE"

echo "== Rustle never replaced the Caddy configuration"
admin http://127.0.0.1/config/apps/http/servers | grep -q '"old.localhost"' && ok "the original routes are still in the running config" || bad "original routes lost"
grep -qi 'degraded' "$D/rustle.log" && { bad "Rustle reported a degraded gateway"; tail -5 "$D/rustle.log"; } || ok "no gateway errors in Rustle's log"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
