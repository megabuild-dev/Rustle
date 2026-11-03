## What and why

<!-- One paragraph: the change and the reason. Link the issue. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` pass
- [ ] Tests cover the change, including boundaries (limit and limit + 1)
- [ ] `scripts/quality.sh crap` still passes (every function < 30)
- [ ] Memory: no new idle cost, or numbers from `scripts/rss_smoke.py` included
- [ ] New dependency? Justified, and `cargo deny check` passes
- [ ] Docs and `CHANGELOG.md` updated if behaviour changed
- [ ] No secrets, tokens or personal data in the diff
