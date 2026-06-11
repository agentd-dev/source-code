# RFC 0003: The agent loop — sequential bounded traversal

**Status:** Accepted, implemented.
**Author:** Andrii Tsok
**Depends on:** RFC 0001 §8–§9.

## 1. Problem

Every LLM-agent runtime has to answer one question first: **who owns
the loop?** Two architectures dominate:

1. **Model-owned (ReAct-style).** The LLM emits a thought and an
   action; the runtime executes the action and feeds the observation
   back; repeat until the model decides to stop. Maximum flexibility —
   the model can pursue tasks nobody enumerated in advance.
2. **Runtime-owned (workflow-style).** Control flow is fixed before
   execution; the model fills designated reasoning steps. Maximum
   predictability — every possible action is enumerable before the
   process starts.

This RFC records why `agentd` is permanently in camp 2, what exactly
the loop does, and which relaxations we will and will not entertain.

## 2. Decision

The engine is a **sequential interpreter over a predeclared DAG**:

```
resolve start ─► [deadline check ─► dispatch node ─► record output
      ▲                                   │
      └────────── follow matching edge ◄──┘ ]   × at most MAX_STEPS
```

- One node executes at a time. A node has at most one unconditional
  out-edge and any number of `when`-labelled edges.
- Handlers never select successors. They return a value and an
  optional **branch label**; the engine resolves the edge. This is
  load-bearing: even a hostile node implementation cannot redirect
  the graph somewhere the workflow didn't declare.
- `llm_infer` is a node like any other. Its output lands in the
  context under its node id; a downstream `switch` may route on it.
  The model never sees the graph and has no API to alter it.
- Termination is structural, not behavioural: the validator proves
  acyclicity (Kahn) before the engine accepts the document, the
  per-run deadline bounds wall-clock, and `MAX_STEPS` (10 000)
  backstops both against engine bugs.

## 3. Why not a model-owned loop

We prototyped the obvious alternative — a `plan_act` module where the
model proposed the next action each iteration — and discarded it.
The observed failure modes match what the literature reports:

- **Non-termination.** Without reliable self-evaluation the loop
  repeats the same thought/action pair indefinitely. Step caps turn
  this from a hang into a cost ceiling, but the run is still wasted.
- **Unauditable surface.** "What can this process do?" becomes a
  function of model behaviour. Reviews and threat models need an
  enumerable answer.
- **Injection becomes control-flow corruption.** When tool output
  feeds the planner, a hostile document can steer the *program*, not
  just a value. With a frozen graph, the blast radius of injected
  text is the value-space of one node's output — and the `when`
  labels it can select among were declared by the author.
- **Cost is unbounded by construction.** Replanning loops degenerate
  into uncontrolled token spend exactly when they're confused, which
  is exactly when you least want them spending.

## 4. What we accept in exchange

A frozen graph is brittle under unanticipated branching: if reality
produces a case the author didn't model, the run takes a declared
fallback (`fail`, dead-end completion) instead of inventing recovery.
We consider this the correct trade for the runtime's target domain —
repeatable, policy-bound automation — and say so in user-facing docs
rather than pretending otherwise.

## 5. Bounded relaxations

Both preserve the invariants and are explicitly
*declared-by-the-author*, never model-initiated:

- **Declared bounded cycles** (shipped). An edge annotated
  `max_iterations = N` is a loop edge: the validator admits exactly the
  cycle it forms (the acyclicity check is "acyclic modulo loop edges"),
  and the engine follows it at most N times per run, tracked per edge.
  This enables evaluator–optimizer patterns (generate → evaluate →
  retry) with a hard cap, without an open-ended agent loop. See
  `examples/evaluator-optimizer.toml`.
- **Parallel fan-out / fan-in** (RFC 0001 §9.1, future). Changes
  scheduling, not authority: the set of reachable actions is still the
  declared graph.

A model-owned inner loop ("agentic sub-node") is the only relaxation
that would weaken §2's guarantees; if it ever lands it will be a
separate RFC with its own budget, tool subset, and step cap, and the
outer DAG remains the authority over everything it may touch.

## 6. Consequences

- The execution trace is a faithful, replayable record: same graph,
  same inputs, same path — modulo the contents of `llm_infer`
  outputs, which are recorded.
- Engine code stays small enough to audit (one file owns traversal).
- Benchmarkable: throughput and latency are properties of the graph,
  not of model mood.
