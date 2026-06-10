# RFC 0006: The dynamic harness — goals, loops, and providers on a governed substrate

**Status:** Accepted, implementation in progress.
**Author:** Andrii Tsok
**Depends on:** RFC 0001, RFC 0003, RFC 0005.

## 1. Retrospect

RFC 0003 drew a hard line: the runtime owns the loop, the model fills
nodes. That line bought auditability, bounded cost, and
injection-resistant control flow — and it still holds for everything
agentd was used for first: webhook handlers, pollers, classifiers.

It also left a class of work on the table, and §5 of RFC 0003
anticipated this day: tasks where *authoring the graph is the
expensive part*, where the step count is genuinely unknown, or where
an operator wants to hand the runtime a **goal** and an **instruction
file** rather than a finished workflow. The industry's hybrid
consensus (deterministic outer process, bounded model-driven inner
steps) matches what our own users ask for: "spin up an agent, give it
the task, let it work — as a daemon or as a one-shot — without losing
the governance story."

The retrospect conclusion is *not* that the bounded substrate was
wrong. It's that the substrate is the asset: validation, policy,
budgets, signing, audit, tracing. What must change is **who is
allowed to produce the structure that runs on it** — and under what
controls.

## 2. Decision: three execution modes, one substrate

Every mode produces the same artifact — a `WorkflowDoc` — and every
artifact passes the same validator, the same policy gates, the same
budgets, and emits the same audit stream. Dynamism enters through
controlled doors; nothing bypasses the substrate.

### Mode 1 — Workflow (existing)

A human-authored, optionally signed TOML DAG. Unchanged. Still the
recommended shape for anything run more than once.

### Mode 2 — `agent_loop`: a bounded agentic step inside the graph

A new node kind embeds a ReAct-style loop *as a node*:

```toml
[[nodes]]
id = "investigate"
type = "agent_loop"
backend = "claude"
instructions_from = "trigger.task"     # or `instructions = "..."`
tools = ["read_file", "http_request"]  # subset, by name — nothing implicit
max_steps = 8                          # required; validator-enforced
max_tokens = 40000                     # counts against the run budget
```

Inside the loop the model sees its instructions, the running
transcript, and *only the listed tools*. Each proposed call is
executed through the **same policy- and budget-gated implementations**
the declared node kinds use — a loop cannot reach anything the
workflow's `[policy]` would deny a regular node. Every step emits an
`agentd::audit` event (`loop.step`, `loop.tool_call`, `loop.final`).
The node's output is `{result, steps, transcript}`; downstream
routing on it remains the graph's job (RFC 0003 §2 intact: the loop
cannot add nodes or pick edges *outside itself*).

The loop is the relaxation RFC 0003 said would need its own RFC.
Its containment guarantees: author-declared tool subset,
author-declared step cap (hard ceiling 64), token budget, run
deadline still binding, dry-run executes zero tool calls.

### Mode 3 — Goal mode: the agent defines its own workflow

```bash
agentd --goal "Audit the access logs under /var/log/app and write a summary" \
       --instructions agent.toml --plan-only          # inspect the plan
agentd --goal @goal.txt --instructions agent.toml --auto-approve
```

A planner prompt (instructions + the build's *actual* node-kind
vocabulary + the active policy summary) asks the configured backend
to emit a workflow TOML. The plan is then treated exactly like a
human-authored one:

1. **Validated** by the standard validator. Validation errors are fed
   back to the model for a bounded number of repair rounds.
2. **Approval-gated.** The materialized plan prints in full.
   Headless runs refuse to execute without `--auto-approve`
   (fail-closed governance); `--plan-only` stops after printing.
3. **Executed** on the normal engine, under the normal policy,
   budgets, and audit.
4. **Bounded self-improvement.** On a failed outcome, the planner
   may revise the plan with the failure trace in context — at most
   `--max-replans` times (default 2). Every generation, approval,
   execution, and replan is an audit event with the plan content
   hashed into it.

The plan is a file. It can be saved, diffed, signed, and promoted
into a Mode-1 workflow — the intended lifecycle for anything that
proves itself.

## 3. Provider layer

`llm_infer` and `agent_loop` address backends by name:

```toml
[[intelligence.backends]]
name = "claude"
provider = "anthropic"            # anthropic | openai | gemini | openai-compatible
model = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"

[[intelligence.backends]]
name = "local"
provider = "openai-compatible"
base_url = "http://127.0.0.1:8000/v1"
model = "qwen3"
```

- Remote providers live behind the `intel-remote` Cargo feature
  (pulls a small blocking HTTPS client; the dep-light core stays
  dep-light). The existing Unix-socket and plain-HTTP JSON-RPC
  transports are unchanged and register as the `default` backend
  when their flags are present.
- API keys come from the environment by name (`api_key_env`) — never
  from the TOML, so workflow files stay shareable and signable.
- `openai-compatible` + `base_url` covers vLLM, Ollama, LM Studio,
  and any future provider speaking that dialect; first-party
  Anthropic / OpenAI / Gemini get native request shapes.
- Backend definitions hot-reload with the rest of the config
  (RFC 0005 semantics); key rotation is an env + HUP away.

## 4. Instruction files

`--instructions agent.toml` defines the agent's standing identity:

```toml
[agent]
name = "log-auditor"
system = """You are a careful operations assistant..."""
default_backend = "claude"
loop_tools = ["read_file", "json_select"]   # default agent_loop subset
```

Instructions feed the planner (Mode 3) and any `agent_loop` that
doesn't override them. They are config, not code: signable,
diffable, reloadable.

## 5. Governance & observability additions

- `[budget].max_llm_tokens` — cumulative per-run token ceiling,
  enforced in `llm_infer` and inside `agent_loop` steps.
- `agentd_llm_tokens_total` / `agentd_llm_calls_total` metrics.
- Audit events: `plan.generated`, `plan.approved`, `plan.rejected`,
  `plan.replanned`, `loop.step`, `loop.tool_call`, `loop.final`,
  each carrying backend, token usage, and (for plans) a content hash.
- The approval gate's default is *refuse*: autonomy is opt-in per
  invocation, never ambient.

## 6. Explicit non-goals (this RFC)

- Unbounded loops. `max_steps` stays required and capped.
- Model-initiated capability acquisition. Tool subsets and policy
  are author/operator-declared, always.
- Distributed execution. Clustering, work distribution, and a
  coordination layer are roadmap items (see `docs/ROADMAP.md`) —
  the single-process daemon must be excellent first.

## 7. Consequences

agentd's honest description changes from "a workflow runtime" to
**"an agent harness with three governed execution modes"**. The
security story survives intact because every dynamic pathway
materializes into the same validated, policy-bound artifact — what
changed is that the runtime can now *write* on that substrate, not
only *execute* what a human wrote.
