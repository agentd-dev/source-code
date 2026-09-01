# RFC 0030: Configuration schema v2

**Status:** Implemented (agentd 2.0 track, phase P1; the parameter set is refined by P2–P6 as their features land)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.12); supersedes the flag/env parameter set of RFC 0011 §2 / RFC 0017 §3 (docs/configuration.md §3); built on the config **mechanism** landed 2026-08-16 (`config::{file,yaml,paths}` — YAML/JSON, multi-file JSON-Merge-Patch, path-derived env `AGENTD_<PATH>`/`AGENT_<PATH>`/`<PATH>`, generic `--<path>` flags, hot reload).

---

## 1. Summary

agentd 2.0 is configured by **one nested document** (YAML or JSON; several
files merge in order) whose every path is also an env var and a flag. The
document is versioned (`config_version: "2"`); a v1 document (flat keys such
as `model`, `subscribe`, `mcp_servers`) is recognised and refused with a
migration hint once v1 is removed (until then it selects the v1 runtime). A
short list of **aliases** keeps the quickstart flags (`--instruction`,
`--intelligence`, `--model`, `--mcp`, `--config`, `--log-level`, …). Precedence
is unchanged: `built-in < files < env < flags`; setting a path sets its value;
the named repeatable flags add.

## 2. Detection

A document is v2 when `config_version` is `"2"` **or** any v2-only top-level
key is present (`agent`, `intelligence`, `store`, `workflows`, `tools`, `a2a`,
`lifecycle`, `observability`, `security`, `knowledge`, `search`, `skills`,
`memory`, `context`, `limits`, `cluster`). Mixing v1 and v2 keys is refused.

## 3. The schema (paths, types, defaults, reload class, alias)

Reload class: **R** = reloadable at a quiesce boundary (RFC 0017 §5 semantics),
**S** = restart-only (a live reload that changes it is refused). Type notation
follows JSON Schema; `duration` = the RFC 0011 duration string (`10m`, `500ms`,
bare seconds); `secret` = a string that MUST be a `{{secret:NAME}}` /
`{{secret-file:PATH}}` reference or come from env/flag (never an inline
credential in a file).

### 3.1 `agent`

| path | type | default | class | alias |
|---|---|---|---|---|
| `agent.name` | string | downward-API instance › hostname › `agentd` | S | — |
| `agent.instruction` | string — text, or a single-token URI a configured MCP server serves (read + subscribed) | — | R | `--instruction`, `--instruction-file` (reads the file), `INSTRUCTION` |
| `agent.preflight` | `never|auto|always` | `auto` | R | — |
| `agent.wake_on` | array of `a2a_message|human_reply|subagent_result|workflow_finished|workflow_failed|instruction_updated|budget_resumed` | `[a2a_message, human_reply, subagent_result, workflow_failed]` | R | — |
| `agent.on_workflow_finished` | `ignore|note|think` | `note` | R | — |
| `agent.tools.internal` | `all|none` or array of names | `all` | R | — |
| `agent.tools.mcp` | `all|none` or array of `server` / `server.tool` | `all` | R | — |
| `agent.tools.code` | `all|none` or array | `all` | R | — |
| `agent.max_parallel_turns` | integer ≥ 1 | 4 | R | — |
| `agent.conversation_budget` | budget object (§3.2) | — | R | — |

### 3.2 `intelligence`

| path | type | default | class | alias |
|---|---|---|---|---|
| `intelligence.endpoints` | array of URL (or one comma-separated string) — HTTPS, loopback http for dev | — (required unless no intelligence use) | R (hot-swap, RFC 0018) | `--intelligence`, `INTELLIGENCE` |
| `intelligence.model` | string | — | R | `--model` |
| `intelligence.token` | secret | — | R | `--intelligence-token` |
| `intelligence.token_file` | path | — | R | `--intelligence-token-file` |
| `intelligence.headers` | map string→string (secret refs allowed) | `{}` | R | — |
| `intelligence.swap_policy` | `finish-on-old|restart-turn` | `finish-on-old` | R | `--model-swap` |
| `intelligence.structured_output` | `auto|json_schema|tool|prompt` | `auto` | R | — |
| `intelligence.budget.windows[]` | `{per: second|minute|hour|day|week, tokens?: int, requests?: int, reset?: "HH:MMZ"}` | `[]` | R | — |
| `intelligence.budget.lifetime_tokens` | integer ≥ 0 (0 = unbounded) | 0 | R | `--budget-tokens-lifetime` |
| `intelligence.budget.scope` | array of `instance|run|conversation|principal` | `[instance]` | R | — |
| `intelligence.budget.on_exhausted` | `wait|slow|degrade|refuse|fail` | `wait` | R | — |
| `intelligence.budget.slow.factor` | number (0,1] | 0.5 | R | — |
| `intelligence.budget.degrade.model` | string | — | R | — |
| `intelligence.budget.reserve` | `{estimate: context|fixed|none, fixed?: int}` | `{estimate: context}` | R | — |
| `intelligence.pricing` | map model→`{input_per_1k, output_per_1k, currency}` | `{}` | R | — |
| `intelligence.timeout` | duration | `60s` | R | — |

### 3.3 `mcp`

| path | type | default | class | alias |
|---|---|---|---|---|
| `mcp.servers[]` | `{name, endpoint, ns?, headers?, tags?, aauth?, oauth?: {token_url, client_id, client_secret, scope?}, timeout?}` | `[]` | R (re-handshake) | `--mcp name=endpoint` (adds), `--mcp-tags name=tags` |
| `mcp.default_timeout` | duration | `60s` | R | — |

### 3.4 `tools`

| path | type | default | class |
|---|---|---|---|
| `tools.disabled` | array of tool names | `[]` | R |
| `tools.overrides` | map name→`{server, tool, args?, result?}` | `{}` | R |

### 3.5 `store` (RFC 0025)

| path | type | default | class |
|---|---|---|---|
| `store.kind` | `mcp|http|memory|none` | `none` (refused when durability is required) | S |
| `store.prefix` | string | `agentd` | S |
| `store.mcp` | `{server, put, get, list?, delete?}` (op = `{tool, args?, ok?, conflict?, value?, keys?}`) | default checkpointer profile | S |
| `store.http` | `{base_url, headers?, get, put, list?, delete?}` (op = `{method, url, body?, value?, keys?, conflict_status?}`) | — | S |
| `store.checkpoint.debounce_ms` | integer | 250 | R |
| `store.durability.a2a` / `store.durability.steps` | `strict|eventual` | `strict` / `eventual` | R |
| `store.on_error` | `halt|degrade` | `halt` | R |
| `store.audit` | boolean | false | R |
| `store.timeout` | duration | management timeout | R |

### 3.6 `memory`, `context`, `knowledge`, `search`, `skills`

| path | type | default | class |
|---|---|---|---|
| `memory.max_value_bytes` | integer | 65536 | R |
| `memory.list_default_limit` | integer | 100 | R |
| `context.compact_at` | number (0,1] | 0.7 | R |
| `context.keep_last` | integer | 12 | R |
| `context.plan.max_items` | integer | 32 | R |
| `knowledge.server` | server name | — | R |
| `knowledge.auto_context` | `{on: turn|never, top_k, max_bytes}` | `{on: never, top_k: 5, max_bytes: 16384}` | R |
| `search.server` | server name | — | R |
| `skills.sources[]` | `{server, discover: prompts|resources|auto, filter?}` | `[]` | R |
| `skills.reference_prefix` | string | `@skill:` | R |
| `skills.max_loaded` / `skills.max_bytes` | integer | 8 / 32768 | R |

### 3.7 `workflows`

`workflows[]` — either an inline dialect-3 definition (RFC 0027 §2) or a
reference `{name, file}` / `{name, uri}` (an MCP resource: read + subscribed,
definition updates re-validate and re-arm). Class R (definitions), with live
runs continuing on their pinned hash (`resume_policy`). Alias: `--workflow F`
appends `{file: F}`.

### 3.8 `limits`

| path | type | default | class | alias |
|---|---|---|---|---|
| `limits.max_runs` | integer | 8 | R | — |
| `limits.run.steps` | integer | 500 | R | `--max-steps` |
| `limits.run.tokens` | integer | 2 000 000 | R | `--max-tokens` |
| `limits.run.deadline` | duration | `1h` | R | `--deadline` |
| `limits.subagents.depth` / `breadth` / `total` / `rate` | integer / integer / integer / `"<burst>/<per>s"` | 3 / 8 / 64 / `8/2s` | R | `--max-depth` |
| `limits.inline_max_bytes` | integer | 65536 | R | — |
| `limits.step_timeout` | duration | `10m` | R | — |

### 3.9 `lifecycle`

| path | type | default | class | alias |
|---|---|---|---|---|
| `lifecycle.run_until` | `auto|idle|drained` | `auto` | S | — |
| `lifecycle.idle_grace` | duration | `5s` | R | — |
| `lifecycle.drain_timeout` | duration | `25s` | S | `--drain-timeout` |
| `lifecycle.run_id` | string | minted | S | `--run-id` |
| `lifecycle.exit_code_map` | map status→int (only policy codes 3/7 remappable) | `{}` | S | `--budget-exit-code` |
| `lifecycle.watch_config` | boolean | false | S | `--watch-config` |

### 3.10 `a2a` (RFC 0029)

| path | type | default | class | alias |
|---|---|---|---|---|
| `a2a.listen` | URL `https://host:port` (loopback `http://` dev) | — (no listener) | S | `--serve-mcp` (renamed: `--listen`) |
| `a2a.tls.cert` / `key` / `client_ca` | paths | — | S (files re-read live) | `--serve-cert/-key/-client-ca` |
| `a2a.bearer` | secret | — | S | `--serve-bearer` |
| `a2a.principals[]` | `{match: {san?, sub?, bearer_ref?, aauth_agent?, any?}, role, grants?, quotas?}` | `[]` (loopback ⇒ operator, as today) | R | — |
| `a2a.peers[]` | `{name, endpoint, headers?, client_cert?, client_key?}` | `[]` | R | `--a2a-peer` (adds) |
| `a2a.conversation_ttl` | duration | `30d` | R | — |

### 3.11 `observability`, `security`, `cluster`

| path | type | default | class | alias |
|---|---|---|---|---|
| `observability.log_level` | `trace|debug|info|warn|error` | `info` | R | `--log-level` |
| `observability.log_content` | boolean | false | R | `--log-content` |
| `observability.otel` | `{endpoint, traces: bool, metrics: bool, logs: bool}` | env `OTEL_EXPORTER_OTLP_ENDPOINT` | S | — |
| `observability.metrics_addr` | host:port | — | S | `--metrics-addr` |
| `observability.health_file` | path | — | S | `--health-file` |
| `observability.report_file` | path | — | R | `--report-file` |
| `observability.events_ring` | integer | 1024 | S | `--events-ring` |
| `observability.audit.sink` | array of `log|store` | `[log]` | R | — |
| `observability.traceparent` | W3C string | — | S | `--traceparent` |
| `security.allow_trifecta` | boolean | false | S | `--allow-trifecta` |
| `security.tls_ca` | path | — | S | `--tls-ca` |
| `security.aauth` | `{provider, key_file, enroll_token?, enroll_assertion_file?, person_server?}` | — | S | `--aauth-*` |
| `security.cgroup` | `{spec, memory_max?, pids_max?}` | — | S | `--cgroup*` |
| `cluster.shard` | `K/N` | — | S | `--shard` |
| `cluster.timer_shard` | `shard0|keyed` | `shard0` | S | — |

## 4. Env and flags

Derived by the mechanism: `agent.instruction` ⇒ `AGENTD_AGENT_INSTRUCTION` ›
`AGENT_AGENT_INSTRUCTION` › `AGENT_INSTRUCTION`; `intelligence.model` ⇒
`AGENTD_INTELLIGENCE_MODEL` › … › `INTELLIGENCE_MODEL`; flags `--<path>` with
`.`/`_`/`-` spellings; a dotted flag reaches into a map (`--tools.overrides.
memory.get '{…}'`, `--intelligence.headers.x-team ops`). Aliases in §3 are
kept for the quickstart; the bare-name env allow-list is `INSTRUCTION`,
`INTELLIGENCE`, plus every path's bare spelling as today (open question in
progress.md).

## 5. Validation (exit 2, before any side effect)

`deny_unknown_fields` per object; secrets by reference only; HTTPS-only
endpoints (loopback http dev carve-out); `store.kind: none` refused when
durability is required (any workflow, A2A listener, or `agent.instruction`
run); overrides/disabled consistency (RFC 0028 §4); workflows validated (RFC
0027 §8) including references to servers/tools/knowledge/skills sources;
principals matchers well-formed; the trifecta gate over the root grant and
each workflow/subagent grant; `--validate-config` collects everything.

## 6. Reload

SIGHUP / `lifecycle.watch_config` re-merge the files and re-validate; the
class-**S** paths must be unchanged (else `restart_required`); class-**R**
changes apply at the quiesce boundary (RFC 0017 §5.3 choreography):
intelligence swap (RFC 0018), MCP re-handshake, registry rebuild (overrides,
disabled), workflow definitions re-armed (live runs keep their hash), budgets
re-windowed, principals refreshed.

## 7. Aliases and sugar

`agentd --instruction "X"` with no v2 document ⇒ a synthetic v2 document
`{agent: {instruction: X}, workflows: [{name: main, steps: {start: {kind: once},
work: {kind: agent, depends_on: [start], instruction: "{{env.instruction}}"},
done: {kind: finish, depends_on: [work], output: "{{steps.work.output}}"}}]}`
and `lifecycle.run_until: auto` (⇒ idle ⇒ exit code from `finish`). The
`--config-schema` export prints the v2 schema (`contract_version: "2.0"`).
Removed flags (`--mode`, `--subscribe`, `--continue`, `--interval`, `--cron`)
exit 2 with the migration hint naming the start-node kind to use.

## 8. Test plan

Schema/struct drift test per object; every path binds to env + flag and
round-trips a sample; alias table; v1/v2 detection and mixed-key refusal;
validation cases per §5; reload partition per path; `--instruction` sugar
document; `--config-schema` v2 export.
