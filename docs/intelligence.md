# Intelligence: the LLM wire

agentd reaches the model over **one logical wire**. The agentic ReAct loop —
which runs only inside a child process, a turn worker or a subagent — sends it
messages plus a scoped tool catalogue and gets back text *and* structured tool
calls. That wire is named in
`AGENT_INTELLIGENCE` (or `--intelligence`) and authenticated with
`AGENT_INTELLIGENCE_TOKEN` (or `--intelligence-token`). For resilience the wire
can list **several endpoints** (failover priority by order), each with its own
credential, and the list + model are **hot-swappable without a restart** — but it
stays one model-facing channel. That is the whole surface.

This is the **intelligence wire** — the model-facing channel. It is
**categorically not MCP.** Tools come from MCP servers (RFC 0004); this channel
only carries the LLM request/response. Do not conflate the two.

> **One wire, many endpoints.** `AGENT_INTELLIGENCE` takes an **ordered list**
> of endpoints for failover (see
> [Resilience](#resilience-multi-endpoint-failover--the-circuit-breaker)), and
> that list and the model are **hot-swappable** without a restart (see
> [Runtime hot-swap](#runtime-hot-swap-model-swap)). The single-endpoint
> behaviour described first is exactly the one-element-list case.

## The one URI: HTTPS

Intelligence is reached over **HTTPS** — a hand-rolled HTTP/1.1 client over a
`Read + Write` byte stream, so agentd ships no async runtime and no `url`/ICU
stack. The endpoint is a single transport, `https://`, with a **loopback
`http://` carve-out for local development**.

| URI form | Transport | Use case | Build |
|---|---|---|---|
| `https://host[:port]/path` | TCP + TLS | direct provider, or a gateway sidecar/service | feature `tls` (default) |
| `http://127.0.0.1[:port]/path` | TCP, loopback only | a same-host dev gateway (LiteLLM, a local vLLM, your own proxy) | core |

The URI is validated **at startup**, before any side effect. A scheme that isn't
`https://` (or a **loopback** `http://`) exits `2` in milliseconds:

```
$ agentd --instruction 'hi' --intelligence ftp://x
agentd: intelligence endpoint must be https://host[:port][/path] (got: ftp://x)
$ echo $?
2
```

A non-loopback `http://` is rejected — plaintext to a remote LLM would leak the
prompt and the token. Terminate TLS + provider auth at a gateway if you don't want
the key in the agentd process; agentd reaches that gateway over `https://`, or over
`http://127.0.0.1` when it is a same-host sidecar.

### `https://` — direct provider or gateway (feature `tls`)

```bash
export AGENT_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd \
  --instruction 'summarize the open incidents' \
  --intelligence https://api.openai.com/v1/chat/completions \
  --model gpt-4o \
  --mcp incidents=https://intra/incidents/mcp
```

TLS is rustls with the `ring` provider and `webpki-roots` — no C toolchain, no
cmake. SNI is the parsed host.

### `http://127.0.0.1` — a same-host dev gateway (loopback only)

```bash
agentd \
  --instruction-file ./task.md \
  --intelligence http://127.0.0.1:4000/v1/chat/completions \
  --model gpt-4o \
  --mcp fs=https://intra/fs/mcp
```

A loopback gateway (a sidecar in the same pod, a dev proxy) terminates TLS and
provider auth; agentd talks plaintext HTTP to it over loopback only. Any other
`http://` host is a startup error.

---

## The wire: OpenAI-compatible by default

agentd ships **three** in-binary adapters, selected with `intelligence.dialect`
(`--intelligence-dialect`, `AGENTD_INTELLIGENCE_DIALECT`): `openai` (the
default), `anthropic`, and `bedrock`. The bias is deliberate: few adapters, thin
binary, push provider quirks to a gateway.

### Canonical: `openai-compatible` `POST /v1/chat/completions`

This is what the loop emits and parses by default. It covers vLLM, Ollama,
LM-Studio, OpenAI proper, and most hosted gateways, and gives the model
first-class `tools` + `tool_calls` (native tool-calling). The request body the
adapter builds, with one round of tool-calling in the transcript:

```jsonc
{
  "model": "gpt-4o",
  "max_tokens": 1024,
  "messages": [
    {"role": "system", "content": "…"},
    {"role": "user", "content": "read /etc/hosts"},
    {"role": "assistant", "content": null,
     "tool_calls": [{"id": "call_1", "type": "function",
       "function": {"name": "fs.read", "arguments": "{\"path\":\"/etc/hosts\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "127.0.0.1 localhost"}
  ],
  "tools": [
    {"type": "function", "function": {
      "name": "fs.read", "description": "Read a file",
      "parameters": { /* MCP inputSchema, verbatim */ }}}
  ],
  "tool_choice": "auto"
}
```

Auth header: `Authorization: Bearer <token>`. The key is **optional** — a local
keyless vLLM/Ollama needs no token. Each `tools[]` entry's `parameters` is the
MCP `tools/list` `inputSchema` passed through verbatim (RFC 0004 owns
discovery). The adapter reads back `choices[0].message.content`,
`choices[0].message.tool_calls[]` (parsing each `function.arguments` string into
a JSON object), `finish_reason`, and `usage.{prompt_tokens,completion_tokens}`.

### `anthropic` `POST /v1/messages`

Selected with `intelligence.dialect: anthropic`. Headers are `x-api-key: <token>` +
`anthropic-version: 2023-06-01`. The system prompt is extracted out-of-band into
the top-level `system` field; tools map to Anthropic's
`{name, description, input_schema}` (same `input_schema` key — passed through
verbatim). Assistant tool calls serialize as `tool_use` content blocks; tool
results as `tool_result` blocks. `stop_reason` normalises into the same finish
reason, usage from `usage.{input_tokens,output_tokens}`.

### `bedrock` — the Amazon Bedrock Converse wire

The one dialect that requires an auth block: `intelligence.auth.kind` must be
`aws`, because every dial is SigV4-signed rather than bearer-authenticated
(validation rejects the pair otherwise). The model id rides the **URL path** —
`/model/{modelId}/converse` — not the request body, so `model:` is the Bedrock
model id or an inference-profile id/ARN. Tool-calling and system prompts map
onto the Converse shapes. See
[Authentication](authentication.md#enterprise-llm-providers-azure-google-aws)
for a complete block.

### Anything else → push it to a gateway

Gemini, Cohere, and other providers are **not** in the binary. Run a gateway
that exposes an OpenAI-compatible `/chat/completions`, point `AGENT_INTELLIGENCE`
at it (`https://`, or a loopback `http://` for dev), and the canonical adapter
handles the rest. This keeps the binary thin and the provider matrix out of
agentd's release cadence.

---

## Native tool-calling

Native tool-calling is the **only** tool path. Every dial that has a non-empty
catalogue carries it in the dialect's own field — `tools` + `tool_choice:"auto"`
(openai), `tools` with `input_schema` (anthropic), `toolConfig.tools[].toolSpec`
(bedrock) — and the loop reads the model's structured tool calls back out. There
is no knob and no prompt-embedded action protocol: no catalogue rendered into
the system prompt, no brace-matching over prose.

The consequence is a requirement, not a fallback. An endpoint that ignores
`tools` never asks for one, so the turn ends on the model's first message. Put
an OpenAI-compatible gateway that implements tool-calling in front of a model
that lacks it.

---

## Credentials

The credential is resolved **per endpoint** and **never logged**. Set it from
env, from a flag, or from a config file through a `{{secret:…}}` reference (see
[Authentication](authentication.md) for the full `auth:` block — OAuth, AWS
SigV4, SPIFFE):

```bash
# flag (sets endpoint 1's credential)
agentd … --intelligence-token "$OPENAI_API_KEY"
# or env (preferred for 12-factor / secret mounts)
export AGENT_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd …
# or read from a mounted file (rotation-friendly)
export AGENT_INTELLIGENCE_TOKEN_FILE=/var/run/secrets/llm/token
agentd …
```

### Per-endpoint credentials

With a multi-endpoint list, each element resolves its **own** credential by
position (1-indexed):

| Endpoint | Inline env | File env |
|---|---|---|
| 1 (primary) | `AGENT_INTELLIGENCE_TOKEN` (or `--intelligence-token`) | `AGENT_INTELLIGENCE_TOKEN_FILE` (or `--intelligence-token-file`) |
| 2 | `AGENT_INTELLIGENCE_TOKEN_2` | `AGENT_INTELLIGENCE_TOKEN_2_FILE` |
| *N* | `AGENT_INTELLIGENCE_TOKEN_<N>` | `AGENT_INTELLIGENCE_TOKEN_<N>_FILE` |

Precedence per endpoint: an explicit inline env override wins, then the `…_FILE`
variant, then (endpoint 1 only) the resolved `--intelligence-token`. An endpoint
with no token resolved is legal — a public/keyless gateway needs none. The list
URI itself **never carries a key**.

Rules:

- **Never inline in a file.** A config file may name the credential only through
  a `{{secret:NAME}}` / `{{secret-file:PATH}}` reference; a literal value there is
  a validation error (`intelligence.token carries an inline credential`). Env and
  flag values may be literal. Wherever it comes from, the resolved secret is never
  persisted and never put in the transcript fed back to the model.
- **Redacted everywhere.** The `Config` `Debug` impl prints the token as `***`;
  the secret-header allowlist keeps `authorization` / `x-api-key` out of the
  JSON-lines logs and any span; the endpoint-health telemetry shows transport +
  index only. There is a test asserting the raw value never appears.
- **Optional for keyless endpoints.** A local vLLM/Ollama on a loopback `http://`
  endpoint (dev) needs no token at all.
- **File rotation.** A named-but-unset per-endpoint token *file* is caught at
  startup (exit 2) so a failover never discovers an unreadable secret. The
  `…_FILE` variants are read through the secret-file reader, the rotation-friendly
  path for k8s Secret mounts / Vault Agent sidecars.

Example of the redaction (the token is set but never echoed):

```jsonc
// proc.start — note: no token field exists anywhere in the log stream
{"ts":"2026-06-25T12:00:00Z","level":"info","event":"proc.start","run_id":"r-…",
 "agent_id":"sup","agent_path":"0","comp":"supervisor","pid":1,
 "version":"2.0.0","runtime":"2.0","instance":"agentd",
 "config_files":["settings.yaml"]}
```

---

## How the call behaves

- **One connect per call**, `Connection: close` — no keep-alive, no pooling. The
  request rate is single-digit per second per subagent, so this is free.
- **Synchronous and blocking** for the subagent's turn — the agentic loop is
  single-threaded per subagent. The supervisor never blocks on the LLM call.
- **Non-streaming** (`stream:false`). A timeout surfaces as a transient
  transport error and is retried with bounded backoff (RFC 0007).
- **HTTP status taxonomy** (RFC 0007 / RFC 0011):
  - `429` / `5xx` → bounded retry with backoff + jitter.
  - `401` / `403` → fatal auth → **exit 4**.
  - connection refused/reset → fatal intelligence-unreachable → **exit 4**.
  - a **named-but-unset** key is caught at startup → **exit 2** (validate first,
    don't burn a round-trip on a 401).

---

## Resilience: multi-endpoint failover & the circuit breaker

`AGENT_INTELLIGENCE` (or `--intelligence`) accepts an **ordered,
comma-separated list** of endpoints. List order *is* failover priority — the
first element is the primary. A single-element list is exactly the
single-endpoint behaviour above; the failover/breaker machinery is inert with one
endpoint.

```bash
# a primary provider, a second region, then a loopback sidecar as last resort
agentd \
  --intelligence 'https://gw-a.example/v1,https://gw-b.example/v1,http://127.0.0.1:4000/v1' \
  --model claude-opus-4 \
  …
```

Every element is an `https://` endpoint (or a loopback `http://` sidecar), and each
resolves its **own** credential (see [Credentials](#credentials)).

### The failover sweep (sticky-primary)

Each logical `complete` call wraps one bounded sweep over the list:

- Try the **active** endpoint. On a **failover-class** error — connection
  refused/reset, timeout, HTTP `5xx`, or `429` — advance to the next *available*
  endpoint in list order.
- A **non-failover** error is returned immediately, with no failover: `401`/`403`
  auth, other `4xx`, or a malformed body are the same on every endpoint, so
  trying the next one only wastes a round-trip. (An auth failure on *every*
  endpoint is a misconfig → **exit 4**, never an endless backoff loop.)
- On success, snap `active` **back to the lowest-index healthy endpoint**
  (sticky-primary), so a fallback is temporary by construction — once the primary
  recovers, the next call returns to it.

The wire/adapter/JSON path is unchanged; only endpoint *selection* wraps it. Each
attempt still dials fresh (`Connection: close`).

### The per-endpoint circuit breaker

Every endpoint carries its own three-state breaker, decided **synchronously**
against the wall clock when the endpoint is consulted — no prober thread, no
background timer:

| State | Meaning |
|---|---|
| `closed` | Normal, in rotation. |
| `open` | Removed from rotation for a cooldown after **3 consecutive** failover-class failures. |
| `half-open` | After the cooldown elapses the next consult promotes it to half-open: it is eligible for exactly **one** probe — success re-closes it, failure re-opens it with a longer cooldown. |

The cooldown starts at **5s** and doubles on each consecutive open up to a **60s**
cap. While an endpoint's breaker is open-and-cooling it is skipped entirely (no
failover advance is even recorded for it). When **every** endpoint is
open-and-cooling, the list is "all down": on a `once` run that surfaces as
**exit 4**; a long-lived daemon backs off and keeps serving (it does not crash on
a transient roll).

These transitions feed the metrics (`agent_intel_up`,
`agent_intel_errors_total{reason}`) and the `intel.*` events — see
[Observability](observability.md).

### Endpoint health — the failover snapshot

Each subagent keeps a live view of every intelligence endpoint's health. It is
what the failover sweep and the breakers read, and it never holds a URL, cid, or
credential (RFC 0012 §3.7) — only the bounded structural `transport` + `addr` and
the live counters:

```jsonc
{
  "active": 0,
  "all_down": false,
  "model": "claude-opus-4",
  "endpoints": [
    { "index": 0, "transport": "https", "addr": "gw-a.example", "state": "closed",
      "active": true, "ewma_latency_ms": 41, "error_rate": 0.0, "consec_fail": 0,
      "last_ok_ms_ago": 120 },
    { "index": 1, "transport": "https", "addr": "gw-b.example", "state": "open",
      "active": false, "ewma_latency_ms": 0, "error_rate": 1.0, "consec_fail": 3,
      "opened_ms_ago": 800, "cooldown_ms": 5000, "last_err": "refused" }
  ]
}
```

The `addr` is the bounded structural address (`host[:port]` with the path
dropped) — enough to tell endpoints apart, never a secret.

What leaves the process is the summary, not the snapshot. The supervisor has no
LLM of its own, so a child reports **transport + index only** on entering and
leaving all-down; the supervisor latches that into `agent_intel_all_down`, the
`intel.health` event, and `/readyz` (which flips to `503` while every endpoint is
open). Per-call outcomes drive `agent_intel_up`, `agent_intel_errors_total{reason}`,
`agent_intel_calls_total`, and `agent_intel_call_duration_ms` — see
[Observability](observability.md).

---

## Runtime hot-swap (`--model-swap`)

The intelligence endpoint list and the model are **reloadable** — a hot reload
(SIGHUP, or a watched config-file change; see
[Configuration](configuration.md)) that changes `intelligence.endpoints` or
`intelligence.model` swaps the model **live**, with no restart:

- **New spawns** use the new config immediately (the spawn template is
  repointed).
- **In-flight runs** — turn workers and subagents already running — receive a
  control frame and apply it at the **next turn boundary**. An in-flight model
  call (`complete_once`) is **never torn**, and the conversation transcript is
  continuous (no context reset).

A repoint that changes only the *endpoint list* (model unchanged) is always
invisible — the run rebuilds its client with **fresh breaker state** (so no stale
breaker carries to a new endpoint) and continues. The endpoint URL and credential
travel on the control frame like the spawn payload and are **never logged**.

`--model-swap` (env `AGENT_MODEL_SWAP`) controls only what happens when a reload
changes the **model** under an in-flight turn:

| Policy | Behaviour |
|---|---|
| `finish-on-old` *(default)* | The turn in flight when the reload lands **completes on the old model**; the next turn uses the new model over the full existing transcript. Cheapest — no wasted work. |
| `restart-turn` | The in-flight turn still finishes (the model call is never torn), but its result is **discarded and the turn re-runs** on the new model from the same pre-turn transcript. Costs one turn, bounded by the step budget. |

A swap is audited with the `intel.swap` event (kind `model` or `endpoint`, the
model names, the policy, and whether the endpoint list changed — **never** a
token or URL).

A `ConfigMap`-driven roll is the canonical trigger: mount the config file from a
ConfigMap, run with `--watch-config` (needs `--config` + `--features
config-watch`), and a ConfigMap update reloads the endpoint list/model live. The
intelligence **endpoint identity is reloadable via the config-file schema**
(`intelligence.endpoints` / `.model` / `.swap_policy`); the credential in that
file is a `{{secret:…}}` reference, so rolling the ConfigMap never moves a
literal secret.

---

## The real flag/env surface

These are the flags and env vars the binary accepts. (Env name in parentheses;
the flag wins over env, which wins over the default.)

| Flag | Env | Meaning |
|---|---|---|
| `--intelligence <URI[,URI…]>` | `AGENT_INTELLIGENCE` | the endpoint **list**: comma-separated `https://` (or a loopback `http://`), order = failover priority (required) |
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | endpoint-1 bearer / `x-api-key` value (never logged) |
| `--intelligence-token-file <PATH>` | `AGENT_INTELLIGENCE_TOKEN_FILE` | read endpoint-1's token from a mounted file (rotation) |
| *(per-endpoint, env-only)* | `AGENT_INTELLIGENCE_TOKEN_<N>` / `…_<N>_FILE` | endpoint *N*'s token / token-file (1-indexed, N ≥ 2) |
| `--model <NAME>` | `AGENT_MODEL` | model id sent in the request body (reloadable) |
| `--model-swap <POLICY>` | `AGENT_MODEL_SWAP` | in-flight model-swap policy: `finish-on-old` (default) \| `restart-turn` |
| `--intelligence-dialect <D>` | `AGENTD_INTELLIGENCE_DIALECT` | wire dialect: `openai` (default) \| `anthropic` \| `bedrock` |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | token budget for the run (default 2000000) |
| `--deadline <dur>` | `AGENT_DEADLINE` | wall-clock deadline, e.g. `600s`, `5m` (default 3600s) |

Every one of these is a `config_version: "2"` document path as well —
`intelligence.endpoints`, `intelligence.token`, `intelligence.model`,
`intelligence.swap_policy`, `intelligence.dialect`, `limits.run.tokens`,
`limits.run.deadline` — settable from a file, from `AGENTD_<PATH>`, or from
`--<path>`; the flags above are the short spellings. Durations accept `ms`, `s`,
`m`, `h`, or a bare integer (seconds). The endpoint list and `model` are
**reloadable** (see [Configuration](configuration.md) and the
[hot-swap](#runtime-hot-swap-model-swap) section); a token in a file must be a
`{{secret:…}}` reference.

---

## See also

- [Configuration reference](configuration.md) (the full flag/env surface + the reloadable config file)
- [Authentication](authentication.md) (the `auth:` block — OAuth, AWS SigV4, SPIFFE — on this endpoint)
- [Observability](observability.md) (the `intel.*` events, `agent_intel_*` metrics, the breaker signals)
- [Deployment](deployment.md) and [Scaling](scaling.md) (multiple daemon replicas coordinate through the durable store)
- [RFC 0006 — Intelligence transport & wire](../rfcs/0006-intelligence-transport-and-wire.md) (this channel, in full)
- [RFC 0018 — Intelligence transport resilience](../rfcs/0018-intelligence-transport-resilience.md) (failover, the breaker, swap)
- [RFC 0004 — MCP client subset & codec](../rfcs/0004-mcp-client-subset-and-codec.md) (where tools come from)
- [RFC 0007 — Agentic loop & terminal status](../rfcs/0007-agentic-loop-and-terminal-status.md) (who calls `complete`)
- [RFC 0012 — Security posture](../rfcs/0012-security-posture.md) (SSRF, header injection, secret handling)
