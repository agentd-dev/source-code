# Intelligence: the LLM wire

agentd reaches a model over **one logical wire** per connection — the
top-level one described here, plus any named providers (see
[Providers, models and roles](#providers-models-and-roles)). The agentic ReAct loop —
which runs only inside a child process, a turn worker or a subagent — sends it
messages plus a scoped tool catalogue and gets back text *and* structured tool
calls. That wire is named in
`AGENTD_INTELLIGENCE` (or `--intelligence`) and authenticated with
`AGENTD_INTELLIGENCE_TOKEN` (or `--intelligence-token`). For resilience the wire
can list **several endpoints** (failover priority by order), each with its own
credential, and the list + model are **hot-swappable without a restart** — but it
stays one model-facing channel per connection.

This is the **intelligence wire** — the model-facing channel. It is
**categorically not MCP.** Tools come from MCP servers; this channel only carries
the LLM request/response. Do not conflate the two.

> **One wire, many endpoints.** `AGENTD_INTELLIGENCE` takes an **ordered list**
> of endpoints for failover (see
> [Resilience](#resilience-multi-endpoint-failover--the-circuit-breaker)), and
> that list and the model are **hot-swappable** without a restart (see
> [Runtime hot-swap](#runtime-hot-swap---model-swap)). The single-endpoint
> behaviour described first is exactly the one-element-list case.

## The one URI: HTTPS

Intelligence is reached over **HTTPS** — a hand-rolled HTTP/1.1 client over a
`Read + Write` byte stream, so the intelligence path needs no `url`/ICU
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
export AGENTD_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd \
  --instruction 'summarize the open incidents' \
  --intelligence https://api.openai.com/v1/chat/completions \
  --model gpt-4o \
  --mcp incidents=https://intra/incidents/mcp
```

TLS is rustls with the `ring` provider and `webpki-roots`; SNI is the parsed
host.

### `http://127.0.0.1` — a same-host dev gateway (loopback only)

```bash
agentd \
  --instruction ./task.md \
  --intelligence http://127.0.0.1:4000/v1/chat/completions \
  --model gpt-4o \
  --mcp fs=https://intra/fs/mcp
```

A loopback gateway (a sidecar in the same pod, a dev proxy) terminates TLS and
provider auth; agentd talks plaintext HTTP to it over loopback only. Any other
`http://` host is a startup error.

---

## The wire: OpenAI-compatible by default

agentd ships **three** in-binary chat adapters, selected with
`intelligence.dialect` (`--intelligence-dialect`, `AGENTD_INTELLIGENCE_DIALECT`):
`openai` (the default), `anthropic`, and `bedrock` — and a fourth, `decisions`,
for a decision model's endpoint, on a provider only ([Kinds of
model](#kinds-of-model-chat-and-decision)). The bias is deliberate: few
adapters, thin binary, push provider quirks to a gateway.

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
MCP `tools/list` `inputSchema` passed through verbatim — discovery belongs to the
MCP client, and this wire never rewrites a schema. The adapter reads back
`choices[0].message.content`,
`choices[0].message.tool_calls[]` (parsing each `function.arguments` string into
a JSON object), `finish_reason`, and `usage.{prompt_tokens,completion_tokens}`.

### `anthropic` `POST /v1/messages`

Selected with `intelligence.dialect: anthropic`. Headers are `x-api-key: <token>` +
`anthropic-version: 2023-06-01`. The system prompt is extracted out-of-band into
the top-level `system` field; tools map to Anthropic's
`{name, description, input_schema}` (same `input_schema` key — passed through
verbatim). Assistant tool calls serialize as `tool_use` content blocks; tool
results as `tool_result` blocks. `stop_reason` normalises into the same finish
reason, usage from `usage.{input_tokens,output_tokens}`. No `temperature` is
sent: current Claude models refuse one, and the models that take one run at
their own default.

### `bedrock` — the Amazon Bedrock Converse wire

The one dialect that requires an auth block: `intelligence.auth.kind` must be
`aws`, because every dial is SigV4-signed rather than bearer-authenticated
(validation rejects the pair otherwise). The model id rides the **URL path** —
`/model/{modelId}/converse` — not the request body, so `model:` is the Bedrock
model id or an inference-profile id/ARN. Tool-calling and system prompts map
onto the Converse shapes. A Claude model (`anthropic.…`, optionally
region-prefixed) is sent no `temperature`, as on the `anthropic` dialect. See
[Authentication](authentication.md#enterprise-llm-providers-azure-google-aws)
for a complete block.

### Anything else → push it to a gateway

Gemini, Cohere, and other providers are **not** in the binary. Run a gateway
that exposes an OpenAI-compatible `/chat/completions`, point `AGENTD_INTELLIGENCE`
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
export AGENTD_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd …
# or read from a mounted file (rotation-friendly)
export AGENTD_INTELLIGENCE_TOKEN_FILE=/var/run/secrets/llm/token
agentd …
```

### Per-endpoint credentials

With a multi-endpoint list, each element resolves its **own** credential by
position (1-indexed):

| Endpoint | Inline env | File env |
|---|---|---|
| 1 (primary) | `AGENTD_INTELLIGENCE_TOKEN` (or `--intelligence-token`) | `AGENTD_INTELLIGENCE_TOKEN_FILE` (or `--intelligence-token-file`) |
| 2 | `AGENTD_INTELLIGENCE_TOKEN_2` | `AGENTD_INTELLIGENCE_TOKEN_2_FILE` |
| *N* | `AGENTD_INTELLIGENCE_TOKEN_<N>` | `AGENTD_INTELLIGENCE_TOKEN_<N>_FILE` |

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
 "version":"1.1.0","instance":"agentd",
 "config_files":["settings.yaml"]}
```

---

## How the call behaves

- **One connect per call**, `Connection: close` — no keep-alive, no pooling. The
  request rate is single-digit per second per subagent, so this is free.
- **Synchronous and blocking** for the subagent's turn — the agentic loop is
  single-threaded per subagent. The supervisor never blocks on the LLM call.
- **Non-streaming** (`stream:false`). A timeout surfaces as a transient
  transport error and is retried with bounded backoff. One call's ceiling is
  its connection's `timeout` (`intelligence.timeout`, or a provider's;
  default 120 s); the run's deadline is the real bound.
- **HTTP status taxonomy** — which status is retried and which is fatal:
  - `429` / `5xx` → bounded retry with backoff + jitter.
  - `401` / `403` → fatal auth → **exit 4**.
  - connection refused/reset → fatal intelligence-unreachable → **exit 4**.
  - a **named-but-unset** key is caught at startup → **exit 2** (validate first,
    don't burn a round-trip on a 401).

---

## Providers, models and roles

A model is a name for a wire model, on a connection. Each job the agent does
runs on the model its **role** names, so a cheap model can triage, a frontier
one can answer, and a second vendor can think — in one instance, with no
subagent process forked to change a string ([RFC 0046](../rfcs/0046-kinds-of-intelligence.md)).

```yaml
services:                       # under `egress: closed`, every connection is catalogued
  anthropic: {kind: intelligence, endpoint: "https://api.anthropic.com"}
  openai:    {kind: intelligence, endpoint: "https://api.openai.com/v1"}

intelligence:
  # The top-level connection.
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"

  # Further connections, by name — the same vocabulary, nothing more:
  # endpoints, dialect, token / token_file, headers, auth, timeout.
  providers:
    openai:
      endpoints: https://api.openai.com/v1/chat/completions
      token: "{{secret:OPENAI_API_KEY}}"

  models:
    smart: {model: claude-opus-5-5, window: 1000000, fallback: fast}
    fast:  {model: claude-haiku-4-5, window: 200000}
    other: {model: gpt-5.1, provider: openai, fallback: smart}

  roles:
    turn: smart          # the agent's turns, agent/extract/summarize nodes, subagents
    think: other         # the think tool and think nodes
    decide: fast         # classify, route and judge nodes
    preflight: fast      # triage of an inbound message
    compaction: fast     # summarising the transcript
    goal: fast           # the goal check
    approve: smart       # asks answered on the operator's behalf (approval: auto)

workflows:
  - name: triage
    steps:
      sort:   {kind: classify, input: "…", classes: [a, b]}   # roles.decide → fast
      answer: {kind: agent, model: other, instruction: "…"}   # a node's model: wins
```

**Models.** `intelligence.models.<name>` is `model` (the wire name, required),
`provider` (a name under `providers`; absent, the top-level connection),
`window` and `fallback`. A reference — in a role, on a node, in
`budget.degrade.model`, on a subagent — is a declared name or, when no model
of that name is declared, a literal wire model on the top-level connection.
Every reference resolves to the wire model at the edge, so a model's *name*
never reaches a provider, and every one is checked at startup: an unknown
model on a step or a role, or an unknown provider on a model, is exit 2
rather than a run quietly asking for a model called `smal`.

**Roles.** Every role defaults to `turn`, and `turn` defaults to
`intelligence.model` — they are one setting said two ways, and setting both is
exit 2 ("say it once"); `--model` / `MODEL` keep working. No role inherits
from another, so binding `decide` moves the nodes that decide and nothing
else. A node with no `model:` runs on its kind's role: `agent`, `extract` and
`summarize` on `turn`, `think` on `think`, `classify`, `route` and `judge` on
`decide`. The goal check runs on `goal` and the `approval: auto` judge on
`approve`.

A role is a model's name, or `{model, confidence}` for a role that decides —
`decide`, `preflight`, `goal`, `approve` — where `confidence` is the least
sureness its decisions are acted on with; `turn`, `think` and `compaction`
write and take no threshold. What a deciding role does on each kind:

| Role | on a chat model | on a decision model |
|---|---|---|
| `preflight` | the structured triage it always was | one decision: `intent` (choice), `needs_plan` (noul), `risk` (choice), one noul per catalogued skill (up to 13, the decision's 16 questions; `preflight.skills_capped` says when more are left out); a plan it calls for is the turn's to write, and the turn is told |
| `goal` | the judge it always was | `achieved` and `stuck` as two statements about the status, each true at 0.5 and, with a `confidence`, only when that sure |
| `approve` | the judge it always was | a closed ask — its answer an `enum` or a boolean, at the top of its schema or as the schema's one property — as a choice; an open one to the `escalate` model when that model writes, else `UNDECIDED`; bound only with a `confidence`, which an escalated answer is held to as well |
| `decide` | emulated decisions | native decisions; its `confidence` is the threshold of every deciding node and `decide` call that sets none |

An answer under a role's `confidence` is asked once more on the model's
`escalate` model when it names one; otherwise it reads as no answer:
preflight treats the message as a `task` (as a missing verdict always did),
the goal is neither achieved nor stuck, an approval stays `UNDECIDED`.
Whatever the model, the `approve` judge never answers an ask whose run carries
outside input ([experience.md](experience.md#approvals-and-steering)).

**Providers.** Each provider gets its own client, endpoint failover, breakers
and credential refresh. Every endpoint of a provider dials with the
provider's own `token` (or `auth:`); the positional `AGENTD_INTELLIGENCE_TOKEN`
/ `…_<N>` environment overrides belong to the top-level connection and never
reach a provider. An `oauth2` provider is logged into on its own:
`agentd --login intelligence:openai` (the top-level connection stays
`intelligence`). A provider is a destination, so under `security.egress:
closed` each of its endpoints must match a `kind: intelligence` service entry,
as `intelligence.endpoints` must; a provider's `token` in a file is a
`{{secret:…}}` reference, as the top-level one is.

**Fallback.** When a model is unavailable — every endpoint of its connection
failed, or every breaker is open — the call is asked of its `fallback`, with
the fallback's wire model on the fallback's own provider, and so down the
chain; `turn.model_fallback` says which model served (once per change, not
every round). The next call tries the model first again: with its breakers
open that costs no dial, and a breaker that has cooled half-opens, so the
model is back the moment it answers. An auth failure is not unavailability
and is never handed down the chain. Readiness follows the chain: `/readyz`
reports the intelligence down only when no model in it answered. A `fallback` cycle is refused at
startup — a degradation ladder that loops is a hang under exactly the
conditions it exists to survive.

### Kinds of model: chat, reasoning and decision

A model is `kind: chat` (the default), `kind: reasoning` or `kind: decision`.
A chat model generates — text, tool calls, JSON.

A **reasoning model** generates too, after thinking — wherever a chat model
can serve, it can. Its defaults are `reasoning: {effort: low|medium|high,
budget: <tokens>}` (effort `medium` when unset); `effort:` on a `think` or
`agent` node, or on a `think` tool call, overrides the effort for that call,
and is refused where the model does not reason. Each dialect gets its own
form:

| Dialect | Sent | Also |
|---|---|---|
| `openai` | `reasoning_effort` | `max_completion_tokens`; no `temperature` |
| `anthropic` | `thinking: {type: adaptive}` and `output_config: {effort}`; with a `budget`, `thinking: {type: enabled, budget_tokens}` instead | no `temperature`; while the same turn is still calling tools, the model's own message — its signed `thinking` blocks among the rest, in order — goes back to it unchanged |
| `bedrock` | a Claude model (`anthropic.…`, optionally region-prefixed): `additionalModelRequestFields` as for `anthropic`; an OpenAI open-weight model (`openai.…`): `additionalModelRequestFields.reasoning_effort` | no `temperature`; `reasoningContent` blocks go back within the turn as for `anthropic` |

Every dialect raises the output cap above the answer's by the thinking's
room — `budget`, else 2048 / 8192 / 24576 tokens by effort — and budget
admission counts it. Current Claude models think adaptively and refuse a
`budget`; set one only for a model that takes the budget form. On Bedrock the
fields depend on the model's family, so a reasoning model whose id names
neither (an ARN, an application inference profile) is refused at load.

The reasoning a model returns is carried only within the turn that made it:
never in the transcript the agent keeps, never on the feed, never logged,
never across a compaction. It goes back only to the model that wrote it —
each message that thought records its dialect and model — so a fallback or a
degrade model is sent the text and tool calls alone. A fallback is asked as
the model it is — a chat model without the head model's reasoning, a
reasoning model with its own.

A **decision model** answers typed
questions about a state and writes nothing: Jev-style models, OpenRouter's
decisions resource, Strands Decider, OpenAI's Decisions API. It is reached on a
provider with `dialect: decisions`, whose endpoint is the decisions resource's
**full URL**, posted as is:

```yaml
intelligence:
  providers:
    openrouter:
      endpoints: https://openrouter.ai/api/alpha/decisions
      dialect: decisions
      token: "{{secret:OPENROUTER_API_KEY}}"
  models:
    smart: {model: claude-opus-5-5}
    jev:   {kind: decision, provider: openrouter, model: typesafe/jev-1.13, escalate: smart}
  roles: {turn: smart, decide: jev}
```

The body is the shape those vendors share — `{model, state, questions}` in,
`{answers, usage}` out — and every answer is checked: a question left
unanswered, an option nobody offered, a probability out of range is an error,
never a decision acted on with a hole in it. A vendor's own limit (state size,
questions, options) is its 4xx, surfaced with its reason.

A decision model serves only what decides: `roles.decide` and the `decide`,
`classify`, `route` and `judge` nodes ([node-registry.md](node-registry.md#decisions)).
Every other role and node writes and refuses one at load, as do
`subagents.defaults.model`, a template's `model`, and `budget.degrade.model`.
A chat model may answer a decision too — one structured call with every
question in it, its confidence a self-report (`calibrated: false`) — so a
decision model's `fallback` and `escalate` may be chat models; a chat model's
`fallback` may not be a decision model. A decision's answers are logged
(`intel.decision`: question ids, answers, confidence, `escalated`); its state
is content and is not.

**What a served document may set.** An instruction document may declare
models and bind every role but `approve` — *which* model, never where it
lives: a model it declares cannot name a `provider`, and `providers` and
`roles.approve` are the operator's ([`directives.md`](directives.md)). The
`approve` judge resolves in the operator's own section — the operator's
`roles.approve`, else the operator's turn model, with the operator's models
and fallbacks — so a document cannot re-point it by declaring a model under
that name or binding `turn`; only when the operator names no model at all does
it run on the agent's. And a turn model the operator names, in either
spelling, wins over a document's (which is dropped with a warning).

A model's declared `window` also replaces the compaction threshold's guess
from the model *name*, which is a substring match and simply wrong for any
provider whose naming does not happen to match. Trifecta tags are not part of
any of this — `tags` is `kind: mcp` vocabulary, and writing it on a `kind:
intelligence` entry is exit 2.

`turn.model` puts the resolved model on the log line, because "how much did
that cost, and on what" now has a per-turn answer.

<a id="cost-recorded-not-spent"></a>
The "on what" is the half agentd answers, and only that half. There is **no
cost accounting**: nothing multiplies a rate by a token count, no event carries
a money figure, and every limit under `intelligence.budget` is denominated in
tokens and requests rather than currency. The configuration has no price key,
because a field that accepts a number and does nothing with it reads like a
feature.

If you want money, take it from the token counts: `turn.model` puts the
resolved model on the log line and the usage events carry the token totals, so
a rate card applied downstream — in your metrics pipeline, against
`agent_tokens_total` by model — gives you a figure agentd can stand behind.
Real in-process cost accounting is a feature worth designing rather than half
declaring: which tokens are cached, how a tier rollup composes, what a budget
denominated in currency does when a provider changes its prices mid-run.

## Resilience: multi-endpoint failover & the circuit breaker

`AGENTD_INTELLIGENCE` (or `--intelligence`) accepts an **ordered,
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
credential — a health snapshot is read by operators and shipped in logs, so it
carries only the bounded structural `transport` + `addr` and the live counters:

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

Everything under `intelligence` is **reloadable** — a hot reload (SIGHUP, or a
watched config-file change; see [Configuration](configuration.md)) swaps it
**live**, with no restart:

- **New work** uses the new settings immediately: which model each job runs on
  is resolved at each call, and a connection that moved — its endpoints,
  credential, headers, login — is rebuilt at the reload's commit.
- **Running subagents** are each handed their model resolved again — its
  connection, credential and fallbacks — on a control frame (`intel.swap.sent`)
  and apply it at their **next turn boundary**. An in-flight model call is
  **never torn**, and the conversation transcript is continuous (no context
  reset). A turn worker is one turn long and is not swapped: the next one is
  built from the new settings.

A swap that leaves the model unchanged is always invisible — the run rebuilds
its client with **fresh breaker state** (so no stale breaker carries to a new
endpoint) and continues. The endpoint URL and credential travel on the control
frame like the spawn payload and are **never logged**.

`--model-swap` (env `AGENTD_MODEL_SWAP`) controls only what happens when a reload
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
config-watch`), and a ConfigMap update reloads the connections, models and
roles live. The credentials in that file are `{{secret:…}}` references, so
rolling the ConfigMap never moves a literal secret.

---

## The real flag/env surface

These are the flags and env vars the binary accepts. (Env name in parentheses;
the flag wins over env, which wins over the default.)

| Flag | Env | Meaning |
|---|---|---|
| `--intelligence <URI[,URI…]>` | `AGENTD_INTELLIGENCE` | the endpoint **list**: comma-separated `https://` (or a loopback `http://`), order = failover priority (required) |
| `--intelligence-token <T>` | `AGENTD_INTELLIGENCE_TOKEN` | endpoint-1 bearer / `x-api-key` value (never logged) |
| `--intelligence-token-file <PATH>` | `AGENTD_INTELLIGENCE_TOKEN_FILE` | read endpoint-1's token from a mounted file (rotation) |
| *(per-endpoint, env-only)* | `AGENTD_INTELLIGENCE_TOKEN_<N>` / `…_<N>_FILE` | endpoint *N*'s token / token-file (1-indexed, N ≥ 2) |
| `--model <NAME>` | `AGENTD_MODEL` | model id sent in the request body (reloadable) |
| `--model-swap <POLICY>` | `AGENTD_MODEL_SWAP` | in-flight model-swap policy: `finish-on-old` (default) \| `restart-turn` |
| `--intelligence-dialect <D>` | `AGENTD_INTELLIGENCE_DIALECT` | wire dialect: `openai` (default) \| `anthropic` \| `bedrock` |
| `--max-tokens <N>` | `AGENTD_MAX_TOKENS` | token budget for the run (default 2000000) |
| `--deadline <dur>` | `AGENTD_DEADLINE` | wall-clock deadline, e.g. `600s`, `5m` (default 3600s) |

Every one of these is a configuration document path as well —
`intelligence.endpoints`, `intelligence.token`, `intelligence.model`,
`intelligence.swap_policy`, `intelligence.dialect`, `limits.run.tokens`,
`limits.run.deadline` — settable from a file, from `AGENTD_<PATH>`, or from
`--<path>`; the flags above are the short spellings. Durations accept `ms`, `s`,
`m`, `h`, or a bare integer (seconds). The endpoint list and `model` are
**reloadable** (see [Configuration](configuration.md) and the
[hot-swap](#runtime-hot-swap---model-swap) section); a token in a file must be a
`{{secret:…}}` reference.

---

## See also

- [Configuration reference](configuration.md) (the full flag/env surface + the reloadable config file)
- [Authentication](authentication.md) (the `auth:` block — OAuth, AWS SigV4, SPIFFE — on this endpoint)
- [Observability](observability.md) (the `intel.*` events, `agent_intel_*` metrics, the breaker signals)
- [Deployment](deployment.md) and [Scaling](scaling.md) (multiple daemon replicas coordinate through the durable store)
- [MCP](mcp.md) (where the tools on this wire come from)
- [The agent loop](agent-loop.md) (who calls `complete`, and how a turn ends)
- [Security](security.md) (SSRF, header injection, secret handling)
