# agentd — Agent Control Contract (ACC) v1 conformance

`agentd` is the **reference implementation** of an agent driven by the **agentctl**
Kubernetes control plane. agentctl depends only on the published **Agent Control
Contract (ACC)** — never on agentd's code (principle P0). This document records
agentd's conformance to **ACC v1** (`contract_version` `1.0`), surface by surface:
what passes, the gaps closed to get there, and the P-series items deliberately
deferred.

- Contract (consumed, **never edited by agentd**):
  `/root/agentctl-dev/source-code/contract/` — `README.md`, `SPEC.md`,
  `schemas/*.json`, `fixtures/capabilities/*`.
- Conformance is judged by **behavior** against those artifacts (ACC SPEC §8), not
  by sharing code. The branded spellings stay accepted **and emitted**; neutral
  acceptance was *added*, never substituted (L4).

## Verdict — conformant to ACC v1

| Evidence | Result |
|---|---|
| `cargo test -p agentd` (full feature matrix) | **579 passed / 0 failed** |
| `cargo test -p agentd` (default features) | **406 passed / 0 failed** |
| Black-box conformance suite (`cargo run -p agentd-conformance`) | **38 passed / 0 failed** |
| ACC schema+behavior harness (drives the real binary; see *Validation report*) | **22 passed / 0 failed** |
| Golden `--capabilities` fixtures vs `manifest.schema.json` | **4 / 4 valid** |
| `cargo clippy --all-targets -D warnings` (default + full) | clean |

Hard invariants preserved: the manifest is built `json!`→`Value` (the `Secret`
type has no `Serialize`, so it cannot reach it — L5); `intelligence` is structural
only; no `deny_unknown_fields` on any discovery surface; config-input is the one
closed surface; the lethal-trifecta gate is the single `validate()` authority;
no credential reaches the manifest / config file / identity path.

## Per-surface results

| Surface | ACC schema | Status | Notes |
|---|---|---|---|
| **Capabilities manifest** | `manifest.schema.json` | ✅ PASS | `json!`→`Value` (secret-safe); all required root keys + `agentd_version`; now also emits neutral `agent_version` + `surfaces.events_schema` (when served). Sum types correct. Live `agent://capabilities` is parsed-equal to the one-shot. |
| **Management profile** | `management-profile.json` | ✅ PASS | Frozen order `[drain, lame-duck, pause, resume, cancel]`; Management-gated; a non-Management caller of an operator **tool or resource** (read **and** subscribe) → `-32601`. `attach` not a tool; no `force`. `drain ≡ SIGTERM ≡ exit 0`; `lame-duck` readiness-only; `cancel{handle:"0"\|omitted}` = whole run. |
| **Exit codes** | `exit-codes.table.json` | ✅ PASS | Table + `pod_failure_intent` exact; unknown → `retriable`; `EXIT_CODES="1.0"`. Clean drain = **0, not 143**. Only **3 & 7** remappable, via the new `--budget-exit-code`. |
| **Run-outcome report** | `report.schema.json` | ✅ PASS | 12 required keys; `report_schema="1.0"`; `mode ∈ {once,loop,schedule}` (never `reactive`); 9-value closed `status`; `distillate_ref` `^(agent\|agentd)://`; `instance`/`trace_id` omitted-not-null; tokens never currency. A real once-run report validates. |
| **Metrics** | `metrics.registry.json` | ✅ PASS | `metrics_schema="1.0"`; all **34** stable non-cgroup names present on a live scrape (`agent_memory_*` are cgroup-v2-conditional); **no histogram samples**; `agent_saturation` the only float; bounded labels only; `agent_pending_events` canonical (no `agent_reactive_backlog`). |
| **Events** | `events.schema.json` | ✅ PASS | `agent://events` read body = `{events_schema:"1.0", oldest_seq, newest_seq, dropped, events[]}`; each line carries monotonic `seq` + the required tuple; `level`/`comp` closed, `event` open; lossy ring; `?after/level/event` parsing. Accepts the neutral `agent://` scheme. Validates live. |
| **Config file** | `config.schema.json` | ✅ PASS | The one **closed** surface — typo → exit 2. `--validate-config` 0/2 (good→0, typo→2, inline secret-shaped header→2). `--config-schema` emits a valid draft-2020-12 closed doc, `x-agentd-contract-version="1.0"`. Restart-only keys rejected on a live reload. |
| **A2A** | `a2a.methods.json` | ✅ PASS | 6 live methods served, 5 gateway-owned → `-32601`; closed error set `-32001/-32601/-32602/-32603`; Management-gated; COMPLETED task = exactly one `<taskId>.distillate` artifact; framed streaming until `final`; `TASK_STATE_*` mapping; `surfaces.a2a` emitted. Live drive confirms the error codes. |
| **Env convention** | `env-convention.json` | ✅ PASS | Downward-API identity; **neutral `AGENT_*` now accepted** alongside branded `AGENTD_*` (env, identity, per-endpoint tokens). Empty → unset; `run_id` always present; `AGENT_SHARD "K/N"` rejects `N==0`/`K>=N` → exit 2; credentials only on the `*_TOKEN[_FILE]` path. |

## Validation report (live harness — drives the real binary)

Each artifact below is produced by the running binary and validated against its
contract schema with a draft-2020-12 validator (or asserted behaviorally).

```
[PASS] config:--config-schema            — draft2020-12 closed schema, x-contract=1.0
[PASS] config:good-file vs schema        — valid vs config.schema.json
[PASS] config:--validate-config (good)→0 — exit 0
[PASS] config:typo'd key→exit 2          — exit 2
[PASS] config:inline secret header→exit 2— exit 2
[PASS] report:once-run vs schema         — valid vs report.schema.json
[PASS] report:mode≠reactive / distillate_ref scheme / exit_code
[PASS] manifest:live agent://capabilities vs schema   — valid vs manifest.schema.json
[PASS] manifest:neutral agent:// scheme accepted
[PASS] manifest:live≈--capabilities (semantic, modulo run_id + live intel overlay)
[PASS] events:agent://events read-body vs schema      — valid vs events.schema.json
[PASS] a2a:GetTask unknown id→-32001 / gateway method→-32601 / empty text→-32602
[PASS] metrics:34 stable names present / no histogram samples / pending_events canonical
[PASS] exit:bad flag→2 / intel down→4 / --budget-exit-code leaves non-budget (4) untouched
  22/22 checks passed
```

The black-box `agentd-conformance` suite independently drives the exit-code table
under induced failures: `exit-0-on-success`, `exit-2-on-bad-flag`,
`exit-2-on-validation`, `exit-4-on-intel-down`, `exit-6-on-required-mcp-down`,
**`drain-0-on-sigterm`** (clean drain = 0, not 143), `spawn-rate-refused` — all pass.

### Reproduce

```sh
cargo run -p agentd-conformance                 # 38 behavioral checks
cargo build -p agentd --features "serve-mcp,a2a,events,metrics,cluster,internal-mocks"
python3 acc_validate.py ./target/debug/agentd   # 22 live schema+behavior checks
# fixtures vs schema:
for f in /root/agentctl-dev/source-code/contract/fixtures/capabilities/*.json; do
  python3 - "$f" <<'PY'
import json,sys; from jsonschema import Draft202012Validator
s=json.load(open("/root/agentctl-dev/source-code/contract/schemas/manifest.schema.json"))
print("VALID" if not list(Draft202012Validator(s).iter_errors(json.load(open(sys.argv[1])))) else "INVALID", sys.argv[1])
PY
done
```

## Golden fixtures (regenerated from v2.8.1)

`fixtures/capabilities/default.json` and `full-features.json` were re-captured from
the current binary's `--capabilities` and validate against `manifest.schema.json`.
Together they exercise **both branches of every capturable sum-type surface key**:

| sum-type key | `default.json` (release, surfaces off) | `full-features.json` (debug, surfaces on) |
|---|---|---|
| `surfaces.management` | `false` | `"vsock:7000"` |
| `surfaces.metrics` | `false` | `"127.0.0.1:9090"` |
| `surfaces.a2a` | `false` | object |
| `surfaces.claim` | **omitted** | object |
| `surfaces.shard` | `null` | `"0/3"` |
| `surfaces.events_schema` | omitted | `"1.0"` |
| `agent_version` / `agentd_version` | both present | both present |

The two synthetic fixtures (`reference-full.json`, `minimal-degraded.json`) are
untouched. `intelligence.healthy` is `"unknown"` in both real captures — its
boolean branch is reachable only on the live resource after a connection (the
one-shot probe is network-free admission, ACC SPEC §3); it is unit-tested there.

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
  synthesizes the branded `AGENTD_<X>` iff absent (**branded wins on conflict**, so
  every fielded deployment is byte-for-byte unchanged); every config env var is
  covered with no per-read change.
- **Identity** (`identity.rs`) — `AGENT_POD_NAME/UID/NAMESPACE/NODE_NAME`
  neutral-first, branded fallback; empty → unset retained.
- **Per-endpoint tokens** (`intel/endpoints.rs`) —
  `AGENT_INTELLIGENCE_TOKEN[_N][_FILE]` accepted; value stays opaque (L5).
- **Resource URIs** (`agentd_uri.rs`) — `agent://…` accepted on read alongside
  `agentd://…` for every resource; still emits branded `agentd://`.
- **Manifest** (`capabilities.rs`) — emits neutral `agent_version` next to
  `agentd_version`, plus additive `surfaces.events_schema` when the stream is served.

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
| **P3b** — versioned golden corpus per feature-set | **RESOLVED for the two real captures** (default = off/omitted/null branches; full = string/object branches; both validate). A broader per-feature matrix keyed by `(major.minor + digest)` remains a downstream corpus task. |
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
   `AGENT_RUN_ID`/`AGENTD_RUN_ID` is set. Switching the format is unnecessary for
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

- **C1 — `SPEC.md` §4.4 metric count is stale.** The prose says "46 records — 29
  stable, 8 legacy, 9 provisional"; the registry JSON now has **51 records — 36
  stable, 8 legacy, 7 provisional**. agentd emits a superset of the stable set; the
  registry JSON, not the prose, is authoritative. Suggest updating the prose.
- **C2 — "ULID" wording for `run_id`** (note 3) is not schema-enforced and not met
  literally by the reference; suggest softening to "opaque stable id (ULID
  recommended)".
