# Research Notes: Modern Agent-Loop Engineering (2024–2026) for a Minimal Rust MCP-Native Agent

**Status:** Research artifact (durable). Feeds RFC 0001 (`agentd` — minimal, MCP-native, reactive agent runtime).
**Author:** research subagent.
**Date:** 2026-06-25.
**Scope:** Pure web research distilled into concrete design implications for `agentd`. Every finding ends with **→ Implication for agentd**.

This document is the evidence base for the agentic-loop decisions in RFC 0001. It is deliberately opinionated about what to *adopt*, what to *skip* (per the minimalism bar, §12 of the RFC), and what to *defer*. A full bibliography with URLs is at the end.

---

## 0. How `agentd`'s design maps to the literature (orientation)

`agentd` already commits to several positions that the modern literature endorses:

- **A plain ReAct-style tool-use loop** (RFC §6.1) is exactly what Anthropic calls the "agent" pattern and recommends keeping simple. The literature's strong message — *the loop is ordinary; the value is in what surrounds it* — matches the RFC's own framing.
- **Process-isolated subagents with their own context windows** (RFC §4.2, §6.3) is the orchestrator-worker pattern, and the "separate context window per subagent" is independently the #1 cited reason multi-agent systems work.
- **Capability scoping by granted MCP subset** (RFC §6.3, §13) is a structurally sound answer to the single most important *unsolved* agent-security problem (prompt injection / the lethal trifecta), provided the scope grant is interpreted as a *trust budget*, not just a capability list.

The research below sharpens each of these and flags the places where the current RFC is under-specified (notably: termination/goal-checking, context compaction for warm reactive sessions, tool-error recovery semantics, and what to log).

---

## 1. Anthropic on building effective agents & the agent loop

**Primary source:** Anthropic, *Building Effective Agents* (engineering blog).

Key claims:

- **Workflows vs agents.** Workflows orchestrate LLMs through *predefined code paths*; agents are systems where *the LLM dynamically directs its own process and tool usage*. `agentd` is squarely an *agent* (it retired the workflow-DAG design). The RFC's "control flow is the model's, bounded by budgets" is the textbook agent definition.
- **The loop is simple.** "[Agents] are typically just LLMs using tools based on environmental feedback in a loop." The implementation is "often straightforward." Anthropic explicitly warns against over-engineering: *"find the simplest solution possible, and only increase complexity when needed."* Sometimes the right answer is *not* an agent at all.
- **Stopping conditions are a first-class concern.** Agents should run "until task completion or stopping conditions are met," and you should include "stopping conditions (such as a maximum number of iterations) to maintain control." This is a design requirement, not an afterthought.
- **Ground truth from the environment each step.** The agent should gain "ground truth from the environment at each step" (tool results, test results) and pause at checkpoints or when blocked.
- **Three implementation principles:** (1) keep the design simple, (2) prioritize transparency by *explicitly showing the agent's planning steps*, (3) carefully craft and test the tool/agent-computer interface (ACI).
- **Reduce framework abstraction in production.** "Don't hesitate to reduce abstraction layers and build with basic components as you move to production." A from-scratch Rust runtime with no agent framework is the endorsed direction.

**Building blocks Anthropic enumerates** (workflow patterns, listed because they bound what `agentd` deliberately does *not* build into core): augmented LLM, prompt chaining, routing, parallelization (sectioning + voting), orchestrator-workers, evaluator-optimizer.

→ **Implication for agentd:**
1. The RFC's "intentionally ordinary ReAct loop" is correct and validated; do not add planner/critic machinery to the core loop. Any of the workflow patterns (routing, evaluator-optimizer, voting) that a deployment wants can be expressed by *the model spawning subagents*, not by runtime features. Keep them out of the binary.
2. **Make stopping conditions an explicit, enumerated subsystem**, not just incidental budget checks. The RFC has step/token/deadline/cancel; add an *explicit completion signal* path (see §6) so "done" is a represented state, not merely "model stopped emitting tool calls."
3. **Transparency is a runtime obligation, not a nicety.** The RFC's per-turn event streaming (thought / tool-call / tool-result / final up the control channel) directly implements Anthropic's "explicitly show planning steps." Keep it; it doubles as observability (§9).

---

## 2. Orchestrator-worker / subagent patterns and their pitfalls

**Primary source:** Anthropic, *How we built our multi-agent research system*. This is the single most relevant external document to RFC §4/§6.

### 2.1 What works

- **Orchestrator-worker, parallel subagents, each with its own context window.** "A lead agent coordinates the process while delegating to specialized subagents that operate in parallel." Separate context windows give **compression**: subagents "explore different aspects... before condensing the most important tokens for the lead." A subagent may burn tens of thousands of tokens but **return only a 1,000–2,000-token distilled summary**.
- **Performance, when it fits the task.** Multi-agent (Opus lead + Sonnet subagents) beat single-agent Opus by **90.2%** on their internal research eval. Multi-agent excels at "valuable tasks that involve heavy parallelization, information that exceeds single context windows, and interfacing with numerous complex tools" — i.e. **breadth-first** problems with independent sub-directions.
- **Parallelism is a big win for latency.** Lead spins up 3–5 subagents in parallel (not serially); subagents call 3+ tools in parallel → **up to 90% reduction** in research time for complex queries.

### 2.2 The pitfalls (these are the load-bearing findings)

- **Cost: the 15× tax.** "Agents typically use about 4× more tokens than chat interactions, and multi-agent systems use about 15× more tokens than chats." **Token usage alone explained ~80% of the variance** in eval performance. Therefore multi-agent is only economically justified for **high-value tasks**. Anthropic is explicit: "most coding tasks involve fewer truly parallelizable tasks than research" → multi-agent is a poor fit when agents must share context or have many inter-dependencies.
- **Vague delegation fails.** Short instructions like *"research the semiconductor shortage"* were misinterpreted; subagents duplicated work or left gaps. Each subagent needs **an objective, an output format, guidance on tools/sources, and clear task boundaries.**
- **Effort-scaling must be taught.** Without guidance, early agents "spawn[ed] 50 subagents for simple queries." They embedded explicit rules: simple fact-find = 1 agent / 3–10 tool calls; comparison = 2–4 subagents / 10–15 calls each.
- **Synchronous orchestration bottleneck.** Their lead waits for each subagent batch to finish before proceeding — simpler coordination, but a real throughput bottleneck and a single point of stall.
- **Aggregation should bypass the coordinator.** "Rather than requiring subagents to communicate everything through the lead agent, implement artifact systems where specialized agents can create outputs that persist independently." Funneling all results back through the orchestrator's context wastes tokens and creates a bottleneck.
- **Coordination failure modes:** continuing when results already suffice; overly verbose queries; wrong-tool selection; agents "distracting each other with excessive updates."

→ **Implication for agentd:**
1. **The spawn payload schema must be rich, not minimal.** RFC §4.2/§8 lists `instruction, context, tool_scope, limits`. Add (or make first-class within the instruction contract) an explicit **output contract**: objective + required output format/shape + tool/source guidance + boundaries. A bare instruction string for `subagent.spawn` will reproduce Anthropic's vague-delegation failure. This is a *protocol* decision (the self-MCP `subagent.spawn` tool description), and it costs zero binary weight.
2. **Effort budgets are not optional.** The RFC already has per-subagent step/token/deadline limits and a tree-wide token ceiling + max depth (§6.3, §10, §14.7). Keep them, default them *conservatively*, and surface them in the `subagent.spawn` tool schema so the parent model is forced to choose an effort tier. The 15× cost finding is the justification for the RFC's "budgets bound a model-owned loop" stance — make the budgets visible to the model, not just enforced silently.
3. **Lean into the breadth-first sweet spot; warn against the dependency case.** Document (in the self-MCP `subagent.spawn` description and in user docs) that subagents pay a large token tax and are worth it for *parallel, independent* sub-tasks — not for serial/dependent work that one agent should just do in one context. This is guidance encoded in a tool description, exactly as Anthropic did.
4. **Result aggregation via resources, not via the control channel transcript.** This is a *perfect* fit for RFC §8's "resources the self-MCP exposes (subagent outputs as readable + subscribable resources)." Design subagent results as **artifacts/resources the parent reads on demand**, rather than dumping the full child transcript into the parent's context. The parent should receive a condensed summary + a resource handle, mirroring the 1–2k-token distilled return. This both saves tokens and is the agent-to-agent reactivity mechanism the RFC already wants.
5. **Synchronous-vs-async spawn is a real design fork (RFC §14.5 routing).** The supervisor is process-based and can naturally run children concurrently. Prefer **async/concurrent subagent execution with the parent able to continue or suspend** (the RFC's reactive "continue a warm session on a resource update" mechanism is the right primitive: a parent subscribes to its children's output resources and is re-entered when they complete, instead of blocking). This sidesteps Anthropic's synchronous bottleneck *using machinery the RFC already specifies.*
6. **Depth/breadth caps prevent the "50 subagents" failure structurally.** RFC §14.7 — set conservative defaults (e.g. max depth 3–5, a hard tree-wide token ceiling). The OS-process model means a runaway fan-out also consumes PIDs/memory; the tree-wide budget must be enforced by the *supervisor*, which is the only component with the whole-tree view.

---

## 3. ReAct vs Plan-and-Execute vs ReWOO vs Reflexion

**Sources:** ReWOO paper (arXiv 2305.18323); Reflexion replication discussion; the AI Engineer "4 single-agent patterns" comparison.

| Pattern | Core idea | Strength | Weakness | Token cost |
|---|---|---|---|---|
| **ReAct** | Thought→Action→Observation loop, one step at a time | Maximally adaptive; reacts to each observation; robust to surprises | "Short-term thinking" — no holistic plan; can take inefficient paths; repeated prompt overhead | High (re-sends growing context every turn) |
| **Plan-and-Execute** | Plan whole strategy first, then execute steps | Good for multi-step dependent tasks; fewer LLM calls; visible plan | Rigid; needs explicit *replanning* when reality diverges from plan | Medium |
| **ReWOO** | Produce full tool-use plan with `#E1/#E2` placeholders, execute without re-prompting per step | **~64% fewer tokens, +4.4% accuracy** across 6 benchmarks; decouples reasoning from observation | Brittle if early results invalidate the plan; no mid-course adaptation | Low |
| **Reflexion** | Agent self-critiques after failures, retries with reflection in context | Helps when there's an automated success signal (code, math, extraction) | **Self-reinforcing blind spots**: same model writes output *and* critique → 2025 replications show it "repeats earlier misconceptions"; weak without external ground truth | High (multiple attempts) |

Key cross-cutting findings:
- ReAct's adaptivity is the safe default for **open-ended, uncertain** tasks (Anthropic's exact "use an agent" criterion). ReWOO's efficiency wins assume a *predictable* tool chain.
- **Reflexion's failure is instructive:** self-critique without *external* ground truth tends to reinforce errors. Verification works best when grounded in the *environment* (tool/test results), not in the model judging itself.

→ **Implication for agentd:**
1. **Keep ReAct as the in-core loop (RFC §6.1) — correct and confirmed.** It matches `agentd`'s open-ended, MCP-tool-driven use case. Do **not** bake Plan-Execute, ReWOO, or Reflexion into the binary.
2. **Let plan-execute / ReWOO emerge from the model + subagents, not from runtime code.** A capable model can emit a plan and then issue parallel tool calls within a single ReAct loop, or spawn subagents per plan-step. The token-efficiency of ReWOO is available to instructions/prompts that ask for it; it should not be a runtime mode. This preserves the minimalism bar.
3. **Verification must be environment-grounded, never pure self-critique.** When designing the loop's "is it done / did that work?" checks (§6 below), prefer **tool/exec results, MCP resource state, and external evaluators** over asking the same model to grade itself. The RFC's exec tool and MCP resource reads are the ground-truth sources. This is the concrete lesson from Reflexion's failure mode.
4. **Token cost is the dominant lever** (echoing §2's 80%-of-variance finding). The choice of pattern is largely a token-economics choice. `agentd`'s budgets are therefore the right control surface; the model picks the strategy, the budgets bound the bill.

---

## 4. Context engineering / context-window management & compaction

**Primary source:** Anthropic, *Effective context engineering for AI agents*. Plus the long-running-harness and Agent-SDK posts.

Key claims:

- **Context engineering > prompt engineering for multi-turn agents.** It is "the set of strategies for curating and maintaining the optimal set of tokens during inference." The goal: *"find the smallest set of high-signal tokens that maximize the likelihood of some desired outcome."*
- **Context rot is real and architectural.** As tokens grow, recall degrades; transformer attention spreads over n² pairwise relations, so there's "a natural tension between context size and attention focus." Bigger context ≠ better; it can be worse.
- **Just-in-time retrieval beats pre-loading.** Keep **lightweight identifiers** (file paths, queries, URIs) and load data into context *at runtime* via tools, rather than stuffing everything up front. Claude Code's hybrid: drop `CLAUDE.md` up front, but use `glob`/`grep` to pull files just-in-time.
- **Compaction.** When nearing the window limit, **summarize the conversation and reinitialize a fresh window with the summary.** Tactic: *"maximize recall first... then iterate to improve precision by eliminating superfluous content."* The **safest, lightest compaction is tool-result clearing** — drop raw tool outputs from deep history once they've been consumed.
- **Structured note-taking / external memory.** Have the agent **write notes to durable memory outside the context window** (e.g. a to-do list / progress file) and pull them back later. The long-running-harness post shows this concretely: `claude-progress.txt`, a JSON feature list (agents flip only a `passes` field), and git history as recovery points — because *"each new session begins with no memory of what came before."*
- **Sub-agent isolation as context management.** Subagents do deep work in clean windows and return condensed summaries (the §2 mechanism, viewed as a context tactic).
- **System-prompt "altitude" (Goldilocks).** Be "specific enough to guide behavior... yet flexible enough to provide strong heuristics." Avoid both brittle if-else hardcoding and vague hand-waving.
- **Tool sets must be small and unambiguous.** "If a human engineer can't definitively say which tool should be used in a given situation, an AI agent can't be expected to do better." Avoid bloated tool sets with overlapping functions.

→ **Implication for agentd:**
1. **Reactive *warm sessions* (RFC §5.3) make context management unavoidable.** A long-lived agent that "wakes up" repeatedly on resource updates will accumulate context across many wake-ups. The RFC currently treats warm sessions as in-memory message accumulation (§14.3) without a compaction story. **Add a compaction step to the agentic loop**: when a session's context approaches the model's window, summarize-and-reinitialize. Start with the *cheapest* form — **tool-result clearing** (drop raw MCP tool-call outputs from old turns, keep their distilled effect) — before full summarization. This is a small amount of code and directly addresses context rot for the signature mode.
2. **MCP resources are the native just-in-time substrate.** `agentd` should **not** pre-read all subscribed/available resources into context. It should keep **resource URIs as lightweight handles** and read them on demand via `resources/read` when the model asks. This is exactly Anthropic's just-in-time pattern and it's free — MCP already provides `resources/list` (handles) separate from `resources/read` (content). Make "list = cheap handles in context, read = on-demand" an explicit invariant.
3. **External memory via MCP, not a built-in store.** The note-taking pattern (progress files, to-do lists) should be realized through an MCP filesystem/memory server or the gated `exec` tool — *not* a built-in memory subsystem (minimalism bar). For warm reactive sessions, a compaction summary written to a resource lets a session survive even a process restart (relevant to RFC §14.3 "session durability": checkpoint = write the compacted summary to a resource; restore = read it back).
4. **System prompt: ship a small, heuristic, "medium-altitude" base prompt** and let the user `INSTRUCTION` carry specifics. Resist the temptation to encode elaborate rules in core; encode *heuristics* (e.g. "prefer specialized tools; start broad then narrow; verify with the environment before declaring done").
5. **Tool-scope curation is a context-engineering act, not only a security act.** The parent's `tool_scope` grant (RFC §6.3) doubles as context hygiene: a subagent given a *small, relevant* tool subset both is safer (lethal trifecta, §7) *and* reasons better (no ambiguous tool choices, smaller catalogue in context). Frame `tool_scope` in docs as serving both goals.
6. **Distilled returns from subagents are the compaction boundary between agents.** Enforce/encourage that `subagent` results are summaries + resource handles (§2.4), so a parent never inherits a child's raw context. This keeps the tree's aggregate context-in-window bounded.

---

## 5. The agent loop / harness: gather context → take action → verify work

**Primary sources:** Anthropic, *Building agents with the Claude Agent SDK*; *Effective harnesses for long-running agents*.

- **Canonical loop:** **gather context → take action → verify work → repeat.** Verification is a *named stage*, not implicit. Self-correction comes from the agent checking its own output against ground truth before finishing.
- **Verification, in priority order:**
  1. **Rule-based feedback (best):** "clearly defined rules for an output, then explaining which rules failed and why" (e.g. a linter, a type check, a test runner). Deterministic, cheap, trustworthy.
  2. **Visual/structured feedback:** screenshots, structured diffs.
  3. **LLM-as-judge (last resort):** "generally not a very robust method" due to latency/cost and the self-grading blind spot — use only for fuzzy criteria.
- **Action substrate:** *tools* are the primary, prominent actions; *bash/scripts* give flexible access; *generated code* is preferred for "complex, reusable operations" because "code is precise, composable, and infinitely reusable"; *MCP* provides standardized external integrations with auth/API handled.
- **Long-running harness lessons (very relevant to reactive/loop modes):**
  - "Each new session begins with no memory of what came before" → bridge via **durable state** (progress file + structured task list + git/VCS checkpoints).
  - **Compaction alone is insufficient**; pair it with **structural decomposition** (work feature-by-feature) so the agent doesn't "try to do too much at once."
  - **Premature completion is a real failure**: agents "mark a feature complete without proper testing." Counter with **explicit completion criteria** (a feature list / checklist the agent must satisfy and must *not* be allowed to edit away) and **mandatory verification before declaring done.**
  - **Diagnose failures by root cause:** missing info → improve search/tools; repeated errors → add formal rules; limited problem-solving → give better/creative tools; high variance → build a representative eval set.

→ **Implication for agentd:**
1. **Adopt the three-phase loop explicitly in the subagent loop (RFC §6.1).** The RFC's loop is "build request → call intelligence → execute tools → loop." Reframe/annotate it as **gather (context, incl. on-demand resource reads) → act (MCP tool calls / exec) → verify → repeat.** The *verify* phase is currently missing as a named concept. It need not be runtime code — it's a loop-prompt + a place in the event stream — but it should exist so "done" is earned, not assumed.
2. **Ground verification in MCP/exec, not self-judgment.** Prefer rule-based ground truth: MCP tool results, resource state checks, and the gated `exec` tool (run a test, a linter, a build). This matches both Anthropic's verification ranking and the Reflexion lesson (§3.3). LLM-as-judge stays out of core; if a deployment wants it, it's an MCP server.
3. **Completion detection is the RFC's biggest under-specification.** The loop terminates on "final / step cap / token budget / deadline / cancel" (§6.1). "Final" is currently "model emitted a final message." Strengthen it: a run should produce a **structured result** (see §8) and, where the instruction defines success criteria, the loop should **verify against them before emitting final.** Add a guard against premature completion for long-running/reactive sessions (e.g. an explicit "are all stated objectives satisfied?" check). This is the analogue of Anthropic's "don't let the agent declare done without testing."
4. **Durable state for warm/long-running sessions** (ties to §4.3 and RFC §14.3): keep a compacted summary + an explicit task/objective list as a resource the session re-reads on each wake-up. This is the "bridge the gap between sessions" mechanism, expressed in MCP terms, and it's what makes the reactive mode robust to process restarts.
5. **`exec` is the verification workhorse** — another argument for the RFC's gated `exec` tool (§9): tests/linters/builds are how an agent gets cheap, deterministic ground truth. Keep it off by default, but recognize its role in self-correction.

---

## 6. Tool-use error recovery, self-correction, and when to stop

**Sources:** practitioner write-ups on agent error handling (Fast.io, SparkCo), the ERR-measure paper (arXiv 2601.22352), and the IDC cost survey cited widely in 2025.

- **Malformed tool calls are the common case.** LLMs frequently emit invalid JSON / wrong arguments. The robust pattern: a **validator step that feeds the error back to the model as a new observation** ("your call failed schema X because Y; correct it"). This "resolves most format errors on the first retry." Do **not** crash on a bad tool call — turn it into an observation.
- **Graceful tool-failure degradation.** Anthropic (multi-agent post): "letting the agent know when a tool is failing and letting it adapt works surprisingly well," combined with **deterministic safeguards** (retry logic, regular checkpoints). Tool errors should be returned to the loop as observations the model can react to, *and* wrapped in bounded retry/backoff at the transport layer.
- **Recovery patterns catalog** (commonly cited 7): retries (exponential backoff + jitter), circuit breakers, validation gates, sagas/compensation, checkpoints, **budget guardrails**, human escalation. Advanced systems enumerate explicit recovery actions: Retry / Skip / Replan / Substitute-Tool / Escalate / Regenerate-prior-step.
- **Runaway loops are the #1 cost incident.** A 2025 IDC survey: **92% of orgs using agentic AI reported higher-than-expected costs, with runaway loops the named main cause.** Budget guardrails are not optional.
- **Resumability over restart** (Anthropic multi-agent): "restarts are expensive... we built systems that can resume from where the agent was." Pair AI adaptability with **deterministic checkpoints + retry**.
- **Stuck/loop detection.** Beyond hard budgets, watch for *no-progress* signatures: repeated identical tool calls, oscillation between two states, tool-error loops. The supervisor (whole-tree view) is well-placed to detect these.

→ **Implication for agentd:**
1. **Tool errors are observations, not crashes — make this a loop invariant.** When an MCP `tools/call` returns an error (or the model emits an unparseable/invalid call), the subagent loop must **append a structured error observation and continue** (within budget), not abort. This is a small, high-value piece of loop logic. The model self-corrects on the next turn. This belongs in §6.1's "execute, append result to context."
2. **Two layers of retry, clearly separated:**
   - **Transport layer (deterministic):** bounded retry with exponential backoff + jitter for *transient* failures (MCP server stdio hiccup, HTTP 5xx/timeout to intelligence or HTTP-MCP). This is the supervisor/client's job and is invisible to the model.
   - **Semantic layer (model-driven):** persistent/logical failures become observations the model reasons about (substitute tool, replan, give up gracefully). Don't retry these deterministically in a loop — that *is* the runaway-loop failure.
3. **Budget guardrails are the headline reliability feature.** The RFC's step/token/deadline/tree-budget limits (§10, §13) are directly validated by the 92%-cost-overrun / runaway-loop finding. Treat them as a *hard* safety system: enforced by the supervisor, conservative defaults, and **observable** (emit a clear "budget exhausted" terminal event distinct from "completed"). Different exit codes for *completed* vs *budget-exhausted* vs *error* (RFC §11 "meaningful exit codes").
4. **Add explicit stuck/no-progress detection in the supervisor.** Because subagents stream every turn's events up the control channel (RFC §6.1, §6.2) and the RFC already requires "detect dead/stuck subprocesses" (hard requirement #8), the supervisor can cheaply detect: (a) **liveness** — a child that stops emitting events / stops consuming input within a heartbeat window → presume stuck → `SIGKILL` the subtree; (b) **progress** — repeated-identical-tool-call or error-loop signatures → cancel or surface. This is the concrete realization of the RFC's "detect dead/stuck subprocesses, recover state" requirement. Recommend a **heartbeat on the control channel** (a periodic liveness event from each subagent) so the supervisor can distinguish "thinking/long tool call" from "hung."
5. **Termination is a small explicit rule set, not emergent.** Enumerate terminal states: `completed` (final result, criteria verified) | `budget_exhausted` (step/token/deadline) | `cancelled` (parent/signal) | `failed` (unrecoverable error) | `killed` (supervisor liveness/limit). Each maps to a structured result + exit code/event. This makes "when to stop" auditable — the RFC's reliability goal.
6. **Checkpoint = write compacted state to a resource** (reuse §4/§5 mechanism). For reactive warm sessions this gives resumability without a bespoke durable-execution engine (keeps minimalism bar). v1 can be in-memory (RFC §14.3 bias); the resource-checkpoint is the clean later extension.

---

## 7. The lethal trifecta & prompt injection — for an agent whose tools are arbitrary MCP servers

This is the **most important security section** for `agentd`, because its entire value prop is connecting *arbitrary, possibly-untrusted* MCP servers, and MCP explicitly encourages mixing tools from many sources.

**Primary sources:** Simon Willison, *The lethal trifecta*; Meta, *Agents Rule of Two*; the MCP *Security Best Practices* spec page; MCP threat-modeling / tool-poisoning papers (arXiv 2603.22489, 2512.08290).

### 7.1 The lethal trifecta (Willison)

Any agent that simultaneously has **(1) access to private data, (2) exposure to untrusted content, and (3) the ability to communicate externally** can be turned into a data-exfiltration tool by a *single injected prompt*. The mechanism needs no malware: poisoned content steers the agent → agent reads sensitive data → agent ships it out. **MCP makes this worse by design** — it "encourages mixing tools from multiple sources," so one combined toolset can easily hold all three legs (e.g. a repo/email reader = private data + injection vector; an HTTP/PR/send tool = exfiltration channel).

**Prompt injection is unsolved and probably permanent.** LLMs "follow instructions in content" — trusted instructions and untrusted data arrive as the *same token stream*; the model has no reliable trust boundary. 95%-effective guardrails are *failures* in security terms. (2026 reporting frames it as a possibly-permanent architectural flaw, not a patchable bug.)

### 7.2 Meta's "Agents Rule of Two"

Until robust injection detection exists, an agent operating **without per-action human supervision should satisfy at most two of {processes untrusted input, has access to sensitive data/systems, can change state / communicate externally}** *within a session*. Satisfying all three requires a human in the loop or a hard preventative control (sandbox, restricted params, confirmation).

### 7.3 MCP-specific threats (spec + papers)

- **Tool poisoning / context poisoning** = indirect prompt injection via **tool/metadata fields**: malicious instructions hidden in a tool's *description*, schema, or returned content. OWASP Agentic Top-10 (2026) classes this as ASI01 *Agent Goal Hijack*. Empirically, **most MCP clients accept server-provided tool descriptions/metadata without validation** (5 of 7 tested). **Treat ALL server-provided content — tool/resource definitions, descriptions, prompts, elicitations, and tool results — as untrusted input.**
- **SSRF via OAuth/metadata URLs** (MCP spec): a malicious server can point discovery URLs at `169.254.169.254` (cloud metadata), private ranges, or localhost. Clients **SHOULD** enforce HTTPS, **block private/loopback/link-local IP ranges**, validate redirect targets, beware DNS-rebinding TOCTOU, and prefer egress proxies. (Relevant to RFC §7.2 `https://` intelligence and HTTP-transport MCP §7.1.)
- **Token passthrough is forbidden** (spec, MUST NOT): an MCP server must not accept tokens not issued *to it*. Relevant if/when `agentd`'s self-MCP (§8) takes auth.
- **Confused deputy** in OAuth proxy flows; **session hijacking** for stateful HTTP MCP (use non-deterministic session IDs, bind to user id, never use sessions for authn). Relevant when `agentd` serves its self-MCP over HTTP (§8) or speaks to HTTP MCP servers.
- **Local server compromise** (spec): a malicious stdio server config = arbitrary code execution at client privilege (e.g. a poisoned launch command exfiltrating `~/.ssh`). Clients **MUST** get explicit consent before executing local-server launch commands; prefer **stdio** (limits access to just the client), sandbox spawned servers, least privilege.

### 7.4 Mitigations that fit `agentd`'s design

- **Constrain untrusted input so it can't trigger consequential actions** (the only robust developer pattern Willison endorses), or use design patterns like **CaMeL** (DeepMind: separate a trusted "planner" path from untrusted data so data can parametrize but not author actions). The structural answer is **capability isolation**, which `agentd` already has.

→ **Implication for agentd:**
1. **`tool_scope` is the lethal-trifecta control, *if* it's interpreted as a trust budget.** RFC §6.3 scopes a subagent to a subset of MCP endpoints. Make this explicitly serve the **Rule of Two**: a subagent that reads untrusted content (e.g. web/email/issues) should **not** simultaneously be scoped to private-data tools *and* exfiltration tools. Recommend the supervisor/parent be able to **classify tools** (or accept declared tags: `untrusted_input` / `sensitive` / `egress`) and *warn or refuse* a scope grant that hands one subagent all three legs without an explicit override. This turns the existing scoping mechanism into a real injection defense at near-zero binary cost. **Document the Rule of Two as the recommended scoping discipline.**
2. **Process isolation is a structural mitigation — use it.** Splitting work across subagents with disjoint scopes is the CaMeL-style separation in practice: an "untrusted-content reader" subagent (no sensitive/egress tools) returns *distilled, structured* findings to a parent that *does* hold sensitive tools but never sees the raw untrusted content directly. The §2/§4 "subagent returns a 1–2k-token summary" pattern is *also* a prompt-injection firewall when the summary is structured/constrained. Call this out as a recommended deployment pattern.
3. **Treat every byte from an MCP server as untrusted — including tool descriptions.** Because the model ingests tool descriptions and tool results, and MCP encourages multi-server mixing, `agentd` must assume **tool poisoning**. Concretely: (a) do **not** auto-trust/auto-execute based on server-provided metadata; (b) consider surfacing/logging tool descriptions so an operator can audit them; (c) the *operator declares which servers exist* (RFC §7.1 "no discovery magic") — keep that — and document that adding an MCP server is a trust decision equivalent to running its code.
4. **SSRF defenses for HTTP transports (spec MUSTs/SHOULDs).** When `agentd` makes outbound HTTP (intelligence `https://`, HTTP-MCP, or following any server-provided URL): enforce HTTPS in production, **block RFC-1918 / loopback / link-local (169.254/16) ranges by default**, validate redirects (don't blindly follow cross-host — the RFC's WebFetch-style "return redirect to caller" is the right instinct), and pin DNS where feasible. The RFC's minimal hand-rolled HTTP client (§12) must include this; it's a few checks, not a library. Provide an explicit opt-out for localhost dev.
5. **`exec` off by default is correct and trifecta-aligned** (RFC §9). `exec` is a state-change/egress capability; gating it is exactly the Rule-of-Two posture. When on, it's the strongest leg of the trifecta — so a subagent with `exec` should be the one *least* exposed to untrusted content. Encode this as guidance.
6. **stdio as the default transport is a security win** (spec): stdio limits server access to just `agentd`; HTTP servers need auth/loopback restriction. The RFC's "stdio is default and lightest" (§7.1) aligns with the spec's security guidance — keep it.
7. **Local-server launch is code execution.** The RFC spawns stdio MCP servers from operator-declared launch commands. That's equivalent to running arbitrary code at `agentd`'s privilege (spec: local server compromise). Since `agentd` is operator-configured (not one-click-install), the consent burden is lower, but **document that an MCP server definition = trusting that command**, and never construct launch commands from model/server-controlled strings.
8. **Self-MCP over HTTP needs the spec's server hardening** (§8 `--serve-mcp`): non-deterministic session IDs, no sessions-as-authn, no token passthrough, loopback/auth restriction. For v1, prefer **unix/vsock** transports for the self-MCP (RFC already lists them) to avoid the HTTP attack surface entirely; HTTP is opt-in.
9. **The honest framing:** the RFC's §13 "outer boundary (container/VM/granted MCP subset) is the security model" is *defensible* given prompt injection is unsolved — but the granted-MCP-subset leg must be the Rule-of-Two-aware scoping above, or it's just a capability list. **Minimalism here means structural isolation, not a policy engine** — which is exactly what process isolation + scoped tool grants provide.

---

## 8. Structured outputs & tool-call formats

**Sources:** structured-output reliability write-ups (2025–2026); constrained-decoding explainers; Anthropic ACI guidance.

- **Four eras:** prompt-engineering (→2023) → JSON mode (2023–24) → schema enforcement / strict mode (2024–25) → high-performance constrained-decoding engines (2025–26). Native structured output now supported by OpenAI (Aug 2024), Google, **Anthropic (beta Nov 2025, GA early 2026)**, Cohere, xAI.
- **Constrained decoding = syntactic guarantee only.** A JSON Schema compiles to an FSM; only tokens keeping output on a valid path are sampled → **valid JSON, guaranteed.** But this gives **schema compliance, not semantic correctness** — "your output can be perfectly valid JSON and completely wrong." Naive prompting has ~10–20% format failure; proper schema enforcement pushes sub-1%.
- **ACI / tool format (Anthropic):** choose a tool-call format "close to what the model has seen naturally"; give the model "enough tokens to think before it writes itself into a corner"; avoid formats that demand error-prone bookkeeping (exact line counts, heavy string escaping); document tools with examples, edge cases, input formats, and boundaries; apply **poka-yoke** (make wrong calls hard to express — e.g. require absolute paths).
- **Tool descriptions are high-leverage.** Anthropic's tool-tester rewriting a flawed MCP tool's description → **40% faster task completion** for later agents. Each tool needs a *distinct purpose and clear description*; bloated/overlapping tools cause wrong-path failures.

→ **Implication for agentd:**
1. **Use the provider's native tool-calling / structured-output where available; don't hand-roll parsing.** `agentd` speaks an OpenAI-compatible `/chat/completions` (RFC §7.2) and/or a normalized gateway shape. Lean on the provider's tool-call schema for tool invocations (the model's primary action format), and on JSON-Schema/strict-mode for the agent's **own structured result** where the gateway supports it.
2. **But never assume semantic correctness from schema validity.** Combine constrained/strict output with the **environment-grounded verification** of §5/§6 — a well-formed result still needs a ground-truth check before "completed."
3. **Robust parse-fail handling (ties to §6.1).** If the model emits a malformed/invalid tool call despite schema hints, **feed the validation error back as an observation and retry** rather than crashing. Keep a small bounded retry count for format errors specifically (they "resolve on first retry").
4. **`agentd`'s own MCP tool descriptions are product surface.** The self-MCP tools (`subagent.spawn/send/cancel/status`, `subscribe/unsubscribe`, `exec`) must have **excellent, poka-yoke descriptions** — distinct purposes, examples, clear boundaries, and (per §2) the spawn tool's description should *teach effort-scaling and output contracts*. Anthropic's 40% finding says this is one of the highest-ROI things to get right. Budget real effort here.
5. **Keep the self-MCP tool set small (§4 / Anthropic's "if a human can't pick the tool…").** The RFC's minimal self-MCP surface (§14.4) is correct; resist adding overlapping tools. A small, unambiguous catalogue both reasons better and is safer.
6. **Result shape:** define a small **structured run-result** (status enum from §6.5, summary text, optional output resource handle, token/step accounting). One-shot mode prints it; reactive/subagent modes return it up the control channel and/or expose it as a resource. This makes results machine-consumable for the future external orchestrator (RFC's stated goal).

---

## 9. Eval & observability for agent reliability

**Sources:** Anthropic multi-agent post (eval + tracing); OpenTelemetry GenAI semantic conventions & AI-agent-observability blog; Datadog/Greptime write-ups on `gen_ai.*`.

### 9.1 Evaluation

- **Start tiny.** Anthropic: "~20 queries representing real usage" was enough to see the impact of most changes. Don't wait for a big eval set.
- **LLM-as-judge against a rubric** for fuzzy outputs: factual accuracy, citation accuracy, completeness, source quality, tool efficiency — one judge, one rubric, structured score. Useful but imperfect.
- **Humans catch what evals miss:** hallucinations on unusual inputs, systemic biases (their agents preferred SEO content farms over authoritative PDFs), subtle failures.
- **End-state, not turn-by-turn, evaluation** is often more robust for agents (judge the final artifact + whether the goal was met), since many valid paths exist.

### 9.2 Observability / tracing

- **Industry is converging on OpenTelemetry GenAI semantic conventions** (`gen_ai.*`): standardized spans/attributes for LLM calls, agent invocations, tool executions, token usage, costs, latency; trace/session structure across an agent run. Backends (Datadog, New Relic, Dynatrace) consume it natively.
- **Full production tracing was decisive** (Anthropic): it "let us diagnose why agents failed and fix issues systematically." They monitor **agent decision patterns and interaction structures** — *without* recording conversation contents (privacy). Telemetry doubles as a **feedback loop** to improve the agent, not just to alarm.
- **Three signals:** traces (the run tree), metrics (tokens, latency, tool counts, error rates), logs (structured events). For agents specifically: capture the agent/tool/LLM span tree, token usage, tool error rates, and decision/transition patterns.
- **Minimal-instrumentation guidance:** make telemetry toggleable; align to OTel conventions rather than inventing a format; don't bloat the core.

→ **Implication for agentd:**
1. **The control-channel event stream IS the trace.** RFC §6.1/§6.2 already streams thought/tool-call/tool-result/final up the tree as structured JSON events. This is the agent trace tree for free — the supervisor sees the whole process tree's events. **Make these events the canonical observability primitive**: structured (JSON lines to stdout/stderr per RFC §11), with stable fields. This satisfies hard requirement #6 (first-class logging/observability) without an OTLP/tracing SDK (explicitly out per §12).
2. **Map event fields to `gen_ai.*` semantic conventions in spirit** (don't pull the SDK). Capture, per LLM call: model, token usage (prompt/completion), latency; per tool call: server, tool name, success/error, duration; per subagent: handle, parent, depth, status, budget consumption. Naming these to *mirror* OTel `gen_ai.*` makes downstream OTel mapping trivial for an operator without `agentd` depending on OpenTelemetry. (A thin external collector can translate JSON-lines → OTLP.)
3. **Privacy by default:** like Anthropic, **log structure and decisions, not raw conversation/secret content.** RFC §13 already forbids secrets in logs; extend the principle — make full prompt/result content logging *opt-in* (a verbosity flag), default to metadata + decision events. This is both privacy and the lethal-trifecta posture (don't accidentally exfiltrate sensitive content into logs).
4. **Health signal (RFC §11 "trivial health signal").** The supervisor's tree view → a cheap healthcheck: process alive, subagents within budgets, no stuck children (heartbeat from §6.4), last-event timestamps. Expose it as: a `--health` exit-code check, a self-MCP introspection resource (§8), and/or a log heartbeat. Keep it dependency-free.
5. **Ship a minimal eval hook, not an eval framework.** Don't build evals into core. But make `agentd` *evaluable*: deterministic structured results (§8.6), structured event traces, and the ability to run an instruction over a fixed input and compare results. The "20 real queries" discipline lives in the user's test harness, fed by `agentd`'s structured outputs. Document this.
6. **Exit codes carry the headline outcome** for the external scheduler (RFC §11): distinct codes for completed / budget-exhausted / failed / killed (from §6.5). This is the cheapest, most robust observability surface for a "schedulable unit of work."

---

## 10. Consolidated design implications (checklist for RFC 0001)

**Loop & termination**
- [ ] Keep ReAct in-core; express plan/ReWOO/reflexion via model+subagents, not runtime modes (§1, §3).
- [ ] Add a named **verify** phase to the subagent loop; ground it in MCP/exec results, never pure self-judgment (§3, §5, §6).
- [ ] Make **termination an explicit enumerated state machine**: `completed` (criteria-verified) / `budget_exhausted` / `cancelled` / `failed` / `killed`; guard against premature completion in long-running sessions (§5, §6).

**Subagents / tree**
- [ ] Enrich `subagent.spawn`: objective + output contract + tool/source guidance + boundaries + effort tier (§2). Bad delegation is the top multi-agent failure.
- [ ] Subagent **results = distilled summary + resource handle**, not raw transcript — for cost, context hygiene, *and* injection firewalling (§2, §4, §7).
- [ ] Prefer **async/concurrent** subagents with reactive continuation over synchronous blocking (§2).
- [ ] Conservative depth/breadth/token-tree budgets, supervisor-enforced (§2, §6).

**Context**
- [ ] Add **compaction** to warm/long-running sessions; start with tool-result clearing (§4).
- [ ] Resources = **just-in-time handles**, read on demand; never pre-load all resources into context (§4).
- [ ] External memory/checkpoint via MCP resources/exec, not a built-in store; checkpoint = compacted summary written to a resource (§4, §5, §6).

**Errors & reliability**
- [ ] Tool errors → **observations, not crashes**; bounded format-retry (§6, §8).
- [ ] **Two retry layers:** deterministic transport backoff (transient) vs model-driven semantic recovery (logical) (§6).
- [ ] **Budget guardrails as a hard safety system** (the runaway-loop / 92%-cost finding) (§6).
- [ ] **Heartbeat + stuck/no-progress detection** in supervisor; `SIGKILL` hung subtrees (§6; RFC hard req #8).

**Security (lethal trifecta)**
- [ ] Interpret `tool_scope` as a **Rule-of-Two trust budget**; warn/refuse grants giving one subagent untrusted-input + sensitive + egress without override (§7).
- [ ] Treat **all** MCP server content (incl. tool descriptions/results) as untrusted; assume tool poisoning (§7).
- [ ] **SSRF defenses** in the HTTP client: HTTPS-in-prod, block RFC-1918/loopback/link-local, validate redirects, pin DNS (§7).
- [ ] Keep `exec` off by default; stdio MCP default; self-MCP prefer unix/vsock over HTTP; spec MUSTs if HTTP (§7).

**Outputs & observability**
- [ ] Use provider-native tool-calling / strict structured output; never assume validity = correctness (§8).
- [ ] Invest in **excellent self-MCP tool descriptions** (poka-yoke; 40%-speedup finding); keep the tool set small (§8).
- [ ] Control-channel events = the trace; structure them to **mirror `gen_ai.*`** without taking the OTel dependency (§9).
- [ ] **Log structure/decisions, not content** (privacy + anti-exfiltration); content logging opt-in (§9).
- [ ] **Distinct exit codes** per terminal state for the external scheduler; cheap health signal from supervisor tree view (§6, §9).

---

## 11. Bibliography (primary sources preferred)

**Anthropic engineering (primary):**
- Building Effective Agents — https://www.anthropic.com/engineering/building-effective-agents
- How we built our multi-agent research system — https://www.anthropic.com/engineering/multi-agent-research-system
- Effective context engineering for AI agents — https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- Building agents with the Claude Agent SDK — https://claude.com/blog/building-agents-with-the-claude-agent-sdk (formerly anthropic.com/engineering/building-agents-with-the-claude-agent-sdk)
- Effective harnesses for long-running agents — https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents
- Equipping agents for the real world with Agent Skills — https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills

**Model Context Protocol (primary spec):**
- MCP Security Best Practices — https://modelcontextprotocol.io/docs/tutorials/security/security_best_practices
- MCP Authorization spec (referenced by the above) — https://modelcontextprotocol.io/specification/latest/basic/authorization

**Prompt injection / lethal trifecta / agent security:**
- Simon Willison — The lethal trifecta for AI agents — https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/
- Meta — Agents Rule of Two: A Practical Approach to AI Agent Security — https://ai.meta.com/blog/practical-ai-agent-security/
- MCP Threat Modeling & Tool-Poisoning (arXiv) — https://arxiv.org/abs/2603.22489
- SoK: Security and Safety in the MCP Ecosystem (arXiv) — https://arxiv.org/pdf/2512.08290
- Cloud Security Alliance — Agentic MCP Security Best Practices — https://labs.cloudsecurityalliance.org/agentic/agentic-mcp-security-best-practices-v1/

**Agent reasoning patterns (papers / comparisons):**
- ReWOO: Decoupling Reasoning from Observations (arXiv 2305.18323) — https://arxiv.org/pdf/2305.18323
- ReAct vs Plan-and-Execute vs ReWOO vs Reflexion — https://theaiengineer.substack.com/p/the-4-single-agent-patterns
- Recoverability Has a Law: the ERR Measure for Tool-Augmented Agents (arXiv 2601.22352) — https://arxiv.org/pdf/2601.22352

**Structured outputs:**
- Beyond JSON Mode: reliable structured outputs in production — https://tianpan.co/blog/2025-10-29-structured-outputs-llm-production
- Structured Output isn't Reliable Output — https://rotascale.com/blog/structured-output-isnt-reliable-output/

**Observability / OpenTelemetry GenAI:**
- OpenTelemetry — AI Agent Observability: Evolving Standards and Best Practices — https://opentelemetry.io/blog/2025/ai-agent-observability/
- OpenTelemetry — Inside the LLM Call: GenAI Observability — https://opentelemetry.io/blog/2026/genai-observability/
- Datadog — native support for OTel GenAI Semantic Conventions — https://www.datadoghq.com/blog/llm-otel-semantic-convention/

**Error recovery / cost (secondary):**
- Fast.io — AI Agent Error Handling: Best Practices & Patterns (2025) — https://fast.io/resources/ai-agent-error-handling/
- IDC 2025 agentic-cost survey (runaway loops as #1 cost cause) — widely cited; see SparkCo summary — https://sparkco.ai/blog/mastering-agent-error-recovery-retry-logic
