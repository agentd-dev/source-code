# agentd — Agent Control Contract (ACC v1) conformance

agentd is the **reference implementation** of an agent driven by the **agentctl**
control plane. agentctl depends only on the published **Agent Control Contract
(ACC)** — never on agentd's code (principle P0). This document records agentd's
conformance to ACC v1, surface by surface.

- Contract: `/root/agentctl-dev/source-code/contract/` (`README.md`, `SPEC.md`,
  `schemas/*.json`, `fixtures/`).
- Conformance is judged by **behaviour** against the contract, not by sharing code.

## Status (after the conformance pass)

| Surface | ACC schema | Status | Notes |
|---|---|---|---|
| Capabilities manifest | `manifest.schema.json` | ✅ | `json!`→`Value` (secret-safe); all sum types correct; now emits `agent_version` + `surfaces.events_schema` |
| Management profile | `management-profile.json` | ✅ | frozen tool order; non-Management caller → `-32601`; `cancel "0"`/omitted = whole run |
| Exit codes | `exit-codes.table.json` | ✅ | table honoured; clean drain = 0 |
| Metrics | `metrics.registry.json` | ✅ | rendered names match the (corrected) registry; bounded labels |
| A2A | `a2a.methods.json` | ✅ | 6 live methods; closed error set; reference PascalCase binding; `surfaces.a2a` emitted |
| Events | `events.schema.json` | ✅ | ring envelope; `agent://events` accepts neutral scheme; `surfaces.events_schema` emitted |
| Config file | `config.schema.json` | ✅ | the one closed surface; `--validate-config`/`--config-schema`; reloadable/restart-only honoured |
| Env convention | `env-convention.json` | ✅ | downward-API identity; **neutral `AGENT_*` now accepted** alongside branded |
| Run-outcome report | `report.schema.json` | ✅ | 12 keys; never `reactive`; tokens not currency |

`cargo build -p agentd` green; `cargo test -p agentd` green (405 default-feature /
530 full-feature). Hard invariants preserved: manifest stays `json!`→`Value`;
`intelligence` structural-only; no `deny_unknown_fields` on a discovery surface;
**branded spellings are still accepted and emitted** — neutral acceptance was
*added*, never substituted.

## What was fixed in agentd

The dominant gap was **de-branding on the input side** (ACC SPEC L4: branded
`AGENTD_*`/`agentd://` stay accepted, but neutral `AGENT_*`/`agent://` must *also*
be accepted). Closed across every surface:

- **URI scheme** (`crates/agentd/src/agentd_uri.rs`) — `parse()`/`is_agentd()`
  now accept either `agentd://` or `agent://`; still **emit** branded `agentd://`.
- **Env vars** (`crates/agentd/src/config.rs`) — `debrand_env()` normalises the
  env map once: any `AGENT_<X>` synthesises `AGENTD_<X>` when the branded form is
  absent (**branded wins on conflict**), so every downstream read accepts both.
- **Identity** (`crates/agentd/src/identity.rs`) — `from_env` reads neutral-first
  with branded fallback (`AGENT_POD_NAME` → `AGENTD_POD_NAME`, …); empty ⇒ unset.
- **Intelligence credentials** (`crates/agentd/src/intel/endpoints.rs`) — accept
  `AGENT_INTELLIGENCE_TOKEN[_N][_FILE]` alongside the branded forms.

Other fixes:

- **Management gating error code** (`crates/agentd/src/mcp/server.rs`) — a
  non-Management caller of an operator tool/resource now returns
  `METHOD_NOT_FOUND (-32601)` (was `INVALID_PARAMS`/`RESOURCE_NOT_FOUND`).
- **`cancel` whole-run sentinel** (`server.rs`) — `handle:"0"` or omitted cancels
  the root subtree (new `fan_cancel`); `handle` is no longer required.
- **`surfaces.events_schema`** (`crates/agentd/src/capabilities.rs`) — emitted
  from the owning module const alongside `surfaces.events`, omitted when unserved.
- **`agent_version`** — emitted next to `agentd_version` (forward-compat; branded
  retained).

## Contract corrections (source-wins, ACC SPEC L8)

Where the *schema* had drifted from agentd's actual behaviour, the **contract**
was corrected (in the agentctl repo) — these are contract bugs, not agentd bugs:

- `config.schema.json` — added the `intelligence` and `model_swap` file keys
  (agentd's `ConfigFile` accepts them and `--config-schema` emits them); moved
  `intelligence` from restart-only to reloadable.
- `metrics.registry.json` — added the stable gauge `agent_intel_all_down`
  (agentd's `render()` emits it; RFC 0018 §6).
- `exit-codes.table.json` — code 124 (`DEADLINE`) `returned_by_agent: true` —
  it is reachable via the supervisor hard-kill ladder (not only `once_exit()`).

## Deferred / known follow-ups

- **Golden fixture re-capture against the current version.** The fixtures in
  `contract/fixtures/capabilities/` are the authored real captures from agentd
  2.5.0; re-capturing from the current binary changes `agent(d)_version` and the
  surface profile, which must be done in lockstep with agentctl's fixture tests
  (`crates/agent-contract-client/tests/fixtures.rs`). Deferred to avoid breaking
  that baseline; not required for behavioural conformance.
- **Live `intelligence.healthy` boolean** — the served `agent://capabilities`
  still reports `"unknown"`; surfacing real last-known reachability on the live
  read is a later RFC chunk (schema permits `"unknown"` always, so conformant
  today).
- **`agent://events` read/subscribe origin gating** — the operator *tool* and the
  Inventory/Intelligence/Capacity/ConfigEffective resource gates now return
  `-32601`; the `events` read/subscribe gates were left as-is (out of the cited
  scope) — flip them to `-32601` in a small follow-up for full uniformity.
- **Contract asks still open upstream:** P2 (normative A2A wire-string binding —
  both spellings recorded, reference PascalCase frozen), P4 (`agent://metrics`
  text body + `agent://capacity` schema), `AGENT_POD_GRACE_SECONDS` (RFC-specified,
  name unsettled).
