# Roadmap

Direction, not promises. Items graduate from here into RFCs.

**The through-line.** A frozen, validated graph is the unit of
correctness — that is the moat. Autonomy is admitted on top of it as a
*tunable granted by evidence, not by default*. So the roadmap advances
two things in lockstep: the **bounds** (durability, composition,
observability, policy) and the **autonomy dial** (compile-your-own
workflows, agentic sub-loops, reliability-gated approval). Both stay
first-class; neither is allowed to erode the other.

---

## Shipped — the road to v1.0

v1.0 (RFC 0006) freezes the substrate and the autonomy dial *together*.
Everything in this section ships in the `v1.0.0` binary, covered by unit
tests and the conformance suite. The public surface — TOML schema, CLI
flags, outcome / record JSON, exit codes — is now stable under semver;
breaking it requires a major bump.

### The dynamic harness (v0.8 foundation, RFC 0006)

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

### Durable & composable execution (the bounds)

- [x] **Run records** — the engine captures a structured record of a
      run (per-node input/output, cost, timing, policy decisions,
      outcome, trace). `--record run.json`; `agentd inspect run.json`
      renders a human-readable timeline. The substrate the run inspector
      (CLI + the `/inspect` UI) consumes.
- [x] **Durable execution + `pause_for_approval`** — the engine writes a
      checkpoint at a `pause_for_approval` node and stops with a `Paused`
      outcome (exit 7); `--resume RUN_ID` continues with state restored.
- [x] **Crash-recovery** — `--checkpoint-each-node` snapshots after every
      node; a crashed run is recoverable from its last completed node via
      `--resume` / `--resume-incomplete` / `--list-checkpoints`.
      At-least-once for the interrupted node.
- [x] **Sub-workflow `call` node** — invoke another workflow as a sub-DAG
      under the parent's policy/budget envelope, depth-bounded. Compose
      the substrate; never invent an orchestrator-of-agents.
- [x] **Declared bounded cycles** (`max_iterations` edges) —
      evaluator–optimizer patterns without an inner loop (RFC 0003 §5).
- [x] **Parallel fan-out / fan-in** — a `parallel` node runs
      sub-workflows concurrently (scoped threads) and joins (RFC 0003 §5).

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
- [x] **Full JSON-Schema enforcement** on `llm_infer` outputs — with the
      `schema` feature, an `output_schema` that names a JSON Schema file
      is enforced against the model's output, with bounded
      `output_repairs` re-prompt rounds on a schema failure (RFC 0003 §5).

### Conformance as a product

- [x] **Cost forecasting** — project spend from cost-per-success ×
      trigger rate (`--forecast-runs-per-day` / `--price-per-mtok`).
- [x] **Drift detection** — compare a suite run against a saved baseline
      (`--save-baseline` / `--baseline`) and fail on a `pass_rate`
      regression (e.g. after a model update).

### Authoring & inspection

- [x] **TypeScript authoring SDK** (`sdk/typescript`) — a typed builder
      that emits workflow TOML, so app engineers author in their stack
      and inherit the runtime's guarantees. TOML stays the compile
      target; the package round-trips its output through a real `agentd
      --validate-only` in CI.
- [x] **Run inspector UI** (v1) — a browser surface over run records at
      `/inspect`: paste/upload a record, see the node timeline with
      per-node I/O, cost, and policy decisions, client-side.

---

## After v1.0 — shipped (v1.1.0 → v1.3.0)

The business-automation use-case catalog (docs/use-cases/) was built
against the real runtime as a stress test, and its
[gap analysis](use-cases/GAP-ANALYSIS.md) drove three releases in its
own recommended order. Every buildable gap it surfaced is closed:

- [x] **`tools-http-tls`** — HTTPS for `http_request` (ureq + rustls,
      the intel-remote stack); redirects never followed so the policy
      decision stays exact. v1.1.0 (§1).
- [x] **`respond` node** — an http-triggered workflow shapes its own
      HTTP reply; TwiML / Slack shapes / challenge echoes answered
      natively. v1.2.0 (§2).
- [x] **`map` node + array-index context paths** — bounded fan-out over
      data: one sub-workflow per array element, mandatory `max_items`,
      `parallel`'s join contract. v1.2.0 (§3).
- [x] **Form-encoded / multipart webhook bodies** — Twilio /
      inbound-email callers need no JSON relay. v1.2.0 (§4).
- [x] **Per-route idempotency keys** — redeliveries replay the recorded
      response; exactly-once *effect* at the route boundary
      (single-node). v1.2.0 (§5).
- [x] **HTTP basic auth** — RFC 7617, constant-time, dependency-free
      strict base64. v1.2.0 (§9).
- [x] **Secrets-provider integration** — `[[secrets]]` pluggable
      sources (env alias / file / command / OAuth2 client-credentials)
      through one resolve front door with env fallback; per-request
      re-resolution in LLM backends; `{{secret:NAME}}` headers + MCP
      child env. Secret material in TOML is a parse error. v1.3.0 (§6).

---

## Deferred — parked until demand pulls

**Status: deliberately not scheduled** (2026-06-11). With the
gap-analysis catalog closed, the runtime covers the single-process
deployment shape it targets end to end. Everything below is real,
designed-enough-to-start work — and exactly because each item is an
architecture commitment (a fleet story, a stateful product surface),
none of it starts on spec alone. An item leaves this section when a
concrete deployment hits its wall, the same way the catalog promoted
idempotency keys and secrets.

### Scale-out (design RFCs required first)

The single-process daemon stays the unit of correctness. Scale-out
composes daemons rather than complicating one:

- **Clustering** — N agentd processes behind shared triggers;
  leader-elected cron so schedules fire exactly once per fleet.
- **Work distribution** — a queue-backed trigger (NATS/SQS-class) so
  goals and runs submit to a pool; at-least-once delivery composing
  with the shipped per-route idempotency keys.
- **Coordination layer** — shared run-state store (leases, progress,
  outcomes) enabling hand-off, cross-node retries, fleet-wide budget
  accounting.
- **Fleet governance** — centrally-distributed signed policies and
  instruction files; per-tenant budget envelopes; audit shipping.

### Control plane (product surface)

The CLI inspector and the conformance suite are the substrate; a
control plane would turn them into a product:

- **Persistent run history** — a queryable store of run records with
  retention windows + audit search.
- **Conformance & drift dashboards** — suite results and
  baseline-drift over time, alerting on `pass_rate` regressions.
- **Inspector v2** — replay, cross-run search, run-diff on top of
  `/inspect`.
- **Plan review & approval queue** — a durable human-facing surface
  for `pause_for_approval` runs and promotion approvals.

### Substrate odds and ends

- **JWKS live fetch** — the OIDC verifier fetches + caches signing
  keys from the issuer's endpoint in-process (statically configured
  today; operators rotate externally and hot-reload).
- **Windows path-pattern canonicalisation** for `[policy.fs]` (the
  matcher is `/`-separated today; see maturity.md).
