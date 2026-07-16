# RFC 0023: AAuth [DRAFT] — agent identity for calling AAuth-protected MCP servers

**Status:** Implemented (draft support, 2026-07-09) — `--features aauth`, **ships in the release binary** (zero marginal dep: the crypto is already linked by the `tls` transport)
**Author:** Andrii Tsok
**Date:** 2026-07-09
**Part of:** the MCP client transport (RFC 0004) + the security posture (RFC 0012); references the AAuth agent/MCP guides supplied by the agentprovider team.

> **DRAFT SPEC.** AAuth itself is an evolving spec, so the feature is labelled
> `[draft]`. Unlike `cel` (which pulls a real dependency and stays
> build-from-source), the `aauth` feature is **in the default release feature
> set** — its crypto (`ring`) is the same crate rustls already links for the
> `tls` transport, so shipping it adds **no new crate to the graph**. It stays a
> compile-time feature (a trimmed build can drop it). The agent-side
> implementation is **complete**: **Case A** (identity-based), **Case B**
> (resource-managed access-token adoption), and **Case C** (Person-Server /
> user-scoped identity) all run end to end — plus discovery, content-digest
> covering, **federated (assertion) enrollment**, and **signing the
> intelligence dial**. The wire details track the guide agentd was built
> against; they may shift as AAuth stabilizes.

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
MCP/resource server side (agentd is the client), or the Person-Server's own
consent UX (the human approves *at their PS*; agentd only drives the exchange).

## 2. Where the human user sits (per the guide)

agentd is the **agent**. The human's involvement is entirely a function of the
server's access mode — and agentd **reacts** to what the server signals, it does
not choose:

- **Case A — identity-based**: the server only wants *which agent*. The user
  acts **at setup only** (enable the agent; provide a one-time enrollment token
  if the provider requires one). No per-request consent.
- **Case B — resource-managed**: the server runs its own OAuth-style consent
  once and hands back an opaque token; agentd **adopts** the `AAuth-Access`
  token (from the response) and presents it on the retry + later calls. The
  interactive first consent is out of agentd's request loop (a human/gateway
  concern).
- **Case C — Person-Server / user-scoped**: the server wants the *human behind
  the agent*. On `401 requirement=auth-token`, agentd exchanges the resource
  token at the user's Person Server (the human approves there — the exchange
  carries a justification), receives the user-scoped auth token, and presents
  it on the retry. The user's active steps are: one-time authorize the agent at
  their PS, and approve/deny (and optionally clarify) per new scope.

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
Fully automatic after setup; losing a token just fetches another (no long-lived
secret).

**Enrollment gates.** The enroll body carries whatever the provider's gate
needs: `token` mode presents a one-time enrollment token (a `{{secret:…}}`
template resolved at use — RFC 0012 §3.7, never inline, never logged);
**`federated` mode** presents an `enrollment_assertion` read from
`--aauth-enroll-assertion-file` (e.g. a Kubernetes projected ServiceAccount
token). The assertion is **re-read fresh on every enroll** — projected tokens
rotate, so the path (not the value) rides config and the spawn payload, and the
short-lived assertion never touches config or logs. Federated is the secret-free
fleet path: each pod presents its own platform identity, so there is no shared
enrollment secret.

### 3.3 Request signing (`aauth::sig`, RFC 9421)

Every MCP request gets three headers:

```text
Signature-Input: sig=("@method" "@authority" "@path" "signature-key");created=<now>
Signature: sig=:<base64(ed25519(signature-base))>:
Signature-Key: sig=jwt;jwt="<agent_token>"
```

The covered set is the guide's minimum (`@method`, `@authority`, `@path`,
`signature-key`), plus `content-digest` when discovery says the server requires
body integrity. Signing is hand-rolled string assembly + `ring` Ed25519, unit-
tested by reconstructing the base a verifier builds and checking the signature.
`created` is unix-now (the verifier's ±60 s window applies).

### 3.4 The transport seam + reaction loop (`::mcp::http::RequestSigner`)

The signer is a **trait in `agentd-mcp`** — `sign(method, authority, path, body)
→ Vec<(name, value)>` and `on_response(AuthResponse, authority) → bool`, taking
and returning strings only, so `agentd-mcp` gains **no crypto dependency**. The
transport runs the RFC 0023 §5 **request loop**: sign → send → if the response
carries an `AAuth-Requirement`/`AAuth-Access`/`401`/`202`, call `on_response`
(which adopts an access token or runs the Person-Server exchange) → if it
returns `true`, re-sign (now presenting the new token) and retry — bounded
(3 attempts) so a mis-satisfied requirement cannot spin.
`agentd::aauth::AAuthClient` implements the trait; the crypto lives only in
`agentd-core` behind `aauth`. A token-fetch failure yields no headers (the
request goes unsigned; the server answers with its requirement).

### 3.5 One identity per process tree

The `AAuthClient` is process-global (installed once). It rides the **spawn
payload** to every subagent (the key file is a shared-fs path, like `--tls-ca`),
so the whole re-exec'd tree signs under **one agent identity**. The root
**primes** it at startup — enroll + first token — so an unreachable provider or
bad enrollment token fails fast (exit 4/2), not on the first MCP call.

### 3.6 What gets signed (per-server opt-in + the intel dial)

When `--aauth-provider` is set, **every** configured MCP server is signed by
default (the agent has one identity; a non-AAuth server ignores the extra
headers). A specific server opts **out** with `aauth: false` on its `--mcp`
config-file entry — useful to withhold identity from a server that should not
learn who the agent is. Static-bearer/mTLS auth is unaffected — signing is
additive.

The **intelligence dial** is signed too: agentd's LLM client applies the same
RFC 9421 headers to its requests to the `--intelligence` endpoint (both the chat
POST and the `/v1/models` discovery GET), so a model gateway can attest the
individual agent by signature instead of source IP (agentctl RFC 0024 §7.1).
Identity-cover only (no content-digest for the intel dial in this cut); the
endpoint's bearer, if any, still rides alongside.

## 4. Config surface

| Flag | Env | Meaning |
|---|---|---|
| `--aauth-provider <url>` | `AGENT_AAUTH_PROVIDER` | The Agent Provider — **turns AAuth on**. |
| `--aauth-key-file <path>` | `AGENT_AAUTH_KEY_FILE` | Durable Ed25519 key (created 0600 if absent; default `agent.key`). Shared-fs. |
| `--aauth-enroll-token <T>` | `AGENT_AAUTH_ENROLL_TOKEN` | One-time enrollment token (`{{secret:…}}`), provider `token` mode. |
| `--aauth-enroll-assertion-file <path>` | `AGENT_AAUTH_ENROLL_ASSERTION_FILE` | Enrollment assertion file (projected SA token / JWS), provider `federated` mode — re-read fresh on every enroll. |
| `--aauth-person-server <url>` | `AGENT_AAUTH_PERSON_SERVER` | Person Server (`ps`) for Case C — the resource-token → user-scoped auth-token exchange. |

All exit `2` at validation without `--features aauth`, or on a bad URL — before
any network I/O. Manifest: `surfaces.aauth = {draft:true, agent:"aauth:…"}` when
configured (never a key/token), so a fleet view sees which identity a signed
instance carries. Reserved MCP server name `code` is unrelated (RFC 0022);
AAuth reserves no names.

## 5. Security posture (RFC 0012 alignment)

- **No new secret on the wire**: the key seed is a local 0600 file; the agent
  token is short-lived and re-fetchable; the enrollment token is a secret
  reference resolved at use. None are logged.
- **The signature covers request identity** by default; when discovery
  (`/.well-known/aauth-resource.json`) says a server requires body integrity,
  the signature **also covers `content-digest`** (RFC 9530 SHA-256 of the body).
- Signing is **additive and opt-in**: a build without `aauth` has no signing
  path and no `ring` edge; a run without `--aauth-provider` signs nothing.
- The agent token is presented to **every** signed server by default; an
  operator withholds identity from a specific server with `aauth: false` on its
  `--mcp` entry (§3.6).

## 6. Conformance & tests

Unit: base64 (RFC 4648 vectors), Ed25519 keygen/persist/reload/sign + `ring`
verify, RFC 9421 base reconstruction + verify (identity-only **and**
`content-digest`-covering), `hwk` JWK presentation, Person-Server resource-token
parse, config parse/validation. E2e:

- `aauth_e2e.rs` (**Case A**): the full chain against a **live mock Agent
  Provider socket** — key → signed enroll → signed agent-token → cache →
  request-signature headers that a verifier checks against the enrolled public
  key, plus cache-reuse (no second token fetch).
- `aauth_flow_e2e.rs` (**Case C**, over the real transport): a real `McpClient`
  with the AAuth signer against a live mock apd + Person Server + AAuth MCP
  server. The first signed `tools/call` gets `401 requirement=auth-token`; the
  transport reaction loop runs the PS exchange (carrying a justification),
  caches the user-scoped auth token, re-signs presenting it, and the retry
  returns the protected result — the whole loop inside one `call_tool`.
- `aauth_enroll_assertion_e2e.rs` (**federated enroll**): two fresh clients read
  the same assertion file across a rotation; each enroll body carries the file's
  current contents — proving the assertion is read *at enroll*, not cached.
- `aauth_intel_sign_e2e.rs` (**signed intel dial**): a real `IntelClient` with
  the signer installed dials a mock LLM that asserts the RFC 9421 headers
  arrived (and the bearer still rides alongside).

Together these mirror exactly what a real AAuth MCP server / model gateway
verifies across all three access modes and both signed surfaces.

## 7. Deferred (roadmap)

The agent-side loop (Case A/B/C, discovery, content-digest, per-server opt-in,
federated enrollment, signing the intel dial) is done and ships in the release
binary. What remains:

- **`202 requirement=interaction`** (elicitation/HITL): drive the user to the
  URL + poll `Location`. The Person-Server exchange already polls an interaction
  URL for the async-approval case; extending it to a server's own `202` and
  wiring it to the RFC 0021 `human` gate is the remaining step.
- **AAuth Events** (`/inbox` polling) for async tool results.
- **Content-digest on the intel dial** — identity-cover only today; add body
  integrity if a model gateway requires it.
- **Signing the A2A client** (peer delegation is bearer/mTLS-only today —
  agentctl RFC 0024 §8).

### 7.1 Bootstrap-draft gap analysis (2026-07-16)

Reviewed against `draft-hardt-aauth-bootstrap-01`. That draft is *informational*
guidance for **Agent Provider** and web/mobile **client-platform** implementers;
much of it (Secure Enclave / StrongBox, WebAuthn / App Attest / Play Integrity,
WebCrypto / IndexedDB, ephemeral browser keys) is platform machinery that does
not apply to a headless server agent. agentd is conformant for the hosted-AP
wiring a *workload* agent needs. The applicable gaps, with disposition:

- **G4 — Agent-token claim awareness** (**DONE**, see §Step 2): the token is a
  JWT; parse it best-effort to (a) drive refresh off its own `exp` and (b) fail
  fast if `cnf.jwk` ≠ the signing key (a mismatch would otherwise be a silent
  downstream 401 storm). Backward-compatible: a non-JWT/opaque token falls back
  to `expires_in`.
- **G1 — AP metadata discovery + issuer validation** (**DONE**): at prime,
  `discover::fetch_agent_provider` fetches `/.well-known/aauth-agent.json`
  (protocol §12.10.1) and enforces the §12.10 anti-host-poisoning rule — a served
  document whose `issuer` ≠ the configured provider aborts startup. The verified
  issuer is then enforced per token: `inspect_agent_token` fails fast if the
  agent token's `iss` isn't the configured provider (ties the token to the AP we
  enrolled with). Best-effort: an AP that publishes no document still works. Note
  the enroll/token *endpoints* are deliberately NOT discovered — the protocol
  keeps them out of metadata (informational bootstrap conventions), so the
  hardcoded `/enroll` + `/agent-token` paths are correct.
- **G5 — Person-Server (`ps`) claim** (**DONE**): the *normative* protocol
  (§5.2.1) makes `ps` a **per-agent-instance** claim ("configured per agent
  instance"), enrolled from `person_server` config — NOT the per-request
  targeting the informational bootstrap draft sketched. agentd already enrolls
  it; the added delta is validation: `inspect_agent_token` now fails fast if the
  token's `ps` claim ≠ the configured person server (a wrong `ps` would route the
  Case-C exchange to the wrong PS). Per-*request* `ps` targeting is a non-goal
  (one PS per agent instance is the protocol model).
- **G2 — Self-hosted mode** (deferred, low priority): publish an
  `/.well-known/aauth-agent.json` + JWKS and self-issue tokens (be your own AP),
  for on-prem/air-gapped deploys with no separate provider.
- **G3 — Two-key durable/ephemeral split + `jkt-jwt` scheme** (deferred —
  **contingent on G6**): a durable enrollment anchor + a fresh per-token ephemeral
  request-signing key (the token's `cnf.jwk`), the token request signed with the
  `jkt-jwt` Signature-Key scheme (a durable-signed JWT naming the ephemeral in
  `cnf`), per draft-hardt-httpbis-signature-key-07. Exact wire format is now
  known and implementable: `Signature-Key: sig=jkt-jwt;jwt="<JWT>"`, JWT header
  `{typ:"jkt-s256+jwt", alg:"EdDSA", jwk:<durable pub>}`, payload
  `{iss:"urn:jkt:sha-256:<durable thumbprint>", iat, exp, cnf:{jwk:<ephemeral pub>}}`,
  durable signs the JWT, ephemeral signs the RFC 9421 base.
  **Why still deferred:** the security benefit is realized only when the durable
  anchor lives somewhere the ephemeral does not — i.e. hardware (G6, WILL NOT
  DO). Today the durable key is a `0600` file, so the two-key split gives ~no
  marginal protection: the file leak that would expose a single key exposes the
  durable anchor either way (and the anchor's leak is the worse compromise). Both
  keys also share one process's memory. The protocol blesses single-key for
  workload agents. Revisit only alongside G6 (durable in TPM/KMS).
- **G6 — Hardware-backed / non-extractable key ("secure enclaves")**
  (**WILL NOT DO** — most likely): the durable key is a `0600` seed file, hence
  extractable. On a Linux server "Secure Enclave" means TPM 2.0 / PKCS#11-HSM /
  cloud KMS. The signing surface is already a tiny seam (`AgentKey::sign` +
  `public_jwk`), so a `Signer` trait with a KMS/TPM backend is *mechanically*
  cheap — but TPMs/most HSMs don't do Ed25519, so real hardware backing would
  force adding ES256 (P-256) across the whole wire (`hwk` crv, `cnf.jwk` kty,
  signature base). Not worth it while identities are short-lived and gated by a
  rotating workload assertion (§5.1). Revisit only if a long-lived durable anchor
  + KMS-Ed25519 (e.g. GCP, no wire change) becomes a concrete requirement.
