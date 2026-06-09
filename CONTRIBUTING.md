# Contributing

## Ground rules

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, and `cargo test -p agentd --all-features` must pass.
  CI also builds each canonical feature combination on its own — if your
  change touches feature-gated code, run the combination you touched
  with `--no-default-features --features <set>` locally.
- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `chore:`,
  scoped like `feat(engine):`). One logical change per commit.
- New node kinds: model variant (+ `name()` / `is_side_effect()`),
  validator coverage if the kind cross-references anything, a handler
  with policy checks before any side effect, dry-run behaviour, unit
  tests, and a row in `docs/capabilities.md`.
- New config surface: TOML field + env var + CLI flag follow the
  existing precedence (CLI > env > workflow > default), documented in
  `docs/configuration.md` and the `--help` text in the same commit.

## Fixture tests

Drop `tests/fixtures/<name>/workflow.toml` + `fixture.toml` and the
suite auto-discovers it. Fixtures pin the expected outcome AND the
exact node path — use them for any engine-behaviour change.

## Design changes

Anything that alters the execution model, the policy posture, or the
trust boundary gets an RFC in `rfcs/` first. Small and sharp beats
comprehensive — RFC 0002 (signed workflows) is the calibre to aim for.

## Security

Suspected vulnerabilities: email andrii@tsok.org rather than opening a
public issue.
