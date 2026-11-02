#!/usr/bin/env bash
# CRAP score + (optionally) mutation testing.   Usage: scripts/quality.sh [crap|mutants [file...]]
# Needs: cargo-llvm-cov, cargo-mutants, rust-code-analysis-cli, python3.
set -euo pipefail
cd "$(dirname "$0")/.."
mode="${1:-crap}"; shift || true
case "$mode" in
  crap)
    mkdir -p target/rca
    cargo llvm-cov --workspace --exclude rustle-ui --lcov --output-path target/cov.lcov >/dev/null
    rm -rf target/rca/*
    rust-code-analysis-cli -m -p crates/rustle-server/src -p crates/rustle-types/src -O json -o target/rca --pr
    python3 scripts/crap.py target/cov.lcov target/rca --max="${CRAP_MAX:-30}" --top="${CRAP_TOP:-25}"
    ;;
  mutants)
    files=()
    for f in "$@"; do files+=(-f "$f"); done
    [ ${#files[@]} -eq 0 ] && files=(-f crates/rustle-server/src/cron.rs -f crates/rustle-server/src/imageref.rs -f crates/rustle-server/src/metrics.rs)
    cargo mutants -p rustle-server "${files[@]}" -j "${JOBS:-2}" --timeout-multiplier 3 --no-shuffle -o target/mutants
    ;;
  *) echo "usage: $0 [crap|mutants [files]]" >&2; exit 2;;
esac
