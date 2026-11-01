#!/usr/bin/env python3
"""RSS smoke gate: run a dev-fakes build, exercise it, and fail if its resident memory is too high.

usage: RUSTLE_BIN=target/release/rustle python3 scripts/rss_smoke.py
Build the binary with: cargo build --release -p rustle-server --features dev-fakes
"""
import json, os, re, subprocess, sys, tempfile, time, urllib.request, urllib.error, http.cookiejar

BIN = os.environ.get("RUSTLE_BIN", "target/release/rustle")
MAX_KB = int(os.environ.get("RSS_MAX_KB", "8192"))          # steady state after load (the product target)
PEAK_KB = int(os.environ.get("RSS_PEAK_KB", "24576"))       # transient peak (Argon2 working set); hard cap is 48 MB
PORT = os.environ.get("RUSTLE_SMOKE_PORT", "39001")
BASE = f"http://127.0.0.1:{PORT}"


def rss(pid):
    s = open(f"/proc/{pid}/status").read()
    g = lambda k: int(re.search(rf"^{k}:\s+(\d+) kB", s, re.M).group(1))
    return g("VmRSS"), g("VmHWM")


def main():
    d = tempfile.mkdtemp(prefix="rustle-smoke-")
    env = dict(os.environ, RUSTLE_STATE_DIR=d, RUSTLE_MASTER_KEY_FILE=f"{d}/master.key", RUSTLE_DB=f"{d}/rustle.db",
               RUSTLE_BIND=f"127.0.0.1:{PORT}", RUSTLE_SOCKET="none", RUSTLE_FAKE_BACKENDS="1", MALLOC_ARENA_MAX="1")
    out = subprocess.run([BIN, "init"], env=env, capture_output=True, text=True, check=True).stdout
    pw = re.search(r"password: (\S+)", out).group(1)
    srv = subprocess.Popen([BIN], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen(BASE + "/healthz", timeout=1).read()
                break
            except Exception:
                time.sleep(0.1)
        else:
            sys.exit("server did not start: " + srv.stderr.read().decode())
        idle_rss, _ = rss(srv.pid)

        # The session cookie is `Secure`; urllib's cookie jar would refuse to send it over http, so carry it by hand.
        csrf = ""
        cookie = ""

        def call(method, path, body=None, extra=None):
            nonlocal cookie
            hdr = {"content-type": "application/json", "x-csrf-token": csrf, "cookie": cookie}
            hdr.update(extra or {})
            req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, method=method, headers=hdr)
            try:
                with urllib.request.urlopen(req, timeout=10) as r:
                    raw = r.read()
                    sc = r.headers.get("set-cookie")
                    if sc:
                        cookie = sc.split(";")[0]
                    return r.status, (json.loads(raw) if raw[:1] in (b"{", b"[") else raw)
            except urllib.error.HTTPError as e:
                return e.code, e.read()

        st, me = call("POST", "/api/v1/auth/login", {"username": "admin", "password": pw})
        assert st == 200, (st, me)
        csrf = me["csrf"]

        # a realistic dashboard session: 3 sites, deploys, env, tokens, many polls and asset loads
        tokens = {}
        for n in ("blog", "shop", "docs"):
            st, body = call("POST", "/api/v1/sites", {"name": n, "domain": f"{n}.example.com", "image_repo": f"ghcr.io/o/{n}", "ram_limit_mb": 128, "skip_dns_check": True, "override_budget": True})
            assert st == 201, (st, body)
            site = body
            st, t = call("POST", f"/api/v1/sites/{site['id']}/tokens", {})
            tokens[n] = t["token"]
            call("PUT", f"/api/v1/sites/{site['id']}/env", {"vars": [{"key": "A", "value": "b" * 200}]})
        for n, tok in tokens.items():
            st, _ = call("POST", "/api/v1/webhooks/deploy", {"image": f"ghcr.io/o/{n}:1"}, {"authorization": f"Bearer {tok}"})
            assert st == 202, st
        time.sleep(1.5)
        for i in range(400):
            call("GET", "/api/v1/system/stats")
            call("GET", "/api/v1/sites")
            if i % 40 == 0:
                for a in re.findall(r'(?:src|href)="(/[^"]+)"', urllib.request.urlopen(BASE + "/").read().decode()):
                    urllib.request.urlopen(BASE + a).read()
        # password hashing is the allocation spike we care about
        for _ in range(3):
            call("POST", "/api/v1/auth/login", {"username": "admin", "password": pw})
        time.sleep(0.5)
        cur, peak = rss(srv.pid)
        print(f"idle RSS  : {idle_rss/1024:.1f} MB")
        print(f"after load: {cur/1024:.1f} MB (peak {peak/1024:.1f} MB)")
        print(f"gates     : steady < {MAX_KB/1024:.1f} MB, peak < {PEAK_KB/1024:.1f} MB")
        if cur > MAX_KB:
            sys.exit(f"FAIL: steady RSS {cur} kB > {MAX_KB} kB")
        if peak > PEAK_KB:
            sys.exit(f"FAIL: peak RSS {peak} kB > {PEAK_KB} kB")
        print("OK")
    finally:
        srv.terminate()
        try:
            srv.wait(5)
        except Exception:
            srv.kill()


main()
