# Changelog

All notable changes to **`agentd`** — the minimal, MCP-native, reactive agent
runtime (developed in the `agentd-dev` org). The format is loosely
[Keep a Changelog](https://keepachangelog.com); versions are the released git tags
(`vX.Y.Z`) and the published image `ghcr.io/agentd-dev/agentd:X.Y.Z`.

## Unreleased

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
