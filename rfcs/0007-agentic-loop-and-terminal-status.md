# RFC 0007: Agentic loop & terminal-status state machine

**Status:** Draft
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

The inner agentic loop is the one place in agentd where the model owns
control flow. Everything else — the supervisor (RFC 0002/0003), the MCP
client/server (RFC 0004/0005), intelligence transport (RFC 0006), the modes
and router (RFC 0008), subagent spawn (RFC 0009) — exists to *start*,
*feed*, *bound*, and *stop* this loop. RFC 0001 specified "final" as "the
model stopped emitting tool calls." The assessment (§1.2.5) calls this **the
biggest loop hole**: it conflates a model going quiet with a task being done,
has no named verification grounded in reality, no taxonomy for which errors
are recoverable, and no defense against the two empirically dominant failure
modes of 2024–2026 production agents:

- **Runaway loops.** Four agents ran 11 days and billed **$47K** on a missing
  `max_steps`; a 2025 IDC survey found **92% of orgs** running agentic AI
  reported higher-than-expected costs with runaway loops the named cause. The
  global step/token/deadline cap is therefore non-negotiable safety, not a
  knob.
- **Premature / confident completion.** Agents mark work "done" without
  testing; a loop that self-judges spins confidently while making no progress.
  Reflexion-style self-critique *reinforces the model's own blind spots*
  because the same model writes the output and grades it — verification that
  is not grounded in tool/exec ground truth is worse than none.

This RFC makes the loop an explicit state machine: one ReAct turn structure,
a **disjunction of cheap per-turn stop checks each with a distinct terminal
status**, a named **VERIFY** discipline grounded in tool/exec results, an
**error taxonomy** that separates model-recoverable from runtime-fatal, and
the **context-management levers** that keep a warm session from rotting. It is
identical across all execution modes — `once`/`loop`/`reactive`/`schedule`
differ only in what the supervisor does *after* a terminal status (RFC 0008).

The loop runs **inside the subagent process** (assessment §2.6, RFC 0009).
The supervisor never runs it. Module home: `loop/{agent,stop,context,action}.rs`
(assessment §4.0).

---

## 2. Decision

**One ReAct loop implementation, mode-agnostic, living only in the subagent
process.** Per turn: assemble request → call intelligence → record
usage/bump budgets → branch on tool-calls vs final. Stopping is a
**disjunction of cheap checks evaluated every turn, each mapping to a distinct
terminal status** drawn from the closed set:

```
completed · refused · exhausted_steps · exhausted_tokens · deadline ·
stalled · loop_detected · cancelled · crashed
```

This RFC is the **single authority for the `TerminalStatus` enum**; every other
RFC that names a terminal status (0001 §6.1, 0005 §4.4, 0008 §3.1, 0009 §3.4,
0011 §5.2) refers to *this* closed set and must not introduce a variant absent
here. The two *fatal-infrastructure* aborts — intelligence-unreachable/auth and
required-MCP-server-down (§3.6) — are not loop stop-conditions; they are
abort outcomes whose exit-code mapping is owned by RFC 0011 §5.2, not new
`TerminalStatus` variants.

The global step / token / deadline cap is a **hard safety system**, not a
preference: at every soft budget the agent wraps up gracefully and returns a
labeled partial; `RLIMIT`/`SIGKILL` from the supervisor (RFC 0003) is the
backstop for a wedged child that cannot wrap up.

**VERIFY is grounded in tool/exec results and MCP resource state — never in
the model judging itself.** It is loop discipline + a place in the event
stream, not a second model call. agentd ships no LLM-as-judge in core.

**Errors split three ways:** tool-domain errors and malformed model output
become **observations** the model adapts to (recoverable, step-consuming);
transient transport errors get **bounded transport-layer retry** before
surfacing; fatal infra **aborts** with a matching terminal status.

**Resources reach the agent two ways at once:** a compact catalogue
(URIs + descriptions + mtime/etag, never bodies) is injected for *awareness*;
the `resource.read` self-tool pulls bodies on demand for *attention*.

**Context is managed by ordered levers:** clear stale tool results (keep last
M≈5 verbatim) → compact at ~75% window via one summarize call → optional
`note.write/read`. Token budget is estimated from the previous response's
`usage` plus a chars/4 forward heuristic; **no tokenizer dependency.**

This RFC does not introduce plan-execute, ReWOO, or a reflection pass into
core (assessment §1.1; §3 of the agent-loop notes). Richer patterns are
expressed by the model and by spawning subagents (RFC 0009), not by runtime
modes.

---

## 3. Mechanisms

### 3.1 Turn structure (ReAct, deliberately ordinary)

The shape stays boring. The value is in the guardrails around the loop, not
in exotic topologies. One turn:

```
1. assemble request          (§3.2) — system + instruction + seed + transcript
                                       + scoped tool catalogue + resource catalogue
2. evaluate PRE-CALL stops   (§3.4) — exhausted_tokens (next call would exceed),
                                       deadline, cancelled  -> may terminate here
3. call intelligence         (RFC 0006) -> assistant message (text and/or tool_calls)
4. record usage, bump budgets, append assistant message      (§3.7)
5. branch on the message:
     - has tool_calls?  -> for each: scope-check -> route -> append result/error
                           as observation (§3.3); this is the VERIFY ground truth
     - final / no calls -> run VERIFY gate (§3.5); if it passes, emit `completed`
6. evaluate POST-CALL stops  (§3.4) — exhausted_steps, stalled, loop_detected
7. if no stop fired: loop
```

The loop is a straight-line state machine, not async. The subagent's control
reader (ping/pong, cancel) runs on a **separate thread** (assessment §2.3,
RFC 0005/0009) so liveness survives a long in-flight model or tool call; the
agentic loop reads `cancel` as a flag it checks at the points marked in §3.4.

Rust sketch of the loop driver:

```rust
struct LoopState {
    transcript: Transcript,          // §3.6 — managed by compaction
    budgets:    Budgets,             // steps/tokens/deadline, check-before/record-after
    scope:      ToolScope,          // granted MCP subset (RFC 0012)
    catalogue:  ResourceCatalogue,  // §3.8 — awareness, never bodies
    repeats:    HashMap<u64, u32>,  // hash(tool+canon_args) -> count (§3.4 loop_detected)
    progress:   ProgressTracker,    // content-hash over N turns (§3.4 stalled)
    cancel:     Arc<AtomicBool>,    // set by the control thread (RFC 0009)
}

enum TurnOutcome {
    Continue,
    Terminal(TerminalStatus),
}

fn run_loop(mut st: LoopState, intel: &IntelClient, mcp: &McpRegistry)
    -> (TerminalStatus, RunResult)
{
    loop {
        match run_turn(&mut st, intel, mcp) {
            TurnOutcome::Continue => continue,
            TurnOutcome::Terminal(status) => {
                return (status, distill_result(&st, status)); // §3.9
            }
        }
    }
}
```

### 3.2 Message / context assembly

Each request is assembled fresh, in this order (assessment §2.6; loop notes
§1.2):

1. **System block** — standing instructions + operating contract + tool-use
   and verification guidance, at medium "altitude" (specific enough to steer,
   flexible enough to generalize), in labeled sections (`<role>`, `<task>`,
   `## tools`, `## resources`, `## output`, `## limits`, `## verify`). agentd
   ships a small heuristic base prompt; the `INSTRUCTION` carries specifics.
2. **Resource awareness block** — the compact catalogue (§3.8). Never bodies.
3. **Instruction** — the task: parent's `instruction` (subagent) or the
   templated event payload (a reactive `spawn` turn, RFC 0008).
4. **Context seed** — parent-provided slices for a subagent (RFC 0009), or the
   carried-over transcript for a warm `continue` session (RFC 0008). **Never**
   the parent's full transcript.
5. **Running transcript** — prior turns as managed by §3.6 compaction.

The scoped tool catalogue (granted MCP tools + agentd self-tools) is passed in
the provider's native `tools` field (RFC 0006), **not** stuffed into system
text, when the gateway supports tool-calling. When it does not, the catalogue
is rendered into the system text and the JSON-action fallback parser (§3.3) is
used. agentd ships no tools of its own beyond the self-MCP surface (RFC 0005);
tool-description quality is the MCP server author's responsibility — agentd
passes them through faithfully and token-efficiently, and treats every byte of
server-provided metadata as untrusted (RFC 0012).

### 3.3 Tool-call execution + result feedback (the VERIFY ground truth)

For each tool call in an assistant turn:

```rust
fn handle_tool_call(st: &mut LoopState, mcp: &McpRegistry, call: &ToolCall)
    -> ToolObservation
{
    // 1. scope check — recoverable, never abort
    if !st.scope.allows(&call.name) {
        return ToolObservation::error(&call.id,
            format!("tool `{}` is not in this agent's granted scope", call.name));
    }
    // 2. per-tool repeat cap — refuse, do not execute (§3.4 loop_detected feeder)
    let key = repeat_key(&call.name, &call.arguments);   // hash(name + canonicalized args)
    let n = st.repeats.entry(key).or_insert(0);
    *n += 1;
    if *n > REPEAT_CAP {   // default K = 3
        return ToolObservation::error(&call.id, format!(
            "you have called `{}` with these arguments {} times with no progress; \
             try a different approach or finish", call.name, *n));
    }
    // 3. route + call (MCP server or agentd self-MCP), with transport retry (§3.5)
    match mcp.call(&call.name, &call.arguments) {
        Ok(result) => {
            // isError:true is a SUCCESSFUL result carrying a domain error → observation
            ToolObservation::from_call_result(&call.id, result)
        }
        Err(TransportError::Fatal(e)) => ToolObservation::Fatal(e),  // §3.5 abort
        Err(TransportError::Exhausted(e)) =>                          // retried, still failing
            ToolObservation::error(&call.id, format!("tool transport failed: {e}")),
    }
}
```

Key invariants:

- **`isError:true` vs JSON-RPC `error` is load-bearing** (assessment §1.3.7).
  `isError:true` lives *inside* a successful `tools/call` result → it is a
  domain error, fed back as an observation; a JSON-RPC `error` is a
  protocol/transport failure handled by §3.5. The loop must distinguish them.
- **On a successful repeat that did make progress**, reset the counter for that
  key (a tool that now returns new content is not looping). "Progress" =
  the result's content digest differs from the prior call's (§3.4.1).
- **Native tool-calling is primary; JSON-action is the demoted fallback**
  (assessment §2.4). Fallback parser: balanced-brace, prose-tolerant
  `extract_json_object` + `parse_action` over `{"action":"tool"|"final", ...}`
  (lifted from the retired `loop_node.rs`). A parse failure is itself a
  recoverable, step-consuming observation: *"your reply was not a valid action
  object; reply with exactly one JSON object."*

### 3.4 Stop-condition disjunction + terminal statuses

Evaluated every turn, cheapest first. Each fires a **distinct** terminal
status so the parent (RFC 0009) and the exit-code mapping (RFC 0011) can tell
*why* it stopped. `completed` ≠ "capped" is the whole point.

| Status | When | Check site | Default |
|---|---|---|---|
| `completed` | assistant final + VERIFY gate passed | post-call (§3.5) | — |
| `refused` | model concludes the task cannot be done / declines (a `{"action":"final"}` or final turn that asserts impossibility) | post-call (§3.5) | — |
| `exhausted_steps` | turn counter ≥ `max_steps` | post-call | 40 |
| `exhausted_tokens` | cumulative ≥ `max_tokens`, or next call's estimate would exceed | pre-call | per grant |
| `deadline` | `now ≥ deadline` (wall clock) | pre-call + per turn | finite, never ∞ |
| `stalled` | progress signature unchanged for `idle_limit` turns | post-call | N = 3 |
| `loop_detected` | a single tool+args repeated past `K` with no intervening success | post-call | K = 3 |
| `cancelled` | control-channel `cancel` flag set | pre-call + post-call | — |
| `crashed` | process exit / panic | observed by supervisor (RFC 0003) | — |

Implementation:

```rust
enum TerminalStatus {
    Completed, Refused, ExhaustedSteps, ExhaustedTokens, Deadline,
    Stalled, LoopDetected, Cancelled, Crashed,
}

// Pre-call: would taking another turn violate a hard budget?
fn pre_call_stop(st: &LoopState, next_estimate: u64) -> Option<TerminalStatus> {
    if st.cancel.load(Ordering::Relaxed)             { return Some(Cancelled); }
    if Instant::now() >= st.budgets.deadline         { return Some(Deadline); }
    if st.budgets.tokens_used + next_estimate
        >= st.budgets.max_tokens                     { return Some(ExhaustedTokens); }
    None
}

// Post-call: after recording the turn's effect.
fn post_call_stop(st: &LoopState) -> Option<TerminalStatus> {
    if st.cancel.load(Ordering::Relaxed)             { return Some(Cancelled); }
    if st.budgets.steps_used >= st.budgets.max_steps { return Some(ExhaustedSteps); }
    if st.progress.idle_turns() >= IDLE_LIMIT        { return Some(Stalled); }
    // loop_detected surfaces via §3.3's refusal; a hard cap is the backstop:
    if st.repeats.values().any(|&c| c > REPEAT_CAP * 2) { return Some(LoopDetected); }
    None
}
```

`max_steps`, `max_tokens`, and `deadline` are bounded by the **remaining tree
budget** the supervisor mints into the spawn payload (RFC 0003/0009): a node
over its grant is cancelled; the tree root over the tree ceiling drains the
tree. The agentic loop enforces its own grant locally (check-before /
record-after, salvaged from the retired `budget.rs`); the supervisor is the
source of truth for the tree (RFC 0003 §hierarchical accounting). The hard
`RLIMIT_AS`/`RLIMIT_CPU` + `SIGKILL` ladder is the backstop for a child that
fails to wrap up gracefully — observed by the parent as `crashed`.

`cancelled` and `crashed` are the two statuses the *supervisor* may attribute
rather than the loop: a `cancel` control frame (RFC 0009) sets the flag the
loop honors; a panic/exit is reaped and classified by RFC 0003's EOF×pong
classifier.

#### 3.4.1 No-progress / `stalled` detection (concrete, no model call)

Define a per-turn **progress signature**:

```rust
fn progress_signature(turn: &Turn) -> u64 {
    let mut h = FnvHasher::default();          // tiny, hand-rolled; no crate
    for r in &turn.new_tool_results { h.write(&blake_digest(r.content_bytes())); }
    h.write(&blake_digest(turn.assistant_text_delta.as_bytes()));
    h.finish()
}
```

`ProgressTracker` keeps the last signature and a counter; an unchanged
signature (or an identical tool error) for `idle_limit` (default 3)
consecutive turns → `stalled`. This catches the "confidently spinning, making
no real progress" silent failure with a content hash, not semantic judgment. A
model stuck emitting garbage (repeated malformed output, §3.3) terminates as
`stalled` rather than spinning forever, because malformed output is
step-consuming and produces no new signature.

For `loop` mode, the *outer* "nothing to do" idleness is the supervisor's
re-entry/backoff concern (RFC 0008), not this inner detector.

### 3.5 VERIFY phase — grounded, never self-judgment

The canonical harness loop is **gather context → act → verify → repeat**, and
verify is a *named* stage, not implicit. agentd implements it as discipline,
not as a second model call:

1. **The act phase already produces the ground truth.** Tool results, MCP
   resource state, and gated `exec` output (test/lint/build) are the
   verification substrate. The model's *final* must be earned against this,
   not against the model re-reading itself.
2. **The VERIFY gate runs when the model emits a final** (no tool calls). It is
   a guard against premature completion:
   - If the instruction declared an **output contract** (objective + required
     output format + success criteria; RFC 0009 spawn payload), the loop checks
     the structural criteria it *can* check cheaply: required output
     fields/shape present (schema-validate the structured result), declared
     artifacts written (a `resource.read`/`exec` probe the *instruction* asked
     for). A structurally incomplete final is fed back as an observation
     (*"your result is missing required field X / objective Y is unverified;
     verify against the environment before finishing"*) and the loop continues,
     consuming a step.
   - If no machine-checkable contract was declared, the final is accepted as
     `completed`; agentd does not invent semantic judgment.
   - A final that explicitly declines the task (the model concludes it *cannot*
     be done) terminates as **`refused`**, not `completed`. This is the
     semantic-refusal status the one-shot exit-code mapping sends to exit 5
     (RFC 0011 §5.2); it is the model's own verdict, never agentd judging.
3. **No LLM-as-judge in core.** Self-critique without external ground truth
   reinforces blind spots (the Reflexion lesson). If a deployment wants a
   critic, it spawns a subagent with a clean context (RFC 0009) or runs an
   MCP-served judge — neither is runtime machinery here.

Ordering of verification preference (from best to last resort), encoded as
guidance in the base prompt and realized through tools, never as core code:
**rule-based** (linter/type-check/test runner via `exec`, MCP validators) >
**structured/state** (resource state checks, structured diffs) >
**self-check in the instruction** (the model told to re-examine) — and
explicitly *not* a separate judge model.

**Transport retry (the deterministic layer)** sits under §3.3, not under the
model:

```rust
fn call_with_retry(mcp: &McpServer, name: &str, args: &Value) -> Result<CallResult, TransportError> {
    let mut backoff = Duration::from_millis(100);
    for attempt in 0..MAX_TRANSPORT_RETRIES {     // default 3
        match mcp.tools_call(name, args) {
            Ok(r) => return Ok(r),                 // includes isError:true results
            Err(e) if e.is_transient() => {        // timeout, reset, 429/503
                sleep(backoff + jitter(backoff));
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
            Err(e) if e.is_fatal() => return Err(TransportError::Fatal(e)),
            Err(_) => break,
        }
    }
    Err(TransportError::Exhausted(/* last */))
}
```

The two retry layers are kept strictly separate: **transport** = deterministic
bounded backoff for transient failures, invisible to the model;
**semantic** = persistent/logical failures become observations the model
reasons about (substitute tool, replan, give up gracefully). Retrying a
*semantic* failure deterministically in a loop **is** the runaway-loop bug —
forbidden.

### 3.6 Error taxonomy (which errors are observations, which abort)

| Class | Examples | Handling | Terminal? |
|---|---|---|---|
| Tool-domain error | file not found, HTTP 404, bad args, validation fail, `isError:true` | observation; model adapts | no (step-consuming) |
| Malformed model output | invalid tool-call JSON args, unparseable JSON-action | observation: validation error fed back | no (step-consuming) |
| Capability-unavailable | tool not in scope / server down / `exec` not compiled | observation: "tool X unavailable" | no |
| Transient transport | MCP stdio hiccup, conn reset, 429/503, intel 5xx/timeout | bounded transport retry (§3.5); if still failing → observation | no |
| Fatal infrastructure | intelligence unreachable/auth after retries, OOM, hard budget mid-call | abort the loop | yes (matching status) |

The line: **errors the model can do something about are observations; errors
about the runtime itself are terminal.** Fatal infrastructure maps to the exit
codes in RFC 0011 (intelligence unreachable/auth → 4; required MCP server dead
→ 6; budget → 7). Malformed output and tool-domain errors *count toward*
`max_steps` and feed the `stalled`/`loop_detected` detectors, so a model that
cannot recover terminates cleanly with a labeled status rather than spinning.

### 3.7 Usage accounting + token estimation (no tokenizer)

Check-before / record-after, the retired `budget.rs` discipline:

```rust
struct Budgets {
    max_steps: u32, steps_used: u32,
    max_tokens: u64, tokens_used: u64,
    deadline: Instant,
}

// AFTER each intel call: record the provider's authoritative usage.
fn record_usage(b: &mut Budgets, usage: &Usage) {
    b.steps_used += 1;
    b.tokens_used += usage.prompt_tokens as u64 + usage.completion_tokens as u64;
}

// BEFORE each intel call: estimate the next request's cost without a tokenizer.
fn estimate_next(transcript_bytes: usize, last: Option<&Usage>) -> u64 {
    // ground truth from the previous response, plus a chars/4 forward heuristic
    // for the new bytes added since.
    (transcript_bytes as u64) / 4
        + last.map(|u| u.prompt_tokens as u64).unwrap_or(0) / 8  // small safety margin
}
```

No tokenizer crate (minimalism bar). The provider's `usage` from the previous
response is ground truth for what already happened; `chars/4` estimates
forward. Estimate **conservatively (early)** so an off-by-some never causes a
hard context overflow. If the provider returns a context-length error despite
the estimate, treat it as "compact-now-and-retry once" (§3.6 → §3.7 compaction
→ one retry, then surface).

### 3.8 Resources: list AND read (awareness vs attention)

Two mechanisms at once (assessment §2.6; loop notes §3):

- **Awareness — the catalogue, in context.** At loop start and after any
  `notifications/resources/list_changed`, the loop calls `resources/list` on
  each scoped server (cursor-paginated, RFC 0004) and injects a compact entry
  per resource: `{uri, name, one-line description, mtime/etag/size if known}`.
  **Never the bodies.** Cap the catalogue by a size budget; if a server exposes
  thousands of resources, summarize by URI prefix/type rather than listing
  every URI.
- **Attention — the body, via a tool.** The `resource.read(uri)` self-tool
  (RFC 0005, a thin wrapper over `resources/read`) pulls a body only when the
  model decides it needs it. List = cheap awareness; read = deliberate,
  model-chosen attention — Anthropic's just-in-time / "smallest high-signal
  token set" pattern, for free over MCP's existing list/read split.

```rust
struct CatalogueEntry { uri: String, name: String, desc: String, etag: Option<String>, size: Option<u64> }
struct ResourceCatalogue { entries: Vec<CatalogueEntry>, byte_budget: usize }
```

For a **reactive** turn (RFC 0008), the event delivered to the loop carries the
*changed URI* (+ etag/version when the server provides one), not the new body.
The agent `resource.read`s the current value if the change is relevant —
re-reading **current state** on the agent's terms, never auto-inlining a diff,
which is also what makes redelivery idempotent (RFC 0008's
re-read-current-state contract).

### 3.9 Context management (lever-ordered)

Long contexts rot — recall degrades as tokens rise (a gradient, not a cliff).
Apply levers in this order:

1. **Lever 1 — clear stale tool results (cheapest, safest).** Keep the **last
   M verbatim** (default M = 5), replace older tool-result bodies with a tiny
   stub: `[tool result for read_file(x) elided; re-read if needed]`. Lossless
   in practice because the agent can re-fetch. Reclaims most bloat in
   tool-heavy loops.
2. **Lever 2 — compaction at ~75% of the window (one model call).** When the
   §3.7 forward estimate crosses the high-water mark (default 0.75 of the
   model's context window), make **one** summarize-and-reinitialize call:
   *"summarize this conversation, preserving decisions, unresolved problems,
   key facts/IDs, and the current plan; drop redundant chatter."* Reinitialize:
   `transcript = [system] + [summary] + [last M verbatim tool results]`.
   Maximize recall first, then precision. No new dependency — one ordinary
   model call. The compaction call's usage is recorded against the budget like
   any other.
3. **Lever 3 — externalize to notes (optional, opt-in).** A
   `note.write(text)` / `note.read()` self-tool pair backed by a single file
   (RFC 0005) lets a long-horizon agent persist durable state outside the
   window and pull it back. This is the **only** "memory" in core: a file
   behind a tool. No vector store, no embedding model, no memory service.

```rust
struct Transcript { turns: Vec<Turn>, keep_verbatim: usize /* M=5 */ }
impl Transcript {
    fn stub_stale_tool_results(&mut self);                  // Lever 1, every turn
    fn needs_compaction(&self, est: u64, window: u64) -> bool { est * 100 >= window * 75 }
    fn compact(&mut self, intel: &IntelClient, b: &mut Budgets);  // Lever 2, one call
}
```

**Subagents are themselves the cross-agent context lever** (RFC 0009): pushing
token-heavy exploration into a child with a clean window that returns a
1–2k-token distillate keeps the parent's window bounded — §3.9 and the spawn
result contract are the same lever viewed from two angles. `distill_result`
(§3.1) produces that structured value + terminal status + usage for the parent.

---

## 4. Interactions with other RFCs

- **RFC 0001 (core).** This RFC replaces 0001's "final = model stopped
  emitting tool calls" with the §3.4 terminal-status state machine and the
  §3.5 VERIFY gate.
- **RFC 0002 (reactor) / RFC 0003 (supervision).** The loop runs in the
  subagent; the supervisor enforces the *outer* deadline (Detector A), the
  no-progress watchdog (Detector B), and ping/pong liveness (Detector C) — all
  on a thread separate from this loop. `cancelled` honors a control-channel
  cancel; `crashed` is attributed by the supervisor's EOF×pong classifier.
  Tree-budget enforcement and the `RLIMIT`/`SIGKILL` backstop for a child that
  won't wrap up live there; this RFC enforces the per-node grant locally.
- **RFC 0004 (MCP client).** Tool calls, `isError` vs JSON-RPC `error`,
  `resources/list`+`read`, cursor pagination, and `list_changed` consumption
  are this RFC's act/awareness substrate.
- **RFC 0005 (self-MCP & control protocol).** Provides the `resource.read`,
  `note.write/read`, and `subagent.*` self-tools the loop routes internally,
  and the control-channel `cancel` the loop honors.
- **RFC 0006 (intelligence).** Supplies the assistant message (native
  tool-calls or JSON-action fallback) and the authoritative `usage` the §3.7
  accounting and §3.9 compaction trigger depend on; the JSON-action fallback
  parser is the demoted path here.
- **RFC 0008 (modes & routing).** Identical loop in all modes; the mode is the
  exit predicate over a terminal status. Reactive events arrive as a
  templated instruction (spawn route) or as a delivery into a warm session's
  next turn (continue route); the §3.8 changed-URI re-read contract is what
  makes redelivery safe.
- **RFC 0009 (subagent model).** The loop's `distill_result` is the spawn
  result contract; the output contract this RFC's VERIFY gate checks is part of
  the spawn payload; per-node budgets are minted by the supervisor from the
  parent's remaining tree budget.
- **RFC 0010 (observability).** Each loop stage emits the closed-vocabulary
  events `loop.start/step/final/error`, `intel.call/result`,
  `tool.call/result`; the terminal status is logged on `loop.final`. Content
  capture is off by default (hashes/lengths only).
- **RFC 0011 (exit codes).** One-shot maps the root subagent's terminal status
  to an exit code: `completed→0`, semantic refusal→5, partial (any
  `exhausted_*`/`deadline`/`stalled` with a usable partial)→3, budget without
  result→7, deadline→124, fatal infra→4/6.
- **RFC 0012 (security).** All MCP server content — including tool descriptions
  and results — is untrusted; the §3.3 scope check is the Rule-of-Two trust
  budget in the loop; distilled returns (§3.9) double as an injection firewall.

---

## 5. Non-goals / Deferred

- **No plan-execute / ReWOO / Reflexion mode in core** (assessment §1.1).
  Expressed by the model + subagents, never as runtime machinery.
- **No LLM-as-judge / self-critique pass in core** (§3.5). A critic is a
  spawned subagent or an MCP server.
- **No tokenizer dependency** (§3.7). `usage` + chars/4 only.
- **No built-in memory store** (§3.9). The only "memory" is `note.write/read`
  over a file.
- **No streaming of partial child results into the parent's reasoning**
  (deferred per the agent-loop notes §6.3; the async-handle + completion-as-
  resource in RFC 0009 covers the real need).
- **MCP-backed session checkpointing of warm-session transcript** is a v2
  extension (assessment §2.8, RFC 0013); v1 warm sessions are in-memory and
  recovered by idempotent re-trigger (RFC 0008).
- **Semantic verification of arbitrary instructions** is out: VERIFY checks
  only machine-checkable contract criteria (§3.5). agentd does not judge
  open-ended task quality.

---

## 6. Open items

- **Default numeric knobs need empirical tuning** (all overridable):
  `max_steps` 40, `idle_limit` 3, repeat-cap K 3, `keep_verbatim` M 5,
  compaction high-water 0.75, `MAX_TRANSPORT_RETRIES` 3. These are starting
  defaults; the M7 conformance/eval pass (assessment §4) should validate them
  against a small (~20-query) real-usage set.
- **Coalesce/re-read assumes current-state resource semantics** (loop notes
  §11.4). If any target resource has discrete *event-stream* semantics (each
  update is a must-not-lose item), the §3.8 re-read-current-state contract is
  insufficient for that route — confirmed against use cases in RFC 0008, not
  here.
