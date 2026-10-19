# Outbound credentials

agentd dials three kinds of endpoint that may need a credential: model
providers, MCP servers and A2A peers. Each takes one `auth:` block with the
same shape, so a static token, an OAuth login, AWS SigV4 or a SPIFFE workload
identity is configured the same way wherever it is used. This guide is for
operators: it shows how to choose a credential, configure it, log in once
when a person is needed, and what happens when a credential is missing.
Signing callers *in* to agentd's own listener is a different task, covered in
[Connect agents over A2A](connect-agents.md).

## Choose a credential

| Where agentd runs | Use | Interactive? |
|---|---|---|
| anywhere, the endpoint takes an API key | `static`, from a secret reference | never |
| a server, machine-to-machine OAuth | `oauth2` with `grant: client_credentials` | never |
| a developer's or operator's machine, a person's OAuth identity | `oauth2` with `grant: device` (or `authorization_code` for a browser) and one `agentd --login` | once |
| EC2 or EKS, an endpoint behind AWS IAM | `aws` with `source: imds` or `source: irsa` | never |
| a workstation, AWS IAM Identity Center | `aws` with `source: sso` and one `agentd --login` | once |
| Kubernetes or a cloud with SPIRE | `spiffe`, a JWT-SVID as a bearer or an X.509-SVID for mutual TLS | never |

A daemon never stops to wait for a person. Interactive credentials are
obtained ahead of time with `agentd --login`, cached, and refreshed by the
daemon.

## A complete example

This agent calls a corporate model gateway with a machine credential, reads
an issue tracker as the operator who logged in, and hands work to a partner
agent with a static token:

```yaml
agent:
  instruction: "Triage new tracker issues and hand billing questions to the partner desk."

intelligence:
  endpoints: https://llm.corp.example.com/v1/chat/completions
  model: gpt-5.1
  auth:
    kind: oauth2
    grant: client_credentials
    issuer: https://sso.corp.example.com
    client_id: agentd-triage
    client_secret: "{{secret:SSO_CLIENT_SECRET}}"
    scopes: [llm.invoke]

mcp:
  servers:
    - name: tracker
      endpoint: https://mcp.tracker.example.com/mcp
      tags: { "*": [untrusted_input] }
      auth:
        kind: oauth2
        grant: device
        issuer: https://sso.corp.example.com
        client_id: agentd-cli
        scopes: [tracker.read, offline_access]

a2a:
  peers:
    - name: partner
      endpoint: https://a2a.partner.example.com
      auth: { kind: static, token: "{{secret:PARTNER_TOKEN}}" }
```

With `SSO_CLIENT_SECRET` and `PARTNER_TOKEN` set, validate it, then log in to
the tracker once:

```sh
agentd --validate-config --config agentd.yaml
agentd --login mcp:tracker --config agentd.yaml
```

The client-credentials grant needs no login: the daemon fetches and refreshes
that token itself.

## Where a credential goes

| Endpoint | `auth:` block | Other ways to set a credential |
|---|---|---|
| the model connection | `intelligence.auth` | `intelligence.token` or `token_file` (a bearer), `intelligence.headers` |
| a named model provider | `intelligence.providers.<name>.auth` | that provider's `token`, `token_file`, `headers` |
| an MCP server | `mcp.servers[].auth` | `headers`; `oauth: {token_url, client_id, client_secret, scope}`, a client-credentials shortcut |
| a service catalog entry | `services.<name>.auth` | `headers` |
| an A2A peer | `a2a.peers[].auth` | `headers`; `client_cert` and `client_key` for mutual TLS |

On a model connection, a credential from `auth:` takes precedence over
`token`. The `AGENTD_INTELLIGENCE_TOKEN` and `AGENTD_INTELLIGENCE_TOKEN_FILE`
environment overrides apply to the top-level connection only, never to a
named provider.
On an MCP server, `auth:` wins over `oauth:`, and either one turns off AAuth
signing for that server; static `headers` are sent alongside.

**One credential for many consumers.** A service catalog entry holds the
credential every server or peer that references it uses, cached once under
`service:<name>`:

```yaml
agent:
  instruction: "Answer account questions from the CRM."

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"

services:
  crm:
    endpoint: https://mcp.crm.example.com/mcp
    tags: { "*": [sensitive] }
    auth:
      kind: oauth2
      grant: device
      issuer: https://sso.corp.example.com
      client_id: agentd-cli
      scopes: [crm.read]
mcp:
  servers:
    - { name: crm, service: crm }
```

A server that references an entry may not restate its endpoint, `auth` or
`headers`. `agentd --login mcp:crm` and `agentd --login service:crm` log into
the same cached credential.

## Static credentials

A fixed bearer token, or a fixed value in a header of your choosing, always
from a secret reference:

```yaml
# fragment: under mcp.servers[]:
auth: { kind: static, token: "{{secret:KB_TOKEN}}" }
```

```yaml
# fragment: under mcp.servers[]:
auth: { kind: static, header: X-API-Key, value: "{{secret:KB_API_KEY}}" }
```

`token` is sent as `Authorization: Bearer <token>`. A `{{secret-file:PATH}}`
reference is read again on every request, so a rotated file takes effect
without a restart.

On a model connection, a `static` block takes `token`; a `header` + `value`
pair is refused at load there. Use `intelligence.headers` for a custom header
on a model connection.

## OAuth 2.0

```yaml
# fragment: under mcp.servers[]:
auth:
  kind: oauth2
  issuer: https://sso.corp.example.com      # or pin token_url, device_authorization_url
  client_id: agentd-cli
  client_secret: "{{secret:SSO_CLIENT_SECRET}}"   # omit for a public client
  grant: device                             # device | authorization_code | client_credentials
  scopes: [tracker.read, offline_access]
  audience: https://tracker.example.com     # sent with the device authorization request
```

| Grant | When | How the token arrives |
|---|---|---|
| `device` (the default) | a person authorizes once, from any terminal, over SSH included | `agentd --login` prints a URL and a short code and polls until the person approves |
| `authorization_code` | a person with a desktop browser | `agentd --login` prints the authorization URL, catches the redirect on a one-time `127.0.0.1` listener, checks `state` and `iss`, and exchanges the code with PKCE |
| `client_credentials` | machine to machine | the daemon fetches the token with the client's authentication — its `client_secret`, or a `private_key_jwt` assertion — and fetches a new one a minute before it expires, or halfway through a lifetime shorter than two minutes; no login |

**Endpoints.** With `issuer`, agentd discovers the token, device,
authorization and revocation endpoints from the server's metadata. It asks
for the RFC 8414 document first, then the OpenID Connect one, in the order
MCP's authorization spec gives: for `https://sso.example/tenant`,
`https://sso.example/.well-known/oauth-authorization-server/tenant`, then
`https://sso.example/.well-known/openid-configuration/tenant`, then
`https://sso.example/tenant/.well-known/openid-configuration`; for an issuer
with no path, the first two at the root. The first document that answers is
used only if its `issuer` is the configured `issuer`, byte for byte: one that
names another issuer, or none, is refused (`oauth: discovery: … names the
issuer …`), because metadata that does not match is how a mix-up attack
points a client at another server's endpoints. Write `issuer` exactly as the
server's metadata does, trailing slash included or not. Pin `token_url`,
`device_authorization_url` or `authorization_url` to skip discovery. An
`oauth2` block needs `client_id` — or, without one,
[agentd's Client ID Metadata Document](#client-id-metadata-document) — and
either `issuer` or `token_url`; the device grant also needs `issuer` or
`device_authorization_url`, and the browser grant `issuer` or
`authorization_url`. The load checks each of these.

With no `issuer` and no pinned endpoint, `agentd --login` asks the MCP
server itself (RFC 9728): the `resource_metadata` URL its `401` challenge
names, else the protected-resource metadata with the well-known suffix
inserted before the server's path
(`https://mcp.example/.well-known/oauth-protected-resource/mcp`), else the
one at the root.

**The browser grant's checks.** The redirect's `state` must be the one the
login sent, and its `iss` (RFC 9207) the issuer the request went to, byte
for byte; a redirect without `iss` is refused when the server's metadata
declares `authorization_response_iss_parameter_supported`. With endpoints
pinned and no `issuer` there is nothing to compare `iss` with, and it is not
checked. The `state` and the PKCE verifier are drawn from the kernel's
random source with no fallback: a host that cannot read it fails the login.

**Client authentication.** At the token and revocation endpoints a
confidential client sends its `client_secret` in the form body. Set
`client_auth` to change how:

```yaml
# fragment: under mcp.servers[].auth (kind: oauth2):
client_auth:
  method: private_key_jwt          # private_key_jwt (default) | client_secret_basic | client_secret_post
  key: "{{secret-file:/run/secrets/agentd-client.pem}}"   # PKCS#8 P-256 key; private_key_jwt only
  kid: agentd-2026-10              # optional; default the key's RFC 7638 thumbprint
  assertion_aud: token_endpoint    # token_endpoint (default) | issuer
```

`private_key_jwt` (RFC 7523) sends no secret: each token request carries a
fresh ES256 assertion signed with `key`, good for 60 seconds, with `iss` and
`sub` the `client_id`, `aud` the token endpoint (or the `issuer`), and a
random `jti`. Register the key's public half with the authorization server.
`client_secret_basic` sends the `client_secret` in an `Authorization: Basic`
header instead of the body. `client_auth` applies to every grant: a
`client_credentials` grant with `private_key_jwt` needs no `client_secret`.
A confidential client authenticates at the device authorization endpoint
too, as at the token endpoint (RFC 8628 §3.1).

The daemon discovers them at the first request that needs them — the first
token fetch of a `client_credentials` grant, the first refresh of a login —
not when the connection is built, and again at each later request until
discovery succeeds. While the issuer does not answer, those requests fail
without being sent; an issuer that is down at startup does not stop the
start. Pin the endpoints to depend on no discovery at all.

**The MCP shortcut.** `mcp.servers[].oauth` is a client-credentials grant
with a pinned token endpoint, and nothing else:

```yaml
# fragment: under mcp.servers[]:
oauth:
  token_url: https://sso.corp.example.com/oauth2/token
  client_id: agentd-search
  client_secret: "{{secret:SEARCH_CLIENT_SECRET}}"
  scope: search.read
```

> **Limit.** agentd reads OAuth responses as JSON, which is what RFC 6749
> and RFC 8628 specify, and does not send `Accept: application/json`. An
> authorization server that answers form-encoded unless asked for JSON, such
> as GitHub's OAuth endpoints, fails with `oauth: bad device-auth response`.

## Client ID Metadata Document

An authorization server agentd has no registration at can still know it, by
a [Client ID Metadata Document](https://datatracker.ietf.org/doc/draft-ietf-oauth-client-id-metadata-document/)
(draft -02): agentd's `client_id` is an HTTPS URL, and the JSON document at
that URL says who the client is and where its public keys are. MCP's
2026-07-28 client registration prefers it to dynamic registration, which it
deprecates. Configure the identity once, under `security.client`:

```yaml
# fragment: under security:
client:
  id: https://agent.example.com/oauth/client.json   # the client_id, and where the document is published
  name: Example operations agent                     # client_name, on consent pages
  key_file: /var/lib/agentd/client-key               # the ES256 key, created 0600 when absent
  grants: [device, client_credentials]               # what the document lets a server issue tokens by
```

An `oauth2` block that names no `client_id` then uses it: the document's URL
is the `client_id`, and every token request — and the device authorization
request — carries a `private_key_jwt` assertion signed with `key_file`. The
block needs `issuer`, no `client_secret`, and a `grant` the document lists;
`client_auth` may set `assertion_aud` and nothing else. A block that names a
`client_id` keeps its own registration.

```yaml
# fragment: under mcp.servers[]:
auth:
  kind: oauth2
  issuer: https://auth.tracker.example.com   # its metadata says whether it takes the document
  grant: device
  scopes: [tracker.read]
```

**Publish it.** The authorization server fetches the document from its URL,
so the URL must answer it over HTTPS. Write the two files for a static host:

```sh
agentd --config agentd.yaml --print-client-metadata ./published
```

```text
./published/client.json
./published/jwks.json
```

Host `client.json` at `id` and `jwks.json` at `jwks_uri`, `jwks.json`
beside the document unless `jwks_uri` names another URL. Or set
`security.client.serve: true` and the A2A listener serves both at their
URLs' paths, when `a2a.url` is the `client_id`'s origin. The document
carries `client_id`, `client_name`, `logo_uri` when set, `jwks_uri`,
`redirect_uris`, `grant_types`, `response_types` and
`token_endpoint_auth_method: private_key_jwt`; never a secret or a private
key. The key set lists the signing key and every `published_key_files` key
— the next one before a rotation, the previous one after — by their public
halves only. A server may refuse the document's tokens once its keys
change, so publish a new key before signing with it.

**Where agentd uses it.** In the order MCP's client registration gives: a
configured `client_id` first; else the document, where the authorization
server's metadata advertises `client_id_metadata_document_supported`. agentd
does not register itself dynamically (RFC 7591). Where the metadata does not
advertise the document, `agentd --login` and the daemon's first token
request fail before anything else is sent, saying what to register:

```text
agentd: oauth: the authorization server https://auth.tracker.example.com does not advertise client_id_metadata_document_supported, so it would not fetch agentd's Client ID Metadata Document (https://agent.example.com/oauth/client.json); register agentd there — a client authenticating by private_key_jwt with the key set at its jwks_uri, or by a client secret — and set the block's `client_id` (and its `client_auth` or `client_secret`). agentd does not register dynamically (RFC 7591, which MCP 2026-07-28 deprecates)
```

A server that refuses the document with `invalid_client` is named with the
document's URL: check that it serves what `--print-client-metadata` writes,
and that its key set holds the signing key.

**Grants.** `device` and `client_credentials` need no redirect URI. The
document lists the grant types of `grants`, with `refresh_token` beside
`device` and `authorization_code`, whose tokens agentd renews. For
`authorization_code`, list a loopback redirect URI, where the login waits
for the browser:

```yaml
# fragment: under security.client:
grants: [authorization_code]
redirect_uris: ["http://127.0.0.1:8765/callback"]
```

With a port, `agentd --login` listens on that port and sends the URI
exactly, so it matches the document under the draft's exact comparison.
Without one (`http://127.0.0.1/callback`), it listens on a free port and
sends the URI with that port, for a server that allows any port on a
loopback address, as RFC 8252 §7.3 and OAuth 2.1 have it. A redirect URI is
`http://127.0.0.1` or `http://[::1]`, never `localhost`. A login uses the
first one listed.

**Every endpoint.** The document is the client of any `oauth2` block with no
`client_id`: an MCP server's, a service entry's, a model connection's or an
A2A peer's.

The [Personal Agent Protocol preview](act-at-companies.md) keeps a document
of its own, `security.companies.client`: that protocol holds every redirect
URI to an HTTPS URL on the `client_id`'s domain, where this one lists
loopback URIs, and adds an `extensions` member.

## Log in with agentd --login

When a credential needs a person (an `oauth2` device or authorization-code
grant, or `aws` with `source: sso`), complete it once with `agentd --login`.
The release binaries and the container image include the `oauth` feature it
needs.

```sh
agentd --login mcp:tracker --config agentd.yaml
```

```text
┌─ authorize agentd ──────────────────────────────
│  target   mcp:tracker
│  visit    https://sso.corp.example.com/device
│  code     WDJB-MJHT
│  waiting… (expires in 900s · Ctrl-C to cancel)
└──────────────────────────────────────────────────
logged in to mcp:tracker — token cached in /home/you/.local/state/agentd/creds
```

The prompt goes to stderr and the one-line result to stdout. The token is
never printed.

| Target | Logs into |
|---|---|
| `intelligence` | the top-level model connection's `auth` |
| `intelligence:<provider>` | `intelligence.providers.<provider>.auth` |
| `mcp:<name>` | that MCP server's `auth`; a server that references a service logs into `service:<entry>` |
| `service:<name>` | a service catalog entry's `auth`, shared by every consumer |

An A2A peer can be logged into only through a service entry: give the peer
`service: <entry>` with a `kind: peer` catalog entry, and log into
`service:<entry>`. `--login` takes no target for a peer's own `auth` block,
so an oauth2 `device` or `authorization_code` grant there, or one with no
`grant`, is refused at load.

`agentd --logout <target>` deletes the cached credential from the cache,
then revokes it at the authorization server that issued it, when that
server's metadata lists a `revocation_endpoint` (RFC 7009) — the refresh
token when there is one, which at most servers ends the access tokens it
issued too. The delete comes first, so a logout cut short — a server that
does not answer, a Ctrl-C — has still removed it. The server is the
block's `issuer`, or the one RFC 9728 finds for the resource; with neither,
or no revocation endpoint, the logout is local only. A revocation that
fails is reported, and the logout exits 2 saying the token stays good at
the server until it expires. A build without the `oauth` feature only
deletes. A daemon running at the time serves the access token it holds
until it is due, then finds no login: it fails its requests, naming the
`--login` that fixes it, and does not renew.

The device grant, from login to the daemon's refresh:

```mermaid
sequenceDiagram
  participant P as You
  participant L as agentd --login
  participant AS as Authorization server
  participant C as Credential cache
  participant D as agentd daemon
  participant E as Endpoint
  L->>AS: device authorization request
  AS-->>L: user code and verification URL
  L->>P: prints the URL and the code
  P->>AS: approves in a browser
  L->>AS: polls the token endpoint
  AS-->>L: access and refresh tokens
  L->>C: writes them, mode 0600
  D->>C: reads them when it has no usable token
  D->>E: request with the bearer token
  D->>AS: refreshes before the token expires
  D->>C: writes the renewal back before using it
```

**The cache** is one file per target, mode `0600`, in `$AGENTD_CRED_DIR`, else
`$XDG_STATE_HOME/agentd/creds`, else `~/.local/state/agentd/creds`. Log in as
the user the daemon runs as, or point both at the same `AGENTD_CRED_DIR`. The
daemon refreshes a token a minute before it expires, using the refresh
token, and writes the result back to the cache before it uses it: a server
that rotates refresh tokens has its new one kept, and a refresh response
that carries none keeps the old one. Every process of the daemon renews from
the refresh token the cache holds, so a restart, or a turn worker, renews
with the token the server last issued. A renewal holds a lock on the
target's entry — an advisory `flock` on a `.lock` file beside it — so the
processes and connections that find the token due at once renew it once:
the first renews, and the rest wait and take what it wrote back. Each
presenting the same refresh token would have a server that rotates refuse
all but one, or end the grant as stolen. `--login` and `--logout` take the
same lock, so a renewal in flight neither overwrites a new login nor brings
back one a logout removed. Each write replaces the file whole, through a
`0600` temp file renamed over it, so a process reading it never sees part
of one, and a crash mid-write leaves the old. A write that fails leaves the
refresh unused, and each request fails, naming the cause, until the write
succeeds. Whenever the daemon holds no usable token it reads the cache
again, so a login made while it runs is used by the next request, with no
reload or restart. A login is kept with the authorization server it was
made at: the block's `issuer`, the one RFC 9728 found, or, where the block
names no issuer, its `token_url`. Under a block whose server is another —
an `issuer` changed, or added to a block that only pinned `token_url` — the
login is not presented, renewed or revoked, and requests fail naming the
`--login` that fixes it.

**Waits the server asks for.** A token endpoint that answers `429` or `503`
with `Retry-After` (seconds or an HTTP date) is not asked again before the
wait is over, five minutes at most: a refresh in that time fails at once,
naming the wait, and a device-login poll waits it out.

## AWS SigV4

An endpoint behind AWS IAM, such as an API Gateway in front of an MCP server
or Amazon Bedrock, takes a `kind: aws` block. Every request is signed over
its method, path and body; a session token is signed as
`x-amz-security-token`.

```yaml
# fragment: under mcp.servers[]:
auth:
  kind: aws
  region: us-east-1
  service: execute-api
  source: irsa
```

| `source` | Credentials | Login |
|---|---|---|
| `env` (the default) | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, read at each request | none |
| `static` | the same as `env` | none |
| `imds` | the EC2 instance role, through IMDSv2, refreshed a minute before expiry | none |
| `irsa` | the EKS pod's web identity (`AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN`) exchanged with STS, refreshed a minute before expiry | none |
| `sso` | IAM Identity Center; needs `sso_start_url`, `account_id`, `role_name` | `agentd --login`, which caches temporary credentials; the signer re-reads them on every request |

```yaml
# fragment: under mcp.servers[]:
auth:
  kind: aws
  region: us-east-1
  service: execute-api
  source: sso
  sso_start_url: https://my-org.awsapps.com/start
  account_id: "123456789012"
  role_name: AgentdLedger
```

For Amazon Bedrock's own API, set `intelligence.dialect: bedrock` with a
`kind: aws` block and `service: bedrock`; the load refuses the Bedrock
dialect without one. [Connect model providers](connect-models.md) has
complete configurations for Bedrock, Azure OpenAI with Microsoft Entra ID, and
Google Vertex AI.

## SPIFFE workload identity

With SPIRE writing SVIDs to files, an agent needs no stored secret at all.

```yaml
# fragment: under mcp.servers[]:
auth: { kind: spiffe, svid: jwt, jwt_svid_file: /run/spire/jwt.svid }
```

```yaml
# fragment: under mcp.servers[]:
auth: { kind: spiffe, svid: x509, svid_file: /run/spire/svid.pem, key_file: /run/spire/svid.key }
```

A JWT-SVID is sent as a bearer token and read again on every request, so
SPIRE's rotation needs nothing from you. An X.509-SVID is a mutual-TLS client
identity: it works on MCP servers. A model connection cannot use one, and the
load refuses it there. For an A2A peer, an `x509` block applies nothing; set
the peer's `client_cert` and `client_key` to the SVID files instead.

## Mutual TLS and private CAs

An A2A peer that requires a client certificate takes `client_cert` and
`client_key`, PEM files read when the peer is dialled:

```yaml
# fragment: under a2a.peers[]:
name: billing
endpoint: https://a2a.billing.example.com
client_cert: /run/spire/svid.pem
client_key: /run/spire/svid.key
```

To trust an endpoint whose certificate a private CA signed, set
`security.tls_ca` to a PEM bundle. agentd adds it to the bundled trust roots
for every outbound TLS dial, in the daemon and its child processes.

## Which kinds each endpoint accepts

| | `static` token | `static` header | `oauth2` | `aws` | `spiffe` jwt | `spiffe` x509 |
|---|---|---|---|---|---|---|
| model connection or provider | yes | refused at load | yes | yes | yes | refused at load |
| MCP server or service | yes | yes | yes | yes | yes | yes (mutual TLS) |
| A2A peer | yes | yes | yes, login through a service entry | yes | yes | use `client_cert` and `client_key` |

## When a credential is missing or refused

The load catches what it can see, and exits 2 before any side effect:

- a `{{secret:…}}` or `{{secret-file:…}}` reference that does not resolve;
- an `auth:` block missing the fields its kind and grant need;
- an inline `token`, `value`, `client_secret` or `client_auth.key` in any `auth:` block, and an
  inline `intelligence.token`, provider `token` or `oauth.client_secret` in a
  config file;
- an `auth:` block on a model connection, an MCP server or an A2A peer in a
  binary built without the `oauth` feature, which could not present it.

What depends on the world is not caught at load: an interactive credential
that was never logged in, or has expired with no refresh token; an `issuer`
or a token endpoint that does not answer; an `aws` source with no
credentials; a `{{secret-file:…}}` or `token_file` that cannot be read when a
request needs it; an AAuth Agent Provider that does not answer, or will not
enroll the key or issue an agent token (target `aauth`; see
[AAuth agent identity](aauth.md#turn-it-on)). agentd never sends a request
without the credential it is configured to present. The request is not sent,
and it fails with an error that names the target and the cause:

- an MCP server's connect fails (`mcp.connect.fail`) and is dialed again in
  the background, or, on a live connection, the tool call fails;
- a model call fails as an authentication error: it is not failed over to
  another endpoint or model, and it fails the turn, step or run;
- an A2A peer call fails.

The log says so once, not once a call: `auth.unavailable` (warn) names the
target and the cause when its requests start failing, and again at most
once a minute while they keep failing; `auth.recovered` (info) says when one
succeeds again. The supervisor writes them for the whole process tree: a
turn worker or subagent hands what its credentials do up to it, so a
credential every turn fails on is one line a minute, not one a turn. Every
later request tries again, so logging
in with `agentd --login <target>`, as the user the daemon runs as, takes
effect on the next request. In a one-shot job, the run's `run.done` line
carries the error:

```text
{…,"err":"step \"work\" failed: turn failed: intel: intelligence request not sent: intelligence: no cached credential — run `agentd --login intelligence`","event":"run.done",…,"status":"failed",…}
```

The job then exits 4. A `401` from an endpoint means the credential was sent
and refused: a token that is wrong or revoked. A `403` means it was accepted
and the request is beyond it. An MCP server's challenge says which: a `401`
or `403` with `WWW-Authenticate: Bearer error="…"` fails the call as
`HTTP 403: insufficient_scope (needs scope "…")`, `sign_in_required` or
`invalid_token`, after the credential's signer has been shown it and had
nothing to retry with. An MCP server that answers `429` or `503` with
`Retry-After` is sent nothing more — no call, no handshake, no notification
stream — until the wait is over, five minutes at most; a request in that
time fails at once, unsent. A model endpoint's `Retry-After` of up to 30
seconds is waited out before the retry, and a longer one takes the endpoint
out of rotation for it, so another endpoint serves meanwhile. The
[CLI reference](cli.md) has every exit code.

## Field reference

| Field | Kinds | Meaning |
|---|---|---|
| `kind` | all | `static`, `oauth2`, `aws` or `spiffe` (required) |
| `token` | `static` | bearer token; must be a secret reference |
| `header`, `value` | `static` | a custom header and its value; `value` must be a secret reference |
| `issuer` | `oauth2` | authorization server for endpoint discovery |
| `token_url` | `oauth2` | token endpoint; pins discovery |
| `device_authorization_url` | `oauth2` | device authorization endpoint |
| `authorization_url` | `oauth2` | authorization endpoint for `grant: authorization_code` |
| `client_id` | `oauth2` | the registered client; without one, [agentd's Client ID Metadata Document](#client-id-metadata-document) (`security.client`) |
| `client_secret` | `oauth2` | client secret; must be a secret reference; required for `client_credentials` unless `client_auth` signs |
| `client_auth` | `oauth2` | `{method, key, kid, assertion_aud}`: `private_key_jwt` (default) with a PKCS#8 P-256 `key` (a secret reference), `client_secret_basic` or `client_secret_post`; with no `client_id`, only `assertion_aud` |
| `grant` | `oauth2` | `device` (default), `authorization_code` or `client_credentials` |
| `scopes` | `oauth2` | requested scopes |
| `audience` | `oauth2` | `audience` parameter sent with the device authorization request and the client-credentials grant, for authorization servers that want one |
| `region` | `aws` | AWS region (required) |
| `service` | `aws` | signing service name, such as `bedrock` or `execute-api` (required) |
| `source` | `aws` | `env` (default), `static`, `imds`, `irsa` or `sso` |
| `sso_start_url`, `account_id`, `role_name` | `aws` | IAM Identity Center portal, account and permission set; required for `source: sso` |
| `svid` | `spiffe` | `jwt` (default) or `x509` |
| `jwt_svid_file` | `spiffe` | JWT-SVID file; required for `svid: jwt` |
| `svid_file`, `key_file` | `spiffe` | X.509-SVID certificate and key; required for `svid: x509` |

## Next steps

- [Connect model providers](connect-models.md): complete configurations for
  each provider, credentials included.
- [Connect MCP servers](connect-mcp.md): declare servers, tags and tool
  ceilings.
- [AAuth agent identity](aauth.md): a signed agent identity for servers that
  ask for one.
- [Harden a deployment](harden.md): secrets, egress and the rest of the
  checklist.
