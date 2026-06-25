# RFC 0009: Subagent process model & nesting

**Status:** Draft
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

The agentic ReAct loop lives only inside subagent child processes; the supervisor owns no LLM dependency (assessment §1.1, §2.6). This RFC specifies **how a subagent process comes into existence, what it is handed, what it returns, and how subagents nest** — i.e. the spawn payload, the result shape, the sync/async/detach dispositions, the tool-scope narrowing, and the depth/breadth/rate/tree-token caps that keep a model-owned loop from forking a bomb.

The RFC 0001 prose treated this as one line ("spawn, track, reap, enforce limits"). The reliability review (notes-review-reliability §7, §8, §9, §12) found this is a safety-critical gap: the model owns the loop, so nothing but the supervisor stops `spawn → spawn → spawn`. The agent-loop-modes review (notes-review-agent-loop-modes §6) found that a *bare* instruction reproduces Anthropic's vague-delegation failure, and that handing a child the parent's full transcript is both a context-hygiene loss and an injection vector.

This RFC covers assessment **§2.7** in full. It is the authority on:

- same-binary re-exec (subagent mode);
- the **rich** spawn payload (output contract + narrowed context seed + tool scope + limits + telemetry);
- supervisor-minted depth (never trusted from the child);
- the distilled structured result + store-and-reference;
- sync-default / async-opt-in / detach (v1 sync-only; async in M3);
- nesting **only** via the supervisor-owned `subagent.spawn` chokepoint;
- the caps enforced at that chokepoint, refused as **tool results**, never crashes;
- folding `exec` children into the same regime.

It does **not** respecify the process-tree mechanics (process groups, PDEATHSIG, reaping, kill ladder, restart governor, hierarchical token accounting) — those are RFC 0003. It does not respecify the control-channel wire — that is RFC 0005. It does not respecify the inner loop's stop conditions — that is RFC 0007. This RFC owns the *payload and policy at the spawn boundary*.

---

## 2. Decision

1. **A subagent is the same binary re-exec'd in subagent mode** (`argv[0]`-dispatched). One artifact, instant `SIGKILL`, OS isolation; the process tree *is* the agent tree.
2. **The spawn payload is rich, not minimal.** It carries an **output contract** (objective + required output format + tool/source guidance + boundaries), a **narrowed context seed** (only chosen slices — never the full transcript), a **tool scope** (a subset of the parent's, narrowing monotonically), **limits**, and a **telemetry** block. A bare instruction string is rejected at the chokepoint as malformed.
3. **Depth is minted by the supervisor** from the caller's handle. The child cannot assert its own depth, parent edge, or budget grant — those fields, if present in the request, are ignored.
4. **The result is a distilled, structured value** (~1–2k tokens) + a terminal status + usage. Large outputs use store-and-reference (the child writes a resource, returns a handle). The parent appends the **distillate**, never the child's raw transcript.
5. **Sync-default, async-opt-in, detach-rare.** `subagent.spawn` blocks the parent's turn by default. `{async:true}` returns a handle; `{detach:true}` is fire-and-forget but still budgeted, depth-counted, and reaped. **v1 ships sync-only; async/detach land in M3** alongside reactivity (they share the subscribe/notify machinery — RFC 0008).
6. **Nesting happens only through the supervisor-owned `subagent.spawn` self-tool** — exactly one unforgeable chokepoint for every cap.
7. **Caps are enforced at the chokepoint** (`max_depth`, `max_children`, `max_total_subagents`, spawn-rate token-bucket, tree-token ceiling) and a violation is **refused as a tool result**, never a crash.
8. **`exec` children are folded into the same regime** (mandatory deadline, process-group kill, subtree budget, breadth/rate caps) — but with no control channel, only the deadline + kill detectors apply, not ping/pong.

---

## 3. Mechanisms

### 3.1 Same-binary re-exec (subagent mode)

There is one binary. `main` dispatches on `argv[0]`/a mode flag before doing anything else:

```rust
fn main() -> ExitCode {
    match dispatch_mode() {
        Mode::Supervisor(cfg) => supervisor::run(cfg),   // owns the reactor (RFC 0002)
        Mode::Subagent        => subagent::run(),        // owns the ReAct loop (RFC 0007)
    }
}

enum Mode { Supervisor(SupervisorConfig), Subagent }

/// Subagent mode is selected by an explicit, non-spoofable marker the
/// supervisor sets when it re-execs itself. We do NOT rely on argv[0]
/// string matching alone (a model-controlled instruction could otherwise
/// try to influence it); the marker is an env var the supervisor controls.
fn dispatch_mode() -> Mode {
    if env::var_os("AGENTD_SUBAGENT").is_some() {
        Mode::Subagent
    } else {
        Mode::Supervisor(SupervisorConfig::load_and_validate()) // exit 2 on bad config
    }
}
```

**Spawn is `/proc/self/exe` re-exec, not a fork-only child.** The supervisor builds the child `Command` from `current_exe()`, sets `AGENTD_SUBAGENT=1`, wires three pipes (control-in on the child's stdin, control-out on its stdout, stderr for the child's own telemetry per assessment §2.9), and applies `pre_exec` hooks (`setpgid`, `setrlimit`, `PR_SET_PDEATHSIG` — all owned by RFC 0003). Rationale (assessment §2.7): one artifact to ship and audit; `SIGKILL` works the instant the process exists; OS isolation is free; `pstree` shows the agent tree.

The subagent's **early `main`** (before any loop work) sets `prctl(PR_SET_PDEATHSIG, SIGKILL)` so a supervisor crash collapses the tree from the leaves up (assessment §2.8; RFC 0003 owns the mechanism — this RFC only notes that subagent mode is where it is armed, because PDEATHSIG is cleared across `execve` and must be re-set in the re-exec'd child).

The **spawn payload does not travel on the command line.** argv is world-readable via `ps`/`/proc`; instructions and context seeds may contain secrets-adjacent or untrusted content. The payload is written as the **first control frame** on the child's stdin (length-prefixed JSON-RPC, RFC 0005 framing) after the pipes are up. argv carries only the mode marker and, optionally, a non-sensitive `--agent-path` for early self-logging before the first frame arrives.

### 3.2 The rich spawn payload

The payload is the `params` of the first downward control frame. Type sketch (serde types live in `subagent/protocol.rs`, per the §4.0 layout):

```rust
struct SpawnPayload {
    // ---- identity & tree position: ALL minted by the supervisor ----
    agent_id:   AgentId,          // emitting process id, e.g. "0.2.1"
    agent_path: String,           // dotted tree path (assessment §2.9)
    depth:      u16,              // minted from caller.depth + 1; child input ignored

    // ---- the output contract (assessment §2.7; notes §6.1) ----
    contract: OutputContract,

    // ---- narrowed context seed: ONLY chosen slices ----
    seed: ContextSeed,

    // ---- capability scope: a subset of the parent's, monotone-narrowing ----
    scope: ToolScope,

    // ---- limits: bounded by the parent's remaining tree budget ----
    limits: Limits,

    // ---- telemetry / correlation block (assessment §2.9) ----
    telemetry: Telemetry,
}

struct OutputContract {
    /// The specific objective. A bare instruction with no objective/format
    /// is REJECTED at the chokepoint as malformed (vague-delegation guard).
    objective: String,
    /// Required output shape: free-text spec or a JSON schema the distilled
    /// result must satisfy. Drives §3.4's structured return.
    output_format: OutputFormat,        // enum { Text, Json(schema), Markdown }
    /// Which tools/sources to prefer and how — steers tool selection without
    /// re-deriving it from scratch.
    tool_guidance: Option<String>,
    /// Explicit task boundaries: what is in scope, what to NOT do, when to stop.
    boundaries: Option<String>,
}

struct ContextSeed {
    /// Parent-chosen facts/messages. NEVER the parent's full transcript.
    /// Each entry is a labeled slice (e.g. "file_path", "id", "sub_goal").
    slices: Vec<SeedSlice>,
}
struct SeedSlice { label: String, content: SeedContent }
enum SeedContent { Text(String), ResourceRef(String) /* agentd:// or server uri */ }

struct ToolScope {
    /// Allowed (server, tool) pairs — a SUBSET of the parent's granted scope.
    allow: Vec<ToolRef>,            // ToolRef { server: String, tool: String }
    /// Rule-of-Two trust tags carried for the scope check (RFC 0012).
    /// Present here so the chokepoint can evaluate the trifecta on grant.
    tags: ToolTags,                 // { untrusted_input, sensitive, egress }
}

struct Limits {
    max_steps:   u32,              // per-subagent step ceiling (notes default 30–50)
    max_tokens:  u64,              // per-subagent token grant (carved from tree budget)
    deadline:    Duration,         // wall-clock; MANDATORY, never infinity
    max_children: u16,             // this node's breadth cap (see §3.6)
}

struct Telemetry {
    run_id:        String,         // ULID, stable across the whole tree
    trace_id:      String,         // 16-byte hex W3C, propagated
    parent_span_id: Option<String>,
    log_level:     LogLevel,
    log_content:   bool,           // --log-content propagation
}
```

**Why rich (assessment §2.7, notes §6.1):**

- **Output contract.** Anthropic's finding: a subagent given only a vague instruction duplicates work, misses requirements, or returns an unusable blob. Each child gets an objective, a required format, tool/source guidance, and boundaries. A `SpawnPayload` whose `contract.objective` is empty is **rejected as a tool result** (`spawn denied: missing objective/output-contract`), not silently accepted.
- **Narrowed seed = hygiene + firewall.** The parent passes only the slices it chooses. This keeps the child's window clean (notes §5: subagents are themselves a context lever) **and** is the injection firewall (assessment §2.11): an untrusted-content reader child receives only what it is told and returns a distilled summary, so raw untrusted content never crosses back into a parent that holds sensitive/egress tools. Passing the full transcript would defeat both — so the protocol has no field for it.
- **Scope narrows monotonically.** `scope.allow` must be a subset of the caller's own granted scope; the chokepoint intersects and rejects any tool the parent does not itself hold (§3.5). Capability never widens down the tree.
- **Limits bounded by remaining tree budget.** `max_tokens`/`deadline` are clamped at the chokepoint to what the tree budget can still afford (RFC 0003 owns the accounting; this RFC owns the clamp at grant time).

### 3.3 Supervisor-minted depth & identity (the non-negotiable trust boundary)

`depth`, `agent_id`, `agent_path`, and the parent edge are **minted by the supervisor from the caller's supervision record**, never read from the child's request:

```rust
// inside the subagent.spawn handler, running in the supervisor
fn handle_spawn(caller: Handle, req: SpawnRequest) -> ToolResult {
    let parent = self.tree.get(caller);              // authoritative record
    let depth  = parent.depth + 1;                   // MINTED here
    // req.depth / req.agent_path / req.parent (if present) are DISCARDED.
    ...
}
```

A child cannot lie about its depth because it does not mint the value (notes-review-reliability §7). This is what makes the depth cap unforgeable: the only way to get a higher depth is to actually be deeper in a supervisor-tracked tree.

### 3.4 The distilled, structured result

The child runs to a terminal status (RFC 0007's state machine) and returns a **result** as the final upward control frame:

```rust
struct SubagentResult {
    status: TerminalStatus,        // the RFC 0007 §3.4 closed enum (the authority):
                                   // completed | refused | exhausted_steps | exhausted_tokens
                                   // | deadline | stalled | loop_detected | cancelled | crashed
    /// The distillate: ~1–2k tokens, satisfying contract.output_format.
    /// This is what the parent appends to its OWN context — never the
    /// child's transcript.
    distillate: ResultBody,
    /// Accounting the supervisor folds into node + tree-root counters (RFC 0003).
    usage: Usage,                  // { tokens_in, tokens_out, steps }
}

enum ResultBody {
    Inline(serde_json::Value),     // small structured/text result
    /// Store-and-reference for large outputs: the child wrote the bulk to a
    /// resource and returns a handle the parent reads on demand.
    Reference { uri: String, summary: String },
}
```

**Store-and-reference (notes §6.2).** When the distillate would exceed the ~1–2k budget, the child writes the bulk to a resource via a scoped MCP tool (or an `agentd://` self-resource) and returns `ResultBody::Reference { uri, summary }`. The parent appends the `summary` and reads `uri` only if it needs the bulk — keeping the coordinator's window lean.

**The parent appends the distillate, not the transcript.** This is enforced structurally: the control channel only ever carries `SubagentResult` upward; the child's transcript never leaves the child process. There is no protocol path for a raw transcript to flow up.

If the result frame's `distillate` fails to satisfy a JSON `output_format` schema, the supervisor surfaces the schema-validation failure to the parent **inside** the tool result (so the parent's model can adapt), with `status` preserved — it does not crash the parent.

### 3.5 Tool-scope narrowing & the Rule-of-Two check at grant time

At the chokepoint the supervisor computes the child's effective scope as an **intersection**, never a union:

```rust
fn narrow_scope(parent: &ToolScope, requested: &ToolScope) -> Result<ToolScope, Refusal> {
    // every requested tool must be one the PARENT itself holds
    for t in &requested.allow {
        if !parent.allow.contains(t) {
            return Err(Refusal::ScopeWiden(t.clone()));   // refused as tool result
        }
    }
    let effective = requested;        // already a subset
    Ok(effective.clone())
}
```

Because the granted set can only narrow, capability is monotone down the tree (notes §6.1). The chokepoint also evaluates the **Rule-of-Two trust budget** (RFC 0012 owns the policy; this RFC owns the evaluation point): if the requested scope hands one child all three trifecta legs (`untrusted_input` + `sensitive` + `egress`) the spawn is **warned or refused** unless `--allow-trifecta` is set. Refusal is a tool result, never a crash. This RFC defers the exact tag taxonomy and override semantics to RFC 0012; it only fixes *that the check happens at spawn grant*.

### 3.6 Caps at the chokepoint (refused as tool results)

`subagent.spawn` is served **by the supervisor** (it owns the process table), so it is the single place every cap is enforced. The caps and their conservative defaults:

| Cap | Default | Scope | Action on violation |
|---|---|---|---|
| `max_depth` | 4 (range 3–5) | tree | refuse: `spawn denied: max_depth N reached` |
| `max_children` | 8 | per node | refuse: `spawn denied: node child cap reached` |
| `max_total_subagents` | 64 | tree-wide | refuse: `spawn denied: tree subagent cap reached` |
| spawn-rate | token bucket: 8 burst, 2/s refill | tree-wide | refuse: `spawn denied: spawn rate exceeded` |
| tree-token ceiling | from `--tree-tokens` | tree-wide | refuse new spawns + new model calls; drain & quiesce |

Enforcement is a guard at the top of the handler:

```rust
fn handle_spawn(caller: Handle, req: SpawnRequest) -> ToolResult {
    if self.tree.draining { return tool_err("spawn denied: tree draining"); }
    let parent = self.tree.get(caller);
    let depth = parent.depth + 1;
    if depth > self.caps.max_depth { return tool_err("spawn denied: max_depth"); }
    if parent.children.len() as u16 >= parent.limits.max_children {
        return tool_err("spawn denied: node child cap");
    }
    if self.tree.total >= self.caps.max_total_subagents {
        return tool_err("spawn denied: tree subagent cap");
    }
    if !self.spawn_bucket.try_take() {
        return tool_err("spawn denied: spawn rate exceeded");
    }
    if self.tree.root_tokens >= self.caps.tree_token_ceiling {
        return tool_err("spawn denied: tree token ceiling");
    }
    // ... mint depth/identity, clamp limits, narrow scope, re-exec ...
}
```

**`tool_err` returns a normal MCP tool result with `isError:true`** (assessment §1.3 #7 — the `isError` channel), so the parent's model *sees* the refusal as an observation and adapts (notes §6.4). It is **never** a JSON-RPC protocol error and **never** a crash. A wedged or runaway child that keeps hammering `subagent.spawn` therefore just keeps getting refusals — it cannot fork-bomb, because the only spawn path is this chokepoint (notes-review-reliability §7: the enforcement point is structural and unforgeable). The token-bucket catches a fast churn loop that stays under the absolute count.

The **tree-wide draining flag** makes `subagent.spawn` error during teardown (assessment §2.8, RFC 0003) so a parent cannot spawn replacements mid-kill-ladder.

### 3.7 Sync / async / detach dispositions

`subagent.spawn` takes a disposition. **v1 implements only sync.**

```rust
enum Disposition { Sync, Async, Detach }   // default = Sync
```

- **`Sync` (default, v1).** The handler blocks the parent's *tool call* until the child reaches a terminal status, then returns `SubagentResult` as the tool result. The parent's loop is between turns, so the parent process is cheaply paused — no orphan management, deterministic mental model (notes §6.3; Anthropic's lead-agent default). Concretely: the supervisor does not return the JSON-RPC response for the `subagent.spawn` request until the child's result frame arrives (or the child is killed, in which case it returns the terminal status that the kill produced, e.g. `deadline`/`cancelled`/`crashed`).

- **`Async` (M3).** Returns a **handle** immediately: `{ handle, resource_uri }`. The parent keeps reasoning and later calls `subagent.status(handle)` / `subagent.await(handle)`, **or** subscribes to `resource_uri` — the child's completion *is* an `agentd://` resource update the parent reacts to (closing the loop with the reactive router, RFC 0008). Bounded by the route/parent `max_inflight` (default 4).

- **`Detach` (M3).** Fire-and-forget: returns a handle, the parent does not await. The child **still** counts against the tree budget, the depth cap, and the breadth/rate caps, and is **still reaped** by the supervisor (notes §6.3). It reports to logs and an `agentd://` resource. Use sparingly.

**Streaming partials into the parent's reasoning is out of scope for v1** (notes §6.3): it complicates the parent's context management. The child always streams loop events up the control channel for *observability and supervision* (RFC 0003/0005), but those partials do not enter the parent's context — only the final distillate does. The async-handle + await/subscribe covers the real fan-out need.

The v1 sync-only stance is deliberate (notes §11.3, assessment §2.7): async shares the subscribe/notify machinery, so it lands in **M3** with reactivity, not before.

### 3.8 `exec` children folded into the same regime

The gated `exec` self-tool (RFC 0012, off by default) spawns a child **outside** the subagent control protocol — it has no control channel. It is nonetheless folded into the same supervision regime (assessment §2.7; notes-review-reliability §12), so it is not a fork-bomb bypass around the chokepoint:

- **Routed through the chokepoint's caps.** An `exec` invocation is counted against the subtree budget and the breadth/rate caps exactly like a spawn. It is created via the same supervisor-owned path (the `exec` self-tool handler calls into the same spawn-accounting as `subagent.spawn`).
- **Mandatory deadline + process-group kill.** Each `exec` child gets its own process group (`setpgid`) and a mandatory finite deadline; it is reaped by the §3-of-RFC-0003 kill ladder. Reference implementation: the retired `tools/shell.rs::run()` (reader-threads + `try_wait` + timeout-kill + signal-extract).
- **Only the deadline + kill detectors apply.** With no control channel there is no ping/pong and no loop-event stream, so Detectors B (no-progress) and C (ping/pong) of the three-detector model are inapplicable. Only Detector A (hard deadline) and the kill ladder bound an `exec` child (notes-review-reliability §12). This is stated explicitly so operators understand an `exec` child cannot be distinguished "busy vs wedged" — it is bounded purely by its deadline.

Because `exec` is the strongest trifecta leg, the security posture (RFC 0012) recommends an `exec`-scoped subagent be the one least exposed to untrusted content. This RFC only fixes that `exec` is **inside** the same caps/kill/budget regime.

---

## 4. Interactions with other RFCs

- **RFC 0001 (core architecture).** The two-loop split is the premise: the loop lives only in subagent processes, the supervisor mints and supervises. This RFC fills in the spawn boundary RFC 0001 left as a slogan.
- **RFC 0002 (reactor & concurrency).** The supervisor serves `subagent.spawn` from its single reactor thread; the child's control-out fd becomes a reader-thread feeding the merged `mpsc`. The sync disposition's "block the tool call until result" is implemented as the reactor holding the pending JSON-RPC response, not blocking the reactor thread.
- **RFC 0003 (supervision, dead/stuck, recovery).** Owns the process-tree mechanics this RFC depends on: `setpgid`, `PR_SET_PDEATHSIG` (armed in subagent mode per §3.1), `PR_SET_CHILD_SUBREAPER`, the three-detector model, the bounded kill ladder, the restart governor, and the **hierarchical token accounting** that the chokepoint's tree-token ceiling and per-child limit clamp read from. This RFC owns the *grant-time policy*; RFC 0003 owns the *runtime enforcement & teardown*.
- **RFC 0005 (self-MCP server & control protocol).** Owns the `subagent.spawn/send/cancel/status` tool surface and the length-framed JSON-RPC control wire that carries `SpawnPayload` down and `SubagentResult` up. This RFC defines the *payload semantics*; RFC 0005 defines the *wire and tool registration*.
- **RFC 0007 (agentic loop & terminal-status state machine).** Owns the `TerminalStatus` enum referenced in `SubagentResult` and the inner-loop stop conditions. The output contract here becomes the loop's system/instruction assembly; the per-tool repeat cap and `stalled` detector live there.
- **RFC 0008 (execution modes & reactive routing).** Async/detach completion-as-resource (M3) plugs into the routing rule (a child's completion is a resource update; self-subscribe = self-scheduling). `max_inflight` for async children is the same backpressure knob as a spawn route's.
- **RFC 0010 (observability).** The `Telemetry` block in the payload is RFC 0010's correlation tuple; each child self-logs pre-correlated by `run_id` + `agent_path`. `--aggregate-logs` forwards child telemetry up the control channel.
- **RFC 0012 (security posture).** Owns the Rule-of-Two tag taxonomy and `--allow-trifecta` override evaluated at §3.5, the gated `exec` tool folded in §3.8, and the secrets-handling that keeps the spawn payload off argv (§3.1).
- **RFC 0011 (cloud-native contract).** The terminal status of the root subagent maps to the process exit code (completed→0, refused→5, partial→3, budget→7).

---

## 5. Non-goals / Deferred

- **Async & detach dispositions are deferred to M3** (assessment §2.7). v1 ships **sync-only**. The `Disposition` enum and `subagent.status`/`await` tools are reserved but not implemented in v1.
- **Streaming partial results into the parent's reasoning** is out of scope for v1 (notes §6.3); only the final distillate enters the parent's context.
- **Supervisor-crash session checkpointing** (serializing the spawn-payload map + routing table to disk) is a deferred v2 extension (RFC 0013, assessment §2.8). v1 retains the spawn payload in supervisor memory only as the minimum recoverable unit for bounded restart; warm sessions are lost on supervisor crash and recovered by idempotent re-trigger.
- **Aggregate subtree *memory* enforcement** is explicitly *not* in-binary (assessment §2.8): only the *token* ceiling and per-child `RLIMIT_AS`/`RLIMIT_CPU` are. Aggregate memory needs cgroups v2 (deployment layer, RFC 0003/0011). Stated here so the caps table is not read as a memory guarantee.
- **A second spawn path.** There is exactly one: the supervisor-owned `subagent.spawn`. No fork-from-within-the-loop, no model-controlled `Command`, no spawning from a tool result. This is the unforgeable chokepoint and is a non-goal to ever add another.

---

## 6. Open items

- **Default numeric knobs require empirical tuning** (notes §11.1): `max_depth` (3–5, default 4), `max_children` (8), `max_total_subagents` (64), spawn-bucket (8 burst / 2 per s), per-subagent `max_steps` (30–50). Shipped as conservative starting defaults, all overridable; none are unbounded.
- **`output_format` schema strictness.** Whether a JSON-schema-mismatched distillate is returned to the parent annotated-but-intact (current decision, §3.4) versus rejected and retried once by the child is left to RFC 0007's loop policy; this RFC fixes only that a mismatch never crashes the parent.
