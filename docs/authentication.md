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
  resource: auto                            # RFC 8707: auto | omit | a URI
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
`authorization_url`. An MCP server's block, or an MCP catalog entry's, may
name none of them: the server names its authorization server. The load
checks each of these.

**An MCP server names its authorization server.** With no `issuer` and no
`token_url`, `agentd --login` and the daemon — at the first request that
needs a token — ask the MCP server itself (RFC 9728): the
`resource_metadata` URL its `401` challenge names, when it is on the
server's own origin, else the protected-resource metadata with the
well-known suffix inserted before the server's path
(`https://mcp.example/.well-known/oauth-protected-resource/mcp`), else the
one at the root. The document is used only when its `resource` is the
server's `endpoint`, byte for byte (RFC 9728 §3.3); metadata describing
another resource is refused (`… describes the resource …, not … (RFC 9728
§3.3)`). Its first `authorization_servers` entry is the issuer, and a login
is kept with it: a server whose metadata names another issuer later finds
the login is not presented there. That issuer is a URL the server chose:
on the server's own origin it is reached as the server is; anywhere else
only over HTTPS and through the SSRF guard — no loopback, private or
link-local address — and, under `security.egress: closed`, only when a
`services` entry covers it. Its metadata must keep the token, device and
revocation endpoints on its own origin; pin `issuer` to use one that does
not. Metadata of an issuer you pinned is trusted for the endpoints it
names, wherever they are (Google's `https://accounts.google.com` names
`https://oauth2.googleapis.com/token`).

**The resource (RFC 8707).** Every authorization, token, refresh and
client-credentials request names the endpoint the token is for as
`resource`: on an MCP server or an MCP catalog entry, the server's
`endpoint` in canonical form — scheme and host lowercased, no fragment, no
trailing `/` — which is what MCP's authorization spec requires whether the
authorization server supports the parameter or not. `resource: omit` sends
none, for a server that refuses parameters it does not know; a URI sends
that URI. A model connection's and an A2A peer's blocks send none unless
told to (`resource: auto` sends the endpoint), because Microsoft Entra
refuses the parameter (`AADSTS901002`). The `mcp.servers[].oauth` shortcut
always sends the server's endpoint.

**The browser grant's checks.** Before any URL is printed, the server's
metadata must list `S256` in `code_challenge_methods_supported`: a server
that cannot be held to PKCE is refused (MCP 2026-07-28). With every endpoint
pinned no metadata is read, and nothing is judged. The redirect's `state`
must be the one the login sent, and its `iss` (RFC 9207) the issuer the
request went to, byte for byte; a redirect without `iss` is refused when the
server's metadata declares `authorization_response_iss_parameter_supported`.
With endpoints pinned and no `issuer` there is nothing to compare `iss`
with, and it is not checked. A login that asks for `openid` sends a
`nonce`, and an ID token in the answer must carry it, name the issuer and
have this `client_id` in its `aud`, or the login is refused. The `state`,
the PKCE verifier and the nonce are drawn from the kernel's random source
with no fallback: a host that cannot read it fails the login.

**Step-up.** An MCP server that answers a call `403` with `WWW-Authenticate:
Bearer error="insufficient_scope", scope="…"` names scopes the token lacks.
For a login (`device` or `authorization_code`), the daemon renews the token
with the union of the scopes it holds and those the server named, and sends
the call again with the new one — at most twice for one union in the life
of the process. When the authorization server will not grant the union by
a renewal, the call fails with the server's challenge, and the union is
kept with the login: the next `agentd --login` asks for it. A challenge for
scopes the token already holds is the server's inconsistency, and asks
nothing. A `client_credentials` grant asks for nothing more: the call fails
with the challenge, as MCP permits a client acting for itself.

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
with a pinned token endpoint, and nothing else; its token request names the
server as its `resource`:

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

## Enterprise-managed authorization

**A preview, not in the release binaries**: build with `--features xaa,a2a`,
and set `security.experimental.id_jag: true`. ID-JAG is an IETF draft
(draft-ietf-oauth-identity-assertion-authz-grant-04) and MCP's
Enterprise-Managed Authorization extension is young; agentd follows both as
they stand ([RFC 0044](../rfcs/0044-enterprise-managed-authorization.md)).

With it, agentd acts **for each enrolled person** at an MCP server, with a
token whose subject is that person, issued because their enterprise IdP —
Okta's Cross App Access, or any IdP that issues ID-JAGs — says agentd may.
agentd's own credential is never used at such a server, and nothing falls
back to it. Each person's grant is theirs alone: their work mints with it,
nobody else's does, and no token, cache entry or connection is shared
between two people.

How a call is made:

1. The person enrols once by signing in at the IdP: a per-person operator
   with `agentd --login idp:<name>`, anyone else from a display client
   (below). The daemon runs the sign-in, verifies the ID token with the
   IdP's keys, holds its `iss` and `sub` to who the person is — their pin,
   or the identity the sign-in establishes — and keeps the refresh token
   sealed (AES-256-GCM, `identity.delegation.seal_key`). Then it lists each
   allowed server's tools, with one delegated token per server.
2. At a call, the supervisor exchanges the grant at the IdP for an ID-JAG
   (RFC 8693): `audience` is the server's authorization server and
   `resource` the server, both pinned in the configuration. It checks the
   ID-JAG — `typ`, `aud`, `client_id`, `iss`, `sub`, `exp`, the `resource` it
   names and the scope it grants — and redeems it at the authorization server
   (RFC 7523, the JWT bearer grant) for an access token for that server.
3. The call goes to the server with that token, declaring the
   `io.modelcontextprotocol/enterprise-managed-authorization` extension, and
   `agent/acting_for` set to the ID-JAG's `sub`.

```yaml
# needs: ./okta-agent-key.pem, ./linear-ras-key.pem, ./delegation-seal
agent: {name: assistant}
intelligence: {endpoints: "mock:final", model: m}
a2a:
  listen: unix:///run/agentd/a2a.sock
  principals:
    # The per-person operator: the unix listener's peer, by its uid.
    - match: {uid: 1000}
      role: operator
      id: alice
      idp_subject: {iss: https://acme.okta.com, sub: 00u1alice}
security:
  experimental: {id_jag: true}
identity:
  providers:
    okta:
      issuer: https://acme.okta.com
      profile: okta
      client_id: 0oa9agentdXAA
      client_auth:
        method: private_key_jwt
        key: "{{secret-file:./okta-agent-key.pem}}"
        kid: agentd-2026-10
      sign_in: {grants: [device], scopes: [openid, offline_access]}
  delegation:
    seal_key: "{{secret-file:./delegation-seal}}"
    templates:
      - name: eng-tools
        principals: {source: operator}
        idp: okta
        servers:
          linear: {scopes: ["issues:read", "issues:write"]}
mcp:
  servers:
    - name: linear
      endpoint: https://mcp.linear.example/mcp
      tags: {"*": [sensitive, egress]}
      auth:
        kind: oauth2
        grant: id_jag
        idp: okta
        audience: https://auth.linear.example
        resource: https://mcp.linear.example/mcp
        client_id: agentd-at-linear
        client_auth:
          method: private_key_jwt
          key: "{{secret-file:./linear-ras-key.pem}}"
          kid: agentd-ras-1
        scopes: ["issues:read"]
        tool_scopes: {create_issue: ["issues:write"]}
```

```sh
agentd --login idp:okta --config agentd.yaml
```

```text
Open https://acme.okta.com/activate and enter the code WDJB-MJHT (it expires in 600s)
operator:alice is enrolled at okta as 00u1alice (template eng-tools): agentd acts as them on linear until 2026-11-18; listed linear (12 tools)
```

**Who may delegate.** Whoever an `identity.delegation.templates` entry
admits, by the principal's `source` — nobody else:

- `operator` — a **per-person operator**: an `a2a.principals` rule that
  matches the unix A2A listener's peer by `uid` and carries an exact
  `idp_subject` pin. That caller is `operator:<id>`, and enrols with
  `agentd --login idp:<name>`. Several people may each have one.
- `oidc` — a person who **signed in at the IdP from a display client**
  (`a2a.login`, below): `user:oidc:<iss>#<sub>`, whatever their IdP says
  they are. Signing in is their enrolment.
- `x509` — a **certificate principal**: an `a2a.principals` rule on an mTLS
  listener (`a2a.tls.client_ca`) that admits one client certificate's key —
  `match.spki`, the SHA-256 of its SubjectPublicKeyInfo — with an `id` and
  an exact `idp_subject` pin. Over that certificate it enrols with
  `delegation.enrol {idp}` (the IdP's device grant, since it is pinned), and
  its grant is the key's (`x509:<SPKI SHA-256>`): a reissued certificate
  with another key is not this principal. An operator one is the person
  `operator:<id>`; a user one is `user:<id>`.
- `device` — an RFC 0043 **device session** an operator approved under a
  name, and only a name an operator has bound to one person: pinned
  (`identity.delegation.device_pins.<name>: {iss, sub}`), or — opted in —
  bound by its first enrolment (`identity.delegation.device_first_binding`;
  a sign-in link forwarded to someone else lands their proof on the session
  that started it, so pin where you can). A device session uses the grant
  only after it has itself signed in at the IdP as the identity the name is
  bound to (`delegation.enrol`): a second device approved under the same
  name, or one that proved someone else, is `not_enrolled`, and its sign-in
  as someone else is `binding_mismatch` and leaves the grant alone.

A bearer principal never delegates (`principal_not_delegable`), since a
`bearer_ref` secret is typically shared. A template's principal is checked
at every call, as it stands then: a run is the person's who started it,
through the session it was started through, and a delegated server's tools
are not offered at all in work for someone without a usable grant there.
A run started by a schedule, a webhook, another operator or anyone else
fails `no_delegation` at a delegated server — unless its workflow's
`act_for` names the person and their template allows it ([Work nobody is
there for](#work-nobody-is-there-for)); `identity.autonomous_as` may not
name a person. A subagent's call is `delegation_not_permitted`, and so
is a call in a run a subagent's call started — through a workflow tool,
`workflow.run` or `workflow.signal` — or in a run that one started, unless
the person's template says `subagents: inherit`: then the subagent acts
with the person's grant at the delegated servers it declared itself, and
holds no token — the supervisor makes each call. A sign-in whose `iss` and
`sub` are not who the person is keeps nothing (`binding_mismatch`). Signing
in again keeps the grant while it is the same — the same identity,
unexpired, under the template that admits the person now with the ceilings
it allows now — and otherwise replaces it, revoking the refresh token it
held at the IdP: that is how an expired grant, or one enrolled before the
pin changed, is renewed. A grant is held at every call to the ceilings its
template allows now. The IdP's device grant shows `verification_uri` and
`user_code` only; `sign_in.grants: [authorization_code]` uses PKCE, `state`,
`nonce` and RFC 9207's `iss`, through a one-time start link: the link sets a
cookie bound to the sign-in, and the callback is taken only with it, so an
IdP URL forwarded to someone else never completes.

**Signing in from a display client.** `a2a.login: {idp: <name>}` lets a
display client sign a person in at that IdP on a TCP listener. The client
asks `POST /oauth2/login_authorization` (named `login_authorization_endpoint`
in the listener's RFC 8414 metadata) for a start link and a request code,
opens the link in the person's browser, and polls the token endpoint with
the request code and the grant `https://agentd.dev/oauth/grant-type/login`.
The link, on the listener origin, is good once and for 60 seconds; it sets a
`__Host-` cookie bound to the sign-in and sends the browser to the IdP; the
IdP returns it to `/oauth2/callback`, which is taken only with that cookie.
The daemon exchanges the code, verifies the ID token and issues an RFC 0043
session of `source: oidc` carrying the IdP proof (`a2a.login.token_ttl`, 8
hours by default) — the client never sees an IdP token. Where a template
admits the person, that sign-in kept their grant; where none does, they are
signed in all the same and the refresh token is revoked at the IdP. A
listener with `a2a.login` declares `bearer` in its card, grants a loopback
caller no implicit operator, and serves `/oauth2/revoke`; `a2a.login` beside
`a2a.tls.client_ca`, on a unix listener, or at a provider whose
`sign_in.grants` lacks `authorization_code` is refused at load. A device
session enrols the same way from its display client: `delegation.enrol
{idp}` answers with a task whose `signIn` annotation is the start link
([Display clients](interface.md#acting-for-you-at-an-mcp-server)).
[`examples/okta-xaa-mcp.yaml`](../examples/okta-xaa-mcp.yaml) is a whole
configuration of it: people signing in through `a2a.login`, a pinned
laptop, an agent-mode chat server their template lets their work use, and
an operator's `to: principal` rule.

**Ending a session ends what it delegated.** A run, a conversation turn and
a subagent started through a session act through it: `auth.sessions.revoke`,
`/oauth2/revoke`, the session's expiry and a restart — every session is held
in memory — end it, and its work's next delegated call fails `revoked`; a
consent gate it waits on closes `revoked` at once, the call never made. The
grant itself stays: the person signs in again for new work.

**The person's approvals.** agentd tracks which work has read outside text
(*tainted*) and which holds the person's delegated data. A delegated
server's results are both — anyone can write a Linear issue body — unless
the operator sets `auth.trusted_output: true` on it, when they hold data and
taint nothing; an `untrusted_input` tool's result, a webhook's or a
non-operator's message, a peer's answer and the rest taint
([Security model](security.md#acting-for-an-enrolled-person)). A person's
own message — sent through a session whose principal is the grant's, a
device session having itself signed in as the identity its name is bound
to, or by a per-person operator over the unix listener — is theirs: it
taints no work acting with their grant, and taints any other work it
reaches. Then, after every `security.policies` rule of the
operator's, these wait for the person, in this order: any `egress` call —
delegated or under agentd's own credential, a workflow step that sends the
run's data out by its own effect too (an `http` step, `a2a.send`,
`a2a.delegate`, an `emit` forwarded to a webhook or a peer, an `mcp.tool`
or `mcp.resource` step at a server tagged `egress`) — from work that is
tainted and holds delegated data
(`egress_from_tainted_work_holding_delegated_data`); a delegated call to a
`sensitive` or `egress` tool from tainted work
(`delegated_call_from_tainted_work`); an `exec` command from work holding
delegated data (`exec_holding_delegated_data`); a delegated `egress` call
from a workflow an instruction document defined, unless the person's
template lists it for the server by name and definition digest
(`templates[].servers.<name>.document_workflows`, the `hash`
`workflow.list` shows; `document_sourced_delegated_egress`); and a
delegated `egress` call at its first use per run — or conversation, or
subagent — and tool (`delegated_egress`). An operator adds rules of their
own: an `ask` with `to: principal` is the person's gate as these are, on
any `security.policies` matcher — the facts `delegated`, `tainted`,
`holds_delegated_data`, `unattended`, `document_sourced` and `mode`
included — and `once_per: run_tool` makes it ask at the first use alone; it
takes no `question` (the text is agentd's) and denies unanswered.

```yaml
# fragment: under security:
policies:
  # Every delegated write to the tracker asks, at its first use in a run.
  - {match: {tool: "create_*", mode: delegated}, action: ask, to: principal, once_per: run_tool}
  # Nothing a schedule started sends anything out.
  - {match: {unattended: true, any_tags: [egress]}, action: deny}
```

The call gates, `agentd --delegation-list` shows it
— agentd's own text of it, with the server, the tool, the arguments quoted
and escaped, the scopes the token carries, the run, and what tainted it —
and the person answers it: a per-person operator with `agentd
--delegation-approve <gate>` or `--delegation-deny <gate>` over the unix
listener; anyone else from their display client, where the gate is shown on
the task of the work it holds (`input-required`, badged as agentd's
security gate, its `consentGate` annotation naming it) and answered with
`delegation.approve` / `delegation.deny` through a session whose own IdP
sign-in is younger than `identity.delegation.consent_session_max_age` (12
hours by default; sign in again for an older one); a certificate principal
over the connection its certificate authenticates. An answer is bound to
the gate's nonce and the hash of its arguments, and an approved step runs
with the spec the person saw. Gates of one shape — the same `shape` in
`delegation.list`: rule, server, tool and argument names alike — may be
answered together (`delegation.approve {gates: [{gate, nonce, args_hash},
…]}`, `agentd --delegation-approve <gate>,<gate>`), each bound and recorded
on its own. At most `identity.delegation.consent_per_minute` gates (10 by
default) open for one person a minute; a call past that is refused rather
than asked (`delegation.consent.rate_limited`). An approved first use is
remembered for the unit of work and the tool, across a restart, for at most
a day. No operator answers for the person unless
their template lists `approvers: [principal, operator]` (recorded `via:
operator`), no message on the gate's task is an answer — each is refused and
recorded — no judge answers whatever `agent.ask_human_fallback` says, and an
unanswered gate denies after `identity.delegation.consent_timeout` (24 hours
by default). When the IdP asks for a fresher sign-in
(`insufficient_user_authentication`), the call waits on a sign-in gate
instead: the person signs in again — at the start link it carries, for a
display session's work, or with the IdP's device code for a per-person
operator's, where `sign_in.grants` offers the device grant — and the call
is made with the fresh ID token — that call alone: its server, its run or
conversation and its step; another call to the same server, even the
person's own in another task, mints as it would have; a sign-in as anyone
but the grant's own person is `binding_mismatch`. The start link asks the
IdP for `max_age` and `prompt=login`, and the sign-in must be as recent as
the IdP asked: its ID token's `auth_time` within that `max_age` — or, where
the IdP named none, after it asked, a moment the gate's wait record keeps
for a device grant a restart resumes. A sign-in older than that, or whose ID
token carries no `auth_time`, is refused and the call fails `step_up`. Each decision is a record of the audit chain whose content — what
the person saw — is sealed under their own key: `agentd --delegation-audit`
reads it back, and `agentd --delegation-forget <principal>` erases the
person — the gates open for them denied first, then their grants revoked
and their key destroyed — so their records stay in the chain, unreadable,
and no key is made for them again.

**Pins, not discovery.** `audience` (the authorization server's issuer) and
`resource` are required, and discovery only confirms them: before anything
is minted for the server, its RFC 9728 metadata, read from the well-known
locations on its endpoint's origin, must describe the pinned `resource` byte
for byte and list the pinned `audience` (otherwise the call is `config`), and
the authorization server's metadata must list the JWT bearer grant and the
`urn:ietf:params:oauth:grant-profile:id-jag` profile (`auth.id_jag: assume`
for one that takes it unlisted). The IdP's metadata is read from its own
issuer only — both the RFC 8414 and the OpenID document, merged, since Okta
splits `jwks_uri` and the identity-chaining member between them. An IdP
whose issuer is the server's authorization server is refused (ID-JAG §9.3),
and so, under `profile: okta`, is an `/oauth2/<id>` issuer.

**Renewal.** An access token is used until `exp − 60 s`, at most
`identity.delegation.at_cache_cap` (60 minutes) whatever `expires_in` says,
and only while the grant's refresh token was proved alive within
`identity.delegation.liveness_interval` (15 minutes): a stale proof is renewed
first by a plain `refresh_token` grant, and an IdP that does not answer then
is `idp_unavailable` — never a success on an old token. A `401` on a token
that was not just minted re-presents the unexpired ID-JAG (no new mint)
unless `id_jag_reuse: never`, or the authorization server refused a
re-presented one before; a `401` right after a mint is the server's fault
(`delegation_rejected_by_resource`), backed off. A rotated refresh token is
kept, compare-and-swap on its version, before it is used — and before the
ID-JAG it came with is checked, so a refused ID-JAG never leaves the spent
token in the vault; one IdP call per
grant runs at a time, so two servers minting at once never present one
rotating token twice. `insufficient_user_authentication` is `step_up`: the
call waits on a sign-in gate (above) — or a per-person operator runs
`agentd --login idp:<name>` again — and the next mint presents that fresh ID
token. Mints are budgeted per day and month per server
(`templates[].budget`, 16 and 200 by default, 80% of Okta's 250 a month),
per run (12), and per person and server in any minute (`per_minute`, 4 by
default, every mint asked for counted, whatever the IdP answers), so a
steered loop over scope sets or a server's string of refusals spends
minutes of mints, never the day's; `delegation.quota_warn` fires at 80%.

**Pairwise subjects.** With `identity.providers.<name>.subjects: pairwise`,
an ID-JAG's `sub` is recorded and not compared with the grant's: the IdP
may give each resource its own subject for the person. A server is sent the
ID-JAG's own `sub` as `agent/acting_for` (`attribution: none` sends nothing)
and `agent/grant`, a random handle per grant and server kept in memory —
never the grant's id or the person's principal — so two servers share no
handle to correlate the person by.

**Agent-mode servers beside a delegation.** Work acting with a person's
grant — their run, their conversation's turn, a subagent that inherits —
is not offered an agent-mode MCP server tagged `sensitive` or `egress`, and
a call to one is refused (`mode_mixing`): agentd's own credential there is
broader than the person's grant, and a steered model would reach for it.
`templates[].agent_mode_servers` lists the ones the person's work may still
use; each of their calls comes back to the supervisor and is recorded
(`delegation.agent_mode_call`). The load refuses the same resource or
authorization server under a delegated and an agent-mode server.

**A delegated server's questions.** A server that sends an elicitation on a
connection made with a person's grant asks that person alone: the question
is a gate of its own, owned by and addressed to them, in agentd's words with
the server's quoted. It is answered as their consent gates are — over the
unix listener, over their certificate, or from a display session whose own
IdP sign-in is recent enough — and only through a session that holds the
grant it was asked through: a device session under the same name that never
signed in as the identity the name is bound to is refused
(`delegation.elicitation.refused`). No operator answers it for them — an
attempt is refused — no judge and no approval policy does, and unanswered
within the server's wait it is cancelled. A URL elicitation is declined.

**Revocation.** `agentd --delegation-revoke operator:alice`, or
`--delegation-revoke idp:okta#00u1alice` for every grant of that IdP
subject, revokes the refresh token at the IdP (RFC 7009, retried), writes the
tombstone, deletes the sealed record and drops every token. A person revokes
their own grant from their own session with `delegation.revoke {}`; anyone
but an operator revokes their own alone — the grants the session they ask
through holds, so a device session under the same name that never signed in
as the identity the name is bound to revokes nothing. A revoked grant's calls fail
`revoked` for as long as the tombstone register keeps the revocation, a
restart included; enrolling again makes a new grant one epoch up. An
`invalid_grant` from an exchange runs the liveness probe: if the refresh
token is dead too, the grant is revoked the same way; if it is alive, the
error is that server's alone and is backed off. `agentd --delegation-lint`
prints, from the configuration alone, the Resource connections each IdP must
have — exactly those: the IdP's list, not the template, is the real ceiling.

**What a worker never holds.** Delegated servers are the supervisor's: no
worker, subagent or instance is handed one to dial, every call comes back to
the supervisor, and its answer — or its refusal, a server's error quoting the
token included — reaches the model, the log and `delegation.denied` with
every live token value — the refresh token, ID tokens, ID-JAGs and access
tokens, as they are, base64- and percent-encoded — replaced by
`[redacted]`. A secret under
`identity.*` or on a delegated server is read from a mounted file
(`{{secret-file:…}}`) or entered at the startup prompt, never from the
environment, and no child is started with the variables one names. A served
document may not declare a delegated server, set any of its pins or its
client authentication, or reference any of those secrets by any spelling —
a reference is compared by what it resolves to, and a `*_file` field's path
as a file (RFC 0042, as RFC 0044 §5.14.1 amends it). `security.exec.enabled`
beside `identity.providers`, a seal key or a delegated server loads only
under the whole isolation set — `spawn.uid` and `spawn.gid`, Landlock, and
every path only the supervisor may open closed to the worker uid by mode
and ownership — and the load names each piece missing. Where the kernel has
Landlock, no child can open the store's directory, where the vault is, a
mounted key file or the audit chain ([Security model](security.md#secrets));
a delegation several people can hold grants under is refused on a kernel
without it unless the worker uid closes them. While the instruction is
frozen — its trust `freshness` deadline passed unconfirmed — no model turn
and no run of a workflow it defined makes a delegated call.

**Keys an external signer holds.** With `identity.delegation.key_custody:
external`, agentd holds none of the keys: a signer process on a unix socket
does, and agentd asks it to sign each client assertion — at the IdP and at
each delegated server's authorization server — and to wrap the data key each
grant's refresh token is sealed under. Each key is named by its
`client_auth.kid`, which is also the `kid` the assertion carries; no `key`
and no `seal_key` is given. The socket's peer must be the uid configured,
which should not be agentd's own (the load warns), and the start checks that
the signer holds every key under the algorithm configured. With the signer down, a delegated call fails
`signer_unavailable` and nothing is sent. The protocol, for a signer of your
own in front of a KMS or an HSM, is
[The external signer protocol](external-signer.md).

```yaml
# fragment: under identity:
providers:
  okta:
    issuer: https://acme.okta.com
    profile: okta
    client_id: 0oa9agentdXAA
    client_auth: {method: private_key_jwt, kid: agentd-idp-2026}
delegation:
  key_custody: external
  signer: {socket: /run/agentd-signer/signer.sock, uid: 991, seal_key: vault-kek-2026}
```

**The audit chain anchors revocation.** With `observability.audit.chain`
set, enrolments, mints, uses, refusals, revocations and the person's consent
answers are lines of a hash-chained log verified at every start, and each
revocation and refresh-token version is anchored there before the store is
written. A store restored from before a revocation then refuses the revoked
grant as it loads: the person's calls are `revoked` until they enrol again,
and nothing is presented to the IdP.

**A reload** applies `identity.delegation.templates`, `device_pins`,
`device_first_binding`, `consent_per_minute`, `consent_session_max_age` and
`consent_timeout`: a
narrowed template holds every grant it allowed to the new ceilings at the
next call; a template removed, a server removed from one, a pin changed or
removed, and first binding turned off revoke the grants they allowed — at
the IdP, tombstoned, `why` said in `delegation.revoked` — and drop the IdP
proofs of those device names' sessions. Adding or changing a per-person
operator's or a certificate principal's rule is a restart.
`security.policies` reloads whole or not at all: a list that does not load
refuses the reload and the old one stands; one that loads holds from the
next call, while a gate already open keeps the rule that raised it. A list
whose rules could hold a tool a running turn or subagent dials itself is
refused until that work ends.

**A first-use approval is kept with the run.** The person's approval of a
tool's first use in a run (`once_per: run_tool`) is stored with the run,
bound to the rule and the gate's shape, so the run's next use after a
restart asks nothing; it ends with the run, and a revoked grant or a
changed template asks again. A conversation's or a subagent's approval is
stored the same way: a subagent's ends with it, and every one is forgotten
a day after it was given.

### Callers with their own token

A person whose front end presents their enterprise access token
([Enterprise access tokens](a2a.md#enterprise-access-tokens)) is
`user:oidc:<iss>#<subject>` (`#<tenant>/<subject>` under `tenant_claim`):
the same person `principals: {source: oidc}` admits, and — where the
token's issuer is the login IdP's own and its `subject_claim` is `sub` —
the same principal `a2a.login` signs in. Where the token's issuer is not the
provider's own — an Okta custom authorization server beside the org server
the grant was enrolled at — `identity.providers.<name>.equivalent_issuers`
says its subjects are the provider's. A template's `clients` narrows it to
the front ends listed.

**No grant yet: the task asks for a sign-in.** A caller who starts a
workflow that calls a delegated server, holding no grant there while a
template would admit them, is asked to sign in before any step runs: the
task waits in `auth-required` with a start link, the sign-in at their IdP
is bound to the token's own subject and kept as their grant, and the run
starts by itself. A delegated *call* that finds no grant never asks: it
fails `not_enrolled`, so a steered model cannot make anyone sign in.

### The same-domain exchange

Some MCP servers sit behind the enterprise's own authorization server — the
person's IdP itself — where an ID-JAG's cross-domain grant has no place (the
draft's §9.3). For them, `grant: token_exchange` asks that authorization
server for a token on the person's behalf (RFC 8693, an access token for an
access token), for the server's pinned audience:

```yaml
# fragment: under mcp.servers[]:
name: issues
endpoint: https://issues.acme.example/mcp
auth:
  kind: oauth2
  grant: token_exchange
  idp: okta                 # identity.providers entry: the authorization server
  subject: grant            # or inbound
  audience: api://issues    # the aud the exchanged token must name
  resource: https://issues.acme.example/mcp
  scopes: ["issues:read"]
```

- **`subject: grant`** (default): the person's enrolled grant is refreshed
  at the IdP for an access token for agentd, which is exchanged there for
  the server's. The grant's budget, revocation and liveness hold as they do
  for `id_jag`.
- **`subject: inbound`**: the caller's own access token is exchanged — at
  the issuer that issued it, which must be the provider's issuer exactly,
  and nowhere else. No grant is stored and nothing is enrolled: the token is
  the subject, and the exchanged token lives no longer than it. Its tools
  come from the operator's file (`tools_from: file`). Once the caller's
  token has expired, or the IdP will not exchange it, a call fails
  `subject_expired`: the caller calls again with a fresh one.

agentd authenticates there as the provider's client: the block names no
`client_id`, `client_auth` or `token_url` of its own. The answer is held to
the pins: an access token, a `Bearer`, a JWT one naming the pinned audience
and the person, no scope beyond those asked, and any refresh token in it
dropped unread — renewal is a new exchange. The server's RFC 9728 metadata
must name the provider's issuer among its authorization servers.

### Work nobody is there for

A workflow's `act_for` names the person its `schedule`, `loop` and `once`
firings act for with nobody there, and how:

- **`via: ciba`** (preferred): each firing asks the person to approve it on
  their own device, by OpenID CIBA in poll mode, and acts with that
  approval alone.
- **`via: standing`**: each firing acts with the person's grant — their
  sealed refresh token — and nobody is asked.

```yaml
# fragment: under identity:
providers:
  okta:
    issuer: https://acme.okta.com
    ciba: {enabled: true}          # request_expiry 300s, per_day 10 by default
delegation:
  templates:
    - name: eng-tools
      principals: {source: operator}
      idp: okta
      servers: {linear: {scopes: ["issues:read", "issues:write"]}}
      unattended: ciba             # never (default), ciba, or standing
      unattended_tools: [linear.create_comment]
```

```yaml
# fragment: under workflows:
- name: weekly-report
  act_for:
    principal: {source: operator, id: alice}   # or {source: oidc, issuer, sub}
    via: ciba
  steps:
    tick:  {kind: schedule, cron: "0 7 * * MON"}
    issues: {kind: mcp.tool, depends_on: [tick], server: linear, tool: list_issues, args: {}}
    done:  {kind: finish, depends_on: [issues]}
```

**Who and when.** `principal` is typed, never a display string: a
pinned per-person operator or certificate principal by its
`a2a.principals` rule's `id`, or an IdP subject (`source: oidc`, `issuer`,
`sub`); a device principal is refused (`act_for_device_principal`), since a
device delegates only through a live session. Only the operator's own
configuration declares `act_for` — never an instruction document or a
definition `workflow.create` stored. A firing acts for the person; a
`manual` start's `workflow.run` runs as whoever asked. `act_for` beside a
`webhook`, `stream`, `correlate`, `subscribe`, `signal`, `event` or `a2a`
start, or a `webhook` wait, is refused at load (`act_for_trigger_not_allowed`):
whoever can fire one could make the person's phone buzz at every firing,
and drive an approved run with a payload of their own. The person's
template must allow the `via`: `unattended: ciba` allows `ciba`;
`unattended: standing` allows both. `standing` at a `profile: okta` IdP is
refused without `allow_against_idp_guidance: true`
(`standing_against_idp_guidance`) — Okta advises against XAA for work no
user started — and warned with it.

**Consent is to the definition.** Signing in — enrolling, or signing in
again — consents to every `act_for` workflow that names the person and
whose `via` their template allows, at the definition digest it has then
(`delegation.list` shows each grant's `unattended`). A firing acts only
while its workflow is that definition: a reload that changes it — an edit,
or a `url:` or `dir:` body that changed — suspends its firings until the
person signs in again. A firing that may not act — no grant, an expired or
revoked one, a template that no longer allows it, a changed definition —
starts no run and asks nobody; it logs `start.unattended.suspended` with
the code (`unattended_suspended` for a changed definition) and is audited.

**What a firing may do.** A firing's run acts through an authority of its
own, which every turn and child of its work acts through too, and which
ends with the run — a call on its way included: one whose firing ended
meanwhile mints nothing and is not made. A run its work starts — a
`workflow` step, `workflow.run`, or a signal or message that starts one —
is a firing of its own for the same person (`start.unattended.carried`),
acting for them only where its own workflow declares an `act_for` naming
them that they consented to (`unattended_suspended` otherwise); a
conversation turn the work starts by message acts with no one's grant.
Its work is unattended — the `unattended` policy
fact — and tainted, so the person's consent rules ask them, through their
display client or `--delegation-approve`, and deny unanswered after
`consent_timeout`. A delegated tool tagged `egress` is refused unasked
(`policy_denied`) unless the template lists that very tool, `<server>.<tool>`,
in `unattended_tools`. No step-up is asked for: nobody is there to sign in.
Every mint is recorded `unattended: true`, with its `via`.

**How `via: ciba` asks.** At a firing's first delegated call at the
person's IdP, the call waits while agentd asks them: a CIBA request for
`scope=openid` alone, its hint a fresh ID token from a `refresh_token`
grant — discarded once sent — or, where the IdP returns none on refresh, the
`preferred_username` they enrolled with, kept sealed under their personal
key. The `binding_message` their device shows is agentd's own: the
instance, the workflow, the run and the tool, never a payload. agentd polls
the token endpoint at the IdP's `interval`, 5 s longer after each
`slow_down`; `requested_expiry` (`request_expiry` under `profile: okta`) is
`ciba.request_expiry`, 300 s by default. The approval's ID token must name
the grant's own `iss` and `sub`, or the firing fails closed
(`binding_mismatch`): a hint can resolve to another account. It is then the
subject of that firing's exchanges alone — kept in memory, and the tokens
minted with it pooled for that firing alone — until it expires, after which
the firing's calls fail `subject_expired`. That holds for a `grant:
token_exchange` server too: its exchange presents the approval's ID token
(`subject_token_type` `urn:ietf:params:oauth:token-type:id_token`), binding
the token it gets to that approval, never the grant's access token; an
authorization server that takes no ID token subject (`invalid_request`)
fails the call `config`, and so, before the person is asked, does a
`profile: okta` IdP, whose on-behalf-of exchange takes an access token
alone — run such a server `via: standing`. A firing asks once at a grant —
its run's record says so before the request is sent: another call it makes
while the request waits waits on the same request, and a refused or
expired approval is never asked again. At most one request per person and
workflow waits at a time — once a firing's wait closes, however it closes,
another firing may ask — and at most `ciba.per_day` (10) are sent a person
a day (`budget_exhausted` past either). A refresh token the IdP issued with
an approval anyway is kept sealed in the store — as a grant's refresh token
is, under `seal_key` or a data key the external signer wraps — while the
run lasts, and revoked (RFC 7009) and forgotten when it ends; one a crash
or a restart left there is revoked at its IdP and forgotten at the next
start's first pass, before any request is sent. Where that pass cannot
finish — the external signer down, or the store not answering — the
record stays and the pass is tried again, 5 s later and doubling to
5 min, until it does, whether or not a request comes; a request a firing
makes meanwhile is not sent, and its call fails `signer_unavailable` (or
`idp_unavailable`). One that cannot be kept sealed (the signer down) is
revoked at once, and the approval fails `signer_unavailable`.

**A restart asks once more.** The request id and the approval are never
written down, so a restart loses them; the firing then asks the person once
more — one new request, its binding message saying `again`, nothing of the
old one used — and its run's record says so before it is sent
(`unattended.asked.<grant>: again`). A step that was waiting on the person
waits on the new request; a later call of a firing they had approved asks
when it finds no approval. A second restart while that request is out
fails the call closed (`ciba_denied`), and one after it was approved fails
the next call `subject_expired`: never a third request.

**How a waiting run shows.** The step is suspended on its wait —
`{kind: consent, signin: {how: ciba}}` in its wait record — for up to the
request's expiry; the gate is among the person's own in
`delegation.list`, and the run's task, when it has one, is in
`auth-required` with agentd's words. A denial or an expiry fails the call
`ciba_denied`; the person may also deny the gate itself
(`--delegation-deny`). After a restart the run shows its second wait as it
showed the first: the same step suspended on the same wait, for the new
request, the gate among the person's again, the task in `auth-required` —
and its record naming the grant `again` under `unattended.asked`; the log
says `delegation.ciba {decision: asked_again}`. Nothing is added to the
workflow.

### When a grant is gone mid-run

`auth.on_revoked: ask` on a delegated server lets a workflow step wait for
the person rather than fail, when the grant it needs was revoked or has
expired, or the session its run acts through has ended: the step waits —
among the person's gates, and on the run's task in `auth-required` with
agentd's words — for at most `identity.delegation.consent_timeout`, and is
made again, with no message, once the person enrols again (the same
principal and IdP subject) or, for a run whose session ended, once a live
session of theirs holds an IdP proof of that very identity: the run moves
to it, logged and audited as `delegation.session.rebound`. Only a workflow
step's own call waits; a model's call, and a call from work that has read
outside text or that nobody is there for, fails `revoked` as with
`on_revoked: fail` (the default) — so a steered model cannot make anyone
sign in. A call that waited before any rule was consulted meets the
operator's rules and the person's consent rules when it is made again.
Unanswered, or denied, the step fails `revoked`.

### Sender-constrained tokens

Where a delegated server's RFC 9728 metadata and its authorization
server's metadata both advertise ES256 DPoP proofs
(`dpop_signing_alg_values_supported`), its access token is bound to a key
agentd makes in memory at first use and never writes down (RFC 9449): the
redemption carries a proof — sent once more with the nonce the server asks
for (`use_dpop_nonce`) — and must come back `token_type: DPoP`; each MCP
request presents the token as `Authorization: DPoP` with a proof of that
request, sent once more with the nonce an MCP server's `use_dpop_nonce`
challenge issues. Where they do not, neither leg has one. `auth.dpop: off` never
binds; a server whose metadata takes bound tokens alone
(`dpop_bound_access_tokens_required`) is then refused, as it is behind an
authorization server that advertises no DPoP, before anything is minted
for it.

**What this preview does not do yet:** live interop against an Okta
Integrator org or xaa.dev; DPoP on the exchange leg at the IdP, and the
other deferred items of RFC 0044 P4.

### Fields

| Field | Under | Meaning |
|---|---|---|
| `issuer`, `client_id` | `identity.providers.<name>` | the IdP, pinned; agentd's client there (required) |
| `profile` | `identity.providers.<name>` | `generic` (default) or `okta`: keys at `{issuer}/oauth2/v1/keys`, subject `refresh_token`, no `/oauth2/<id>` issuer |
| `client_auth`, `client_secret` | `identity.providers.<name>` | `private_key_jwt` with an ES256 or RS256 (`alg`) PKCS#8 key, or a secret; a public client is refused |
| `sign_in` | `identity.providers.<name>` | `grants` (`device`, `authorization_code`), `scopes` (default `[openid, offline_access]`) |
| `subject`, `subjects`, `jwks_uri` | `identity.providers.<name>` | what the exchange presents; whether an ID-JAG's `sub` must be the person's; the key set's pin |
| `equivalent_issuers` | `identity.providers.<name>` | `a2a.inbound_tokens` issuers whose subjects are this provider's: a caller's token from one finds the grant enrolled here |
| `ciba` | `identity.providers.<name>` | `{enabled, request_expiry, per_day}`: OpenID CIBA in poll mode for `via: ciba` firings (`false`, `300s`, `10`) |
| `seal_key`, `seal_key_previous` | `identity.delegation` | 32 bytes, hex or base64, as a secret reference (`key_custody: local`) |
| `key_custody`, `signer` | `identity.delegation` | `local` (default) or `external`; the signer's `{socket, uid, timeout, seal_key, seal_key_previous}` |
| `at_cache_cap`, `liveness_interval`, `consent_timeout`, `consent_session_max_age`, `consent_per_minute` | `identity.delegation` | `60m`, `15m`, `24h`, `12h`, `10` |
| `templates` | `identity.delegation` | `{name, principals: {source, role, labels}, idp, servers: {<name>: {scopes, document_workflows}}, ttl, budget: {per_day, per_month, per_run, per_minute}, subagents, approvers, agent_mode_servers, clients, unattended, unattended_tools, allow_against_idp_guidance}`: `source` is `operator`, `oidc`, `x509` or `device`; `subagents` `none` (default) or `inherit`; `approvers` `[principal]` (default) or `[principal, operator]`; `clients`, the front ends whose callers' tokens it admits (default any the issuer lists); `unattended` `never` (default), `ciba` or `standing`; `unattended_tools`, the `<server>.<tool>`s tagged `egress` a firing may call |
| `device_pins`, `device_first_binding` | `identity.delegation` | `<device name>: {iss, sub}`, each name bound to one person; `false` (an unpinned name binds to its first enrolment when `true`) |
| `idp`, `token_ttl` | `a2a.login` | the IdP display clients sign people in at; their sessions' lifetime (`8h`, `5m`–`30d`) |
| `grant: id_jag`, `idp`, `audience`, `resource`, `client_id` | `mcp.servers[].auth` | the delegated server's IdP, its authorization server, itself, and agentd's client there (all required) |
| `grant: token_exchange`, `idp`, `audience`, `resource`, `subject` | `mcp.servers[].auth` | a server behind the IdP's own authorization server: the IdP, the exchanged token's audience, the server, and `grant` (default) or `inbound`; no client of its own |
| `client_auth`, `client_secret` | `mcp.servers[].auth` | agentd's authentication at the authorization server; never the IdP's key |
| `scopes`, `tool_scopes`, `scope_mode` | `mcp.servers[].auth` | baseline and per-tool scopes; one token per server with their union (`server`, default) or per scope set (`per_tool`) |
| `tools_from`, `tools_file` | `mcp.servers[].auth` | `first_grant` (default) or `file` |
| `id_jag`, `id_jag_reuse`, `allow_unbound_at`, `attribution`, `max_at_age` | `mcp.servers[].auth` | `discover`/`assume`; `auto`/`never`; accept an ID-JAG that names no resource; `sub`/`none`; a stricter cap |
| `trusted_output` | `mcp.servers[].auth` | the operator vouches nobody outside writes what the server answers: its results hold the person's data and taint nothing (default `false`) |
| `on_revoked` | `mcp.servers[].auth` | `fail` (default), or `ask`: a workflow step whose grant is gone waits for the person to sign in again |
| `dpop` | `mcp.servers[].auth` | `auto` (default): DPoP-bound tokens where the server and its authorization server advertise ES256 proofs; `off` |
| `act_for` | `workflows[]` | `{principal, via}`: the person a workflow's `schedule`, `loop` and `once` firings act for with nobody there; `via` `ciba` or `standing` |
| `uid`, `spki`, `idp_subject` | `a2a.principals[]` | the person a rule pins: a per-person operator by the unix peer's uid, or a certificate principal by its key's SPKI SHA-256 (`match.spki`), and their exact IdP identity |
| `to: principal`, `once_per` | `security.policies[]` | an `ask` that is the person's consent gate; `run_tool`: at the first use per run and tool alone |

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
| `resource` | `oauth2` | RFC 8707 `resource` on every authorization, token, refresh and client-credentials request: `auto` (the endpoint, canonical), `omit` or a URI; unset, `auto` on an MCP server or MCP catalog entry and `omit` elsewhere |
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
