# Samples — a guided tour

A runnable tour of agentd, from a frozen workflow to a self-planning,
human-gated, reliability-certified agent. Every sample under
[`examples/`](../examples) is a real, validated workflow; this walks
through them in the order the ideas build.

Build the binary once (the default build covers most samples; a few note
extra features):

```bash
cargo build --release -p agentd
alias agentd=./target/release/agentd
```

Each sample is `--validate-only`-clean. Run any of them with `--input`;
add `--record run.json` to any run and `agentd inspect run.json` to see
exactly what happened.

---

## 1 · The bounded workflow

The substrate: a predeclared DAG of typed nodes. The LLM fills one node;
the graph routes.

| Sample | Shows |
|---|---|
| [`llm-classifier.toml`](../examples/llm-classifier.toml) | an `llm_infer` node's structured answer drives a `switch` |
| [`webhook-receiver.toml`](../examples/webhook-receiver.toml) | an HTTP trigger (bearer / HMAC) into a bounded pipeline |
| [`cron-poller.toml`](../examples/cron-poller.toml) | a scheduled trigger |

```bash
agentd --config examples/llm-classifier.toml \
       --intel-unix /run/intel.sock --input doc.json --record run.json
agentd inspect run.json     # node timeline, per-node I/O, cost
```

## 2 · Bounded agentic steps

When one step needs open-ended investigation but the rest is fixed.

| Sample | Shows |
|---|---|
| [`agent-loop.toml`](../examples/agent-loop.toml) | a bounded ReAct loop inside one node (`max_steps`, tool subset, every call policy-gated) |
| [`multi-provider.toml`](../examples/multi-provider.toml) | named backends across Anthropic / OpenAI / Gemini / local |

## 3 · Human-in-the-loop (durable execution)

The line between automation and an agent that works alongside you. The
run checkpoints at the gate and stops; a person reviews, then resumes.

[`approval-gate.toml`](../examples/approval-gate.toml):

```bash
# Runs to the gate, writes a checkpoint, exits 7 (paused).
agentd --config examples/approval-gate.toml --state-dir /tmp/state \
       --input '{"service":"api","env":"prod"}' --record paused.json
agentd inspect paused.json          # see everything up to the pause

# After review — continue from the node after the gate.
agentd --config examples/approval-gate.toml --state-dir /tmp/state \
       --resume <run_id>            # → completed; the checkpoint retires
```

Exit code 7 lets a supervisor distinguish "awaiting approval" from
success (0) or failure (5).

## 4 · Composition

Compose the substrate — a workflow calls another as a sub-DAG under the
same policy and budget, never an orchestrator-of-agents.

[`subworkflow-parent.toml`](../examples/subworkflow-parent.toml) +
[`subworkflow-child.toml`](../examples/subworkflow-child.toml):

```bash
agentd --config examples/subworkflow-parent.toml --input '{"kind":"invoice"}'
# the `classify` call returns {result: …}; a child failure routes `error`
```

## 5 · The agent plans itself — then collapses to a bound

Instruction mode (RFC 0006): hand the agent a goal; it compiles its own
workflow, you approve it at the capability altitude, and you can
**promote** the approved plan into a durable, signed Mode-1 workflow.

[`self-planning-agent.toml`](../examples/self-planning-agent.toml) is a
self-contained agent spec (identity + standing task):

```bash
# Compile + review (capability summary, not raw TOML), and save it.
ANTHROPIC_API_KEY=… agentd --config examples/agent-loop.toml \
   --instructions examples/self-planning-agent.toml \
   --plan-only --promote workflows/log-auditor.toml

#   agentd: Plan `…`: reads env[…], writes file (…); Touches the world via: write_file
#   Runs under policy: fs.write [/tmp/agentd-reports/**] …
#   promoted to workflows/log-auditor.toml

# Run it unattended only after you approve it.
… agentd --config examples/agent-loop.toml \
   --instructions examples/self-planning-agent.toml --auto-approve
```

Instruction mode is the *design-time* fast path; the promoted workflow is
the *production* path — dynamism that collapses to a bound.

## 6 · Observability is built in

Every one-shot run takes `--record PATH`; `agentd inspect PATH` renders
the node timeline with each node's output, timing, cost, and policy
decisions. The record is plain JSON keyed for a dashboard, and its
`execution_id` lines up with the audit log. See operations.md §3.6–3.7.

## 7 · Reliability as a deliverable

`agentd-conformance` drives the real engine and measures what the runtime
promises. See [CONFORMANCE.md](CONFORMANCE.md).

```bash
# pass^k reliability, capability coverage, fault tolerance, security
# denials, cost-per-success — and the reliability gate.
cargo run -p agentd-conformance -- crates/agentd-conformance/corpus \
    --min-pass-rate 0.95 --forecast-runs-per-day 5000 --price-per-mtok 5

# Drift: gate future runs against a saved baseline (catch a model
# update that silently lowers reliability).
cargo run -p agentd-conformance -- crates/agentd-conformance/corpus \
    --save-baseline baseline.json
cargo run -p agentd-conformance -- crates/agentd-conformance/corpus \
    --baseline baseline.json
```

A workflow earns the right to run unattended by clearing the pass^k gate
in CI — autonomy you earn, measured.

## 8 · Author in TypeScript

TOML is the compile target, not the authoring surface.
[`@agentd/sdk`](../sdk/typescript) is a typed builder whose output
round-trips through `agentd --validate-only` in its own CI.

```ts
import { workflow, node } from "@agentd/sdk";
const wf = workflow("classifier")
  .start("main", "classify")
  .node("classify", node.llmInfer({ backend: "claude", prompt: "…", outputSchema: "inline" }))
  .node("done", node.terminate())
  .edge("classify", "done");
console.log(wf.toToml());   // → agentd --config - --validate-only
```

---

**The through-line:** a frozen, validated graph is the unit of
correctness. Autonomy is admitted on top of it — one bounded node, a
sub-loop, or a whole compiled plan — and always forced back through the
same validator, policy gates, audit trail, approval gate, and
reliability bar. You dial up autonomy by evidence, never by default.
