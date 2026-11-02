#!/usr/bin/env python3
"""CRAP score per function: CC^2 * (1 - cov)^3 + CC   (Savoia & Evans).

CC  = cyclomatic complexity (rust-code-analysis-cli), cov = line coverage of the function
      body (cargo llvm-cov lcov). Test code (everything from `mod tests`) is excluded.

  cargo llvm-cov --workspace --exclude rustle-ui --lcov --output-path target/cov.lcov
  rust-code-analysis-cli -m -p crates/rustle-server/src -p crates/rustle-types/src -O json -o target/rca --pr
  python3 scripts/crap.py target/cov.lcov target/rca [--max 30] [--top 25]

Exit 1 if any function exceeds --max (default 30, the conventional threshold).
"""
import glob, json, os, re, sys

args = [a for a in sys.argv[1:] if not a.startswith("--")]
opt = {a.split("=")[0]: a.split("=")[1] for a in sys.argv[1:] if a.startswith("--") and "=" in a}
lcov, rca = args[0], args[1]
MAX = float(opt.get("--max", 30))
TOP = int(opt.get("--top", 25))
root = os.getcwd()

cov = {}  # file -> {line: hits}
cur = None
for line in open(lcov):
    line = line.strip()
    if line.startswith("SF:"):
        cur = os.path.relpath(line[3:], root)
        cov[cur] = {}
    elif line.startswith("DA:") and cur:
        n, h = line[3:].split(",")[:2]
        cov[cur][int(n)] = int(h)

rows = []
for f in glob.glob(os.path.join(rca, "**/*.json"), recursive=True):
    d = json.load(open(f))
    path = os.path.relpath(d["name"], root) if os.path.isabs(d["name"]) else d["name"]
    lines = cov.get(path)
    if lines is None:
        continue
    try:
        src = open(path).read().splitlines()
    except OSError:
        continue
    tests_at = next((i + 1 for i, l in enumerate(src) if re.match(r"\s*mod tests\b", l)), 10**9)

    def walk(n):
        if n["kind"] == "function" and n["start_line"] < tests_at and n["name"] != "<anonymous>":
            exe = [h for ln, h in lines.items() if n["start_line"] <= ln <= n["end_line"]]
            if exe:
                c = sum(1 for h in exe if h > 0) / len(exe)
                cc = n["metrics"]["cyclomatic"]["sum"]
                rows.append((cc * cc * (1 - c) ** 3 + cc, cc, c, path, n["name"], n["start_line"]))
            return  # nested fns/closures are folded into the parent
        for c in n["spaces"]:
            walk(c)

    walk(d)

rows.sort(reverse=True)
print(f"{'CRAP':>7} {'CC':>4} {'cov':>5}  function")
for crap, cc, c, path, name, ln in rows[:TOP]:
    print(f"{crap:7.1f} {cc:4.0f} {c*100:4.0f}%  {path}:{ln} {name}")
bad = [r for r in rows if r[0] > MAX]
print(f"\n{len(rows)} functions, {len(bad)} above CRAP {MAX:g}; mean {sum(r[0] for r in rows)/max(len(rows),1):.2f}")
sys.exit(1 if bad else 0)
