# Quality gates: CRAP score and mutation testing

Line coverage says code *ran*; these two say whether the tests would *notice* a defect.

```bash
scripts/quality.sh crap                 # coverage x complexity, per function (gate: every function < 30)
scripts/quality.sh mutants [files...]   # cargo-mutants (default: cron, imageref, metrics)
```

Tools: `cargo install cargo-llvm-cov cargo-mutants rust-code-analysis-cli --locked`.

| Where | What | Blocking |
|---|---|---|
| `.github/workflows/ci.yml` job `crap` | every function must have CRAP < 30 | yes |
| `.github/workflows/quality.yml` (weekly + manual) | CRAP report, then full-crate mutation run | no (reports uploaded) |

## CRAP score

`CRAP = CC^2 * (1 - coverage)^3 + CC` per function (`scripts/crap.py`). CC = cyclomatic complexity
(rust-code-analysis), coverage = line coverage of the function body (llvm-cov). `mod tests` is excluded.
The gate fails when any function reaches **30**.

Baseline before this work: 473 functions, 22 at or above 30 (entry points at 0 %, the real Docker
transport at 0 %, and four functions with CC 31-55).
Now: **542 functions, none at or above 30, mean 4.4, highest 26.5** (`update::apply`).

What it took:

* **Entry points made testable.** `main` is three lines; `commands::{parse, execute, entry}` hold the CLI,
  `server::{open_checked, run_until, listen_tcp, listen_unix, reconcile_loop, ...}` hold the process
  wiring, `Config::from_lookup` holds configuration parsing (`from_env` only supplies `std::env::var`).
* **Long functions split by stage:** `run_deploy` (CC 48 -> 19: pull/pin, env snapshot, spec, create+start,
  health, traffic switch, retire), `import_site_ctl` (55 -> 24: plan / persist / warnings / cron lines),
  `cli::init` (44 -> small helpers), `reconcile::sync` (31 -> desired upstream + push route),
  `imageref::parse` (30 -> digest / tag / host helpers), `httpc::send_stream` (TLS connect extracted).
* **Real transports tested against `MockHttp`:** Docker list/logs/follow/wait/pull/stop, `set_admin` in
  every Caddy mode, GitHub downloads with redirects, TLS connect failures.

## Mutation testing

`cargo mutants` rewrites operators and return values one at a time and re-runs the unit tests; a surviving
mutant is a behaviour no test pins down. Configuration: `.cargo/mutants.toml`.

Crate-wide result (about 3,000 mutants over every module of `rustle-server`; "unviable" = the mutation does
not compile, which is most of the handler-returning-`Json` replacements):

* every survivor was either fixed with a test (about 150 tests were added: boundaries of every limit, exact
  values for arithmetic, exact status/error mapping, side effects such as audit entries and slot release) or is
  listed below;
* the dependency profile now builds with `opt-level = 2` for third-party crates (argon2 dominated test time),
  which made the suite 5x faster and the mutation runs feasible (about 2 h for the whole crate).

### Accepted (documented) outcomes

| Mutant | Why it stays |
|---|---|
| `imageref.rs`: `i > last_slash` -> `>=` | equivalent: a ':' and a '/' can never share an index |
| non-terminating mutants (`cron::field` `step==0`, `docker::logs_follow` `i += 8+len` -> `*=`) | the mutated loop never ends; cargo-mutants reports a timeout, i.e. the suite cannot pass |
| `release_memory`, `tune_allocator`, `init_logging`, `serve`, `shutdown_signal`, `run` -> `Ok(())`, `dev_flags` wrapper | excluded in `.cargo/mutants.toml`: process-global side effects or a signal; covered by `scripts/e2e-*.sh`, `scripts/rss_smoke.py` and the systemd unit |

### Hardening found by the exercise

* `Deployer`: a panic inside the run loop left the site "busy" forever in debug/test builds; a `SlotGuard`
  now releases the slot (release builds abort on panic and recover through the boot reconcile).
* `jobs::tail` had an unbounded `while` that could spin on an out-of-range index; it is now a single search.
* `cli::parse_env_file` lost two dead special cases (blank/comment lines are skipped by the key check).
* `crypto`: the non-unix `write_secret_file` fallback was removed (Rustle is Linux-only).

### Running it yourself

```bash
scripts/quality.sh mutants crates/rustle-server/src/auth.rs                # one file, ~10 min
cargo mutants -p rustle-server --iterate -o target/mutants ...             # re-test only survivors
cargo mutants -p rustle-server --list | awk -F: '{print $1}' | sort | uniq -c   # sizes
```
