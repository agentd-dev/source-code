# Changelog

All notable changes to `agentd` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

As of **1.0.0** the public surface — the workflow TOML schema, the CLI
flags, the outcome / run-record JSON, and the process exit codes — is
stable under semver. A breaking change to any of them requires a major
version bump.

## [Unreleased]

_Nothing yet. See [docs/ROADMAP.md](docs/ROADMAP.md) for what's next._

## [1.0.0] — 2026-06-11

The 1.0 cut freezes the substrate and the autonomy dial together. A
frozen, validated graph is the unit of correctness; autonomy is admitted
on top of it as a tunable granted by evidence, not by default. Everything
below is in the released binary, covered by unit tests and the
conformance suite, green across Linux / macOS / Windows.

### Added

- **Instruction mode** — the agent compiles its own workflow from a
  natural-language instruction, capability-injected and grafted onto a
  base environment, approval-gated, with bounded re-planning. Backed by a
  **capability catalogue** describing what the planner may use, and
  **instruction files** (`--instructions agent.toml`) with a standing
  task + explicit `auto_approve` opt-in.
- **Conformance & benchmark suite** (`crates/agentd-conformance`) — a
  scenario corpus with pass^k reliability scoring, capability-matrix
  coverage as goal tracking, a fault-injection battery with
  bounded-degradation assertions, security / prompt-injection
  conformance, cost-per-success reporting, and criterion benchmarks.
  See [docs/CONFORMANCE.md](docs/CONFORMANCE.md).
- **Reliability-gated autonomy** — a per-scenario `min_pass_rate` and a
  `--min-pass-rate` deploy gate, so a workflow earns the right to run
  unattended by clearing a measured bar.
- **Cost forecasting** (`--forecast-runs-per-day` / `--price-per-mtok`)
  and **drift detection** (`--save-baseline` / `--baseline`) — fail a
  suite run on a `pass_rate` regression after, e.g., a model update.
- **Run records** — `--record run.json` captures per-node input/output,
  cost, timing, policy decisions, outcome, and trace; `agentd inspect
  run.json` renders a human-readable timeline.
- **Durable execution + `pause_for_approval`** — the engine checkpoints
  at a `pause_for_approval` node and stops with a `Paused` outcome
  (exit 7); `--resume RUN_ID` continues with state restored.
- **Crash-recovery** — `--checkpoint-each-node` (with `--state-dir`)
  snapshots after every node; a crashed run is recoverable from its last
  completed node via `--resume` / `--resume-incomplete` /
  `--list-checkpoints`. At-least-once for the interrupted node.
- **Sub-workflow `call` node** — invoke another workflow as a sub-DAG
  under the parent's policy / budget envelope, depth-bounded.
- **`parallel` fan-out / fan-in node** — runs sub-workflows concurrently
  (scoped threads) and joins, flattening per-branch failure.
- **Declared bounded cycles** — `max_iterations` loop edges express
  evaluator–optimizer patterns without an unbounded inner loop.
- **Capability-altitude plan review** — the approval gate renders a
  compiled plan as what it reads / writes / reaches / calls, plus the
  policy it runs under, not raw TOML.
- **Plan promotion** — `--promote PATH` writes an approved plan as a
  self-contained, durable workflow with a provenance header (the base
  environment travels with it).
- **Full JSON-Schema enforcement on `llm_infer`** — with the `schema`
  feature, an `output_schema` naming a JSON Schema file is enforced
  against the model's output, with bounded `output_repairs` re-prompt
  rounds on a schema failure.
- **TypeScript authoring SDK** (`sdk/typescript`) — a typed builder that
  emits workflow TOML and round-trips through a real `agentd
  --validate-only` in CI.
- **Run inspector UI** — a client-side browser surface over run records
  at `/inspect`.

### Changed

- The public surface (TOML schema, CLI, outcome / record JSON, exit
  codes) is now declared **stable under semver**.
- **MSRV is now Rust 1.88** (was 1.85). The dependency graph
  (`tracing-appender` → `time 0.3.47`) requires it; edition 2024 already
  set a 1.85 floor. The release image pins `rust:1.88-bookworm`.
- Plan promotion now emits a **self-contained** workflow — the base
  environment (backends, policy, budgets, auth) is carried into the
  promoted file rather than referenced, so the production artifact runs
  standalone.

### Fixed

- Cross-platform test hygiene: the real-filesystem security test and the
  Unix-only shell tests are gated to Unix; `call`-node path tests are
  normalized for Windows; signal tests serialize on shared process-global
  flags.

## [0.8.0] and earlier — 2026-06-10

Foundational runtime, developed as an incremental simulated history and
released as tags `v0.1.0` … `v0.8.0`: the typed-DAG model and TOML
parser, the full validator (acyclicity, reachability, start-node shape,
edge / route / trigger integrity), node execution dispatch, control flow
(condition / switch / merge / fail / terminate), per-node retry and run
timeouts, process-wide resource budgets, the HTTP / cron / fs-watch /
manual triggers, fail-closed policy allowlists + optional Rego, the named
intelligence backends and the bounded `agent_loop` node, ed25519 workflow
signing, and the observability spine (spans, Prometheus `/metrics`,
`/healthz`, audit JSONL, OTLP, traceparent). See the git history and the
RFCs under [`rfcs/`](rfcs/) for the design record.

[Unreleased]: https://github.com/agentd-dev/source-code/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/agentd-dev/source-code/compare/v0.8.0...v1.0.0
