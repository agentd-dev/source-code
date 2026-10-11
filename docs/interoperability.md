# Interoperability

This page lists what agentd works with: the protocol revisions it speaks, the
model providers and wire formats it reaches, the credential schemes it can
present, the platforms it ships for, and the telemetry standards it emits. It
is for evaluators and integrators checking fit before they build. Each section
says what is supported and, as plainly, what is not.

## Model Context Protocol (MCP)

agentd is an MCP **client**: every tool and resource an agent uses comes from
an MCP server you declare. It is not an MCP server, and nothing reaches into
agentd over MCP.

| | Support |
|---|---|
| Revisions | **2026-07-28**, reached with `server/discover` (no handshake, no session). **2025-11-25** and older revisions a server lists, reached with the `initialize` handshake. agentd probes each server and picks per server. |
| Transport | Streamable HTTP over HTTPS. Plain `http://` only to a loopback host, for development. |
| Implementation | [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk), the official Rust SDK (3.5), over agentd's own HTTP transport, so agentd's credentials and signing apply to every request. |
| Tools | `tools/list` (every page) and `tools/call`; `list_changed` refreshes the catalogue. |
| Resources | list, read, and subscriptions: `resources/subscribe` on 2025-11-25, a `subscriptions/listen` filter on 2026-07-28. A change notification triggers a re-read (notify-then-read). |
| Prompts | `prompts/list` and `prompts/get`, used as skills and by the `mcp.resource` workflow step. |
| Completions | the `mcp.resource` step with `op: complete`. |
| Elicitation | supported: a server's question becomes a human approval gate in every attached client; an answer that cannot match the requested schema, or none within 300 s, is returned as `cancel`. |
| MCP Events | the draft extension SEP-3415, on 2026-07-28 connections: bindings into durable streams, runtime subscriptions inside an operator's grant, poll and webhook delivery. In the release binaries; agentd says at load that it tracks a draft. |
| Authorization | OAuth 2.1 with RFC 9728 protected-resource discovery, and the other outbound schemes [below](#credentials-agentd-can-present). |

**Not supported:** `sampling`, `roots` and `tasks` (agentd declares no client
capability for them, so a server's request is dropped rather than answered);
the `logging` capability; the stdio transport (run a bridge for a stdio-only
server); acting as an MCP server. A `resources/read` that answers
`input_required` fails.

[MCP support reference](mcp-reference.md) has the probe rules, the
capability matrix and the known limits.

## Agent2Agent (A2A)

A2A is how people and other agents reach a running agentd, and how agentd
delegates to other agents.

| | Support |
|---|---|
| Version | **A2A 1.0**, JSON-RPC 2.0 binding over HTTPS. Every request carries `A2A-Version: 1.0`; a missing or other version is refused with `-32009`. |
| Methods | all eleven of the 1.0 JSON-RPC binding: `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, the four task push-notification config methods, `GetExtendedAgentCard`. |
| Discovery | the public Agent Card at `/.well-known/agent-card.json`; an authenticated extended card. |
| Streaming and push | SSE streams; push notifications to caller-registered webhooks when `a2a.push.enabled` is on. |
| Wire format | proto3 JSON, written by the official SDK's types generated from the A2A protobuf. |
| Implementation | [`a2aproject/a2a-rs`](https://github.com/a2aproject/a2a-rs), the A2A project's official Rust SDK (`a2a-lf`, `a2a-pb`, `a2a-server-lf`). |
| Inbound identity | bearer tokens, mTLS client certificates (subject and SAN matching, SPIFFE URIs included), and sessions from an OAuth 2.0 device authorization grant (RFC 8628) the listener serves itself. |
| Co-located agents | a JSON-RPC binding over a unix domain socket, where the kernel authenticates the peer: [binding-jsonrpc-unix](https://agentd.dev/a2a/binding/jsonrpc-unix). |

agentd declares three extensions on its card, none of them required, so a
plain A2A client gets the whole core protocol:

| Extension | What it adds |
|---|---|
| [command](https://agentd.dev/a2a/ext/command) | structured operations sent as a DataPart on `SendMessage`: run a workflow, read status, operator controls |
| [events](https://agentd.dev/a2a/ext/events) | an instance-wide observation feed for display clients |
| [task-annotations](https://agentd.dev/a2a/ext/task-annotations) | agentd's facts about a task, under the task's metadata |

**Proven against another implementation.** In CI the official Python SDK,
`a2a-sdk` 1.1.5, runs as a client of a real agentd and parses every answer
strictly into the protocol's own types. The TypeScript display clients and
agentd's outbound client run against that SDK's official sample server.

**Not supported:** A2A 0.3 method names (`message/send` and the like are
`-32601`); the gRPC and HTTP+JSON bindings.
[A2A API reference](a2a-reference.md) has the methods, errors and limits.

## Model providers

agentd speaks four wire formats, chosen per connection with `dialect`. A
configuration can name several connections (`intelligence.providers`), each
with its own credentials, endpoint failover and circuit breakers.

| Dialect | Wire | Reaches |
|---|---|---|
| `openai` (default) | OpenAI-compatible Chat Completions, posted to the endpoint URL as written | OpenAI, Azure OpenAI, vLLM, Ollama, LM Studio, LiteLLM and most gateways |
| `anthropic` | the Anthropic Messages API | Anthropic, at `https://api.anthropic.com` |
| `bedrock` | the Amazon Bedrock Converse API, every request SigV4-signed | models on Amazon Bedrock |
| `decisions` | the decisions shape (`{model, state, questions}` in, `{answers, usage}` out) that Jev, OpenRouter's decisions resource and Strands Decider share | decision models, on a named provider only |

Write an OpenAI-compatible endpoint in full —
`https://api.openai.com/v1/chat/completions` — because a URL with a path is
posted to exactly as written. Gemini, Cohere and other providers are not in
the binary; reach them through an OpenAI-compatible gateway.

Every model is one of three kinds. A **chat** model generates text, tool calls
and JSON. A **reasoning** model thinks first, at an effort agentd maps to each
dialect's own form. A **decision** model answers typed questions with a
confidence and writes nothing. Calls whose answer has a JSON Schema ask the
provider to hold the model to it: `response_format` on OpenAI,
`output_config.format` on Anthropic and on Claude models on Bedrock.

```yaml
# fragment: under intelligence:
endpoints: https://api.anthropic.com
dialect: anthropic
token: "{{secret:ANTHROPIC_API_KEY}}"
providers:
  openai:
    endpoints: https://api.openai.com/v1/chat/completions
    token: "{{secret:OPENAI_API_KEY}}"
  openrouter:
    endpoints: https://openrouter.ai/api/alpha/decisions
    dialect: decisions
    token: "{{secret:OPENROUTER_API_KEY}}"
models:
  smart: {model: claude-sonnet-5}
  fast: {model: gpt-5-mini, provider: openai}
  deep: {model: claude-opus-5-5, kind: reasoning}
  jev: {kind: decision, provider: openrouter, model: typesafe/jev-1.13, escalate: smart}
roles:
  turn: smart
  think: deep
  compaction: fast
  decide: {model: jev, confidence: 0.8}
```

> **Limit.** Native tool calling is the only tool path. A model or gateway
> that ignores the `tools` field ends every turn on its first message; put a
> gateway that implements tool calling in front of it.

[Connect model providers](connect-models.md) shows each provider's
configuration; [Intelligence wire reference](intelligence-reference.md) has
the per-dialect mapping.

## Credentials agentd can present

agentd can present these credentials on its outbound connections. Most are
set in an `auth:` block, which has the same shape on model connections, MCP
servers and A2A peers; AAuth is set once, under `security.aauth`. A scheme a
connection cannot apply is refused when the configuration loads.

| Scheme | Model connection | MCP server | A2A peer |
|---|---|---|---|
| Static bearer token | yes | yes | yes |
| Static custom header in an `auth:` block | — (use `headers`, below) | yes | yes |
| OAuth 2.1: device code, authorization code with PKCE, client credentials; RFC 8414 and OIDC discovery | yes | yes | yes |
| AWS SigV4: environment, IAM Identity Center (SSO), EC2 instance role, EKS IRSA | yes, and required by `dialect: bedrock` | yes | yes |
| SPIFFE JWT-SVID as a bearer | yes | yes | yes |
| SPIFFE X.509-SVID or a client certificate (mTLS) | — | yes | yes |
| AAuth agent identity: Ed25519 key, RFC 9421 HTTP message signatures (draft) | yes, signed beside the bearer | yes | yes |

A model connection that needs a custom credential header — an `api-key`
header, say — sends it through its `headers` map, with the value as a secret
reference. Interactive OAuth logins run once with
`agentd --login <target>` and refresh on their own; the targets are
`intelligence`, `intelligence:<provider>`, `mcp:<name>` and `service:<name>`.
An A2A peer that needs an interactive login takes its credential from a
`services:` entry. Secrets are always
`{{secret:NAME}}` or `{{secret-file:PATH}}` references. [Outbound
credentials](authentication.md) and [AAuth agent identity](aauth.md) have the
details.

## Platforms and distribution

| Artifact | What it is |
|---|---|
| Release binaries | static Linux (musl) builds for `x86_64` and `aarch64`, with a `SHA256SUMS` file |
| Installer | `curl -fsSL https://agentd.dev/install.sh \| sh` picks the architecture and verifies the checksum |
| Container image | `ghcr.io/agentd-dev/agentd`, multi-arch (`linux/amd64`, `linux/arm64`), `FROM scratch`, runs as uid 65532, cosign-signed (keyless) with an SPDX SBOM attestation |
| Display clients | the npm package `@agentd-dev/cli`: `agentd-tui`, `agentd-ui` and the thin-client core they share |
| Rust crates | `agentd-core` (the engine, library name `agentd`), `agentd-cli`, `agentd-mcp`, `agentd-net`, `agentd-instruction` on crates.io |
| Host install | a hardened systemd unit and environment file under `packaging/` |

There are no prebuilt macOS or Windows binaries. A source build needs Rust
1.96 or newer and a C compiler. The release binaries and the image carry the
same feature set. It includes the mock model endpoint (`mock:final`,
`mock:file:<playbook.json>`), for trying an agent or a workflow without a
model provider; the guarded local command runner (`exec`) is in neither.
[Build features and crates](features.md) lists every feature.

## Observability standards

| Standard | How agentd uses it |
|---|---|
| JSON Lines | every log event on stderr, one object per line, with a run id and a trace id; the result of a one-shot run on stdout |
| Prometheus text exposition 0.0.4 | `/metrics`, beside `/healthz` and `/readyz`, on the address `observability.metrics_addr` names |
| OpenTelemetry OTLP/HTTP (JSON) | spans per run, mapped to the GenAI semantic conventions, and the log stream with `observability.otel.logs: true`, both sent to `observability.otel.endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT` when it is unset) |
| W3C Trace Context | a `traceparent` adopted at start from `--traceparent` or `AGENTD_TRACEPARENT`, and sent on every MCP request and every model request |
| Exit codes | a stable table a Kubernetes `podFailurePolicy` can branch on; see [CLI, environment and exit codes](cli.md) |

[Events and metrics reference](telemetry-reference.md) lists every event and
series.

## Documents, webhooks and other formats

| Standard | Where |
|---|---|
| [Instruction Specification](https://github.com/instruction-md/specification) version 1, registry revision 1.1 | instruction documents: parsing, delivery, signatures and the resolution manifest |
| OCI distribution, cosign signatures | `oci://` instruction sources, verified against a cosign key |
| age v1, JWE Compact (ECDH-ES with X25519, `dir`) | encrypted instruction documents |
| HMAC-SHA256 webhook signatures | inbound webhook routes, beside header and bearer checks |
| Standard Webhooks | MCP Events webhook delivery, verified before the body is parsed |
| JSON Schema 2020-12 (a subset) | workflow inputs, output schemas, command arguments |
| CEL | guards, filters and computed values in workflows and policies |
| Cron, five fields, UTC | `schedule` start nodes |

## Next steps

- [Connect model providers](connect-models.md) — configure each provider.
- [Connect MCP servers](connect-mcp.md) — declare servers and their
  credentials.
- [Connect agents over A2A](connect-agents.md) — serve an agent and call
  others.
- [Build a client](build-a-client.md) — talk to agentd from your own code.
