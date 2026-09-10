# agentd — Agent Control Contract (ACC) v1 conformance

`agentd` is the **reference implementation** of an agent driven by the **agentctl**
Kubernetes control plane. agentctl depends only on the published **Agent Control
Contract (ACC)** — never on the binary's code (principle P0). This document records
`agentd`'s conformance to **ACC v1** (`contract_version` `1.0`), surface by surface:
what passes, the gaps closed to get there, and the P-series items deliberately
deferred.

> **Naming model.** The product/binary/image is **`agentd`**, and it now
> **emits the neutral ACC tokens only** — `agent://` resources, `agent_` metrics,
> the unbranded manifest `version`, `agent/*` `_meta`, and `AGENT_*` env
> (documented). The legacy branded spellings are **still accepted on input**
> (graceful) but never emitted. The image is `ghcr.io/agentd-dev/agentd`. The
> golden fixtures were re-captured from the `agentd 1.3.1` binary
> (`version: "1.3.1"`); the agentctl fixture tests moved in lockstep. Where this
> document still cites a branded form below, read it as the legacy input alias.

- Contract (consumed, **never edited by `agentd`**):
  `/root/agentctl-dev/source-code/contract/` — `README.md`, `SPEC.md`,
  `schemas/*.json`, `fixtures/capabilities/*`.
- Conformance is judged by **behavior** against those artifacts (ACC SPEC §8), not
  by sharing code. The neutral spellings are emitted; the branded spellings remain
  accepted on **input** to GA (L4).

## Verdict — conformant to ACC v1

| Evidence | Result |
|---|---|
| `cargo test -p agentd-core --all-features` (full feature matrix) | **615 passed / 0 failed** (614 unit + 1 doc-test) |
| `cargo test -p agentd-core` (default features) | **428 passed / 0 failed** (427 unit + 1 doc-test) |
| Black-box conformance suite (`cargo run -p agentd-conformance`) | **21 passed / 0 failed** (the 2.0 families: supervisor, security, store, durability, tools, a2a-conversation, interface) |
| ACC schema+behavior harness (drives the real binary; see *Validation report*) | **22 passed / 0 failed** |
| Golden `--capabilities` fixtures vs `manifest.schema.json` (both, agentctl-owned) | **2 / 2 valid** |
| agentctl `agent-contract-client` fixture tests | **5 / 5 pass** (cross-repo consistency) |
| `cargo clippy --all-targets -D warnings` (default + full) | clean |

Hard invariants preserved: the manifest is built `json!`→`Value` (the `Secret`
type has no `Serialize`, so it cannot reach it — L5); `intelligence` is structural
only; no `deny_unknown_fields` on any discovery surface; config-input is the one
closed surface; the lethal-trifecta gate is the single `validate()` authority;
no credential reaches the manifest / config file / identity path.

## Per-surface results

| Surface | ACC schema | Status | Notes |
|---|---|---|---|
| **Capabilities manifest** | `manifest.schema.json` | ✅ PASS | `json!`→`Value` (secret-safe); all required root keys; emits neutral `version` + `runtime` + `surfaces{exit_codes, config_schema}`. Sum types correct. Live `agent://capabilities` is parsed-equal to the one-shot. |
| **Management profile** | `a2a.profile.json` (`admin_verbs`) | ✅ PASS | Frozen order `[drain, lame-duck, pause, resume, cancel]`; Management-gated; a non-Management caller of an operator **tool or resource** (read **and** subscribe) → `-32601`. `attach` not a tool; no `force`. `drain ≡ SIGTERM ≡ exit 0`; `lame-duck` readiness-only; `cancel{handle:"0"\|omitted}` = whole run. |
| **Exit codes** | `exit-codes.table.json` | ✅ PASS | Table + `pod_failure_intent` exact; unknown → `retriable`; `EXIT_CODES="1.0"`. Clean drain = **0, not 143**. Only **3 & 7** remappable, via the new `--budget-exit-code`. |
| **Run-outcome report** | *(retired anchor — behavioral)* | ✅ PASS | 12 required keys; `report_schema="1.0"`; `mode ∈ {once,loop,schedule}` (never `reactive`); 9-value closed `status`; `distillate_ref` `^(agent\|agentd)://`; `instance`/`trace_id` omitted-not-null; tokens never currency. A real once-run report validates. |
| **Metrics** | `metrics.registry.json` | ✅ PASS | `metrics_schema="1.2"`; all **37** `live` non-cgroup names present on a live scrape (`agent_memory_*` are cgroup-v2-conditional); **no histogram samples**; `agent_saturation` the only float; bounded labels only; `agent_pending_events` canonical (no `agent_reactive_backlog`). |
| **Events** | *(retired anchor — behavioral)* | ✅ PASS | `agent://events` read body = `{events_schema:"1.0", oldest_seq, newest_seq, dropped, events[]}`; each line carries monotonic `seq` + the required tuple; `level`/`comp` closed, `event` open; lossy ring; `?after/level/event` parsing. Accepts the neutral `agent://` scheme. Validates live. |
| **Config file** | `config.schema.json` | ✅ PASS | The one **closed** surface — typo → exit 2. `--validate-config` 0/2 (good→0, typo→2, inline secret-shaped header→2). `--config-schema` emits a valid draft-2020-12 closed doc, `x-agentd-contract-version="1.0"`. Restart-only keys rejected on a live reload. |
| **Workflow dialect** | `workflow.schema.json` | ✅ PASS | `agentd --workflow-schema` emits the vendored dialect `$id` `https://agentd.dev/schema/workflow-3.json`; **73** kinds, **11** of them start nodes (`once`, `manual`, `loop`, `schedule`, `subscribe`, `stream`, `correlate`, `signal`, `event`, `a2a`, `webhook` — `KINDS`, `engine/model.rs:108`). The 1.3.1 capture pins 72 kinds; the dialect is additive within its major, so a consumer written against that capture still parses. |
| **Reload/restart partition** | `restart-only.json` | ✅ PASS (superset) | `RESTART_ONLY_PATHS` (`config/v2/mod.rs:7212`) holds **28** paths — all **21** the capture pins, plus seven it predates (`interface.enabled`, `interface.pairing`, `store.max_value_bytes`, `agent.document_capabilities`, `agent.instruction.trust`, `webhooks.listen`, `webhooks.tls`). A diff touching one applies **nothing** and reports restart-required; `every_config_path_is_classified` proves the restart-only and reloadable partitions cover the whole surface. |
| **Store profile** | `store.profile.json` | ✅ PASS | `store.kind: mcp` maps its four operations onto the profile verbatim — `state.put`/`state.get`/`state.list`/`state.delete`, reading `result.structuredContent.{ok, latest, state, keys}` (`store/mcp.rs::default_ops`). Every call carries `_meta["agent/idempotency_key"] = "<key>#<seq>"` and `agent/instance`; all four remappable via `store.mcp.{put,get,list,delete}`. Driven black-box by the conformance `store` family. |
| **A2A** | `a2a.profile.json` | ✅ PASS | 11 spec methods + `SubscribeToEvents` served (plus the `GetAgentCard`/`Pair` bootstrap calls); no spec method is gateway-owned; closed error set `-32001/-32601/-32602/-32603`; Management-gated; COMPLETED task = exactly one `<taskId>.distillate` artifact; framed streaming until `final`; `TASK_STATE_*` mapping; `surfaces.a2a` emitted. Live drive confirms the error codes. |
| **Env convention** | `env-convention.json` | ✅ PASS | Downward-API identity; **neutral `AGENT_*` is the documented form**, the legacy `AGENTD_*` still accepted on input (env, identity, per-endpoint tokens). Empty → unset; `run_id` always present; `AGENT_SHARD "K/N"` rejects `N==0`/`K>=N` → exit 2; credentials only on the `*_TOKEN[_FILE]` path. |

`management-profile.json`, `report.schema.json`, `events.schema.json` and
`a2a.methods.json` were **retired with ACC 1.x** (`contract/README.md`): the
management surface folded into the A2A `admin_verbs`, `a2a.methods.json` became
`a2a.profile.json`, and the report/event shapes are no longer contract anchors —
those two rows are held behaviorally. Every one of the nine artifacts the
contract vendors today (ACC SPEC §1) has a row above.

## Validation report (live harness — drives the real binary)

Each artifact below is produced by the running binary and validated against its
contract schema with a draft-2020-12 validator (or asserted behaviorally).

```
[PASS] config:--config-schema            — draft2020-12 closed schema, x-contract=1.0
[PASS] config:good-file vs schema        — valid vs config.schema.json
[PASS] config:--validate-config (good)→0 — exit 0
[PASS] config:typo'd key→exit 2          — exit 2
[PASS] config:inline secret header→exit 2— exit 2
[PASS] report:once-run shape             — behavioral (anchor retired with ACC 1.x)
[PASS] report:mode≠reactive / distillate_ref scheme / exit_code
[PASS] manifest:live agent://capabilities vs schema   — valid vs manifest.schema.json
[PASS] manifest:neutral agent:// scheme accepted
[PASS] manifest:live≈--capabilities (semantic, modulo run_id + live intel overlay)
[PASS] events:agent://events read-body                — behavioral (anchor retired)
[PASS] a2a:GetTask unknown id→-32001 / gateway method→-32601 / empty text→-32602
[PASS] metrics:37 live names present / no histogram samples / pending_events canonical
[PASS] exit:bad flag→2 / intel down→4 / --budget-exit-code leaves non-budget (4) untouched
  22/22 checks passed
```

The black-box `agentd-conformance` suite independently drives the exit-code table
under induced failures: `supervisor/exit-0-on-success`,
`supervisor/exit-2-on-bad-flag`, `supervisor/exit-2-on-retired-v1-flag`,
`supervisor/exit-4-on-intel-down` — all pass.

### Reproduce

The `acc_validate.py` that captured the transcript above is not vendored in this
repo, so the probes it drives are spelled out against the real binary instead:

```sh
cargo run -p agentd-conformance                 # 21 behavioral checks
cargo build --release -p agentd-cli             # or: -p agentd-cli --features "a2a,metrics,internal-mocks"
C=/root/agentctl-dev/source-code/contract
A=$PWD/target/release/agentd                   # absolute: one probe runs from the fixture dir

$A --config-schema   | python3 -c 'import json,sys; d=json.load(sys.stdin); print("x-contract", d["x-agentd-contract-version"], "closed", not d["additionalProperties"])'
$A --workflow-schema | python3 -c 'import json,sys; d=json.load(sys.stdin); print("dialect", d["$id"], len(d["$defs"]["kinds"]), "kinds")'
(cd $C/fixtures/config && HOOK_BEARER=x $A -c full-featured.yml --validate-config); echo "good exit=$?"  # 0
printf 'config_version: "1"\nagent:\n  nmae: x\n' > /tmp/typo.yaml
$A -c /tmp/typo.yaml --validate-config; echo "typo exit=$?"                                             # 2
$A --capabilities > /tmp/live.json

# the live manifest and both golden fixtures vs manifest.schema.json:
for f in /tmp/live.json $C/fixtures/capabilities/*.json; do
  python3 - "$f" <<'PY'
import json,sys; from jsonschema import Draft202012Validator
s=json.load(open("/root/agentctl-dev/source-code/contract/schemas/manifest.schema.json"))
print("VALID" if not list(Draft202012Validator(s).iter_errors(json.load(open(sys.argv[1])))) else "INVALID", sys.argv[1])
PY
done
```

## Golden fixtures (agentctl-owned; conformance proven by the live binary)

The golden corpus in `contract/fixtures/capabilities/` is **owned by the agentctl
contract repo** and pinned by its `agent-contract-client` fixture tests
(`tests/fixtures.rs`), which assert exact content — every capture parses,
identifies as runtime 1 and carries `version == "1.3.1"`, plus the bare
daemonless shape of `agentd-1.3.1-default.json` and the listener, workflow and
admin-verb values of `agentd-1.3.1-configured.json`. They remain the **real
`--capabilities` captures from agentd 1.3.1** and both validate against the
*current* `manifest.schema.json`.

The current binary's `--capabilities` is additive over the 1.3.1 capture in three
blocks — `surfaces{exit_codes, config_schema}`, `webhooks` and `document` — with
one non-additive move, `a2a.admin`'s verbs folding into `command_ops`. It is
validated **live** against `manifest.schema.json`, and that live validation, plus
the behavioral suites, is the authoritative conformance proof, not a static
fixture. Both sum-type branches of every capturable key are covered: the off/null
branches by `agentd-1.3.1-default.json`, the string/object branches by
`agentd-1.3.1-configured.json`.

> **Re-capture is a coordinated agentctl-side change, intentionally not forced
> here.** `contract/README.md` carries the regeneration recipe, and running it
> against 1.14.1 yields two captures that still validate against
> `manifest.schema.json` — but they move four things `fixtures.rs` pins by hand.
> `version` becomes `"1.14.1"`, so the `Some("1.3.1")` assertion fails for both
> captures. The `hook` workflow's `start_kinds` becomes `["webhook"]` and
> `consumer`'s becomes `["stream"]`, because the manifest now derives start kinds
> from the parser's own kind table rather than a hand-maintained list — that is
> exactly the U3 gap the test anticipates in its own failure message ("if this
> starts failing, upstream fixed it; update the contract"). The `a2a.admin` array
> is gone: the same five verbs — drain, lameduck, cancel, pause and resume — now
> ride in `command_ops` under the neutral `admin.*` spellings, so `admin_verbs()`
> reads empty. And the captures are named for the version they came from, so
> `load()`'s paths move with them. Only the three added blocks pass untouched,
> covered by `additive_unknown_keys_are_tolerated`. Since the agentctl repo owns
> both the fixtures and the tests that pin them, the two must move in lockstep
> there; forcing a re-capture from agentd would only leave the other repo red.

## Gaps closed in this pass

### `--budget-exit-code` (new this commit)
Implemented per RFC 0011 §5.2 / `exit-codes.table.json` `x-budget-exit-code-remap`
(previously doc-only). Remaps **only** the two operator-tunable `policy` budget
codes — `EXIT_PARTIAL` (3) and `EXIT_BUDGET` (7) — to `N` at the **process** exit a
Job's `podFailurePolicy` observes; every other code (deadline 124, refusal 5, clean
0, kernel 137) is untouched. `N` is range-checked to `0..=255` (else exit 2). The
run **report** keeps the canonical 3/7 projection + precise `status`, so the durable
record stays truthful and `report.schema`-valid. New: `exit::apply_budget_remap`,
`Config.budget_exit_code` (flag + help), wiring in `run_once`, unit tests.

### De-branding input acceptance (ACC SPEC L4)
Branded forms stay accepted **and emitted** (none dropped); neutral spellings are
now **also accepted on input** (cutover to neutral-primary is a GA decision):
- **Env** (`config.rs::debrand_env`) — one normalization pass: any `AGENT_<X>`
  synthesizes the branded `AGENT_<X>` iff absent (**branded wins on conflict**, so
  every fielded deployment is byte-for-byte unchanged); every config env var is
  covered with no per-read change.
- **Identity** (`identity.rs`) — `AGENT_POD_NAME/UID/NAMESPACE/NODE_NAME`
  neutral-first, branded fallback; empty → unset retained.
- **Per-endpoint tokens** (`intel/endpoints.rs`) —
  `AGENT_INTELLIGENCE_TOKEN[_N][_FILE]` accepted; value stays opaque (L5).
- **Resource URIs** (`agentd_uri.rs`) — `agent://…` accepted on read alongside
  `agent://…` for every resource; still emits branded `agent://`.
- **Manifest** (`runtime/mod.rs::capabilities()`) — emits the neutral, unbranded
  `version` alongside `runtime: "1"` and `surfaces{exit_codes, config_schema}`;
  no branded `agentd_version` is emitted, and there is no `events_schema` key.

### Operator-surface gating uniformity (ACC SPEC L7)
A non-`Management` caller of an operator **tool or resource** — `inventory`,
`intelligence`, `capacity`, `config/effective`, `events` — now uniformly gets
`-32601` METHOD_NOT_FOUND on **both** `resources/read` and `resources/subscribe`
(previously some returned `-32002 RESOURCE_NOT_FOUND`), so a stdio peer can't even
confirm the surface exists. `cancel{handle:"0"}`/omitted fans a whole-run cancel.

> The de-branding, L7-read, and `cancel` work above predated this commit as
> uncommitted working-tree changes; it has been gated (build/test/clippy green)
> and is committed here together with `--budget-exit-code` and the L7-subscribe/
> events uniformity completion.

## P-series dispositions

| Ask | Disposition |
|---|---|
| **CC/P6** — manifest/config as consumable schemas; `--config-schema`/`--validate-config` round-trip | **RESOLVED.** Draft-2020-12 closed `--config-schema`; `--validate-config` 0/2; drift tests pin it to the struct + `contract_version`. |
| **P3b** — versioned golden corpus per feature-set | **PARTIAL (owner-pinned).** The two-fixture corpus exists and validates against the current schema, exercising both branches of every capturable sum-type. The corpus is agentctl-owned and currently pinned to the 1.3.1 captures by `fixtures.rs`; a broader per-feature matrix keyed by `(major.minor + digest)`, and re-capture to the current version, are coordinated agentctl-side tasks (see *Golden fixtures*). |
| **P2** — freeze A2A wire strings + `surfaces.a2a` | **RESOLVED (reference binding).** `surfaces.a2a` emitted; reference PascalCase `a2a.*` served. Normative spelling stays open per the contract; a gateway translates. |
| **P10** — reconcile autoscaling metric names | **RESOLVED (source wins).** Only `agent_pending_events` emitted; `agent_reactive_backlog` an alias-only, never on the scrape; `agent_tokens_per_sec`/`agent_intelligence_latency_ms` stay provisional/not-emitted. |
| **P-pause** — pause/resume served | **RESOLVED.** Both ship (frozen order), Management-gated, reflected in `agent_paused`. |
| **P4** — `agent://metrics` text body + `agent://capacity` schema | **DEFERRED (contract: OUT OF SCOPE / downstream).** `surfaces.metrics` carries the scrape address; the byte-identical Prom-text resource + capacity schema are undefined upstream. `agent://capacity` is served (cluster builds) but its frozen schema is not pinned. No agentd change for v1. |
| **P5** — re-readable distillate after exit | **DEFERRED (contract-open).** `distillate_ref` points; the durable copy is the report file + stdout. |
| **P-trace** — traceparent ingest on the A2A surface | **DEFERRED (contract-open).** `trace_id` uses the existing env/`_meta` ingest path. |
| **P3 (shard)** — `--shard auto/N` derive K from the pod ordinal | **DEFERRED (owner = agentctl).** agentd parses explicit `K/N` and rejects `N==0`/`K>=N` (exit 2); deriving `K` is the control plane's job. |
| **P-grace** — `AGENT_POD_GRACE_SECONDS` | **DEFERRED (contract-flagged provisional).** Not read in source (drain budget = `--drain-timeout`/`AGENT_DRAIN_TIMEOUT`); the neutral alias would nonetheless be honored by `debrand_env`. |

## Notes & honest absences (not gaps)

1. **`intelligence.healthy` boolean is not capturable from `--capabilities`** — the
   one-shot probe is network-free admission, always pre-connect `"unknown"`. The
   boolean branch is unit-tested on the live `agent://intelligence`/`capabilities`
   read.
2. **`_meta` input de-branding is N/A for v1** — agentd's served `tools/call`
   handler consumes **no** incoming `_meta` keys (only `name`/`arguments`); its
   `agentd/*` keys are all **outbound** stamps onto downstream MCP servers (kept
   branded, the accepted current form). There is no incoming `_meta` read site to
   alias.
3. **`run_id` is an opaque stable string, not a Crockford ULID** — no schema
   enforces a ULID pattern (`manifest`/`report` require only a non-empty string);
   agentd mints `format!("{millis:x}{pid:x}")`, stable across a retried Job when
   `AGENT_RUN_ID`/`AGENT_RUN_ID` is set. Switching the format is unnecessary for
   conformance and risk-bearing, so it is left as-is.
4. **`--config-schema` is the strict serde mirror** (reloadable keys only, closed);
   the contract's `config.schema.json` is the broader VIEW that also declares
   restart-only `mode`/`interval`/`cron` for WARN purposes. A file with those keys
   is exit 2 in the reference (deny_unknown_fields) — which the contract anticipates.
5. **Exit 124 is reachable** via the supervisor force-kill of a stuck/deadline
   subtree — consistent with `x-mode-reachability` (intent `policy`). The
   "unreachable via `once_exit()`" note is about that internal fn only.

## Contract reconciliation & asks (no schema edited by agentd)

The contract's `config.schema.json`, `metrics.registry.json`, and
`exit-codes.table.json` were reconciled upstream to match the reference (agentctl
commit `68a61b8`, "source-wins" per L8 — `intelligence`/`model_swap` config keys,
the `agent_intel_all_down` stable gauge, code 124 `returned_by_agent:true`). agentd
made **no** edit to any ACC schema in this pass; remaining disagreements are
recorded here as asks rather than schema edits:

- **C1 — metric-registry counts (the prose count is gone upstream).** The ask was
  that `SPEC.md`'s prose count be reconciled with the registry JSON. ACC 2 dropped
  the prose count entirely — §1 now just points at `metrics.registry.json`, which
  holds **54 records — 39 `live`, 13 `reserved`, 2 `child_local`** (the
  stable/legacy/provisional vocabulary is retired). Every `live` name is emitted by
  `obs/metrics.rs`; the registry JSON, not the prose, stays authoritative.
- **C2 — "ULID" wording for `run_id`** (note 3) is not schema-enforced and not met
  literally by the reference; suggest softening to "opaque stable id (ULID
  recommended)".
