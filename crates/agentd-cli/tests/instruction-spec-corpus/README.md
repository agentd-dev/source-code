# agentd instruction-document probes

Black-box probes of agentd's instruction loader. Each case is a bare
instruction document (`core/NNN-*.instruction.md`) plus its expected
observable outcome (`core/NNN-*.expected.json`): whether it validates, which
error substrings appear, and which workflows and MCP servers register.
`instruction_spec_corpus.rs` writes each document into a config as
`agent.instruction`, runs the real `agentd` binary on it (`--validate-config`,
then `--capabilities`), and compares. They are agentd's own tests of what the
binary does with a document, not the specification's conformance corpus —
that lives with the crate, in `crates/instruction/tests/conformance/`, and is
run by `crates/instruction/tests/corpus.rs`.

## Where the cases came from

- `001`–`018` were copied from the specification repository's `core/`
  directory before upstream deleted it at `21fca67` (the corpus and its runner
  were removed there: they belong with an implementation, not with the text).
  agentd owns them now. The `*.expected.yaml` beside them are the upstream
  originals the JSON was derived from; the runner reads only the JSON.
- `019`–`022` are agentd-authored probes of the §4 forms against agentd's
  real config: a leaf-form `!mcp` (a document with no `:::` line at all, which
  the loader must still recognize), a table set, a section-form `!workflow`,
  and the section boundary (a YAML section ends at its code fence, so a leaf
  that follows is top-level, not swallowed). Their workflow bodies use
  agentd's own step shape.

Every fixture is written against version 1 of the format. Each declares the
`grants:` it needs (default none), so the trust ladder's fail-closed guarantee
(`018`) is actually exercised. `001` keeps the `when=` spelling of a skill's
trigger; `013` refuses an `!mcp` with no name ("requires name").

A valid document this build refuses only because it needs a cargo feature
compiled out of it (a `cron:` schedule without `cron`) is skipped, not failed;
the rows that build the feature run it in full, and a run that skips half the
cases fails.

## A probe result carries its binary version

The runner prints the agentd version it drove, and refuses to report
per-fixture results for a binary that does not implement directive extraction
(it is named and the run stops). A green here and a red elsewhere were once
both true and neither named its binary.

Authoring note: fixtures contain `:::!` fences, and `!` triggers history
expansion in interactive bash/zsh — a fixture authored through a
double-quoted shell string will corrupt. Author fixtures as files, via quoted
heredocs (`<<'EOF'`), or in single quotes.
