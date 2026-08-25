# Changelog

All notable changes to **`agentd`** — the minimal, MCP-native, reactive agent
runtime (developed in the `agentd-dev` org). The format is loosely
[Keep a Changelog](https://keepachangelog.com); versions are the released git tags
(`vX.Y.Z`) and the published image `ghcr.io/agentd-dev/agentd:X.Y.Z`.

## v1.1.1 — comments that describe the code

Comments and documentation now explain the current logic — the rules,
invariants and failure modes — rather than citing the spec documents behind
them. Roughly 1,660 internal spec references and a layer of development
history ("used to", "no longer", "the old path", plan-phase markers,
version-era branding) are gone from code and prose; each was replaced with
the reasoning it stood in for. External IETF citations (message signatures,
device grant, PKCE, merge patch, resource metadata) are preserved, since
they are load-bearing for anyone reading the auth and MCP paths.

The specifications remain in `rfcs/` for the formal contract but are no
longer published as documentation, so the docs site is current product
only.

Reading every comment surfaced places where the documentation and the code
disagreed, all corrected: the budget governor documented "tightest window
wins" when the code stops at the first refusing scope; a doc comment
described parameters its function does not take; the person-server consent
flow was called unimplemented while it ships; six doc comments sat on the
wrong function; a conversation-turn caller distinction was built and then
ignored by both branches; and the README still declared the previous
`config_version`.

Crates: `agentd-core` / `agentd-cli` **1.1.1**; `agentd-mcp` / `agentd-net`
**1.0.1**. `@agentd-dev/cli` **1.1.1**; `ghcr.io/agentd-dev/agentd:1.1.1`.

## v1.1.0 — the prompt you can write

The system prompt stops being Rust and becomes **data plus a template** you
can override — with loops, conditions and limits over the agent's own
environment. The built-in default is written in that same language and
printed by `agentd --context-template`, and it is now ordered for provider
prefix caching, which the previous fixed order quietly defeated.

Crates: `agentd-core` / `agentd-cli` **1.1.0**; `agentd-mcp` / `agentd-net`
1.0.0 unchanged. `@agentd-dev/cli` **1.1.0**;
`ghcr.io/agentd-dev/agentd:1.1.0`.

### Added

- **The system prompt becomes data plus a template (RFC 0038).** The runtime
  exposes its environment — instance, instruction, workflows, services,
  streams, subagent templates, skills, peers, parked signals, memory keys,
  granted internal tools — and a two-block language renders it: `{{#if}}`,
  `{{#each}}` (with `this` and `@index`), interpolation and comments.
  Expressions resolve as a **path first, CEL second**, so bare lookups work
  in any build and only real expressions need `--features cel` (refused at
  config load otherwise, never mis-rendered at turn time). Two helpers fill
  CEL's gaps and are available to workflow expressions too: `take(list, n)`
  and `join(list, sep)`.
- **`agentd --context-template`** prints the built-in template, which is
  written in that same language — an override starts as a copy.
- **Named templates, selectable per node.** `context.template` is the
  instance default; `context.templates.<name>` are alternates a step picks
  with `context: {template: <name>}` — an extraction step can drop the whole
  environment without inlining one.
- **The default is ordered for prefix caching.** Providers cache on the
  literal prefix, so the shipped template runs stable-to-volatile: persona
  and instruction, then configuration-derived sections, then live state.
  Previously parked signals rendered *before* configuration, so ordinary
  coordination traffic invalidated the cache for everything after it.
- **`context.summarize.prompt` / `.model`** — override the compaction
  summarizer's guidance, and run it on a cheaper model. The summary's JSON
  schema stays fixed (it is parsed back into the context).

### Changed

- The persona line now names the internal tools this instance **actually
  grants**, derived from the registry. It previously recited a hardcoded
  list, so an instance that narrowed `agent.tools.internal` still told the
  model it had `subagent.*` — briefing it on tools it would be refused.
- **`context.cards` is removed** (shipped in v1.0.0, no users): the template
  supersedes it, and carrying two selection mechanisms would be worse than
  the break. A step's `context: {cards: […]}` becomes
  `context: {template: <name>}`.

## v1.0.0 — the relicense reset

**agentd restarts its public numbering at v1.0.0 under a new license.** The
whole tree — every crate (`agentd-core`, `agentd-cli`, `agentd-mcp`,
`agentd-net`, `agentd-conformance`), the Node clients, the site and the docs
— is now **AGPL-3.0-only**, and the git history was rewritten so no commit
claims otherwise (the vendored `third_party/connectrpc` keeps its own
Apache-2.0 license and attribution, as it must). agentd remains fully open
source; commercial licensing (for proprietary embedding or AGPL-free use)
and commercial support are available — contact **agent@agentd.dev**. The
previously published 2.x/0.x artifacts are withdrawn (GitHub releases, tags
and GHCR images deleted); the `config_version` marker becomes `"1"` and
version-branded strings drop the "2.0" era. Everything below this heading
ships as **v1.0.0**; the 2.x entries further down record the same code's
pre-reset history.

Crates: `agentd-core` / `agentd-cli` / `agentd-mcp` / `agentd-net`
**1.0.0**. `@agentd-dev/cli` **1.0.0**; `ghcr.io/agentd-dev/agentd:1.0.0`.

### Added

- **The service catalog & egress policy (RFC 0037 Phase A).** A top-level
  `services:` section names the external services a deployment may use — one
  shared RFC 0031 credential per entry (`agentd login service:<name>`),
  **authoritative trifecta tags** (an unconditional floor for any MCP server
  whose endpoint matches, so under-tagging cannot launder a sensitive
  endpoint past the gate), a tool-surface ceiling consumers can only narrow
  (`mcp.servers[].service:` references inherit connection settings and may
  not restate them), and per-instance `rate:` pacing of workflow `mcp.tool`
  steps. `security.egress: closed` refuses any MCP dial or A2A push target
  whose URL matches no entry; `--validate-config` prints each consumer's
  effective endpoint, admission lists and tags.
- **Subagent templates & instance-tier children (RFC 0036 Phase A).** A
  `subagents:` section with operator-declared `templates:` whose
  `instruction` is a full RFC 0034 document, section-wide `defaults:`, and
  `allow_freeform: false` to make templates the only spawn path. One
  resolution rule, two tiers: no machinery ⇒ the existing flat worker
  (template fields merge under the call site's); machinery
  (`:::workflow`/`:::mcp`/`:::stream`/`:::config`/`:::tools`) ⇒ an
  **instance-tier child** — a full agentd daemon composed by the parent
  (own workflows, signals, streams, file store), supervised and reaped,
  auto-wired as an A2A peer over a unix socket (`output.peer` works with
  `a2a.send`/`a2a.delegate` immediately), retired by `ttl:`/`until:` (a
  signal delivered in the child; `lifecycle.until_signal`) or
  `subagent.retire` through the child's own graceful drain, respawned after
  a parent restart, and capped by `limits.subagents.instances.*`. The model
  fills declared, schema-checked `params` only — extraction runs once at
  boot on operator text, params fold in as data, and a param value that
  would introduce a directive fence refuses the spawn.
- **Environment cards in the model's context.** The system prompt now
  carries what the instance can actually reach and park on: `## Services`
  (catalog + closed-egress note), `## Streams`, `## Signals` (waits parked
  right now + recently fired), `## Peers` (configured + live instance
  children), and `## Subagent templates` (with declared params) — selectable
  with `context.cards:` in config and per node via the `agent`/`think`
  step's `context: {cards: […], seed: […]}` object form.

- **Durability classes — the fast path for recomputable work.** A workflow
  can opt out of persistence entirely with `durable: false` (runs are
  memory-only: no run record, no checkpoints, forgotten by a restart — the
  dominant per-step cost gone), a subagent spawn/template takes the same
  knob (`durable: false` = no record, no restore-respawn; a non-durable
  instance child runs on a memory store), and `store.durability.work:
  ephemeral` flips the deployment default with `durable: true` opting
  individual workflows back in. The default is unchanged: durable. Mixed
  shapes get a load-time warning (a durable parent waiting on a non-durable
  child fails its wait after a restart); the inbox, tasks, memory and
  credentials stay durable regardless of class.

- **RFC 0037 Phase B — the whole outbound surface.** Catalog entries carry a
  `kind:` (`mcp` / `intelligence` / `peer` / `http`; matching is
  kind-filtered), `a2a.peers[].service:` references inherit like MCP servers
  (and give peers `agentd login` via the shared `service:<entry>` credential),
  and `closed` egress now covers intelligence endpoints, peers, the `http`
  step (literals at load, templates at execution, plus per-entry
  `methods:` ceilings), the HTTP store and workflow-reference URLs —
  `observability.otel.endpoint` is the one named exception. Per-entry
  `rate:` pacing now applies in EVERY process (turn workers and flat
  subagents pace their own in-loop calls), and per-entry `breaker:` is the
  default breaker policy for `mcp.tool` steps (state stays per step).
  `examples/startup/` runs on a shared `services.yaml` as the reference
  deployment.
- **RFC 0036 Phase B — instance children grow up.** `mode: sync` with
  `result: {workflow}` resolves the spawn with the child workflow's first
  output (a composed reporter of existing nodes dials the internal
  `_instance.result` op home; first completion wins; the child lives on);
  `mirror_streams:` forwards child stream events into the parent's
  same-named streams (`source: instance:<handle>`); a durable child's token
  usage meters against the parent's budget windows off its manifest (a
  non-durable child is invisible to the meter by construction); `status`
  shows template/tier/pid/retire_at per child.

### Changed

- The `subagent` node/tool: `template` + `params` instantiate a declared
  template (`instruction` and `template` are mutually exclusive;
  `tools`/`servers` are refused with `template` — the template defines the
  grant); the vestigial `workflow` field — accepted and silently ignored
  since the v1 in-child driver was removed — is gone from the registry.
- `subagent.send` to an instance-tier child delivers over its A2A socket
  into the child's conversation; `subagent.retire` begins graceful
  retirement (SIGKILL only after the drain window).

## v2.7.0 — the event-driven company

agentd becomes an event-driven agent, and the docs prove it with a company:
durable event streams (RFC 0035 Phase A), an instruction document that can
define the whole agent, workflow execution measured 2–12× faster, typed
cross-agent commands, and `examples/startup/` — eleven instances running
every role of a SaaS business except the two founders, written up as
[The two-person company](https://agentd.dev/docs/two-person-company/).

Crates: `agentd-core` / `agentd-cli` **2.7.0**; `agentd-mcp` 0.4.0 /
`agentd-net` 0.4.0 unchanged. `@agentd-dev/cli` **2.7.0**;
`ghcr.io/agentd-dev/agentd:2.7.0`.

### Added

- **The convenience batch** — every gap the `examples/startup/` build-out
  surfaced, closed at the source:
  - `wait.on_timeout: <step>` — a deadline is an EXPECTED branch: the named
    step runs (forced), the wait's dependents stay unfired, the run
    continues. Replaces the `on_error: continue` + sentinel-`switch` idiom.
  - **Typed A2A commands** — `a2a.send`/`a2a.delegate` take `command` +
    `args` and send the DataPart the peer's `a2a` start actually matches on
    (prose objectives reach the peer's *model*; commands now reach its
    *registry*, deterministically). The `a2a` start takes `schema:` — a
    payload contract enforced at the listener, synchronously, naming the
    mismatch — and its runs read `{{steps.cmd.output.args.*}}` typed. A
    command-fired run now completes its A2A task, so a delegate BLOCKS on
    the command's actual output.
  - `webhook` starts take `signal: "name/{{ body.field }}"` — the
    webhook→signal relay workflow collapses into one field.
  - `memory.push` / `memory.shift` / `memory.pop` — durable array
    operations: the queue primitive (`{found: false}` on empty, so a drain
    loop just stops), with no model call to mutate a list.
  - `stream` starts take `rate: "<burst>/<per>"` — paced consumption turns
    any stream into a worked-off queue (`1/1d` = one per day; events wait,
    durably, in order).
  - `human.asked` / `human.answered` internal events — a workflow
    (`{kind: event, on: human.asked}`) can escalate an opening gate
    out-of-band (mail the approver, ring a phone) instead of hoping someone
    is watching a terminal.
  - `switch` takes `on_no_match: skip` — an honest "else: do nothing"
    (completes, prunes every branch) instead of a forced failure or a
    dummy default step.
  - Durations accept `d` (days) and `w` (weeks) everywhere — `30d`, not
    `720h` — including rate windows.
  - `intelligence.endpoints: mock:<script>` — the built-in mock LLM spawned
    in-process: a whole agent runs offline with no key and no second
    process (debug builds always; release under `--features
    internal-mocks`).
- **`validate` counts as a pure data step** (no per-step checkpoint), and
  its `schema` field is no longer template-expanded.

### Fixed

- **A `from: new` stream consumer could skip events.** Its initial offset
  was resolved on every poll but persisted only when something fired, so
  everything emitted between arming and the first fire vanished. The anchor
  now persists the moment it is resolved.

### Changed

- **Workflow execution is 2–12× faster** (measured; `bench/wfperf/`).
  Profiling a 400-step chain showed the cycles going to per-step durable
  writes and clones, not scheduling. Fixed at the sources: `FileStore` no
  longer reads and re-parses the previous envelope on every put (the
  instance lock makes the in-memory seq exact); workflow definitions are
  shared (`Arc`), not deep-cloned per step; **pure data steps no longer
  checkpoint per step** — RFC 0025 §7 guards *effects*, and a crash replays
  a pure step deterministically, so an inline chain batches into its tick's
  single checkpoint (a completed `foreach` batch is still an explicit
  durability point, and a run-terminal completion still lands immediately);
  CEL programs are compiled once per expression, not per evaluation; the
  `memory.<key>` definition scan is memoized per content hash; and an `emit`
  wakes same-process `stream` consumers in the same reactor iteration
  instead of the next tick. Wall-clock on the bench shapes: a 200-assign
  chain 1564→155 ms on the file store (10×), interpolation-heavy 383→32 ms
  (12×), 300-element foreach 2475→612 ms (4×), CEL-gated chain 670→124 ms
  (5.4×). Crash-resume semantics verified: SIGKILL mid-chain and mid-batch
  both restart to the documented durability points.

### Added

- **The whole agent from one document** (RFC 0034 §5.1). Four config-defining
  directives — `:::config` (any v2 fragment), `:::mcp{name=…}` (an
  `mcp.servers[]` entry), `:::stream{name=…}`, `:::tools` — let
  `agentd --instruction-file agent.md` define everything a config file can:
  store, lifecycle, limits, MCP servers, streams, tool policy, workflows,
  skills. The document's fragment merges *under* explicit config (a file
  key, env var, or flag beats it; fragment `mcp.servers` append unless the
  name exists), through exactly the config file's own deserialization,
  validation, and reload partition — no parallel pipeline.
- **Per-server MCP tool admission** — `mcp.servers[].allow` / `exclude`
  globs gate a server's advertised tool names at the registry; exclude beats
  allow, and a gated-out tool is absent, not disabled — nothing downstream
  can resurrect it.

- **Event streams — Phase A of RFC 0035** (agentd as an event-driven agent).
  Declare durable, named streams under `streams:`
  (`orders: {retention: {max_events, max_age}}`); the `emit` step gains a
  stream form (`emit: {stream, subject, data, correlation}`) that appends a
  durable event, and the new **`stream` start** fires a run per event —
  including events another workflow published. `subject` matches exactly or
  by `prefix.*` glob, `filter` is CEL over the event, and `from: earliest`
  replays the whole backlog into a consumer that did not exist when the
  events were emitted. Offsets are durable (a restart resumes exactly where
  it left off) and consumers dedup by event id — the emit's id is the step's
  idempotency key, so crash-replayed publishes cannot double-fire. Naming an
  undeclared stream is a startup error, not a runtime surprise. Spec:
  RFC 0035 (Phases B–D — `correlate`, `wait {on: event}`, edge bindings, the
  `_runtime` stream — remain draft).

### Fixed

- **An unfiltered `event` watcher no longer triggers on its own runs.** A
  workflow with `{kind: event, on: workflow.finished}` and no filter used to
  fire on the completion of the run it had just fired — forever. Runtime
  events now never fire the workflow that produced them; watch yourself with
  an explicit filter if you truly mean to.

## v2.6.0 — the instruction becomes a document

Two features that make an agent something you author rather than assemble,
plus the missing thirds of two stories v2.5 started.

Crates: `agentd-core` / `agentd-cli` **2.6.0**; `agentd-mcp` 0.4.0 /
`agentd-net` 0.4.0 unchanged. `@agentd-dev/cli` **2.6.0**;
`ghcr.io/agentd-dev/agentd:2.6.0`.

### Added

- **Instruction directives** (RFC 0034, [docs](https://agentd.dev/docs/directives/)).
  `agent.instruction` is now a specified format — prose that may embed
  machinery in the `:::type{attrs}` colon-fence syntax: `:::workflow` bodies
  join `workflows:` exactly as inline entries (same folding, validation,
  hashing, pinning, retirement — and no sugar `main` loop when an instruction
  declares its own machinery); `:::skill{name}` defines an **inline skill**
  with no MCP server, referenced as `@skill:<name>`; `:::context` /
  `:::example` mark text as reference or few-shot, wrapped in tags the model
  reads unambiguously. The model always sees the *cleaned* text. Executed
  ONLY from operator-authored surfaces — never conversation text —
  fail-closed on unknown names, unclosed fences, bad bodies.
- **Graceful workflow retirement.** Every way a definition leaves — reload
  removal, replacement by a new version, `workflow.delete`, an instruction
  edit — now takes one path: disarm + unsubscribe MCP resources nothing else
  wants, pin the definition for live runs, stop admitting, apply the
  workflow's own `unload: {policy: drain|cancel|detach, timeout}` (default
  drain), and garbage-collect the pin when the last run lands
  (`workflow.retiring` / `workflow.unloaded`).
- **Circuit breakers on remote-effect steps** —
  `breaker: {failures, cooldown}` on `http` / `mcp.tool` / `a2a.send` /
  `a2a.delegate`: N consecutive failures open the circuit, further attempts
  fail instantly without dialling (`breaker open` error prefix — route
  fallbacks with `on_error` + `switch`), one probe per cooldown closes or
  re-opens it. State is durable and shared by fan-out iterations; proven by
  an e2e spanning five daemon processes.
- **Outbound rate pacing** — `rate: "<burst>/<per>s"` on the same kinds: past
  the burst the step *waits* on a durable timer (consuming neither an attempt
  nor a retry) instead of failing, so fan-outs drain at the declared pace.
  With `retry` (exponential, jittered — already shipped) the trio covers the
  failure taxonomy: transient / outage / quota.
- **Docs**: [Directives](https://agentd.dev/docs/directives/) and
  [PID 1 — agentd as init](https://agentd.dev/docs/pid-1/) concept pages;
  RFC 0034; RFC 0033 restored to the site index; the landing install panel
  grew curl/docker/npm/AI-agent channels with a copy button.

### Fixed

- `workflow.delete` stranded its own live runs (definition lost mid-flight);
  a reload leaked pinned definitions and MCP subscriptions forever; a
  *failed* reload left the workflow registry empty. All three are what the
  retirement path now guarantees against.
- A start node whose `inputs:` mapping fails to render now refuses to fire,
  loudly (`start.inputs.invalid`) — it used to fire with silently-empty
  inputs. The `event` start's payload shape
  (`{event, payload: {run, workflow, status}}`) is now documented.

## v2.5.1 — the answer must beat the obituary

A patch on the day of 2.5.0, for a defect the release's own CI surfaced. The
2.5.0 wake fix routed child frames through a forwarder thread; on a loaded
machine the forwarder could be descheduled, a child's exit could overtake its
final result frame, and a worker that had completed was failed as "worker
exited without a result". Readers now send directly into the reactor's queue
(a `FrameSink` closure — no hop thread), so joining a child's reader is a real
ordering guarantee: every frame it wrote is queued before its reap is
processed. Verified where it failed: the implicated suites pinned to 2 CPUs
against busy-loop spinners, 130/130 green.

Also: `schedule_at_once_e2e` now waits for outcomes instead of the runner's
clock (the other thing a slow CI machine disproved).

Crates: `agentd-core` / `agentd-cli` **2.5.1**; `agentd-mcp` 0.4.0 /
`agentd-net` 0.4.0 unchanged. `@agentd-dev/cli` **2.5.1**;
`ghcr.io/agentd-dev/agentd:2.5.1`.

## v2.5.0 — pressure, priority, and the fast lane

An agent that accepts work it cannot finish is worse than one that says no.
This release teaches the daemon to feel its own limits — disk, memory, CPU —
and to degrade in the order the operator chose; it makes retries safe to send
twice; and it makes the runtime fast where measurement, not intuition, said it
was slow.

Crates: `agentd-core` / `agentd-cli` **2.5.0**, `agentd-mcp` **0.4.0**
(`RawResponse` gained response headers); `agentd-net` **0.4.0** unchanged.
Display clients ship as `@agentd-dev/cli` **2.5.0**; the image is
`ghcr.io/agentd-dev/agentd:2.5.0`.

### Performance — measured, then fixed

- **Data pipelines run at execution speed.** The scheduler now re-runs to a
  fixpoint while inline steps (`assign`, `map`, `template`, `switch`…) keep
  completing, instead of advancing one step per 200 ms tick. A 200-step chain:
  **42.4 s → 2.26 s** (debug build).
- **A child's answer wakes the loop.** Subagent and turn-worker frames ride the
  same channel the reactor parks on, so a 5 ms answer no longer waits out the
  tick. A cross-agent delegation round trip: **214 ms → 18 ms**. Reaps are
  re-queued once behind the child's already-flushed frames, so "exited without
  a result" cannot be a race.
- **`subscribe` reads moved off the loop.** The notify-then-read for `subscribe`
  start nodes ran on the reactor thread; a slow MCP server could stall the
  daemon per update. Same fix the `wait on: resource` path already had.
- **A2A streaming results arrive.** The server sent the terminal status frame
  *before* the result artifact; a conformant client stops at terminal, so the
  answer was dropped ("completed without a distillate"). Artifact now precedes
  the final status, and the client recovers over unary `GetTask` when a peer
  still orders it the old way.

### Added

- **Resource pressure** (`store.file.min_free`, default 256 MB): the daemon
  watches the store filesystem's headroom (and the cgroup's memory), warns at
  2×, and below the threshold **sheds new work while in-flight work drains** —
  schedules skip with `start.shed`, webhooks answer `429` + `Retry-After`
  (after authentication), queued turns stay queued, `workflow.run` and subagent
  spawns refuse with the cause. Transitions log once
  (`pressure.warn/shed/cleared`); metrics schema 1.2 adds
  `agent_pressure_level`, `agent_disk_free_bytes`, `agent_runs_active`,
  `agent_turns_queued`.
- **Priority** — `priority: low|normal|high` on workflows and subagent spawns.
  `low` sheds one pressure level early (at *warn*), higher-priority runs
  schedule first each tick, and priority maps to OS niceness (`low` → +10,
  `high` → −5 best-effort). A tiebreak under scarcity, not a reservation.
- **OS resource caps for subagents.** `limits: {memory: 512MB, cpu: 5m}` become
  `RLIMIT_AS` / `RLIMIT_CPU` between fork and exec — kernel-enforced, beside
  the existing steps/tokens/deadline budgets. Verified in tests against
  `/proc/<pid>/limits` of the live child.
- **Idempotency for remote effects.** `http` (`idempotency: {header|query,
  value?}`), `mcp.tool` (automatic `agent/idempotency_key` in `_meta`),
  `a2a.send`/`a2a.delegate` (`idempotency: true` pins the A2A `messageId`).
  The default key is derived — `sha256(run_id.step_id)`, stable across retries
  by arithmetic, opaque on the wire — and `value:` substitutes an application
  key. The old `mcp.tool` key named the *attempt*, which defeated the field's
  purpose; the attempt now rides separately. Steps also see `env.step`,
  `env.attempt`, `env.idempotency_key`.
- **Unix-socket A2A for co-located instances.** `a2a.listen: unix:///path` and
  peer `endpoint: unix:///path`: same protocol, no TCP or TLS — the socket file
  is `0600` and every connection's `SO_PEERCRED` uid must be the daemon's own
  user (or root), which is strictly stronger than loopback TCP. TLS material on
  a unix listener is refused; webhooks deliberately stay `https://`.
- **Config `vars`** — named values (any JSON type, nested) referenced as
  `{{config.NAME}}` anywhere a string sits, substituted at load time so the
  definition hash pins the *resolved* workflow; exact-token references keep the
  value's type. The startup preflight now reports **every** unresolved
  `{{secret:…}}` / `{{secret-file:…}}` / `{{config.…}}` reference across the
  config and all loaded workflows in one refusal.
- **`--prompt-missing`** — each missing `{{secret:NAME}}` is asked for on
  `/dev/tty`, echo off, one by one; values live in process memory only and
  resolve exactly like environment ones. Refused without a controlling
  terminal. Tested against a real pty, including that the typed value is not
  echoed and reaches the point of use.
- **`--env <FILE>`** (repeatable) — a dependency-free dotenv subset loaded
  before anything reads the environment; the real environment beats any file,
  later files beat earlier. A malformed line refuses startup naming file:line.
- **Workflow sources.** A `workflows:` entry can be fetched by `url:` (with
  headers, timeout, and the same SSRF guard as `http` nodes; fail-closed at
  startup) or discovered by `dir:` + `glob:` (`**` crosses directories; zero
  matches is a refusal). `security.workflows.immutable: true` makes the loaded
  set read-only for the agent itself — `workflow.create/update/delete` are
  refused and logged `workflow.locked`.
- **`subscribe window`** — `window: {samples: N}` (≤ 256) keeps a durable ring
  of the last N read values and delivers it as `output.window`, oldest→newest:
  the trend, not just the reading, for hardware-driver-style streams. The ring
  accrues through a debounce — coalescing drops *firings*, the window keeps the
  *samples*.
- **Webhook arrival throttling** — `rate: "<burst>/<per>s"` per route: past the
  burst the route answers `429` with a computed `Retry-After`, before anything
  is written to the durable inbox. `parallelism` bounds how many run at once;
  `rate` bounds how fast they arrive.

### Changed

- `agentd-mcp`'s `RawResponse` carries extra response headers (how the webhook
  listener says `Retry-After`) — the 0.4.0 bump.
- The `unix:` scheme, retired for MCP/intelligence endpoints in the 2.0 pivot,
  returns **only** for A2A listeners and peers; `vsock:` stays retired.
- Docs: `configuration.md` (vars, sources, `--env`/`--fresh`/`--prompt-missing`,
  `min_free`), `operations.md` §7 (the shed/drain story), `workflows.md` and
  the node registry (idempotency, `rate`, `window`, subagent caps, priority),
  `a2a.md` (the unix-socket lane), `architecture.md` (the loop, as it now is —
  with the measured numbers).

## v2.4.0 — the display clients grow up

The clients could show you that an agent was working. This release is about
showing you *what* it is doing, letting you act on it, and letting the UI be
hosted somewhere other than the machine the daemon runs on.

Crates: `agentd-core` / `agentd-cli` / `agentd-conformance` **2.4.0**;
`agentd-net` **0.4.0** and `agentd-mcp` **0.3.1** unchanged. Display clients
ship as `@agentd-dev/cli` **2.4.0**; images are
`ghcr.io/agentd-dev/agentd:2.4.0` and, new, `ghcr.io/agentd-dev/agentd-ui`.

### Added

- **A hosted web UI.** The client is a thin one — the page connects to *your*
  daemon from *your* browser, and the host never sees a request to an agent or
  holds a credential. That is what makes it safe to be public, and CI asserts
  the bundle stays that way. Ships as an OCI image (unprivileged nginx,
  amd64+arm64) and a static tarball; `docs/hosting-the-ui.md` covers the deploy.
- **Private Network Access.** A page on a public origin reaching a daemon on
  loopback is the shape browsers gate: Chrome drops the request unless the
  preflight is answered with `Access-Control-Allow-Private-Network: true`.
  agentd now answers it, riding the existing `interface.origins` allow-list so
  it grants the origin you configured and no other.
- **Status values a workflow maintains.** A `memory:<key>` display item renders
  whatever a workflow wrote to that key — a branch, a PR number, a deploy
  state. agentd still executes nothing locally; the workflow reads the value
  from an MCP server and the chrome shows it. Unset renders nothing, and TTL is
  honoured so a stale value empties rather than lying.
- **An approval policy.** `agent.approval`: `ask` (a person answers), `auto` (an
  LLM judge decides, marked as such), `accept` (take the ask's recommendation).
  Runtime-settable, because how much you want to be asked changes with what the
  agent is doing. `accept` degrades to `auto` when there is no recommendation
  rather than inventing an answer.
- **Gates that ask the way their schema says.** The answer schema now travels
  with the gate, so a client offers the actual choices — single-select,
  multi-select, "other", yes/no — instead of a text box the person guesses the
  wording for. Free text still works for anything a form cannot express.
- **Subagent control.** Message a warm subagent or stop one, from the client
  that is already showing it. The web UI keeps the list beside the detail; the
  TUI uses `m` and `k`, with a confirmation on the one that is not undoable.
- **Per-step visibility.** Every step transition reaches the clients, so a run
  shows *what* it is doing rather than "3 done, 1 running" — with per-step
  durations, because the slow step is usually what you came to find.
- **Step breakpoints.** `workflow.pause {before_step}` stops a run just before a
  named step, so it can be inspected in the state it is in rather than one
  effect later. Durable, like pause itself.

### Changed

- **Durations everywhere they mean something**: the live counter while working,
  what it settled at when the turn ended, and how long each step took. A step
  whose start was never observed reports no duration rather than a wrong one.
- **Colour carries meaning** rather than decoration, and body text keeps the
  readable neutral even when its mark is coloured.
- **The interface documentation shows the program**, illustrated with frames
  the shipped TUI actually rendered and regenerated by `npm run frames`, so it
  cannot drift into describing something agentd no longer does.

## v2.3.0 — CEL in the box, and the branch nobody chose

A feature-parity analysis against LangGraph and LangChain raised 59 candidate
gaps; 56 survived adversarial refutation. This release is the roadmap that came
out of it, and the headline is a correctness bug: **the branch a `switch` did
not take still ran its whole tail.**

Crates: `agentd-core` / `agentd-cli` / `agentd-conformance` **2.3.0**;
`agentd-net` **0.4.0** and `agentd-mcp` **0.3.1** are unchanged. The display
clients ship as `@agentd-dev/cli` **2.3.0**; the image is
`ghcr.io/agentd-dev/agentd:2.3.0`.

### Fixed

- **An untaken branch ran its downstream.** `switch` marked the not-taken case
  *targets* `Skipped` but never their descendants, and a skipped step satisfies
  its dependents — so everything below the dead branch ran, sometimes before the
  chosen branch's own steps. The satisfying behaviour could not simply be
  removed: it is what lets a workflow with several start nodes fire one and
  still run the graph below the others, and what lets an uneven join proceed.
  So `Pruned` is now a distinct status — terminal, and **not** satisfying — with
  the rule that a step is pruned only when *every* inbound path is pruned. One
  live parent keeps it alive. A false `when` guard prunes for the same reason.
  Sibling start nodes stay `Skipped` and keep satisfying.
- **Fan-out was sequential by default and silently clamped at 8.** `foreach`
  defaulted to one lane at a time — a loop with extra syntax — and a request for
  more than the ceiling was quietly reduced, so a workflow could run eight-wide
  while its author believed it ran fifty. The ceiling is now
  `limits.workflow.fan_out`, over-asking is refused **at load** naming the step,
  and the default is 4.
- **A stall was reported as "no ready step"**, which is a symptom. It now names
  the first failed ancestor, and a run blocked behind a failure ends `failed`
  rather than `stalled`.
- **Retry had no jitter**, so steps that failed in one wave retried in lockstep.
  Deterministic per (run, step, attempt), so a replay reproduces the schedule.
- **A workflow silent about limits ran unbounded.** It now inherits
  `limits.run.*`, which the documentation already described as applying.

### Added

- **CEL ships in the released binaries.** `when`, `until`, `filter` and the
  `expr` of `map`/`filter`/`reduce` are how a workflow branches, and they were
  refused at load on every published build — so any non-trivial workflow was a
  build-from-source job. It costs 1.86 MiB (6.62 → 8.48 MiB amd64) and 23
  crates. `exec` stays out; that one is a security posture, not a size decision.
- **The A2A workflow nodes.** `a2a` (start), `a2a.send` and `a2a.wait` were
  refused by the parser; all three now work, which makes the asynchronous half
  of an agent-to-agent conversation expressible. A workflow declaring
  `{kind: a2a, command: "x"}` REGISTERS `x` as a command the listener accepts.
  `wait {on: message}` is fixed by the same change — it could previously only
  ever end by timing out.
- **Declared state.** `state: {key: {schema, reducer}}` — a schema gates the
  write at the step that produced it, and a declared reducer turns the
  concurrent-write check from a heuristic into a policy.
- **Retention.** `store.retention.runs: {keep_last, ttl}` evicts terminal runs;
  before this a long-lived instance kept one record per run forever.
- **Step-level debugging.** Every step transition emits a `step` feed event, and
  `workflow.pause {before_step}` is a durable breakpoint.
- **A node registry** at `docs/node-registry.md` — all 67 kinds with their
  required fields, generated from the binary's own registry.

### Changed

- **Declared knobs now work or are refused.** `outputs.schema` was checked for
  well-formedness and never applied; `on_replay` was published and read by
  nothing; `checkpoint` was an alias for `noop`; `human.to`/`reply_uri` were
  accepted and ignored; `collect.mode` typos fell through to overwrite;
  `store.durability: eventual` promised weaker-but-faster writes that were never
  wired. Each now does what it says, or fails saying it does not exist.
- **Two concurrent steps overwriting one variable** is refused at load — a
  last-write-wins race decided by completion order. Reducers, ordered pairs and
  mutually exclusive `switch` arms are exempt.
- **A `human` gate inside `foreach`/`parallel`/`batch`/`race`** is refused at
  load until each gate carries its own identity; only one can be live per run,
  so the second item would wait for a reply that can never reach it.
- **A human's answer is checked against `human.schema`**, and a mismatch
  re-asks rather than failing the step.

## v2.2.0 — durability by default, and the defects a real review found

The headline feature is small and the bug list is not. A multi-agent review of
the 2.1.0 tree raised 34 findings; 33 survived adversarial refutation, and the
worst of them wedged the daemon on **default configuration**. This release is
that list, closed, plus the store that should have existed all along.

Crates: `agentd-core` / `agentd-cli` / `agentd-conformance` **2.2.0**,
`agentd-net` **0.4.0** (new public API), `agentd-mcp` **0.3.1**. The display
clients ship as `@agentd-dev/cli` **2.2.0**; the image is
`ghcr.io/agentd-dev/agentd:2.2.0`.

### Added

- **A local file store (RFC 0033), and durability by default.** A long-lived
  instance — a schedule, a subscription, an A2A listener, a goal — used to exit
  `2` unless you had already stood up an MCP or HTTP coordination backend.
  Durability should be something a laptop already satisfies. `store.kind: file`
  is a full adapter: seq CAS, atomic writes (tmp → fsync → rename → fsync dir),
  `0700` directories and `0600` files at every level, path traversal closed at
  the adapter, and an exclusive `flock` so two instances on one directory fail
  at startup naming the holder's pid rather than interleaving into corruption.

  Identity is **`agent.name`**, not a hash of the config. A hash looks automatic
  and is actively harmful: adding an MCP server or fixing a typo changes it, so
  the agent starts fresh, abandons its in-flight workflows and orphans the old
  state — silently, from the most ordinary edit anyone makes. The digest is kept
  where it belongs, in the manifest, logging `store.config_changed` and resuming
  anyway. `--fresh` exposes the generation counter the manifest already had.

  One-shot runs are unchanged (they still write nothing), and an explicit
  `store.kind: none` is still refused. Durability is the filesystem's property,
  not agentd's: on a container's writable layer this survives a restart but not
  a reschedule, and the defaulted store says exactly that at startup.

### Fixed (BREAKING for two configurations — see the last two entries)

**Ship-blockers.**

- **The reactor livelocked on its own default concurrency policy.** A start
  event over the cap was pushed back onto the deque `process_inbox` was
  draining, and the cap can only be relieved by a later step of the same tick.
  The single writer spun at 100% CPU forever: no timers, no checkpoints, no
  SIGTERM. `on_overflow: queue` is the default and `max_runs` defaults to 4, so
  a workflow with no `concurrency:` block at all wedged the daemon on its fifth
  concurrent start. The default path had never been executed past the cap.
- **One malformed request killed the daemon.** A `SendMessage` whose `params`
  was not an object panicked the A2A listener through serde_json's `IndexMut`,
  and the release profile is `panic = "abort"`.
- **A hostile chunk header aborted the process** — the accumulated-length check
  overflowed. Reachable at startup, since agentd dials every configured MCP
  server then.
- **`poll_pending` removed by a stale index**, panicking the reactor when a
  reply re-entered and pruned the table underneath it.
- **A turn ending mid-tool-call wedged its context permanently**: an assistant
  message with `tool_calls` no result ever answered, replayed from durable
  state on every later turn and every restart.
- **A dead turn worker never failed its step.** The guard asked whether the
  child was still in the table; the reap path removes it first.

**Security.**

- **The SSRF guard was decorative.** It resolved a name, classified the
  addresses, then discarded them — and the callers dialled by *name*, resolving
  a second time. Hostile DNS answered the check with a public address and the
  connect with `169.254.169.254`. The guarded URLs are exactly the model- and
  peer-supplied ones. Now resolve-once, dial the vetted address, re-assert on
  every entry at the syscall boundary; TLS/SNI and `Host` stay on the hostname.
- **Two more credential-bearing dials took their URL from the remote side** —
  the AAuth Person-Server consent poll (a signed, token-bearing GET repeated
  twice a second for five minutes at a PS-chosen address) and RFC 9728
  discovery (whose answer then chose the issuer for the authenticated flow).
- **`SubscribeToTask` attached to any task id** with no ownership check.
- **A `{{secret:…}}` that failed to resolve was silently dropped**, so the
  daemon started and dialled the model with the `authorization` header absent.
- **`subagent.run`'s `tools` narrowing was accepted and never enforced** — the
  grant was written into the spawn payload and read by nothing, so a parent
  bounding an untrusted sub-task got a child with the full catalogue.
- **A discovered `.agentd.yml` could lift the lethal-trifecta gate** — `cd` into
  a repo you cloned, run a flags-only `agentd`, and that repo's dotfile governed
  your grant. Discovery is a convenience and may no longer relax a security
  control; an explicitly named `--config` still can. *(Behaviour change for
  anyone relying on 2.1.0 discovery to configure security settings.)*
- **A non-loopback `webhooks.listen` with no auth was a warning**, while
  `a2a.listen` in the same situation was a hard error. Both are inbound
  listeners that trigger work. It is an error now, checked per route.
  *(Breaking: a public webhook listener with an unauthenticated route now exits
  `2`. Set `webhooks.default_auth`, or give the node its own `auth`.)*

**Correctness and robustness.**

- `cancel_scoped_children` matched nothing: element and branch children are
  keyed `parent[ix].step` / `parent{branch}.step`, which never start with
  `parent.`, so the `foreach`/`parallel` failure paths and the `race` timeout
  cancelled no child, disarmed no timer and dropped no pending entry.
- `timeout` on `race`, `join` and `workflow.wait` was silently dead — the parser
  stripped it before the handlers read it.
- A `schedule` with `at:` re-armed forever: a job asked to run once at 03:00 ran
  continuously from 03:00 onward.
- `Timers::fire` deleted the durable timer *before* checkpointing its effect,
  orphaning the suspended step on a crash in that window. Restore gained a
  repair pass for a `Suspended` step whose timer did not come back.
- The reactor did a synchronous MCP `resources/read` on the single-writer
  thread, stalling timers, checkpoints, drain and SIGTERM for up to the MCP
  timeout — on the subscription path, which is agentd's whole reactivity story.
- Compaction could leave an assistant message first, which the Anthropic dialect
  cannot send; the context is durable, so one bad fold poisoned every later turn.
- MCP elicitation could not work: the answer was a bare string (so every
  request became `cancel`), the notification stream dialled without the request
  signer, and an interleaved server→client request was buffered until timeout.
- The A2A task id counter reset to 0 on restart while tasks were restored, so a
  new message silently joined a restored task.
- The A2A `config` command returned the raw merged settings, credentials in the
  clear, over a remote protocol surface.
- Non-ASCII in a JSON/JSONC config was silently mojibake'd by a byte-wise
  comment stripper.
- The observation feed never signalled `resync` when a client's cursor was ahead
  of it, so display clients silently stopped receiving events after a restart.
- The HTTP request reader bounded neither header bytes nor header count.
- `principals::bare()` leaked a String per request on an attacker-controlled
  method name.
- `intelligence.budget.windows[].reset` panicked instead of exiting `2` on a
  multi-byte character.

### Changed

- The measured footprint is re-measured rather than inherited. Idle RSS is
  **5.5 MiB** on one thread with a schedule workflow and a file store, and one
  CPU jiffy per six seconds. The README claimed ~2 MiB and `why-rust.md`
  3.8–3.9 MiB; both predated the protocol SDKs and neither had been re-measured.
  The README's footprint table also drops its retired three-dependency row and
  its stale v1.0.0 attribution.

## v2.1.0 — a project config, and a build that needs no C toolchain

### Added

- **`.agentd.yml` is discovered.** An invocation that names no config — no
  `--config`, no `AGENT_CONFIG` — now loads `.agentd.yml` (or `.agentd.yaml`)
  from the working directory, the way a linter or a formatter finds its
  dotfile, so a project with a checked-in config stops repeating the flag.
  It is only ever a fallback: naming a config means you have decided, and the
  dotfile is not consulted, merged, or layered underneath it. Only the working
  directory — no walk to a parent, no `$HOME`, no `/etc`. Both spellings
  present is an error (exit `2`) rather than a silent pick between them.
  `--help`, `--version`, `--config-schema` and `--workflow-schema` never
  discover a config, so a malformed dotfile cannot stop you reading the help.
  (`docs/configuration.md` §12.1.)

### Fixed

- **The build no longer needs a C toolchain.** `connectrpc`, a hard dependency
  of `a2a-rs`, declared rustls, tokio-rustls and hyper-rustls with their default
  features, which selects the C/assembly `aws-lc-rs` provider — and because
  Cargo unifies features additively and globally, that one default applied to
  the whole graph however carefully agentd, `agentd-net` and `a2a-rs` itself
  asked for `ring`. It put `cmake`, `make`, `perl` and a C++ compiler in the
  builder image, and it hung the v2.0.0 release's cross-compiled amd64 job for
  **90 minutes** while the *emulated* arm64 job finished in three. A vendored
  `connectrpc` with three corrected dependency entries and no Rust source
  changes removes it (`third_party/connectrpc/PATCH.md`); `aws-lc-rs`,
  `aws-lc-sys` and `rustls-native-certs` leave the graph. `Cross.toml` existed
  only to install `cmake` and is gone, the Dockerfile is back to `musl-dev`
  alone, and cross-building arm64 now takes 2 minutes. CI asserts `aws-lc`
  stays out, from the job that installs no `cmake`.

  This does **not** reach `cargo install agentd-cli` or crates.io consumers of
  `agentd-core`: `[patch.crates-io]` is workspace-local, and a published crate
  cannot turn off a transitive dependency's features. Those builds still need
  `cmake` until the fix is upstream. Every artifact we ship is unaffected.

- **The v2.0.0 amd64 release asset was stale.** The `SHA256SUMS` and the
  `x86_64` tarball on the v2.0.0 release dated from an earlier build of that
  tag and did not contain the 2.0 runtime, so an amd64 user installing v2.0.0
  received the older binary while `--version` reported `2.0.0`. Both are
  rebuilt from the tag and replaced, verified by behaviour rather than by
  `--version`. arm64 was always correct.

- **The documented binary size was wrong** — 2.98 MiB, measured on that stale
  asset. The shipped binary is **6.57 MiB** on amd64 and **4.98 MiB** on arm64
  (3.00 / 2.79 MiB compressed).

## v2.0.0 — the durable agent: A2A, workflows, display clients, and protocols from their own SDKs

The rewrite lands. agentd 2.0 is a daemon you can attach to, hand work to, and
hold to account: durable workflows that survive a restart, an A2A surface other
agents and your own display clients speak, human-in-the-loop gates that reach
every attached screen, and — as of this release — MCP and A2A implemented by
`rmcp` and `a2a-rs` rather than by us.

**Breaking.** The 1.x execution modes and their flags are gone (each exits `2`
naming its replacement), MCP and A2A wire details moved to strict proto3 JSON,
and clustering was removed rather than finished. Read the three BREAKING
sections below before upgrading.

Crates step to `agentd-core` / `agentd-cli` **2.0.0** (`agentd-mcp` /
`agentd-net` **0.3.0**); the display clients ship as `@agentd-dev/cli` **2.0.0**;
the image is `ghcr.io/agentd-dev/agentd:2.0.0`. The MSRV is **1.96**, set by the
protocol SDKs.

### Removed (BREAKING: clustering and sharding are gone)

Shard identity, work-claim leases and the standby pool were declared in the
config schema, validated at startup, and **read by nothing**. They had been that
way for a long time; the honest options were to finish them or remove them, and
the premise turned out to be wrong for agentd, so they are removed.

Coordination needs a shared source of truth. agentd already talks to two that
are better placed to own it than a replica is — the MCP server the work comes
from, and the store. A queue can hand an item to somebody else when a lease
expires; no agentd-side hash can. So a fleet partitions **upstream**: one
subscription per replica, or the queue's own claim/lease semantics called from a
workflow step. Both are described with working config in `docs/scaling.md`.

Gone: the `cluster` build feature; `cluster.shard` and `cluster.timer_shard`;
`--shard`, `--claim`, `--claim-ttl`, `--claim-renew-fraction`, `--standby`,
`--assign-from` and their env aliases; `claim` and `shard` as fields of a
`subscribe` start node; and the `agent_shard_skipped_total`, `agent_claims_*`
and `agent_saturation` metrics, which were reserved names flat at zero. Each
removed flag now exits `2` naming what to do instead. RFC 0019 is marked
**Withdrawn**, with the reasoning kept.

One thing this gives up honestly: in a fleet, three replicas arming the same
nightly `schedule` will run it three times. That is a real problem, and the right
place to solve it is one line of deployment config — replica 0 arms the schedule,
the others do not — rather than a hash inside the agent.

### Added (push notifications and the authenticated agent card)

- **Push notifications.** A caller registers a webhook and agentd POSTs its
  task's updates there instead of the caller holding a stream open. Default-OFF
  at two levels, because the URL comes from a peer: `a2a.push.enabled` says you
  will make the request at all, and `a2a.push.allow_private` — a separate and
  larger decision — says you will make it to a private or loopback address. The
  target is guarded at registration *and* again at delivery, since a name can
  resolve somewhere new in between. Configs are durable with the task, so a
  restart keeps the promise the caller was given.
- **`GetExtendedAgentCard`.** The authenticated card: the same document, with
  the skills this caller may actually run rather than every workflow. An
  anonymous caller gets `-32007`.
### Fixed

- **The agent card named no interface.** It carried the older flat
  `url`/`preferredTransport`, and nothing in `supportedInterfaces` — so a peer
  reading the card parsed it happily and had nowhere to send anything.
  Discovery is the card's whole purpose; the interface is now there, and the
  spec oracle asserts it.
- **`SendStreamingMessage` sometimes did not stream.** A command DataPart was
  answered with a JSON body, so a caller that asked for SSE could not parse the
  reply. A streaming method answers with a stream whatever the message contained.
- **`shard` on a `subscribe` start is refused rather than ignored.** It promised
  partitioned deliveries and filtered nothing, so a fleet built on it processed
  everything N times while looking configured. Failing to start is the honest
  answer; `docs/scaling.md` says what to use instead.

The spec oracle grew from 8 checks to 13 — discovery, capability honesty,
cancellation, stream resumability and the push-config surface — which is how the
first two fixes above were found.

### Changed (BREAKING: MCP and A2A are the published implementations now)

agentd used to implement both protocols itself. It does not any more, and the
reason is worth stating: a protocol written from your own reading of a
specification fails **silently, in the peer** — your tests encode the same
reading as your code, so they agree with it, and what finally disagrees is
somebody else's client, reporting nothing. Checking agentd's A2A output against
an independent implementation found four such faults in an hour (see the entry
below); that was the argument.

- **MCP is [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)**, the
  official Rust SDK: the handshake, the typed requests and notifications,
  capability negotiation, the streaming rules, the error mapping, the version
  table. It is not a feature you can turn off — there is no second
  implementation to fall back to; the hand-rolled client, the era probe and the
  modern-dialect implementation are gone, and so is the `rmcp-client` flag.
- **A2A is [`a2a-rs`](https://github.com/emillindfors/a2a-rs)**, generated from
  the specification's protocol buffers: method dispatch, the typed shapes, SSE
  framing, the blocking-send rule, the error codes. agentd supplies the ports —
  identity, the authorization matrix, and the durable tasks the reactor owns.
- **The socket stays agentd's.** Both SDKs run over agentd's own HTTP transport,
  so nothing that was working stopped: AAuth request signing with its
  challenge/re-sign loop, AWS SigV4, mTLS client identities, OAuth refresh, the
  SSRF guard. There is no split fleet and no fallback path.

Two bugs surfaced during the migration, both of the quiet kind: the SDK's
notification hooks were not connected to agentd's wake path (a reactive daemon
that never wakes), and the SDK ran on a current-thread runtime that only
advances inside `block_on` (so dispatch happened only while agentd was already
busy). Both fixed.

**What this costs.** The dependency graph went from 26 crates to 182 in the
shipped feature set, and the MSRV moves to **1.96**, which `a2a-rs` sets. The
build stays pure Rust: `connectrpc` (under `a2a-rs`) asks for rustls with
default features, which would have selected the C/assembly `aws-lc-rs` provider
for the entire graph, so a vendored copy with three corrected dependency entries
keeps it on `ring` — see `third_party/connectrpc/PATCH.md`. Building this
repository needs no `cmake` and no C++ compiler; building `agentd-core` *from
crates.io* still does, because a published crate cannot turn off a transitive
dependency's features. The CI job that asserted three direct dependencies is
retired, replaced by one that guards what a user actually receives: the release
binary is still a statically linked musl artifact on `scratch` — about 6.5 MiB,
no shell, no libc, nothing to scan but agentd itself — and asserts `aws-lc` has
not come back. The docs that claimed the old posture (`README`,
`architecture.md`, `why-rust.md`, `mcp.md`, the landing page) say this instead.

**What this changes on the wire.** The modern/stateless MCP revision now follows
rmcp's schedule — it pins `LATEST` to the legacy revision today, and agentd gains
the stateless one when the SDK does. `configuration.blocking` was never an A2A
field; the spec spells it `returnImmediately`, and the listener translates so
existing clients keep working.

### Fixed (the A2A wire was not quite proto3 JSON — a second reader found it)

A2A is defined in protocol buffers, and its JSON binding is proto3 JSON. agentd
followed that for task states (`TASK_STATE_*`) but had drifted elsewhere, in the
way that is hardest to notice: every one of these parses fine as *JSON* and is
rejected by a peer's *generated types*, in production, with no error we would
ever see.

- **`role` is `ROLE_AGENT`, not `"agent"`.** Every agent-authored `Message` —
  task status, streaming status frames — used the English word. A peer built
  from the schema could not deserialize any of them. (The outbound A2A *client*
  already sent `ROLE_USER`, so the two halves of agentd disagreed.)
- **`status.timestamp` is an RFC 3339 string, not epoch milliseconds.** It is a
  `google.protobuf.Timestamp`; an integer is a type error to a peer.
- **`ListTasks` returns `Task`s.** It was returning agentd's internal record —
  a flat `state`, plus `principal`/`link`/`updated` — so the one method whose
  whole job is enumerating tasks emitted objects a peer could not read as tasks.
  The result now also carries `totalSize`, `pageSize` and `nextPageToken`, which
  `ListTasksResult` requires.
- **`Task.history` is `repeated Message`**, so agentd's state transitions no
  longer sit there pretending to be messages. Everything agentd wants to say
  that the spec has no field for now lives under `metadata`, namespaced:
  `agentd/principal`, `agentd/link`, `agentd/statusHistory`, `agentd/created`.

The TUI and web clients read the new shape and still accept the old one.

### Added (an independent reader for the A2A spec)

- **`crates/a2a-oracle`** boots the real daemon, drives its real A2A listener,
  and parses every response with [a2a-rs] — an unrelated implementation of the
  same specification, by a different author, with types derived from the
  published schema. Our own tests were written from our own reading of the spec
  and so cannot catch a *plausible* misreading; this is a second reader that
  can, and it is what found everything in the section above. It is deliberately
  **outside the workspace** (it brings ~180 crates, which have no place in the
  `FROM scratch` static build) with its own target dir, and runs
  as its own CI job: `cargo test --manifest-path crates/a2a-oracle/Cargo.toml`.
- The conformance suite gained
  `a2a-conversation/tasks-are-proto3-json-on-every-path`, so the shapes stay
  fixed by the fast suite in ordinary CI.

[a2a-rs]: https://github.com/emillindfors/a2a-rs

### Fixed (`--validate-config` missed workflow errors — and the docs had some)

- **`--validate-config` now parses workflow definitions**, the same strict parse
  the runtime runs at startup. It used to check only the config *around* the
  workflows: a typo'd step field (`prompt:` on an `agent` node, which takes
  `instruction`) validated **clean** and then exited `2` on the first real
  start. A pre-flight check that passes what production refuses is worse than
  none — this is the split it existed to prevent. Structural errors are still
  reported first, so the more basic message keeps leading.
- **Audited every command and workflow sample in the docs against the binary**
  (flags against `--help`, node fields against `--workflow-schema`, whole blocks
  through `--validate-config`) and fixed what was wrong: `prompt:` on `agent`
  steps, `writes:`/`input:` on `agent` (they belong to `assign` / nowhere),
  `rate:` on `foreach` (a `batch` field), `min_success:` on `parallel` (a `race`
  field), `emit_url_to` on `wait` (never existed), `every:` on `loop` (it is
  `interval`; `every` is `schedule`'s), and `debounce:` on `subscribe` (it is
  `debounce_ms`). `docs/scaling.md` still taught the removed 1.x
  `--mode`/`--continue`/`--claim` flags for continue-claim; it now shows the 2.0
  `subscribe` + `deliver: wait` + `claim` shape. Every workflow block in the docs
  now passes the real validator. (`--model` was suspected and is fine — it is a
  documented alias for `intelligence.model`.)

### Changed (landing page: each idea in one place)

- The MCP/A2A pitch appeared three times — hero, "shape of a run", and its own
  section — so the page repeated itself instead of building. The hero now says
  what agentd *is*, "shape of a run" covers the two-loop architecture, and the
  protocol section owns where capability comes from. The example model id was
  refreshed too.

### Added (`--prompt`: a one-shot job, or a self-setup)

- **`agentd --prompt "…" --intelligence <url>`** asks, answers on stdout, and
  exits — the shape people expect from an agent CLI. `--prompt-file` and
  `AGENTD_AGENT_PROMPT` work too, and `--instruction` still means the standing
  policy: give both and the instruction is the system prompt while the prompt
  is the task.
- **The prompt is delivered as a message into the agent's root context**, not
  as a synthesized workflow step, and that distinction is the feature:
  workflow-authoring tools are root-scoped, so a prompt running as a step could
  never build what it was asked for. A prompt may therefore **set the instance
  up** — "check the queue every 30 seconds from now on" has the agent
  `workflow.create` its own `loop`/`schedule`/`subscribe` workflow.
- **`lifecycle.run_until: auto` now re-reads the live workflow set.** It used
  to decide "job or daemon" once, at startup, from the *configured* workflows —
  so an instance that armed a long-lived workflow at runtime idle-exited out
  from under it moments later. A self-set-up agent now stays up, and a plain
  one-shot still exits.

### Changed (the display clients ship as one package: `@agentd-dev/cli`)

- **Three npm packages became one.** `@agentd/client`, `@agentd/tui` and
  `@agentd/ui` are now a single published package, **`@agentd-dev/cli`**, whose
  `bin` provides both `agentd-tui` and `agentd-ui` and whose library entry point
  is the shared thin-client core. A display client is one install, and the core
  can no longer skew against the UIs that render from it. `interface/` is a
  plain package (`src/{client,tui,ui}`), not a workspace.
- **`agentd tui --inline` works.** The passthrough forwarded only `--debug` and
  `--no-open` to the client, so the documented `--inline` fell through to the
  daemon and died with `unknown argument: --inline`.
- The "cannot start the client" hint and `agentd --help` now name
  `npm install -g @agentd-dev/cli`.

### Added (getting it onto a machine: installer, skill, security policy)

- **`install.sh` is served from the domain it advertises.** The site build
  publishes the repo-root script at `https://agentd.dev/install.sh` (it used to
  404), so `curl -fsSL https://agentd.dev/install.sh | sh` works. The script now
  matches what the release actually builds — it accepts **arm64 Linux** (which
  was refused despite being published) and no longer offers a macOS binary that
  was never built, pointing at a source build instead. It **verifies the
  download against the release `SHA256SUMS`**, takes `--version` / `--dir` /
  `--no-verify` as flags or `AGENTD_*` env vars, and refuses rather than
  half-installing when a checksum mismatches.
- **`skills/agentd/`** — an Agent Skill that teaches an AI coding assistant to
  install, configure, run and debug agentd, with `reference/config.md` and
  `reference/coding-agent.md` for detail. Drop it in `~/.claude/skills/`. (Not
  to be confused with agentd's own `skills:` config, which discovers instruction
  bundles from MCP servers — RFC 0028 §7.)
- **`SECURITY.md`** — private reporting, and an explicit boundary list: what
  counts as a vulnerability here (an `exec` fence escape, a secret in a log, a
  trifecta bypass, unauthenticated access to the A2A listener) versus what does
  not (a model using powers you granted it).

### Added (`-c` as a short `--config`, and `=` value forms)

- **`-c a.yaml`, `-c=a.yaml` and `--config=a.yaml`** now work everywhere
  `--config a.yaml` did, including through the `agentd tui|ui` passthrough.
  Previously only the space-separated long form parsed, so the documented
  `agentd ui -c=code.yml` failed with `unknown argument`.
- **`agentd --help` documents the `tui` and `ui` subcommands**, which were
  implemented but invisible in the help text.

### Changed (TUI renders fullscreen by default)

- `agentd-tui` now takes over the terminal (the **alternate screen**), so the
  layout is stable and the shell is restored on exit. Because that buffer has
  no scrollback, the client owns it: **PgUp/PgDn** scroll a bottom-anchored
  transcript viewport, a hint reports what is above the fold, and new messages
  follow the tail unless you have scrolled up (then your position holds).
- **`--inline`** (or `AGENTD_TUI_INLINE=1`) keeps the previous behavior —
  settled rows ride Ink's `<Static>` into the terminal's real scrollback and
  survive quitting. Non-interactive runs (pipes, CI) degrade to inline
  automatically (Ink gates the alternate screen behind an interactive TTY).

### Added (live activity — RFC 0032 §17)

- **The working row now says what the agent is doing**: `thinking · 12s ·
  1.2k tok · round 2`, `read_file · 3s`, `waiting · subagent · 40s`. The turn
  worker's coarse progress frames (`AgentMsg::Event`) were previously dropped
  by the supervisor; they now fold into a per-unit activity record (phase,
  tool, round, tokens, `started_ms`) published as `activity` /
  `activity.removed` feed events and mirrored in `status.activity`. New
  child-side signals `turn.think` / `turn.tool` (the only way an MCP tool call
  is visible to the supervisor — the child holds its own connections) and
  `turn.round` now carries per-round usage; a deferred tool parks the unit.
- Deliberately coarse: events fire only on a change an operator would notice,
  and elapsed is never streamed (clients tick from `started_ms`), so a long
  think emits nothing and the feed's replay ring stays a record of state.
  `activityLine()` in `@agentd-dev/cli` renders it identically in both UIs.

### Added (human-in-the-loop + steering — RFC 0032 §16, RFC 0029 §5/§7)

- **`ask_human` is real** (was a stub): an ask — the model's tool call, or a
  workflow `human` step — flips the owning A2A task to `input-required` with
  the question as its status message; every attached display client renders an
  answerable gate, and a `SendMessage` carrying the `taskId` resolves the
  suspended asker with the reply (tool result to the model; step output to the
  workflow — later steps template on `steps.<gate>.output`). Answers broadcast
  on the feed; ask + answer are audited. Run-linked gates are rebuilt after a
  restart (durable HITL); turn gates degrade to conversation continuation.
  Cancelling a gate unblocks its asker with an error.
- **`agent.ask_human_fallback`** when no human channel exists (and, for
  `auto`, when a rendered gate times out unanswered): `fail` (default),
  `wait` (park until the ask timeout), or `auto` — an LLM judge answers on
  the operator's behalf (conservative prompt, `UNDECIDED` ⇒ fail), always
  marked as auto in the task/log/audit.
- **Steering command ops now dispatch** (were `UNSUPPORTED_OPERATION`):
  `workflow.signal` (resumes `wait: {on: signal}` steps), `subagent.send`
  (inject into a warm subagent), `subagent.kill`, `subagent.status`,
  `plan.get`.
- **`a2a.pause` / `a2a.resume`** (operator): with `{run}`, flip one run
  between Paused and Running; without, hold the WHOLE instance — intake
  continues (inbox, tasks), no new turns dispatch and no steps schedule until
  resume. Reversible, unlike drain; a paused instance never idle-exits;
  `status.paused` + a `lifecycle` feed event keep every client honest (the
  UIs show PAUSED).
- Clients: `/signal /send /pause /resume /plan` in both UIs (+
  `signal()/subagentSend()/pause()/resume()/planGet()` on the client core).

### Added (the display-client interface & observation plane — RFC 0032, `docs/interface.md`)

- **`interface:` config** (default OFF): `enabled` serves the display-client
  surface on the existing A2A listener; `debug` exposes the extra reads;
  `origins` allowlists hosted web-UI origins (CORS + preflight; loopback
  origins always pass; every other cross-site origin stays 403).
- **`SubscribeToEvents`** — the global observation feed (SSE): a seq-cursored,
  principal-scoped event ring (`hello`/`event`/`goodbye`) carrying task
  transitions (with artifacts), every NL prompt (`message` — the cross-client
  transcript), mutating commands, run/conversation/subagent/child/status
  section deltas (a 4 Hz fingerprint diff in the loop), lifecycle, and (debug)
  audit records. N attached clients converge with no client-to-client sync;
  reconnect resumes from the cursor.
- **Taskless interface reads** (command ops that create no durable task):
  `interface.info` (discovery — the client keys its debug panes off the
  daemon), and under `debug`: `conversation.get` (transcripts with message
  bodies), `run.get` (per-step run detail), `debug.events` (the live log ring,
  revived from RFC 0016 §7.2 and installed when debug is on).
- **`agentd tui` / `agentd ui`** (unix): run the daemon AND its display client
  as one command — forces `--interface.enabled` (argv, reload-safe), redirects
  daemon logs to a file, hands the tty to the client (`AGENTD_TUI_BIN`/
  `AGENTD_UI_BIN` override), ties lifetimes (client exit ⇒ graceful drain).
- **`interface/` — the display clients** (a separate Node package,
  `@agentd-dev/cli`; the Rust 3-dependency moat is untouched): the shared
  thin-client core (wire, task normalization, the event-sourced Mirror, the
  Observation driver with automatic poll fallback), the `agentd-tui` Ink
  terminal UI (chat, tasks, daemon-gated debug screen; degrades to a read-only
  view without a tty), and the `agentd-ui` web UI in the format of the TUI
  (statically hostable, local server with `--open`).
- The agent card advertises `urn:agentd:interface` when enabled;
  `--capabilities` reports the interface block and the extra methods/ops.
- **Pairing-code login** (`interface.pairing`, RFC 0032 §13): the daemon
  derives a 6-digit code per 60 s window (HMAC over a per-process
  `/dev/urandom` seed); an operator reads it (`pairing.code` / the TUI's
  `/pair`), and an UNAUTHENTICATED client exchanges it (`Pair {code}` — the one
  anonymous method) for an in-memory session token (constant-time verify,
  previous-window grace, 5-fail/window lockout, configurable role
  operator|user + TTL, revoked on restart). Clients: `agentd-tui --code`, the
  web connect form's code field. On a non-loopback listener pairing counts as
  client auth (validation + 401→anonymous admission).
- **Daemon-driven client chrome** (`interface.display`, RFC 0032 §12): ordered
  item lists for the top/bottom edges every attached client renders (`name
  version instance model endpoint conn debug draining active turns tokens
  tool_calls runs subagents conversations screen keys clock`); served in
  `interface.info`, defaults preserved, unknown items warn + are skipped.
- **Runtime `config.set`** (operator, RFC 0032 §14): whitelisted knobs only —
  `interface.debug` (live debug toggle; installs the log ring) and
  `interface.display.top|bottom` — echoed as a `config` feed event so every
  client re-shapes at once; everything else names the whitelist and stays with
  the config file + SIGHUP (deliberate: no wire path forks config provenance).
- **`subagent.get {handle}`** (debug): one subagent's drill-down — instruction,
  status, mode, attempts, tokens, truncated result/error, requested_by —
  behind the new subagents screen (TUI: list → Enter → detail → Esc back;
  web: clickable rows → detail → back), fed live by `subagent` events.
- **Composer affordances** (shared client-core rules, both UIs, with
  as-you-type suggestions): `/` system commands **plus every workflow as a
  shortcut**; `@skill` catalogue completion (inline — agentd preloads
  references); leading `#task-…`/`#ctx` message targeting (answer a specific
  input-required gate / address a conversation); `$model $instance $version
  $turns $tokens $tasks` live-value interpolation (`$$` escapes). New TUI
  commands: `/pair`, `/set`, `/config [path]`, `/subagents`; `status_value`
  now carries `model`.

### Added (configuration mechanism — RFC 0017 §3, `docs/configuration.md` §1.1/§12)

- **YAML config files.** `--config <file>` accepts **YAML** (`.yaml`/`.yml`)
  alongside JSON/jsonc (`.json`/`.jsonc`; other extensions are sniffed). YAML is
  read by a hand-rolled, dependency-free subset reader (`config::yaml`) —
  mappings, sequences, flow collections, quoted/plain/block scalars, comments,
  YAML 1.2 core typing — into the same document the JSON path yields, so
  validation, `--config-schema`, `--validate-config` and **hot reload** (SIGHUP
  / `--watch-config`) are format-agnostic. Anchors/aliases, tags, merge keys,
  multi-document streams, tabs and duplicate keys are rejected with a
  line/column error. The default build stays at exactly three direct deps.
- **Path-derived env vars.** Every config-file path is an env var named after
  the path: `limits.max_steps` ⇒ `AGENTD_LIMITS_MAX_STEPS` ›
  `AGENT_LIMITS_MAX_STEPS` › bare `LIMITS_MAX_STEPS` (first present wins);
  values are typed by the schema (integers, enums, `[a, b]` / comma lists,
  `{k: v}` objects). Derived from the config schema, so a changed parameter set
  needs no per-field plumbing. The named `AGENT_*` variables keep working.
- **Generic path flags.** Every config-file path is a flag too —
  `--limits.max-steps 5` (also `--limits-max-steps`, `--limits.max_steps`),
  applied in argument order like any flag; a dotted flag reaches into a
  free-form map (`--intelligence_headers.x-team ops` sets one entry, exact
  spelling kept); `agentd --help` lists the full `CONFIG PATHS` table (path ·
  flag · env). Precedence is unchanged: `built-in < file < env < flag`.
- **Multiple config files.** `--config` is repeatable and `AGENT_CONFIG` takes a
  `:`-separated list; the files compose into one document **in order, a later
  file overriding the earlier ones** (JSON Merge Patch, RFC 7396: objects merge,
  scalars/lists replace, `null` unsets). Each file is type-checked on its own so
  an unknown key names its file; `config.loaded` lists the merged
  `config_files`; `--watch-config` watches every file.
- **Set vs add.** Setting a path from env or a `--<path>` flag *sets* its value
  (a list/map path replaces the files' value); the named repeatable flags
  (`--mcp`, `--subscribe`, `--a2a-peer`) keep adding one element.

### Added (agentd 2.0 track — in development, RFC 0025–0030)

- **Config schema v2** (`config::v2`, RFC 0030): the nested settings document
  (`agent` / `intelligence` / `mcp` / `tools` / `store` / `memory` / `context` /
  `knowledge` / `search` / `skills` / `workflows` / `limits` / `lifecycle` /
  `a2a` / `observability` / `security` / `cluster`) with a hand-written JSON
  Schema (`--config-schema=2`), path-derived env/flags (`AGENTD_INTELLIGENCE_MODEL`,
  `--limits.run.steps`), an alias table for the 1.x quickstart flags, v1/v2
  detection, collected validation (`--validate-config`), and the
  `--instruction` sugar workflow. A v2 document selects the 2.0 runtime (below);
  `--capabilities` for a v2 configuration lands with the A2A v2 phase.
- **Durable state core** (`store`, `state`; RFC 0025): the 4-op remote-store
  contract (`put(key, seq, envelope)` with a seq compare-and-set, `get`, `list`,
  `delete`) with adapters over **MCP tools** (the default checkpointer profile
  `state.put/get/list/delete`, or any server via template/CEL argument and
  result mappings), **HTTP** (templated REST with `Idempotency-Key` and
  secret-ref headers) and an in-process **memory** store; envelope v2 under
  `<prefix>/<instance>/<kind>/<id>`; the entity model (manifest with a live
  index + generation, write-ahead inbox, timers, contexts, runs, subagents,
  tasks, artifacts, memory, audit); the `Durable` façade — per-key sequence
  tracking, a conflict on an owned key is a fatal split-brain signal, debounced
  manifest flush, `store.on_error: halt|degrade`, and the restore protocol
  (manifest → indexed entities → `list` reconciliation → generation bump);
  dependency-free ULIDs. The built-in mock MCP server (`--internal-mock-mcp-http`)
  gained `state.list {prefix}`, `state.delete`, `structuredContent` results
  and `mock.fault`/`mock.ops` for chaos tests; `AGENTD_TEST_KILL_AT=<point>`
  (debug / `internal-mocks` builds) SIGKILLs the process at a named kill point.
- **The 2.0 runtime — agent loop v2** (`runtime`, `registry`, `context`,
  `governor`, `engine`, `jsonschema`; RFC 0026–0028, in development): a v2
  configuration document (or `--config-version 2` / `AGENTD_CONFIG_VERSION=2`
  with the quickstart flags) now runs the **new event loop** — a single-writer
  reactor over durable state with a **flat child tree** of **turn workers**
  (one model turn per child; internal tools round-trip to the supervisor as
  `ToolRequest`/`ToolResult`; budget admission per model call) and subagents.
  Landed: the **tool registry** (every RFC 0028 §3 internal contract with input/
  output JSON Schemas and grants; internal > code > MCP; `tools.overrides` map a
  contract onto any MCP tool with template/CEL args+result mappings, validated at
  startup; `tools.disabled`; knowledge/search profile servers); **contexts**
  (root + per-conversation durable transcripts, structured summaries, the
  conversation **plan** (`plan.*`), **compaction** by a summarizer think,
  **skills** discovered from MCP prompts/resources with `@skill:` preloading,
  **memory** KV with TTL, artifacts); the **conversation preflight**
  (`agent.preflight`) with `status`/`clarify` short-circuits, plan seeding and
  skill preloading; **knowledge auto-context** (`knowledge.auto_context`); the
  **token governor** (`intelligence.budget`: unit-aligned windows incl. calendar
  resets, requests + tokens, sub-budgets per conversation/run, tactics `wait |
  slow | degrade | refuse | fail`, the lifetime ceiling, counters durable in the
  manifest); the **workflow engine core** (dialect-3 model + strict validation
  over the full RFC 0027 catalogue, `{{path | default}}`/`CEL:` templates,
  durable run records, the scheduler with `when` guards and `on_error`
  fail/continue/goto, retries with backoff, `once`/`manual` start nodes, the
  step kinds `noop checkpoint assign transform template validate assert fail
  emit finish sleep tool memory.* artifact.* knowledge.* search.* mcp.tool
  agent think` — the rest of the catalogue lands with the P4 engine); the
  **subagent registry** (`subagent.run` sync/async/detached/warm, caps, the
  trifecta gate, durable records + re-spawn on restore); durable **timers**;
  **restore** of runs (running steps replayed with the same idempotency key),
  contexts, subagents, timers, artifacts and pending inbox events; the
  **lifecycle** (`lifecycle.run_until`: job shape ⇒ idle exit with the finish
  status mapped to the RFC 0011 exit code; daemon ⇒ SIGTERM drain via the kill
  ladder); **hot reload** of the v2 reloadable partition on SIGHUP /
  `lifecycle.watch_config` (intelligence, budgets, instruction, MCP servers,
  registry, skills, workflow definitions — live runs stay pinned to their
  hash; restart-only paths refuse); the instruction as static text or an MCP
  resource (read + subscribed, re-read on update); `context.model_window`.
  Test scaffolding: mock-LLM `file:<playbook.json>` scripts, mock-MCP
  `knowledge.*`/`search.*` profiles + skills as prompts/resources,
  `AGENTD_TEST_INBOX_FILE` (debug builds) to inject inbox events; e2e suites
  `runtime_v2_e2e.rs` (job through the loop, overrides, SIGKILL/restore across
  four lives, preflight+skills+knowledge conversation, status intent +
  subagent delegation, budget `fail` exit 7, compaction + restore) and
  `runtime_v2_reload_e2e.rs` (SIGHUP apply / restart-required / SIGTERM drain).
  A2A v2 intake/commands/principals now land the HTTPS listener (see the A2A v2
  transport binding entry below). Not yet: the mode cut-over (removing the 1.x
  surfaces), `--capabilities` for v2, docs.
- **Workflow engine v3 — the full node catalogue + start-node triggers** (RFC
  0027, in development): the dialect-3 workflow language now executes end to end
  through the 2.0 runtime. Nested bodies — `foreach`/`batch` (bounded
  parallelism, `rate` pacing, per-batch durable progress that resumes after a
  crash, positional `collect`, `on_error: continue` element slots, `batch.by`
  grouping, artifact-backed item lists), `iterate` (`while`/`until`/
  `max_iterations`), `parallel` (fan-in object, `min_success`), `race`
  (first-branch-wins with cancellation + timeout), `subgraph`; `switch`
  routing; the pure data steps `map`/`filter`/`reduce`/`sort`/`dedupe`/
  `chunk`/`parse` (CEL or `{{…}}` element expressions; CSV/YAML/JSON/lines);
  orchestration steps `wait` (on a resource update, a CEL condition, a signal,
  a run, a subagent, a conversation message or a deadline), `join`, `workflow`
  (a child run — `sync`/`async`/`detached` with `cascade`),
  `workflow.signal`/`wait`/`cancel`, `subagent`, `human`, `mcp.resource`
  (read/list/prompt/complete/templates) and `a2a.delegate` (the RFC 0020 client,
  feature-gated); the `think` presets `classify`/`extract`/`summarize`/`judge`/
  `route`; and per-step `cache` (memoized by input hash). Large step outputs
  spill to artifacts (`{"$artifact": id}`) and dereference transparently in
  templates. **Start nodes as triggers**: `loop` (re-run on completion with
  `interval`/`until`/`max_iterations`/backoff), `schedule` (5-field cron /
  `every` / one-shot `at`), `subscribe` (an MCP resource update, notify-then-read,
  `debounce`/`coalesce`/`filter`), `signal` and `event` (`workflow.finished`/
  `failed`) — start-node state (last fired, iteration, next deadline, debounce)
  is durable in the manifest, and the reactor tick is now adaptive to the
  nearest deadline so time-based work fires promptly. The run registry gained
  concurrency policies (`queue`/`drop`/`replace`), pause/resume, cascade
  cancellation, the `workflow.*` tools and `--workflow-schema`; restored runs
  replay their in-flight steps with the same idempotency key.
- **A2A v2 transport binding** (RFC 0029; `--features a2a`) — the 2.0 daemon's
  only external channel. When `a2a.listen` is set, the runtime binds the real
  HTTPS listener and turns A2A JSON-RPC into runtime work: **principals & roles**
  (mTLS / bearer → `operator | user | agent | anonymous`) with a `may` /
  `may_command` authorization matrix; **natural-language messages** become
  conversation turns whose answer lands as the task artifact; **command
  DataParts** (`{"data":{"agentd":{"op":…}}}`) run `status` and `workflow.run` /
  `workflow.status` / `workflow.cancel`; **durable tasks** (`Kind::Task`) that
  survive a restart, with `GetTask` / `ListTasks` / `CancelTask` (ownership
  scoped) and `SendStreamingMessage` / `SubscribeToTask` streaming
  `working`→status/artifact→terminal frames; the operator admin family
  (`a2a.drain` / `lameduck` / `cancel`) and a public `GetAgentCard` (workflows
  advertised as A2A skills). The listener runs off the single-writer loop —
  requests cross as a new loop event and blocking/streaming reads are served
  from a shared task snapshot the loop keeps current, so the loop never stalls.
  A task started by `workflow.run` tracks its run to completion. Built additively
  beside the 1.x surfaces (removed at the P5 cut-over). Current limits: the serve
  framework surfaces only whether a client cert was presented (no SAN/subject),
  so `san`/`sub` principal matchers need a bearer and mTLS conveys *operator*
  until the subject is exposed; command DataParts cover the `status`/`workflow.*`
  subset (the NL path reaches every internal tool); `pause`/`resume` admin is a
  stub. The default build is unaffected (feature-gated; deps unchanged).
- **Audit stream** (plan §3.11) — an append-only *who-did-what* trail:
  `{ts, instance, principal, role, action, target, outcome, request_id, trace}`
  to the configured `observability.audit.sink`s — `log` (a closed-vocabulary
  `audit` log line) and/or `store` (a durable, ULID-keyed, append-only
  `Kind::Audit` record that is never rewritten). Every A2A call is audited with
  its principal, method + command op, and outcome; config reloads and durable
  restores (with the `lost`-entity count) are audited too. Off by default (no
  sink configured ⇒ no overhead).
- **Observability serving in the 2.0 runtime.** The Prometheus `/metrics`
  surface (`observability.metrics_addr`, `metrics` feature) and the health-file
  liveness heartbeat (`observability.health_file`) are now started by the 2.0
  runtime (their 1.x wiring was removed with the mode cut-over).
- **OTEL traces from the 2.0 turn worker** (`otel` feature): a turn opens an
  `invoke_agent` span with a `chat` child span per model call and an
  `execute_tool` child span per tool call, exported as one OTLP trace — matching
  the 1.x agent loop's tracing on the 2.0 path.
- **OTLP logs export** (`otel` feature, `observability.otel.logs`): the JSON-lines
  log surface is mirrored to `<endpoint>/v1/logs` by a bounded background buffer,
  hooked at the logger's single emit point (a no-op when unarmed).
- **Per-child cgroup placement in the 2.0 runtime** (`security.cgroup`): the
  runtime arms the process-tree cgroup at startup, and every spawned child (turn
  workers + subagents) is placed in its own leaf with the configured
  `memory.max`/`pids.max` and `cgroup.kill` atomic teardown on reap.
- **v2 runtime metrics** (plan §3.11): `agent_turns_total{kind}`,
  `agent_steps_total{status}`, `agent_store_ops_total{result}` +
  `agent_store_latency_ms_sum`, and the `agent_inbox_pending` /
  `agent_context_tokens` gauges — bounded closed-label series on the existing
  `/metrics` surface.
- **`agent://` read surface over A2A.** The A2A `status` command already returns
  a comprehensive runtime view (store, workflows, runs, conversations, subagents,
  timers, inbox, budget, tools, skills, counters); added a `config` command that
  returns the effective merged configuration (secret **references**, never
  values), operator-gated.

### Changed

- **The mode cut-over: agentd 2.0 is v2-only.** The 1.x mode drivers and the flat
  v1 schema are gone (~32,000 lines removed). The CLI routes every configuration
  through the 2.0 runtime (`runtime::run`); a 1.x document — the flat schema or a
  `--mode` invocation — is rejected at the gate with a migration hint (exit 2).
  Removed the entire v1 surface: the mode drivers (`once`/`loop`/`reactive`/
  `schedule`/`workflow`), the v1 self-MCP server and its v1 A2A binding (replaced
  by the RFC 0029 A2A v2 listener; the A2A client wire helpers were preserved),
  the v1 capabilities manifest + run-report backends (replaced by `--capabilities`
  over the v2 loader + the durable A2A task model), RFC 0019 cluster sharding, the
  v1 cyclic-workflow graph engine (superseded by the v3 engine), the in-child
  orchestrator + the v1 supervisor reactor/gate/restart/swap, and the v1 lifetime
  budget ledger (superseded by the v2 governor). **Subagents are now RFC 0026
  flat-tree leaves** — a subagent runs a ReAct loop over its granted MCP/code tools
  and reports its result, with no in-child nesting/scheduling/delegation (that is
  the reactor's job). The `serve-mcp` / `serve-https` / `events` build features were
  retired (`a2a` now rides `tls` directly — the v2 listener needs no v1 server);
  `cel` no longer implies `workflow`. **Zero change to v2 behavior** (verified by
  the full v2 e2e suite, incl. subagent spawn/delegation, staying green). The
  black-box conformance suite was reduced to its v2-viable families (supervisor,
  security); a full v2 conformance rebuild (A2A, the durable store, the v3 engine)
  is planned. The now-unreachable v1 `Config` (and `config::Mode` + the vestigial
  `workflow` feature) remain as dead code, slated for a follow-up deletion.
- `config.rs` / `config_file.rs` / `config_watch.rs` moved into a `config/`
  module directory (`config::{file, yaml, paths, watch}`); the public
  `agentd::config` surface is unchanged. Config-layer error messages now name
  their source (`config file` / `env` / the flag).

## v1.4.0 — OpenAI-compatibility fixes + AAuth provider/token validation

Real-provider hardening. Proving the eval harness (RFC 0024) against live OpenAI
surfaced three genuine compatibility bugs in the intelligence dial — all fixed —
and the AAuth agent-identity client gained the provider/token validation the
protocol calls for. The stock binary's behavior is otherwise unchanged; crates
step to `agentd` / `agentd-cli` **1.4.0** (library crates `agentd-mcp` /
`agentd-net` stay **0.3.0** — unchanged).

### Fixed (intelligence dial — OpenAI/Anthropic compatibility)

- **Dotted tool names are sanitized on the wire.** OpenAI/Anthropic reject tool
  names that aren't `^[a-zA-Z0-9_-]+$`, but agentd uses namespaced names
  (`resource.read`, `subagent.spawn`). Each name is now made wire-safe on the
  outbound request (in the tool defs *and* the assistant message-history
  `tool_calls`) and reverse-mapped on the response, so tool-calling against real
  OpenAI works and routing is unaffected. No-op when every name is already legal.
- **Reasoning models use `max_completion_tokens`.** gpt-5 / o-series reject
  `max_tokens`; the request now selects the correct token-limit parameter by
  model, so those models produce tool calls instead of an HTTP 400.
- **Transient `429`/`5xx` are retried before failing.** `complete_once` now
  retries a momentary provider blip on the same endpoint (bounded, short
  backoff) before the error propagates. Previously, `once` mode — which arms no
  higher-level retry loop — surfaced a single transient error as an immediate
  exit 4. A non-transient `4xx` (bad request, auth) still surfaces immediately.

### Added (AAuth [DRAFT], RFC 0023 §7.1 — `--features aauth`)

- **Agent-Provider metadata discovery + issuer validation.** At startup agentd
  fetches `/.well-known/aauth-agent.json` and enforces the AAuth protocol's
  anti-host-poisoning rule (a document whose `issuer` ≠ the configured provider
  aborts enrollment). Best-effort: a provider that publishes no document works.
- **Agent-token claim validation.** The agent token is acted on rather than held
  opaque: agentd refreshes against the token's own `exp`, and fails fast if the
  token's `iss` isn't the configured provider, its `ps` isn't the configured
  Person Server, or its `cnf.jwk` isn't the signing key — each a misconfiguration
  that would otherwise be a silent wall of downstream `401`s.

### Tooling

- **Evaluation harness (RFC 0024, `bench/`).** A dependency-free (stdlib) rig
  that drives the real binary per task and grades it: BFCL tool-calling with a
  faithful AST value-matcher, a τ²-bench-style simulated-user conversation loop,
  an execution-graded coding suite, a sandboxed shell/file environment, and a
  workflow-lift ablation — plus a model×benchmark matrix, run live against
  OpenAI. Not part of the shipped binary.

## v1.3.0 — library split, code-registered tools, AAuth agent identity & lifetime budgets (RFC 0022/0023/0025)

agentd is now consumable as a **library**. The workspace splits into four
publishable crates around one engine; the stock binary is behaviorally
unchanged. Library crates step to `agentd-mcp` **0.3.0** / `agentd-net`
**0.3.0** alongside the `1.3.0` binary.

### Added

- **Crates**: `agentd-core` (the engine — lib name `agentd`), `agentd-cli` (the
  thin binary shell producing the `agentd` command), `agentd-mcp` and
  `agentd-net` (the already-reusable MCP + transport libraries, renamed for
  crates.io; the bare name `agentd` on crates.io belongs to an unrelated
  project). Feature graphs are isomorphic between core and cli; CI gates both
  per matrix row.
- **Code-registered tools** (`agentd::tools`, RFC 0022 §4): an embedder
  registers native Rust tools (`CodeTool::new(name, description, schema,
  handler)`) that join the model's catalogue, are addressable from workflows as
  the reserved server **`code`**, and are callable via the public
  `tools::call`. Dispatch precedence self → code → MCP: the orchestration
  surface is unshadowable and a remote server cannot steal a first-party
  tool's calls (`ToolClass::Code`). The **stock CLI registers nothing** — its
  no-local-code posture holds by construction; the manifest surfaces
  `surfaces.code_tools` only when non-zero.
- The compile-guaranteed embedder reference:
  `crates/agentd/examples/custom-cli.rs` (built by CI, runs offline);
  [docs/embedding.md](docs/embedding.md); RFC 0022 with the three
  API-stability tiers.

### Added (AAuth [DRAFT], RFC 0023 — `--features aauth`)

- Agent-side auth for calling **AAuth-protected MCP servers**: an Ed25519 agent
  identity (`agentd::aauth::AgentKey`), an Agent-Provider enroll + agent-token
  client with cache/refresh (`ApdClient`), and **RFC 9421 HTTP Message
  Signatures** on every outbound MCP request — no shared API key. Wired via a
  dependency-free `RequestSigner` seam in `agentd-mcp` (the crypto stays in
  `agentd-core`; `ring` is a direct edge only under `aauth`, the same crate
  rustls already links). One identity per process tree (rides the spawn
  payload). Config: `--aauth-provider` / `--aauth-key-file` /
  `--aauth-enroll-token` / `--aauth-person-server` (+ `AGENT_AAUTH_*`).
  Manifest: `surfaces.aauth`.
- **Now ships in the release binary and container image** (the `aauth` feature
  joined the default release feature set). Its crypto (`ring`) is already linked
  by the default `tls`/rustls transport, so shipping it adds **no new crate to
  the graph** — unlike `cel`, which stays build-from-source. Still a compile-time
  feature (a trimmed build can drop it) and still `[draft]`.
- The transport runs the full **request reaction loop** (sign → inspect
  `AAuth-Requirement`/`AAuth-Access` → satisfy → re-sign → retry, bounded), so
  **all three access modes run end to end**: **Case A** (identity-based),
  **Case B** (adopt a returned `AAuth-Access` token and replay it), and
  **Case C** (user-scoped — the Person-Server resource-token → user auth-token
  exchange, presented as the `Signature-Key`). Plus **discovery**
  (`/.well-known/aauth-resource.json`), **content-digest** covering when a
  server requires body integrity, and **per-server opt-out** (`aauth: false` on
  a `--mcp` config entry).
- **Federated enrollment** (`--aauth-enroll-assertion-file` /
  `AGENT_AAUTH_ENROLL_ASSERTION_FILE`): the enroll body carries an
  `enrollment_assertion` (e.g. a Kubernetes projected ServiceAccount token),
  **re-read fresh on every enroll** so a rotated token is always current — the
  secret-free fleet path (no shared enrollment secret, no operator key custody).
- **The intelligence dial is signed** (agentctl RFC 0024 §7.1): with an identity
  installed, agentd applies the same RFC 9421 headers to its `--intelligence`
  requests (chat POST + `/v1/models` GET), so a model gateway can attest the
  individual agent by signature instead of source IP. Additive — the bearer, if
  any, still rides alongside.
- Proven by four live-socket e2e tests (`aauth_e2e` Case A; `aauth_flow_e2e`
  Case C over a real `McpClient`; `aauth_enroll_assertion_e2e` federated
  fresh-read; `aauth_intel_sign_e2e` signed model dial). **Draft**: the wire
  tracks the AAuth guide agentd was built against and may shift.
- The `hwk` Signature-Key (enroll + single-key token refresh) presents the raw
  Ed25519 key as **inline `kty`/`crv`/`x` structured-field params** per
  `draft-hardt-httpbis-signature-key` — not a `jwk="<b64url JSON>"` blob, which a
  conformant Agent Provider rejects (`invalid_key`). Verified by driving the real
  binary against a real Agent Provider end to end (the mock-AP unit tests do not
  parse the `hwk` form, so this surfaced only under real integration).

### Added (harness-tracked budgets, RFC 0025)

- **Per-instance lifetime token budget** (`--budget-tokens-lifetime` /
  `AGENT_BUDGET_TOKENS`, config-file `limits.lifetime_tokens`): a cumulative
  token cap across **all** runs/reactions of an instance, distinct from the
  per-run `--max-tokens` box. Bounds a long-lived agent on a path with no
  metering gateway (e.g. an AAuth direct dial). `0`/unset = unbounded (today's
  behaviour). A bounded `once` run folds `min(max_tokens, cap)` and trips
  `EXIT_BUDGET(7)`; a `reactive`/`loop`/`schedule` daemon stops accepting new
  work and **drains cleanly** (exit `0`, or `--budget-exit-code`) with a
  `budget.exhausted` event. Metered where all child tokens converge in the
  supervisor.
- Observability (`metrics_schema` → **1.1**, additive): the gauge
  `agent_budget_tokens_remaining` (the alerting/scaling hook) and the
  `tokens_lifetime` value of `agent_limit_exceeded_total{limit}`; a one-shot
  `limit.threshold` event fires the first time usage crosses 90% of the cap.

### Changed

- `--mcp code=…` is refused (`code` is the reserved code-tools server name).
- Building from source: the binary is now `cargo build -p agentd-cli`
  (release artifacts unchanged).

## v1.2.0 — workflow dialect 2: durable, parallel, human-in-the-loop workflows (RFC 0021)

Workflows now match — and in places exceed — the code-first agent SDKs, while
staying a declarative JSON artifact. Zero new dependencies (the moat holds at 3).
`contract_version` stays `1.0`; feature-detect via `surfaces.workflow.dialect >= 2`.

### Added

- **Human gates over A2A** (`human` node): a workflow suspends and asks a person
  — the served task projects **`TASK_STATE_INPUT_REQUIRED`** with the payload as
  its status message; the reply is a spec-native `SendMessage` carrying the
  `taskId` (the A2A multi-turn shape — no agentd-specific API). Reply /
  `reply_uri` update / timeout race, first wins. Duplicate reply → `-32004`,
  unknown task → `-32001`; degrades to a plain wait without serving.
- **The MCP checkpointer** (`checkpoint` graph policy): per-superstep durable
  run state, with the checkpointer as *any MCP server* implementing
  `state.put`/`state.get`/`state.list` (monotonic-seq guard; a refused put is
  always fatal — the split-brain protection). `--workflow-resume
  <server>:<key>[@seq]` (+ `AGENT_WORKFLOW_RESUME`, `--workflow-resume-force`):
  crash-recovery from the latest envelope (exactly-once for completed nodes,
  at-least-once for the in-flight one), `@seq` under a new run-id = a fork,
  hash-mismatch = refusal (exit `5`). Budgets carry over across resume.
  Envelopes bind the graph by SHA-256 (hand-rolled FIPS 180-4, NIST-vector
  tested).
- **Write reducers** (`writes_mode: overwrite|append|merge|union` on every
  writing node): accumulate instead of clobber; pure, clamp-aware, type
  mismatch → the `error` edge with a readable marker.
- **The `parallel` node**: named heterogeneous branch bodies run concurrently
  on the SAME 8-lane pool `foreach` uses (composition never multiplies
  concurrency); ≤16 branches, step pre-charge, shared token pool, results as
  one object keyed by branch name, `fail_fast|continue`.
- Manifest: `surfaces.workflow.{dialect: 2, checkpoint: true, kinds: [12]}`.

### Changed (fail-closed hardening)

- **Unknown workflow fields are define-time errors** (a typo'd `writes_mode`
  can no longer silently mean overwrite): one strict `parse_graph` front door
  behind `--workflow`, `workflow.define`, and `workflow.patch`. Dialect-1
  graphs are byte-identical on the wire and behaviorally unchanged.
- A2A `SendMessage` now accepts `message.taskId` as a gate-reply continuation
  of an existing task (`-32004` when nothing is waiting).

### Verified

Two new real-process e2e suites: a `--mode workflow` run SIGKILLed mid-node
resumes from its checkpoint on a real HTTP checkpointer and completes with the
pre-crash blackboard; a served A2A task flows WORKING → INPUT_REQUIRED → reply
→ COMPLETED on the wire. 686/686 featured tests (36 new), 384/384 default,
conformance 38/38. RFC 0021 (Implemented) is the normative spec;
[docs/workflows.md](docs/workflows.md) the guide.

## v1.1.0

### Added

- **Bare env spellings for the two required inputs.** `INTELLIGENCE` is now
  accepted alongside `AGENT_INTELLIGENCE` (mirroring the existing bare
  `INSTRUCTION`), so the minimal launch is `INSTRUCTION=… INTELLIGENCE=… agentd`.
  Precedence within the env layer is by specificity — branded `AGENTD_*` >
  neutral `AGENT_*` > bare — so a prefixed spelling always wins over the bare
  one. Additive; no existing spelling changes meaning. `contract_version`
  stays `1.0`.

### Fixed

- **`AGENT_INSTRUCTION` is honoured.** It was silently ignored (de-branded to an
  `AGENTD_INSTRUCTION` nothing read), so following the neutral `AGENT_*`
  convention for the instruction produced a confusing "missing instruction"
  error. It now works like every other `AGENT_*` key.
- **Docs/site consistency sweep** (post-1.0.0-reset): five run modes everywhere
  (the modes page gains the `workflow` mode row); exit `124` correctly
  attributed to the supervisor hard-kill backstop (a self-detected `--deadline`
  is `7`); stale stdio-era claims in `architecture.md` ("MCP servers over
  stdio", "gated `exec`") rewritten to the remote-HTTPS / no-exec reality; one
  stray `agentd://` → `agent://`; wire/log example versions → 1.0.0; the
  landing page's workflow card lists all **ten** node kinds (`join` was
  missing); `use-cases.md` added to the docs index.

## v1.0.0 — first public release

The first public release of **`agentd`**: a small, MCP-native, HTTPS-everywhere
agent runtime built for Kubernetes. It takes an instruction plus tools from remote
MCP servers and runs the agentic loop — as a one-shot, a loop, a schedule, a
reactive daemon, or an agent-authored workflow — supervised, bounded, and
observable.

`agentd` is the **reference implementation of the neutral Agent Control Contract
(ACC 1.0)**. It is named `agentd` (the daemon) but speaks the neutral `agent`
contract, so the agentctl control plane drives it without depending on this binary:
`agent://` resources, the `agent_` Prometheus metric prefix (`metrics_schema` 1.0),
the `AGENT_*` env/config convention, and a `--capabilities` manifest carrying
`contract_version` 1.0.

### Runtime

- **HTTPS everywhere.** Intelligence, the MCP client, the served self-MCP, and the
  A2A / operator control surface are all HTTP(S) over mTLS; plaintext `http://` is a
  loopback-only dev carve-out. There is no unix, vsock, or stdio transport and no
  local execution surface — the only process agentd starts is itself (a
  `current_exe()` re-exec for subagents). TLS (rustls + ring, bundled roots) is a
  default feature.
- **Remote MCP tools.** `--mcp name=<https://host/mcp>` (Streamable HTTP: sessions +
  SSE, multi-version negotiation). Per-server auth is secret-free — header templates
  (`{{secret:NAME}}`), an mTLS client identity, or OAuth 2.1 client-credentials
  (`--features oauth`).
- **Serving requires identity.** `--serve-mcp https://host:port` with
  `--serve-cert`/`--serve-key`; a non-loopback listener MUST authenticate peers
  (`--serve-client-ca` mTLS and/or `--serve-bearer`). Verified identity — never the
  transport — mints the Management origin.
- **Run modes:** once, loop, schedule, reactive daemon, and workflow. Reactive
  subscriptions support content conditions, an in-turn `await_resource` wait, and
  live warm-session tool-catalogue refresh on `tools/list_changed`.

### Workflows

- **Agent-authored workflows** (`--features workflow`, dependency-free): an explicit
  cyclic graph the agent defines and drives — `workflow.define` / `workflow.run`
  (sync or `detach` into a supervised child) / additive `workflow.patch`, or the
  operator-pinned `--mode workflow --workflow <file>`. Ten node kinds (`agent`,
  `tool`, `assign`, `infer`, `branch`, `foreach`, `join`, `subgraph`, `wait`,
  `halt`) with layered, attributed termination (a step budget, a shared token pool,
  a wall-clock deadline, per-node visit caps, and a progress guard).
- **Reactive-daemon workflows** (`--mode reactive --workflow <file>`): waits hold no
  process — the child suspends with its serialized run slice and a fresh child
  resumes on update/timeout, the budget continuing across processes.
- **CEL** (`--features cel`, the one dependency-bearing opt-in): compile-checked,
  fail-closed predicates, computed `assign.expr`, `infer.check` constraints, and
  reactive wake conditions.

### A2A

- Real agent-to-agent interoperability over HTTPS with the **bare PascalCase** method
  binding (`SendMessage`, `GetTask`, `CancelTask`, `ListTasks`,
  `SendStreamingMessage`, `SubscribeToTask`); `SendMessage` returns the
  `{"task": <Task>}` envelope; SSE streaming terminates on the terminal task state
  and stream close. Peer client-auth via bearer header templates and/or a presented
  mTLS identity.
- Operator control is the `a2a.*` method family — `a2a.Drain`, `a2a.LameDuck`,
  `a2a.Pause`, `a2a.Resume`, `a2a.Cancel` — Management-gated JSON-RPC methods
  (refusals as protocol errors).

### Cloud-native contract

- The frozen exit-code table (a clean drain is 0, not 143), the run-outcome report,
  the metrics schema (`metrics_schema` 1.0), the `agent://events` stream,
  liveness/readiness probes, `--budget-exit-code`, horizontal scaling (sharding +
  work-claim leases + standby), and SIGHUP/inotify hot reload of the reloadable
  config subset.

### Security

- The lethal-trifecta (Rule-of-Two) gate as the single `validate()` authority, and
  **structural secret-freedom**: no credential ever reaches the capabilities
  manifest, the config file, or the identity path. The served MCP endpoint is
  hardened (`Origin` validation as a DNS-rebinding defense; a per-`initialize`
  `Mcp-Session-Id`).

### Conformance

- Every contract surface validates against its schema and behaves as specified — see
  `CONFORMANCE.md`.
