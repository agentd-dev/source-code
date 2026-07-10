# RFC 0024: The agentd evaluation harness — benchmarking the runtime × its paired intelligence

**Status:** Proposed (2026-07-10)
**Author:** Andrii Tsok
**Date:** 2026-07-10
**Part of:** the quality/credibility track. agentd is an agentic **harness**; this RFC defines how we measure an `agentd × model` configuration against public agentic benchmarks, and how we prove agentd is a *good* harness. References the workflow dialect (RFC 0021), MCP client transport (RFC 0004), telemetry contract (RFC 0016), and the embedding/code-tools surface (RFC 0022).

---

## 1. Problem / Context

We ship a harness (agentd) and pair it with an intelligence (an LLM endpoint,
RFC 0006/0018). We have no *external* yardstick for how that pairing performs on
real agentic work, and no evidence that agentd is a competitive scaffold rather
than merely a working one.

The load-bearing fact from the 2025–26 evaluation literature is that **the
harness moves scores more than the model does.** Harness-Bench and the Holistic
Agent Leaderboard (HAL) report the *same model* swinging **34–48 percentage
points** on SWE-bench Verified purely from scaffold changes — an order of
magnitude larger than the 2–4 pp deltas papers report as model progress. The
field's conclusion: **a score belongs to a `model × harness` configuration, never
to a model alone.**

That reframes benchmarking for us into **two experiments, both valuable**:

1. **Model quality, agentd fixed** — which paired intelligence is best *in our
   runtime* (the buyer's question).
2. **Harness quality, model fixed** — is agentd competitive with a reference
   scaffold (e.g. `mini-swe-agent`, ~100 lines, >74% on SWE-bench Verified) when
   both drive the same model? This is the more novel, more defensible claim and
   the one that markets agentd.

**This RFC owns:** the benchmark portfolio, the mapping to agentd's surfaces, the
workflow-evaluation methodology, the orchestration architecture, the metric set,
and a runnable Phase-0 reference. **It does not own:** the runtime under test
(that is the rest of agentd), nor the third-party benchmarks/graders themselves
(we integrate them, we do not re-implement them).

## 2. Design principles

1. **Measure the configuration.** Every result is stamped with
   `{agentd version, harness config, model id, mode}` (the Harness-Bench
   discipline). A bare number is not a result.
2. **MCP-native ⇒ adapters are tool-bridges, not harnesses.** agentd already
   *is* the ReAct loop, retries, budgets, subagents, and telemetry. Because it
   speaks MCP natively, nearly every agentic benchmark reduces to "stand up the
   right MCP server(s) + capture the deliverable." We write a **tool-bridge per
   environment**, not a harness per benchmark. This is the leverage the whole
   design rests on.
3. **Report reliability and cost, not just accuracy.** Headline pass@1 hides the
   production picture. We report **pass^k** (all *k* i.i.d. trials succeed — a
   90% pass@1 collapses to 57% at k=8) and **cost-per-solved-task** alongside
   accuracy. These are the metrics production-oriented leaderboards emphasise.
4. **Coordination must earn its cost.** Workflows/subagents are evaluated by
   *ablation* against plain ReAct, measured cost-adjusted — because the evidence
   is mixed (a single agent matches or beats multi-agent on ~64% of tasks at
   ~half the cost; parallel decomposition wins big on genuinely wide tasks). The
   honest deliverable is a curve showing **where** decomposition pays off, not a
   claim that it always does.
5. **Contamination hygiene.** Prefer `-Verified` / `v2` / held-out variants;
   several headline benchmarks have been retired by their maintainers for
   contamination. Record dataset version + date on every run.
6. **Tooling stays out of the moat.** The runner is external tooling (Python —
   the benchmark ecosystem's lingua franca, and where the native graders live),
   not a runtime dependency. The `agentd-core` dependency count is untouched.

## 3. The benchmark portfolio

Tiered by **fit to agentd**, not by fame. `⭐` = native fit.

| Benchmark | Measures | Fit | Cost |
|---|---|---|---|
| **MCP-Universe** / MCP-AgentBench / MCP-Atlas | task completion *through real MCP servers* (nav, repo, finance, browser, web-search); 231 tasks / 11 servers | ⭐ **Native** — agentd is the runtime built for exactly this; frontier models score low (GPT-5 ~44%, Sonnet ~29%) → headroom + on-message story | Low–med |
| **τ²-bench (tau2)** | tool + simulated-user + **policy adherence**, stateful DB, **pass^k**; retail/airline/telecom/banking, dual-control | ⭐ domain tools → MCP; simulated user → A2A peer or the RFC 0021 `human` gate; policy → instruction; reliability → durability story | Low–med |
| **BFCL v4** | function-call correctness (AST + executable), multi-turn, +web/memory | ⭐ cheap **unit-test of the tool loop** — gates the pairing before spending on the big ones | Very low |
| **GAIA** | generalist assistant Qs (web + files + multi-hop), 3 levels, exact-match | web/file/code MCP; **Levels 2–3 are the workflow showcase** | Med |
| **SWE-bench Verified** | resolve real GitHub issues; graded by the repo's own tests | headline credibility; shell+fs MCP over a per-task sandbox; capture the git diff; **baseline vs mini-swe-agent** | High (Docker, ~120 GB) |
| **Terminal-Bench 2** | hard terminal tasks; Harbor framework | de-facto standard; Harbor has a clean custom-agent adapter | Med–high |

**Start with the top three** — they exercise agentd's actual core (MCP
tool-calling, multi-turn, policy) at low cost and infra.

## 4. Mapping to agentd surfaces

| agentd surface | Benchmark it lights up |
|---|---|
| MCP tool-calling (RFC 0004) | MCP-Universe, MCP-AgentBench, BFCL |
| Reactive + A2A + `human` gate (RFC 0008/0020/0021) | τ²-bench (simulated user, dual-control) |
| Workflows: fan-out, subgraph/join, CEL, durable checkpoint, resume (RFC 0021) | the ablation (§5) across GAIA-L3 / τ² / SWE-bench |
| Subagent tree + context isolation (RFC 0009) | PerspectiveGap-style context-distribution checks |
| Durability / checkpointer (RFC 0021) | pass^k reliability across all of the above |
| Budgets: `--max-tokens`, `--budget-tokens-lifetime` (RFC 0025) | cost-adjusted scoring; bounded sweeps |
| Static `FROM scratch` binary | trivial per-task container isolation |
| JSON-lines telemetry + metrics (RFC 0016) | tokens/steps/tool-errors/latency capture, free |

## 5. Evaluating the workflow capability (the ablation)

Most agentic benchmarks test one ReAct loop, so workflows are evaluated by
running the **same task set** through progressively richer configurations and
measuring the delta:

- **A. Plain** — `once` mode, single loop.
- **B. Workflow** — decompose → fan-out subagents → join/synthesize.
- **C. Workflow + durability** — add the MCP checkpointer + resume; report
  **pass^k**.

Every cell is reported **cost-adjusted** (tokens + wall-clock). The expected —
and publishable — result is a **curve**: decomposition wins on wide/multi-hop
tasks (GAIA-L3, τ² multi-step) and loses on simple ones. That "where it pays
off" map is more useful and more honest than a single lift number.

## 6. Orchestration architecture

```
   matrix runner  →  {benchmark} × {model} × {agentd config} × {N runs}
   (fan-out)                         │ one task
                    ┌────────────────▼─────────────────┐
                    │  per-task container (isolated)    │
  model under   ──► │  agentd (once|workflow)  ◄─MCP─►  tool-bridge MCP server
  test (--intel)    │        │ JSON-lines + report        │ env / sandbox
                    └────────┼───────────────────────────┼──────────────┘
                             ▼                            ▼
                   telemetry capture               native grader
              (wall-clock, tool-calls, exit,   (repo tests / reward /
               tokens where surfaced)           exact-match / pass^k)
                             └──────────────┬─────────────┘
                                            ▼
                                   unified scorecard
```

**Four pieces, one of them the real investment:**

1. **The tool-bridge (reusable).** An MCP server exposing each environment: a
   *shell+fs* server for SWE-bench/Terminal-Bench (bash + read/write/patch); a
   *web+file* server for GAIA; the *domain-API* servers for τ² (retail/airline
   over a stateful DB). Built once per environment; thereafter every benchmark on
   that environment is a config change, not new harness code. agentd's own
   mock-MCP scaffolding is the pattern to copy.
2. **The per-benchmark adapter (thin).** Render the task into `--instruction`,
   point `--mcp` at the bridge and `--intelligence` at the model, set budgets,
   run, **capture the deliverable** (SWE-bench → git diff → `preds.json`; τ² →
   final DB state/actions; GAIA → answer string), hand it to the benchmark's
   **native grader** unchanged.
3. **The metric layer.** agentd already emits JSON-lines telemetry (RFC 0016), so
   per run we get tool-call counts, latency, exit code, and — where surfaced —
   tokens/steps for free. Report pass@1, pass^k, cost-per-solved-task, and the
   workflow-lift delta.
4. **The matrix runner + isolation.** Sweep the grid, one task per container
   (cheap: static binary), fan out across workers. **agentd can orchestrate the
   sweep itself** (reactive/workflow fan-out over a task queue) — a dogfood that
   is also a second-order test of the workflow engine at scale. Every result
   stamped with `{agentd version, config, model, dataset@version}`.

## 7. Metrics & scorecard

Per `{benchmark, model, config}` cell:

- **pass@1** — mean single-run success.
- **pass^k** — fraction of tasks solved on *every* one of *k* runs (reliability).
- **cost** — tokens (in/out) and wall-clock per task; **cost-per-solved-task**.
- **tool-call error rate** — malformed/failed tool calls / total (from telemetry).
- **exit distribution** — clean vs budget(7) vs error, per the exit-code contract.
- **workflow-lift** — Δ(accuracy, cost) of config B/C vs A on the same tasks.

The scorecard is machine-readable JSON + a printed table; it can back a served
`agentd://` view or a static report page.

## 8. Phasing

- **Phase 0 (done):** the reference runner + offline smoke — cost-adjusted
  scoring + config comparison (§9).
- **Phase 1 (in progress):** ✅ **BFCL** landed — the generic tool-bridge MCP
  stub, the tool-call grader, and the BFCL converter (§9). ✅ The **τ²-bench
  foundation** — the tool-bridge is now a **stateful environment** (tool
  `effect`s over a JSON store) and grading can be **outcome-based**
  (`grade.state`: the world must reach the expected end-state), the τ² grading
  model. Both proven end-to-end offline. Remaining τ² piece: the simulated user
  (a second model), which maps onto agentd's A2A / `human` gate. Next: point at
  the real MCP-Universe servers + τ²-retail data.
- **Phase 2:** SWE-bench Verified (with the mini-swe-agent baseline) + GAIA — the
  shell/web bridges + headline credibility.
- **Phase 3:** the workflow-lift ablation matrix (§5) across GAIA-L3 / τ² /
  SWE-bench.

## 9. Phase-0 reference implementation (`bench/`)

A dependency-free (Python stdlib) runner lives at `bench/`:

- `bench/run.py` — spawns `agentd` per task (booting its built-in mock LLM /
  mock MCP so the rig runs **offline, no API keys**), captures stdout (the
  deliverable) + stderr (telemetry) + exit + wall-clock, grades against a
  per-task matcher, and aggregates **pass@1 / pass^k** plus **cost-adjusted**
  metrics — **tokens, steps, cost-per-solved-task** (§7) — into a JSON scorecard
  + a printed table. Cost is real, at no runtime cost: the child loop already
  reports per-run usage in its `loop.final` telemetry, which the runner sums
  across the subagent tree.
- `bench/compare.py` — diffs two scorecards side-by-side with deltas + per-task
  regressions/fixes. This is the thesis's core operation (a score is a
  *comparison*): model A vs B, agentd vs a reference scaffold, or plain `once`
  vs a fan-out `workflow` (the §5 ablation).
- **Phase 1 (BFCL):** `bench/mcp_stub.py` — the **generic tool-bridge** (§6): a
  configurable MCP server that serves an arbitrary tool set to agentd, so a
  benchmark environment is a data file, not harness code. It is **stateful** — a
  tool may declare an `effect` (`set`/`append`/`return` over a dotted-path JSON
  store) and the world is persisted for grading. `bench/graders.py` — the
  **tool-call grader** (BFCL-style name + args match) **and the outcome grader**
  (`grade_state`: the final environment must satisfy an expected subset — the
  τ²-bench model). `bench/bfcl.py` — the **BFCL converter** (functions →
  tool-bridge tools, question → instruction, ground-truth → matcher). agentd
  exposes MCP tools by their verbatim catalogue name, so BFCL function names map
  straight through.
- `bench/tasks/{smoke,bfcl_smoke,tau2_smoke}.jsonl` — offline suites: the
  mechanics (pure-answer + tool-call ReAct), the **full BFCL pipeline**
  (tool-bridge serves a function → the `mcp-call` mock model calls it → the
  grader scores name + args), and the **τ²-shaped stateful pipeline** (a tool
  `effect` mutates the environment → outcome grading on the end-state) — all with
  no keys.
- A task's `intelligence` / `model` / `tool_server` (or `mcp`) fields make
  **pointing at a real model + real tools a data change**, not a code change.

Phase 0 deliberately tests the *harness plumbing* (drive agentd → capture
deliverable → grade → aggregate cost/telemetry → compare configurations), which
is the reusable core every later phase builds on, with zero external cost.

## 10. Non-goals / caveats

- Not a new benchmark — we integrate public ones and their native graders.
- Not a runtime dependency — the runner is external tooling; the moat holds.
- Token/step **cost is captured from the child's `loop.final` telemetry** (the
  loop reports real per-run usage; the runner sums it across the subagent tree),
  so cost-adjusted scoring works with no runtime change. The durable *run report*
  (`report.usage`, RFC 0016 §6.2) still records honest-absence zeros in `once`
  mode; wiring it to the real total is a separate, optional follow-up (the
  telemetry path is sufficient for the harness).
- SWE-bench full is expensive; the Verified 500 / Mini subset is the default.

## 11. References (external)

Harness-Bench (arXiv 2605.27922); Holistic Agent Leaderboard (arXiv 2510.11977);
MCP-Universe (arXiv 2508.14704); MCP-AgentBench (arXiv 2509.09734); τ-bench
(arXiv 2406.12045) + τ²-bench (sierra-research); BFCL (PMLR v267) + BFCL v4;
SWE-bench Verified + mini-swe-agent; Terminal-Bench / Harbor; GAIA (arXiv
2311.12983) + HAL leaderboard. Pass^k reliability framing per the τ-bench paper.
