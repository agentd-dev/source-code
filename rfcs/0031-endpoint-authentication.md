# RFC 0031: Endpoint Authentication — Interactive & Workload Credential Providers

**Status:** Implemented (behind the `oauth` / `tls` features)
**Author:** agentd
**Date:** 2026-08-16
**Part of:** extends RFC 0023 (AAuth workload identity) and RFC 0012 (no local execution / secret handling); builds on RFC 0030 (config schema v2), RFC 0025 (durable state), RFC 0026 (reactor lifecycle).

---

## 1. Summary

agentd talks to three families of outbound endpoints that require credentials:
**intelligence** (LLM) endpoints, **MCP** servers, and **A2A** peers. Today each
supports a *static* credential only (a `{{secret:…}}` bearer/header, plus mTLS for
A2A peers and the feature-gated AAuth request-signer for MCP/intel). That covers
cloud deployments with pre-provisioned secrets, but not two things users need:

1. **Interactive login** — a human runs agentd and must complete an OAuth /
   enterprise login (e.g. **AWS Bedrock via IAM Identity Center**, Azure AD, Okta,
   Google) before agentd may call the endpoint.
2. **Workload identity** — a headless/cloud agentd authenticates with *no secret*
   via **SPIFFE/SPIRE SVIDs**, cloud instance identity (IMDS/IRSA), or OAuth
   client-credentials.

This RFC introduces one abstraction — a per-endpoint **`CredentialProvider`** — that
unifies these behind a single config surface (`auth:`), a **durable, auto-refreshing
token cache**, and a **fail-closed** interactive flow with a **device-authorization**
default and an `agentd login` pre-flight command. It reuses the existing
`RequestSigner`/challenge-retry precedent and the `ring` crypto dependency (already
present behind the `aauth` feature), so the default build's three-dependency moat is
unchanged.

Non-goals: agentd remains a **client** — it does not become an OAuth authorization
server or an identity provider. Inbound auth (A2A principals, RFC 0029) is unchanged
except where noted (§10.3).

## 2. Motivation — two contexts, one rule

| Context | Who is present | Credential source | Interactive? |
|---|---|---|---|
| **Headless / cloud** | no human | static secret (env/file), **workload identity** (SPIFFE, IMDS/IRSA), OAuth client-credentials, AAuth | never |
| **Interactive / local** | a human at a terminal | an OAuth/enterprise login the human completes once, then a cached (refreshable) token | yes, once |

**The rule that ties them together: fail-closed, never block a daemon on a human.**
A daemon that needs a credential it cannot obtain non-interactively **exits with a
clear instruction** (`run: agentd login <target>`) rather than hanging. A foreground
run on a TTY may complete the flow inline. This preserves the "a job either runs to
completion or exits" contract (RFC 0026) and the "no surprising blocking" posture.

## 3. The current surface (what we build on)

Grounded in the code as of this RFC:

- **Intelligence:** one static credential per endpoint — OpenAI `Authorization:
  Bearer`, Anthropic `x-api-key` — injected per-request per dialect
  (`intel/endpoints.rs`, `intel/openai.rs`, `intel/anthropic.rs`), resolved once at
  `IntelClient::from_parts` from `intelligence.token`/`token_file` or
  `AGENT(D)_INTELLIGENCE_TOKEN[_N][_FILE]`. **No per-endpoint auth sub-config; the
  parsed `intelligence.headers` never reaches the wire** (a latent gap this RFC
  fixes). TLS is server-auth only; mTLS was deferred ("Phase E", `intel/client.rs`).
- **MCP:** live outbound auth is **static resolved headers only**
  (`mcp/auth.rs::resolve_headers` → `crates/mcp/src/http.rs`). OAuth 2.1
  **client-credentials** is fully implemented and tested (`mcp/oauth.rs`,
  `mcp_oauth.rs`) **but unwired** — `McpServer::to_spec()` drops the `oauth` block and
  `OAuthClient` has no callers. A **`RequestSigner` trait with `on_response`** already
  exists (`crates/mcp/src/http.rs`) and drives a 401/`requirement`/access-token
  **challenge-retry** loop — the precedent this RFC generalizes. mTLS client-cert
  exists in the transport but is unwired for MCP.
- **A2A:** outbound `PeerAuth { headers, identity(mTLS) }` is fully wired
  (`mcp/a2a_client.rs`, built in `runtime/waits.rs`). Inbound classification and the
  `Resolver` principal matrix are RFC 0029. Outbound A2A carries no AAuth signature.
- **AAuth (RFC 0023):** a working, feature-gated, **client-only** Ed25519 RFC-9421
  signer for outbound MCP (all three cases), with `.well-known/aauth-*` discovery and
  `exp`/`iss`/`ps`/`cnf` checks — but its **token cache is in-memory only**
  (re-enrolls on restart) and it does not cover A2A. Uses `ring` (feature `aauth`).
- **SPIFFE/SPIRE:** none (only example `spiffe://` SANs in docs/tests).
- **Secrets & store:** `sec/secret.rs::resolve` materializes `{{secret:…}}` at
  moment-of-use; there is **no credential/token store**. `state::Durable` offers a
  `Kind`-keyed durable KV (`Kind::Memory` used by the goal watchdog and webhook
  idempotency) — the natural home for a token cache.

## 4. The `CredentialProvider` model

One trait, resolved **per endpoint** (each intelligence endpoint, each MCP server,
each A2A peer), generalizing today's `RequestSigner`:

```text
trait CredentialProvider {
    /// Produce the credential to present now, refreshing/loading as needed.
    /// Cheap when a cached credential is still valid.
    fn credential(&self, ctx: &EndpointCtx) -> Result<Credential, AuthError>;

    /// React to a challenge (e.g. 401 + WWW-Authenticate / AAuth requirement):
    /// discover metadata, step up, and ask the caller to retry — or give up.
    fn on_challenge(&self, resp: &AuthResponse) -> ChallengeOutcome;

    /// Whether obtaining this credential needs a human (drives fail-closed).
    fn interactivity(&self) -> Interactivity; // NonInteractive | InteractiveOnce
}

enum Credential {
    Headers(Vec<(String, String)>),   // Authorization / x-api-key / custom
    Mtls(ClientIdentity),             // X.509 client identity (rustls)
    Signer(Box<dyn RequestSigner>),   // per-request signing (SigV4, AAuth 9421)
    None,
}
```

`Credential::Headers`/`Mtls`/`Signer` map exactly onto the three attachment points
that already exist in the transports (resolved headers; `ClientIdentity`; the
`RequestSigner` hook). A provider carries an **expiry** and a **refresh** routine; the
runtime arms a refresh timer (§9). The existing AAuth signer and the static-header
path are refactored to *be* providers, with **no behavior change** (a regression
gate).

Providers are additive-composable where it makes sense (e.g. a SPIFFE mTLS identity
*and* an OAuth bearer), matching how AAuth signing rides alongside a static bearer
today.

## 5. Config surface

A per-endpoint `auth:` block selects a provider. It is optional; the existing static
fields remain as **sugar** for `kind: static` (full backward compatibility).

```yaml
intelligence:
  endpoints: ["https://bedrock-runtime.us-east-1.amazonaws.com"]
  model: anthropic.claude-3-5-sonnet
  auth:
    kind: aws
    service: bedrock
    region: us-east-1
    source: sso                 # sso | imds | irsa | env | static
    sso: { start_url: "https://my-org.awsapps.com/start", account_id: "…", role: "AgentdBedrock" }

mcp:
  servers:
    - name: github
      endpoint: https://mcp.github.com
      auth: { kind: oauth2, grant: device, scopes: [repo, read:org] }   # discovery via RFC 9728

a2a:
  peers:
    - name: partner
      endpoint: https://a2a.partner.example
      auth: { kind: spiffe, svid: x509 }        # mTLS via SPIRE-issued identity
```

Provider `kind`s: `static` (§6), `oauth2` (§7), `aws` (§8), `spiffe` (§9), `aauth`
(§10). Backward-compat sugar:

| Existing field | Desugars to |
|---|---|
| `intelligence.token` / `token_file` | `auth: { kind: static, token: … }` |
| `mcp.servers[].headers` | `auth: { kind: static, headers: … }` |
| `mcp.servers[].oauth` | `auth: { kind: oauth2, grant: client_credentials, … }` (and **finally wired**) |
| `mcp.servers[].aauth: true` | `auth: { kind: aauth }` |
| `a2a.peers[].client_cert/key` | `auth: { kind: static, mtls: … }` |

Validation (`--validate-config`) checks provider-specific required fields and warns on
insecure combinations (e.g. a non-loopback endpoint with `kind: static` + a plaintext
token in the file). The config schema (`schema.rs`) and drift tests are extended.

**Fix carried by this work:** `intelligence` gains a real per-endpoint auth surface,
and the dropped `intelligence.headers` is threaded to the wire.

## 6. Provider: `static`

Today's behavior, made explicit. `{ token }` → `Authorization: Bearer`; `{ header,
value }` → arbitrary header; `{ mtls: { cert, key, ca? } }` → client identity. Values
are `{{secret:…}}`/`{{secret-file:…}}` references, resolved at use, never logged.
`interactivity = NonInteractive`.

## 7. Provider: `oauth2` (the interactive engine)

The general OAuth 2.1 / OIDC provider. Grants:

- **`device`** (RFC 8628) — **the interactive default** (§12). Works headless/SSH; no
  browser or open port required.
- **`authorization_code`** + **PKCE** (RFC 7636) + loopback redirect (RFC 8252) —
  opt-in (`agentd login --browser`); nicer on a desktop.
- **`client_credentials`** — non-interactive M2M (wires the existing `mcp/oauth.rs`).
- **`refresh_token`** — silent renewal for all interactive grants.

Discovery: `.well-known/openid-configuration` / `oauth-authorization-server`
(RFC 8414); for MCP, a `401` with `WWW-Authenticate` → **RFC 9728 protected-resource
metadata** → the authorization server (§ MCP spec). Config may pin `issuer`/
`token_url`/`authorization_url` to skip discovery.

```yaml
auth:
  kind: oauth2
  issuer: https://login.example.com     # or explicit *_url endpoints
  client_id: agentd
  client_secret: "{{secret:OIDC_SECRET}}"   # omit for a public client (+PKCE)
  grant: device                          # device | authorization_code | client_credentials
  scopes: [llm.invoke, offline_access]
  audience: https://api.example.com      # optional
```

`interactivity = InteractiveOnce` for `device`/`authorization_code` (unless a valid
cached/refresh token exists); `NonInteractive` for `client_credentials`.

Crypto: token requests are HTTP form-POST + JSON (dep-free). ID-token / JWT
**signature verification** (RS256/ES256/EdDSA) reuses **`ring`** behind the auth
feature; PKCE is SHA-256 (already present). State/nonce are random (CSRF/replay).

## 8. Provider: `aws` (Bedrock, and the SSO device flow)

AWS is not plain OAuth: requests are **SigV4-signed** with AWS credentials, and the
"enterprise login" is **AWS IAM Identity Center (SSO)**, which *is* an OIDC device
flow that yields temporary AWS credentials.

- **SigV4 signing** — canonical request + HMAC-SHA256 signing key derivation (the
  exact HMAC-SHA256 primitive already in `sha.rs`). Emitted as a `RequestSigner`
  credential; **zero new dependency**.
- **Credential `source`:**
  - `static` / `env` — access key / secret / session token from `{{secret:…}}` / the
    standard `AWS_*` env.
  - `imds` (EC2 instance role) / `irsa` (EKS `AWS_WEB_IDENTITY_TOKEN_FILE` +
    AssumeRoleWithWebIdentity) — **headless workload identity**, no secret.
  - `sso` — **the interactive path**: `RegisterClient` → `StartDeviceAuthorization`
    → print code+URL, poll `CreateToken` (the §12 device UX) → `GetRoleCredentials`
    for temporary keys. Refreshes via the cached SSO token until it expires, then
    re-prompts.

Temporary credentials (and their expiry) are cached durably (§9). Azure AD (client
credentials / device) and Google Vertex (service-account JWT / metadata) follow the
same shape as later providers; SigV4 + SSO is the first enterprise target.

## 9. Provider: `spiffe` (zero-secret workload identity)

Two delivery modes, MVP first to preserve the dependency moat:

- **File-SVID (MVP, dep-free):** SPIRE (via the SPIFFE Helper or a CSI driver) writes
  the SVID to files; agentd **watches** `svid.pem`/`key.pem`/`bundle.pem`
  (reusing the config-watch machinery) and uses them as a rustls client identity
  (`svid: x509`) or reads a `jwt_svid` file. No socket, no gRPC.
- **Workload API socket (follow-up, feature-gated):** fetch X509-SVID / JWT-SVID from
  the SPIFFE Workload API over the `SPIFFE_ENDPOINT_SOCKET` Unix socket. This is gRPC
  (protobuf/HTTP-2); implemented behind a `spiffe` feature (a minimal client for just
  `FetchX509SVID`/`FetchJWTSVID`, or an optional dep) so the default build is
  untouched.

Usage:
- **X509-SVID** → mTLS client identity to MCP/A2A/intel endpoints (rustls path already
  exists for A2A peers; generalized to all endpoints).
- **JWT-SVID** → presented as a bearer, or **exchanged** (RFC 8693 token exchange) at
  an STS for a target-scoped access token (e.g. to obtain a cloud token). Verification
  reuses `ring`.

Inbound: the A2A `Resolver` already has a `san`/`aauth_agent` matcher shape; surfacing
the client-cert SAN so a `spiffe://…` SVID authenticates an inbound peer is tracked in
§10.3.

## 10. Provider: `aauth`, and AAuth improvements

`kind: aauth` selects the existing RFC 0023 signer as a provider. Three concrete
improvements fall out of this work:

1. **Durable token cache (§9 store):** the AAuth agent token moves from the in-memory
   `Cached{good_until:Instant}` to the durable cred cache, so a restart does not
   re-enroll while a token is still valid.
2. **A2A coverage:** the AAuth signer becomes attachable to **outbound A2A** (not just
   MCP), via the same `Credential::Signer` path.
3. **Inbound identity (RFC 0029 gap):** surface the verified client-cert **SAN/subject
   and AAuth agent** from the serve framework so the `san`/`sub`/`aauth_agent`
   principal matchers bind real mTLS/SVID/AAuth evidence (today mTLS conveys
   operator-only). This is the one inbound change; it is additive and gated.

## 11. The durable, auto-refreshing token cache

A new durable record class **`Kind::Cred`** on `state::Durable` (or `Kind::Memory`
under a `_cred/` prefix), keyed by a hash of `(endpoint, provider, principal)`. Value:

```json
{ "access_token": "…", "refresh_token": "…", "expires_at_ms": 0, "extra": { … } }
```

- **Redaction:** the cred cache is excluded from all logs, audit, and the `agent://`
  read surface, exactly like `intelligence.token`. Values are never printed.
- **Refresh:** the reactor arms a **refresh timer** per credential (the same
  nearest-deadline mechanism the goal watchdog and schedules use), renewing
  ~60 s before `expires_at_ms` via the provider's refresh routine. A refresh failure
  degrades to the fail-closed rule (§2).
- **Restart:** on startup the cache is restored; still-valid tokens (and refresh
  tokens) mean no re-login. This directly upgrades AAuth's current re-enroll-on-restart
  behavior.
- **Encryption at rest** is delegated to the store backend (an operator using an
  `mcp`/`http` store to a secrets-aware backend). agentd does not roll its own
  envelope encryption in this RFC (noted as an open question, §16).

## 12. Interactive UX — device code + `agentd login`

**Device-authorization grant is the default** (user-selected). When a credential is
needed and interactivity is permitted:

```
┌─ authorize agentd ─────────────────────────────────┐
│  endpoint  mcp:github                              │
│  visit     https://github.com/login/device         │
│  code      WDJB-MJHT                               │
│  waiting…  (expires in 15:00 · Ctrl-C to cancel)   │
└────────────────────────────────────────────────────┘
```

agentd polls the token endpoint at the server's `interval`, handling
`authorization_pending`/`slow_down`, until it receives tokens → caches them (§11).

**The command surface:**

- `agentd login <target>` — run the flow for a named endpoint and cache the result.
  `<target>` = `intelligence`, `mcp:<name>`, or `a2a:<name>`. `--browser` selects
  authorization-code + PKCE + loopback instead of device code.
- `agentd login --all` — every endpoint whose provider is interactive and uncached.
- `agentd login --list` — show each endpoint, its provider, and cache status
  (valid / expiring / absent).
- `agentd logout <target>` — evict the cached credential.

**When the flow triggers:**

| Invocation | Behavior when a needed cred is absent/expired |
|---|---|
| `agentd login …` | run the flow (this is its purpose) |
| foreground `agentd` on a **TTY**, cred interactive | auto-run the device flow inline (suppress with `--no-interactive`) |
| **daemon** / non-TTY / `--no-interactive` | **fail-closed**: exit with `run: agentd login <target>` (unless a refresh token silently renews) |

The future Ink CLI (backlog) renders this as a first-class panel; the core flow lives
in the daemon so both share one implementation.

## 13. Security considerations

- **Fail-closed** (§2): a daemon never blocks on a human; missing interactive creds
  are a clean startup error, not a hang.
- **No ambient authority:** every credential is explicitly configured per endpoint;
  there is no implicit global credential. IMDS/IRSA/SPIFFE are opt-in per endpoint.
- **Secrets never inline/logged:** all secret inputs are `{{secret:…}}` references;
  the token cache is redaction-excluded; PKCE `code_verifier`, device `device_code`,
  and OAuth `state`/`nonce` are treated as secrets.
- **PKCE + state/nonce** on every interactive flow (CSRF/replay/interception defense);
  loopback redirect binds to `127.0.0.1` with a random path and a one-shot listener.
- **Token binding:** honor `cnf`/DPoP where the AS supports it (AAuth already checks
  `cnf.jwk`); prefer sender-constrained tokens for high-value endpoints.
- **TLS everywhere:** token/discovery endpoints are HTTPS-only with server-cert
  verification (the existing SSRF/TLS guards apply); no plaintext except loopback.
- **Least privilege:** `scopes`/`audience`/AWS `role` are explicit and minimal;
  validation warns on over-broad or missing scoping.

## 14. Dependencies (the moat holds)

| Capability | Crypto/transport | New default dep? |
|---|---|---|
| OAuth device / auth-code / client-creds / refresh | HTTP form-POST + JSON (`net::http`) | **no** |
| PKCE | SHA-256 (`sha.rs`) | **no** |
| SigV4 (AWS/Bedrock) | HMAC-SHA256 (`sha.rs`) | **no** |
| JWT / ID-token verify (RS256/ES256/EdDSA), token exchange | **`ring`** — already a dep behind `aauth` | **no** (feature-gated) |
| SPIFFE file-SVID (MVP) | rustls (existing `tls`) | **no** |
| SPIFFE Workload API socket | gRPC/HTTP-2 | behind a `spiffe` feature (opt-in) |

The **default three-dependency build is unchanged.** Interactive/enterprise/workload
providers live behind features (`oauth`, `aauth`, `aws`, `spiffe`) that compose with
the existing `tls` feature.

## 15. Rollout plan

> **Implementation status (2026-08-16): built and green.** All three endpoints
> (MCP, intelligence, A2A) accept the `auth:` block; all four provider kinds
> (`static`, `oauth2`, `aws` SigV4, `spiffe`) are wired. OAuth covers the **device
> grant**, **browser + PKCE** (`authorization_code`), **client-credentials**,
> refresh, and OIDC discovery — so **Azure OpenAI and Google Vertex** work through
> it directly. AWS covers **all sources** (`env`/`static`/`sso`/`imds`/`irsa`) with
> SigV4 validated against the AWS test vector. SPIFFE covers **JWT-SVID** + **X.509
> mTLS**. `agentd login`/`logout`, the file+durable cred cache, and in-memory
> refresh work; config validation, schema, `docs/authentication.md`, and tests are
> green (`cargo test --workspace --all-features`, 3-dep moat intact). Provider code
> lives behind the existing **`oauth`** feature (mTLS behind `tls`). The few
> remaining items are deliberately deferred refinements — see P5.

Each phase ends green: `cargo fmt --check`; clippy (default + all-features); `cargo
test --workspace --all-features`; the 3-dep moat intact; e2e against a **mock IdP/AS**;
`--validate-config` sample configs; docs updated. Phasing follows the selected
priorities (MCP OAuth, AWS Bedrock, SPIFFE, generic OAuth2), foundation-first:

- **P0 — Provider abstraction + cred cache. ✅ DONE.** The transport
  `RequestSigner` seam is the provider hook; `auth::device::signer_for` is the
  factory; the static path + AAuth signer coexist. `Kind::Cred` + the file/durable
  cache + in-memory refresh landed. The **inert OAuth client-credentials is
  wired** (fixed `to_spec()` dropping `oauth`) and **`intelligence.headers` reach
  the wire.** Config `auth:` block + schema + drift + back-compat sugar. *(A full
  `CredentialProvider` trait refactor of the AAuth path was skipped in favor of the
  existing seam — same effect, lower risk.)*
- **P1 — OAuth2/OIDC interactive engine. ✅ DONE.** Device grant (default) +
  refresh + RFC 8414/OIDC discovery; `agentd login`/`logout`; fail-closed daemon
  rule. Wired into **intelligence + MCP + A2A**. *(follow-up: `login --list`, TTY
  auto-inline.)*
- **P2 — MCP OAuth discovery. ✅ DONE (RFC 9728).** `agentd login mcp:<name>` with
  no configured `issuer` probes the server: the unauthenticated request draws a
  `401 WWW-Authenticate: Bearer resource_metadata="…"`, agentd fetches that
  protected-resource metadata (or the origin well-known), and the
  `authorization_servers[0]` becomes the issuer for OIDC discovery
  (`auth/challenge.rs`; e2e `mcp_rfc9728_e2e.rs`). An explicit `issuer`/`token_url`
  still wins; best-effort otherwise.
- **P3 — AWS. ✅ DONE.** SigV4 (validated against the AWS `get-vanilla` vector);
  **all sources**: `env`/`static`, **`sso`** (IAM Identity Center device flow →
  temp creds), **`imds`** (EC2 IMDSv2), **`irsa`** (EKS web identity → STS) — the
  temporary sources refetch as they near expiry (e2e-proven). **Azure OpenAI +
  Google Vertex work via the generic `oauth2` device provider** (their
  OpenAI-compatible endpoints + an Entra ID / Google bearer — documented in
  `docs/authentication.md`). **Native Bedrock ✅ DONE:** SigV4 now signs the
  intelligence dial (`intel/endpoints.rs`; e2e `intel_sigv4_e2e.rs`), and the
  `intelligence.dialect: bedrock` selector speaks the **Bedrock Converse** wire
  (`intel/bedrock.rs`) — the model id rides the URI-encoded `/model/{id}/converse`
  path that the signature covers, tool-calling and system prompts included (e2e
  `bedrock_e2e.rs`). Pair it with `auth: { kind: aws, service: bedrock }`.
- **P4 — SPIFFE. ✅ DONE (file-SVID).** JWT-SVID bearer + X.509-SVID mTLS.
  *(follow-up: the Workload-API gRPC socket, behind a `spiffe` feature.)*
- **P5 — Browser + PKCE. ✅ DONE.** `grant: authorization_code` runs the loopback
  PKCE flow (`agentd login` prints the URL, captures the redirect, verifies
  `state`, exchanges the code; e2e-proven).

  **The P5 deferred follow-ups are now resolved:**
  - **Native Bedrock ✅ DONE** (see P3): the `bedrock` dialect + SigV4-signed
    dynamic `/model/{id}/converse` path (`intel/bedrock.rs`; e2e `bedrock_e2e.rs`,
    `intel_sigv4_e2e.rs`).
  - **MCP RFC 9728 discovery ✅ DONE** (see P2): 401-challenge → protected-resource
    metadata → issuer (`auth/challenge.rs`; e2e `mcp_rfc9728_e2e.rs`).
  - **Outbound A2A SigV4 + AAuth ✅ DONE:** a peer `auth: { kind: aws }` dials with a
    per-request SigV4 signature over the exact JSON-RPC body (a `PeerAuth.signer`,
    not a baked header), and an ambient AAuth process identity signs every outbound
    A2A POST — mirroring the intelligence dial (`mcp/a2a_client.rs`,
    `runtime/waits.rs`; e2e `a2a_sigv4_e2e.rs`).
  - **Inbound mTLS SAN/subject surfacing ✅ DONE (RFC 0029 §10.3):** the serve
    framework now lifts the verified client-cert subject CN + SANs (`net/x509.rs`,
    a dependency-free DER field extractor) and threads them to `a2a.principals`, so
    a `san`/`sub` rule matches a client cert directly — a SPIFFE X.509-SVID's
    `spiffe://…` arrives as a URI SAN (`net/tls.rs`, `mcp/http_server.rs`,
    `runtime/a2a_server.rs`). An all-empty-principals listener keeps the "any
    verified cert ⇒ operator" default.
  - **Durable AAuth token cache — WON'T-DO** (deliberate non-goal). The agent
    **key** is already durable and `/enroll` is idempotent (keyed on that key), so a
    restart's only cost is one cheap signed `POST /agent-token`. The token is a
    short-lived JWT, so a persisted copy is usually already inside its refresh skew
    on the next start and gets re-fetched regardless. Persisting it would write an
    agent bearer to disk — new secret-at-rest surface — to save a round-trip that is
    normally stale. Not worth it; the in-memory cache stands. (Same disposition as
    §7.1 G6.)
  - **Remaining (one axis):** *inbound* AAuth-agent attribution — surfacing a
    verified `aauth_agent` into a principal — awaits an inbound AAuth **verifier**
    (agentd signs AAuth requests but does not yet verify them; a distinct build, not
    a §10.3 surfacing gap).

## 16. Open questions & non-goals

- **Cred-cache encryption at rest** — delegate to the store backend (this RFC), or add
  an agentd-side envelope (a `security.cred_cache.key` sealing key)? Leaning: delegate
  now, revisit if a file/memory store must hold refresh tokens.
- **Credential scope granularity** — per-endpoint is the unit here; a per-*workflow* or
  per-*principal* credential (delegated/on-behalf-of) is a possible extension via the
  cache key's `principal` component.
- **Azure/Vertex** enterprise providers — same model, sequenced after AWS.
- **Non-goal:** agentd does not become an AS/IdP, does not store long-lived user
  passwords, and does not proxy third-party credentials between endpoints beyond the
  explicit RFC 8693 exchange.

---

*This RFC plans the work; implementation is sequenced after the workflow docs and the
command-exec tool (per the maintainer's direction). It supersedes nothing; it extends
RFC 0023 and gives RFC 0012's "secrets, not ambient authority" posture an interactive
and workload-identity story.*
