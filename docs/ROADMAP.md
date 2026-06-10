# Roadmap

Direction, not promises. Items graduate from here into RFCs.

## Now (v0.8 — the dynamic harness, RFC 0006)

- [x] Named intelligence backends: Anthropic / OpenAI / Gemini /
      openai-compatible (`intel-remote`), Unix socket, HTTP JSON-RPC.
- [x] `agent_loop` node — bounded agentic steps inside the graph.
- [x] Goal mode — planner-generated workflows with validation,
      approval gates, and bounded re-planning.
- [x] Instruction files (`--instructions agent.toml`).
- [x] Token budgets + LLM usage metrics.

## Next

- [ ] Declared bounded cycles (`max_iterations` edges) —
      evaluator–optimizer patterns without an inner loop (RFC 0003 §5).
- [ ] Full JSON-Schema enforcement on `llm_infer` / loop outputs with
      schema-failure repair rounds.
- [ ] Checkpoint / resume: persist `ExecutionContext` at node
      boundaries; `--resume RUN_ID`. Replay from the execution trace.
- [ ] Parallel fan-out / fan-in (RFC 0001 §9.1).
- [ ] Plan library: promote approved goal-mode plans into versioned,
      signed Mode-1 workflows automatically.
- [ ] Conformance + benchmark suite (`crates/agentd-conformance`):
      scenario corpus, capability-matrix coverage, pass^k reliability
      scoring, fault-injection battery, injection-corpus security
      conformance, cost-per-success reporting.

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
- [ ] Windows path-pattern canonicalisation for `[policy.fs]`
      (matcher is `/`-separated today; see maturity.md).
