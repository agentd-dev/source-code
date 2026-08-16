# progress

Working tracker for the current development track: what we are doing now, what
is done, and what we plan to do next. Living document — edited as work lands.
(Release history stays in `CHANGELOG.md`; normative specs stay in `rfcs/`.)

Baseline: `main` = `develop` = **v1.4.0** (86a9b53), all suites green
(core 661 unit tests, mcp 56, net 37, every CLI e2e suite, conformance).

## Now — agentd 2.0 (goal set 2026-08-16: implement the whole work plan)

Decisions D1–D18 adopted as recommended in the plan (the `/goal` is the
proceed signal). Ordering refinement: v2 is built **beside** v1 as new modules
(`store/`, `state/`, `runtime/`, `engine/`, `registry/`, `context/`, `config::v2`),
selected by a v2 config document (`config_version: "2"` / `agent:` key); the
**cut-over** (delete modes, mode drivers, served peer tools, nested tree;
migrate e2e tests) happens at P5 — so every intermediate tree is green and
releasable.

Phase checklists (mirroring the plan §6; ✅ = done, 🔄 = in progress):

- ✅ **P0 Freeze** — RFCs 0025–0030 written (`rfcs/0025…0030`), `docs/design/02-v2-test-strategy.md`, RFC index updated (2026-08-16)
- ✅ **P1 Config v2** (2026-08-16): `config::v2` — typed `Settings` + hand-written JSON Schema (drift-tested both ways), path env/flag bindings over the v2 schema, alias table (`--instruction`, `--intelligence`, `--model`, `--mcp`, …) + legacy env aliases, removed-flag hints, `--instruction` sugar workflow, v1/v2/mixed detection, RFC 0030 §5 validation (collected for `--validate-config`), restart-only diff, `--config-schema=2`, v2 `--help`; CLI routes v2 documents (`run_v2`). Deferred by design: `--capabilities` v2 (P5), Mode removal (cut-over P5), docs (P7). Note: `a_warm_session_runs_a_turn_per_send` flakes under the full parallel workspace run (passes alone) — pre-existing CPU-starvation sensitivity.
- ✅ **P2 Durable state core** (2026-08-16): `store/` — the 4-op contract (`Store` trait: put/get/list/delete, seq CAS, `PutOutcome::Conflict`), envelope v2 + key layout, template/CEL mapping (`store::mapping`), adapters `mcp` (default checkpointer profile `state.*` + custom mappings + `with_advertised`), `http` (templated REST + `Idempotency-Key` + secret-ref headers), `memory` (history/CAS/fault injection); `state/` — `Kind`, `Manifest` (generation/entities index/starts/budget/lifecycle), `InboxEvent` write-ahead + `inbox_done`, `TimerRecord`, `Durable` façade (per-key seq tracking; conflict on an owned key = fatal split-brain, first-touch adopts a stale seq once; debounced manifest flush; `on_error: halt|degrade`; `restore()` = manifest + indexed entities + `list` reconciliation + lost/unindexed + generation bump; `kill_point()` for `AGENTD_TEST_KILL_AT`), dependency-free ULIDs. Mock MCP (`--internal-mock-mcp-http`) grew `state.list {prefix}`, `state.delete`, `structuredContent`, `mock.fault`/`mock.ops`. Tests: 15 unit + `store_e2e.rs` over the real HTTP wire (round-trip/restore/split-brain, retry-vs-halt-vs-degrade, and a SIGKILL-at-kill-point chaos life proving accept ⇒ durable ⇒ replay-once). Runtime wiring of the store (`store::open` + `Durable` in the loop) is P3.
- ✅ **P3 Agent loop v2** (2026-08-16; all green: fmt/clippy both feature sets, `cargo test --workspace --all-features` = 764 core unit tests + every e2e suite): `jsonschema` validator · mock-LLM `file:` playbooks · protocol (`SpawnPayload.role/turn`, `ToolRequest/ToolResult`, `BudgetRequest/BudgetGrant`, `TurnDone`) + `runtime::worker` turn worker · `registry/` (contracts, precedence, overrides/disabled, profiles, grants) · `context/` (transcript, plan, memory, compaction, skills, tokens) · `governor/` · `engine/` (dialect-3 model + validation over the full catalogue, templates, run records, scheduler) · `runtime/` (reactor, children, timers, artifacts, internal tools, turns with preflight + knowledge auto-context, steps for the P3 kinds, subagents, lifecycle, hot reload, instruction resource) · CLI `Ask::Run` + `--config-version 2` · e2e `runtime_v2_e2e.rs` (7) + `runtime_v2_reload_e2e.rs` (1). Deferred by design: A2A intake/commands/principals + `--capabilities` v2 (P5), docs (P7), the remaining step kinds + start nodes (P4). Design decisions: turn workers reuse the v1 child machinery with a new payload `role`; the v2 turn loop is new (`runtime::worker`), v1 `agentloop` stays until the P5 cut-over; store writes are synchronous in the loop (single writer; async pipeline = P7 candidate); mapped tools + `mcp.tool` steps run on executor threads; runtime-created workflow definitions persist under `memory/_workflows/<name>`; governor windows are unit-aligned fixed windows; the CEL parser runs under `catch_unwind` (`antlr4rust` panics on some malformed inputs); `job_shape` follows `lifecycle.run_until` (`auto` ⇒ no listener + no long-lived start; `idle` ⇒ job; `drained` ⇒ daemon); conversations are reachable before P5 only via the debug seam `AGENTD_TEST_INBOX_FILE`.
- ✅ **P4 Workflow engine v3 — node catalogue + start nodes** (2026-08-16; green: fmt/clippy both feature sets, `cargo test --workspace --all-features` = 766 core unit + all e2e): the full RFC 0027 §5 catalogue now executes — nested bodies `foreach`/`batch` (bounded parallelism, `rate` pacing, per-batch durable progress + resume, positional `collect`, `on_error: continue` slots, `batch.by` grouping, artifact-backed items), `iterate` (`while`/`until`/`max_iterations`), `parallel` (fan-in object, `min_success`), `race` (first-wins + cancel + timeout), `subgraph`; `switch` routing; data steps `map`/`filter`/`reduce`/`sort`/`dedupe`/`chunk`/`parse` (`engine::data`, CEL/`{{…}}` element exprs, CSV/YAML/JSON/lines); orchestration `wait` (resource/condition/signal/run/subagent/message/deadline), `join`, `workflow` child runs (sync/async/detached + `cascade`), `workflow.signal`/`wait`/`cancel`, `subagent`, `human`, `mcp.resource` (read/list/prompt/complete/templates), `a2a.delegate` (the 1.x A2A client, feature-gated); `think` presets `classify`/`extract`/`summarize`/`judge`/`route`; step `cache` (memoized by input hash); large step outputs spill to artifacts and dereference transparently. **Start nodes** `loop` (interval/`until`/`max_iterations`/backoff), `schedule` (cron/`every`/`at`, next-deadline armed), `subscribe` (MCP resource, notify-then-read, debounce/coalesce/filter), `signal`, `event` (workflow.finished/failed) — durable start-node state in the manifest; the reactor tick is now adaptive to the nearest deadline. Run registry: concurrency policies (queue/drop/replace), pause/resume, cancel with cascade, `workflow.*` tools, `--workflow-schema`. Runs restore + replay running steps; per-batch resume proven under SIGKILL. e2e `engine_v3_e2e.rs` (data pipeline, parallel/race/switch/subgraph + on_error, mcp.tool + artifacts + concurrent runs, SIGKILL mid-batch resume) + `engine_orchestration_e2e.rs` (loop `until` + event reaction, schedule interval, child workflow + signal coordination, cache + classify preset). Deferred: cluster `claim`/`shard` on `subscribe` (D8 — hooks present, wiring is a follow-up), the `at`-as-wall-clock and `catch_up: all` refinements.
- ⬜ **P4 leftover: cluster claim/shard on subscribe start nodes** (D8)
- ✅ **P5 A2A v2** — transport binding ✅ + mode cut-over ✅ (the v1 `Config`/`Mode`/`workflow`-feature dead-code cleanup is a deferred follow-up):
  - ✅ **A2A v2 transport binding** (2026-08-16; green: fmt/clippy both feature sets, `cargo test --workspace --all-features` = 773 core unit + all e2e incl. new `runtime_v2_a2a_e2e.rs` ×3): the v2 daemon binds the real HTTPS listener (`crates/mcp` serve framework) when `a2a.listen` is set, and turns A2A requests into runtime work. `runtime/a2a_server.rs` = the whole binding: `A2aAuth` (`HttpAuth`) classifies the connection (plaintext-loopback ⇒ operator; mTLS `peer_cert` ⇒ operator; server `bearer` ⇒ operator; a principal bearer ⇒ resolved) and stashes the presented bearer in a per-connection **thread-local** (the framework hands `dispatch` only `PeerOrigin`, and one connection = one thread = one request); `A2aHandler` (`Handler`) resolves the [`Principal`] via `a2a::Resolver`, enforces the `Principal::may`/`may_command` matrix, and posts each request to the single-writer loop as a new `Event::A2a` (a per-request oneshot reply) — **never blocking the loop**: a blocking `SendMessage` and streaming are served by the transport thread polling a **shared task-snapshot map** the loop republishes on every transition. On the loop, `impl Runtime` creates/advances **durable `a2a::Task`s** (`Kind::Task`, restored at startup, `GetTask` survives a restart): a natural-language message → an `a2a_message` inbox event → a conversation turn, and its task completes at the `deliver_reply`/`finish_root_turn` hooks (via an `event_to_task` map, re-linked on replay); a command DataPart (`{"data":{"agentd":{"op":…}}}`) routes to `status` (sync), `workflow.run` (links its task to the run it starts — `on_start_event` now honors a pre-generated `run_id`; the task tracks the run to terminal at `on_run_terminal`), `workflow.status`/`workflow.cancel`; `GetTask`/`ListTasks`/`CancelTask` read/cancel durable tasks (ownership-scoped, non-owner ⇒ `task not found`); `SendStreamingMessage`/`SubscribeToTask` stream `working`→status/artifact→terminal frames; the operator admin family (`a2a.drain`/`lameduck`/`cancel`) and a public `GetAgentCard` (workflows as A2A skills). Listener startup is fatal-on-failure at boot; `a2a.listen` is parsed via `ServeTarget` (scheme ⇒ TLS), bound addr logged (`bound`) so `:0` works in tests. **Scope notes (honest):** the serve framework exposes only `peer_cert: bool` (no SAN/subject), so `san`/`sub` principal matchers need a bearer today and mTLS conveys *operator* only — surfacing the client-cert subject is a `crates/mcp` follow-up; command DataParts cover the `status`/`workflow.*` subset (others return `UNSUPPORTED_OPERATION` — the NL path reaches every internal tool incl. subagents); `pause`/`resume` admin is a stub; built additively (v1 modes/surfaces untouched) so the whole tree stays green.
  - ✅ **The mode cut-over — DONE & GREEN** (2026-08-16; Stages 1+2; **31,962 lines of v1 deleted this turn**; fmt + clippy both feature sets clean, all feature combos build, 3-dep moat intact, `cargo test --workspace --all-features` = 480 core unit + all v2 e2e + reduced conformance, default lib 382). Verified line-precise plan from Explore `aacc00c29c37f9363`. **Stage 2 DONE & GREEN**: the in-child `Orchestrator` self-handler is replaced by `NoSelfTools` in `subagent/control.rs` — v2 `Role::Agent` subagents are now RFC 0026 **flat-tree leaves** (they run a ReAct loop over granted MCP/code tools + `finish`, with no in-child nesting/`schedule`/`subscribe`/`workflow.*`/`a2a.delegate`; `finish` is runner-intrinsic so completion is unaffected — proven by `subagent_spawn` + the runtime_v2 subagent-delegation e2e staying green). Deleted `subagent/orchestrator.rs` (2171L), `supervisor/{reactor,gate,restart,swap}.rs`, `graph/**` (dialect-1/2 driver, ~7000L), `triggers/router.rs`, `agentd_uri.rs` (inlined `starts_with("agentd://")` at `agentloop/runner.rs:376`), `budget.rs` (v1 lifetime ledger; the v2 governor replaced it); removed the `control.rs` gatebus + workflow-child path and the `SpawnPayload` workflow fields (`protocol.rs`) + their initializers (`runtime/{subagents,turns}.rs`). `run_loop`/`Session::prepare` already took `&mut dyn SelfHandler`, so the swap needed no runner change. KEPT `agentloop/**` (the in-child ReAct loop v2 subagents reuse), `supervisor/{budget,cgroup,kill,liveness,reap,reaper,spawn,tree}`, `triggers/timer` (v2 `schedule`). **Deferred (dead-code cleanup, low value):** `config::Mode` + the `workflow` feature + the unreachable v1 `Config` (~2800L; its shared utils — `McpServerSpec`/`ServeTarget`/`A2aPeerSpec`/`AAuthSettings`/`parse_duration`/`SwapPolicy` — stay) are vestigial (unreachable from the v2-only CLI) and compile clean; `WorkflowResumeRef` kept graph-free for the dead v1 `--workflow-resume` parse; the two `cel`-dependent engine tests were gated on `cel` so the default build is green too. Removing the whole v1 `Config` is a bounded follow-up. **Stage 1 DONE & GREEN** (2026-08-16; fmt + clippy both feature sets clean, all feature combos build, 3-dep moat intact, `cargo test` = 634 core unit + all v2 e2e + reduced conformance): the CLI is **v2-only** — `main.rs` routes every config through `runtime::run`, rejecting a 1.x document/`--mode` with a migration hint (`run_v2 -> i32`, no v1 fallthrough); the entire v1 supervisor surface is gone — deleted `triggers/mode.rs`+`warm.rs` (mode drivers), `mcp/server.rs` (v1 self-MCP, 5450L) + `mcp/a2a.rs` (v1 A2A, 1261L; its `TaskState`+client wire helpers relocated to `mcp/a2a_wire.rs` for `a2a_client`), `capabilities.rs`+`report.rs` (v1 control-plane manifest/reports), `cluster/**` (RFC 0019 sharding). **Feature surgery:** `serve-mcp`/`serve-https`/`events` dropped (their only consumers were the deleted files); `a2a = ["tls"]` (the v2 listener is `mcp::http_server`+`net::tls`, not the deleted v1 server); `cel` decoupled from `workflow`; CI matrix updated. **Tests migrated:** deleted the 9 pure-v1 e2e suites + `chaos`/`otel`/`cgroup` (gaps noted for P6/P7); `cli_once` rewritten to v2. **Conformance reduced to its v2-viable core** (supervisor + security families; `mcp-server`/`mcp-client`/`work-claim`/`agent-loop` retired — a full v2 conformance rebuild is P7). `config::Mode` KEPT for now (dead in the unreachable v1 `Config`; removed with the full v1-`Config` deletion). ~12K v1 lines removed; **zero v2 behavior change** (the orchestrator cluster was untouched).
- ✅ **P6 Observability & audit — DONE & GREEN** (2026-08-16; fmt + clippy both feature sets clean, `cargo test --workspace --all-features` green incl. new audit e2e + OTLP-logs/metrics unit tests): audit stream ✅, metrics/health serving ✅, v2 metric series ✅, `agent://` read surface ✅, OTEL traces ✅, **per-child cgroup ✅** (`security.cgroup` armed at startup; `supervisor::spawn` places each child in its own leaf held on the `Subagent` — `cgroup.kill` atomic teardown on reap), **OTLP logs export ✅** (`observability.otel.logs` → a bounded buffer drained by a background thread to `<endpoint>/v1/logs`, hooked at the Logger's one emit point, no-op when unarmed). The MCP-resource-style `agent://` listener surface (D7) is the only deferred sub-item (the read surface is fully served over A2A).
  - ✅ **OTEL traces from the v2 turn worker** (plan §3.11; 2026-08-16; green under `--features otel`): `runtime::worker::run_turn` now opens an `invoke_agent` `RunSpan` (the existing `obs/otel` OTLP exporter) with a `chat` child span per model call (model, in/out tokens, ok) and an `execute_tool` child span per tool call, exported as one OTLP trace on turn finish — closing the "turn worker emits no `invoke_agent` span" gap. Trace-id/`traceparent` propagation to children + MCP was already in place.
  - ✅ **v2 metric series** (plan §3.11; 2026-08-16; green, 19 metrics unit tests incl. valid-Prometheus render): added + wired `agent_turns_total{kind}` (turn worker spawn), `agent_steps_total{status}` (`finish_step`), `agent_store_ops_total{result}` + `agent_store_latency_ms_sum` (`Durable::put` — ok/conflict/error + timing), `agent_inbox_pending` + `agent_context_tokens` (reactor-loop gauges). Closed-label-domain registry pattern (`obs/metrics.rs`), so the cardinality stays bounded; `metrics`-feature-gated recording, no-op otherwise.
  - ✅ **`agent://` read surface** — `status_value()` already serves a comprehensive read (instance/store/workflows/runs/conversations/subagents/children/timers/inbox/budget/tools/skills/counters/instruction), mirrored by the A2A `status` command; added the `config` command op (`agent://config/effective` — the merged doc with `{{secret:…}}` refs, operator-gated via `may_command`). (An MCP-resource-style `agent://` surface over the listener — D7 — remains a follow-up.)
  - ✅ **The audit stream** (plan §3.11; 2026-08-16; green, e2e `runtime_v2_a2a_e2e::a2a_calls_are_audited…`): `runtime/audit.rs` — an append-only *who-did-what* record `{ts, instance, principal, role, action, target, outcome, request_id, trace}` to the configured `observability.audit.sink`s — `log` (a closed-vocabulary `audit` log line) and/or `store` (a durable, ULID-keyed, **append-only** `Kind::Audit` record — not indexed, never CAS'd/overwritten). Emit points wired: **every A2A call** (`audit_a2a` in `on_a2a_request` — method:op + principal + outcome, so the whole principal×action surface I built in P5 is now audited), **config reload** (applied/invalid/restart_required, in `on_reload_requested`), and **restore** (generation adoption, `lost`-count, at startup). Cheap no-op when no sink is set.
  - ✅ **Observability serving** (the cut-over gap): `observability.metrics_addr` (the Prometheus `/metrics` surface, `metrics` feature) and `observability.health_file` (RFC 0016 §10 liveness heartbeat) are now started from `runtime::run` (their v1 `main.rs` wiring was deleted at the cut-over).
  - ⬜ **Deferred sub-item only:** explicit named spans for `workflow.run`/`workflow.step`/`a2a.request` (turns + tool calls span; runs/steps/a2a still ride the JSON-log events the exporter maps + the durable state), and an MCP-resource-style `agent://` surface over the A2A listener (D7 — the read surface is already fully served via the A2A `status`/`config` commands).
- ✅ **P7 Hardening, docs, conformance v2** — hardening ✅, docs ✅, **conformance v2 ✅**:
  - ✅ **Chaos matrix** (2026-08-16; `runtime_v2_chaos_e2e.rs`, green): a store-backed workflow run is SIGKILLed at each durable-write kill point (`state.before_put`/`state.after_put`/`step.running`/`step.before_done`) and the next life **restores and completes it exactly once** — the RFC 0025 §5–§7 durability contract, proven end-to-end through the real binary with a mock-MCP store surviving both lives.
  - ✅ **`docs/modes-and-triggers.md` rewritten to v2** — the durable runtime, `lifecycle.run_until` job/daemon shape, start-node triggers, A2A channel, + a 1.x→2.0 migration table.
  - ✅ **`README.md` swept to v2** — the "Five modes" section → "Lifecycle & triggers" (job + start-node YAML), the workflow section → the RFC 0027 v3 catalogue, the composition section → the A2A v2 listener/peers, the feature table (dropped `serve-https`/`workflow`/`events`, added `otel`).
  - ✅ **`docs/configuration.md`** — the "Build status" note replaced with the v2 nested-schema overview (`config_version: "2"` sections + `--config-schema=2`/`--capabilities`/`--validate-config`), §6 Modes marked **removed (1.x migration reference)**, and §14 "A complete example" rewritten as a full v2 daemon config (A2A + subscribe workflow + audit/cgroup). The middle flag/env table is now explicitly framed as a 1.x migration reference.
  - ✅ **`docs/getting-started.md`** — the loop/reactive sections rewritten to `loop`/`subscribe` start-node workflows; the supervisor diagram + scope notes updated to A2A/flat-tree/v3-engine reality.
  - ✅ **`docs/deployment.md` swept to v2** — the header/intro, the config-surface table (v2 nested sections + aliases), the daemon example (A2A + subscribe workflow + store), the systemd unit, the feature list, and the k8s Job/CronJob/Deployment manifests are v2; the sharded `cluster` StatefulSet is marked removed with the v2 store-arbitrated-fleet approach.
  - ✅ **Secondary docs swept to v2** (2026-08-16): `mcp` (A2A endpoint §2 + A2A-composition §3 with an A2A **sequence diagram**), `intelligence` (endpoint-health via telemetry, not `agent://`), `architecture` (v2 module map rebuilt + a **two-loop mermaid diagram** + lifecycle/triggers §6), `security` (§9 A2A listener), `getting-started` (mental-model **ReAct-cycle diagram** + v2 features), `embedding`, `subagents` (a trust-boundary **scope-narrowing diagram**), `observability` (event vocab → real `a2a.*`/`run.*`; `agent://` retired-surface banner), `use-cases` (all 6 shapes as v2 configs, schema-validated against the real binary), `docs/README` index, `configuration` (banner removed-vs-aliased split, §2 validate rules to v2, §3.4/§3.5/§13 1.x markers) — plus **3 real §14 flagship-config bugs** caught by validating against the binary (`limits.run.steps`/`tokens`/`subagents.depth` not `max_*`; budget `per:` not `every:`; daemons need a durable `store`). The **RFCs are design-intent specs, deliberately not reconciled** (the v2 system is specified by RFCs 0025–0030).
  - ✅ **Conformance v2 families — DONE & GREEN** (2026-08-16; `cargo test -p agentd-conformance` = **6/6 families, 15/15 checks pass**; `cargo run -p agentd-conformance` renders 15 passed / 0 failed; fmt + `cargo check --workspace` clean; conformance crate bumped to 2.0.0): rebuilt the retired v1 families into four v2 families driving the real binary black-box — **store** (RFC 0025: boots against an MCP store; a restart restores the completed `once` run and skips re-firing it), **durability** (RFC 0025/§4.4: a SIGKILL at `inbox.after_put` then `step.running`, then a clean life restores + finishes — 3-life chaos), **tools** (RFC 0028: internal round-trip `memory.set`/`plan.create`, unknown-tool-errors-not-crashes, `--capabilities` registry), **a2a-conversation** (RFC 0029: a `status` command completes without a model turn, the agent card advertises streaming, an NL message becomes the task artifact readable via `GetTask`/`ListTasks`). Harness gained `run_env` (the `AGENTD_TEST_KILL_AT` durable hooks); the a2a family drives the listener over loopback-http JSON-RPC. `CONFORMANCE.md` suite count updated (15/15). A longer **soak** run is the one optional remainder.
- 🔄 **P8 Release 2.0.0 — prepped, release itself user-gated:**
  - ✅ **Version bumped to 2.0.0** (`agentd-core` + `agentd-cli` + the cli's core dep; `crate::VERSION` now `2.0.0`, surfaced in the A2A agent card / `--capabilities` / logs).
  - ✅ **Release wiring updated to the v2 feature set** — `release.yml` and the `Dockerfile` default `FEATURES` are now `a2a,metrics,cron,otel,hot-reload,config-watch(,aauth)` (dropped the removed `serve-mcp`/`serve-https`/`events`/`cluster`/`workflow`); CI feature matrix already updated in the cut-over.
  - ✅ **CHANGELOG** carries the full 2.0 arc under `## Unreleased` (ready to become the `2.0.0` section at the tag); migration guidance is in `configuration.md` §6 + `modes-and-triggers.md`.
  - ⬜ **The release cut itself is USER-GATED** (irreversible publish): cutting the `v2.0.0` tag (which triggers `release.yml` → the multi-arch binary + image publish to the PUBLIC repo/registry), finalizing the CHANGELOG header, and pushing. Per every standing signal in this project (the "never-push hold", release user-gated) I will **not** do this autonomously.

- [x] **Plan written & reviewed** —
  Design + implementation plan written: [`docs/design/01-durable-agent-plan.md`](docs/design/01-durable-agent-plan.md)
  (requirements R1–R23 + "no modes", target architecture, behavioural specs,
  data contracts, 8-phase work plan P0–P8, risks, decisions D1–D18).
  Decided 2026-08-16: D5 strict durability = yes; scope/phasing accepted;
  D16 triggers = start nodes; D18 one `instruction` field (URI ⇒ read+subscribe).
  Added on request: R20 conversation preflight + `plan.*`; R21 token governor
  (windowed budgets, `wait|slow|degrade|refuse|fail`); R22 start nodes
  (`once|loop|schedule|subscribe|signal|event|a2a|manual`); R23 workflow-as-a-node
  + node/trigger completeness pass (§3.6.3, "deliberately absent" list).
  **Gate:** Andrii's proceed / revise / stop decision — per phase and per
  remaining decision. Nothing of it is built. On "proceed", P0 (RFCs
  0025–0030 + test-strategy doc) starts and each phase's checklist moves here.

## Done

- 2026-08-16 — **Config mechanism: YAML file + path-derived env + flag overrides
  + hot reload** (uncommitted on `develop`; all suites green — core 690 unit
  tests, CLI e2e incl. YAML SIGHUP reload, clippy/fmt clean).
  - `--config <file>` accepts **YAML** (`.yaml`/`.yml`) as well as JSON/jsonc
    (extension decides; unknown extension → sniff). Hand-rolled YAML-subset
    reader `config::yaml` → `serde_json::Value` (no `serde_yaml`; 3 deps kept).
  - Every `--<flag>` overrides the file (`built-in < file < env < flag`);
    every config-file path is a generic flag `--<path>` (`--limits.max-steps 5`
    = `--limits-max-steps 5` = `--limits.max_steps 5`), typed by the schema.
  - Env vars derived from the **field path**: `limits.max_steps` ⇒
    `AGENTD_LIMITS_MAX_STEPS` › `AGENT_LIMITS_MAX_STEPS` › bare
    `LIMITS_MAX_STEPS`. Driven by the config JSON Schema (`config::paths`), so
    a re-defined parameter set needs no per-field plumbing. Legacy named env
    vars/flags keep working.
  - Hot reload (SIGHUP / `--watch-config`) is format-agnostic (`Config::reload`
    = `load` over the original args/env); e2e-proven with a YAML file.
  - `agentd --help` prints the `CONFIG PATHS` table (path · flag · env · type).
  - **Multiple files**: `--config` repeatable + `AGENT_CONFIG=a:b`; merged in
    order, later wins (JSON Merge Patch: objects merge, scalars/lists replace,
    `null` unsets); per-file typing for error attribution; `config.loaded`
    lists `config_files`; every file watched under `--watch-config`; e2e-proven
    (SIGHUP re-merge of a base+overlay pair).
  - **Dotted flags into objects**: `--limits.max-steps` (schema path) and
    `--intelligence_headers.x-team ops` (one entry of a free-form map, exact
    spelling); array elements are refused by path with a clear message.
  - Semantics: setting a path (env / `--<path>`) SETS it (list/map replaced);
    named repeatable flags (`--mcp`, `--subscribe`, `--a2a-peer`) ADD.
  - Module move: `config.rs`/`config_file.rs`/`config_watch.rs` →
    `config/{mod,file,watch}.rs` + new `config/{yaml,paths}.rs`.
  - Docs: `docs/configuration.md` §1.1 + §12; CHANGELOG "Unreleased".
  - The parameter SET is deliberately untouched (next task).
- 2026-08-16 — Context load of the whole codebase + docs pass; baseline verified
  green; `develop` fast-forwarded locally to `main` (v1.4.0). Not pushed.

## Planned / backlog

Candidates surfaced during the code read (not yet approved — pick from here):

- Refactor: one `net::http` connect helper (six copies today: `aauth/{apd,ps,discover}`,
  `mcp/oauth`, `a2a_client`, `intel/client`).
- Refactor: dedupe `distill()` (server.rs / orchestrator.rs) and `write_atomic()`
  (report.rs / obs/health.rs).
- Refactor: one Management-gate predicate for the served resources
  (`AgentdResource::is_management_only()`), replacing ~8 copies in `mcp/server.rs`.
- Refactor: `SuperviseOpts` builder for the `supervise_once → … → supervise_gated`
  chain; `RunParams` struct for `graph/exec.rs` (13 `too_many_arguments` allows).
- Refactor: split the largest files by concern (`mcp/server.rs` 5.4K,
  `config.rs` 5.3K, `graph/driver.rs` 3.5K, `triggers/mode.rs` 2.8K).
- Tests: `SpawnPayload::minimal()` / `Logger::test()` fixtures (22 hand-built
  payload literals across 8 files).
- Docs drift to fix: module map in `docs/architecture.md`; "four modes"/"ten node
  kinds"; `unix:` intelligence + `exec` tool leftovers; version strings; two broken
  RFC links in `docs/operations.md`; `docs/configuration.md` says
  `--budget-exit-code` does not exist (it ships); code cites "RFC 0025" (lifetime
  budgets) but no `rfcs/0025` exists; CONTRIBUTING's `cargo test -p agentd
  --features serve-mcp` names the old package/feature.

## P7 docs — 2.0 reconciliation sweep (2026-08-16)

Reconciled every **prose** doc to the shipped 2.0 surface (code untouched — only
`docs/*.md` this session). The distinction that drove it: the *served self-MCP
surface* (`subagent.*`/`status` tools + `agent://` resources) was **removed**, but
several 1.x flags were **repurposed as aliases**, not deleted —
`--serve-mcp`→`a2a.listen`, `--serve-bearer`→`a2a.bearer`,
`--serve-cert`/`-key`/`-client-ca`→`a2a.tls.*`, `--shard`→`cluster.shard`. Genuinely
removed (rejected w/ migration hint): `--mode`, `--subscribe`, `--continue`,
`--interval`, `--cron`, `--claim*`, `--standby`, `--assign-from`,
`--workflow-resume*`.

- **Rewritten to v2:** `docs/README.md` (status + mcp entry), `architecture.md`
  (status banner, process-tree diagram, module map rebuilt to the real
  crate-split + v2 runtime tree, thread-exception→a2a, run-flow, §6 lifecycle &
  triggers, RFC list), `mcp.md` (§2 → "A2A endpoint", §3 composition → A2A
  Ask/Stream + SendMessage/GetTask + See-also RFCs), `intelligence.md`
  (`agent://intelligence`→failover-snapshot-via-telemetry, `unix:`→loopback http),
  `security.md` (§9 → "A2A listener", SSRF/flag rows), `getting-started.md` (status
  + feature build examples), `embedding.md` (main.rs ~140 lines / no "five modes"),
  `use-cases.md` (all 6 scenarios: jobs drop `--mode once`; reactive/loop/trust-
  partition → `subscribe`/`schedule` workflow configs; served worker → A2A daemon),
  `observability.md` (served-`agent://`-resources → "A2A read surface" banner; event
  table rows fixed to real vocab `a2a.*`/`run.*`/`workflow.*`/`drain.*`/`cgroup.armed`;
  operability table `mcp.*`→`a2a.*`), `configuration.md` (banner removed-vs-aliased
  split, §3.3 `--serve-mcp`/`--workflow`/`--workflow-resume` rows, §3.4/§3.5/§13
  "1.x reference" markers), `subagents.md` + `deployment.md` (drop `--mode`).
- **Left as banner-framed 1.x reference** (intentional): `scaling.md`,
  `operations.md`, `workflows.md`, and `configuration.md` §§3.4–3.6/§6/§13 — each
  carries a "Removed/Superseded in 2.0" banner. `docs/design/*` notes left as
  historical intent (per policy).
- **Config examples validated against the real binary** (`target/debug/agentd
  --validate-config`) — caught + fixed **real bugs**: §14 flagship used
  `limits.run.max_steps`/`max_tokens`/`subagents.max_depth` (schema keys are
  `steps`/`tokens`/`depth`; `additionalProperties:false` → would reject) and a
  budget window `every: 1h` (schema requires `per: <enum>`); every daemon example
  was missing the **required durable store** (RFC 0025: an A2A listener /
  long-lived start node ⇒ `store.kind` must be set — and there is **no `--store`
  flag**, so a daemon must be a config file) and `a2a.listen: https://` requires
  `a2a.tls.cert`/`key`. All complete examples (§14, use-cases uc2/3/5/6, mcp §3)
  now report `config.valid`.

## Decisions

- 2026-08-16 — YAML support is a **hand-rolled subset parser** (mappings,
  sequences, flow collections, quoted/plain/block scalars, YAML 1.2 core-schema
  typing, comments). Anchors/aliases/tags/multi-doc are rejected with a clear
  error. Rationale: the default build must stay at exactly 3 direct deps
  (CI-enforced), and `serde_yaml` is unmaintained.
- 2026-08-16 — Env naming for config paths: `AGENTD_<PATH>` > `AGENT_<PATH>` >
  bare `<PATH>` (`.` → `_`, upper-case). Bare names are accepted because the
  quickstart already accepts bare `INSTRUCTION`/`INTELLIGENCE`; note the
  collision hazard with common container env names (e.g. `LOG_LEVEL`).

## Open questions

- Should bare (unprefixed) env names stay accepted once the parameter set is
  redefined, or be limited to a short allow-list (`INSTRUCTION`, `INTELLIGENCE`)?
- Parameter redesign: with everything a schema path, the named flags/env vars
  become aliases and the two overlay mechanisms (document merge vs. typed
  `Config` overlay) collapse into one; array elements could then be addressed
  by a stable key (e.g. servers keyed by name) rather than by index.
- Push `develop` (locally at v1.4.0, ahead 10 of origin) and delete the merged
  `fix/aauth-hwk-inline-params` branch?

## Backlog — user-requested 2026-08-16 (post-2.0 features)

- **Workflow documentation + `--workflow-schema` DONE (uncommitted, 2026-08-16):**
  `docs/workflows.md` rewritten from the retired v1 dialect into a comprehensive
  **RFC 0027 dialect-3 guide** — graph model, the three string mini-languages
  (`{{…}}` / `CEL:` / `${VAR}`) + `{{secret:…}}`, all 9 start nodes + the 58-step-kind
  registry (grouped tables), deep-dives (`agent`/`http`/`webhook`/`wait`/`foreach`/
  `parallel`/`workflow`), the goal watchdog, durability/resume, 3 worked examples,
  nuances, and 2 mermaid diagrams. **Every full example validated against the real
  binary** (`--validate-config`) — caught + fixed the `mcp`-store shape and the
  public-bind-needs-TLS rule in drafts. New **`--workflow-schema`** CLI flag wired
  (was advertised in `engine/model.rs` but never exposed) → dumps the dialect-3 JSON
  Schema + node registry; unit-tested + in `--help`. Site manifest: dropped the
  `workflows` `1.x` tag.
- **Visual workflow editor DONE & building (uncommitted, 2026-08-16):** a schema-driven
  **React Flow** editor page in the Next.js site (`web/app/editor/`), user-chosen
  approach. Nodes = steps, edges = `depends_on`; palette of all **67 kinds** grouped by
  the 7 categories (data generated from `--workflow-schema` → `web/lib/workflow-nodes.json`
  + `lib/nodeRegistry.js`); property panel edits id/kind/fields (schema-seeded + custom);
  **multi-workflow tabs**; **import** (file/paste) + **export** (download) + live YAML
  preview via `lib/workflowIo.js` (YAML↔graph, layered auto-layout, whole-doc round-trip
  preserving non-workflow sections). Deps added: `@xyflow/react`, `js-yaml`. **`npm run
  build` green** (53 static pages; `/editor` lazy-loads React Flow so other routes stay
  light); headless parse→serialize→re-parse round-trip exact; **the editor's exported
  YAML validates `config.valid` against the agentd binary** (incl. nested `auth.hmac`,
  http, multi-step `depends_on`). Next: user wanted this after the workflow docs — both
  now done.
- **Post-2.0 features DONE & green (uncommitted):** inbound **webhooks** (RFC 0027 —
  `webhook` start node + `webhooks.listen` + HMAC/idempotency/backpressure;
  `runtime/webhooks.rs`, raw-HTTP path in `mcp::http_server`, hand-rolled
  HMAC-SHA256 in `sha.rs`; e2e `webhook_e2e.rs`) and the **goal watchdog** (RFC 0026
  — `goal:` block, `runtime/goal.rs`: durable timer → CEL condition + async **LLM
  judge** via `Event::Background` → progress/stuck → dispositions; e2e
  `goal_e2e.rs`). **Webhook `wait:{on:webhook}` await-node + `respond:sync` DONE**
  (dynamic callback registry shared loop↔listener, reuses `deliver_signal`;
  `emit_url_to` into run vars; sync reply held until terminal — `webhook_e2e.rs` ×3).
  Design choices were user-made (CEL-then-LLM, replan-on-stuck, dedicated webhook
  listener, per-node HMAC+Idempotency-Key).
- **"Final capability set" DONE & green (uncommitted, 2026-08-16):**
  1. **Outbound `http` REST node** (RFC 0027 — `runtime/http_node.rs`): GET/POST/PUT/
     PATCH/DELETE with `headers`/`query`/`json`|`body`/`timeout`/`expect`/`allow_private`,
     over the one SSRF-guarded client (`net::http`+`net::tls`, `guard_host`); returns
     `{status, ok, headers, body, json}`; `{{secret:…}}` refs resolved in header values
     via the redacting resolver (so `Authorization: Bearer {{secret:API_TOKEN}}` works
     without leaking to logs). Added `http` to `engine/model.rs` KINDS + `steps.rs`
     dispatch.
  2. **Webhook *emit*** — the same `http` node with `sign: {secret, header?, prefix?}`
     HMAC-signs the exact body → `X-Signature: sha256=<hex>`, symmetric with the inbound
     `hmac` verify (round-trip proven: `http_e2e.rs` recomputes + matches).
  3. **Env-var substitution** `${VAR}` / `${VAR:-default}` over **every string value** of
     the merged config **and inline workflows** (`config/v2/mod.rs::substitute_env` +
     `expand_env_str`, applied post-merge pre-typing): braces required (bare `$VAR`
     passes through), `$${` → literal `${`, unset+no-default = **fail-closed** (exit 2).
     Distinct from `{{secret:…}}` (redacted credential) — `${VAR}` is for loggable
     values (hosts/ports/paths). **Nuance:** substitution runs on the *parsed* document,
     so a `${VAR:-x}` value containing `:` must be quoted in YAML, and `${VAR}` can't feed
     a non-string/typed field (eager file-typing rejects it first) — string fields only.
     Also taught `engine/template.rs` to pass `{{secret:…}}` through `render_spec`
     verbatim (never expand a credential into rendered step data). Tests: 2 config unit
     tests + `http_e2e.rs` ×2; whole-set validated against the real binary
     (`--validate-config`): `config.valid` with vars set, clear exit-2 when unset.
- **[BACKLOG] A WASM extension node + sandbox policy.** Extend agentd with **custom
  business logic via WASM** — a workflow node that runs a user-supplied WASM module
  (deterministic, sandboxed compute for transforms/policy/validation the built-in nodes
  can't express), plus a **`wasm` sandbox policy** in config (resource limits: memory/
  fuel/time; capability grants: which host imports — if any — are exposed; deny-by-
  default host access, consistent with the "no ambient authority" posture). Design
  later; fits the internal>code>MCP + node-registry model. NOT for now — parked at user's
  request (2026-08-16).
- **[DONE 2026-08-16] The `exec` command-execution tool + security controls.** Internal
  `exec` contract (`{cmd, args?, cwd?, stdin?, timeout?}` → `{stdout, stderr, exit_code,
  timed_out}`) that is **default-OFF at TWO layers**: the local runner compiles only under
  the new **`exec` cargo feature**, AND needs **`security.exec.enabled`** at runtime —
  otherwise it's **mapping-only** (delegate off-box via `tools.overrides`; the default
  binary can't run local commands). Guards (all in `runtime/exec.rs`, re-checked in
  `tools.rs::exec_tool`): **argv-only, NO shell** (no injection); **allow-list** of
  `argv[0]` (empty = deny-all); **workdir confinement** (canonicalized, no `..`/symlink
  escape); **timeout** (clamped + kill); **output cap**; **minimal env** (never the agent's
  secrets); **`sensitive`+`egress` trifecta tags** (Rule-of-Two); grant = root+workflows+
  subagents (NOT A2A user/agent); runs on an executor thread (never the reactor). Config
  `security.exec` (+ schema + drift + validation-portable without the feature). Tests:
  `runtime::exec` units ×5 (echo/stdin-cap/timeout-kill/cwd-confine/env-minimal), registry
  `exec_is_default_off_mapping_only_and_tagged` (both builds), e2e `exec_e2e.rs` (workflow
  runs allow-listed `echo`, refuses `cat`). Docs: security.md §11. All gates green (37
  suites; default + exec + all-features). Uncommitted.
- **[DONE 2026-08-16] Detailed workflow documentation.** `docs/workflows.md` rewritten as
  the RFC 0027 dialect-3 guide (registry of all 67 kinds, start nodes, the three string
  languages, durability, worked examples, mermaid) + `--workflow-schema` flag. See the
  entries above.
- **Endpoint authentication (RFC 0031) — CORE DONE & green (2026-08-16; uncommitted).**
  Goal (effort=max): implement the whole RFC, then the command-exec tool. **All 3
  endpoints (MCP / intelligence / A2A) × 4 provider kinds now wired:** `static`,
  `oauth2` (device-login + client-credentials + refresh + discovery), **`aws` SigV4**
  (`source: env|static`; validated against the AWS `get-vanilla` test vector; dep-free
  HMAC), **`spiffe`** (JWT-SVID rotating bearer + X.509-SVID mTLS via `from_spec` →
  `with_identity`). `intelligence.headers` bug **fixed** (wire-proven `intel_headers_e2e`;
  threaded via `IntelConfig.headers` + `IntelClient::with_headers` + `Endpoint.extra_headers`);
  the intel OAuth bearer resolves fresh per subagent spawn (`current_intel_bearer`).
  `auth::aws`/`auth::device` (SigV4/spiffe signers), `Auth`/`AuthSpec` grew aws+spiffe
  fields + `AuthKind::{Aws,Spiffe}` + per-kind validation + schema. **`docs/authentication.md`**
  written + on the site; RFC §15 marks implemented-vs-follow-up. Gates green (fmt/clippy/
  `--all-features` 33 suites/0 fail, 3-dep moat; behind `oauth`, mTLS behind `tls`).
  **AWS: all sources DONE** — `env`/`static`, `sso` (`auth/aws_sso.rs` — IAM Identity
  Center device flow → temp creds in `CachedCred.extra`), and **`imds`/`irsa`** (SigV4Signer
  is now a `CredProvider` enum — imds=IMDSv2 direct fetch, irsa=STS AssumeRoleWithWebIdentity
  XML; both cached + auto-refreshed); e2e `aws_sso_e2e.rs` + `aws_workload_e2e.rs`.
  **Browser+PKCE DONE** (`auth/browser.rs`, `grant: authorization_code` — prints the auth
  URL, one-shot loopback callback, state/CSRF, code exchange; dep-free b64url/PKCE via
  `/dev/urandom`; e2e `browser_login_e2e.rs`). **Azure OpenAI + Google Vertex** work via the
  generic `oauth2` device provider (documented, `docs/authentication.md`). **RFC 0031 is
  COMPLETE** (36 suites green); deliberately-deferred niche/beyond-scope items in RFC §15 P5
  (durable-AAuth-token-cache, MCP RFC 9728, A2A SigV4, native-Bedrock intel dialect).
  Original P0+P1 detail (also still true):
- **Endpoint authentication (RFC 0031) — P0 + P1 (MCP OAuth device-login) DONE & green
  (2026-08-16; uncommitted).** Goal (effort=max): implement the whole RFC, then the
  command-exec tool. **Shipped this pass:** `Kind::Cred` durable class; a new
  `crates/agentd/src/auth/` module (`oauth2` RFC 8628 **device grant** + refresh +
  RFC 8414/OIDC discovery, `cache` file+durable cred store, `login` device orchestration,
  `device` refreshing `TokenSource`/`BearerSigner`/`StaticSigner` + `signer_for`); the
  previously-**inert OAuth client-credentials is now wired** (fixed `McpServer::to_spec()`
  dropping `oauth`; `OAuthBearerSigner`); a unified **`auth:` config block** on `McpServer`
  (`kind: static|oauth2`, `grant: device|authorization_code|client_credentials`) with
  schema + `validate_auth_block` (per-kind required fields + inline-secret rejection);
  **`agentd --login/--logout <target>`** CLI (device flow prints a code+URL box, caches a
  0600 token file the daemon reads); `from_spec` precedence `auth: → oauth: → aauth`. Tests:
  `auth::*` units + **`login_e2e`** (real `--login` device flow → cache → daemon injects the
  bearer, ~1s) + `mcp_oauth` signer-wiring. All gates green (fmt/clippy/`--all-features`
  32 suites/0 fail, 3-dep moat; device-login behind the `oauth` feature). **Remaining:**
  intelligence + A2A `auth:` wiring (+ the `intelligence.headers`-dropped bug), AWS
  Bedrock (SigV4 + SSO), SPIFFE, browser+PKCE, durable-cred headless refresh, docs. Details
  in RFC 0031 + the `endpoint-auth-rfc-0031` memory. Original planning note follows:
- **[SUPERSEDED-by-above — original plan] Endpoint authentication flows (interactive + workload).** Intelligence (LLM) / MCP / A2A endpoints may need auth beyond a static
  secret: **interactive OAuth / enterprise login** when a human runs agentd (e.g. AWS
  Bedrock via AWS SSO/IAM Identity Center device flow → SigV4; Azure AD; Vertex), plus
  **workload identity** for headless/cloud (**SPIFFE/SPIRE SVID**, cloud IMDS/IRSA, AAuth
  client creds). Extend **AAuth** (RFC 0023). Wants **good UX** (device-code default,
  works over SSH; browser+PKCE loopback opt-in; a pre-flight `agentd login <target>` for
  daemons; fail-closed when a daemon needs a human). User: "Once workflow docs and tools
  done — work on this! Plan the work." → authoring a design RFC now; implementation gated
  on docs + the command-exec tool. Building blocks that exist: `{{secret:…}}` resolver,
  A2A HttpAuth/Principal, AAuth client-signer, the `oauth` feature + `mcp_oauth.rs`,
  durable store for a token cache.
- **[BACKLOG] A dedicated interactive CLI (Ink/TUI) for agentd — "Claude Code / Codex"-
  class UX.** A separate CLI (using **Ink** — https://github.com/vadimdemedes/ink) for
  **controlling, configuring, observing, inspecting, and communicating** with an agentd
  instance across its full capability surface (over the A2A control plane + `agent://`
  read surface): great UX for **prompting and working side-by-side** with a live agentd
  (conversations/turns, workflow runs, approvals/human-gates), plus a **debug mode** for
  inspecting/previewing internal state (runs, steps, blackboard, durable state, tasks) and
  **metrics**. Distinct from the browser workflow editor. Design later; align with the A2A
  command families + observability read surface (RFC 0029 / observability docs). Parked at
  user request (2026-08-16).
