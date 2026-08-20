<!-- Generated 2026-08-20 by a 16-agent analysis workflow: research → per-dimension
comparison → adversarial refutation of every claimed gap → synthesis.
59 candidate gaps, 56 survived refutation. Items marked "▶ ran it" were confirmed
by executing a real workflow against the binary, not by reading code.
Supersedes notes-langgraph-parity.md, which describes a pre-2.0 design. -->

# agentd workflows vs LangGraph / LangChain — final feature-parity report

Repo state: `/root/agentd-dev/source-code` @ `3c5ce1f`. Crate version 2.2.0; newest tag `v2.2.0` (`5e6ae05`). All file:line references are relative to that root.

---

## 1. Headline

**agentd already wins** on everything that happens *before* the first effect and *after* a crash. Static graph validation is a property LangGraph structurally cannot have — Kahn acyclicity, reachability from a start node, mandatory `finish`, dependency existence, unreachable-root refusal, per-kind field whitelist, CEL compilation, and a live registry cross-check that exits 2 if a `tool` step names an ungranted tool (`crates/agentd/src/engine/model.rs:1367-1435`). LangGraph's routing lives in opaque Python callables; its own docs concede END-reachability is not checked and the only universal backstop is `recursion_limit`. agentd's bad workflow dies at boot; LangGraph's dies at superstep 400 of real spend. Durability is the same story: checkpoint-before-effect is the *only* mode (`runtime/steps.rs:664-666`) against LangGraph's `durability="async"` default that "carries a small risk that LangGraph does not write checkpoints if the process crashes" — and agentd's is a *tested* contract, with SIGKILL kill-points and a three-life chaos check in `crates/agentd-conformance/src/checks/durability.rs`. Suspension resumes without replaying the node body, which deletes the entire class of author-discipline rules LangGraph's `interrupt()` forces (no bare try/except, strictly index-matched resume, idempotent pre-interrupt effects).

**At parity, differently shaped:** control-flow expressiveness. agentd has primitives LangGraph has no answer for (`race` with `min_success`, quorum `join` with `min`/`partials`, `batch` with `by`/`rate`, model-driven `route`/`classify`/`judge`, nine trigger-shaped start kinds that are *part of the graph*). LangGraph has `Command(goto=…)` and `Send`, which agentd deliberately does not — and mostly does not need.

**agentd is behind** on state *modeling* and the *developer inner loop*: no declared state schema with per-key reducers, no conflict detection on concurrent writes, no checkpoint history (therefore no time travel, replay-from-point, or fork), no operator state edit, no step-level breakpoint, no per-step live stream, and no retention for terminal runs. It also carries **one verified correctness defect** — the downstream of a not-taken `switch` branch still executes — which LangGraph's message-passing model makes structurally impossible.

**And one packaging fact dominates all of it:** CEL is on `main` but in no released tag. `git tag --contains 55845be` is empty; `git show v2.2.0:.github/workflows/release.yml` has no `cel` in `FEATURES`. Anyone who installs agentd today gets a binary where `when` guards, `iterate.while/until`, `assert.condition`, `wait.condition`, trigger `filter`s and `map`/`filter`/`reduce` `expr` are refused at load. That is a categorically different product than the tree this report analyses.

---

## 2. Dimension comparison

| Dimension | Verdict | agentd is ahead on | agentd is behind on |
|---|---|---|---|
| **Control flow & graph expressiveness** | Parity, different shape, **+1 correctness bug** | Static termination guarantees; 9 trigger start kinds as graph nodes; `race`/`join`/`batch`/`route`/`classify`/`judge`; durable suspension with no node replay; positional fan-in; deterministic inline order (`BTreeMap` topo) | Dynamic routing (deliberate); fan-out throughput (sequential default, hard cap 8); concurrent-write conflicts; **untaken branches still run their tails** |
| **State model & durability** | **Split — clearly ahead on durability, clearly behind on state modeling** | Checkpoint-before-effect as the only mode + SIGKILL conformance suite; write-ahead inbox ("accept means durable"); seq-CAS split-brain fatal, not LWW; definition-hash pinning → `Refused`; per-workflow `concurrency.on_overflow` in OSS | No state schema/reducers; no conflict detection; **no checkpoint history at all** → no time travel/fork/state edit; no retention or GC; `on_replay` and `store.durability` parsed but dead |
| **Human-in-the-loop & steering** | Mixed — ahead on the gate, behind on everything downstream | Wire-standard A2A gate any client can answer; approver identity + audit + `via: human\|auto`; deadline + `ask_human_fallback: fail\|wait\|auto`; MCP `elicitation/create` bridge; multi-client convergence by broadcast; two-scope reversible pause | Answer is untyped free text (`schema` inert); one live gate per run; no state edit; no breakpoint; no fork-and-re-answer; no runtime-enforced approval on a tool call |
| **Runtime, observability & deployment** ⚠️ | Parity, sharply lopsided | Fail-closed boot validation; `run_id`/`agent_path`/`pid` tree reassembly with no backend join; W3C trace-context into MCP/LLM/subagent hops at zero dependency cost; steering/HITL/cron/pause-resume in the OSS binary where LangGraph charges for the Platform | Graph visualisation from the binary; per-step live stream; time travel/replay/fork; deterministic offline test mode; hosted serving; versioned assistants; evaluation tooling |

⚠️ **Evidence caveat on dimension 4.** The per-gap adversarial verification for the runtime/observability dimension was not returned — I have its verdict and a partial advantages list only. Its "behind on" column is therefore **verdict-level assertion, not verified fact**, except where a specific item is independently confirmed elsewhere in this report (no per-step stream → §3.12; no time travel → §3.20). Treat the rest as a lead to check, not a finding. Do not put unverified dimension-4 items on a roadmap without running them down first.

---

## 3. Real gaps, ranked

Score = (impact × philosophy-fit) / effort, with impact 3/2/1, fit 1.0 for aligned and 0.4 for "would cost a stated principle", effort 1/2/4 for small/medium/large. The formula is a sorting aid, not an oracle — item 0 outranks everything and the formula does not say so, because a wrong answer is not a missing feature.

Items marked **▶ ran it** were confirmed by executing a real workflow on `target/debug/agentd`, not by reading code.

---

### 0. An untaken branch's downstream still executes — ▶ ran it
**impact high · effort medium · fit yes · score 1.5 (understated)**

**LangGraph:** routing is message passing on `branch:to:{node}` channels. A conditional edge that does not name a target writes nothing, so the untaken node never fires — and transitively nothing downstream of it fires. Pruning is structural and free.

**agentd today:** `StepStatus::is_satisfied()` is `Done | Skipped` (`engine/run.rs:46`); `schedule()` admits a step when every dep `is_satisfied` (`engine/run.rs:437-441`). `switch` marks the non-chosen case *targets* `Skipped` (`runtime/steps.rs:1035-1049`) — only the targets, never their descendants. `grep` for `prune|descend|transitive` across `src/engine` and `src/runtime` returns nothing relevant. Running `pick(switch on "left") -> {la, ra}; la->la2; ra->ra2`: the log shows `step.start kind=emit step=ra2` at 02:38:43.660, *before* `la` at 02:38:43.664. The pruned branch's tail ran first. A false `when` guard is identical (`engine/run.rs:465`, unit test at `:598-606`). `docs/node-registry.md:74` says only "the others are skipped" and never warns about their descendants.

**Closing it:** a third status distinguishing *pruned — do not run my dependents* from *skipped but joinable*. The skip-satisfies rule is exactly what makes uneven-branch joins work without LangGraph's `defer=True`, so it must survive: a dependent runs when *some* dep was pruned but another live path reached it, and is pruned when *all* inbound paths are pruned. That is reachability over the live subgraph, computable inside the existing fixpoint loop in `schedule()`.

**Should it be closed?** Yes, first. Any workflow with more than one step per branch silently executes both tails — with `http`, `mcp.tool` and `agent` effects — unless the author hand-guards every descendant with `when: steps.<dep>.status == "done"`, which itself requires CEL, which is not in a released binary. This is the one place agentd is not different from LangGraph but wrong.

---

### 1. Map-reduce fan-out is sequential by default and silently clamped at 8 — ▶ ran it
**impact high · effort small · fit yes · score 3.0**

**LangGraph:** `Send` creates one PUSH task per element; the whole set runs in one superstep with no built-in ceiling. Concurrency is bounded only if the caller sets `max_concurrency` in the `RunnableConfig`. A 200-way fan-out costs one superstep.

**agentd today:** `foreach` defaults to `parallel: 1`, `batch` to `size: 10, parallel: 1`, and the value is `.clamp(1, MAX_BATCH_PARALLEL)` where `MAX_BATCH_PARALLEL = 8` (`runtime/nested.rs:235-241`, `engine/model.rs:20`). Measured: `foreach` over 4 items of `sleep 1s` = 4.05s with defaults; 16 items with `parallel: 50` = 2.12s (i.e. 8 at a time). `validate_graph` never reads the field — `parallel: 50` passes `--validate-config` with `{"event":"config.valid"}` and exit 0, then gets clamped at run time. There is no config path: `grep -rn 'fan_out|fanout' crates/agentd/src docs/configuration.md` is empty. Independent top-level DAG branches are *not* capped — 12 concurrent `sleep 2s` steps finished in 2.08s.

**Closing it:** raise/relocate the default, make the cap a config path (`limits.workflow.fan_out`), and make an over-cap request a load-time error instead of a silent clamp.

**Should it be closed?** Yes. "Classify 500 tickets" is the single most common agentic fan-out, and agentd runs it one at a time unless the author knows to opt in, then refuses to exceed 8 without saying so. Bounding resource use on a single-binary appliance is legitimate; hiding the bound is the exact class of trap the parser's field whitelist exists to prevent.

---

### 2. CEL is in no released binary — ▶ ran it
**impact high · effort small · fit yes · score 3.0**

**LangGraph:** branch predicates are Python; there is no build flag between the author and a conditional edge.

**agentd today:** `git tag --contains 55845be` is empty. `git show v2.2.0:.github/workflows/release.yml | grep FEATURES` → `"a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth"`. Only `main` carries `,cel` (`.github/workflows/release.yml:28`). On the CEL-less build, `when: "CEL: 1 == 2"` is refused at load: `agentd: workflow "celtest" step "a": when: CEL expressions require the 'cel' build feature`, exit 2.

**Closing it:** cut 2.3.0. The work is done and on `main`.

**Should it be closed?** Yes, and pair it with item 0 in the same release, because the documented workaround for untaken-branch pruning *is* a CEL guard. **Scope correction:** the claim that "conditional routing does not work at all" is overstated — `switch` needs no CEL (`on` is a plain template, `cases` is RAW at `model.rs:1467`), and all branch-routing tests ran fine on the CEL-less binary. What a released binary loses is the CEL-gated *predicates*. Also, `docs/node-registry.md:44-47` is **not** out of sync: it says CEL ships "in the release binaries from 2.3.0" — accurate, and it explicitly warns an older binary exits 2.

---

### 3. Concurrent writes to the same var are silently last-write-wins — ▶ ran it
**impact high · effort small · fit yes · score 3.0**

**LangGraph:** two writes to a non-reduced key in one superstep raise `InvalidUpdateError` ("Can receive only one value per step. Use an Annotated key to handle multiple values."). It refuses to guess.

**agentd today:** `RunState::write_var(key, value, mode)` (`engine/run.rs:321-361`) takes the mode from the *writing* step; the catch-all arm `(_, _) => value` overwrites for any unrecognised mode. `validate_graph` (`model.rs:1367-1435`) checks six things and never reads the `writes:` field. `grep -rn 'conflict|InvalidUpdate|already written|concurrent' engine/run.rs engine/model.rs` returns nothing. Live: `a: {kind: assign, depends_on: [go], writes: acc, mode: overwrite}` and `b: {kind: assign, depends_on: [go], writes: acc, mode: append}` — no ordering path between them — passed `--validate-config` clean and ran to completion with `acc: [1,2]`, no warning, exit 0.

**Closing it:** agentd already computes `topo_order()` at parse time, so "these two steps can be in the same wave and both write `acc` with disagreeing modes" is a *static* query. Refuse the document at load.

**Should it be closed?** Yes, and it is strictly better than LangGraph's answer, because it fires before the run starts rather than at superstep N. Without item 12 (declared reducers) the check keys off mode agreement rather than a declared policy — weaker, still worth having, and buildable this week.

---

### 4. The human answer is untyped free text; `human.schema` is inert
**impact high · effort small · fit yes · score 3.0**

**LangGraph:** `Command(resume=<any JSON>)` makes an arbitrary value the return of the `interrupt()` call, so an approval gate hands back `{"approved": false, "reason": "…"}` and downstream code branches on it.

**agentd today:** `human` declares fields `["question","schema","to","timeout","reply_uri"]` (`engine/model.rs:566`, confirmed in `--workflow-schema`). `runtime/waits.rs:196-207` forwards `schema/to/timeout` into `ask_human`, and `ask_human_tool` (`runtime/human.rs:39-115`) reads only `question` (truncated at 2000) and `timeout` — `grep -n schema runtime/human.rs` returns zero hits. The answer is the raw `SendMessage` text as `Value::String` (`human.rs:295-300`), asserted by `crates/agentd-cli/tests/hitl_e2e.rs:315-318` (`steps.gate.output == "yes, ship it"`). Parse-time `check_schema` runs only for `think|agent` (`model.rs:1249-1254`) and `validate` (`:1256-1261`) — no `human` arm. Consequence: `output_schema: {type: object}` on a `human` step *always* fails (`steps.rs:1707`), and the approval example at `docs/workflows.md:441` is decorative.

**Closing it:** when `schema` is present, parse a JSON reply, validate with the existing `crate::jsonschema`, re-ask or fail on mismatch. `mcp/elicit.rs:110+ shape_reply` already does exactly this coercion on the elicitation return path and can be reused.

**Should it be closed?** Yes. An author who writes the documented schema gets silent non-enforcement — the opposite of agentd's stance everywhere else. **Scope correction:** the downstream branch *is* expressible today via `human` → `parse {text, format}` → `validate {value, schema}` → `switch`, two extra nodes. What is absent is enforcement on the node itself.

---

### 5. `outputs.schema` is declared, documented, and never enforced — ▶ ran it
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** `output_schema` on a `StateGraph` actually filters the run output; `response_format` on `create_agent` enforces structure with provider-native constrained decoding and a validation retry loop.

**agentd today:** `grep -rn outputs_schema crates/agentd/src` returns four sites — field (`model.rs:801`), parse with `jsonschema::check_schema` well-formedness only (`:943-949`), construction (`:1023`), and `None` for a synthesized subgraph (`nested.rs:1160`). The `finish` arm (`steps.rs:843-870`) reads `spec["output"]` and calls `run.finish(...)` with no validation. Live: a workflow declaring `required: [must_be_here]` whose `finish.output` was `{wrong_key: 1, …}` ran to `"status":"completed"`, exited 0, and printed output without the required key.

**Closing it:** call the same `jsonschema::validate` already wired for per-step `output_schema` in `finish_step`, at the `finish` step, against the run output.

**Should it be closed?** Yes — handful of lines. agentd's own docs already name it as "false assurance."

---

### 6. `on_replay: retry|skip|fail` is published in the JSON Schema and read by nothing
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** solves the same problem structurally — `@task`-wrapped work is memoized in the checkpointer, so resume "restores completed task and subgraph results instead of recomputing them." A started-but-unfinished task may re-run, and LangGraph says so, but the author has a mechanism.

**agentd today:** `grep -rn 'on_replay\|OnReplay' crates/agentd/src` returns five sites, **all in `engine/model.rs`** (`:582`, `:711`, `:1179-1185` parse+range-check, `:1293`, `:1533` published schema). Zero runtime sites. Restore hardwires one policy at `runtime/mod.rs:441-457`: any `Running` step → `Pending`, re-execute. `--workflow-schema` emits `"on_replay": {"enum": ["retry","skip","fail"]}`. Worse: the idempotency key is `(instance, run, step, attempt)` and `begin_step` increments `attempt`, so the replay reaches the MCP server under a *new* key and the callee cannot dedupe it either. `docs/workflows.md:220,500-501` concedes both.

**Closing it:** honour `skip`/`fail` in the restore loop (a dozen lines), and/or offer an attempt-invariant idempotency key.

**Should it be closed?** Yes — a knob in the published schema that does nothing is worse than no knob. A builder can author `on_replay: fail` on a payment step, pass validation, and get a silent double-charge. The attempt-invariant key is the more valuable half and matches RFC 0025's exactly-once ambition.

---

### 7. A failed dependency is diagnosed as a stall on the wrong step — ▶ ran it
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** an unretried exception propagates with the failing task named ("During task with name '<n>' and id '<id>'"), and `StateSnapshot.tasks[].error` carries it per task.

**agentd today:** `schedule()` skips a step with a failed dep and marks nothing (`engine/run.rs:443-457`); with nothing in flight the run finishes `Stalled` with the literal string `"no ready step and no finish reached"` (`runtime/steps.rs:549-556`), and `run.stalled` logs only `{"run": run_id}`. Reproduced with `bad` (validate) → `on_error: "goto:fix"`, `after` depends on `bad`, `done` depends on `after`: `run.done status=stalled err='no ready step and no finish reached'`. Neither the failed step nor the blocked one is named. `docs/workflows.md:578-580` documents rather than fixes it.

**Closing it:** walk the blocked steps' `depends_on` in the stall path and name the first failed ancestor. Pure diagnostics, no semantic risk.

**Should it be closed?** Yes. **Scope correction:** narrower than the headline. With the default `on_error: fail`, `route_failure` returns `Err` and the run fails *with the step named* — measured: `run.done status=failed err='step "bad" failed: validation failed: /: expected type string, got number'`. The undiagnosed stall only occurs once the failure was routed away (`on_error: goto`, or `continue` on a peer).

---

### 8. Retry has no error classification and no jitter
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** `RetryPolicy(initial_interval, backoff_factor, max_interval, max_attempts, jitter=True, retry_on=default_retry_on)`. `default_retry_on` is a deny-list — retry `ConnectionError` and 5xx, never `ValueError`/`TypeError`/`KeyError`/`RuntimeError`/`OSError`. A sequence of policies is allowed, first match wins; `set_node_defaults(retry_policy=…)` applies graph-wide; `run_with_retry` clears collected writes at the start of each attempt.

**agentd today:** `struct Retry { max: u32, backoff_ms: u64 }` (`engine/model.rs:642-648`); the published schema exposes only `{max, backoff}` (`:1529`). On `Failed | Timeout`: `backoff_ms.saturating_mul(1u64 << (attempt-1).min(10))` — no randomness (`runtime/steps.rs:1729-1740`). Deterministic failures are included: `steps.rs:1707-1721` rewrites a step to `Failed` on an `output_schema` mismatch immediately *before* the retry branch, so a schema mismatch retries `max` times. `struct Workflow` (`model.rs:787-807`) has no `defaults` key, so `retry:` must be repeated per step.

**Closing it:** a `retry_on: transient | any` discriminator over error classes the runtime already distinguishes (timeout / validation / transport), plus jitter, plus a workflow-level `defaults: {retry: …}`.

**Should it be closed?** Yes. Without jitter a fan-out of 8 retrying steps re-fires in lockstep against the endpoint that just rate-limited them. Nothing here touches the dependency budget.

---

### 9. A workflow run has no default step budget or wall-clock deadline
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** `recursion_limit` is always in force — `PregelLoop` sets `stop = step + recursion_limit + 1` and raises `GraphRecursionError` naming the config key to raise. Env-overridable, applies to subgraphs, and `RemainingSteps` lets a node degrade instead of throwing.

**agentd today:** `drive_run` guards both caps behind `if let Some(cap) = wf.limits.steps` / `.tokens` (`runtime/steps.rs:483-503`), and `RunState::new` sets `deadline_ms: wf.limits.deadline_ms.map(…)` (`engine/run.rs:220`), so an undeclared deadline is `None` and `deadline_passed()` is never true (`:524-526`). `grep -rn 'settings.limits.run' crates/agentd/src` hits only the agent-turn path (`steps.rs:1392,1396`), `subagents.rs:203,207,213`, `turns.rs:317-319`, `tools.rs:811` — never `drive_run`. So the "Per-run" defaults documented at `docs/configuration.md:228-230` (steps 500, tokens 2000000, deadline 3600s) **do not reach workflow runs** — a docs-vs-code discrepancy in its own right.

**Closing it:** default a run's step/token/deadline budget from `limits.run.*` when the definition is silent.

**Should it be closed?** Yes. Static acyclicity is stronger than anything LangGraph has, but it does not cover the two sanctioned backward edges (`switch` and `on_error: goto` can force an already-`Done` target), and a workflow that omits `limits:` has literally no budget. A few lines; converts "spins until an operator notices" into a named terminal state.

---

### 10. `human.to` and `human.reply_uri` are accepted and ignored
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** has no approver-routing concept at all — its documented absence is "no built-in authorization or approver identity model in the interrupt mechanism." This is agentd's own surface not being honoured, not a LangGraph feature.

**agentd today:** both fields are in the registry (`model.rs:566`, `--workflow-schema`). `waits.rs:200` forwards only `["schema","to","timeout"]` — `reply_uri` is not even passed. Neither `ask_human_tool` (`human.rs:39-115`) nor `human_gate` (`:118-221`) reads `to`; the task comes from `caller.node`/`caller.run` (`:126-147`) and a standalone gate's principal from `caller.principal` with a hardcoded `"operator"` default (`:167-178`). `grep -rn reply_uri crates/` returns exactly one source hit — the field list. The unhonoured promises are `registry/internal.rs:543` ("Principal or conversation to ask") and `docs/node-registry.md:156` ("`to` targets a channel").

**Closing it:** cheapest honest fix is to reject the fields at parse time until wired; the better one binds the gate task's principal to `to` so the existing owner check enforces it.

**Should it be closed?** Yes. `to: security-team` validates cleanly and then asks whoever happens to be attached — a silent authorization failure in the one surface agentd otherwise polices carefully.

---

### 11. Long-term memory has no namespaces, filters, or per-principal scoping
**impact medium · effort small · fit yes · score 2.0**

**LangGraph:** `BaseStore` items live under arbitrary-depth namespace *tuples* (`(org_id, user_id, "memories")`) with prefix matching, plus `search(namespace_prefix, query=, filter=, limit, offset)` and `list_namespaces(prefix=, suffix=, max_depth=)`. Multi-tenant scoping is free.

**agentd today:** one flat key space per instance under `<prefix>/<instance>/memory/<key>` (`context/memory.rs:3-4`). `set(d, key, value, ttl_ms, by)` stores `by` as metadata and nothing more; `get(d, key)` (`:139-153`) takes a bare key with **no** principal or scope argument and no ownership check; `list(d, prefix, limit)` is `k.starts_with(prefix)`. Dispatch confirms nothing is injected — `runtime/tools.rs:350-391` passes `args["key"]` straight through. `--workflow-schema` shows `memory.set ['key','value','ttl']`, `memory.get ['key']`, `memory.list ['prefix','limit']`, `memory.delete ['key']`.

**Closing it:** a structured namespace (`memory.set {namespace: [principal, kind], key, value}`) enforced by the runtime rather than by the author's string discipline.

**Should it be closed? Half of it.** The semantic-search half is correctly *not* agentd's job — routing `knowledge.search` to an MCP server is the MCP-only rule working. The *scoping* half is a security property, not a retrieval feature: a multi-tenant agent today encodes the tenant into the key by hand, with nothing enforcing it, and one leaked prefix in a prompt reaches another tenant's memories. agentd already has principals on every A2A request and a `by:` field on every record. agentd's TTL is already ahead of the LangGraph OSS store.

---

### 12. No declared state schema with per-key reducers — ▶ ran it
**impact high · effort medium · fit yes · score 1.5**

**LangGraph:** the state schema *is* the contract. Every top-level key is an independently versioned channel and its merge policy is declared once via `Annotated[list, operator.add]` / `add_messages`. Every writer — node, parallel branch, subgraph, `update_state`, time-travel fork — merges identically because the policy lives on the channel, not the write site.

**agentd today:** `RunState.vars` is an untyped `Map<String, Value>` (`engine/run.rs:139`). Merge policy is per write site: `assign`/`transform` carry `mode:`, `foreach.collect` carries its own `{into, mode}`. `struct Workflow` (`model.rs:787-816`) has `inputs_schema` and `outputs_schema` only — no var typing. `grep -rn reducer crates/agentd/src` hits only a doc comment on `write_var`. A `state:` block is refused today: `{"event":"config.invalid","msg":"unknown workflow field \"state\""}` — which is fail-closed reservation working as designed.

**Closing it:** a `state: {key: {type, reducer}}` block validated at parse time. That also makes item 3's check declarative instead of mode-agreement heuristics.

**Should it be closed?** Yes, but after item 3. LangGraph's insight — merge policy belongs to the key, not the writer — is what makes fan-out safe by construction, and agentd already refuses unknown fields, validates `assign.mode`, compiles CEL and enforces per-step `output_schema` at parse time. This is the same discipline one level up. **Correction:** the supporting claim that `vars.typo` resolves to null is **false** — a missing template path with no default is a hard error (`engine/template.rs:98`, doc at `:10-12`, test at `:211-215`).

---

### 13. No retention or GC for terminal runs — ▶ ran it
**impact high · effort medium · fit yes · score 1.5**

**LangGraph:** also weak — no TTL, no keep-last-N, no automatic pruning in the core library — but it exposes `checkpointer.delete_thread(thread_id)` as an operator primitive.

**agentd today:** `grep -rn 'Kind::Run' crates/agentd/src` shows only put (`reactor.rs:948`, `steps.rs:342`) and restore (`mod.rs:438`) — never delete. At `runtime/mod.rs:438-462` the `if !r.status.is_terminal()` guard gates only the replay reset; `rt.runs.insert(...)` is **unconditional**, so completed runs are adopted into memory on every restart. `limits.max_runs` bounds live runs only (`steps.rs:258-270`). There is no `store.retention` in `config/v2/schema.rs` or `--help`. Live: a `loop` start with `interval: 1s, max_iterations: 6` on the file store left 6 permanent run files under `st2/agentd/t2/run/` and 6 permanent manifest `entities` rows, all `completed`, with no configured way to reclaim them.

**Closing it:** `store.retention: {runs: {keep_last, ttl}}`, an eviction pass, manifest index cleanup, and a cap on in-memory adoption at restore. `Durable::delete` and tombstones already exist.

**Should it be closed?** Yes. This is the operational gap that bites a genuinely long-lived daemon, which is agentd's whole identity: a laptop instance with an hourly `schedule` accumulates records forever. It also **blocks item 20** — adding checkpoint history without retention makes the growth problem strictly worse.

---

### 14. No runtime-enforced approval gate on a tool call
**impact high · effort medium · fit yes · score 1.5**

**LangGraph:** `HumanInTheLoopMiddleware(interrupt_on={"write_file": True})` intercepts the *proposed* tool call before execution and suspends; the human approves, edits the arguments, or rejects, and the runtime enforces the decision.

**agentd today:** nothing equivalent. A gate exists only if the model chooses to call `ask_human`, or an author places a `human` step. `dispatch_tool` (`agentloop/runner.rs:588-601`) calls `servers[i].call_tool(name, args)` with no gate. The trifecta/Rule-of-Two check is spawn-time grant evaluation (`sec/scope.rs:14`, `:129-142`), not per call. `grep -rni approv crates/agentd/src` returns only the AAuth Person-Server consent flow and the auto-judge prompt. There is no config key (`config/v2/mod.rs:183-191` carries only `ask_human_fallback`). The protocol even has a vestigial `AgentMsg::Gate`/`GateClosed` pair (`subagent/protocol.rs:138,142`) that the reactor discards into an empty arm (`runtime/reactor.rs:631-632`), recorded as such in `docs/design/03-tui-thin-client.md:176`. `grep -rni approve interface/` returns nothing.

**Closing it:** the plumbing exists — `AgentMsg::ToolRequest` → supervisor → `ControlMsg::ToolResult` is already the internal-tool round trip, and `mcp/elicit.rs` proves a child-side handler can block on a supervisor gate on the MCP event thread without stalling the turn. Effort is medium, not large. It moves no execution into the supervisor and keeps tools MCP-only.

**Should it be closed?** Yes. `docs/coding-agent.md` says "the model's cooperation is not a control" — yet today approval *is* the model's cooperation. **Correction:** `tool_permitted` **is** consulted on every call (`runner.rs:400`, its own doc comment at `:221-224` calls it "the second gate"), not only at catalogue build. It is a static grant-pattern check, so the conclusion — no human decision per call — stands.

---

### 15. Only one human gate can be live per run
**impact high · effort medium · fit yes · score 1.5**

**LangGraph:** parallel branches may each call `interrupt()`; the runtime surfaces every pending `Interrupt` with an `id`, and `Command(resume={interrupt_id: value, …})` pairs each answer with its question. Per-item review of a map-reduce fan-out is a supported shape.

**agentd today:** a gate binds to the run's single A2A task (`human.rs:139-190`). A second ask on that task returns `ToolOutcome::Ready("ask_human: an ask is already pending on this task", true)` — `is_error` — which `waits.rs:206-217` turns into `StepStatus::Failed`. Nested bodies are steps of the *same* run under a scope (`runtime/nested.rs:1-13`), so both branches resolve to one task. `rebuild_human_asks` (`human.rs:527-540`) uses `find_map` and re-arms only the first suspended `human` step after a restart. Nothing refuses it at parse time — `parse_body` (`model.rs:1318`) applies no kind allow-list and the sanity match (`:1194-1270`) has no `human` arm.

**Closing it:** give a gate its own identity (a gate task per suspended step, or a gate id carried on the answer) instead of reusing the run's task, and re-arm all suspended human steps on restore.

**Should it be closed?** Yes, but the honest interim is a **parse-time refusal** of `human` inside `parallel`/`foreach`/`batch`/`race`/`subgraph`, because today it fails at runtime rather than at validation. **Two corrections:** (1) RFC 0032 §16 states the constraint deliberately — "One live gate per task; asks within one unit are sequential" — so this is a documented stance whose premise happens to be false for inline concurrent bodies. (2) Concurrent gates **do** work across separate runs: a run with no task mints its own gate task (`human.rs:180-195`), so per-item review expressed with the `workflow` node (child runs) works today.

---

### 16. No operator state edit (`update_state` equivalent)
**impact high · effort medium · fit yes · score 1.5**

**LangGraph:** `graph.update_state(config, values={…}, as_node="node")` applies values through the same reducers a node write would use, writes a *forked* checkpoint (`source: "update"`, original history intact), and `as_node` chooses where execution resumes. It is the primary tool for correcting a run rather than restarting it.

**agentd today:** no such op. The full surface is `status, config, workflow.run|status|cancel|signal, subagent.send|kill|status, plan.get` plus interface reads and admin `drain/lameduck/pause/resume/cancel` (`runtime/mod.rs:790-815`, `a2a/principals.rs:110-149`). `config.set` is whitelisted to `interface.debug` and `interface.display.*` and rejects everything else (`a2a_server.rs:1250-1300`). `run.get` exposes `vars` read-only and only behind `interface.debug` (`:1390-1427`). Across the whole runtime the only external write into a run's `vars` is the webhook URL binding (`runtime/webhooks.rs:766`).

**Closing it:** gate on operator principal, re-validate against the pinned `workflow_hash`, route through the existing `write_var` merge path, audit it. The hard half is "resume from node X" semantics — which agentd's DAG makes *easier* than LangGraph's, since forcing a step already exists as `StepState.forced` for `switch` and `on_error: goto`.

**Should it be closed?** Yes, and the instinctive objection is weaker than it looks: agentd already has an operator principal, an audit stream, envelope versioning and definition pinning — exactly the four things that make a state edit safe and attributable. **Correction:** "nothing writes run vars or a step output" overstates the step-output half. Two scoped injection paths exist — `workflow.signal` sets a suspended `wait on: signal` step's output (`waits.rs:645-655`, dispatched at `a2a_server.rs:1043-1060`), and the `human` node resumes with a supplied answer (`steps.rs:1178-1184`). Both require the author to have placed a wait point; neither writes arbitrary vars nor chooses a resume node.

---

### 17. Node cache never memoizes the expensive steps, and caches the value not the effect — ▶ ran it
**impact medium · effort medium · fit yes · score 1.0**

**LangGraph:** `CachePolicy(key_func, ttl)` + `compile(cache=…)` caches the node's *writes*, not its return value, so a hit reproduces the full effect on state including routing writes. Works on any node and on `@task`; `set_node_defaults(cache_policy=…)` applies graph-wide. Backends: InMemory, SQLite, Redis.

**agentd today:** the cache itself is *better* than LangGraph's — an authored key template hashed with kind+id, stored under `_cache/<sha256>` in the durable store, so hits survive restart and cross runs (`runtime/waits.rs:1197-1240`), against LangGraph's `pickle.dumps`-of-input default that silently defeats itself on non-picklable or churning inputs. But the pending key is parked in `StepState.wait` (`steps.rs:704-712`), the field `suspend_step` (`engine/run.rs:298-302`) and nested progress (`nested.rs:308,686,847,940`) overwrite — so the write site (`steps.rs:1844-1857`) finds it gone. `docs/workflows.md:590-594` admits the excluded list. Separately, a hit calls `finish_step` with the stored output and returns *before* the kind dispatch (`steps.rs:694-701`): I ran a `switch` with `cache: {key: "fixed"}` twice against one durable store and run 2 logged `step.cache_hit step=pick` then executed **both** `ra` and `la` — the hit neither forced the chosen target nor skipped the others.

**Closing it:** a dedicated `StepState.cache_key` field instead of sharing `wait` (small); then decide whether a cached step replays its routing writes, which argues for caching an *outcome record* (status + output + forced targets) rather than a bare value.

**Should it be closed?** Yes — and a hit is precisely the case where there is no effect to checkpoint, so it is consistent with checkpoint-before-effect. **Correction:** `agent`/`think` steps are **not** excluded — they set `st.worker` (`steps.rs:1453`), not `st.wait`, so their cache writes land. Only `subagent` (`PendingKind::Subagent` → `suspend_step`, `steps.rs:1163-1184`) and the nesting/suspending kinds lose the key. That materially reduces the "where the money is" argument, though `subagent` and `foreach` bodies remain expensive.

---

### 18. No reusable named sub-DAG; `subgraph` bodies are inline and single-use — ▶ ran it
**impact medium · effort medium · fit yes · score 1.0**

**LangGraph:** a compiled graph *is* a node — `builder.add_node("x", subgraph)` reuses one compiled object in many places and parents, with per-invocation checkpoint namespaces (`parent|child:task_id`) keeping instances independent.

**agentd today:** `--workflow-schema` gives `subgraph {'fields': ['body'], 'required': ['body']}`; `model.rs:206` is `k("subgraph", false, &["body"], &["body"], true, true)`. `grep -rn '"ref"|bodies' engine/model.rs runtime/nested.rs` finds no reference mechanism, and `struct Workflow` has no section that could hold a named body. `MAX_NESTING = 4` (`model.rs:19`). The serialization workaround does not exist either — a config using `&shared`/`*shared` is rejected by agentd's own parser: `config file parse error (yaml): line 4, column 11: anchors, aliases and tags are not supported`. Copy-paste is the only path.

**Closing it:** a `bodies:` section with `subgraph: {ref: <name>}`, expanded at parse time before `validate_graph`. The scoped-id machinery (`sub.step`, `each[3].step`) already handles multiple instantiations; validation is unchanged; the hash stays honest; run-time cost is zero.

**Should it be closed?** Yes. Copy-paste is a correctness hazard in a system whose identity is a SHA-256 of the canonical definition — two copies drift and the hash cannot tell you they were meant to be the same. The *reuse-across-workflows* path (`workflow` node with `mode: sync|async|detached`, hash pinning, `cascade`) is genuinely better than a LangGraph subgraph — child run identity, own concurrency policy, pinned definition — it is just a heavier unit than "call these three steps again." The missing `Command.PARENT` (child→parent control transfer) is a defensible no; switching on the child's returned output expresses the safe subset.

---

### 19. `store.durability.{a2a,steps}: strict|eventual` is parsed, advertised, and dead — ▶ ran it
**impact medium · effort medium · fit yes · score 1.0**

**LangGraph:** `durability` is a per-*invocation* argument on `invoke`/`stream`: `"sync"`, `"async"` (default), `"exit"`. One graph runs cheap for a batch backfill and expensive for a run that touches money, and even `"exit"` flushes on a HITL pause.

**agentd today:** `grep -rn 'DurabilityLevel::' crates/agentd/src` returns **zero**. Defined at `config/v2/mod.rs:855-865`, schema'd at `config/v2/schema.rs:143`, and used only to be stringified into the config digest (`state/mod.rs:306`). `Policy::from_settings` (`state/mod.rs:196-222`) reads only `checkpoint.debounce_ms`, `on_error` and a hardcoded `retries: 3`. It is an advertised CLI flag (`--store-durability-steps <strict|eventual>`, `AGENTD_STORE_DURABILITY_STEPS`). Live: the same 5-element × 2-body-step `foreach` persisted at seq 32 with defaults **and** at seq 32 with `--store-durability-steps eventual --store-checkpoint-debounce-ms 5000` — identical write count.

**Closing it:** implement RFC 0025 §5 coalescing for step progress, or delete the config.

**Should it be closed?** Yes, with a hard line: `strict` stays the default and **only step progress may relax** — the inbox write-ahead and the pre-effect `Running` write are the guarantee and must never be dialable. RFC 0025 already drew that line. If it is not going to be built, delete the knob rather than ship one that lies.

---

### 20. Whole run record rewritten on every transition; scoped step entries never pruned — ▶ ran it
**impact medium · effort medium · fit yes · score 1.0**

**LangGraph:** has the same full-value default and admits it ("LangGraph checkpoints write the full value of every state channel at each super-step", with quadratic growth called out), then shipped `DeltaChannel` in 1.2 with periodic re-snapshotting bounded by `snapshot_frequency`. Note LangGraph's granularity is the *superstep*; agentd's is the *step*, so agentd writes more often.

**agentd today:** `checkpoint()` does `serde_json::to_value(&*run)` for every dirty run (`runtime/reactor.rs:940-955`). Scoped entries are created by `run.steps.entry(scoped_id(scope_id, id)).or_default()` (`nested.rs:406`, `scoped_id` at `:74-76`). `grep -rn 'steps.remove\|steps.retain\|.steps.clear' crates/agentd/src` returns **zero**. Live: a `foreach` over 5 items with 2 body steps left a terminal run record with 13 entries (`c`, `c[0].s1` … `c[4].s2`, `fin`, `go`) at **seq 32** — 32 full-record writes for 5 elements.

**Closing it:** cheaper than LangGraph's delta channels, because the outputs are *already duplicated* — `advance_foreach` copies each finished element's result into the progress record's positional `results` map, so the scoped `StepState` entries under a terminal element are redundant and can be dropped. No new store contract, no new channel type.

**Should it be closed?** Yes. This is O(N²) write amplification inside one run, landing exactly on the workload agentd markets workflows for. ⚠️ **Contested evidence — resolve this first.** The dimension-2 advantages list claims "automatic large-value offload out of the checkpoint … above `limits.inline_max_bytes` (64 KB default) … `runtime/steps.rs:1672-1704`," while the verification of this gap states `grep -rn 'offload' crates/agentd/src` is **empty** and the only cap is `runtime/artifacts.rs:13-14 MAX_INLINE_BYTES = 4 * 1024 * 1024`, a refusal ceiling on the explicit `artifact.create` node, not an automatic spill. These cannot both be true. If the verification is right, a large step output rides inside the run record on every rewrite with nothing capping it, and this gap is worse than rated. **Check `steps.rs:1672-1704` before acting.**

---

### 21. `collect.mode` is not validated at parse time — ▶ ran it
**impact low · effort small · fit yes · score 1.0**

**LangGraph:** a reducer is a Python callable attached to the channel, so a typo is a `NameError` at import.

**agentd today:** the kind-specific validation match (`model.rs:1194-1270`) has arms for `switch`, `finish`, `sleep`, `assert`, `think|agent`, `validate`, `assign|transform` (`:1263-1269` enforces `overwrite|append|merge|union`) — there is **no** `foreach`/`batch`/`iterate` arm. `apply_collect` reads `collect.get("mode")…unwrap_or("overwrite")` at run time (`nested.rs:1024-1034`) and `write_var` falls through `(_, _) => value` (`run.rs:355`). Live: `collect: {into: reviews, mode: appned}` passed `--validate-config` clean and ran with no warning.

**Should it be closed?** Yes — same one-line-per-kind typo shield agentd applies everywhere else. **Correction, and it downgrades the item:** the claimed consequence ("silently discards N-1 results") is **false**. `apply_collect` is called exactly once per nested step, on a terminal path, with the already-complete positional array from `collect_results(&results, total)` (`nested.rs:1133-1142`, call sites `:717, :762, :788, :855`). The typo'd run produced `reviews: ["el","el"]` — both elements present. The real consequence is narrower: an intended append/merge/union *across repeated executions* of the same nested step (an `iterate` inside a `foreach`, a re-entered `subgraph`) silently becomes overwrite.

---

### 22. No static interrupt / step-level breakpoint
**impact medium · effort small · fit yes · score 2.0** *(listed here for grouping with debug affordances)*

**LangGraph:** `compile(interrupt_before=["node_a"], interrupt_after=[…])`, or the same lists per invocation, stops at a named node boundary; inspect with `get_state`, edit with `update_state`, resume with `invoke(None, config)`. `All='*'` breaks on every node.

**agentd today:** `pause`/`a2a.pause` reads only `params["run"]` and `reason` and flips `RunStatus::Paused` (`a2a_server.rs:1800-1825`); the internal tool agrees (`registry/internal.rs:435-442`). The scheduler skip is whole-run (`steps.rs:417-423`). `checkpoint` as a node kind is a labelled **no-op** — it shares the `noop` arm at `steps.rs:713-720` and its `name` field is never read, despite `docs/node-registry.md:88` saying "Forces a durable checkpoint here."

**Closing it:** a `before_step`/`after_step` parameter on the existing pause op, consulted in `schedule`/`begin_step`, reusing the durable `Paused` status at `engine/run.rs:85`.

**Should it be closed?** Yes — it is the debugging affordance for a workflow you do not yet trust, and today the only option is to add a permanent `human` node to the definition. **Correction:** the supporting claim that editing a workflow makes an in-flight run `Refused` is too strong — `runtime/reload.rs:287-300` pins the old definition for every live run on SIGHUP and `definition_for_run` (`steps.rs:436-442`) falls back to it. `Refused` happens when the workflow is *gone*, or after a **process restart** (the `pinned` map at `reactor.rs:188-189` is in-memory only and not persisted — arguably a defect of its own). Separately, the `checkpoint`-is-a-noop finding should be fixed or the docs corrected regardless.

---

### 23. The observation feed carries no per-step detail
**impact medium · effort small · fit yes · score 2.0** *(grouped with debug affordances)*

**LangGraph:** `stream_mode="updates"` emits the exact keys each node changed, per node, per superstep; `"values"` the full snapshot; `"tasks"` start/finish with results and errors; `"checkpoints"` the snapshot. No debug flag.

**agentd today:** the `run` feed event is `RunState::summary()` (`a2a_server.rs:1462-1469`), and `progress()` (`engine/run.rs:363-390`) is a **count per status**. The complete set of feed kinds (`grep -rn 'feed_push(' crates/agentd/src`) is run, conversation, subagent, child, status, activity, activity.removed, message, task.removed, lifecycle, config — no `step`. `activity` is turn-scoped and `unit_of` (`activity.rs:180-196`) maps a `ChildKind::StepTurn { run, .. }` to the run's task, **discarding the step id**. Step transitions exist only as log-ring events `step.done`/`step.retry` (`steps.rs:1728,1737`), reachable via `debug.events` (interface.debug + operator gated). The ungated `workflow.status` view is thinner still — `run_view` (`a2a_server.rs:2104-2106`) drops the counts entirely.

**Should it be closed?** Yes. A client watching a workflow sees "3 done, 1 running, 1 pending" with no names, so a person cannot tell what to steer without flipping a debug switch. A `step` feed event on transitions, owner-scoped with the 2 KiB truncation `run.get` already applies, fits the existing feed model and needs no new surface.

---

### 24. A restored gate's deadline resets when the step declared no timeout
**impact low · effort small · fit yes · score 1.0**

**agentd today:** `wait_record` writes `deadline_ms` only inside `if let Some(t) = timeout_ms` (`waits.rs:25-36`), and the human step suspends with `wait_record("human", json!({}), step.timeout_ms)` (`:224-228`). `rebuild_human_asks` falls back to `now_ms() + ASK_TIMEOUT` with `ASK_TIMEOUT = 24h` (`human.rs:30, 534-538`) — a fresh 24 h clock on every restart. This contradicts `docs/node-registry.md:182-184` ("resumes waiting, with its deadline intact") and quietly undermines the `auto` fallback guarantee (`human.rs:404,427`). One-line fix: write the absolute `deadline_ms` into the wait record. **Correction:** the reset is confined to the no-timeout default — a `human` step with `timeout: 12h` sets both the ask deadline and the wait record (`model.rs:1067-1080, 1162-1171`).

---

### 25. Double-texting is hardwired to enqueue for conversation turns
**impact low · effort medium · fit yes · score 0.5**

**LangGraph (Platform):** four `multitask_strategy` values — `reject`, `enqueue`, `interrupt` (halt current run, keep progress, insert new input), `rollback` (cancel and discard).

**agentd today:** `dispatch_turns` (`runtime/turns.rs:152-183`) sets `ctx_busy` for a context with a live `RootTurn`/`Think` child and leaves the job in `turn_queue` — enqueue, always. `subagent.send` → `ControlMsg::Inject` works only for `warm` subagents and lands on the turn-input channel, i.e. consumed at the *next* turn boundary. **Correction:** agentd already ships a three-way version of this at another scope — `Concurrency { max_runs, on_overflow: Queue | Drop | Replace }` (`engine/model.rs:749-771`, applied in `on_start_event`, `steps.rs:267-300`), where queue≈enqueue, drop≈reject, replace≈rollback, and `workflow.run` over A2A routes through it (`a2a_server.rs:991`). The gap is **conversation turns only**, and only LangGraph's `interrupt` strategy has no analogue — which is the one LangGraph's own docs flag as hazardous (dangling tool calls corrupting message history). Low priority; the turn-boundary discipline is defensible.

---

### 26. No checkpoint history — no time travel, replay-from-point, or fork
**impact high · effort large · fit yes · score 0.75**

**LangGraph:** every superstep is an immutable, addressable checkpoint with a monotonic UUIDv6 `checkpoint_id` and a `parent_config` link, forming a history DAG. `get_state_history(config)` lists newest-first; `invoke(None, snapshot.config)` replays from any of them; `update_state(snapshot.config, values, as_node=…)` **forks** — it explicitly "does not roll back a thread." This is what makes "answer the human differently and re-run from there" and "try three trajectories from step 7" possible.

**agentd today:** the store keeps only the latest envelope per key. `Runtime::checkpoint` writes one key per run (`reactor.rs:940-956`) at `prefix/instance/run/<id>` (`store/mod.rs:89-91`), overwritten each time. `seq` is a per-key CAS generation (`state/mod.rs:503-524`), not a version: `FileStore::get(&self, key, _seq)` ignores it outright ("Latest-only, like the http adapter" — `store/file.rs:125-126`), and `Durable::get(kind, id)` has no seq parameter at all (`state/mod.rs:607-621`). Only the in-process `memory` test store keeps a `hist` BTreeMap (`store/memory.rs:121-134`). `--workflow-resume` is parsed and validated (`config/mod.rs:1182-1196, 1625-1645`) but has **zero consumers** outside `config/` — a dead flag in the 2.0 runtime.

**Should it be closed?** **Not yet, and say so out loud.** Nothing contradicts a stated principle — checkpoint-before-effect is compatible with append-only history — but making it work universally means either an append-only default file store (a real design change) or "time travel only on a history-keeping backend," an ugly capability cliff. It also requires retention (item 13) first, or it *is* the quadratic-growth problem LangGraph has. **Two useful nuances:** the `seq` argument is not dead end-to-end — `store/mcp.rs:219-220` and `store/http.rs:151-153` both forward it to the backend, so the plumbing survives further than the file-store comment implies; and the MCP/HTTP adapter contract already documents `get(key[, seq])` as "the pinned seq if the store keeps history," meaning the contract was designed for this and the runtime simply never exercises it. **Immediate action regardless: fix the docs.** `docs/design/notes-langgraph-parity.md:38` claims parity on "Time travel / fork ✅" — written against dialect 2 and false for the 2.0 engine.

---

### 27. A conversational (turn) gate does not survive a restart — **deliberate no**
**impact medium · effort large · fit NO · score 0.2**

`rebuild_human_asks` (`human.rs:519-546`) matches only `Link::Run { id }`; every other link falls to `_ => None`. The module doc states it at `human.rs:9-12` and `:513-518`, corroborated by RFC 0032 §16 (`rfcs/0032-…:293-298`) and `docs/experience.md:406-409`. Closing it means re-spawning a child and replaying its transcript to reach the pending tool call — which is precisely LangGraph's replay-the-node model, importing its idempotency hazards into agentd's clean no-replay resume. The child-holds-the-transcript process model is the architecture, not an accident. **The honest improvement is disclosure, not durability:** mark the gate task so a client can say "this question was lost to a restart" rather than quietly absorbing the answer as a new prompt. Small, and it removes the surprise.

---

### 28. No lease or claim for cross-instance failover — **published won't-do**
**impact medium · effort large · fit stated-boundary · score 0.2**

`grep -rn 'lease\|claim' crates/agentd/src` finds no mechanism — only removed-flag help text. `store/file.rs:294-316` takes an exclusive `flock(LOCK_EX|LOCK_NB)` for the store's life, so a second process fails at startup; a CAS `Conflict` is fatal and never retried (`state/mod.rs:558-585`). The `Store` trait (`store/mod.rs:150-163`) is `put/get/list/delete/kind` — no lease primitive.

**Against LangGraph OSS, agentd is ahead here and should say so** — failing fast beats interleaving silently, and the OSS checkpointer contract has no optimistic check at all. The gap is only against the hosted Agent Server. And it is a *stated boundary*: `docs/scaling.md` §4 — "There is no shard flag, no claim route, no standby pool, and no `cluster` build feature. A version of agentd carried declarations for all of them — parsed, validated, and read by nothing — and they were removed rather than finished. That is a deliberate boundary, not a gap waiting to be filled." §2c ships a complete `worker.yaml` calling a queue server's own `claim` MCP tool from a workflow step. So "a workflow cannot participate in a lease" is **false**; only "agentd holds no lease of its own" is true, and that is the published position. Do not put this on a roadmap ahead of items 13 and 26.

---

## 4. Claims that did not survive verification

Useful signal in both directions: things assumed missing that exist, and supporting claims that were wrong.

| Claim | Reality |
|---|---|
| **"Nothing can compute a routing target at run time"** | **Refuted, ▶ ran it.** `workflow` is not in `RAW_FIELDS` (`model.rs:1440-1469`), so `render_spec` (`run.rs:546-559`) interpolates `workflow.name` before `waits.rs:679` reads it. A parent with `assign → value: "child_b" → writes: target` and `call: {kind: workflow, name: "{{vars.target}}", mode: sync}` dispatched at run time — `run.start … workflow=child_b`. The handoff/supervisor/swarm pattern `Command(goto=…)` motivates **is expressible today**, at child-run granularity, with the child's own run identity, concurrency policy and pinned hash. What is genuinely absent is a runtime-computed *step id inside one graph* — a deliberate no, since it would gut acyclicity, reachability, `finish`-reachability, the boot-time registry cross-check, and the web editor's ability to draw the real topology. |
| **"`vars.typo` resolves to null"** | **False.** A missing template path with no default is a hard error — `engine/template.rs:98`, documented at `:10-12` ("the step takes its error edge rather than running with a silently-wrong shape"), tested at `:211-215`. |
| **"`agent`/`think` steps can't be cached"** | **False.** They set `st.worker` (`steps.rs:1453`), not `st.wait`, so their cache writes land. Only `subagent` and the nesting/suspending kinds lose the key — consistent with `docs/workflows.md:592-594`, which names `subagent` and not `agent`. |
| **"A failed dependency is never diagnosed"** | **Narrower.** With the default `on_error: fail` the run fails *with the step named*: `run.done status=failed err='step "bad" failed: validation failed: …'`. The undiagnosed stall requires the failure to have been routed away. |
| **"Conditional routing doesn't work at all without CEL"** | **Overstated.** `switch` needs no CEL — `on` is a template and `cases` is RAW (`model.rs:1467`). Only the CEL-gated predicates are lost. And `docs/node-registry.md:44-47` is accurate about 2.3.0, not out of sync. |
| **"A typo'd `collect.mode` discards N-1 results"** | **False.** `apply_collect` runs once, terminally, on the complete positional array. `mode: appned` produced `reviews: ["el","el"]` — both elements. Real consequence is limited to accumulation *across repeated executions* of the same nested step. |
| **"`tool_permitted` is only evaluated at catalogue build"** | **False.** It runs on every call (`agentloop/runner.rs:400`; own doc comment at `:221-224` calls it "the second gate"). It is a static allow-list, so the *no human decision per call* conclusion holds. |
| **"Editing a workflow makes an in-flight run `Refused`"** | **False for SIGHUP reload.** `runtime/reload.rs:287-300` pins the old definition per live run and `definition_for_run` falls back to it. `Refused` occurs when the workflow is gone or after a **process restart** — the pinned map (`reactor.rs:188-189`) is in-memory only. |
| **"Double-texting is hardwired"** | **Narrower.** `Concurrency { max_runs, on_overflow: Queue\|Drop\|Replace }` (`model.rs:749-771`, `steps.rs:267-300`) is the same taxonomy for *workflow-run admission*, reachable via `workflow.run`. Missing only for conversation turns. |
| **"Nothing writes run vars or a step output"** | **Narrower.** `workflow.signal` sets a suspended `wait on: signal` step's output (`waits.rs:645-655`), and `human` resumes with a supplied answer. Both need an author-placed wait point; neither writes arbitrary vars or chooses a resume node. |
| **"agentd has no lease story"** | **False as stated.** `docs/scaling.md` §2c ships a `worker.yaml` calling a queue server's own claim/lease MCP tools from a workflow step; §4 declares the absence of an agentd-held lease a deliberate boundary. |
| **"Automatic 64 KB artifact offload keeps large values out of the checkpoint"** | ⚠️ **Contested.** One pass cites `steps.rs:1672-1704` + `limits.inline_max_bytes`; the adversarial check found `grep -rn offload` empty and only `artifacts.rs:13-14 MAX_INLINE_BYTES = 4 MB` as a refusal ceiling on the explicit `artifact.create` node. **Resolve before citing this as an advantage.** |
| **`checkpoint` as a node kind** | Present in the registry and documented at `docs/node-registry.md:88` as "Forces a durable checkpoint here" — but dispatched as `"noop" \| "checkpoint" =>` (`steps.rs:713`). It is a documentation-only alias. Fix the code or the doc. |
| **`seq` plumbing** | Not dead end-to-end: `store/mcp.rs:219-220` and `store/http.rs:151-153` forward it. Only `FileStore` and the `Durable` façade drop it. History is closer to reachable than the file-store comment suggests. |

---

## 5. What agentd has that LangGraph/LangChain do not

Ordered by how hard they'd be for LangGraph to copy.

**Structurally impossible for LangGraph:**

- **Termination as a static property.** Kahn acyclicity + reachability from a start + mandatory `finish` + dependency existence + no self-dependency + no unreachable roots (`model.rs:1367-1435`). LangGraph's routing is opaque Python; `path_map`/`destinations` exist purely to recover a drawable graph, and END-reachability is explicitly unchecked. agentd fails at boot before any effect; LangGraph fails after 10007 supersteps of real spend.
- **Validation past the graph into the environment.** Load cross-checks the live registry — a `tool` step naming a tool not granted to workflows, or an `mcp.tool` naming a disconnected server, exits with the usage code (`docs/workflows.md:535-538`). Plus a per-kind field whitelist: `prompt:` on an `agent` step (which takes `instruction`) is a boot error naming the allowed fields, not a silent no-op. LangGraph's `Send` payload bypasses the state schema entirely.
- **Triggers are part of the graph.** Nine start kinds — `once`, `manual`, `loop`, `schedule`, `subscribe`, `signal`, `event`, `a2a`, `webhook` — with their own fields (cron/tz/jitter/catch_up, debounce/coalesce, webhook auth/idempotency/on_overflow), and sibling starts marked `Skipped` when one fires (`run.rs:185-196`). LangGraph has exactly one `START`; everything about how a run begins lives outside the graph in the Platform's cron and assistants.
- **Definition-hash pinning.** A run stores `workflow_hash` (SHA-256 over the canonical document) and a resume that no longer matches ends `Refused` rather than continuing against changed logic (`docs/workflows.md:505-511`); the `workflow` node can pin a child by hash prefix (`waits.rs:694-710`). LangGraph has no binding between a checkpoint and the code that produced it — redeploy and old threads silently replay new logic. Assistants version *configuration*, not that relationship.
- **A daemon can own concurrency policy.** `concurrency: {max_runs, on_overflow: queue|drop|replace}` in the document (`model.rs:757-772`) plus global `limits.max_runs`. LangGraph's docs say the equivalent can only exist hosted, "because in order to handle this we need to know how the graph is deployed." agentd *is* the deployment.

**Primitives with no LangGraph equivalent at all:**

- `race` with `min_success` and loser cancellation; `join` over async handles with `min` and `partials` (quorum and partial-result fan-in); `batch` with `by` grouping and `rate` pacing; `wait` on a polled CEL condition / signal / webhook / child run / conversation; `human` with a timeout *and* an auto-fallback judge. In LangGraph every one is hand-rolled asyncio inside a node, outside the checkpointed graph.
- Model-driven control flow as **node kinds** with declared fields and enforced output schemas: `route`, `classify`, `judge`, `extract`, `summarize`, `think` (with `output_schema`, `check`, `retries`). LangGraph's equivalent is `create_agent` middleware plus a `tools_condition` router you wire yourself.

**Durability and operations:**

- **Checkpoint-before-effect as the only mode**, with an instrumented `kill_point("step.running")` at the boundary (`steps.rs:664-666`), and a black-box **three-life SIGKILL conformance suite** (`crates/agentd-conformance/src/checks/durability.rs`) asserting `restore.done` with no progress lost or duplicated. LangGraph's default is `"async"` and it publishes no crash-conformance contract. agentd's weakest ordering is LangGraph's strongest.
- **"Accept means durable."** Inbound A2A messages, trigger firings and signals hit the durable inbox before acknowledgement (`runtime/reactor.rs:415-430`, single `accept_event` entry point), and `pending` records re-deliver in ts order on restore. LangGraph has no inbound write-ahead log.
- **Split-brain is fatal, not silent.** Every `put` is a CAS on `seq` (`state/mod.rs:558-585`); the file adapter takes an exclusive flock and a second instance fails at startup naming the holder's pid. The LangGraph OSS checkpointer has no optimistic check — two runs on one `thread_id` interleave silently.
- **Resume does not replay the node.** A suspended step completes with the answer as its output; the body never re-executes. LangGraph restarts the entire node, which is why its docs impose four author rules (no bare try/except around `interrupt()`, strictly index-matched resume ordering, no `while True` validation loops, idempotent pre-interrupt effects). None of those rules — and none of the bugs they exist to prevent — exist in agentd.
- **Deterministic positional fan-in.** `collect_results` builds a dense array indexed by element with `null` in failed slots (`nested.rs:1133-1139`), so a 100-way map preserves input order and tells you which element produced nothing. LangGraph's canonical fan-in is `operator.add` — completion order, correspondence lost.
- **Deterministic inline order.** `topo_order()` walks a `BTreeMap` keyed by step id (`run.rs:417`, `model.rs:837-858`); LangGraph explicitly does not order actors within a superstep.
- **Store is a four-op adapter** (`put/get/list/delete`) over any MCP server, HTTP, filesystem or memory — no database client linked, envelope carries a major version and refuses an unknown major at restore. LangGraph needs `langgraph-checkpoint-postgres`/`-sqlite`/Redis.
- **Durable, cross-run, TTL'd step cache with an authored key** (`waits.rs:1197-1240`) vs LangGraph's `pickle.dumps`-of-input default that silently defeats itself, process-local unless you stand up Redis. (Caveat: item 17.)
- **Context compaction is built in and durable** — self-compacts at `context.compact_at × model_window` into a structured summary, keeps last N verbatim, preserves the plan, evicts unreferenced skill bodies, bumps a version, re-checkpoints; split into pure plan and pure apply so the runtime never calls the model (`context/compact.rs`). In LangGraph this is `Overwrite` plus a summarization middleware you own.
- `memory.list` reports `truncated: true` and sorts deterministically; the LangGraph Store silently drops past `limit` with backend-dependent ordering.

**Human-in-the-loop and observation:**

- **The gate is a wire-standard artifact, not an SDK object.** The ask flips the owning A2A task to `input-required` with the question as its status message; *any* A2A client can list, render and answer it with a `SendMessage` carrying the `taskId`. LangGraph's `Interrupt` is reachable only via the Python/JS SDK or the hosted HTTP API.
- **Approver identity and audit.** LangGraph's own documented absence: "no built-in authorization or approver identity model in the interrupt mechanism — `Command(resume=…)` accepts any value from any caller holding the thread_id." agentd checks owner-or-operator before resolving (`a2a_server.rs:811-813`), audits `ask_human` and `ask_human.answered` with the principal, and stamps `via: human|auto` into task status, log and audit so a judge's guess can never be mistaken for a person's decision.
- **A gate has a deadline and a policy for nobody being there:** 24 h default plus per-ask `timeout`, and `agent.ask_human_fallback: fail|wait|auto` — `fail` the default so a headless deployment cannot silently hang, `auto` a conservatively-prompted judge whose `UNDECIDED` sentinel fails the gate rather than guessing. No LangGraph counterpart at all.
- **An MCP server can ask the human.** `crates/agentd/src/mcp/elicit.rs` bridges `elicitation/create` onto the same durable gate machinery, on the MCP event thread so a server waiting on a person does not stall the turn talking to it. LangGraph has no elicitation bridge; a tool needing a human value must become a node.
- **Multi-surface convergence by broadcast, in the OSS daemon.** `SubscribeToEvents` with `fromSeq` cursor resume, a `goodbye` cursor, `hello.resync`, per-event principal-scoped visibility, and a 4 Hz section diff — a TUI, a browser tab and a phone fold one stream with no client-to-client protocol. The LangGraph equivalent (`join_stream`, `Last-Event-ID` replay) exists only in the paid Platform.
- **Live activity instead of a token firehose.** Change-triggered `activity` events (phase, tool, round, tokens, `started_ms` so clients tick their own clock) give liveness at a handful of events per turn, keeping the replay ring a record of *state*. LangGraph's `messages` mode also carries the documented subgraph trap where a nested agent emits nothing unless the caller passes `subgraphs=True`.
- **Reversible operator holds at two scopes** — `a2a.pause {run}` parks one run, without a run it holds the instance (intake continues, dispatch parks). LangGraph OSS has no pause; the Platform offers cancel and the destructive double-texting strategies.

**Telemetry:**

- `run_id` (constant across the tree) + `agent_path` (`0`, `0.2`, `0.2.1` — subtree queries are a prefix match) + `pid` (joins to `pstree`), minted by the supervisor, never trusted from the child, handed down in the spawn payload before any side effect. LangGraph's equivalent picture requires LangSmith.
- **W3C trace-context on by default at zero dependency cost**, across every hop agentd owns: `_meta.traceparent` on outbound MCP `tools/call`/`resources/*`, the `traceparent` header on the intelligence request, `{trace_id, parent_span_id}` in the subagent spawn payload. Malformed inbound headers are ignored and a trace minted — a bad header never fails a run. LangChain self-instruments only its own model wrapper; anything else needs `@traceable`/`wrap_openai`.

---

## 6. Recommended roadmap

Sized in engineer-days on the assumption of familiarity with the tree. "Release" items are process, not code.

### R0 — Cut 2.3.0 (release)
CEL is done and on `main` (`55845be`), in no tag. Until this ships, "what agentd does" for anyone not building from source excludes `when`, `iterate.while/until`, `assert.condition`, `wait.condition`, trigger `filter`s and `map`/`filter`/`reduce` `expr`. Ship it **with R1**, because the documented workaround for R1 requires CEL. **Size: 0.5d.**

### R1 — Fix branch pruning · **3–5d**
Third `StepStatus` distinguishing *pruned* from *skipped-but-joinable*; reachability over the live subgraph inside the existing `schedule()` fixpoint. Must preserve the property that makes uneven-branch joins work without `defer=True`: a step runs when *some* dep was pruned but another live path reached it, and is pruned when *all* inbound paths are pruned. Add conformance cases for both shapes plus `on_error: goto` re-entry. This is the only wrong-answer bug in the report.

### R2 — Fan-out defaults and an honest cap · **2d**
Raise `foreach`/`batch` `parallel` defaults; move `MAX_BATCH_PARALLEL` to `limits.workflow.fan_out`; make an over-cap request a **load-time error**, not a silent clamp (`nested.rs:235-241`, `model.rs:20`). Silent clamping is the class of trap the field whitelist exists to prevent.

### R3 — The fail-closed hygiene batch · **5–7d total**
Every item small, every item the same posture — "a declared knob must do what it says." Ship as one release note.
- Enforce `outputs.schema` at the `finish` step with the existing `jsonschema::validate` (§5)
- Validate `collect.mode` against the enum at parse time (§21)
- Honour `on_replay: skip|fail` in the restore loop, **or delete it from the published schema** (§6)
- Honour `store.durability.steps: eventual` for step progress only — strict stays default, inbox and pre-effect writes never dialable — **or delete the knob and the CLI flag** (§19)
- Wire `human.to` to the gate task's principal, **or reject `to`/`reply_uri` at parse time** (§10)
- Retry: add jitter + a `retry_on: transient | any` discriminator over classes the runtime already distinguishes; add workflow-level `defaults: {retry: …}` (§8)
- Default a run's step/token/deadline budget from `limits.run.*` when the definition is silent — and fix `docs/configuration.md:228-230`, which currently describes them as per-run (§9)
- Stall message names the first failed ancestor by walking `depends_on` (§7)
- Static concurrent-write refusal: two steps in the same wave writing one key with disagreeing modes → load error (§3)
- Fix `checkpoint`-is-a-noop: implement it or correct `docs/node-registry.md:88` (§22)
- Fix `docs/design/notes-langgraph-parity.md:38` — "Time travel / fork ✅" is false for the 2.0 engine (§26)
- Write the absolute `deadline_ms` into the human wait record (§24)

### R4 — Typed human answers + gate identity · **5–8d**
Validate the `human` reply against `schema` with re-ask on mismatch, reusing `mcp/elicit.rs::shape_reply` (§4). Then give each gate its own identity — a gate task per suspended step rather than the run's single task — so `human` inside `parallel`/`foreach`/`batch` works, and re-arm *all* suspended human steps on restore (§15). **Interim on day one:** refuse `human` inside concurrent bodies at parse time, so it fails at validation instead of at runtime. Also mark restart-lost turn gates so a client can say so (§27) — small, and it removes the surprise without importing replay semantics.

### R5 — Retention, GC, and run-record pruning · **5–8d**
`store.retention: {runs: {keep_last, ttl}}`, an eviction pass, manifest index cleanup, and a cap on in-memory adoption at restore (§13); plus dropping terminal scoped `StepState` entries whose results are already copied into the progress record's positional `results` map (§20). **First: resolve the contested 64 KB offload claim** — if there is no automatic spill, §20 is worse than rated and a large step output rides in the record on every rewrite. This is the item that decides whether a long-lived laptop instance survives a month, and it is a prerequisite for R8.

### R6 — Declared state · **5–8d**
A `state: {key: {type, reducer}}` block validated at parse time (§12), upgrading R3's mode-agreement heuristic into a declared-policy check, and giving `write_var` a schema to route through. The `unknown workflow field "state"` refusal means the name is already reserved fail-closed.

### R7 — Debug affordances · **3–5d**
A `step` feed event on transitions, owner-scoped with the 2 KiB truncation `run.get` already applies (§23), and `before_step`/`after_step` on the existing pause op consulted in `schedule`/`begin_step` (§22). Together these turn "3 done, 1 running" into a usable inner loop without inventing a new surface. Cheap, high daily value.

### R8 — Then decide, deliberately
Three items that should each get an explicit yes-or-no rather than drifting on a backlog:

- **Checkpoint history / fork** (§26) — large, needs retention (R5) first, and needs an answer to "append-only default file store, or a capability cliff between backends?" Highest ratio of builder value to philosophical friction: evals, debugging and re-answering a human decision all want it. The adapter contract already documents `get(key[, seq])`.
- **Operator state edit** (§16) — medium, and the four things that make it safe (operator principal, audit stream, envelope versioning, hash pinning) already exist. Ship it *after* history so an edit can fork rather than mutate.
- **Per-call approval gate** (§14) — medium, plumbing exists (`ToolRequest`/`ToolResult`, `elicit.rs` proving a non-stalling child-side block), and it closes the distance between "the model's cooperation is not a control" and today's reality where approval *is* the model's cooperation.

**Explicitly not on the roadmap, and say so publicly:** runtime-computed step ids inside one graph (refuted as a need — the `workflow` node's `name` is already a rendered template, and closing it would cost the static guarantees agentd sells); agentd-held leases (§28 — a published boundary with a shipped workflow-level alternative in `docs/scaling.md` §2c); turn-gate replay durability (§27 — would import LangGraph's replay-the-node idempotency hazards); `Command.PARENT`-style child→parent control transfer (switching on the child's returned output expresses the safe subset).

---

## 7. Evidence quality

- **Strongest** — items 0, 1, 2, 3, 5, 7, 13, 17, 19, 20, 21 and the routing refutation were confirmed by executing real workflows on `target/debug/agentd` and reading the resulting logs, store files and exit codes, not by reading source.
- **Strong** — the remaining items are grep-complete over `crates/agentd/src` with file:line citations and, where relevant, `--workflow-schema` output or in-tree test assertions (`hitl_e2e.rs:315-318`).
- **Contested** — the automatic 64 KB artifact offload (§20). Two passes disagree; resolve at `steps.rs:1672-1704` before citing it as an advantage or sizing §20.
- **Unverified** — the entire "behind on" column for **dimension 4** (graph visualisation, deterministic offline test mode, hosted serving, versioned assistants, evaluation tooling). Its per-gap adversarial verification was not returned. Do not act on those without running them down; the two items I can corroborate independently (no per-step stream, no time travel) are §23 and §26.
- **Unverified** — "no per-run in-flight concurrency ceiling" (a wide DAG of 30 independent `agent` steps). The claim is plausible and consistent with the measured unbounded top-level parallelism (12 × `sleep 2s` in 2.08s), but it was not separately checked. Low stakes either way.