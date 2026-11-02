# Quality gates: CRAP score and mutation testing

Line coverage says code *ran*; these two say whether the tests would *notice* a defect.

```bash
scripts/quality.sh crap                # coverage x complexity, per function
scripts/quality.sh mutants [files...]  # cargo-mutants (default: cron, imageref, metrics)
```

Tools: `cargo install cargo-llvm-cov cargo-mutants rust-code-analysis-cli --locked`.
CI: `.github/workflows/quality.yml` (weekly + manual, advisory, uploads reports; not a PR gate
because mutation runs take 15-30 min per ~200 mutants on 4 cores).

## CRAP score

`CRAP = CC^2 * (1 - coverage)^3 + CC` per function (`scripts/crap.py`). CC = cyclomatic complexity
(rust-code-analysis), coverage = line coverage of the function body (llvm-cov). `mod tests` is
excluded. Conventional ceiling: **30**. A function can only get under it by being simpler or
better tested; 100 % coverage still leaves CRAP = CC, so CC > 30 is "split it up" regardless.

Baseline (first run, 473 functions, 22 above 30):

| Group | Functions | Why | Plan |
|---|---|---|---|
| Process wiring, 0 % | `server::run`, `main::main`, `config::from_env`, `main::init_logging` | Entry points; exercised by the headless-browser e2e and `scripts/e2e-*.sh`, which do not feed llvm-cov | Leave; covered end-to-end. Split `from_env` into parse helpers if it grows |
| Real Docker transport, 0 % | `docker::logs_follow/list_containers/wait/list_images/logs`, `caddy::set_admin` | Only reachable with a real Docker/Caddy | Covered by `scripts/e2e-real.sh` / `e2e-shared-caddy.sh` (CI jobs `real-docker`, `shared-caddy`) |
| High CC, well covered | `run_deploy` (48), `import_site_ctl` (55), `init` (44), `imageref::parse` (30), `cron::field` (26) | Long validation / state machines | Candidates for splitting into steps; coverage is already 82-100 % so CRAP ~= CC |
| Low coverage + moderate CC | `run_system` (10, 33 %), `run_http_job`, `put_settings`, `download`, `send_stream` | Error branches untested | Add failure-path tests |

`sites::site_action` (CC 20, 0 % in the lcov run) now has `site_actions_start_stop_restart`.

## Mutation testing

`cargo mutants` rewrites operators/returns one at a time and re-runs the unit tests; a surviving
mutant is a behaviour no test pins down.

First scoped run (`cron.rs`, `imageref.rs`, `metrics.rs`; 224 mutants): 174 caught, 45 missed,
2 timeouts (infinite loops, counted as caught), 3 unviable, i.e. **78.7 %**. Survivors were
length/boundary checks in `imageref`, CPU/disk/meminfo arithmetic in `metrics`. Tests added:
`imageref::{component_rules_exact_boundaries, tag_rules_exact_boundaries, parse_boundaries,
digest_and_untagged_name_edges}`, `metrics::{cpu_and_rss_exact_values,
disk_and_system_stats_exact_values}` (disk math split into pure `disk_gb`).

After the added tests the survivors were re-run (`--iterate`): **47 of 48 caught; the only survivor is the
equivalent mutant below**, so the three files are at 100 % of killable mutants.

Equivalent mutants (cannot be killed, behaviour identical) are accepted, e.g.
`imageref.rs: i > last_slash` -> `>=` (a ':' and a '/' can never share an index).

Not yet mutation-tested (next, in order of risk): `crypto`, `auth`, `deploy`, `caddy` route
building, `store`. Run one at a time: `scripts/quality.sh mutants crates/rustle-server/src/crypto.rs`.
