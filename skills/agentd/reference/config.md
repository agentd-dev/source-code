# agentd config reference (schema v2)

Authoritative and complete: `agentd --config-schema=2`. This is the map — which
section owns what, and the keys worth knowing in each.

Every path here is also a flag (`--agent-instruction`) and an env var
(`AGENTD_AGENT_INSTRUCTION`). Precedence: **built-in < file < env < flag**.
Repeat `-c` to merge overlays in order (later wins).

## The 21 sections

| section | owns |
|---|---|
| `config_version` | `"2"`. Required. |
| `agent` | identity, standing instruction, preflight, wake-ups, ask-human fallback |
| `intelligence` | the model endpoints, auth, budgets, failover/swap policy |
| `limits` | per-run bounds: steps, tokens, deadline, subagent depth |
| `lifecycle` | how long the process lives, run id, drain, config watching |
| `store` | durability of conversations/tasks/runs |
| `context` | compaction, retained turns, model window, plan size |
| `mcp` | MCP servers (HTTPS), their tags and tool exposure |
| `tools` | tool overrides, renames, routing |
| `security` | the `exec` fence, trifecta allowance, TLS CA, AAuth, cgroups |
| `a2a` | the HTTP listener, peers, principals, TLS/bearer client auth |
| `interface` | the TUI/web-UI surface: enable, debug, chrome, pairing |
| `workflows` | durable DAGs and their triggers |
| `webhooks` | inbound HTTP triggers |
| `observability` | log level, metrics, OTLP, health/report files |
| `memory` `knowledge` `search` `skills` | agent-side state and retrieval surfaces |
| `goal` | goal statement + stuck/achieved policy |
| `cluster` | sharding across instances |

## agent

```yaml
agent:
  name: helper
  instruction: |          # standing policy. HOW to work — not today's task.
    …
  preflight: auto         # never | auto | always — the intent classifier
  ask_human_fallback: fail  # what an unanswerable question does (see below)
  wake_on: [a2a_message, human_reply, subagent_result, workflow_finished]
  max_parallel_turns: 1
```

`ask_human_fallback` decides what happens when `ask_human` has nobody to answer:

| value | behavior | use when |
|---|---|---|
| `wait` (aliases `pause`, `idle`) | park until the ask times out | interactive — you will come back |
| `fail` (default; `finish`, `stop`) | the ask errors, the agent decides what to do | headless/CI, where hanging is worse |
| `auto` | an LLM judge answers conservatively, always marked as auto | unattended, progress beats precision |

## intelligence

```yaml
intelligence:
  endpoints: https://api.openai.com/v1     # comma-list ⇒ failover order
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"       # or token_file, or `auth:` for OAuth/SigV4/SPIFFE
  budget:
    windows: [{ per: day, tokens: 2000000 }]
    on_exhausted: refuse                   # wait | slow | degrade | refuse | fail
```

OpenAI-compatible wire with native tool-calling. `auth:` covers OAuth2 (device,
browser+PKCE, client-credentials — so Azure OpenAI and Google Vertex work
directly), AWS SigV4, and SPIFFE. `agentd --login <target>` completes a device
login and caches the token.

## limits and lifecycle

```yaml
limits:
  run: { steps: 50, tokens: 200000, deadline: 10m }   # defaults
  subagents: { depth: 3 }
lifecycle:
  run_until: drained     # auto | idle | drained. `drained` = behave like a daemon.
  watch_config: true     # reload on SIGHUP / file change (feature-gated)
```

A bare run without `run_until` finishes and exits — correct for one-shots, wrong
for anything a UI attaches to.

## store — durability

`store.kind: memory` keeps everything in the process; it is gone when the daemon
stops, including pending approvals. Point it at an MCP or HTTP store and
conversations, tasks and workflow runs survive a restart.

## a2a — the listener

```yaml
a2a:
  listen: http://127.0.0.1:8420    # loopback ⇒ caller is the operator, no credential
  bearer: "{{secret:A2A_TOKEN}}"   # REQUIRED (or mTLS/pairing) for non-loopback
  tls: { cert: …, key: …, client_ca: … }
```

Plaintext `http://` is loopback-only by design. `:0` is refused — bind an
explicit host:port.

## interface — the display clients

```yaml
interface:
  enabled: false        # default OFF; `agentd tui|ui` sets it for you
  debug: false          # opens transcripts, per-step run detail, the log ring
  origins: []           # extra allowed browser origins for the web UI
  display:              # which chrome items render, in order. Defaults:
    top:    [name, version, instance, debug]
    bottom: [conn, endpoint, draining, active, turns, tokens, screen, keys]
  pairing:
    enabled: false      # rotating 6-digit code instead of copying a bearer
    role: operator      # operator | user | agent — what a paired session gets
```

The full display vocabulary (unknown items are skipped, not an error):
`name`, `version`, `instance`, `model`, `endpoint`, `conn`, `debug`,
`draining`, `active`, `turns`, `tokens`, `tool_calls`, `runs`, `subagents`,
`conversations`, `screen`, `keys`, `clock`.

## security

```yaml
security:
  exec:                 # needs the `exec` cargo feature; NOT in release binaries
    enabled: false      # …and still off here by default
    workdir: /work
    allow: []           # argv[0] allow-list. EMPTY = nothing runs.
    timeout: 120s
    max_output: 262144
    env: [PATH, HOME, LANG]
  allow_trifecta: false # untrusted input + sensitive powers + egress ⇒ refuse to start
```

## Checking your work

```sh
agentd --validate-config -c agent.yaml   # 0 or 2, before any LLM call
agentd --capabilities                    # what this binary actually supports
agentd --workflow-schema                 # workflow dialect 3 + node registry
```
