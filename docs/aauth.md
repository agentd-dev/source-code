# AAuth [DRAFT] — agent identity for AAuth-protected MCP servers

> **Draft support, but shipped.** AAuth is an evolving spec; agentd implements
> the **agent (client) side**. As of the current release the `aauth` feature is
> **in the release binary and the published container image** — its crypto
> (`ring`) is already linked by the default `tls` (rustls) transport, so
> enabling it adds **zero** marginal dependency (unlike `cel`, which stays
> build-from-source because it pulls a real dep). It remains a compile-time
> feature — a `--no-default-features` / trimmed build can omit it — and is still
> labelled `[draft]`. The normative contract is
> [RFC 0023](../rfcs/0023-aauth-agent-identity.md).

Some MCP servers replace the shared API key with **AAuth**: your agent holds an
**Ed25519 key**, gets a short-lived **agent token** from an **Agent Provider**,
and **signs every MCP request** (RFC 9421). The server verifies the signature
and knows exactly which agent is calling — no shared secret, and no human on
each request.

## Turn it on

The published release binary and container image already carry the feature, so
just pass the flags:

```console
$ agentd \
    --instruction "…" --intelligence https://gw.example/v1 \
    --mcp secure=https://mcp.secure.example/mcp \
    --aauth-provider https://apd.example \
    --aauth-key-file /var/lib/agentd/agent.key \
    --aauth-enroll-token '{{secret:AAUTH_ENROLL}}'
```

(A trimmed build that dropped the feature rebuilds it with
`cargo build -p agentd-cli --release --features aauth`.)

At startup agentd loads (or creates) the key, enrolls it once, fetches its first
agent token, and logs `aauth.ready` with the resolved identity
(`aauth:…@apd.example`). From then on **every** MCP request — and, when
configured, the **intelligence dial** — is signed; a non-AAuth server simply
ignores the extra headers.

| Flag | Env | Meaning |
|---|---|---|
| `--aauth-provider <url>` | `AGENT_AAUTH_PROVIDER` | The Agent Provider — this turns AAuth on. |
| `--aauth-key-file <path>` | `AGENT_AAUTH_KEY_FILE` | Durable Ed25519 key (created 0600 if absent; default `agent.key`). Put it on shared storage so subagents resolve the same identity. |
| `--aauth-enroll-token <T>` | `AGENT_AAUTH_ENROLL_TOKEN` | One-time enrollment token (a `{{secret:…}}` reference), if the provider is in `token` mode. |
| `--aauth-enroll-assertion-file <path>` | `AGENT_AAUTH_ENROLL_ASSERTION_FILE` | **Federated** enrollment: a file holding an enrollment assertion — e.g. a Kubernetes projected ServiceAccount token whose audience is the provider. Re-read fresh on every enroll (so a rotated token is always current); the assertion never touches config or logs. |
| `--aauth-person-server <url>` | `AGENT_AAUTH_PERSON_SERVER` | Person Server for user-scoped identity (Case C — the resource-token → user auth-token exchange). |

Without `--features aauth` these flags exit `2` at validation; a bad provider
URL exits `2` too — before any network I/O.

**Enrollment modes.** `open` (nothing), `token` (`--aauth-enroll-token`, a
one-time secret), and `federated` (`--aauth-enroll-assertion-file`, a
platform-issued assertion). Federated is the secret-free fleet path: each pod
presents its own projected identity token, so there is no shared enrollment
secret and no operator-custodied key — the provider verifies the assertion and
binds it to the agent's public key.

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

**The intelligence dial is signed too.** When an identity is installed, agentd
signs its requests to the `--intelligence` endpoint with the same RFC 9421
headers. This lets a model gateway attest the *individual agent* by signature
instead of source IP — the inbound side of the identity story (agentctl RFC
0024 §7.1). A plain LLM endpoint ignores the headers, and the endpoint's bearer
token (if any) still rides alongside — signing is additive.

**agentd reacts to what a server asks for.** If, at connect, discovery
(`/.well-known/aauth-resource.json`) says the server requires body integrity,
the signature additionally covers a `Content-Digest` (SHA-256 of the body). If a
response carries an `AAuth-Access` token (Case B, resource-managed), agentd
adopts it and presents `Authorization: AAuth …` on the retry and later calls. If
a response is `401 requirement=auth-token` (Case C, user-scoped), agentd runs the
Person-Server exchange and presents the resulting user auth-token — all inside
the same request, bounded so a mis-satisfied requirement can't spin.

**agentd validates its provider and its token.** At startup it fetches the
Agent-Provider metadata document (`/.well-known/aauth-agent.json`) and enforces
the AAuth protocol's anti-host-poisoning rule: a document whose `issuer` isn't
the configured provider aborts enrollment. A provider that publishes no document
still works (best-effort). The agent token itself is then acted on, not treated
as opaque: agentd refreshes against the token's own `exp`, and fails fast if the
token's `iss` isn't the configured provider, its `ps` isn't the configured
Person Server, or its `cnf.jwk` isn't the signing key — each a misconfiguration
that would otherwise surface as a silent wall of downstream `401`s.

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

Plus **resource discovery** (`/.well-known/aauth-resource.json`) and
**provider discovery + issuer validation** (`/.well-known/aauth-agent.json`),
**agent-token claim validation** (refresh off the token's `exp`; fail fast on an
`iss` / `ps` / `cnf` mismatch), **content-digest** covering when a server
requires body integrity, **per-server opt-out** (`aauth: false` on a `--mcp`
config entry), **federated enrollment** (`--aauth-enroll-assertion-file`),
**signing the intelligence dial**, and **shipping in the release binary** (zero
marginal dep — the crypto is already linked by the `tls` transport).

Still on the [roadmap](../rfcs/0023-aauth-agent-identity.md#7-deferred-roadmap):
a server's own `202 requirement=interaction` (HITL elicitation) and AAuth Events
(`/inbox`) for async results.
