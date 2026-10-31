# agentd instruction-document probes

Black-box probes of agentd's instruction loader. Each case is a bare
`instruction/1` document (`core/NNN-*.instruction.md`) plus its expected
observable outcome (`core/NNN-*.expected.json`): whether it validates, which
error substrings appear, and which workflows and MCP servers register.
`instruction_spec_corpus.rs` writes each document into a config as
`agent.instruction`, runs the real `agentd` binary on it (`--validate-config`,
then `--capabilities`), and compares. They are agentd's own tests of what the
binary does with a document as a host — which blocks become configuration,
under which grants — not the specification's conformance cases, which are the
instruction.md SDK's and run with it.

The cases are agentd-authored: the forms agentd maps (`:::config`, `:::mcp`
and `::mcp`, `:::workflow`, `:::skill`, `::person`, `::role`, `::agent`), the
reader's refusals agentd surfaces (`unknown-kind`, `unclosed-directive`,
`invalid-attributes`, `duplicate-id`, a workflow with no `dialect`), blocks
inside a layout block that map and blocks inside an example that do not, an
agent outside its grant that stays text, and a dialect agentd does not run.
Each declares the `grants:` it needs (default none).

A valid document this build refuses only because it needs a cargo feature
compiled out of it (a `cron:` schedule without `cron`) is skipped, not failed;
the rows that build the feature run it in full, and a run that skips half the
cases fails.

## A probe result carries its binary version

The runner prints the agentd version it drove, and refuses to report
per-fixture results for a binary that maps no blocks (it is named and the run
stops).
