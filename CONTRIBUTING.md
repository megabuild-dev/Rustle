# Contributing to Rustle

Thanks for helping. Rustle has one unusual rule that shapes most decisions: **memory is the product**. The admin process
must stay under 8 MB idle (hard cap 48 MB) on a 1 vCPU / 1 GB VPS. A feature that costs memory needs a very good reason.

## Ground rules

* Be kind. See the [Code of Conduct](CODE_OF_CONDUCT.md).
* Security problems go through [SECURITY.md](SECURITY.md), never a public issue.
* By contributing you agree that your work is released under the [MIT licence](LICENSE).
* Open an issue before a large change so we can agree on the approach.

## Setting up

You need Rust 1.80+ (stable), and for the dashboard `trunk` and the `wasm32-unknown-unknown` target.

```bash
git clone https://github.com/megalabs-dev/rustle && cd rustle
cargo test -p rustle-types -p rustle-server        # about 5 s with dependencies optimized
scripts/build-ui.sh                                # builds the dashboard into ui-dist/
cargo run -p rustle-server --features dev-fakes    # in-memory Docker/Caddy/GitHub, see docs/DEV.md
```

`docs/DEV.md` explains the fake backends, configuration variables and the CLI.

## Before you open a pull request

CI runs all of these; run them locally first:

```bash
cargo fmt --all -- --check
cargo clippy -p rustle-types -p rustle-server --all-targets -- -D warnings
cargo test -p rustle-types -p rustle-server
scripts/quality.sh crap          # every function must have CRAP score < 30
```

* **Tests are required** for behaviour changes. Prefer the fakes in `src/testutil.rs` and `MockHttp` over real Docker.
  Test boundaries exactly (limit and limit + 1), not just the happy path.
* **Mutation testing** (`scripts/quality.sh mutants path/to/file.rs`) is how we check that tests notice defects. A new
  surviving mutant in code you touched is a review comment. See [docs/QUALITY.md](docs/QUALITY.md).
* **Memory:** if you touch the server's hot path or add a dependency, run
  `cargo build --release -p rustle-server --features dev-fakes && python3 scripts/rss_smoke.py`
  and mention the numbers. The gate fails above 8 MB steady state.
* **Dependencies:** adding one needs a reason in the PR. It must pass `cargo deny check` (licences, advisories).
* Keep changes small and focused; one logical change per commit with a message that says *why*.

## What is welcome

* Fixes and tests, documentation, and verification reports from real servers (see `docs/VERIFY_ON_VPS.md`: this is the
  most valuable contribution right now).
* Items from the backlog in [docs/STATUS.md](docs/STATUS.md).
* Not welcome: features that need a database server, a language runtime in the admin process, or run privileged
  containers by default.

## Reporting bugs

Use the bug template. Include the Rustle version (`rustle version`), Ubuntu version, whether Caddy is Rustle's own or
shared, and the relevant lines of `journalctl -u rustle`. Remove secrets first.
