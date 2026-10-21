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
| Prompts | `prompts/get`, by the `mcp.resource` workflow step. A prompt is not a skill. |
| Completions | the `mcp.resource` step with `op: complete`. |
| Elicitation | supported: a server's question becomes a human approval gate in every attached client; an answer that cannot match the requested schema, or none within 300 s, is returned as `cancel`. |
| MCP Events | the draft extension SEP-3415, on 2026-07-28 connections: bindings into durable streams, runtime subscriptions inside an operator's grant, poll and webhook delivery. In the release binaries; agentd says at load that it tracks a draft. |
| [MCP Tasks](connect-mcp.md#long-running-tool-calls) | the extension SEP-2663 (`io.modelcontextprotocol/tasks`), on 2026-07-28 connections, as a client: a tool call a server answers with a task is followed to its end — a workflow step's as a durable wait that survives a restart, a model's within the call and then with `mcp.task.get` — polled at the server's interval or pushed on `subscriptions/listen`, cancelled with `tasks/cancel`. `mcp.servers[].tasks: off` refuses a server's tasks. In the release binaries. |
| [MCP Skills](connect-mcp.md#load-skills-from-a-server) | the extension SEP-2640 (`io.modelcontextprotocol/skills`), on 2026-07-28 connections that declare it, as a client: `skills/list` (every page), `skills/get` for a skill no listing names, `resources/directory/read` where the server declares `directoryRead`. Every file is checked against its entry's `sha256:` digest and size, and `SKILL.md`'s frontmatter against the entry's; a skill is named by its server and URI. In the release binaries. |
| [Enterprise-Managed Authorization](authentication.md#enterprise-managed-authorization) | the extension `io.modelcontextprotocol/enterprise-managed-authorization`, as a client, on servers of `grant: id_jag`: declared in `initialize`'s capabilities at 2025-11-25 and in every request's `_meta` at 2026-07-28; the server is reached with an access token its authorization server issued for an ID-JAG from the person's IdP, once its RFC 9728 metadata has confirmed the pinned resource and authorization server. A preview: the `xaa` feature, not in the release binaries. |
| Authorization | OAuth 2.1 with RFC 9728 protected-resource discovery, and the other outbound schemes [below](#credentials-agentd-can-present). Client registration in the order 2026-07-28 gives: a configured `client_id`, else agentd's [Client ID Metadata Document](authentication.md#client-id-metadata-document) where the authorization server advertises one; dynamic client registration, which that revision deprecates, is not implemented. |

**Not supported:** `sampling` and `roots` (agentd declares no client
capability for them, so a server's request is dropped rather than answered);
the experimental core tasks of MCP 2025-11-25 (a connection at that
revision gets every answer inline);
the `logging` capability; the stdio transport (run a bridge for a stdio-only
server); acting as an MCP server, or serving any MCP extension. A
`resources/read` that answers `input_required` fails.

[MCP support reference](mcp-reference.md) has the probe rules, the
capability matrix and the known limits.

## Agent Skills

A skill is a folder holding a `SKILL.md`: YAML frontmatter that names and
describes it, and a Markdown body that tells the agent how to do one kind of
work, beside any `references/`, `assets/` or `scripts/` it needs. agentd
reads skills in the [Agent Skills](https://agentskills.io/specification)
format from folders on disk and, over MCP, from a server's
[Skills extension](connect-mcp.md#load-skills-from-a-server).

| | Support |
|---|---|
| Layout | `<folder>/<name>/SKILL.md`. A loose file in the folder (`README.md`, `<name>.md`) and a directory without a `SKILL.md` are not skills. |
| `name` | required: 1–64 characters of `a-z`, `0-9` and `-`, not starting or ending with `-`, without `--`, and equal to the directory's name |
| `description` | required: 1–1024 characters |
| `license`, `compatibility`, `metadata` | optional: a string; 1–500 characters; a map of strings to strings |
| `allowed-tools` | read as a string and ignored: a skill never grants a tool |
| Other fields | kept and ignored |
| `scripts/` | never run; a file is read as text with `skills.read` |
| User scope | `skills.dir`, or `skills/` beside the configuration file when it is unset |
| Project scope | `.agents/skills/` beside a loaded configuration file. A project skill wins a name over a user skill, said as `skills.collision`. Never read from the working directory alone or the home directory. |
| Progressive disclosure | each skill's name and description in every system prompt; the body once `@skill:<name>` or `skills.load` loads it; a file through `skills.read` |

A folder skill that breaks a rule refuses the configuration, naming the
file and the rule:

```text
skills: ./.agents/skills/runbook/SKILL.md: `name` "Incident_Runbook" has 'I'; only a-z, 0-9 and - are allowed (agentskills.io)
```

The instruction document's `:::!skill` blocks join the same catalogue.
agentd's own skills — the instruction's, the project's and the user's —
share one name each, and are named `local/<name>` where a server's skill
shares the name.

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
| Inbound identity | bearer tokens, mTLS client certificates (subject and SAN matching, SPIFFE URIs included), sessions from an OAuth 2.0 device authorization grant (RFC 8628) the listener serves itself, and enterprise JWT access tokens (RFC 9068, or an Okta custom authorization server's), declared on the card as `oauth2` schemes with the listener's RFC 9728 metadata. |
| In-task authorization | `TASK_STATE_AUTH_REQUIRED` (§7.6) where a person must sign in at their IdP before work acts with their grant — at admission, or at a step-up; both are the `xaa` preview, not in the release binaries — and, delegating, a peer's task in it followed until the person authorizes the peer. |
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

## Personal Agent Protocol

The Personal Agent Protocol is how a company publishes what an agent acting
for one of its customers may use: MCP servers, its own agent and OpenAPI
APIs. agentd is a **personal agent** under it, for one person, the User. The
support is a preview: it is not in the release binaries.

| | Support |
|---|---|
| Version | draft **0.1**, pinned at the draft repository's commit `95fcbf2`. The draft can change without changing its `protocol_version`, so agentd follows the pinned revision, its JSON Schemas vendored and checked in CI. A discovery document whose major version is not `0` is refused. |
| Build | the `companies` build feature, which no release binary or image carries; [Build features and crates](features.md) has the build. |
| Role | the Personal Agent, for one User per instance. Not the Company and not the Company Agent. |
| Discovery | `/.well-known/poppy.json`, following HTTPS redirects only; the issuer's RFC 8414 metadata, its `issuer` exact and its `poppy_domains` listing the domain; every URL the documents name held to the entry's `hosts`. |
| Identity | a `client_id` that is the HTTPS URL of a client metadata document, a key set of ES256 keys, and `private_key_jwt` client assertions (RFC 7523) addressed to the issuer. agentd prints the documents for static hosting or serves them on its A2A listener. |
| User IDs | random, one per User and company, kept in the store and sealed when a seal key is set. |
| Sessions | started and renewed with the JWT bearer grant (RFC 7523) and a Session assertion; DPoP-bound tokens (RFC 9449, ES256, server nonces) on every API and conversation request; for MCP, a Bearer token per server, requested with an RFC 8707 `resource`. |
| Sign-in | Device Sign-In (RFC 8628) and Direct Sign-In (authorization code with PKCE and RFC 9207 `iss`), each at the User's own gate. Account Tokens are sealed with AES-256-GCM, rotated when the company rotates them, and revoked on sign-out (RFC 7009); the User may hold up to eight accounts at a company, each with its own token and its own Session (§4.4). Mediated Sign-In is refused at load. |
| Interfaces | a company's MCP servers (2025-06-18 or later), each tool as `<server>.<tool>`; its agent, in the `poppy` conversation protocol, read over SSE or by long poll; its OpenAPI 3.0 and 3.1 descriptions (JSON). Direct Conversations, which the User joins at their own gate. |
| Extensions | Operations, version 1: an operation is confirmed only with the User's approval of its revision, or an operator's standing permission. |
| Privacy | the strict profile by default: none of agentd's correlators, a keyed idempotency key and a trace of its own per unit of work and company. |
| Tested against | agentd's own fake company, in the conformance suite's `companies` family. Nothing published tests a personal agent. |

**Not supported:** Mediated Sign-In; joining a browser to a Session; several
Users per instance or several accounts per User at one company; the Company
and Company Agent roles; the push and attachments the draft leaves open; any
revision of the draft but the pinned one.
[Act at companies for a person](act-at-companies.md) is the guide, and
[RFC 0047](../rfcs/0047-acting-at-companies-for-a-person.md) the design.

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
| OAuth 2.1: device code, authorization code with PKCE, RFC 9207 `iss` and an OpenID Connect `nonce`, client credentials; RFC 8414 then OIDC discovery with an exact `issuer`; RFC 8707 `resource` (by default on MCP servers only); `private_key_jwt` (RFC 7523) and `client_secret_basic`; RFC 7009 revocation on `--logout`; with no registration, a [Client ID Metadata Document](authentication.md#client-id-metadata-document) (draft -02) | yes | yes | yes |
| AWS SigV4: environment, IAM Identity Center (SSO), EC2 instance role, EKS IRSA | yes, and required by `dialect: bedrock` | yes | yes |
| SPIFFE JWT-SVID as a bearer | yes | yes | yes |
| SPIFFE X.509-SVID or a client certificate (mTLS) | — | yes | yes |
| AAuth agent identity (a draft): Ed25519 key, RFC 9421 HTTP message signatures | yes, signed beside the bearer | yes | yes |
| An enrolled person's grant (`grant: id_jag`, the `xaa` preview): an ID-JAG (draft -04) from their IdP by RFC 8693 token exchange, redeemed by the JWT bearer grant (RFC 7523) for an access token — Enterprise-Managed Authorization | — | yes | — |

A model connection that needs a custom credential header — an `api-key`
header, say — sends it through its `headers` map, with the value as a secret
reference. Interactive OAuth logins run once with
`agentd --login <target>` and refresh on their own; the targets are
`intelligence`, `intelligence:<provider>`, `mcp:<name>` and `service:<name>`.
An A2A peer that needs an interactive login takes its credential from a
`services:` entry. A company entry (`kind: company`, in the `companies`
preview) takes no `auth:` block: agentd presents what the
[Personal Agent Protocol](#personal-agent-protocol) defines —
`private_key_jwt` assertions signed with ES256, DPoP-bound tokens
(RFC 9449), Bearer tokens for one MCP server each with an RFC 8707
`resource`, and RFC 7009 revocation on sign-out. Secrets are always
`{{secret:NAME}}` or `{{secret-file:PATH}}` references. [Outbound
credentials](authentication.md) and [AAuth agent identity](aauth.md) have the
details.

## Internet standards

The IETF RFCs agentd implements, and where. The release binaries implement
all of them except the last two, which only the `companies` and `xaa`
previews use. OAuth discovery also reads OpenID Connect Discovery 1.0
metadata, after RFC 8414. Two OAuth Working Group drafts are implemented
too: in the release binaries, the [Client ID Metadata Document](authentication.md#client-id-metadata-document),
draft-ietf-oauth-client-id-metadata-document-02, agentd's client where it
has no registration; in the `xaa` preview, the Identity Assertion JWT
Authorization Grant ([ID-JAG](authentication.md#enterprise-managed-authorization)),
draft-ietf-oauth-identity-assertion-authz-grant-04. The `xaa` preview also
speaks OpenID Client-Initiated Backchannel Authentication Core 1.0 in poll
mode ([CIBA](authentication.md#work-nobody-is-there-for)): a scheduled
workflow's firing asks its person to approve it on their own device.

| RFC | What | Where agentd uses it |
|---|---|---|
| RFC 6749, RFC 6750 | OAuth 2.0 and bearer tokens | every `oauth2` credential on model connections, MCP servers and A2A peers |
| RFC 7636 | PKCE | authorization-code logins |
| RFC 8628 | Device authorization grant | `agentd --login`, and the grant agentd serves display clients on its A2A listener |
| RFC 8414 | Authorization server metadata | OAuth discovery, before OpenID Connect Discovery |
| RFC 9728 | Protected resource metadata | finding an MCP server's authorization server, its `resource` matched byte for byte; the A2A listener's own, where it takes enterprise access tokens |
| RFC 9068 | JWT access tokens | the enterprise access tokens the A2A listener takes (`a2a.inbound_tokens`), checked claim by claim against their issuer's keys |
| RFC 8707 | Resource indicators | every authorization, token, refresh and client-credentials request to an MCP server's authorization server; the Bearer token for a company's MCP server |
| RFC 9207 | `iss` in authorization responses | browser logins |
| RFC 7523 | JWT client assertions (`private_key_jwt`), and the JWT bearer grant | client authentication at a token endpoint; redeeming an ID-JAG (the `xaa` preview) |
| RFC 7009 | Token revocation | `agentd --logout`; a delegation's refresh tokens, and one a CIBA approval came with (the `xaa` preview) |
| RFC 9421 | HTTP Message Signatures | every request agentd signs with [AAuth](aauth.md) |
| RFC 7748, RFC 7516 | X25519 and JWE Compact | encrypted instruction documents |
| RFC 8446 | TLS 1.3 | every HTTPS connection, in and out |
| RFC 9449 | DPoP | requests to a company (the `companies` preview); the authorization-server and MCP legs of a delegated server whose metadata advertise it (the `xaa` preview) |
| RFC 8693 | Token exchange | exchanging an enrolled person's grant at their IdP for an ID-JAG, and a grant's or a caller's access token for a same-domain MCP server's (the `xaa` preview) |

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
model provider; the guarded local command runner (`exec`) and the
[`companies` preview](#personal-agent-protocol) are in neither.
[Build features and crates](features.md) lists every feature.

## Observability standards

| Standard | How agentd uses it |
|---|---|
| JSON Lines | every log event on stderr, one object per line, with a run id and the trace and span of the unit of work it is about; the result of a one-shot run on stdout |
| Prometheus text exposition 0.0.4 | `/metrics`, beside `/healthz` and `/readyz`, on the address `observability.metrics_addr` names |
| OpenTelemetry OTLP/HTTP (JSON) | one tree of spans per trace — the process, each run, each step attempt, each turn (`invoke_agent`, `chat`, `execute_tool`, mapped to the GenAI semantic conventions), each request served (SERVER) and sent for a unit of work (CLIENT, with the span id the request carried; a connection's own MCP requests name the process's span), each `exec` command, each event emitted (PRODUCER), with a line's `link` and `links` as span links — and the log stream with `observability.otel.logs: true`, each record naming its `traceId` and `spanId`, both sent to `observability.otel.endpoint` (or `OTEL_EXPORTER_OTLP_ENDPOINT` when it is unset) |
| W3C Trace Context | a trace per unit of work (a run, a conversation turn, an A2A task), continued from what started it: the run that started it, an A2A request's or a webhook's `traceparent` and `tracestate` headers, a stream event's `traceparent` attribute, an MCP Events occurrence's `_meta.traceparent`; a `batch` or `correlate` run and an event appended with `into:` start a trace of their own and record their causes as `links`, and a gate's answer, a callback, a reply or an event that wakes a waiting run, or a message that continues a task, is a `link` on the run or task, which keeps its trace. The process's own continues `--traceparent` or `AGENTD_TRACEPARENT`. A `traceparent`, with the trace's `tracestate` when it has one, is sent on every MCP request (`params._meta`), every model request, A2A request and push notification, `http` step and `forward:` push (HTTP headers), as the CloudEvents distributed-tracing attributes of a stream event produced in a trace (an `emit`, an `into:` append, a child's mirrored event, an MCP Events occurrence that names one, a `_runtime` event: the span its line was in), and as `TRACEPARENT` to an `exec` command; an A2A answer that names a task carries a `traceparent` response header, and a webhook's answer one naming the run it started. The outputs name the trace: a task's annotations (`traceId`, `spanId`), a webhook's answer (`run_id`, `trace_id`), `status`, `run.get`, `workflow.status` and the event feed's run, step, activity and audit events (`trace_id`), a `_runtime` event (`traceparent`). A trace whose upstream is unsampled stays unsampled and exports no spans. A call to a company under `privacy: strict` (the `companies` preview) is sent a trace of its own per unit of work and company instead, which the call's span links to, and a company's handshake and tool list carry none |
| Exit codes | a stable table a Kubernetes `podFailurePolicy` can branch on; see [CLI, environment and exit codes](cli.md) |

[Events and metrics reference](telemetry-reference.md) lists every event and
series.

## Documents, webhooks and other formats

| Standard | Where |
|---|---|
| [instruction.md](https://github.com/instruction-md/specification), the Instruction Specification, version 1, registry revision 1.1 | instruction documents: parsing, delivery, signatures and the resolution manifest |
| [AGENTS.md](https://agents.md) (no version; the Agentic AI Foundation's `agentsmd/agents.md` at `d001185d79`) | the [workspace's notes for agents](coding-agent.md#the-repositorys-agentsmd): the `AGENTS.md` files of `agent.workspace`, from its root down to the working directory, nearest last, in the system prompt as text below the instruction |
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
