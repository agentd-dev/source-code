# Intelligence: the LLM wire

agentd reaches the model over **one logical wire**. The agentic ReAct loop —
which runs only inside a subagent process — sends it messages plus a scoped tool
catalogue and gets back text *and* structured tool calls. That wire is named in
`AGENT_INTELLIGENCE` (or `--intelligence`) and authenticated with
`AGENT_INTELLIGENCE_TOKEN` (or `--intelligence-token`). For resilience the wire
can list **several endpoints** (failover priority by order), each with its own
credential, and the list + model are **hot-swappable without a restart** — but it
stays one model-facing channel. That is the whole surface.

This is the **intelligence wire** — the model-facing channel. It is
**categorically not MCP.** Tools come from MCP servers (RFC 0004); this channel
only carries the LLM request/response. Do not conflate the two.

> **Status.** The runtime is implemented: config validation, the agentic ReAct
> loop, the transport/adapter machinery, and the supervisor + subagent process
> tree all ship and are tested. The resilience wave — multi-endpoint failover,
> the per-endpoint circuit breaker, runtime model hot-swap, and best-effort model
> discovery — also ships (RFC 0018); those sections describe live behaviour. The
> examples below describe live behavior per
> [RFC 0006](../rfcs/0006-intelligence-transport-and-wire.md).

---

> **One wire, many endpoints.** The model-facing channel is still a single
> logical wire, but `AGENT_INTELLIGENCE` now accepts an **ordered list** of
> endpoints for failover (see [Resilience](#resilience-multi-endpoint-failover--the-circuit-breaker)),
> the endpoint list and model are **hot-swappable** without a restart (see
> [Runtime hot-swap](#runtime-hot-swap-model-swap)), and agentd can **discover**
> what an endpoint serves (see [Model discovery](#model-discovery)). The
> single-endpoint behaviour described first is exactly the one-element-list case.

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
agentd: intelligence endpoint must be https://host/… (http:// is loopback-only) (got: ftp://x)
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
  --mcp incidents='mcp-server-http --base https://intra/incidents'
```

TLS is rustls with the `ring` provider and `webpki-roots` — no C toolchain, no
cmake. SNI is the parsed host.

### `http://127.0.0.1` — a same-host dev gateway (loopback only)

```bash
agentd \
  --instruction-file ./task.md \
  --intelligence http://127.0.0.1:4000/v1/chat/completions \
  --model gpt-4o \
  --mcp fs='mcp-server-http --base https://intra/fs'
```

A loopback gateway (a sidecar in the same pod, a dev proxy) terminates TLS and
provider auth; agentd talks plaintext HTTP to it over loopback only. Any other
`http://` host is a startup error.

---

## The wire: OpenAI-compatible by default, Anthropic in-binary

agentd ships **exactly two** in-binary adapters. The bias is deliberate: fewer
adapters, thinner binary, push provider quirks to a gateway.

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

The second in-binary adapter. Headers are `x-api-key: <token>` +
`anthropic-version: 2023-06-01`. The system prompt is extracted out-of-band into
the top-level `system` field; tools map to Anthropic's
`{name, description, input_schema}` (same `input_schema` key — passed through
verbatim). Assistant tool calls serialize as `tool_use` content blocks; tool
results as `tool_result` blocks. `stop_reason` normalises into the same finish
reason, usage from `usage.{input_tokens,output_tokens}`.

### Anything else → push it to a gateway

Gemini, Bedrock, Cohere, and other providers are **not** in the binary. Run a
gateway that exposes an OpenAI-compatible `/chat/completions`, point
`AGENT_INTELLIGENCE` at it (`unix:` or `https://`), and the canonical adapter
handles the rest. This keeps the binary thin and the provider matrix out of
agentd's release cadence.

> **Roadmap.** Selecting the Anthropic adapter (vs. the default
> openai-compatible) and the legacy framed-`complete` gateway wire are specified
> in RFC 0006 (`AGENT_INTELLIGENCE_DIALECT`, `AGENT_INTELLIGENCE_WIRE`) but are
> **not yet on the CLI surface** in `config.rs`. Until they land, the binary
> drives the canonical openai-compatible HTTP wire. Track in
> [`design/PLAN.md`](design/PLAN.md).

---

## Native tool-calling vs. the JSON-action fallback

Native tool-calling is **primary**. When a gateway or model lacks it, the loop
falls back to a JSON-action protocol: it omits the `tools` field, renders the
tool catalogue into the system prompt, and asks the model to answer with a
single JSON object:

```jsonc
{"action": "tool", "tool": "fs.read", "args": {"path": "/etc/hosts"}}
// or
{"action": "final", "result": "…"}
```

The response text is run through a balanced-brace, prose-tolerant extractor (so
code fences and surrounding chatter don't break it). An `action:"tool"` is
synthesized into a normal tool call and routed identically to a native one; an
`action:"final"` ends the turn; anything unparseable becomes a recoverable,
step-consuming observation fed back to the model — never a hard abort.

This is a **demoted fallback**: native is always tried first. The toolmode knob
(`AGENT_INTELLIGENCE_TOOLMODE = native | json | auto`) is specified in RFC 0006
but is **roadmap** — not yet on the `config.rs` surface. Prefer an
openai-compatible endpoint with native tool-calling for v1.

---

## Credentials

The credential is resolved **per endpoint**, set via env or flag, and **never
logged**:

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

- **Env or flag only.** The credential is **never** read from the config file (the
  config file may carry the *endpoint list* and *model*, but never a secret),
  never persisted, never put in the transcript fed back to the model.
- **Redacted everywhere.** The `Config` `Debug` impl prints the token as `***`;
  the secret-header allowlist keeps `authorization` / `x-api-key` out of the
  JSON-lines logs and any span; `agent://intelligence` shows transport + index
  only. There is a test asserting the raw value never appears.
- **Optional for keyless endpoints.** A local vLLM/Ollama behind a `unix:` socket
  needs no token at all.
- **File rotation.** A named-but-unset per-endpoint token *file* is caught at
  startup (exit 2) so a failover never discovers an unreadable secret. The
  `…_FILE` variants are read through the secret-file reader, the rotation-friendly
  path for k8s Secret mounts / Vault Agent sidecars.

Example of the redaction (the token is set but never echoed):

```jsonc
// proc.start — note: no token field exists anywhere in the log stream
{"ts":"2026-06-25T12:00:00Z","level":"info","event":"proc.start",
 "version":"1.0.0","mode":"once","mcp_servers":1,"subscribe":0}
```

---

## How the call behaves

- **One connect per call**, `Connection: close` — no keep-alive, no pooling. The
  request rate is single-digit per second per subagent, so this is free.
- **Synchronous and blocking** for the subagent's turn — the agentic loop is
  single-threaded per subagent. The supervisor never blocks on the LLM call.
- **Non-streaming** (`stream:false`) in v1. A timeout surfaces as a transient
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

### `agent://intelligence` — the live endpoint health view

When serving its self-MCP (`--serve-mcp`), agentd exposes a **management-only**,
subscribable resource: **`agent://intelligence`**. It is the ordered endpoint
list with **transport + index only — never the URL, host, cid, or credential**
(RFC 0012 §3.7):

```jsonc
{
  "active": 0,
  "all_down": false,
  "model": "claude-opus-4",
  "swap_policy": "finish-on-old",
  "discovery": true,
  "models": ["claude-opus-4", "claude-haiku-4"],
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

The `addr` is the bounded structural address (`host[:port]` with the path dropped)
— enough to tell endpoints apart, never a secret. The resource fires
`notifications/resources/updated` on a breaker/active/all-down transition, and on
a hot-swap (below), so a subscriber re-reads it. (`swap_policy`, `discovery`, and
`models` are covered in the next two sections.)

---

## Runtime hot-swap (`--model-swap`)

The intelligence endpoint list and the model are **reloadable** — a hot reload
(SIGHUP, or a watched config-file change; see
[Configuration](configuration.md)) that changes `intelligence` / `model` /
`model_swap` swaps the model **live**, with no restart:

- **New spawns** use the new config immediately (the spawn template is
  repointed).
- **In-flight runs** — warm `--continue` sessions and served runs — receive a
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
(`intelligence` / `model` / `model_swap`); the **credential stays env/`_FILE`-only**
and is never read from the config file.

---

## Model discovery

agentd can learn what an OpenAI-compatible endpoint serves via a tiny,
best-effort probe: one hand-rolled `GET /v1/models` over the **same** transport
and bearer auth the chat call uses (no new client, no streaming, zero new deps).
It is **lazy, cached, and silent-degrade**:

- It runs **only** when the served `agent://intelligence` (or the live
  `agent://capabilities`) surface is actually read — never on the hot path, and
  **never at startup** (the one-shot `agentd --capabilities` probe stays
  network-free). The result is cached supervisor-side with a short TTL.
- **Any** failure — a `404` (discovery unsupported), a connection failure, a
  non-JSON body — yields no models and never flips `discovery` to true. It is
  never fatal and never a failover-class error: the configured model is always
  dialed regardless.
- The `anthropic` dialect has **no list endpoint**, so it contributes nothing —
  just the configured model.

It surfaces two fields on `agent://intelligence` and the capabilities manifest's
`intelligence` block:

- `discovery` — `true` if at least one endpoint answered `/v1/models`.
- `models` — the **union of discovered ids across endpoints plus the configured
  `model`**, de-duplicated and order-stable. It may be **empty** (nothing
  discovered and no model configured), or just the configured model (discovery
  unsupported).

agentctl uses this for model-aware placement. Treat it as a hint: `discovery`
may be `false` and `models` may carry only the configured model — that is the
expected, fully-working state for an endpoint without a list API.

---

## The real flag/env surface

These are the flags and env vars that exist **today** in `config.rs`. (Env name
in parentheses; the flag wins over env, which wins over the default.)

| Flag | Env | Meaning |
|---|---|---|
| `--intelligence <URI[,URI…]>` | `AGENT_INTELLIGENCE` | the endpoint **list**: comma-separated `https://` (or a loopback `http://`), order = failover priority (required) |
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | endpoint-1 bearer / `x-api-key` value (never logged) |
| `--intelligence-token-file <PATH>` | `AGENT_INTELLIGENCE_TOKEN_FILE` | read endpoint-1's token from a mounted file (rotation) |
| *(per-endpoint, env-only)* | `AGENT_INTELLIGENCE_TOKEN_<N>` / `…_<N>_FILE` | endpoint *N*'s token / token-file (1-indexed, N ≥ 2) |
| `--model <NAME>` | `AGENT_MODEL` | model id sent in the request body (reloadable) |
| `--model-swap <POLICY>` | `AGENT_MODEL_SWAP` | in-flight model-swap policy: `finish-on-old` (default) \| `restart-turn` |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | token budget for the run (default 200000) |
| `--deadline <dur>` | `AGENT_DEADLINE` | wall-clock deadline, e.g. `600s`, `5m` (default 600s) |

Durations accept `ms`, `s`, `m`, `h`, or a bare integer (seconds). The
`intelligence` endpoint list, `model`, and `model_swap` are also settable from the
config file and are **reloadable** (see [Configuration](configuration.md) and the
[hot-swap](#runtime-hot-swap-model-swap) section); the credential is env/`_FILE`
only.

> The dialect/toolmode/legacy-wire selectors from RFC 0006
> (`AGENT_INTELLIGENCE_DIALECT`, `AGENT_INTELLIGENCE_TOOLMODE`,
> `AGENT_INTELLIGENCE_WIRE`) are **not yet** in `config.rs`; do not rely on
> them until they appear in `agentd --help`. They are tracked in
> [`design/PLAN.md`](design/PLAN.md).

---

## See also

- [Configuration reference](configuration.md) (the full flag/env surface + the reloadable config file)
- [Observability](observability.md) (the `intel.*` events, `agent_intel_*` metrics, the breaker signals)
- [Horizontal scaling](scaling.md) (where `intelligence.warm`/`healthy` feeds `agent://capacity`)
- [RFC 0006 — Intelligence transport & wire](../rfcs/0006-intelligence-transport-and-wire.md) (this channel, in full)
- [RFC 0018 — Intelligence transport resilience](../rfcs/0018-intelligence-transport-resilience.md) (failover, the breaker, swap, discovery)
- [RFC 0004 — MCP client subset & codec](../rfcs/0004-mcp-client-subset-and-codec.md) (where tools come from)
- [RFC 0007 — Agentic loop & terminal status](../rfcs/0007-agentic-loop-and-terminal-status.md) (who calls `complete`)
- [RFC 0012 — Security posture](../rfcs/0012-security-posture.md) (SSRF, header injection, secret handling)
- [`design/PLAN.md`](design/PLAN.md) (build milestones M1–M3, current status)
