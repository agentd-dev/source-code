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

- [ ] **Run records** — the engine captures a structured record of a
      run (per-node input/output, cost, timing, policy decisions,
      outcome, trace). `--record run.json`; `agentd inspect run.json`
      renders a human-readable timeline. The substrate a run inspector
      (CLI today, UI later) consumes.
- [ ] **Durable execution** — persist `ExecutionContext` at node
      boundaries; `--resume RUN_ID` continues a run from its last
      checkpoint. The difference between in-memory automation and a
      daemon that survives a restart.
- [ ] **`pause_for_approval` node** — checkpoint, emit a `Paused`
      outcome + a notification, and resume on a human's response. The
      line between automation and an agent that works alongside you.
- [ ] **Sub-workflow `call` node** — invoke another signed workflow as
      a sub-DAG under the parent's policy/budget envelope, bounded
      recursion depth. Compose the substrate; never invent an
      orchestrator-of-agents.

### The autonomy dial (granted by evidence)

- [ ] **Capability-altitude plan review** — render a compiled plan as
      "reads X, calls model Y, writes Z, stays within policy, new tools
      requested: none" for the approval step, not raw TOML. Approve at
      the altitude a human reasons about.
- [ ] **Plan promotion** — `--promote PATH` writes an approved compiled
      plan as a versioned, signed Mode-1 workflow. Instruction mode
      becomes the *design-time* fast path; the static signed artifact is
      the *production* path. Dynamism that collapses to a bound.
- [ ] **Reliability-gated autonomy** — auto-approve a workflow only when
      its conformance `pass^k` clears a configured threshold. Autonomy
      you earn, measured, per workflow — only possible because the
      substrate is deterministic and the harness exists.

### Conformance as a product

- [ ] **Cost forecasting** — project spend from cost-per-success ×
      trigger rate.
- [ ] **Drift detection** — compare a suite run against a saved baseline
      and flag `pass^k` / cost regressions (e.g. after a model update).

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
- [ ] **Run inspector UI** — a hosted/local web surface over run
      records: node timeline, model I/O, cost, policy decisions, replay,
      search, run diff. The control-plane half of the trust story.
- [ ] Windows path-pattern canonicalisation for `[policy.fs]`
      (matcher is `/`-separated today; see maturity.md).
