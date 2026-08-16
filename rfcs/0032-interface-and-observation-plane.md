# RFC 0032 — The display-client interface & observation plane

- Status: **Implemented**
- Requires: RFC 0029 (A2A conversations, principals, commands), RFC 0016 §7.2 (the event ring)
- Companions: `docs/interface.md` (operator guide), `docs/design/03-tui-thin-client.md` (design rationale),
  `interface/` (the client projects: `@agentd/client`, `@agentd/tui`, `@agentd/ui`)

## 1. Motivation

agentd is the single source of truth — conversations, tasks, runs, subagents,
budgets all live in the daemon. A human working *beside* an instance needs to
**see** that state and steer it, from more than one surface at once: a terminal
UI, a browser tab, both simultaneously, attaching and detaching at will.

RFC 0029 gives a client the core loop (send → task → stream/poll → artifact)
but observation is per-task: there is no "everything happening here" feed, no
transcript read, no way for a second client to learn what the first one is
doing without polling. This RFC adds the missing **observation plane** — as a
small, default-OFF extension of the existing A2A listener, not a new socket.

Principles:

1. **Thin clients.** A display client holds no capability: no tools, no
   secrets, no agent logic. It forwards intent (send/cancel/run) and renders
   projections. Kill it, restart it, open five — the daemon doesn't notice.
2. **Convergence by broadcast, not client sync.** Every state change is an
   event on one feed; N clients fold the same events and converge. There is no
   client-to-client protocol.
3. **Default OFF, debug OFF-er.** The surface exists only under
   `interface.enabled`; the internals-exposing reads only under
   `interface.debug`. With both off, the wire is byte-identical to RFC 0029.

## 2. Configuration

```yaml
interface:
  enabled: true          # serve the interface methods (default false)
  debug: false           # expose extra information (transcripts, step detail, log ring, audit events)
  origins: []            # extra allowed browser origins for a HOSTED web UI (CORS);
                         # loopback origins never need listing
  display:               # what clients render in their chrome (§12); omit for the defaults
    top: [name, version, instance, debug]
    bottom: [conn, endpoint, draining, active, turns, tokens, screen, keys]
  pairing:               # pairing-code login (§13)
    enabled: false
    role: operator       # operator (default) or user
    ttl: 12h             # session-token lifetime
```

`interface.enabled` requires `a2a.listen` (validated). The surface rides the
A2A listener and its existing auth: on a plaintext loopback dev listener with
no principals, a local client is the operator with zero configuration; a
remote client presents the bearer / mTLS identity RFC 0029 already defines.

## 3. Discovery

- The public agent card (`GetAgentCard`) advertises
  `capabilities.extensions: [{uri: "urn:agentd:interface", params: {enabled: true}}]`
  when the surface is on — only the on/off bit is public.
- `interface.info` (a command op, any non-anonymous principal) returns the
  authenticated view:

```json
{"interface": {"enabled": true, "debug": false, "version": "…", "instance": "…",
  "protocol": 1, "feed": {"ring": 1024, "method": "SubscribeToEvents"},
  "ops": ["interface.info", …]}}
```

Clients key their debug panes off `debug` — the daemon decides what may render.

## 4. The feed — `SubscribeToEvents`

A new server-streaming A2A method (SSE over the POST response, like
`SubscribeToTask`). Params: `{"fromSeq": <n>}` (0 = from the window start).
Frames (each a JSON-RPC response reusing the request id):

| Frame | Shape | Meaning |
|---|---|---|
| hello | `{"hello": {seq, resume, resync, debug, version}}` | first frame; `resync: true` ⇒ the cursor predates the replay window — re-bootstrap via `status` |
| event | `{"event": {seq, ts, kind, data}}` | one state change |
| goodbye | `{"goodbye": {seq, reason?}}` | terminal frame (stream deadline / server end); reconnect with `fromSeq: seq` |

Events ride a bounded ring (1024) with a monotonic `seq`, so a reconnecting or
late-joining client replays from a cursor. Visibility is principal-scoped per
event: owner events reach their owner + operators; global sections are
operator-only; lifecycle notices reach everyone. The cursor advances past
invisible events, so a non-operator resumes correctly.

Kinds and sources:

| kind | data | scope | source |
|---|---|---|---|
| `task` / `task.removed` | the full A2A task (+ link, principal) | owner | every task transition (`task_sync`) |
| `message` | `{contextId, taskId, messageId, principal, text}` | owner | every NL send — **the cross-client transcript** |
| `command` | `{op, principal, contextId}` | owner | mutating command ops only (`workflow.run`/`cancel`/`signal`, `subagent.send`/`kill`); reads stay off the feed |
| `run`, `conversation`, `subagent`, `child`, `status` (+ `.removed`) | the section item | operator (run/conversation: owner) | the loop's **section diff**: a 4 Hz fingerprint pass over runs/conversations/subagents/children/slim-status that emits only what changed (moving fields like `age_ms` excluded from the fingerprint) |
| `lifecycle` | `{draining?, paused?, reason}` | all | drain; `a2a.pause`/`a2a.resume` |
| `config` | `{path, value}` | operator | a runtime `config.set` (§14) — every attached client re-shapes live |
| `pairing` | `{paired, sessions}` | operator | a client paired (§13) |
| `activity` / `activity.removed` | the unit's live activity | owner (unbound: operator) | §17 — change-triggered only |
| `audit` | the audit record | operator, **debug only** | every audited action — except the taskless interface reads, which would feed-loop their own polling |

The reply to a prompt is not a special frame: it arrives as the task's
terminal artifact on its `task` event — the same way every other client sees
it. That is what makes N simultaneous surfaces converge with no extra
machinery.

## 5. The reads (taskless command ops)

Interface reads deliberately create **no durable task** (unlike `status` /
`config`): they are reads, not work, and a client polls them freely without
growing the task store. They return their document directly.

| op | gate | returns |
|---|---|---|
| `interface.info` | enabled | §3 — now also `model`, `display` (§12), `pairing.enabled` (§13) |
| `conversation.get {id, limit?}` | enabled + **debug**; owner/operator | the transcript **with message bodies** (system/user/assistant/tool/note, tool calls), summary, plan, skills — strings truncated at 4 KiB |
| `run.get {run}` | enabled + **debug**; owner/operator | the run with **per-step detail**: status, attempt, started/finished, error, wait, truncated output, vars |
| `subagent.get {handle}` | enabled + **debug**; operator | one subagent's detail: instruction, status, mode, attempts, tokens, result/error (truncated), requested_by — the drill-down behind the subagents screen |
| `debug.events {after?, limit?, level?, prefix?}` | enabled + **debug**; operator | a cursor window of the live **log ring** (RFC 0016 §7.2 — installed at startup, or on a runtime debug toggle) |
| `pairing.code` | enabled + pairing; operator | §13 — the current rotating code + remaining validity |
| `config.set {path, value}` | enabled; operator | §14 — runtime-set a whitelisted knob |

Ownership non-disclosure matches RFC 0029: a non-owner gets `-32001`, not a
denial that confirms existence. Everything else is `-32004` with a message
naming the gate (`interface.enabled` / `interface.debug`).

## 6. Authorization

- `SubscribeToEvents` joins the §2-matrix method list for every non-anonymous
  role (the feed itself scopes per event).
- `interface.info` is always granted to non-anonymous roles (like `status`).
- `conversation.get` / `run.get` join the `user` role defaults
  (owner-scoped at the object); `debug.events` stays operator-only.
- Every call is audited like any other A2A call.

## 7. Browsers

The DNS-rebind `Origin` guard gains a configured allowlist: an origin that is
loopback **or** listed in `interface.origins` is served **with CORS response
headers** (echoed origin, `OPTIONS` preflight answered before auth); any other
cross-site origin stays 403. `EventSource` is never used — clients consume the
SSE stream via fetch, so `Authorization` works. A web UI served from loopback
(`agentd-ui`) needs no configuration; a hosted copy lists its origin.

## 8. The clients (`interface/`, separate Node projects)

Not bundled into agentd; the Rust dependency moat is untouched.

- **`@agentd/client`** — the shared framework-free core: the JSON-RPC/SSE
  wire, task-shape normalization (nested vs flat), the **Mirror** (an
  event-sourced projection with the transcript derivation: `message` events +
  task terminal artifacts, local echo reconciled by `messageId`, command-result
  tasks kept off the conversation), and the **Observation** driver
  (bootstrap `status`+`ListTasks` → feed with cursor resume/reconnect →
  automatic poll fallback against a daemon without the surface).
- **`@agentd/tui`** (bin `agentd-tui`) — Ink. Chat, Tasks, Subagents, Debug —
  debug panes render only when the daemon says `debug: true`. **Fullscreen
  (alternate screen) by default**: the client owns the scroll (PgUp/PgDn over
  a bottom-anchored viewport, follow-the-tail unless scrolled up) because that
  buffer has no scrollback; `--inline` renders into the normal buffer instead,
  where settled rows ride `<Static>` into the terminal's own scrollback and
  survive quitting. Degrades to a read-only inline view without an interactive
  terminal.
- **`@agentd/ui`** (bin `agentd-ui`) — the web UI in the format of the TUI
  (dark-terminal identity), same Mirror, statically hostable `dist/`;
  `agentd-ui` serves it locally with an injected endpoint and `--open`.

## 9. The passthrough — `agentd tui` / `agentd ui`

One command runs the daemon AND its display client:

```
agentd tui --config code.yaml [--debug]
agentd ui  --config code.yaml [--debug] [--no-open]
```

The subcommand forces `--interface.enabled true` (and `--interface.debug true`
under `--debug`) as **argv flags**, so a SIGHUP reload keeps them. The daemon's
stdout/stderr are redirected to a log file (path printed first;
`AGENTD_INTERFACE_LOG` overrides) and the saved tty is handed to the client
child (`agentd-tui`/`agentd-ui` from PATH, `AGENTD_TUI_BIN`/`AGENTD_UI_BIN`
override), spawned once the listener accepts connections, with
`AGENTD_ENDPOINT` (the loopback rewrite of `a2a.listen`) and `AGENTD_BEARER`
(resolved) in its environment. Lifetimes are tied: client exit ⇒ SIGTERM ⇒
graceful drain; daemon exit ⇒ client killed. Unix-only.

The detached forms remain first-class: `agentd -c …` + `agentd-tui --endpoint …`
+ a browser on `agentd-ui` — all attached to one daemon, all converging.

## 12. The daemon-driven chrome (`interface.display`)

The daemon decides what its display clients render in their edges — the top
(header) and bottom (status bar) — as ordered item lists, served in
`interface.info.display` and runtime-shapeable via `config.set` (§14): every
attached client re-lays-out live when it changes. Unknown items are skipped
(forward compatibility); TUI-only items (`screen`, `keys`) are skipped by the
web renderer.

Item vocabulary: `name` `version` `instance` `model` `endpoint` `conn` `debug`
`draining` (the lifecycle notice: DRAINING or PAUSED) `active` `turns`
`tokens` `tool_calls` `runs` `subagents` `conversations` `screen` `keys`
`clock`. Unknown names in the config draw a
validation **warning**, not an error.

## 13. Pairing-code login (`interface.pairing`)

The friction pairing removes: connecting a browser tab or a remote TUI without
copying a long-lived bearer out of config/secret stores. The pattern is device
pairing — a short code is safe only as a **bootstrap**, so it exchanges for a
real credential:

1. The daemon derives a **6-digit code per 60-second window** from a
   per-process random seed (`HMAC-SHA256(seed, window)`) — no timer, no
   storage; a restart voids everything.
2. An **operator** reads the current code via `pairing.code` (the TUI's
   `/pair`, which also prints the ready-made connect command) and hands it to
   the joiner — over a shoulder, a call, a chat.
3. The joiner — **unauthenticated** — calls `Pair {code}`. Verification
   accepts the current or previous window (clock/typing grace), compares in
   constant time, and rate-limits (5 failures per window locks pairing out for
   the window — a 6-digit space is only safe rate-limited).
4. A hit mints a **session token** (`pat-` + 32 bytes of `/dev/urandom`, hex)
   with the configured role (default operator — whoever can read the code can
   already see the operator's console) and TTL (default 12 h). The token rides
   `Authorization: Bearer` like any credential; sessions live in memory, so a
   restart revokes all.

Transport interplay: with pairing enabled, a credential-less request on a
guarded listener is admitted as **anonymous** instead of 401 — able to call
exactly `Pair` and the public card, nothing else. On a non-loopback listener,
pairing counts as "client auth exists" for config validation. Every `Pair` is
audited; successes emit an operator-visible `pairing` feed event.

Client surfaces: `agentd-tui --code 123456`, the web connect form's code
field, `AgentdClient.pair()`.

## 14. Runtime config (`config.set`) — and its deliberate limit

`config.set {path, value}` (operator) updates a **whitelisted** set of knobs
in the running daemon, echoing a `config` feed event so every client converges:

- `interface.debug` — toggle the debug surface live (installs the log ring on
  first enable),
- `interface.display.top` / `interface.display.bottom` — reshape every
  client's chrome (§12).

Everything else is refused with a message naming the whitelist — deliberately.
Full config mutation over the wire would fork the daemon's state from the
operator's config files (secret refs, restart-only sections, reload
provenance); the file + SIGHUP hot-reload path (RFC 0017) stays the one way to
change the rest. `/config` (the `config` command) remains the read: the full
effective document, or one path.

## 15. Composer affordances (client-side, shared)

Both shipped UIs speak the same input language (implemented once in
`@agentd/client`'s composer module):

- **`/`** — commands: the system set (`/help /new /tasks /subagents /debug
  /status /config [path] /set /workflow /cancel /pair /drain /quit`), plus
  **every daemon workflow as a shortcut** (`/deploy` ⇒ `workflow.run deploy`;
  system names win). Suggestions render as you type; Tab accepts.
- **`@`** — **skills**: autocompletes the daemon's catalogue; the reference
  stays inline in the text (agentd preloads referenced skills natively).
- **`#`** — **targets**: a *leading* `#task-…` answers/continues that task
  (the way to answer a specific input-required gate); a leading `#<ctx>`
  addresses that conversation. Inline `#…` is plain text.
- **`$`** — **live values**: `$model` `$instance` `$version` `$turns`
  `$tokens` `$tasks` interpolate from the mirror before sending; unknown
  `$words` are untouched; `$$` escapes a literal dollar.

## 16. Human-in-the-loop (`ask_human` + the `human` node)

The interaction loop the interface exists for. An ask — the model calling
`ask_human`, or a workflow reaching a `human` step — flips (or creates) the
owning A2A task to **`input-required`** with the question as its status
message: every attached client renders an answerable gate. A `SendMessage`
carrying that `taskId` resolves the suspended asker with the reply text — the
tool call returns it to the model; the `human` step completes with it as its
output (so later steps template on `steps.<gate>.output`). The answer is
broadcast as a `message` feed event (all clients see who answered what), and
both ask and answer are audited.

Task selection: the ask binds to the A2A task behind the asking turn, or the
task tracking the asking run; a unit with NO A2A owner (a scheduled turn, a
subagent) gets a standalone gate task so attached operators still see and
answer it. One live gate per task; asks within one unit are sequential.

Durability: tasks are durable, so a **run's** gate survives a restart (the
pending ask is rebuilt from the suspended `human` step and the reply path
works across lives). A **turn's** gate degrades gracefully — the asking child
died with the old process, so an answer simply continues the conversation as a
fresh turn. Cancelling a gate task unblocks its asker with an error.

**Fallback** (`agent.ask_human_fallback`) when NO human channel exists
(`interface.enabled` off) — and, for `auto`, when an interface-served gate
times out unanswered (default ask timeout 24 h, per-ask `timeout` arg):

| value (aliases) | behavior |
|---|---|
| `fail` (default; `finish`, `stop`) | the ask errors immediately — the model / the workflow's failure policy decides |
| `wait` (`pause`, `idle`) | park until the ask timeout, then fail |
| `auto` | an LLM judge answers **on the operator's behalf** — prompted conservatively (safe/reversible choices only, `UNDECIDED` ⇒ fail), and always **marked as auto** in the task status, the log and the audit stream |

`fail` is the default because a headless deployment must not silently hang for
a day; `auto`'s marking exists because a judge's guess must never be mistaken
for a human decision.

## 17. Live activity (the working row)

What the agent is doing *right now*, for the clients' working row:
`thinking · 12s · 1.2k tok · round 2` / `read_file · 3s · 1.2k tok` /
`waiting · subagent · 40s`.

The turn worker already reported coarse progress upward (`AgentMsg::Event`);
the supervisor dropped those frames. It now folds them into a per-unit
**activity** record — phase (`thinking` | `tool` | `waiting`), current tool,
round, tokens spent, `started_ms` — published as `activity` /
`activity.removed` feed events and mirrored in `status.activity` for the
poll-fallback path. Sources: the child's `turn.think` / `turn.round` (carrying
that round's usage) / `turn.tool` (the ONLY signal that names an MCP tool —
the supervisor never sees those calls, the child holds its own connections),
plus a supervisor-side park when a tool defers (sleep, subagent, human gate).

Two properties make this cheap where token streaming would not be:

1. **Change-triggered.** An event is emitted only when something an operator
   would notice changes — phase, tool, round. Token-only updates are silent.
2. **Elapsed is never streamed.** The record carries `started_ms` and clients
   tick their own clock, so a five-minute think emits *nothing*.

A turn therefore produces a handful of activity events, not one per second —
the feed's replay ring stays a record of state, which is exactly the property
a token stream would have destroyed (RFC 0032 §18). Activity is ephemeral and
operator/owner-scoped: it is never persisted, never an artifact, and the
task's terminal artifact remains the only truth about the reply.

## 18. Bounds & failure modes

- The feed ring holds 1024 events; overrun evicts oldest and `hello.resync`
  tells a stale cursor to re-bootstrap. Feed frames stop early when the peer
  is gone (a failed SSE write ends the poll loop, not the stream deadline).
- Streams still close at the listener's 600 s deadline; clients reconnect with
  `fromSeq` (the `goodbye` cursor) — attach/detach is the normal case, not an
  error.
- The section diff runs at most 4 Hz and emits only fingerprint changes;
  a quiet daemon emits nothing.
- Debug reads truncate strings (4 KiB / 2 KiB) and cap windows (1000 msgs,
  500 log lines) — a display client cannot balloon a reply.

## 19. Non-goals (unchanged invariants)

- **No token-level streaming.** The reply is the task's terminal artifact
  (status/artifact-level streaming, RFC 0009). agentd does not even request a
  stream from the provider — the intel `Request` has no `stream` flag — so
  this is a wire-layer change across three dialects plus a new
  child→supervisor→client transport, NOT a matter of forwarding something
  already in hand. §17's live activity deliberately covers the perceived-
  liveness gap instead. If it is ever built it must be: operator-channel only
  (never the subagent→parent boundary, RFC 0009), display-only (never a
  context, artifact, or persisted state), and carried on a channel that
  BYPASSES the reactor and the replay ring.
- Steering is now first-class, not a non-goal: `workflow.signal`,
  `subagent.send`/`kill`/`status` and `plan.get` dispatch as command ops, and
  `a2a.pause`/`a2a.resume` hold one run or the whole instance (reversible;
  intake continues, dispatch parks). What remains out: any control that would
  bypass the principal matrix.
- The interface is not an MCP surface; `agent://` resources stay unserved
  (RFC 0029 §8 D7) — the taskless reads cover the display need without them.
