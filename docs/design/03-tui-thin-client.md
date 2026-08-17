# 03 — Thin-Client TUI/UI over A2A (Ink)

Status: **IMPLEMENTED** (2026-08-16) — the contract became **RFC 0032** (`rfcs/0032-interface-and-observation-plane.md`),
the daemon side shipped as the `interface` config + `SubscribeToEvents` feed + taskless reads + `agentd tui|ui`
passthrough, and the clients live under `interface/` (one package, `@agentd-dev/cli`); operator guide in
`docs/interface.md`. This document remains as the design rationale; where it and RFC 0032 differ, the RFC (and code) win —
notably: the observation plane shipped as the feed + taskless command reads (no `agent://` resources), and Phase 0/1/3
landed together.

> **Thesis.** `agentd` is the single source of truth. The TUI (Ink/React) and any web UI are
> **stateless projections** of daemon state — they hold no agent logic, no tools, no secrets, no
> conversation state of their own. They forward *intent* up and render *state* down. Because no
> client owns any truth, multiple clients (a terminal + a browser + a CI script) can attach and
> detach from the same daemon and each render independently, in sync, for free.

This document (a) states the thin-client architecture, (b) maps it onto agentd's **actual** network
surface today, (c) is honest about the gap between that surface and the full vision, (d) proposes the
minimal, moat-preserving agentd-side additions that close the gap, and (e) specifies the Ink client —
component tree, transport, state model, layouts, and the performance rules that actually bite.

---

## Contents

1. [The thin-client principle](#1-the-thin-client-principle)
2. [Reference architectures (why this shape)](#2-reference-architectures)
3. [agentd's real surface today](#3-agentds-real-surface-today)
4. [The gap: the observation plane](#4-the-gap-the-observation-plane)
5. [Proposed agentd-side additions](#5-proposed-agentd-side-additions)
6. [The Ink client](#6-the-ink-client)
7. [Ink best practices that actually bite](#7-ink-best-practices-that-actually-bite)
8. [Multi-surface: TUI + web from one daemon](#8-multi-surface-tui--web-from-one-daemon)
9. [Phased plan](#9-phased-plan)
10. [Open decisions](#10-open-decisions)

---

## 1. The thin-client principle

Three invariants, in priority order:

1. **The daemon owns all state and all capability.** LLM calls, tools, workflows, subagents,
   secrets, durable store — every one stays in `agentd`. The UI never sees a secret, never calls a
   model, never runs a tool. This is a *security* property (a compromised or screen-shared UI leaks
   nothing) as much as an architectural one.
2. **Clients are disposable and interchangeable.** A client is `pure(daemon_state) -> pixels` plus
   `input -> intent`. Kill it, restart it, open a second one — the daemon doesn't notice and nothing
   is lost. The TUI and the web UI are two renderers of the same contract.
3. **Sync is a consequence, not a feature.** If every client is a projection of one authoritative
   event log, "keep the TUI and the web UI in sync" requires *zero* client-to-client logic. They
   converge because they subscribe to the same feed.

The failure mode to avoid: letting "send a prompt" also be "receive the reply" on the same
request/response. That couples the act of steering to the act of observing, and it means a *second*
client never sees the first client's prompt. Keep them separate (next section).

---

## 2. Reference architectures

The pattern is well-trodden. Two proof points shaped this design:

- **OpenCode** runs a headless HTTP server (`opencode serve`) behind an OpenAPI contract. The TUI is
  "simply one implementation of a client"; IDE extensions and web apps speak the *same* API. Prompts
  are `POST /session/:id/message`; **all** live updates arrive on a separate **SSE** event stream
  that the server broadcasts to every attached client. Command channel and observation channel are
  distinct.
- **Zellij's web client** keeps session state server-side; clients are thin ("client passes input to
  the server, server passes render instructions to the client"). Multiple clients attach to identical
  channels and all receive identical updates. Notably it **splits the stream by volume** — a
  high-frequency data channel and a low-frequency control channel — so a burst of output can't block a
  resize or a cancel.

Distilled into two rules we adopt:

- **Two channels.** A *command* channel (unary request/response: send, cancel, list, drain) and an
  *observation* channel (one long-lived server→client stream carrying every state change). The reply
  to a prompt arrives on the observation channel, exactly like it arrives for every other client.
- **Resumable, replayable stream.** Events carry a monotonic id; a reconnecting *or newly-attaching*
  client replays from a cursor (SSE's `Last-Event-ID`) rather than losing history. This is the
  backbone of attach/detach and of a late-joining second client catching up.

---

## 3. agentd's real surface today

`agentd` 2.0 has exactly **three inbound network surfaces** (`runtime/mod.rs:526-617`):

| Surface | Purpose | Use for the UI? |
|---|---|---|
| **A2A HTTPS listener** (`a2a.listen`) | The only external control+observe channel; JSON-RPC 2.0 framed | **Yes — this is the client transport** |
| Webhook listener (`webhooks.listen`) | Inbound HMAC → workflow-run | No (ingress only) |
| Obs probe (`observability.metrics_addr`) | Unauthenticated GET `/metrics` `/healthz` `/readyz` | Health widget only |

So "the UI talks to agentd" means **"the UI speaks A2A."** The `web/` directory is the marketing
site, not a client — there is no existing display client to model against.

### 3.1 The transport (good news for a local client)

- **JSON-RPC 2.0 over HTTP POST.** PascalCase methods (`SendMessage`, `GetTask`, …). The request path
  is ignored — POST to `/` (`http_server.rs:623`). One request per TCP connection (`Connection:
  close`). Body cap 8 MiB.
- **Streaming = SSE over the POST response.** A request whose method is `SendStreamingMessage` or
  `SubscribeToTask` is upgraded to `text/event-stream` (`http_server.rs:297`); each frame is a full
  JSON-RPC response reusing the request `id`; 15 s keep-alive comments; terminal frame then close.
- **Loopback = operator, zero config.** A plaintext loopback listener with no `a2a.principals`
  configured maps *every* caller to `operator` — full management, including the `a2a.*` admin family
  (`a2a_server.rs:280`, `principals.rs:216`). **The local TUI needs no credential setup.** (Configure
  `a2a.principals` and this default turns off — then a local caller needs an `{any:true}` rule or a
  bearer/cert.)
- **Durable tasks.** Tasks survive restart (`GetTask` works across daemon lives), so attach/detach is
  real, not cosmetic.

### 3.2 The methods (the whole contract)

Seven A2A methods + the card (`a2a_server.rs:40-47`):

| Method | Shape | Streaming | Notes |
|---|---|---|---|
| `SendMessage` | `{message, configuration?:{blocking}}` → `{task}` | no | `blocking` defaults **true** (polls to terminal, 120 s cap). Set `false` for the working task immediately. |
| `SendStreamingMessage` | same → SSE frames | **yes** | status/artifact frames, terminal frame closes |
| `GetTask` | `{id}` → **`Task` (bare)** | no | drop-recovery + cross-restart read |
| `CancelTask` | `{id}` → **`Task` (bare)** | no | cascades to linked run / subagent |
| `ListTasks` | `{}` → `{tasks:[Task], totalSize, pageSize, nextPageToken}` | no | snapshot, one page; operator sees all, others own-only. The tasks are `Task`s without `artifacts`. |
| `SubscribeToTask` | `{id}` → SSE frames | **yes** | re-attach to a live task; `-32001` if unknown |
| `GetAgentCard` | `{}` → `AgentCard` | no | **public**; `skills` = workflows only |

One shape asymmetry the client must handle: **wrapped vs bare** — `SendMessage` wraps in `{task}`,
`GetTask`/`CancelTask` return the `Task` directly. Every task is otherwise the same object, so
`status.state` is the only place a state is ever read, and agentd's own facts (`agentd/link`,
`agentd/principal`, `agentd/statusHistory`) live under `metadata`, which is where proto3 puts
extensions. `status.timestamp` is an RFC 3339 string, not epoch millis — it is a
`google.protobuf.Timestamp`.

### 3.3 The read surface (one command carries almost everything)

There is **no `agent://` resource surface served** — RFC 0029 §8's read-model is deferred ("D7").
The entire global read is one command: a message with a **DataPart** `{"data":{"agentd":{"op":"status"}}}`
returns `status_value()` (`reactor.rs:766`) — the master state document:

```json
{ "instance","run_id","uptime_ms","job_shape","draining",
  "store":{"kind","degraded","generation"},
  "workflows":[{"name","hash","armed","starts"}],
  "runs":[{"id","workflow","status","steps":{"done":3,"running":1},"tokens","output","error","task","principal"}],
  "conversations":[{"id","kind","messages","est_tokens","turns","principal","skills","plan","updated"}],
  "subagents":[{"handle","mode","status","tokens"}],
  "children":[{"node","pid","kind","age_ms","tokens","cancelled"}],
  "budget":{…},"timers":{…},"inbox_pending":0,
  "tools":42,"skills":[…],
  "counters":{"turns","tool_calls","runs_started","runs_finished","tokens_in","tokens_out"},
  "instruction":{"source","uri","version","bytes"} }
```

Implemented command ops: `status`, `config` (operator; effective config, secret refs unresolved),
`workflow.run`, `workflow.status`, `workflow.cancel`. Everything else → `-32004`.

**This is enough to build the conversational core and a polling status dashboard today.** It is *not*
enough for the full "watch everything live, debug mode, multi-client convergence" vision — §4.

---

## 4. The gap: the observation plane

The honest part. The vision needs a live, addressable, broadcastable view of *all* daemon state. Here
is exactly what exists vs what's missing.

| Capability the vision needs | State today | Evidence |
|---|---|---|
| Global live event feed (everything happening) | **Missing** — only per-task SSE | `events.rs` is the loop's *internal* vocabulary; no client feed |
| Addressable read resources (`agent://runs`, `…/subagents`, `…/events`, `…/capabilities`) | **Missing** — deferred "D7" | RFC 0029 §8; no `resources/read` handler exists |
| Token-level model streaming | **By-design absent** — status/artifact only | RFC 0009 invariant; frames are working→artifact→terminal |
| Conversation / turn message history | **Missing** — only counts + plan progress | `context/mod.rs:524` exposes no bodies |
| Per-node/edge run-graph (DAG) | **Missing** — only a status→count histogram | `engine/run.rs:380` `progress()` |
| Steering commands over A2A (`subagent.send`, `workflow.signal`/`pause`/`resume`, `plan.get`) | **Granted by matrix, not dispatchable** → `-32004` | `a2a_server.rs:843`; model-only via NL turns |
| Human-in-the-loop / approval (reply into `input-required`) | **Not wired** — `ask_human` is a stub; `Gate` msgs ignored | `tools.rs:525`, `reactor.rs:479` |
| Multi-observer broadcast / convergence | **Missing** — principal-scoped shared state, **poll-based** | no push; each client polls `status` |
| Live capability/introspection endpoint | **Missing** — `--capabilities` is offline CLI; card lists workflows only | `runtime/mod.rs:675` |
| Browser (non-loopback) client | **Blocked** by DNS-rebind `Origin` guard (403) | `http_server.rs:634` |
| `subscriptions/listen` (MCP) on agentd | **Dead** — framework present, agentd never registers/notifies | opens a stream that never emits data |

The shape of the work is therefore **two layers**:

- **Core loop** — prompt → task → SSE/poll → artifact; list; cancel; drain. **A2A has this today.**
- **Observation plane** — a global event feed + an addressable read-model + (optional) token stream +
  HITL replies + multi-observer broadcast. **Internal-only; not network-served.** This is what makes
  "hosts all state, thin client displays it," debug mode, and TUI+web-in-sync real.

Your thin-client instinct is *correct*. It just requires exposing that second layer on the wire.

---

## 5. Proposed agentd-side additions

All additive, all behind existing features, all **moat-preserving** — they reuse the SSE machinery
already in the `mcp` crate (`http_server.rs`) and the existing `status_value()` projections. Zero new
Rust dependencies; the default 3-dep build (libc/serde/serde_json) is untouched. The **TUI itself is a
separate Node/React subproject** and has no bearing on the Rust moat at all.

Ordered by leverage:

1. **`SubscribeToEvents` — a global observation stream (the keystone).** A new streaming A2A method
   (peer of `SubscribeToTask`) that emits a principal-scoped feed of *every* state transition the
   caller may see: `task.*`, `run.step`, `conversation.turn`, `subagent.spawned/exited`,
   `budget.tick`, `drain`. Reuse `serve_stream`; drive it off the same shared snapshot the loop
   already republishes on every transition (`a2a_server.rs:1007 task_sync`) generalized to a bus.
   Each event carries a monotonic `seq`; accept `Last-Event-ID` and replay from a bounded ring so a
   late-joining or reconnecting client catches up. **This single method delivers multi-client
   convergence, live debug, and attach/detach in one stroke.**

2. **Implement RFC 0029 §8 read-model ("D7") as addressable reads.** Expose the existing
   `status_value()` sub-projections as read ops (either `agent://…` resources once a `resources/read`
   handler is added, or, cheaper, discrete DataPart command ops: `runs.list`, `run.get`,
   `conversation.get` *with turn history*, `subagents.list`, `capabilities`). The data already exists;
   this is projection + routing, not new engine work. Adds the run-graph and conversation-history
   reads the debug view needs.

3. **Wire HITL over A2A.** Connect `input-required` ↔ `SendMessage(taskId)` so a workflow `human`
   node suspends to `input-required` and a client reply resumes it (today `Gate` messages are ignored
   and `ask_human` is a stub). This is the single biggest *interaction* unlock — approvals, clarifying
   questions, steering.

4. **Dispatch the granted steering commands.** Add `a2a_command` arms for `subagent.send`,
   `workflow.signal`/`pause`/`resume`, `plan.get` — they're already in the authz matrix; only the
   dispatch is missing. Turns "steer by hoping the model calls a tool" into direct control.

5. **(Decision-gated) operator-facing token stream.** A richer, opt-in token stream *on the operator
   conversation channel only* — explicitly **not** the subagent distillate boundary (RFC 0009 stays
   intact for subagent→parent). Gives the Claude-Code-style live-typing feel. See §10.

6. **Browser origin story.** Extend the DNS-rebind guard with a configurable allowlist (or document
   "serve the web UI from loopback") so a browser client isn't 403'd. Bearer over a fetch-based SSE
   reader for remote (EventSource can't set headers — §7).

Items 1–2 alone are enough for a first-class debug/observability TUI. 3–4 make it *interactive* rather
than observational. This is worth its own RFC (`0032-observation-plane`), which this doc seeds.

---

## 6. The Ink client

### 6.1 Architecture

```mermaid
flowchart LR
  subgraph Client["Ink TUI (stateless projection)"]
    IN["input → intent"] --> CMD
    RED["event reducer\n(daemon state mirror)"] --> VIEW["React/Ink render"]
  end
  CMD["Command channel\n(JSON-RPC POST)"] -->|SendMessage / Cancel / ListTasks / a2a.drain| D
  D["agentd\n(source of truth)"] -->|SubscribeToEvents (SSE)| OBS["Observation channel"]
  OBS --> RED
```

The client is two thin adapters around a React tree:

- **`AgentdClient`** — a tiny transport module (no UI): `send(message)`, `cancel(id)`,
  `listTasks()`, `getTask(id)`, `subscribeEvents(fromSeq)`, `drain()`. It owns the HTTP/SSE plumbing
  and nothing else. **This module is shared verbatim with the web UI** (§8).
- **Event reducer** — folds the observation stream into a plain in-memory mirror of daemon state
  (`tasks`, `runs`, `conversations`, `subagents`, `budget`, `counters`). The UI reads *only* this
  mirror; it never derives truth locally. `Last-Event-ID`/`seq` drives replay and reconnect.

Everything the user does becomes a command; everything the user sees comes from the reducer. That is
the whole design.

### 6.2 Component tree (Screen / Part / Common taxonomy)

```
<App>                         render(); useInput global keymap; owns AgentdClient + reducer
├─ <Screen: Chat>             the default working surface
│  ├─ <Static><Transcript/>   past turns + tool/command results  ← scrollback, never redrawn
│  ├─ <LiveTurn/>             the in-progress turn (status line / streaming artifact)  ← the ONLY dynamic block
│  ├─ <Composer/>             prompt input (ink-text-input); slash-commands; multiline
│  └─ <StatusBar/>            spinner · model · tokens · budget · draining? · #tasks
├─ <Screen: Debug>            "extra debug mode" — toggled (e.g. F2 / ctrl-d)
│  ├─ <RunGraph/>             runs + steps histogram (→ DAG once read-model lands)
│  ├─ <SubagentTree/>         flat leaf view: handle · mode · status · tokens · pid · age
│  ├─ <EventFeed/>            live tail of SubscribeToEvents (the raw truth)
│  ├─ <BudgetPanel/>          governor + timers + counters
│  └─ <WireInspector/>        raw JSON-RPC frames in/out (invaluable while building the protocol)
├─ <Screen: Tasks>           ListTasks browser; select → attach (SubscribeToTask) / cancel
├─ <Overlay: Approval>       when a task hits input-required → inline approve/deny/answer (needs §5.3)
├─ <Overlay: Palette>        command palette: /workflows, /drain, /config, /switch-conversation
└─ <Common>                  TextInput, Select, Spinner, KeyHint, JsonView, Badge
```

Screens are full views; Parts are reusable regions; Common are leaf inputs. Only **one** screen is
mounted at a time; overlays mount above.

### 6.3 State model

- **One reducer, event-sourced.** `SubscribeToEvents` frames are the only writes. On (re)connect,
  replay from `seq` then live-tail. Optimistic UI is allowed for the *local* echo of a just-sent
  prompt, reconciled when its `conversation.turn` event arrives.
- **Handle the shape asymmetries in the client, once.** Normalize wrapped/bare envelopes and
  nested/flat `state` at the transport boundary so the reducer sees one canonical `Task` shape.
- **Degrade gracefully to polling.** Until `SubscribeToEvents` exists (§5.1), the same reducer can be
  fed by a `status`-command poll loop (e.g. 1 s) plus per-task `SubscribeToTask` for the active task.
  The UI code doesn't change when the push feed lands — only the source does. **This lets the TUI ship
  against today's surface and get strictly better as §5 lands.**

### 6.4 Layout (Chat screen)

```
┌────────────────────────────────────────────────────────────────┐
│ agentd · inst-7a3 · daemon · store:mcp gen 3 · ● ready          │  header (1 line)
├────────────────────────────────────────────────────────────────┤
│  you › summarize the incident and open a workflow               │  ┐
│  ▸ command status ✓                                             │  │ <Static>
│  ▸ workflow.run "triage" → task-91c  ✓                          │  │ transcript
│  agent › Started triage. 3 steps queued.                        │  │ (scrollback)
│  ▸ subagent warm:researcher  running  1.2k tok                  │  ┘
│  agent › ⣾ working — step 2/3 (analyze)…                        │  ← LiveTurn (dynamic, 1–2 lines)
├────────────────────────────────────────────────────────────────┤
│ › _                                                             │  Composer
│ ⣾ working · claude-opus · 4.1k/8k tok · $0.12 · esc cancel · F2 │  StatusBar
└────────────────────────────────────────────────────────────────┘
```

Tree-structured, short lines, tool/command results as `▸` blocks — reads well in a small window and
maps 1:1 onto the event stream. The **transcript is committed to `<Static>`** (terminal scrollback);
only the `LiveTurn` + Composer + StatusBar are dynamic (§7).

### 6.5 Keyboard model

- `useInput` global keymap at `<App>`; `useFocus`/`useFocusManager` for pane focus in Debug.
- Enter = send · Shift/Alt-Enter = newline · `/` = palette · Esc = cancel active task · Tab = cycle
  focus · F2/ctrl-d = Debug · ctrl-c = confirm-then-`unmount()`.
- Slash-commands are client sugar that compile to A2A: `/drain` → `a2a.drain`; `/workflow triage` →
  `workflow.run`; `/tasks` → Tasks screen; `/config` → `config` command into `<JsonView>`.

---

## 7. Ink best practices that actually bite

| Concern | Rule | Why |
|---|---|---|
| **Flicker / perf** | Keep the dynamic region **shorter than the terminal height**; commit finished lines to `<Static>`. | Ink redraws the *entire* dynamic tree on every state change; exceeding terminal height triggers a full-screen clear-and-redraw. `<Static>` writes to scrollback once and is never re-rendered (a virtual list for the terminal). This is the technique behind Claude Code / Jest output. |
| **Streaming into the transcript** | Render the streaming turn as the single dynamic `LiveTurn`; on completion, *move* it into `<Static>`. | Only ever one growing block is live. |
| **SSE in Node** | Do **not** use `EventSource` — it can't set headers, so bearer auth fails. Use a fetch-based reader (`@microsoft/fetch-event-source` or undici streaming); send `Last-Event-ID` to resume. | Remote/auth’d agentd needs `Authorization`; loopback-operator can skip it but keep one code path. |
| **Logging** | Never `console.log` while Ink runs — it corrupts layout. Route logs to a file or the Debug `<EventFeed>`. | Ink `patchConsole` helps but don't rely on it; keep stdout for the render tree only. |
| **Layout** | Everything is flexbox `<Box>`. Use `useWindowSize`/`measureElement` for responsive panes; `<Spacer>` to push the StatusBar to the bottom. | Terminal resizes are first-class. |
| **Testing** | `ink-testing-library` for component snapshots; a **fake `AgentdClient`** that replays a recorded event stream to drive the reducer in tests. | The two-adapter split makes the UI trivially testable without a live daemon. |
| **Ecosystem** | `ink-text-input`, `ink-select-input`, `ink-spinner`, `ink-table`, plus a small `<JsonView>` for `config`/wire frames. | Don't hand-roll inputs. |

---

## 8. Multi-surface: TUI + web from one daemon

Because state lives in agentd and both clients are projections:

- **The `AgentdClient` transport module + event reducer are shared code** (a TS package, e.g.
  `packages/agentd-client`). The Ink TUI and a React web UI import the *same* module; only the render
  layer differs (Ink `<Box>` vs DOM). This is the OpenCode "TUI is just one client" property, made
  literal.
- **Convergence is automatic** once `SubscribeToEvents` (§5.1) exists: both clients subscribe, both
  replay from `seq`, both see the same turns/tasks/runs. No client-to-client channel. A prompt typed
  in the TUI appears in the browser because both are watching the daemon's feed.
- **Same principal ⇒ same view.** Loopback TUI is operator (sees all). A browser must clear the
  **DNS-rebind origin guard** (serve from loopback, or the §5.6 allowlist) and present a bearer via
  the fetch-based SSE reader.
- **Attach/detach is free.** Durable tasks + replayable feed mean a client can close and reopen (or a
  second one can join) and reconstruct the current state from the cursor.

---

## 9. Phased plan

- **Phase 0 — Core loop on today's surface.** Ink app; `AgentdClient` (send/cancel/list/get +
  `SubscribeToTask`); reducer fed by a `status` poll + active-task SSE; Chat screen; loopback-operator
  (no auth). *Ships against the daemon as it exists now.* Delivers the prompting UX.
- **Phase 1 — Observation plane (RFC 0032).** `SubscribeToEvents` (§5.1) + read-model D7 (§5.2).
  Swap the reducer's source from poll → push; Debug screen (EventFeed, RunGraph, SubagentTree,
  WireInspector) comes alive. Delivers "display all state" + multi-client convergence.
- **Phase 2 — Interaction.** HITL/approval overlay (§5.3) + steering commands (§5.4). Delivers
  side-by-side working (approve, answer, steer, pause).
- **Phase 3 — Web UI.** Extract `packages/agentd-client`; React web renderer over the shared module;
  origin-guard/bearer story (§5.6). Delivers simultaneous TUI + web.
- **Phase 4 — Polish.** Optional token stream (§5.5, decision-gated), themes, config editor,
  multi-conversation switcher.

---

## 10. Open decisions

1. **Token-level streaming?** RFC 0009 keeps the *subagent→parent* boundary at distillate/status by
   design. A live-typing operator UX needs a token stream on the *operator conversation* channel only.
   Options: (a) leave it status/artifact-level (simplest, honest, no invariant touched); (b) add an
   opt-in operator token stream scoped to top-level turns. **Recommendation: ship Phase 0–2 at
   status/artifact level; revisit (b) as Phase 4 behind a flag.**
2. **Read-model transport: resources vs commands?** Full `agent://…` `resources/read` (spec-faithful,
   more work) vs discrete DataPart command ops (cheaper, ships sooner). **Recommendation: command ops
   first; promote to resources if/when an MCP management surface is wired.**
3. **Web transport: SSE vs WebSocket?** SSE reuses existing machinery and is unidirectional-perfect
   for the observation channel (commands stay POST). WebSocket only if we later need high-frequency
   bidirectional (e.g. a PTY). **Recommendation: SSE + POST; defer WS.**
4. **Where does the client core live?** Same repo (`packages/agentd-client`, `packages/tui`) vs a
   separate repo. **Recommendation: in-repo package so the TS client and the Rust protocol evolve
   together and share conformance fixtures.**
5. **Auth for remote.** Bearer via fetch-based SSE is settled; open question is whether the TUI grows
   a `login`-style flow reusing the RFC 0031 `agentd login` credential cache, or stays loopback-only
   initially. **Recommendation: loopback-only for Phase 0; bearer in Phase 3 with the web UI.**

---

### Appendix — sources (Ink & reference architectures)

Ink (`github.com/vadimdemedes/ink`, Ink 3 perf notes), the `<Static>`/flicker analysis, OpenCode's
server/REST+SSE architecture (headless `serve`, event bus, "TUI is one client"), and Zellij's web
client (server holds state, thin client, split channels, multi-client attach) informed §2 and §7.
