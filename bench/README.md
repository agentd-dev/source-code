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

- `mock_llm` — a built-in mock-LLM script (`final`, `read`, `mcp-call`, …).
- `mock_mcp` — boots the built-in mock MCP server serving `uri`.
- `tool_server` — boots the generic tool-bridge (see Phase 1) serving a tool set.
- `grade` — deliverable matcher: `contains` | `exact` | `regex` (on stdout), or
  `tool_calls` (a BFCL-style tool-call matcher, see Phase 1).
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

## Phase 1: real tool-use benchmarks (BFCL)

The reusable pieces that turn "run agentd" into "run a benchmark":

- **`mcp_stub.py` — the generic tool-bridge.** A configurable MCP server that
  exposes an arbitrary tool set (from JSON) over the transport agentd speaks, so
  a benchmark that provides its own functions is a *data file*, not new harness
  code (RFC 0024 §6). A task's `tool_server` field boots it:

  ```json
  {"id": "…", "instruction": "…", "intelligence": "https://gw/v1", "model": "…",
   "tool_server": {"name": "bfcl", "tools": [
       {"name": "get_weather", "description": "…",
        "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}}}]},
   "grade": {"tool_calls": {"name": "get_weather", "args": {"city": "Paris"}}}}
  ```

  agentd exposes MCP tools by their **verbatim** catalogue name, so BFCL function
  names map straight through — the grader matches ground-truth names directly.

- **`graders.py` — the tool-call grader.** Reads `tool.call` telemetry (name +,
  under `--log-content`, arguments — the runner adds it automatically) and matches
  a ground truth: name + each named arg, where a value may be a **list of
  acceptable forms** (BFCL-style) and `""`-marked params are optional. Supports
  alternatives (any-one-matches) and `{"all": [...]}` (all-must-appear). Run its
  self-checks: `python3 bench/graders.py`.

- **`bfcl.py` — the BFCL converter.** Turns BFCL question + answer files into
  runner tasks (functions → tool-bridge tools, question → instruction,
  ground-truth → `tool_calls`):

  ```console
  $ python3 bench/bfcl.py --questions BFCL_v3_simple.json \
      --answers possible_answer/BFCL_v3_simple.json \
      --intelligence https://gateway.example/v1 --model claude-opus-4-8 \
      --out bench/tasks/bfcl_simple.jsonl
  $ AGENT_INTELLIGENCE_TOKEN=sk-... python3 bench/run.py \
      --tasks bench/tasks/bfcl_simple.jsonl --repeats 5 --config opus
  ```

  Self-check: `python3 bench/bfcl.py --selftest`. Covers BFCL's single/parallel
  *AST* categories; executable / multi-turn categories want BFCL's own runtime.

The whole pipeline is proven **offline** (no keys) by `tasks/bfcl_smoke.jsonl`:
the tool-bridge serves `bench_echo`, the built-in `mcp-call` mock model calls it,
and the grader scores the name + args —

```console
$ python3 bench/run.py --tasks bench/tasks/bfcl_smoke.jsonl
bfcl-echo-offline    PASS    PASS    34.0    2.0    0.21    1.0
```

## Stateful environments + outcome grading (τ²-bench / MCP-Universe shape)

Tool-call correctness (did the agent call the right function) isn't enough for
benchmarks that grade an **outcome** — did the agent's write-actions leave the
environment in the right end-state (τ²-bench: the order is cancelled; the DB
reflects it). Two additions cover this:

- **The tool-bridge is stateful.** A tool may declare an `effect` over a shared
  JSON environment `state`, and the bridge persists the state so the grader can
  inspect it:
  - `{"set": "orders.o1.status", "value_arg": "status"}` — store an arg at a
    dotted path;
  - `{"append": "cart.items", "value_arg": "item"}` — append to a list;
  - `{"return": "orders.o1"}` — read a value back out (a lookup tool).
  A task seeds the initial world via `tool_server.state`.

- **`grade.state` — outcome grading.** Assert the final environment state matches
  a partial spec (a subset: named keys/values must be present; lists match
  element-for-element). Backed by `graders.grade_state`.

```json
{"id": "…", "instruction": "Cancel order o1.", "intelligence": "https://gw/v1",
 "tool_server": {"name": "retail",
   "state": {"orders": {"o1": {"status": "open"}}},
   "tools": [{"name": "cancel_order",
              "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}},
              "effect": {"set": "orders.o1.status", "value_arg": "id"}}]},
 "grade": {"state": {"orders": {"o1": {"status": "cancelled"}}}}}
```

Proven offline by `tasks/tau2_smoke.jsonl`: a stateful env + the `mcp-call` mock
model + an end-state assertion — no keys, no runtime change.

## The simulated-user loop (multi-turn τ²-bench)

τ²-bench is a *conversation*: the agent (with tools + a policy) talks to a
**simulated user** (a second model given a hidden scenario) over several turns
until the user's request is resolved. The harness runs it as a turn loop —
elegantly, **both sides are agentd `once` runs**: the agent turn carries the
policy + history + the stateful env bridge; the user turn is a second model with
the scenario and no tools, whose reply is the next user message. The env state
persists across turns, and grading is outcome-based on the end-state.

```json
{"id": "…", "policy": "You are a support agent. …the rules…",
 "instruction": "Hi, I need to cancel order o1.",          // the first user message
 "intelligence": "https://gw/v1", "model": "claude-opus-4-8",
 "max_turns": 6,
 "tool_server": {"name": "retail", "state": {…}, "tools": [{…, "effect": {…}}]},
 "user_simulator": {"intelligence": "https://gw/v1", "model": "…",
                    "scenario": "You are Ada; you want order o1 cancelled, …"},
 "grade": {"state": {"orders": {"o1": {"status": "cancelled"}}}}}
```

The loop ends when the user emits `###STOP###` (resolved) or `max_turns` is hit;
the metrics sum tokens/steps/tool-calls across every turn. A task with a
`user_simulator` is automatically run as a conversation.

Proven offline by `tasks/tau2_convo_smoke.jsonl` (agent = `mcp-call`, user =
`final`, bounded by `max_turns`): a full agent↔user exchange with a stateful env,
graded on the end-state — no keys, no runtime change.

## Shell / file environments (SWE-bench / Terminal-Bench shape)

Coding benchmarks grade a **filesystem outcome**: the agent runs commands and
edits files, and success is "do the repo's tests pass now." The same tool-bridge
serves this — a tool with a `builtin` handler over a per-task **sandbox** dir:

- `{"builtin": "bash"}` — run `arguments.command` in the sandbox (stdout/stderr/exit);
- `{"builtin": "read_file"}` / `{"builtin": "write_file"}` — confined to the sandbox.

A task seeds the initial tree via `tool_server.files` (path → content), and
grades with `grade.files` (path → `contains`/`exact`) and/or `grade.command`
(run a check command — e.g. the repo's tests — assert its exit):

```json
{"id": "…", "instruction": "Make the failing test pass.",
 "intelligence": "https://gw/v1", "model": "…",
 "tool_server": {"name": "repo",
   "files": {"app.py": "…", "test_app.py": "…"},
   "tools": [{"name": "bash", "builtin": "bash",
              "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}}}}]},
 "grade": {"command": {"run": "pytest -q", "expect_exit": 0}}}
```

Proven offline by `tasks/swe_smoke.jsonl`: the bridge serves `bash`, the built-in
`shell-call` mock model runs a command that writes a file, and grading is
filesystem-based (`grade.files` + `grade.command`) — no keys, no Docker. (A real
SWE-bench run seeds the repo at the pre-fix commit and grades with its test
command; the sandbox should be a container.) `python3 bench/mcp_stub.py --selftest`
covers the bash/file builtins directly.

## The workflow-lift ablation (agentd's distinctive evaluation)

Most harnesses can't measure this: agentd has declarative **workflows**, so we
can run the *same* task suite under progressively richer structures and ask what
decomposition actually buys. `bench/ablate.py` runs each task as:

- `once` — a single ReAct loop (baseline);
- `workflow` — the task wrapped in a one-agent graph (orchestration overhead);
- `fanout-N` — a `foreach` graph fanning the task across N parallel subagents.

and reports **accuracy × cost** per config. Cost is summed across the whole
subagent tree (every `loop.final`), so a fan-out's N× token cost is visible.

```console
$ cargo build -p agentd-cli --features workflow      # workflow mode needs this
$ python3 bench/ablate.py --repeats 2
config        pass@1  pass^k  tok/task  Δcost   verdict
----------------------------------------------------------
once           100%   100%     34.0    +0%   baseline
workflow       100%   100%     34.0    +0%   ≈ baseline
fanout-3       100%   100%    102.0  +200%   cost, no gain
```

The verdict is deliberately honest: the evidence on multi-agent systems is mixed
(a single agent often matches a fan-out at a fraction of the cost; genuinely-wide
tasks win big), so extra cost only counts as a win when it **buys accuracy**. On
a trivial task the fan-out is pure cost (as shown); with a real model on a
decomposable task it would read `+X% acc` — that curve is the deliverable. The
ablation uses outcome-graded tasks (state/files), whose grading is
config-agnostic (the env end-state is what matters, however the work is
structured). Proven offline by `tasks/ablation_smoke.jsonl`.

## Live-model results (real OpenAI, 2026-07)

The harness has been run end-to-end against live OpenAI models — the first proof
that the plumbing works against a real provider, not just the offline mock. Three
models across three benchmark *shapes* (five cells each): BFCL tool-calling
(`simple`/`multiple`/`parallel`, 25 tasks each), the execution-graded `code`
suite (5 tasks, graded by running the fixed module's test), and the `tau2`
simulated-user conversation (5 retail-policy scenarios, outcome-graded on the
persisted env state). Every agent turn drives the real `agentd` binary; the τ²
user is a second live model given a hidden scenario.

```
config                 pass@1  pass^k  cost/solved(tok)  tasks
-----------------------------------------------------------
gpt-4o-mini_simple      100%   100%             3519     25
gpt-4o-mini_multiple     92%    92%            11631     25
gpt-4o-mini_parallel     92%    92%             3720     25
gpt-4o-mini_code        100%   100%            11854      5
gpt-4o-mini_tau2        100%   100%             4748      5
gpt-4.1-mini_simple      96%    96%             3652     25
gpt-4.1-mini_multiple    92%    92%             3846     25
gpt-4.1-mini_parallel    92%    92%             3715     25
gpt-4.1-mini_code       100%   100%            11457      5
gpt-4.1-mini_tau2       100%   100%             4726      5
gpt-5.1_simple          100%   100%             3672     25
gpt-5.1_multiple         92%    92%             4444     25
gpt-5.1_parallel         88%    88%             8329     25
gpt-5.1_code            100%   100%            10834      5
gpt-5.1_tau2            100%   100%             6103      5
```

Reproduce with `bench/matrix.py <scorecard.json> ...` over the per-cell
scorecards (the `bench` GitHub workflow runs a single BFCL cell on demand). The
BFCL cells are graded by the faithful AST value-matcher (`graders._deep_eq`).

**Reading it honestly.** The point of this run was to *prove the harness*, and it
did — end to end it surfaced (and we fixed) three real agentd bugs that only a
live provider exposes: dotted tool names rejected on the wire, reasoning models
needing `max_completion_tokens`, and a transient 429/5xx becoming an immediate
exit 4 in `once` mode (now a bounded same-endpoint retry). The *rankings*,
though, are noisy and should not be over-read:

- **Small samples.** 25 BFCL / 5 code / 5 τ² tasks per cell — a couple of tasks
  swing several points. Treat gaps under ~10pts as noise.
- **Grader fidelity matters — and was measured.** An earlier *subset* grader
  compared arg values naively and marked cosmetic differences wrong ("New York"
  vs `new_york`, `5` vs `5.0`, list ordering), which understated the models on
  `multiple`/`parallel` by up to ~12pts (gpt-5.1 `multiple` read 80%). Swapping
  in BFCL's AST value-matching rules (standardized strings, int/float coercion,
  order-sensitive lists, recursive dicts) lifted the non-saturated cells to a
  tight ~92% band across all three models and left the genuinely-failing cases
  failing (gpt-5.1 `parallel` held at 88%) — a fidelity fix, not inflation.
- **`code` and `tau2` saturate at 100%.** The coding tasks are small and the
  retail-policy scenarios are within reach of all three models, so accuracy
  can't separate them — the live signal there is **cost**: τ²'s
  `cost/solved` cleanly ranks 4.1-mini (4726) < 4o-mini (4748) < 5.1 (6103),
  and 5.1 occasionally spends an extra tool call to reach the same end-state.

The value is the *infrastructure*, verified against a real model: a stateful
outcome grader, a multi-turn simulated user, execution grading, a faithful AST
tool-call grader, and cost-adjusted comparison all working against live OpenAI.
Bigger, cleaner numbers are now a matter of more tasks, not more plumbing.

## Roadmap (RFC 0024 §8)

- **Phase 0 (done):** the runner + offline smoke suite — proves the plumbing.
- **Phase 1 (in progress):** ✅ **BFCL** (converter + tool-bridge + tool-call
  grader), ✅ the **τ²-bench foundation** (stateful tool-bridge + outcome/state
  grading), and ✅ the **simulated-user loop** (multi-turn agent↔user
  conversations, outcome-graded). Next: point them at real datasets/servers
  (MCP-Universe, τ²-retail) with a live model.
- **Phase 2 (in progress):** ✅ the **shell/file environment** (sandboxed
  `bash`/file builtins + filesystem/command grading) — the SWE-bench /
  Terminal-Bench substrate. Next: seed a real SWE-bench-Verified instance
  (repo@commit + test command) and add the mini-swe-agent baseline; GAIA
  (web/file bridge).
- **Phase 3 (in progress):** ✅ the **workflow-lift ablation** (`ablate.py`) —
  plain `once` vs a linear workflow vs a fan-out (`foreach`) workflow, reported
  cost-adjusted (accuracy × cost), to map *where* decomposition pays off
  (above). Next: workflow+durability (checkpointer) for pass^k, and running the
  ablation on decomposable real-model tasks (GAIA-L3, τ²).
