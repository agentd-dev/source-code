# agentd 2.0 — test strategy

Companion to `01-durable-agent-plan.md` §6 (P0 deliverable). Every phase ends
green under this strategy; CI's feature matrix and the 3-dep moat check stay.

## 1. Layers

| Layer | What | Where | Runs |
|---|---|---|---|
| Unit | pure logic: schema/typing, templates/CEL, store adapters against the in-process `memory` store and mock MCP/HTTP servers, envelope round-trips, scheduler on a mock executor, registry precedence, budget windows on a fake clock, compaction, plan, skills catalogue | `crates/agentd/src/**` `#[cfg(test)]` | every `cargo test` |
| Component | the v2 loop with the built-in mock LLM (`--internal-mock-llm` scripts) and mock MCP (`--internal-mock-mcp-http`, extended with the store profile + knowledge/search/skills profiles) — no network | `crates/agentd/src/runtime/**` tests + `crates/agentd-cli/tests/*_e2e.rs` | every `cargo test` |
| Chaos | SIGKILL at named kill points (between `put(running)` and `put(done)`, mid-batch, mid-turn, while suspended, while waiting for budget) then restart against the same store — assert replay with the same idempotency key, no duplicate effect beyond at-least-once, no lost transition | `crates/agentd-cli/tests/durability_e2e.rs` | every `cargo test` (fast: mock store) |
| Conformance v2 | black-box families: `durability`, `store` (mcp/http adapters), `a2a-conversation` (auth matrix, commands, NL turn, tasks across restart, streaming, human gate), `tools` (overrides/disabled), `workflow` (the five 1.x shapes as dialect-3, batching, signals), `skills` | `crates/agentd-conformance` | `cargo test -p agentd-conformance` + runner |
| Soak / perf | many concurrent runs, large `foreach` over an artifact-backed array, long conversations with compaction, budget windows | `bench/` + a `--features internal-mocks` soak binary target | manual / nightly |

## 2. Kill points

The runtime exposes `AGENTD_TEST_KILL_AT=<point>` (debug/internal-mocks only)
that aborts the process (`SIGKILL` self) at: `inbox.after_put`, `step.running`,
`step.before_done`, `batch.k`, `turn.mid_tool`, `wait.armed`, `budget.waiting`,
`context.before_put`. Chaos tests start the daemon, wait for the kill, restart
with the same store, and assert the resumed outcome.

## 3. Mock servers (all dependency-free, in-tree)

- **mock LLM** scripts: `final`, `read`, `schedule`, `subscribe`, `hang`, `slow`,
  `json`, `gate`, `a2a-delegate`, `spawn-churn` (existing) + v2: `preflight`
  (structured verdict), `plan` (creates/advances a plan), `tools-roundtrip`
  (calls internal tools), `compact` (long turn), `budget` (reports large usage).
- **mock MCP HTTP** profiles: reactive resource (existing), checkpointer
  `state.*` (existing; + `list`/`delete`, conflict injection, latency, failure
  rate), `work.*` (conformance), + v2: `knowledge.*`, `search.*`, skills as
  prompts and as `skill://` resources (legacy + modern dialect), a
  `memory`-override server, a `sandbox.execute` server for `code.run`.
- **mock HTTP store** for the `http` adapter (409 conflicts, 404, 5xx).
- **A2A test client** (raw JSON-RPC over HTTP, existing helpers) with mTLS /
  bearer principals.

## 4. Invariants asserted everywhere

- Exit-code table (RFC 0011) for job-shaped runs; clean drain = 0.
- No secret in any log/telemetry/store envelope (grep-based negative tests, as
  the conformance `secret-not-in-telemetry` check does today).
- Every principal action audited; deny-by-default authorization.
- Store `seq` monotonic per key; `Conflict` fatal; entity-first write order.
- Idempotency key present on every effect (`_meta` / `Idempotency-Key`).

## 5. Gates per phase

fmt · clippy `-D warnings` (all-features + default) · `cargo test --workspace
--all-features` · feature-matrix rows (CI) · dep-count = 3 · conformance ·
docs-drift check (P7).
