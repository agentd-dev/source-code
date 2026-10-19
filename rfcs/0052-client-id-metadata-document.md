# RFC 0052: A Client ID Metadata Document for the authorization servers agentd has no registration at

**Status:** Accepted 2026-10-19, as track C of RFC 0048 (§5.3); implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Builds on:** RFC 0031 (endpoint auth, the `oauth2` block), RFC 0047 (the companies preview's client metadata document and `private_key_jwt`), RFC 0042 (what a served document may set), ADR 0001 (MCP extensions and authorization: the client side only), ADR 0002 (the standards strip)
**Standards:** OAuth Client ID Metadata Document, [draft-ietf-oauth-client-id-metadata-document-02](https://datatracker.ietf.org/doc/html/draft-ietf-oauth-client-id-metadata-document-02) (2026-07-06, OAuth Working Group, intended Standards Track), cited as [CIMD]. MCP 2026-07-28 authorization, [client registration](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/b488c16623/docs/specification/2026-07-28/basic/authorization/client-registration.mdx) at `b488c16623`, cited as [MCP-CR]; it links CIMD -00, and nothing it requires of a client changed between -00 and -02. RFC 7591 (the metadata names), RFC 7523 (`private_key_jwt`), RFC 8414 (the metadata that advertises the document), RFC 8252 §7.3 and OAuth 2.1 (draft-ietf-oauth-v2-1-16) §8.4.2 (loopback redirect ports).

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

An OAuth client that has no registration at an authorization server can
still be known there: its `client_id` is an HTTPS URL, and the JSON at that
URL is its metadata [CIMD §3, §4]. MCP 2026-07-28 prefers this to dynamic
client registration, which it deprecates, and gives a client an order:
pre-registered credentials, then the document where the server advertises
`client_id_metadata_document_supported`, then dynamic registration, then
asking the user [MCP-CR].

Before this RFC an `oauth2` block needed an operator-registered
`client_id`. Now:

- **`security.client`** is agentd's OAuth client where it has no
  registration: the `client_id` URL, its name, its key, the grants and
  loopback redirect URIs the document lists. One implementation,
  `auth::client_metadata`, builds it and the companies preview's identity.
- **An `oauth2` block with no `client_id`** presents the document's URL as
  its `client_id` and authenticates with a `private_key_jwt` assertion
  signed by `security.client`'s key — at an authorization server whose RFC
  8414 metadata advertises the document. Where it does not, `--login` and
  the daemon's first token request say what to register. A configured
  `client_id` always wins; dynamic registration is not implemented.
- **`agentd --print-client-metadata <dir>`** writes the document and its key
  set for static hosting, or `security.client.serve` has the A2A listener
  serve them.

## 2. What [CIMD] and [MCP-CR] ask of a client, and agentd

| Requirement | agentd |
|---|---|
| The `client_id` is an `https` URL with a path, no userinfo, no fragment, no `.` or `..` segment; a query SHOULD NOT be used; compared as a string [CIMD §3] | `security.client.id` is held to all of it at load (`client::client_id_url_error`), and so is `security.companies.client.id`. agentd also refuses a query and a `/` path (§3 calls the latter NOT RECOMMENDED): a printed or served document has a path of its own. The URL is sent as written, never normalised. |
| The document is JSON with RFC 7591 metadata names and a `client_id` equal to its URL [CIMD §4] | `client_id`, `client_name`, `logo_uri` (when set), `jwks_uri`, `redirect_uris`, `grant_types`, `response_types`, `token_endpoint_auth_method`. |
| At least `client_id`, `client_name`, `redirect_uris` [MCP-CR] | All three, always; `redirect_uris` is `[]` without the authorization-code grant. |
| No shared-secret method, no `client_secret` or `client_secret_expires_at`, no private key; public `jwks` or `jwks_uri` only [CIMD §4.1] | `token_endpoint_auth_method: private_key_jwt` and a `jwks_uri`. `client_metadata::check` holds every document agentd prints or serves to §4.1 — both identities' — before it leaves the process. |
| `private_key_jwt` makes the client confidential, and the server then requires RFC 7523 client authentication [CIMD §8.2] | Every request to the server authenticates: the token endpoint for each grant, and the device authorization endpoint (RFC 8628 §3.1 has a confidential client authenticate there as at the token endpoint — a gap agentd had for every confidential client, closed here for all of them). |
| Use the document only where the server advertises `client_id_metadata_document_supported` [CIMD §6, MCP-CR] | The flag is read from the issuer's RFC 8414 metadata at `--login` and at the daemon's first token request; without it nothing but the metadata request is sent. |
| Pre-registered, then the document, then dynamic registration, then ask the user [MCP-CR] | A configured `client_id`, then the document. Dynamic registration is not implemented (MCP deprecates it); the refusal tells the operator what to register — §3, D2. |
| Credentials keyed by the authorization server's issuer; a document's `client_id` is portable [MCP-CR, Authorization Server Binding] | The document needs no registration state. The tokens a login obtains are kept with the authorization server they came from and never presented elsewhere (D5). |

## 3. Design decisions

**D1. One client identity, named `security.client`.** It sits under
`security` beside the other identities agentd presents (`security.aauth`,
`security.companies.client`), is restart-only like every `security` path,
and is operator-only: no served document may name who agentd is to an
authorization server, nor which key signs for it (RFC 0042). Its fields are
`id`, `name`, `logo_uri`, `jwks_uri` (default: `jwks.json` beside the
document), `key_file` (default `/var/lib/agentd/client-key`, an ES256
PKCS#8 key created `0600` when absent, its `kid` the RFC 7638 thumbprint),
`published_key_files` (keys listed but never signed with, for rotation),
`redirect_uris`, `grants` (default `[device]`) and `serve`. It needs the
`oauth` feature, which the release binaries carry.

`auth::client_metadata::Identity` is the one implementation: it loads the
keys, builds the document from the members its caller gives and the ones
every identity has (`client_id`, `jwks_uri`, `token_endpoint_auth_method:
private_key_jwt`), checks it, and prints, serves and signs. The companies
preview's `Identity` is built on it and adds only what the Personal Agent
Protocol does — the Session assertion and the self-check. The preview keeps
a document of its own because the two documents must differ: the protocol
holds every `redirect_uris` entry to an HTTPS URL on the `client_id`'s
domain (draft 0.1 §4.1), where this document lists loopback URIs for the
authorization-code grant, and it adds an `extensions` member that is no
RFC 7591 name.

**D2. Which block uses it, and where.** An `oauth2` block — on an MCP server,
a service entry, a model connection or an A2A peer — that names no
`client_id` uses the document. Lowering the block to its runtime spec
(`Settings::auth_spec`) fills in the document's URL as `client_id`,
`private_key_jwt` with `security.client.key_file` as its key (read at each
token request, by the process that makes it, as a configured
`client_auth.key` is), and `client_id_metadata_document: true`. The block
must name `issuer` — only the server's metadata says whether it takes the
document — and no `client_secret`; of `client_auth` it may set only
`assertion_aud` (default the token endpoint, as for any block). Its grant
must be one `security.client.grants` lists. Each is refused at load,
naming the field.

At the first request that needs the server — `--login`, or the daemon's
first token or refresh request — agentd reads the issuer's metadata. With
`client_id_metadata_document_supported: true` it proceeds; otherwise it
fails that request before sending anything else:

> the authorization server `<issuer>` does not advertise
> client_id_metadata_document_supported, so it would not fetch agentd's
> Client ID Metadata Document (`<url>`); register agentd there — a client
> authenticating by private_key_jwt with the key set at its jwks_uri, or by
> a client secret — and set the block's `client_id` (and its `client_auth`
> or `client_secret`). agentd does not register dynamically …

An `invalid_client` from a server that was given the document names the
document's URL and says to check that it serves what
`--print-client-metadata` writes and that its key set holds the signing
key. A block that names a `client_id` is unchanged.

It applies to every endpoint because it falls out of the one lowering:
model connections and A2A peers build their signers from the same runtime
spec as MCP servers, and `--login`'s targets already cover them. The MCP
revision's rules are applied wherever the document is used.

**D3. The grants.** `device` and `client_credentials` need nothing more.
The document's `grant_types` are those of `grants` —
`urn:ietf:params:oauth:grant-type:device_code`, `client_credentials`,
`authorization_code` — with `refresh_token` beside a grant a person signs in
with, whose tokens agentd renews; `response_types` is `["code"]` with the
authorization-code grant and `[]` otherwise. RFC 7591 §2 gives an absent
`grant_types` as `authorization_code` alone, so the list is always written.
A `client_credentials` grant is now fetched by the same helper as every
other grant (`oauth2::client_credentials`), with the block's client
authentication: a secret, or a `private_key_jwt` assertion. So
`client_auth` applies to `client_credentials` blocks with a `client_id`
too. Its token is held in memory and fetched again a minute before it
expires, or halfway through a lifetime shorter than two minutes, so a
short-lived token is still reused.

`authorization_code` returns the browser to a redirect URI the document
must list [CIMD §4.2], compared as a string. `security.client.redirect_uris`
takes loopback IP literals only — `http://127.0.0.1[:port]/path` or
`http://[::1][:port]/path`, never `localhost` (RFC 8252 §8.3) — because the
login's loopback listener is the one place agentd receives a redirect. The
login uses the first. With a port, it listens on that port and sends the URI
exactly, which matches under [CIMD]'s exact comparison whatever the server
does about ports. Without one, it listens on a free port and sends the URI
with that port, for a server that applies the loopback rule of RFC 8252
§7.3 and OAuth 2.1 §8.4.2 ("MUST allow any port to be specified at the time
of the request for loopback IP redirect URIs"); [CIMD] itself says nothing
of ports, and [MCP-CR]'s example lists fixed ports. `authorization_code` in
`grants` without a redirect URI, or a redirect URI without it, is refused at
load.

**D4. Publishing.** The server fetches the document from its URL, so the URL
must be public HTTPS (RFC 0048 §5.3's dependency). Two ways:

- `agentd --print-client-metadata <dir>` writes `client.json` and
  `jwks.json`, creating the key `0600` when absent, and dials nothing. Host
  them at `id` and `jwks_uri`. The companies preview's
  `--print-company-client` stays: it prints the other identity, by the same
  code (`Identity::print`).
- `security.client.serve: true` has the A2A listener serve both at their
  URLs' paths, `Cache-Control: max-age=300`, beside the companies preview's
  documents — the listener already takes a list of public documents. It
  needs the `a2a` feature, `a2a.listen`, `a2a.url` the `client_id`'s origin,
  `jwks_uri` on that origin, and paths the listener and the companies'
  served identity do not use.

The daemon loads the identity at startup, before any connection dials, so a
key that does not load is exit 2 and an absent one is created. No
self-check fetches the published document (the companies preview has one,
because its protocol's errors cannot name the cause): the authorization
server's `invalid_client` is answered with where the document is (D2).

**D5. Stored credentials stay with their authorization server.** [MCP-CR]
binds stored credentials to the issuer of the authorization server that
issued them. The document's `client_id` itself needs no record. The login
cache stays keyed by the login target (`mcp:<name>`, `service:<name>`, …),
and each record now names the authorization server it was obtained from
(`CachedCred::authorization_server`): the issuer the block names or RFC
9728 found, or, for a block that names none and pins its endpoints, its
token endpoint; a renewal keeps it. Under a block whose server is another
— its `issuer` changed, or added where only `token_url` was pinned — the
record is not presented, not renewed and not revoked: the request fails
naming the `--login` that fixes it, and `--logout` deletes it locally and
says to revoke it where it was issued. `--logout` compares the record with
the server it resolves, RFC 9728 included. A record that names no server
is presented nowhere.

**D6. Out of scope.** Dynamic client registration (RFC 7591); a
`software_statement` [CIMD §4.3]; `jwks` inline (the key set is a separate
document, so a rotation does not change the `client_id`'s document); a
client-side check that the published document matches (D4); the
`client_id_expires_at` the draft leaves as a TBD.

## 4. Configuration

```yaml
security:
  client:
    id: https://agent.example.com/oauth/client.json
    name: Example operations agent
    logo_uri: https://agent.example.com/logo.png         # optional
    jwks_uri: https://agent.example.com/oauth/jwks.json  # default: beside the document
    key_file: /var/lib/agentd/client-key                 # default
    published_key_files: []                              # the next key before a rotation
    grants: [device, client_credentials, authorization_code]   # default [device]
    redirect_uris: ["http://127.0.0.1:8765/callback"]    # authorization_code only
    serve: false                                         # serve both on the A2A listener
mcp:
  servers:
    - name: tracker
      endpoint: https://mcp.tracker.example.com/mcp
      auth:
        kind: oauth2
        issuer: https://auth.tracker.example.com          # required with no client_id
        grant: device
        scopes: [tracker.read]
```

`security.client` is restart-only and operator-only. Its schema is in
`config::settings::schema`; the guide is
[docs/authentication.md](../docs/authentication.md#client-id-metadata-document).

## 5. Security

- The key is the client's identity at every server that fetched its
  document: `0600`, never in a document, read only where a token request
  is made.
- Every document agentd publishes is checked for secrets and private key
  members before it is printed or served.
- A server that does not advertise the document is sent nothing but the
  metadata request; a token is never presented to an issuer other than its
  own.
- A loopback redirect URI is an IP literal, so no resolver decides where
  the code goes.
- Changing `security.client.id` makes agentd a new client everywhere
  [CIMD §8.3]: grants and consent made to the old URL stay with it.

## 6. Exit criteria

- Unit tests: the client identifier URL rules (each refusal), the loopback
  redirect parser, the document's grant and response types, the `check` of
  a document (secret members, secret methods, private key members, a
  missing key set), the identity's key file, key set and printed files, the
  lowering of a block with and without a `client_id`, each load refusal of
  `security.client` and of a block that uses it, a login from another
  authorization server — or one that names none — never presented, renewed
  or used after it expires, and a short-lived client-credentials token held
  for half its life.
- End to end against a mock authorization server on loopback
  (`crates/agentd-cli/tests/client_metadata_e2e.rs`): the printed document
  and key set; a device `--login` whose device and token requests carry the
  document's URL as `client_id` and an assertion the printed key set
  verifies, cached with its issuer; a logout after a login whose issuer RFC
  9728 found revoking the token there (`login_e2e.rs`); the daemon's
  client-credentials signer
  doing the same; a server that does not advertise the document refused at
  login and by the signer with only its metadata read; and, with `a2a`, the
  listener serving what the print writes. The browser login listening at
  the registered loopback URI, with and without its port
  (`browser_login_e2e.rs`).
- Docs: authentication (the guide), configuration, cli, interoperability;
  the strip lists "Client ID Metadata Document", draft -02, marked draft.
