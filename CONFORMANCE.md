# agentd conformance

agentd's conformance is judged by **behaviour**, against the specifications and
the documented contracts it serves — never by agreeing with its own types. The
evidence is the black-box suite in
[`crates/agentd-conformance`](crates/agentd-conformance): it knows agentd only
as a binary, drives the real process over its public surfaces, and never links
the agentd library, so a regression in what a caller sees fails a check even
when the implementation's own unit tests still agree with themselves.

This is agentd's own suite. It makes no claim about any external control
plane's contract.

## What it covers

Each check drives the real binary and asserts one contract. The checks are
grouped into families — the process's exit-code table, the security gates, the
durable store, crash-and-restore durability, the internal tool registry, the
core A2A surface, the observation feed of the events extension, the extension
contract itself, the authentication the card declares, agentd as an OAuth
client, and the fake Company agentd is tested against under the Personal
Agent Protocol draft. Every check is
registered in [`src/checks/`](crates/agentd-conformance/src/checks) as a
`Check { id, category, desc, run }`, which both the test harness and the report
runner pick up; the tables below are those registrations, and a unit test of
the suite fails when a row here differs from the check it names, or names one
that does not exist.

Each check is host-independent: conformance is judged against the
specification, never the environment, so there are no capability-gated checks to
skip.

### `supervisor`

| Check | What it proves |
|---|---|
| `supervisor/exit-0-on-success` | a completed once job exits 0 |
| `supervisor/exit-2-on-bad-flag` | an unknown flag is a usage error → exit 2 |
| `supervisor/exit-4-on-intel-down` | an unreachable intelligence endpoint → exit 4 |

### `security`

| Check | What it proves |
|---|---|
| `security/trifecta-refused` | granting one agent all three lethal-trifecta legs is refused at startup → exit 2 |
| `security/secret-not-in-telemetry` | the intelligence token never appears in the JSON-lines telemetry |

### `store`

| Check | What it proves |
|---|---|
| `store/boots-against-mcp-store` | a job backed by an MCP store connects, runs, and completes |
| `store/persists-completed-run-across-restart` | a restarted instance restores the completed run from the store and does not re-fire the once start |

### `durability`

| Check | What it proves |
|---|---|
| `durability/sigkill-restore-and-finish` | a SIGKILL before and during a step is recovered; a later life restores and completes |

### `tools`

| Check | What it proves |
|---|---|
| `tools/internal-round-trip` | internal tools (memory.set, plan.create) round-trip to the supervisor and take effect |
| `tools/unknown-tool-errors-not-crashes` | an unknown tool the model invents is answered as an error; the run still completes |
| `tools/registry-introspection` | --capabilities lists the internal tool registry |

### `a2a-conversation`

| Check | What it proves |
|---|---|
| `a2a-conversation/version-and-vocabulary` | a request whose A2A-Version is absent, empty, 0.3 or 1.1 is -32009 VERSION_NOT_SUPPORTED with its id echoed (a 1.0 patch is served); a method outside the specification's and the declared extensions' — a made-up name, the A2A 0.3 `message/send` — is -32601; neither creates a task |
| `a2a-conversation/read-commands-create-no-task` | every command the card lists as a `message` reply is answered with an agent Message or refused, never with a task — streamed, one message frame — and the task list stays empty |
| `a2a-conversation/agent-card` | the agent card is discoverable, names the configured agent, advertises streaming and a JSONRPC interface at A2A 1.0 |
| `a2a-conversation/card-declares-only-what-it-implements` | streaming, which the card advertises, streams a task to a terminal state; push notifications, which it disclaims, are refused by CreateTaskPushNotificationConfig with -32003 |
| `a2a-conversation/push-delivers-a-stream-response` | with a2a.push.enabled the card advertises push notifications, and a webhook registered with CreateTaskPushNotificationConfig is POSTed the task as a StreamResponse {task} (application/a2a+json) through to its terminal state |
| `a2a-conversation/protocol-errors-use-the-specified-codes` | an unknown method is -32601 and an unknown task is -32001, rather than a generic failure |
| `a2a-conversation/tasks-are-proto3-json-on-every-path` | SendMessage, GetTask and ListTasks all return the same proto3-JSON `Task`: state under `status`, an RFC 3339 timestamp, `ROLE_AGENT` |
| `a2a-conversation/nl-message-becomes-task-artifact` | a natural-language message runs a turn; the answer is the task artifact, readable via GetTask/ListTasks |
| `a2a-conversation/input-required-is-core` | with no extension enabled, ask_human puts the task in TASK_STATE_INPUT_REQUIRED carrying the question, and a SendMessage naming the taskId resumes the turn to completion |
| `a2a-conversation/task-history-carries-the-turn` | Task.history holds, in order, the caller's prompt under its own messageId, the agent's superseded question and the caller's answer, never the current status; historyLength 0 omits it and 1 keeps the newest |

### `events`

| Check | What it proves |
|---|---|
| `events/default-off-gate` | without a2a.events.enabled the card does not declare the events extension, its method is -32601 EXTENSION_NOT_DECLARED even to a caller activating it, and the core still answers |
| `events/feed-hello-and-replay` | with the feed on, the card declares the events extension; the feed takes only {fromSeq} (any other member is a plain -32602), then opens with a hello {seq, resume, resync, introspection, version} and replays from seq 0 a task event whose history holds the prompt |

### `extensions`

| Check | What it proves |
|---|---|
| `extensions/command-requires-activation` | a command whose extension the header does not activate is -32602 EXTENSION_NOT_ACTIVATED, one the message does not list is -32602 EXTENSION_NOT_MARKED, and neither runs nor creates a task |
| `extensions/method-requires-activation` | a method an extension declares is -32601 EXTENSION_NOT_ACTIVATED unless the header names that extension's URI exactly — naming the others or a look-alike does not — and is served once it does |
| `extensions/send-results-are-task-or-message` | every SendMessage result is exactly a task or a message, and every SendStreamingMessage frame exactly one of task, message, statusUpdate, artifactUpdate — for a conversation turn, a read command and a task command; a read streams as one message frame |
| `extensions/no-bare-agentd-metadata` | every metadata key in every reply is the URI of an extension the card declares; task annotations appear under theirs exactly when that extension is activated |

### `auth`

| Check | What it proves |
|---|---|
| `auth/card-declares-what-the-listener-enforces` | under no credential, a bearer, and a bearer beside an `any` rule, an uncredentialed call is served exactly when the card's securityRequirements admit it (none, or an empty alternative) and is otherwise a 401 challenging a declared scheme; the declared bearer is served and a wrong one is 401 |
| `auth/extended-card-needs-a-declared-credential` | on an `any`-rule daemon an uncredentialed GetExtendedAgentCard is 401 and the declared bearer reads it; on a no-auth loopback daemon the card's extendedAgentCard is not true and the method is -32004 |
| `auth/cors-exact-origins` | only a listed origin is granted (a2a-version preflighted, the challenge readable); its uncredentialed POST is a 401, never the implicit operator; an unlisted one — loopback too — is 403 with no grant; the card is public (ACAO *) |

### `mcp-auth-core`

| Check | What it proves |
|---|---|
| `mcp-auth-core/as-metadata-path-inserted-first` | for an issuer with a path, --login reads RFC 8414 metadata first, at the well-known URL with the suffix inserted before the path, and completes the device grant with the endpoints it names |
| `mcp-auth-core/as-issuer-must-match` | metadata naming another issuer than the configured one, by a trailing slash, is refused: --login fails and sends no device or token request |
| `mcp-auth-core/resource-metadata-path-inserted-first` | with no issuer configured, --login asks the MCP server, then reads its RFC 9728 metadata at the well-known URL with the suffix inserted before the server's path, before the root, and uses the issuer it names |
| `mcp-auth-core/logout-revokes-at-the-server` | --logout after a login revokes the refresh token at the revocation_endpoint the metadata names (RFC 7009: token and token_type_hint=refresh_token) |

### `companies`

| Check | What it proves |
|---|---|
| `companies/fake-issuer-matches-its-metadata` | the fake's RFC 8414 metadata, at the well-known URL with the suffix inserted before the issuer's path, names exactly the discovery document's issuer; the issuer-mismatch variant is refused for it |
| `companies/fake-lists-its-domain` | the metadata's poppy_domains lists the discovery document's organization.domain, the host that served it; the no-domains variant is refused for it |
| `companies/fake-publishes-es256` | the metadata's dpop_signing_alg_values_supported includes ES256; the no-es256 variant is refused, and the prefixed-algs variant is refused naming the prefixed member |
| `companies/fake-endpoints-on-its-host` | every URL the two documents name is on the Company's host; the off-host-endpoints variant is refused naming the field |
| `companies/fake-redirects-only-to-https` | the redirect-to-http variant's discovery document redirects to plain HTTP, and a Personal Agent that follows only HTTPS redirects refuses it |
| `companies/fake-starts-and-renews-sessions` | the JWT bearer grant with a Session assertion, a client assertion and a proof starts a signed-out Session (DPoP token, scope "", no-store); session_id renews it, asking it for a scope is invalid_scope, another User's assertion for it is invalid_session, and the record marks a use of the replaced token superseded |
| `companies/fake-rejects-another-aud` | a client assertion whose aud is the token endpoint or a list is invalid_client, and a Session assertion whose aud is the token endpoint is invalid_grant |
| `companies/fake-rejects-a-reused-jti` | a Session assertion sent twice is invalid_grant the second time, and a client assertion sent twice is invalid_client |
| `companies/fake-checks-the-assertion-claims` | a client assertion whose iss or sub is not the client_id is invalid_client, a Session assertion whose iss is not it is invalid_grant, and either assertion expired, issued in the future, living ten minutes, or with exp not after iat is refused the same way |
| `companies/fake-checks-the-client-document` | a client metadata document whose client_id is not its URL, whose jwks_uri is on another host, or whose key set holds a private key is invalid_client |
| `companies/fake-validates-every-proof` | at the token endpoint, no proof without an MCP resource, two DPoP headers, the wrong htm or htu, an iat ten minutes old or ahead, typ JWT, alg none, HS256 or one the Company does not publish, an ath, a private key in the jwk, a signature by another key and a replayed proof are each invalid_dpop_proof |
| `companies/fake-binds-tokens-to-their-key` | the API serves a DPoP Session Token with its proof; no proof, a missing or wrong ath, or a proof by another key is invalid_dpop_proof, the Bearer scheme is invalid_token, and a token in the URL is refused |
| `companies/fake-asks-for-a-nonce` | the nonce-required variant answers a proof without its nonce with use_dpop_nonce and a DPoP-Nonce header, at the token endpoint (400) and the API (401), and takes the retry that carries it |
| `companies/fake-mcp-takes-bearer-for-its-url` | a token request without a proof, with resource the MCP server's url, gets a Bearer token the server takes; unauthenticated, the server challenges with its RFC 9728 metadata, which lists the issuer; a DPoP token there, or a Bearer token from another MCP server, is invalid_token |
| `companies/fake-mcp-is-2025-06-18-or-later` | the fake's MCP server answers server/discover and initialize; the mcp-2025-06-18 variant answers only initialize, at 2025-06-18 |
| `companies/fake-device-sign-in` | device authorization needs a scope it can grant (else invalid_scope); polls are authorization_pending until the scripted User approves, then an Account Token and a signed-in Session Token; a poll sooner than the interval is slow_down; one asking for more than the User approved is invalid_scope, and the next gets what was approved; a decline is access_denied; the off-host-verification-uri variant sends the User to another host |
| `companies/fake-direct-sign-in` | the authorization endpoint redirects to an exactly registered redirect_uri with a code, the state and iss, and to no unregistered one; the code exchange with its PKCE verifier and session_id signs the Session in, once, and one asking for a scope the sign-in did not grant is invalid_scope |
| `companies/fake-account-tokens-rotate-when-it-says` | an Account Token gets signed-in Session Tokens without a new one; with the rotation variant each use returns a new Account Token and the spent one is invalid_grant |
| `companies/fake-revocation-signs-out` | RFC 7009 revocation of an Account Token makes it invalid_grant and signs its Session out at once, and the Session renews signed out; under rotation, revoking the current token signs out every Session of the sign-in, and the record calls a spent token's revocation spent |
| `companies/fake-keeps-a-session-to-one-account` | a Session once signed in to one account answers a sign-in to another with account_mismatch and issues no tokens |
| `companies/fake-rate-limits-with-retry-after` | the retry-after variant answers 429 rate_limited with Retry-After, answers 429 to a request sent before the wait is over, and serves one sent after it |
| `companies/fake-mcp-account-errors` | an MCP tool that needs the account answers a signed-out Bearer token with 403 sign_in_required, and a signed-in one without poppy:read with 403 insufficient_scope naming it |
| `companies/agent-refuses-a-company-that-fails-discovery` | agentd sends no assertion, no proof and no token request to a Company whose metadata names another issuer, lists no domains, leaves out ES256, names off-host endpoints, or whose discovery document redirects to plain HTTP |
| `companies/agent-publishes-its-client-id` | the client metadata document agentd prints is the one at its client_id, and the fake fetched it, held it to the kit's schema and accepted agentd's assertions against its key set |
| `companies/agent-assertions-name-the-issuer` | every client assertion and Session assertion agentd sends has aud the issuer as one string, iss its client_id, a minute's life and a 128-bit jti; the client assertion's sub is the client_id, the Session assertion's a User ID that names nothing about the person |
| `companies/agent-signs-a-proof-for-every-token-request` | every token request but the Bearer one carries a DPoP Proof the fake validated (htm POST, htu the token endpoint, no ath), each with its own jti |
| `companies/agent-uses-bearer-only-at-its-mcp-server` | agentd requests the MCP server's token without a proof, with resource the server's url and its session_id, and presents it at that server only, never a superseded one |
| `companies/agent-supports-nonces` | against the nonce-required variant, agentd's use_dpop_nonce is followed by one retry carrying the nonce, with fresh assertions and a fresh proof |
| `companies/agent-keeps-the-user-across-restarts` | after a restart agentd presents the same User ID and renews the same session_id, with a new DPoP key |

## Running it

```sh
cargo test -p agentd-conformance             # every check, one #[test] per family
cargo run  -p agentd-conformance             # the same checks as a PASS/FAIL report
cargo run  -p agentd-conformance -- --json   # the machine-readable record
```

The suite builds the agentd binary itself, so no prior `cargo build` is needed.
CI runs it as part of `cargo test --workspace --all-features`.

## The Personal Agent Protocol draft

agentd's Personal Agent role under the
[Personal Agent Protocol](https://personalagentprotocol.org) draft 0.1
([RFC 0047](rfcs/0047-acting-at-companies-for-a-person.md)) is built against
one revision of the draft, commit `95fcbf2` of its repository: the draft moves
without a version signal, so agentd follows one revision until it is
deliberately re-pinned. The
draft's specification, Operations extension and guides pages and the 19 JSON
Schemas of its Company kit are vendored, verbatim, under
[`crates/agentd/tests/companies/vendor/`](crates/agentd/tests/companies/vendor),
with the `PIN` that names the commit and the Apache-2.0 licence they are
published under. A drift test compares them, byte for byte, with a checkout of
that commit, which CI makes.

Nothing published tests a Personal Agent, so agentd tests against its own fake
Company, the hidden `--internal-mock-company` mode of debug and
`internal-mocks` builds with `oauth` (and the default `tls`). The `companies`
family holds that fake to the draft before anything is held to the fake: a
Personal Agent written in the suite from the draft drives it, and each check
passes only when the fake does what the draft says and, where a variant is
built to break that rule, the variant is caught. The fake's record of every
request it served — what was refused and why, the tokens it issued, and for a
request carrying a Session Token whether a newer one had replaced it, and the
claims of the assertions and proofs a token request carried — is what the
`companies/agent-*` checks count: they declare the fake as agentd's Company,
host the identity `agentd --print-company-client` writes, and hold the real
binary to the draft through the fake's record alone. Every
check quotes the sentence of the draft it tests, and the suite's unit test
requires each quote to appear verbatim, in its section, in the vendored
pages. The fake validates every document it sends against the vendored
schemas — their regex patterns and their references between files included,
which agentd's validator checks through a hand-written check per pattern —
and forcing tests keep that true across a re-pin: a pattern without a check, a
schema the fake does not load, a keyword the validator would skip, or a
reference that leads nowhere fails until it is handled.

The `mcp-auth-core` family covers the OAuth client pieces agentd builds once
for both the Personal Agent and enterprise-managed MCP authorization: RFC 8414
and RFC 9728 metadata at the path-inserted well-known URLs, the byte-equal
issuer, and RFC 7009 revocation on `--logout`.

## Beside the suite

Two further checks hold the A2A surface to the specification rather than to
agentd's reading of it:

- the listener's method table is compared, name by name, with the constants of
  the A2A project's official Rust SDK (`a2a-lf`), so a method agentd invented or
  misspelled fails, and so does a specification method it forgot to answer;
- every task, message and card agentd emits is built from that SDK's types and
  written by its canonical ProtoJSON, generated from the A2A protobuf, so the
  wire shape is the schema's.

## Adding a check

Append a `Check` to the relevant family's `checks()`. Its `run` function takes
`&Harness` and returns an `Outcome` (`pass` / `note` / `fail` /
`require(cond, why)`); the tests and the runner pick it up automatically, and
its row belongs in the family's table above, which the suite's own unit test
holds to the registration. A
change to a served surface — the exit codes, the A2A listener, the
configuration's admission rules, the store contract — comes with a check that
fails when the change is reverted.
