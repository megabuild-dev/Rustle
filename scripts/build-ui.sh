#!/usr/bin/env bash
# Build the WebAssembly dashboard and stage it in ui-dist/ (embedded into the server binary).
# Requires: rustup target wasm32-unknown-unknown, trunk (cargo install trunk --locked),
#           wasm-bindgen-cli matching Cargo.lock, optionally wasm-opt (binaryen) and brotli.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

(cd crates/rustle-ui && trunk build --release --dist "$OUT")

# Keep the committed placeholder out of the way, stage the real build.
find ui-dist -mindepth 1 ! -name '.gitkeep' -delete
cp "$OUT"/*.js "$OUT"/*.wasm "$OUT"/*.css "$OUT"/*.svg ui-dist/
# The page is served with a strict CSP (no inline scripts): move Trunk's inline bootstrap into boot.js.
python3 - "$OUT/index.html" ui-dist <<'PY'
import re, sys, pathlib
html = pathlib.Path(sys.argv[1]).read_text()
out = pathlib.Path(sys.argv[2])
m = re.search(r'<script type="module">(.*?)</script>', html, re.S)
if not m:
    sys.exit("trunk output has no inline bootstrap script; update scripts/build-ui.sh")
(out / "boot.js").write_text(m.group(1).strip() + "\n")
html = html.replace(m.group(0), '<script type="module" src="/boot.js"></script>')
(out / "index.html").write_text(html)
PY

if command -v wasm-opt >/dev/null 2>&1; then
  for f in ui-dist/*.wasm; do wasm-opt -Oz --enable-bulk-memory --enable-sign-ext --enable-nontrapping-float-to-int -o "$f.opt" "$f" && mv "$f.opt" "$f"; done
fi

compress() {
  if command -v brotli >/dev/null 2>&1; then
    brotli -f -q 11 -o "$1.br" "$1"
  else
    python3 - "$1" <<'PY'
import sys, brotli
p = sys.argv[1]
open(p + ".br", "wb").write(brotli.compress(open(p, "rb").read(), quality=11))
PY
  fi
}
for f in ui-dist/*.js ui-dist/*.wasm ui-dist/*.css ui-dist/index.html; do compress "$f"; done

echo "ui-dist:"; find ui-dist -maxdepth 1 -type f -printf '%s %f\n' 
total=$(cat ui-dist/*.br | wc -c)
echo "compressed total: $total bytes"
[ "$total" -lt 1048576 ] || { echo "ERROR: compressed UI exceeds the 1 MiB budget" >&2; exit 1; }
