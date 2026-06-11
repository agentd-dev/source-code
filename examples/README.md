# Examples

Runnable, `--validate-only`-clean workflows. Start with the guided tour
in [docs/SAMPLES.md](../docs/SAMPLES.md); this is the index.

| File | Demonstrates |
|---|---|
| [`llm-classifier.toml`](llm-classifier.toml) | an `llm_infer` answer drives a `switch` — the model fills one node, the graph routes |
| [`webhook-receiver.toml`](webhook-receiver.toml) | an authenticated HTTP trigger into a bounded pipeline |
| [`cron-poller.toml`](cron-poller.toml) | a scheduled (cron) trigger |
| [`agent-loop.toml`](agent-loop.toml) | a bounded ReAct loop inside one node (`max_steps`, tool subset, policy-gated) |
| [`evaluator-optimizer.toml`](evaluator-optimizer.toml) | a declared bounded cycle (`max_iterations` loop edge): generate → evaluate → retry, capped |
| [`multi-provider.toml`](multi-provider.toml) | named backends across Anthropic / OpenAI / Gemini / local |
| [`approval-gate.toml`](approval-gate.toml) | human-in-the-loop: `pause_for_approval` → checkpoint → `--resume` |
| [`subworkflow-parent.toml`](subworkflow-parent.toml) + [`subworkflow-child.toml`](subworkflow-child.toml) | composition: a `call` node runs another workflow as a sub-DAG |
| [`parallel-fanout.toml`](parallel-fanout.toml) | concurrent fan-out: a `parallel` node runs sub-workflows on threads, then joins |
| [`map-fanout.toml`](map-fanout.toml) | bounded fan-out over DATA: a `map` node runs one sub-workflow per array element (mandatory `max_items`), joins in input order |
| [`self-planning-agent.toml`](self-planning-agent.toml) | an instructions file (`[agent]`) with a standing task — instruction mode + `--promote` |

```bash
agentd --config examples/<file>.toml --validate-only      # check it
agentd --config examples/<file>.toml --input event.json --record run.json
agentd inspect run.json                                    # see what happened
```

`self-planning-agent.toml` is an *instructions* file, not a workflow —
pair it with a `--config` environment (see docs/SAMPLES.md §5).
