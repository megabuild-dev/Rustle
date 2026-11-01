#!/usr/bin/env bash
# shellcheck disable=SC2015  # ok()/bad() never fail, so "A && ok || bad" is a safe if/else here
# End-to-end scenario against the REAL stack: Docker, the socket proxy, Caddy and a release build.
# Verifies behaviour the unit tests can only simulate (resource limits, proxy ACLs, network isolation).
#
#   RUSTLE_BIN=target/release/rustle ./scripts/e2e-real.sh
#
# Needs: docker, curl, python3; cgroup v2; ports 80/443 and 127.0.0.1:3000 free, subnets 172.30.0.0/24 and 172.31.0.0/29 unused; network access to
# pull images. E2E_IMAGE must be a PUBLIC ghcr.io image that listens on E2E_PORT.
set -euo pipefail

BIN="${RUSTLE_BIN:?set RUSTLE_BIN to the rustle binary}"
E2E_IMAGE="${E2E_IMAGE:-ghcr.io/nginxinc/nginx-unprivileged}"
E2E_TAG="${E2E_TAG:-alpine}"
E2E_PORT="${E2E_PORT:-8080}"
D="$(mktemp -d)"
RUN="$D/run"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1"; }
check() { # description command...
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi
}
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$@" 2>/dev/null || echo 000; }

cleanup() {
  set +e
  [ -n "${RUSTLE_PID:-}" ] && kill "$RUSTLE_PID" 2>/dev/null
  docker ps -aq --filter label=rustle.managed=1 | xargs -r docker rm -f >/dev/null 2>&1
  docker rm -f rustle-caddy rustle-docker-proxy >/dev/null 2>&1
  docker network rm rustle-net rustle-docker >/dev/null 2>&1
  docker volume rm rustle_e2e_data rustle_e2e_config >/dev/null 2>&1
  rm -rf "$D"
}
trap cleanup EXIT

echo "== topology"
mkdir -p "$RUN" "$D/state"
chmod 750 "$RUN"
docker network create --subnet 172.30.0.0/24 rustle-net >/dev/null
docker network create --subnet 172.31.0.0/29 rustle-docker >/dev/null
docker run -d --name rustle-docker-proxy -e CONTAINERS=1 -e IMAGES=1 -e NETWORKS=0 -e VOLUMES=0 -e EXEC=0 -e SYSTEM=0 -e EVENTS=0 -e POST=1 \
  --network rustle-docker --ip 172.31.0.2 \
  -v /var/run/docker.sock:/var/run/docker.sock:ro "${PROXY_IMAGE:-tecnativa/docker-socket-proxy:0.3.0}" >/dev/null

export RUSTLE_STATE_DIR="$D/state" RUSTLE_MASTER_KEY_FILE="$D/state/master.key" RUSTLE_BIND=127.0.0.1:3000
export RUSTLE_SOCKET="$RUN/rustle.sock" RUSTLE_ADMIN_UPSTREAM="unix//run/rustle/rustle.sock"
export DOCKER_HOST=tcp://172.31.0.2:2375 CADDY_ADMIN_URL="unix://$RUN/caddy-admin.sock"
INIT="$("$BIN" init)"
PASSWORD="$(printf '%s' "$INIT" | sed -n 's/.*password: //p')"
"$BIN" caddy-config > "$D/caddy.json"
docker run -d --name rustle-caddy --network rustle-net -p 80:80 -p 443:443 -v "$RUN:/run/rustle" -v "$D/caddy.json:/etc/caddy/caddy.json:ro" \
  -v rustle_e2e_data:/data -v rustle_e2e_config:/config "${CADDY_IMAGE:-caddy:2-alpine}" caddy run --config /etc/caddy/caddy.json --resume >/dev/null
"$BIN" serve >"$D/rustle.log" 2>&1 &
RUSTLE_PID=$!
for _ in $(seq 1 60); do curl -fsS http://127.0.0.1:3000/healthz >/dev/null 2>&1 && break; sleep 1; done
check "dashboard answers on loopback" curl -fsS http://127.0.0.1:3000/healthz

api() { # method path [json]
  curl -sS --max-time 20 -X "$1" "http://127.0.0.1:3000$2" -H 'content-type: application/json' -H "cookie: $COOKIE" -H "x-csrf-token: $CSRF" ${3:+-d "$3"}
}
LOGIN="$(curl -sS -i -X POST http://127.0.0.1:3000/api/v1/auth/login -H 'content-type: application/json' -d "{\"username\":\"admin\",\"password\":\"$PASSWORD\"}")"
COOKIE="$(printf '%s' "$LOGIN" | tr -d '\r' | sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\);.*/\1/p')"
CSRF="$(printf '%s' "$LOGIN" | tail -1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["csrf"])')"
[ -n "$COOKIE" ] && ok "admin login" || bad "admin login"

echo "== create a site and deploy through the webhook"
SITE="$(api POST /api/v1/sites "{\"name\":\"e2e\",\"domain\":\"e2e.localhost\",\"image_repo\":\"$E2E_IMAGE\",\"ram_limit_mb\":128,\"container_port\":$E2E_PORT,\"skip_dns_check\":true,\"override_budget\":true}")"
SITE_ID="$(printf '%s' "$SITE" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
TOKEN="$(api POST "/api/v1/sites/$SITE_ID/tokens" '{}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')"
HOOK="$(curl -sS -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:3000/api/v1/webhooks/deploy -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "{\"image\":\"$E2E_IMAGE:$E2E_TAG\"}")"
[ "$HOOK" = 202 ] && ok "webhook accepted" || bad "webhook returned $HOOK"
STATUS=""
for _ in $(seq 1 90); do
  STATUS="$(api GET "/api/v1/sites/$SITE_ID" | python3 -c 'import sys,json; print(json.load(sys.stdin)["status"])')"
  [ "$STATUS" = running ] && break
  [ "$STATUS" = failed ] && break
  sleep 2
done
[ "$STATUS" = running ] && ok "site reached running" || { bad "site status is $STATUS"; api GET "/api/v1/sites/$SITE_ID/deployments" || true; tail -30 "$D/rustle.log"; }

echo "== container hardening (docker inspect)"
CID="$(docker ps -q --filter label=rustle.site="$SITE_ID" | head -1)"
INSPECT() { docker inspect -f "$1" "$CID"; }
[ "$(INSPECT '{{.HostConfig.Memory}}')" = "134217728" ] && ok "hard memory limit 128 MiB" || bad "memory limit"
[ "$(INSPECT '{{.HostConfig.MemorySwap}}')" = "134217728" ] && ok "no swap for the container" || bad "memory swap"
[ "$(INSPECT '{{.HostConfig.Privileged}}')" = "false" ] && ok "not privileged" || bad "privileged"
[ "$(INSPECT '{{.HostConfig.CapDrop}}')" = "[ALL]" ] && ok "all capabilities dropped" || bad "capabilities"
[ "$(INSPECT '{{.HostConfig.NetworkMode}}')" = "rustle-net" ] && ok "on rustle-net" || bad "network mode"
[ "$(INSPECT '{{len .HostConfig.PortBindings}}')" = "0" ] && ok "no published ports" || bad "published ports"
[ "$(INSPECT '{{.HostConfig.RestartPolicy.Name}}')" = "unless-stopped" ] && ok "restart policy" || bad "restart policy"
[ "$(INSPECT '{{len .HostConfig.Binds}}')" = "0" ] && ok "no bind mounts" || bad "bind mounts"

echo "== traffic through Caddy"
for _ in $(seq 1 30); do [ "$(code -k --resolve e2e.localhost:443:127.0.0.1 https://e2e.localhost/)" = 200 ] && break; sleep 2; done
[ "$(code -k --resolve e2e.localhost:443:127.0.0.1 https://e2e.localhost/)" = 200 ] && ok "https://e2e.localhost serves the app" || bad "app not reachable through Caddy"

echo "== socket proxy ACLs"
[ "$(code -X POST http://172.31.0.2:2375/containers/"$CID"/exec -H 'content-type: application/json' -d '{}')" = 403 ] && ok "exec is denied" || bad "exec NOT denied"
[ "$(code http://172.31.0.2:2375/volumes)" = 403 ] && ok "volumes are denied" || bad "volumes NOT denied"
[ "$(code http://172.31.0.2:2375/networks)" = 403 ] && ok "networks are denied" || bad "networks NOT denied"
[ "$(code http://172.31.0.2:2375/info)" = 403 ] && ok "system info is denied" || bad "system info NOT denied"
[ "$(code http://172.31.0.2:2375/containers/json)" = 200 ] && ok "container listing works" || bad "container listing"

echo "== isolation"
check "Caddy admin API is not on TCP" bash -c '! curl -sS --max-time 3 http://127.0.0.1:2019/config/'
check "a tenant cannot reach the Rustle API" bash -c "! docker run --rm --network rustle-net alpine wget -q -T 3 -O- http://172.30.0.1:3000/healthz"
check "a tenant cannot reach the Docker proxy (gateway)" bash -c "! docker run --rm --network rustle-net alpine wget -q -T 3 -O- http://172.30.0.1:2375/_ping"
check "a tenant cannot reach the Docker proxy (its own address)" bash -c "! docker run --rm --network rustle-net alpine wget -q -T 3 -O- http://172.31.0.2:2375/_ping"
check "the proxy publishes no port on the host" bash -c "[ -z \"\$(docker port rustle-docker-proxy)\" ]"
check "plain HTTP never serves the app (only a redirect)" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H 'Host: e2e.localhost' http://127.0.0.1/)\" != 200 ]"
check "a tenant cannot reach Caddy's admin port" bash -c "! docker run --rm --network rustle-net alpine wget -q -T 3 -O- http://rustle-caddy:2019/config/"
check "Rustle socket is not world accessible" bash -c "[ \"\$(stat -c %a '$RUN/rustle.sock')\" = 660 ]"

echo "== second deploy and rollback"
FIRST_DEP="$(api GET "/api/v1/sites/$SITE_ID/deployments" | python3 -c 'import sys,json; print(json.load(sys.stdin)[0]["id"])')"
curl -sS -o /dev/null -X POST http://127.0.0.1:3000/api/v1/webhooks/deploy -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "{\"image\":\"$E2E_IMAGE:$E2E_TAG\"}"
sleep 3
for _ in $(seq 1 60); do
  N="$(api GET "/api/v1/sites/$SITE_ID/deployments" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(sum(1 for x in d if x["status"]=="running"))')"
  [ "$N" = 1 ] && [ "$(api GET "/api/v1/sites/$SITE_ID" | python3 -c 'import sys,json; print(json.load(sys.stdin)["status"])')" = running ] && break
  sleep 2
done
[ "$(docker ps -q --filter label=rustle.site="$SITE_ID" | wc -l)" = 1 ] && ok "exactly one app container after redeploy" || bad "container count"
RB="$(api POST "/api/v1/sites/$SITE_ID/rollback" "{\"deployment_id\":\"$FIRST_DEP\"}" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("deployment_id",""))')"
[ -n "$RB" ] && ok "rollback accepted" || bad "rollback refused"

echo "== Rustle process"
RSS="$(awk '/VmRSS/ {print $2}' "/proc/$RUSTLE_PID/status")"
echo "  rustle RSS: ${RSS} kB"
[ "$RSS" -lt 16384 ] && ok "RSS under 16 MB with a real backend" || bad "RSS ${RSS} kB"

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
