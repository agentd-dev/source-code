# AAuth [DRAFT] — agent identity for AAuth-protected MCP servers

> **Draft support.** AAuth is an evolving spec; agentd implements the **agent
> (client) side**. It ships **build-from-source** (`--features aauth`, like
> `cel`) — OFF by default and not in the release binary. The normative contract
> is [RFC 0023](../rfcs/0023-aauth-agent-identity.md).

Some MCP servers replace the shared API key with **AAuth**: your agent holds an
**Ed25519 key**, gets a short-lived **agent token** from an **Agent Provider**,
and **signs every MCP request** (RFC 9421). The server verifies the signature
and knows exactly which agent is calling — no shared secret, and no human on
each request.

## Turn it on

```console
$ cargo build -p agentd-cli --release --features aauth,serve-https

$ agentd \
    --instruction "…" --intelligence https://gw.example/v1 \
    --mcp secure=https://mcp.secure.example/mcp \
    --aauth-provider https://apd.example \
    --aauth-key-file /var/lib/agentd/agent.key \
    --aauth-enroll-token '{{secret:AAUTH_ENROLL}}'
```

At startup agentd loads (or creates) the key, enrolls it once, fetches its first
agent token, and logs `aauth.ready` with the resolved identity
(`aauth:…@apd.example`). From then on **every** MCP request it makes is signed —
a non-AAuth server simply ignores the extra headers.

| Flag | Env | Meaning |
|---|---|---|
| `--aauth-provider <url>` | `AGENT_AAUTH_PROVIDER` | The Agent Provider — this turns AAuth on. |
| `--aauth-key-file <path>` | `AGENT_AAUTH_KEY_FILE` | Durable Ed25519 key (created 0600 if absent; default `agent.key`). Put it on shared storage so subagents resolve the same identity. |
| `--aauth-enroll-token <T>` | `AGENT_AAUTH_ENROLL_TOKEN` | One-time enrollment token (a `{{secret:…}}` reference), if the provider is in `token` mode. |
| `--aauth-person-server <url>` | `AGENT_AAUTH_PERSON_SERVER` | Person Server for user-scoped identity (Case C — the resource-token → user auth-token exchange). |

Without `--features aauth` these flags exit `2` at validation; a bad provider
URL exits `2` too — before any network I/O.

## What agentd does on the wire

Each MCP `POST` carries three RFC 9421 headers:

```text
Signature-Input: sig=("@method" "@authority" "@path" "signature-key");created=<now>
Signature: sig=:<base64 ed25519 over the signature base>:
Signature-Key: sig=jwt;jwt="<your agent token>"
```

The agent token is fetched and cached automatically, refreshed shortly before it
expires. There is nothing to rotate by hand — losing a token just fetches a new
one. The whole agent process tree (root + every subagent) signs under **one**
identity, inherited via the spawn payload like `--tls-ca`.

**agentd reacts to what a server asks for.** If, at connect, discovery
(`/.well-known/aauth-resource.json`) says the server requires body integrity,
the signature additionally covers a `Content-Digest` (SHA-256 of the body). If a
response carries an `AAuth-Access` token (Case B, resource-managed), agentd
adopts it and presents `Authorization: AAuth …` on the retry and later calls. If
a response is `401 requirement=auth-token` (Case C, user-scoped), agentd runs the
Person-Server exchange and presents the resulting user auth-token — all inside
the same request, bounded so a mis-satisfied requirement can't spin.

## Where the human sits

You (or your operator) act **at setup only** for the common case (identity-based
servers, "Case A"): enable the agent and, if the provider requires it, hand over
a one-time enrollment token. After that the agent operates autonomously — it
signs every call; no per-request consent.

Servers that want the *human's* identity (user-scoped, "Case C") route through a
**Person Server** where the user approves new authority. On a
`401 requirement=auth-token`, agentd exchanges the server's resource token at the
configured `--aauth-person-server` (carrying a justification the human sees),
receives the user-scoped auth-token, and presents it on the retry — the human
consents *at their PS*, not in agentd. See RFC 0023 §2 for the full
user-responsibility breakdown per case.

## Discovery in the manifest

`agentd --capabilities` surfaces `surfaces.aauth = { "draft": true, "agent":
"aauth:…" }` when configured (never the key or token) — so a fleet view can see
which identity a signed instance carries. Absent on a stock build.

## Embedding

An embedder building on `agentd-core` can drive AAuth directly:
`agentd::aauth::{AgentKey, ApdConfig, AAuthClient}` — construct a client, install
it (`agentd::aauth::install`), and every MCP connection agentd makes signs. The
signer is the `agentd::aauth::RequestSigner` trait; `verify_ed25519` is exposed
for the server side of a test. See [embedding.md](embedding.md) and RFC 0023 §3.

## What's covered (and what isn't)

All three access modes run end to end:

- **Case A** (identity-based) — sign every request; the common case.
- **Case B** (resource-managed) — adopt a returned `AAuth-Access` token and
  present it on the retry + later calls.
- **Case C** (user-scoped) — run the Person-Server exchange on
  `401 requirement=auth-token` and present the user auth-token.

Plus **discovery** (`/.well-known/aauth-resource.json`), **content-digest**
covering when a server requires body integrity, and **per-server opt-out**
(`aauth: false` on a `--mcp` config entry).

Still on the [roadmap](../rfcs/0023-aauth-agent-identity.md#7-deferred-roadmap):
a server's own `202 requirement=interaction` (HITL elicitation), AAuth Events
(`/inbox`) for async results, and shipping in the release binary once the draft
stabilizes.
