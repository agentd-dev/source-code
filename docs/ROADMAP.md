# Roadmap

Direction, not promises. Items graduate from here into RFCs.

**The through-line.** A frozen, validated graph is the unit of
correctness — that is the moat. Autonomy is admitted on top of it as a
*tunable granted by evidence, not by default*. So the roadmap advances
two things in lockstep: the **bounds** (durability, composition,
observability, policy) and the **autonomy dial** (compile-your-own
workflows, agentic sub-loops, reliability-gated approval). Both stay
first-class; neither is allowed to erode the other.

## Now (v0.8 — the dynamic harness, RFC 0006)

- [x] Named intelligence backends: Anthropic / OpenAI / Gemini /
      openai-compatible (`intel-remote`), Unix socket, HTTP JSON-RPC.
- [x] `agent_loop` node — bounded agentic steps inside the graph.
- [x] Instruction mode — the agent compiles its own workflow from a
      natural-language instruction, capability-injected, grafted onto
      the base environment, approval-gated, with bounded re-planning.
- [x] Instruction files (`--instructions agent.toml`).
- [x] Token budgets + LLM usage metrics.
- [x] Conformance + benchmark suite (`crates/agentd-conformance`):
      scenario corpus, capability-matrix coverage, pass^k reliability
      scoring, fault-injection battery, injection-corpus security
      conformance, cost-per-success reporting, criterion benchmarks.
      See [docs/CONFORMANCE.md](CONFORMANCE.md).

## v0.9 — the autonomy dial and the bounds, both first-class

### Durable & composable execution (the bounds)

- [x] **Run records** — the engine captures a structured record of a
      run (per-node input/output, cost, timing, policy decisions,
      outcome, trace). `--record run.json`; `agentd inspect run.json`
      renders a human-readable timeline. The substrate a run inspector
      (CLI today, UI later) consumes.
- [x] **Durable execution + `pause_for_approval`** — the engine writes a
      checkpoint at a `pause_for_approval` node and stops with a `Paused`
      outcome (exit 7); `--resume RUN_ID` continues from the checkpoint
      with state restored. The line between automation and an agent that
      works alongside you. (Crash-recovery at any node boundary is the
      next increment.)
- [x] **Sub-workflow `call` node** — invoke another workflow as a sub-DAG
      under the parent's policy/budget envelope, depth-bounded. Compose
      the substrate; never invent an orchestrator-of-agents.

### The autonomy dial (granted by evidence)

- [x] **Capability-altitude plan review** — the approval gate renders a
      compiled plan as what it reads / writes / reaches / calls, plus the
      policy it runs under, not raw TOML. Approve at the altitude a human
      reasons about.
- [x] **Plan promotion** — `--promote PATH` writes an approved plan as a
      durable workflow with a provenance header. Instruction mode is the
      *design-time* fast path; the promoted, signed workflow is the
      *production* path. Dynamism that collapses to a bound.
- [x] **Reliability-gated autonomy** — a workflow earns the right to run
      unattended by clearing a `pass_rate` bar in the conformance suite
      (`min_pass_rate` per scenario + a `--min-pass-rate` deploy gate).
      Autonomy you earn, measured — only possible because the substrate
      is deterministic and the harness exists.

### Conformance as a product

- [x] **Cost forecasting** — project spend from cost-per-success ×
      trigger rate (`--forecast-runs-per-day` / `--price-per-mtok`).
- [x] **Drift detection** — compare a suite run against a saved baseline
      (`--save-baseline` / `--baseline`) and fail on a `pass_rate`
      regression (e.g. after a model update).

### Authoring

- [x] **TypeScript authoring SDK** (`sdk/typescript`) — a typed builder
      that emits workflow TOML, so app engineers author in their stack
      and inherit the runtime's guarantees. TOML stays the compile
      target; the package round-trips its output through a real `agentd
      --validate-only` in CI.

### Substrate depth (carried forward)

- [ ] Declared bounded cycles (`max_iterations` edges) —
      evaluator–optimizer patterns without an inner loop (RFC 0003 §5).
- [ ] Full JSON-Schema enforcement on `llm_infer` / loop outputs with
      schema-failure repair rounds.
- [ ] Parallel fan-out / fan-in (RFC 0001 §9.1).

## Later — scale-out (TODO: design RFCs required)

The single-process daemon stays the unit of correctness. Scale-out
composes daemons rather than complicating one:

- [ ] **Clustering**: N agentd processes behind shared triggers;
      leader-elected cron so schedules fire exactly once per fleet.
- [ ] **Work distribution**: a queue-backed trigger (NATS/SQS-class)
      so goals and workflow runs can be submitted to a pool;
      at-least-once delivery with idempotency keys from run ids.
- [ ] **Coordination layer**: shared run-state store (leases,
      progress, outcomes) enabling hand-off, retries across nodes,
      and fleet-wide budget accounting.
- [ ] **Fleet governance**: centrally-distributed signed policies and
      instruction files; per-tenant budget envelopes; audit shipping.
- [x] **Run inspector UI** (v1) — a browser surface over run records at
      `/inspect`: paste/upload a record, see the node timeline with
      per-node I/O, cost, and policy decisions, client-side. Replay,
      search, and run-diff are the next increments.
- [ ] Windows path-pattern canonicalisation for `[policy.fs]`
      (matcher is `/`-separated today; see maturity.md).
