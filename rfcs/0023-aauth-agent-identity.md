# RFC 0023: AAuth [DRAFT] — agent identity for calling AAuth-protected MCP servers

**Status:** Implemented (draft support, 2026-07-09) — `--features aauth`, ships build-from-source
**Author:** Andrii Tsok
**Date:** 2026-07-09
**Part of:** the MCP client transport (RFC 0004) + the security posture (RFC 0012); references the AAuth agent/MCP guides supplied by the agentprovider team.

> **DRAFT.** AAuth itself is an evolving spec. agentd implements the **agent
> (client) side** for **Case A** (identity-based MCP) end to end, with partial
> Case B (opaque access-token adoption) and scaffolding for Case C (Person
> Server / user-scoped identity). The wire details below track the guide agentd
> was built against; they may shift as AAuth stabilizes. The feature is OFF by
> default and OMITTED from the shipped release binary — build with
> `--features aauth`.

---

## 1. Problem / Context

An MCP server can protect itself with **AAuth**: instead of a shared API key,
the calling agent holds an **Ed25519 key**, gets a short-lived **agent token**
from an **Agent Provider (apd)**, and **signs every MCP request** (RFC 9421 HTTP
Message Signatures). The server verifies the signature against the provider's
public keys and knows exactly *which agent* is calling — accountable delegation
with no shared secret and no per-request human step.

agentd is the natural client: it already funnels every MCP request through one
transport chokepoint. This RFC adds the agent-side machinery — keys, the apd
token client, and request signing — as an opt-in feature, without disturbing the
default no-shared-secret-anyway posture (agentd already prefers mTLS / rotating
bearer, RFC 0012 §3.7).

**This RFC owns:** the agent-side key/identity, the apd enroll + agent-token
client, the RFC 9421 signing applied to MCP requests, the config surface, and
the process-tree identity model. **It does not own:** the AAuth spec itself, the
MCP/resource server side (agentd is the client), or the Person-Server consent
UX (a documented roadmap item).

## 2. Where the human user sits (per the guide)

agentd is the **agent**. The human's involvement is entirely a function of the
server's access mode — and agentd **reacts** to what the server signals, it does
not choose:

- **Case A — identity-based** (implemented): the server only wants *which agent*.
  The user acts **at setup only** (enable the agent; provide a one-time
  enrollment token if the provider requires one). No per-request consent.
- **Case B — resource-managed** (partial): the server runs its own OAuth-style
  consent once and hands back an opaque token; agentd **adopts** an
  `AAuth-Access` token and presents it on later calls. The interactive first
  consent is out of agentd's request loop (a human/gateway concern).
- **Case C — Person-Server / user-scoped** (scaffolded): the server wants the
  *human behind the agent*. agentd enrolls the `ps` claim; the interactive
  consent round-trip (`401 requirement=auth-token` → PS approve/deny) is a
  **roadmap item** — today it surfaces as a clear error, not a PS flow.

In steady state (all cases) the user is not in the loop — agentd just signs.

## 3. Design (implemented)

### 3.1 The identity key (`aauth::AgentKey`)

Ed25519, via `ring` (the crypto provider rustls already links — a direct edge
under `--features aauth`, **no new crate in the graph**; the crypto exception,
as `cel-interpreter` is the expression exception). Generate → persist (32-byte
seed, base64url, 0600) → load. Exposes the public JWK (`{kty:OKP, crv:Ed25519,
x}`), the RFC 7638 thumbprint (`jkt`), and `sign(base) → 64-byte sig`.

### 3.2 The Agent Provider client (`aauth::ApdClient`)

`POST {apd}/enroll` (signed, `hwk` scheme — presents the public JWK; the agent
has no token yet) → agent identity. `POST {apd}/agent-token` (signed) → a
short-lived token, **cached** and **proactively refreshed** 60 s before expiry.
The enrollment token is a `{{secret:…}}` template resolved at use (RFC 0012 §3.7
— never inline, never logged). Fully automatic after setup; losing a token just
fetches another (no long-lived secret).

### 3.3 Request signing (`aauth::sig`, RFC 9421)

Every MCP request gets three headers:

```text
Signature-Input: sig=("@method" "@authority" "@path" "signature-key");created=<now>
Signature: sig=:<base64(ed25519(signature-base))>:
Signature-Key: sig=jwt;jwt="<agent_token>"
```

The covered set is the guide's minimum (`@method`, `@authority`, `@path`,
`signature-key`). Signing is hand-rolled string assembly + `ring` Ed25519, unit-
tested by reconstructing the base a verifier builds and checking the signature.
`created` is unix-now (the verifier's ±60 s window applies).

### 3.4 The transport seam (`::mcp::http::RequestSigner`)

The signer is a **trait in `agentd-mcp`** — `sign(method, authority, path) →
Vec<(name, value)>`, taking and returning strings only, so `agentd-mcp` gains
**no crypto dependency**. The transport calls it just before each POST and adds
the headers. `agentd::aauth::AAuthClient` implements the trait; the crypto lives
only in `agentd-core` behind `aauth`. A token-fetch failure yields no headers
(the request goes unsigned; the server answers with its requirement) rather than
wedging the transport.

### 3.5 One identity per process tree

The `AAuthClient` is process-global (installed once). It rides the **spawn
payload** to every subagent (the key file is a shared-fs path, like `--tls-ca`),
so the whole re-exec'd tree signs under **one agent identity**. The root
**primes** it at startup — enroll + first token — so an unreachable provider or
bad enrollment token fails fast (exit 4/2), not on the first MCP call.

### 3.6 What gets signed

When `--aauth-provider` is set, **every** configured MCP server is signed (the
agent has one identity; a non-AAuth server ignores the extra headers). Per-server
scoping is a possible refinement (§7). Static-bearer/mTLS auth is unaffected —
signing is additive.

## 4. Config surface

| Flag | Env | Meaning |
|---|---|---|
| `--aauth-provider <url>` | `AGENT_AAUTH_PROVIDER` | The Agent Provider — **turns AAuth on**. |
| `--aauth-key-file <path>` | `AGENT_AAUTH_KEY_FILE` | Durable Ed25519 key (created 0600 if absent; default `agent.key`). Shared-fs. |
| `--aauth-enroll-token <T>` | `AGENT_AAUTH_ENROLL_TOKEN` | One-time enrollment token (`{{secret:…}}`), provider `token` mode. |
| `--aauth-person-server <url>` | `AGENT_AAUTH_PERSON_SERVER` | Person Server (`ps`) for Case C (enrolled; consent flow is roadmap). |

All exit `2` at validation without `--features aauth`, or on a bad URL — before
any network I/O. Manifest: `surfaces.aauth = {draft:true, agent:"aauth:…"}` when
configured (never a key/token), so a fleet view sees which identity a signed
instance carries. Reserved MCP server name `code` is unrelated (RFC 0022);
AAuth reserves no names.

## 5. Security posture (RFC 0012 alignment)

- **No new secret on the wire**: the key seed is a local 0600 file; the agent
  token is short-lived and re-fetchable; the enrollment token is a secret
  reference resolved at use. None are logged.
- **The signature covers request identity**, not the body, by default — a
  `content-digest` cover is a future add (§7) for servers that require body
  integrity.
- Signing is **additive and opt-in**: a build without `aauth` has no signing
  path and no `ring` edge; a run without `--aauth-provider` signs nothing.
- The agent token is presented to **every** signed server; an operator who
  needs to withhold identity from a specific server should not route it through
  an AAuth-on agentd (per-server opt-out is §7).

## 6. Conformance & tests

Unit: base64 (RFC 4648 vectors), Ed25519 keygen/persist/reload/sign + `ring`
verify, RFC 9421 base reconstruction + verify, `hwk` JWK presentation, config
parse/validation. E2e (`aauth_e2e.rs`): the full chain against a **live mock
Agent Provider socket** — key → signed enroll → signed agent-token → cache →
request-signature headers that a verifier checks against the enrolled public
key, plus cache-reuse (no second token fetch). This mirrors exactly what a real
AAuth MCP server verifies.

## 7. Deferred (roadmap)

- **Case C interactive consent**: the `401 requirement=auth-token` → Person
  Server approve/deny/clarify round-trip, and presenting the resulting
  user-scoped **auth token** as the `Signature-Key` instead of the agent token.
  The `ps` claim is already enrolled; the resume-after-key-refresh dance
  (guide §6) comes with it.
- **`202 requirement=interaction`** (elicitation/HITL): drive the user to the
  URL + poll `Location`. Natural fit with the RFC 0021 `human` gate.
- **Discovery**: read `/.well-known/aauth-resource.json` to pick the case up
  front (agentd currently signs proactively and reacts to the runtime
  requirement).
- **`content-digest` covering** for servers that require body integrity.
- **Per-server AAuth opt-in/out** (a `--mcp` tag or config-file `aauth: bool`),
  replacing today's sign-all-when-configured.
- **AAuth Events** (`/inbox` polling) for async tool results.
- Shipping in the release binary once the draft stabilizes (today: build from
  source, like `cel`).
