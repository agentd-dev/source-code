# Intelligence: the single LLM endpoint

agentd reaches **exactly one** intelligence (LLM) endpoint. The agentic ReAct
loop — which runs only inside a subagent process — sends it messages plus a
scoped tool catalogue and gets back text *and* structured tool calls. That
endpoint is named by a **single URI** in `AGENTD_INTELLIGENCE` (or
`--intelligence`), and a **single credential** in `AGENTD_INTELLIGENCE_TOKEN`
(or `--intelligence-token`). That is the whole surface.

This is the **intelligence wire** — the model-facing channel. It is
**categorically not MCP.** Tools come from MCP servers (RFC 0004); this channel
only carries the LLM request/response. Do not conflate the two.

> **Status.** The runtime is implemented: config validation, the agentic ReAct
> loop, the transport/adapter machinery, and the supervisor + subagent process
> tree all ship and are tested. The examples below describe live behavior per
> [RFC 0006](../rfcs/0006-intelligence-transport-and-wire.md).

---

## The one URI, three transports

The scheme of `AGENTD_INTELLIGENCE` selects the transport. All three drive the
*same* hand-rolled HTTP/1.1 client over a `Read + Write` byte stream — agentd
ships no async runtime and no `url`/ICU stack.

| URI form | Transport | Use case | Build |
|---|---|---|---|
| `unix:/path/to.sock` | Unix-domain socket | sidecar **gateway** on the same host/pod | core (always on) |
| `https://host[:port]/path` | TCP + TLS | direct provider or a remote gateway | feature `tls` |
| `vsock:<cid>:<port>` | AF_VSOCK | LLM service on the **host** from inside an enclave/microVM | feature `vsock` (roadmap, M4) |

The URI is validated **at startup**, before any side effect. A scheme that
isn't `unix:`, `https://`, or `vsock:` exits `2` in milliseconds:

```
$ agentd --instruction 'hi' --intelligence ftp://x
agentd: intelligence endpoint must be unix:/path, https://host/…, or vsock:cid:port (got: ftp://x)
$ echo $?
2
```

Plain `http://` is accepted only for the local/sidecar dev case and is gated by
the SSRF policy in RFC 0012 (loopback/RFC-1918/link-local blocked by default;
the client warns). Use `unix:` or `https://` in production.

### `unix:` — sidecar gateway (canonical for clusters)

A gateway sidecar (LiteLLM, a local vLLM, your own proxy) terminates TLS and
provider auth; agentd talks plaintext HTTP over the socket. No TLS feature, no
key in the agent process.

```bash
agentd \
  --instruction-file ./task.md \
  --intelligence unix:/run/intel.sock \
  --model gpt-4o \
  --mcp fs='mcp-server-fs --root /data'
```

### `https://` — direct or remote gateway (feature `tls`)

```bash
export AGENTD_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd \
  --instruction 'summarize the open incidents' \
  --intelligence https://api.openai.com/v1/chat/completions \
  --model gpt-4o \
  --mcp incidents='mcp-server-http --base https://intra/incidents'
```

TLS is rustls with the `ring` provider and `webpki-roots` — no C toolchain, no
cmake. SNI is the parsed host.

### `vsock:` — enclave / microVM (feature `vsock`, roadmap M4)

```bash
agentd \
  --instruction-file /task.md \
  --intelligence vsock:2:8080 \
  --model claude-opus-4
```

`vsock:<cid>:<port>` connects to a host-side LLM service from inside an enclave.
The `vsock` transport lands in M4; the URI is parsed and validated today, but
the dial path is roadmap.

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
`AGENTD_INTELLIGENCE` at it (`unix:` or `https://`), and the canonical adapter
handles the rest. This keeps the binary thin and the provider matrix out of
agentd's release cadence.

> **Roadmap.** Selecting the Anthropic adapter (vs. the default
> openai-compatible) and the legacy framed-`complete` gateway wire are specified
> in RFC 0006 (`AGENTD_INTELLIGENCE_DIALECT`, `AGENTD_INTELLIGENCE_WIRE`) but are
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
(`AGENTD_INTELLIGENCE_TOOLMODE = native | json | auto`) is specified in RFC 0006
but is **roadmap** — not yet on the `config.rs` surface. Prefer an
openai-compatible endpoint with native tool-calling for v1.

---

## Credentials

One credential, two ways to set it, **never logged**:

```bash
# flag
agentd … --intelligence-token "$OPENAI_API_KEY"
# or env (preferred for 12-factor / secret mounts)
export AGENTD_INTELLIGENCE_TOKEN="$OPENAI_API_KEY"
agentd …
```

Rules:

- **Env or flag only.** The token is **never** read from a config file, never
  persisted, never put in the transcript fed back to the model, never in a
  checkpoint.
- **Redacted everywhere.** The `Config` `Debug` impl prints the token as `***`;
  the secret-header allowlist keeps `authorization` / `x-api-key` out of the
  JSON-lines logs and any span. There is a test asserting the raw value never
  appears in debug output.
- **Optional for keyless endpoints.** A local vLLM/Ollama behind a `unix:` socket
  needs no token at all.
- **Live file rotation (roadmap).** RFC 0006 resolves the key per request, so a
  file-backed rotating secret (k8s Secret mount, Vault Agent sidecar) is picked
  up on the next call with no reload. v1's posture is restart-to-reload; the
  per-request file source is the rotation path.

Example of the redaction (the token is set but never echoed):

```jsonc
// proc.start — note: no token field exists anywhere in the log stream
{"ts":"2026-06-25T12:00:00Z","level":"info","event":"proc.start",
 "version":"1.3.0","mode":"once","mcp_servers":1,"subscribe":0}
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

## The real flag/env surface

These are the flags and env vars that exist **today** in `config.rs`. (Env name
in parentheses; the flag wins over env, which wins over the default.)

| Flag | Env | Meaning |
|---|---|---|
| `--intelligence <URI>` | `AGENTD_INTELLIGENCE` | the one endpoint: `unix:` \| `https://` \| `vsock:` (required) |
| `--intelligence-token <T>` | `AGENTD_INTELLIGENCE_TOKEN` | bearer / `x-api-key` value (never logged) |
| `--model <NAME>` | `AGENTD_MODEL` | model id sent in the request body |
| `--max-tokens <N>` | `AGENTD_MAX_TOKENS` | token budget for the run (default 200000) |
| `--deadline <dur>` | `AGENTD_DEADLINE` | wall-clock deadline, e.g. `600s`, `5m` (default 600s) |

Durations accept `ms`, `s`, `m`, `h`, or a bare integer (seconds).

> The dialect/toolmode/legacy-wire selectors from RFC 0006
> (`AGENTD_INTELLIGENCE_DIALECT`, `AGENTD_INTELLIGENCE_TOOLMODE`,
> `AGENTD_INTELLIGENCE_WIRE`) are **not yet** in `config.rs`; do not rely on
> them until they appear in `agentd --help`. They are tracked in
> [`design/PLAN.md`](design/PLAN.md).

---

## See also

- [RFC 0006 — Intelligence transport & wire](../rfcs/0006-intelligence-transport-and-wire.md) (this channel, in full)
- [RFC 0004 — MCP client subset & codec](../rfcs/0004-mcp-client-subset-and-codec.md) (where tools come from)
- [RFC 0007 — Agentic loop & terminal status](../rfcs/0007-agentic-loop-and-terminal-status.md) (who calls `complete`)
- [RFC 0012 — Security posture](../rfcs/0012-security-posture.md) (SSRF, header injection, secret handling)
- [`design/PLAN.md`](design/PLAN.md) (build milestones M1–M3, current status)
