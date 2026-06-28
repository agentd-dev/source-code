# RFC 0006: Intelligence transport & wire format

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

> **A2A alignment (RFC 0020).** Note the transport symmetry RFC 0020 exploits:
> here agentd dials intelligence **out** over vsock behind a TLS-terminating
> sidecar; RFC 0020 serves A2A **in** over vsock behind an HTTP-terminating
> gateway. Same posture — agentd network-isolated, the gateway/sidecar owns
> TLS/auth/HTTP — inverted direction. The vsock transport this RFC defines for
> the client is mirrored on the serving side (RFC 0015 §3, RFC 0020).

---

## Problem / Context

The agentic ReAct loop (RFC 0007), running inside a subagent process, must reach
exactly **one** LLM endpoint and get back text *and* structured tool calls. That
endpoint is named by a single URI in `AGENT_INTELLIGENCE`, and it may sit
behind three different transports depending on the deployment: a unix-socket
gateway sidecar, a direct `https://` endpoint, or a `vsock` channel to a host
LLM service from inside an enclave/microVM. This is the **intelligence wire** —
the model-facing channel — and it is **categorically not MCP** (assessment
§2.4: "These carry the LLM wire, **not** MCP — do not conflate"). MCP transport
and codec are RFC 0004's concern; this RFC owns only the LLM channel.

The retired bounded-workflow runtime already shipped most of the parts: a
length-framed unix transport, a hand-rolled HTTP/1.1 client, four provider
dialects, a system-prompt splitter, key-safe `Debug`, a build-time key probe,
balanced-brace JSON extraction, and the `secrets::resolve` front door. But the
retired `Request`/`Response`/`Usage` types model only plain `{role, content}`
messages — **no `tools` in the request, no `tool_calls` in the response**
(notes-mine §2.4, §2.5). The single largest piece of net-new work here is
widening the wire types for native tool-calling; everything else is salvage.

This RFC specifies: the three transports and how they share one
transport-agnostic client over `Read + Write`; the canonical OpenAI-compatible
`/chat/completions` wire with native tool-calling; the two in-binary adapters
(`openai-compatible`, `anthropic`); the JSON-action fallback for gateways
without native tool-calling; and credential handling.

Non-negotiable constraints from the assessment:

- No async runtime, no `url`/ICU stack, no C toolchain in the default build
  (§2.2). The HTTP client is hand-rolled over `Read + Write`; TLS and vsock are
  feature-gated.
- Exactly **two** adapters in-binary. The bias is *fewer adapters, thinner
  binary, push provider quirks to the gateway* (§2.4). Gemini and others live
  behind the gateway — the retired Gemini arm is **dropped** from the binary.
- Credentials env/flag only, through `secrets::resolve`, never logged, never
  persisted, never in transcripts; build-time key probe → fast-fail (§2.4,
  §2.11).

---

## Decision

1. **One transport-agnostic client.** A single `Transport: Read + Write + Send`
   trait. Three implementors selected by the `AGENT_INTELLIGENCE` URI scheme:
   `unix:/path` (core), `https://…` (feature `tls`), `vsock:<cid>:<port>`
   (feature `vsock`). All three drive the *same* hand-rolled HTTP/1.1 request
   writer and response reader (assessment §2.4). `unix:` and `vsock:` may carry
   either framed JSON-RPC `complete` (legacy gateway shape) or plaintext HTTP;
   the canonical default everywhere is HTTP `/chat/completions`.

2. **Canonical wire = OpenAI-compatible `POST /v1/chat/completions` with native
   tool-calling.** This is what the loop emits and parses by default. It covers
   vLLM / Ollama / LM-Studio / most hosted gateways and gives the model
   first-class `tools` + `tool_calls`.

3. **Exactly two in-binary adapters.** `openai-compatible` (the canonical
   default, also serves OpenAI proper) and `anthropic`. Both salvaged from the
   retired `intelligence/providers.rs`, widened for tool-calling. Selected by
   `AGENT_INTELLIGENCE_DIALECT` (default `openai`).

4. **JSON-action fallback.** When a gateway/model lacks native tool-calling
   (declared via `AGENT_INTELLIGENCE_TOOLMODE=json`, or auto-detected when a
   response carries no `tool_calls` but parses as a `{"action":…}` object), the
   loop falls back to the retired `{"action":"tool"|"final"}` shape parsed with
   `extract_json_object` (balanced-brace, prose-tolerant — lifted verbatim from
   `agent/loop_node.rs:343-373`). Native is primary; JSON-action is the demoted
   fallback.

5. **Credentials via `secrets::resolve` only.** Env + file sources kept;
   `command`/`oauth2` dropped (notes-mine §2.10). `Token`/key types never
   `Serialize`, `Debug` prints `***`. Build-time probe fails fast on a
   named-but-unset key.

This RFC owns `intel/` (`client.rs`, `openai.rs`, `anthropic.rs`),
`wire/intel.rs`, the LLM-facing half of `net/{http,tls,unixsock,vsock}.rs`, and
the credential surface in `sec/secrets.rs`. It is built in **M1** (openai +
unix/https) and extended in **M4** (vsock); the JSON-action fallback lands in
M1 alongside the loop.

---

## Mechanisms

### 1. Wire types — `wire/intel.rs` (widened for tool-calling)

The retired `intelligence/protocol.rs` `Request`/`Response`/`Usage` are the
seed (notes-mine §2.5: KEEP framing / ADAPT the types). They are widened, not
replaced. All intelligence wire types live in **this one module** so a future
serde→miniserde swap stays mechanical (assessment §2.2).

```rust
// wire/intel.rs

/// What the loop hands the intelligence client. Transport- and
/// dialect-agnostic; adapters map this into provider JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Concrete model id (e.g. "gpt-4o", "claude-opus-4"). The single
    /// endpoint serves one configured default; the loop may override.
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Scoped tool catalogue. Empty ⇒ omit the `tools` field entirely
    /// (JSON-action fallback path, or a no-tool final turn).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// "auto" (default) | "none" | "required" | a named tool. Maps to
    /// OpenAI `tool_choice` / anthropic `tool_choice`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

/// One message turn. `content` grows from a bare string to a typed
/// enum so tool-result turns and assistant-with-tool-calls turns round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// "system" | "user" | "assistant" | "tool"
    pub role: String,
    pub content: Content,
    /// Present only on assistant turns that requested tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Present only on role="tool" turns: which call this answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    /// Reserved for multimodal MCP content blocks (image/audio passed
    /// through from a tool result). v1 emits only `Text`.
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object, passed through from MCP `tools/list`
    /// `inputSchema` verbatim (RFC 0004). Untrusted server content —
    /// surfaced/logged but not auto-trusted (assessment §2.11).
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id. Anthropic supplies one; for OpenAI
    /// it is the `id` field. Minted locally if a provider omits it.
    pub id: String,
    pub name: String,
    /// Arguments as a parsed JSON object. OpenAI delivers a JSON
    /// *string*; the adapter parses it here so the loop sees an object.
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice { Auto, None, Required, Tool(String) }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Assistant free text. May be empty when the model only emits
    /// tool calls (the common ReAct case).
    #[serde(default)]
    pub content: String,
    /// Native tool calls. Empty ⇒ either a `final` turn or, on a
    /// json-toolmode endpoint, the action lives inside `content`.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// "stop" | "tool_calls" | "length" | "content_filter" | other.
    /// Normalised across dialects. "length" is surfaced to the loop as
    /// a distinct (recoverable) signal, not silently treated as final.
    #[serde(default)]
    pub finish_reason: FinishReason,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    #[default] Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

/// Token accounting — the loop adds these to per-node and tree-root
/// counters every turn (assessment §2.8, RFC 0003).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)] pub prompt_tokens: u32,
    #[serde(default)] pub completion_tokens: u32,
}
```

`Usage` keeps the retired `prompt_tokens`/`completion_tokens` shape verbatim so
RFC 0003's hierarchical token accounting (salvaged CAS tracker) plugs in
unchanged. The loop estimates the next request's window from the previous
response's `usage` plus a chars/4 forward heuristic (assessment §2.6) — no
tokenizer dependency.

### 2. Transport selection — `intel/client.rs`

```rust
pub trait Transport: std::io::Read + std::io::Write + Send {
    /// One-shot connect/timeout setup is done by the constructor; this
    /// trait is just the byte stream the HTTP writer/reader drive.
    fn shutdown_write(&mut self) -> std::io::Result<()>; // half-close for `Connection: close`
}

pub enum IntelTransport { Unix, Https, Vsock }

pub fn parse_intelligence_uri(uri: &str) -> Result<IntelEndpoint> {
    // unix:/run/intel.sock
    // https://host[:port]/v1/chat/completions
    // vsock:<cid>:<port>
    // No `url` crate: hand-split on "://" then scheme-specific parse,
    // reusing the retired `parse_url`/`ParsedEndpoint::parse` machinery
    // (tools/http.rs:162-204, intelligence/client.rs ParsedEndpoint).
}
```

Concrete constructors, each yielding a `Box<dyn Transport>`:

- **`unix:/path`** (core, always). `UnixStream::connect` + `set_read_timeout` /
  `set_write_timeout` from the run's `--timeout-secs`. Lifted nearly verbatim
  from `intelligence/client.rs` `UnixClient::complete` (notes-mine §2.3:
  KEEP-AS-IS). `shutdown_write` → `UnixStream::shutdown(Shutdown::Write)`.
- **`https://`** (feature `tls`). `TcpStream::connect` wrapped in a
  `rustls::StreamOwned` (`ring` provider, `webpki-roots`; assessment §2.2 — no
  `aws-lc-rs`, no cmake). `net/tls.rs` owns the rustls `ClientConfig`
  (constructed once, no per-request handshake config). SNI = the parsed host.
- **`vsock:<cid>:<port>`** (feature `vsock`, M4). `vsock::VsockStream::connect`
  with `VsockAddr::new(cid, port)`. `VsockStream` is `Read + Write` and
  `TcpStream`-shaped (research §4), so it drops into the same code path as the
  unix client — the unix client is the structural template (notes-mine §2.3).

**Plain `http://`** (no TLS) is accepted only for the unix/localhost/sidecar
case and is gated by the SSRF policy in RFC 0012 (`net/http.rs` blocks
RFC-1918/loopback/link-local by default unless explicitly opted in for dev).
The intelligence client enforces HTTPS in prod exactly as RFC 0012 specifies;
this RFC does not re-derive that policy.

### 3. The hand-rolled HTTP/1.1 request/response over `Read + Write`

One internal HTTP/1.1 client, consolidated from the two retired copies
(`intelligence/client.rs` `HttpClient` + `tools/http.rs` `perform_request`;
notes-mine §2.3, §2.6 say keep the better-rounded one and unify). It is
transport-agnostic: it takes `&mut dyn Transport` and never knows whether the
bytes go to a socket, a TLS stream, or a vsock fd.

```rust
// net/http.rs (LLM-facing half; MCP-over-HTTP is deferred per assessment §2.5)

pub struct HttpRequest<'a> {
    pub method: &'static str,           // always "POST" for chat/completions
    pub host: &'a str,                  // Host header
    pub path: &'a str,                  // "/v1/chat/completions"
    pub headers: Vec<(&'a str, String)>,// content-type, authorization, x-api-key, …
    pub body: &'a [u8],                 // serde_json::to_vec of the dialect body
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,                  // capped at 4 MiB (retired default)
}

pub fn round_trip<T: Read + Write>(t: &mut T, req: &HttpRequest) -> Result<HttpResponse>;
```

`round_trip` writes the request line, headers (always
`Connection: close` + `Content-Length`), CRLF, body; then reads the status line
(`parse_status_line`), headers (`parse_response_headers`), and a
Content-Length- or chunked-delimited body capped at 4 MiB (salvaged
`parse_status_line`/`parse_response_headers`/`parse_url`, notes-mine §2.6).
Header construction rejects CR/LF injection — salvage
`render_declared_headers`/`substitute_secret_placeholders` from `tools/http.rs`
for any value derived from config (assessment §2.11: "Salvage the retired
CR/LF-injection-rejecting header construction"). Secret header values
(`authorization`, `x-api-key`) are written but **never logged**.

A non-2xx HTTP status maps to `Error::Intelligence` with the status code and a
truncated (UTF-8-safe, salvaged `truncate`) body excerpt — exactly the retired
behavior (`providers.rs:215-225`) so 401/429/5xx surface cleanly. The loop's
error taxonomy (RFC 0007) classifies these: 429/5xx → bounded
transport-layer retry with backoff+jitter; 401/403 → fatal auth (exit code 4,
RFC 0011); connection refused/reset → fatal intelligence-unreachable (exit 4).

**Legacy framed `complete` over unix/vsock.** For the existing sidecar that
speaks the retired JSON-RPC `complete` envelope over 4-byte-LE length framing
rather than HTTP, the client supports `AGENT_INTELLIGENCE_WIRE=framed`
selecting `read_frame`/`write_frame` (KEEP-AS-IS from
`intelligence/protocol.rs:88-120`) instead of `round_trip`. Default is `http`.
The same `read_frame`/`write_frame` helpers are the control-channel codec in
RFC 0005 — shared, not duplicated.

### 4. Adapters — `intel/openai.rs` + `intel/anthropic.rs`

Each adapter is two functions: build the provider request body from `Request`,
and parse the provider response into `Response`. Salvaged from
`providers.rs` and widened for tool-calling (the net-new cost, notes-mine §2.4).

```rust
pub trait Adapter {
    fn build_body(&self, req: &Request) -> serde_json::Value;
    fn path(&self) -> &str;                       // "/v1/chat/completions" | "/v1/messages"
    fn auth_headers(&self, key: Option<&str>) -> Vec<(&'static str, String)>;
    fn parse(&self, http_status: u16, v: &serde_json::Value) -> Result<Response>;
}
```

**`openai-compatible` (canonical default).** `POST {base}/v1/chat/completions`,
`Authorization: Bearer <key>` (key optional — keyless for local vLLM/Ollama,
salvaged keyless path `providers.rs:441-449`). Body:

```jsonc
{
  "model": "gpt-4o",
  "max_tokens": 1024,
  "messages": [
    {"role": "system", "content": "…"},
    {"role": "user", "content": "…"},
    {"role": "assistant", "content": null,
     "tool_calls": [{"id": "call_1", "type": "function",
       "function": {"name": "fs.read", "arguments": "{\"path\":\"/x\"}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "<result text>"}
  ],
  "tools": [
    {"type": "function", "function": {
      "name": "fs.read", "description": "…",
      "parameters": { /* input_schema verbatim */ }}}
  ],
  "tool_choice": "auto"
}
```

Build rules: messages serialized in order (no system extraction — OpenAI takes
system inline as a message, retired `providers.rs:155-167`); `tools` omitted
when empty; OpenAI `arguments` is a JSON **string**, so `build_body` serializes
`ToolCall.arguments` (a `Value`) to a string. `parse` reads
`choices[0].message.content` (may be null/empty),
`choices[0].message.tool_calls[]` (each `function.name` +
`function.arguments` **parsed** from string back to a `Value` — failure to
parse → malformed-output observation, RFC 0007, not a hard error),
`choices[0].finish_reason` normalised into `FinishReason`, and
`usage.{prompt_tokens,completion_tokens}` (salvaged
`providers.rs:254-266`, extended with the tool-call and finish-reason arms).

**`anthropic`.** `POST {base}/v1/messages`, headers `x-api-key: <key>` +
`anthropic-version: 2023-06-01` (salvaged `providers.rs:131-154`). System prompt
extracted out-of-band via `split_system` (KEEP, `providers.rs:106-117`) into the
top-level `system` field. Tools mapped to anthropic's
`[{name, description, input_schema}]` (input_schema passed through verbatim —
anthropic already uses that exact key). Assistant tool calls serialize as
`content` blocks `{"type":"tool_use","id":…,"name":…,"input":<object>}`;
tool results as user-turn `{"type":"tool_result","tool_use_id":…,"content":…}`
blocks. `parse` walks `content[]`: `text` blocks concatenate into
`Response.content`, `tool_use` blocks become `ToolCall`s (anthropic's `input`
is already an object — no string-parse needed), `stop_reason` ("end_turn",
"tool_use", "max_tokens") normalises into `FinishReason`, usage from
`usage.{input_tokens,output_tokens}` (salvaged `providers.rs:241-252`, extended
with the `tool_use`/`stop_reason` arms).

The retired **gemini** and bare **openai** arms collapse: gemini is **dropped
from the binary** (lives behind the gateway, assessment §2.4); `openai` and
`openai-compatible` are one adapter (they already share the build/parse arms,
`providers.rs:155`). Dialect selected by `AGENT_INTELLIGENCE_DIALECT` ∈
{`openai`, `anthropic`}, default `openai`.

### 5. JSON-action fallback — `loop/action.rs`

When native tool-calling is unavailable, the loop emits no `tools` field and
instructs the model (via a rendered tool catalogue in the system prompt, RFC
0007) to answer with a single JSON object:

```jsonc
{"action": "tool", "tool": "fs.read", "args": {"path": "/x"}}
// or
{"action": "final", "result": "…"}
```

The response's `content` is run through **`extract_json_object`** —
balanced-brace, string-aware, code-fence/prose-tolerant — lifted **verbatim**
from `agent/loop_node.rs:343-373` (notes-mine §2.8: KEEP-AS-IS). The extracted
object is dispatched:

- `action:"tool"` → synthesize a single `ToolCall { id: minted, name: tool,
  arguments: args }` and route it exactly as a native tool call. The minted id
  is fed back as the `tool_call_id` on the result turn so the next request's
  transcript is well-formed regardless of toolmode.
- `action:"final"` → terminal `completed` with `result` as the distilled output.
- No parseable object, or unknown `action` → a malformed-output **observation**
  fed back to the model (recoverable, step-consuming — the retired
  recoverability discipline, notes-mine §2.8), never an abort.

Toolmode is decided per-endpoint:

```
AGENT_INTELLIGENCE_TOOLMODE = native (default) | json | auto
```

`auto` tries native first; if the first response carries no `tool_calls` **and**
`content` parses as an `{"action":…}` object, the client latches `json` for the
session (so chatty no-tool finals don't flip it). `native` and `json` are
explicit overrides. This keeps native primary and JSON-action a demoted
fallback (assessment §2.4).

### 6. Credentials — `sec/secrets.rs`

Single front door, salvaged shape from `secrets/mod.rs` (notes-mine §2.10):

```rust
/// The ONE function every credential consumer calls. Resolution:
///   1. a configured source for `name` (env-alias or file), else
///   2. the process environment variable `name`.
pub fn resolve(name: &str) -> Result<Token>;

/// Wraps a secret string. Never Serialize; Debug prints "***".
/// This is the one secret carrier; RFC 0011/0012 refer to it as `Secret`
/// (same newtype, same never-logged/never-serialized policy — RFC 0012 owns
/// the policy, this RFC owns the `resolve()` resolution order below).
pub struct Token(String);
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}
impl Token { pub fn expose(&self) -> &str { &self.0 } }
```

Sources kept: **`env`** (alias another env var) and **`file`** (live per-resolve
read — k8s Secret mounts / Vault Agent sidecars rotate by replacing the file,
notes-mine §2.10). Sources **dropped for v1**: `command` (`secrets-exec`) and
`oauth2` (`secrets-oauth2`, the whole `secrets/oauth2.rs` — pulls `ureq`,
exceeds the minimalism bar). No `[[secrets]]` TOML block, no
`deny_unknown_fields` custom deserializer — config is flat env/flags
(assessment §2.10); secrets are **never** read from the config file, only
env/flag (§2.10, §2.11).

Credential plumbing into the intelligence client:

- `AGENT_INTELLIGENCE_TOKEN` — the generic bearer / `x-api-key` value (resolved
  by name through `resolve`, so it may be a file-backed rotating secret).
- Provider-specific names accepted as aliases for clarity but resolved through
  the same front door.
- The key is resolved **per request** (salvaged from `providers.rs:123-129`) so a
  rotated file is picked up on the next call with no reload — the assessment's
  restart-to-reload posture (§2.10) plus live file rotation, no hot-reload
  subsystem.

**Build-time / startup probe → fast-fail** (salvaged `providers.rs:55-67`): on
construction the client resolves the configured key name once; a
named-but-unset/empty key is a startup `Error::Config` → **exit 2** in
milliseconds (RFC 0011 validate-at-startup), not a first-request 401. A keyless
endpoint (local gateway) skips the probe.

**Never logged / persisted / in-transcript** (assessment §2.4, §2.11): the
`Token` `Debug` impl, the never-`Serialize` discipline, and the secret-header
allowlist (§2.9 log field allowlist) together guarantee credentials stay out of
JSON-lines logs, OTLP spans, the transcript fed back to the model, and any
checkpoint. The `RemoteClient` `Debug` impl that prints only
`{kind, model, base_url}` and omits key material is salvaged verbatim
(`providers.rs:27-36`).

### 7. The client call path (end to end)

```rust
impl IntelligenceClient {
    pub fn complete(&self, req: &Request) -> Result<Response> {
        let key = self.key_name.as_ref().map(|n| secrets::resolve(n)).transpose()?;
        let body = self.adapter.build_body(req);               // dialect-specific
        let payload = serde_json::to_vec(&body)?;
        let mut transport = self.dial()?;                      // unix | tls | vsock
        match self.wire {
            Wire::Http => {
                let mut headers = vec![("content-type", "application/json".into())];
                headers.extend(self.adapter.auth_headers(key.as_ref().map(Token::expose)));
                let resp = http::round_trip(&mut *transport, &HttpRequest {
                    method: "POST", host: &self.host, path: self.adapter.path(),
                    headers, body: &payload,
                })?;
                let v: serde_json::Value = serde_json::from_slice(&resp.body)?;
                self.adapter.parse(resp.status, &v)
            }
            Wire::Framed => { /* write_frame(complete) ; read_frame ; parse RpcResponse */ }
        }
    }
}
```

One connect per call, `Connection: close` (salvaged posture, notes-mine §2.3) —
no keep-alive state, no pooling; the request rate is single-digit per second per
subagent so this is free. The call is synchronous and blocks the subagent's
turn by design (the agentic loop is single-threaded per subagent; the
supervisor never blocks on it — assessment §2.1). Timeout is the run's
`--timeout-secs`, enforced via `set_read_timeout`/`set_write_timeout` on the
socket (unix/vsock) and the rustls stream's underlying `TcpStream`
(https); a timeout surfaces as a transient transport error (bounded retry, RFC
0007).

---

## Interactions with other RFCs

- **RFC 0001 (core):** this is the intelligence channel of the two-loop split;
  the client lives in the subagent process, never in the supervisor.
- **RFC 0004 (MCP client & codec):** the intelligence wire is **not** MCP and
  must not be conflated (assessment §2.4). They share only `serde`/`serde_json`
  and the `read_frame`/`write_frame` helpers; the `ToolDef.input_schema` is the
  MCP `tools/list` `inputSchema` passed through verbatim (RFC 0004 owns
  discovery; this RFC owns putting it in the `tools` field).
- **RFC 0005 (self-MCP & control protocol):** shares the length-framed codec
  (`read_frame`/`write_frame`) used by the legacy framed intelligence wire;
  otherwise independent.
- **RFC 0007 (agentic loop):** consumes `Request`/`Response`. The loop builds the
  request (system + instruction + transcript + scoped `tools`), calls
  `complete`, records `usage`, and branches on `tool_calls` vs final. This RFC
  owns the wire and the JSON-action *parse*; RFC 0007 owns the loop, stop
  conditions, VERIFY, context compaction, and the error-taxonomy classification
  of the HTTP statuses this RFC surfaces. `finish_reason: Length` is handed to
  RFC 0007 as a distinct recoverable signal.
- **RFC 0011 (cloud-native contract):** an unset key → exit 2 (validate at
  startup); intelligence unreachable / auth after retries → exit 4. The
  per-request resolve + file source realize the "restart-to-reload, live file
  rotation" stance.
- **RFC 0012 (security):** owns the SSRF defenses in `net/http.rs`
  (RFC-1918/loopback/link-local blocking, redirect validation, HTTPS-in-prod)
  and the CR/LF-rejecting header construction; this RFC consumes them. Owns the
  `secrets::resolve` front-door policy this RFC implements the LLM-key half of.
- **RFC 0003 (supervision):** `Usage.{prompt_tokens,completion_tokens}` feed the
  hierarchical token accounting (the salvaged CAS tracker) every turn.

---

## Non-goals / Deferred

- **More than two in-binary adapters.** Gemini and any other provider live
  behind the gateway, not in the binary (assessment §2.4). The retired gemini
  arm is dropped.
- **MCP over this transport.** This channel is LLM-only; MCP transport is RFC
  0004/0005 (assessment §2.4).
- **MCP-over-HTTP *serving*** and the SSE GET read path — deferred (assessment
  §2.5, §2.13); the LLM wire is request/response only (no streaming responses in
  v1; `stream:false`).
- **`command` and `oauth2` secret sources** — dropped for v1; may return as
  optional features (notes-mine §2.10).
- **Hot-reload of credentials/config** — restart-to-reload; live `file`-source
  rotation covers the rotating-key case without a reload subsystem (assessment
  §2.10).
- **Keep-alive / connection pooling / HTTP/2** — out; one connect per call,
  `Connection: close`.
- **`sampling/createMessage` (intelligence-sharing in either direction)** — a v2
  MCP feature, not this transport (assessment §1.3.5, §2.13).
- **Multimodal request content** — the `Content::Blocks` variant is reserved;
  v1 emits `Text` only (tool results flattened to text).
- **Windows** — Unix-first (vsock/unix-socket/signals are Unix); the retired
  Windows paths are not carried.

## Open items

- **Streaming.** v1 is non-streaming (`stream:false`), so a long generation
  cannot send `notifications/progress`-style keepalives on this channel; the
  subagent's no-progress watchdog (RFC 0003) is governed by the control channel,
  not the LLM call, so a long legitimate generation is covered by the request
  timeout + ping/pong, not by streaming. If the request-timeout-vs-long-reasoning
  tension bites in practice, streaming `/chat/completions` (SSE `data:` lines
  over the same hand-rolled reader) is the smallest fix — flagged, not adopted.
- **`auto` toolmode latch granularity.** Latching `json` per session on first
  no-native-tool-calls response is a heuristic; a model that legitimately ends a
  turn with a JSON-shaped final on a native endpoint could mis-latch. Mitigation
  is to prefer explicit `native`/`json` in production and treat `auto` as a
  dev convenience. Left open pending field data.
