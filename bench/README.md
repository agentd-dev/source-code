# `bench/` — the agentd evaluation harness (Phase 0)

The Phase-0 reference runner for [RFC 0024](../rfcs/0024-evaluation-harness.md):
benchmark an **`agentd × model`** configuration by driving the real `agentd`
binary per task, capturing the deliverable + telemetry, grading it, and
aggregating **pass@1 / pass^k / wall-clock / tool-calls** into a scorecard.

It is dependency-free (Python 3 stdlib) and, in Phase 0, **runs offline** — it
boots agentd's own built-in mock LLM and mock MCP server, so you get a working
rig with no API keys and no external datasets. The point of Phase 0 is to prove
the *reusable core* every later phase builds on:

```
drive agentd  →  capture deliverable (stdout)  →  grade  →  aggregate telemetry (stderr)
```

## Why this shape (the load-bearing idea)

The 2025–26 literature is blunt: **the harness moves benchmark scores more than
the model does** (the same model swings 34–48 pts on SWE-bench Verified from
scaffold changes alone). So a score belongs to a `model × harness`
*configuration*, never a model. Every scorecard here is stamped with
`{agentd version, config, model, dataset}` for exactly that reason.

And because agentd is **MCP-native**, a benchmark adapter is mostly a
*tool-bridge* (stand up the right MCP server + capture the deliverable), not a
new harness — agentd already is the ReAct loop, retries, budgets, subagents, and
telemetry.

## Quick start (offline, no keys)

```console
$ cargo build -p agentd-cli            # produces target/debug/agentd (carries the mocks)
$ python3 bench/run.py --repeats 3
```

```
task             pass@1  pass^k  tokens  steps  wall_s  tools  why
-----------------------------------------------------------------
final-answer       PASS    PASS    16.0    1.0   0.209    0.0
tool-call-cycle    PASS    PASS    34.0    2.0   0.213    1.0
-----------------------------------------------------------------
pass@1: 100.0%   pass^3: 100.0%   (2/2 tasks)
tokens: 50 total   cost/solved: 25.0 tokens
scorecard -> bench/scorecard.json
```

Tokens and steps are real — the child loop already reports per-run usage in its
`loop.final` telemetry (`agentloop/runner.rs`), which the runner sums across the
whole subagent tree. No runtime change is needed to capture cost.

Flags: `--agentd <path>` (or `AGENTD_BIN`), `--tasks <file.jsonl>`,
`--repeats <k>` (k for pass^k), `--timeout <s>`, `--config <label>`,
`--out <scorecard.json>`.

## Task format (`tasks/*.jsonl`, one JSON object per line)

**Offline (Phase 0 — uses agentd's built-in mocks):**

```json
{"id": "final-answer",
 "instruction": "Give a short final answer.",
 "mock_llm": "final",
 "grade": {"contains": "mock-llm done"},
 "expect": {"completed": true}}
```

```json
{"id": "tool-call-cycle",
 "instruction": "Read the resource, then answer.",
 "mock_llm": "read",
 "mock_mcp": {"uri": "file:///in.json", "emit": false, "name": "mock"},
 "grade": {"contains": "read complete"},
 "expect": {"tool_call": "resource.read", "completed": true}}
```

- `mock_llm` — a built-in mock-LLM script (`final`, `read`, `schedule`, …).
- `mock_mcp` — boots the built-in mock MCP server serving `uri`.
- `grade` — deliverable matcher: `contains` | `exact` | `regex` (on stdout).
- `expect` — telemetry assertions: `completed` (loop reached `status=completed`),
  `tool_call` (a `tool.call` for the named tool was observed).
- Optional budgets: `max_tokens`, `max_steps`, `deadline`.

**Real (Phase 1+ — point at a real model + real MCP servers, a *data* change):**

```json
{"id": "mcpu-nav-001",
 "instruction": "…the benchmark task prompt…",
 "intelligence": "https://gateway.example/v1",
 "model": "claude-opus-4-8",
 "mcp": ["maps=https://mcp.maps.example/mcp", "search=https://mcp.search.example/mcp"],
 "grade": {"regex": "\\b1600 Amphitheatre\\b"}}
```

Set the model credential via `AGENT_INTELLIGENCE_TOKEN` in the environment. No
runner code changes — swapping the mock fields for `intelligence`/`mcp` is all it
takes. That is the on-ramp to the Phase-1 benchmarks.

## Metrics

- **pass@1** — mean single-run success.
- **pass^k** — solved on *every* one of `k` runs (reliability, not luck — a 90%
  pass@1 is only 57% pass^8). Use `--repeats k`.
- **tokens / steps** — real per-run usage, summed across the subagent tree from
  `loop.final` telemetry.
- **cost/solved** — tokens spent per task actually *solved* (RFC 0024 §7's
  cost-adjusted metric — the one that separates production-ready from
  demo-ready).
- **wall_s / tools** — per-run wall-clock and tool-call count.

## Comparing configurations

The eval thesis is comparative — a score belongs to a `model × harness`
*configuration*, so the useful output is a delta. `compare.py` diffs two
scorecards side-by-side (model A vs B; agentd vs a reference scaffold; plain
`once` vs a fan-out `workflow`):

```console
$ python3 bench/run.py --config opus   --out cardA.json   # model/config A
$ python3 bench/run.py --config sonnet --out cardB.json   # model/config B
$ python3 bench/compare.py cardA.json cardB.json
```

```
metric                       A           B   B−A
---------------------------------------------------
pass@1                  100.0%      100.0%   → +0.0%
cost/solved (tok)         25.0        25.0   → +0.0
...
  B fixes (2): task-7, task-9
  B regresses (1): task-3
```

## Roadmap (RFC 0024 §8)

- **Phase 0 (here):** the runner + offline smoke suite — proves the plumbing.
- **Phase 1:** BFCL (tool-call correctness), MCP-Universe (native MCP tasks),
  τ²-bench-retail (tool + simulated-user + policy + pass^k).
- **Phase 2:** SWE-bench Verified (shell+fs MCP bridge; baseline vs
  mini-swe-agent) + GAIA (web/file MCP).
- **Phase 3:** the workflow-lift ablation — plain `once` vs a fan-out/subagent
  workflow vs workflow+durability, reported cost-adjusted, to map *where*
  decomposition pays off.
