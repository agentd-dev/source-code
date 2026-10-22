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
client, agentd as a client of the MCP Tasks and Skills extensions, the
fake Company agentd is tested against under the Personal Agent Protocol
draft, agentd acting for an enrolled person through their IdP
(Enterprise-Managed Authorization, ID-JAG), and agentd paying with x402 —
over HTTP, MCP and A2A, on EVM and Solana — within ADR 0003's spend policy.
Every check is
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
| `mcp-auth-core/resource-metadata-must-name-the-server` | RFC 9728 metadata whose `resource` is not the MCP server's URL is refused: --login fails, names RFC 9728 §3.3, and asks the authorization server it names for nothing |
| `mcp-auth-core/server-chosen-urls-are-https-and-public` | an authorization server an MCP server's RFC 9728 metadata names off the server's own origin is refused when it is plain http:// or a link-local address |
| `mcp-auth-core/resource-on-every-grant` | RFC 8707: the device authorization, the device token poll, the daemon's refresh and a client-credentials token request to an MCP server's authorization server all carry `resource` — the server's URL in canonical form |
| `mcp-auth-core/rfc9207-iss-table` | the authorization-code login decides every RFC 9207 row — `iss` equal, different, or absent, with the metadata declaring support or not — refuses metadata without S256 before handing out a URL, and refuses an ID token without the login's nonce |
| `mcp-auth-core/insufficient-scope-union-step-up` | a `403 insufficient_scope` naming a scope the token lacks is answered by a renewal asking for the union of the held and the challenged scopes, and the call goes through with the new token |
| `mcp-auth-core/rotation-survives-a-restart` | a refresh token the authorization server rotates is kept: the daemon after a restart renews from the rotated token, never the spent one |
| `mcp-auth-core/google-shape-cross-origin-token-endpoint` | metadata of an operator-pinned issuer that puts its token and device endpoints on another origin (Google's documented shape) is used as declared |
| `mcp-auth-core/entra-shape-no-resource-on-intelligence` | an intelligence connection's oauth2 login sends no `resource` unless told to (Microsoft Entra refuses the parameter, AADSTS901002) |
| `mcp-auth-core/logout-revokes-at-the-server` | --logout after a login revokes the refresh token at the revocation_endpoint the metadata names (RFC 7009: token and token_type_hint=refresh_token) |

### `mcp-tasks`

| Check | What it proves |
|---|---|
| `mcp-tasks/a-step-follows-its-task-to-the-result` | a workflow step whose tools/call becomes a task finishes with the task's result |
| `mcp-tasks/the-step-timeout-cancels-the-task` | a step's timeout ends its wait for a task and sends tasks/cancel |
| `mcp-tasks/off-refuses-a-server-that-requires-tasks` | with tasks: off, a server that requires the extension fails the call with an error naming it |

### `mcp-skills`

| Check | What it proves |
|---|---|
| `mcp-skills/a-listed-skill-loads-once-its-digest-matches` | a skill skills/list names loads once its SKILL.md matches its entry's digest and size |
| `mcp-skills/a-skill-whose-bytes-differ-is-declined` | a SKILL.md whose bytes do not match its entry is declined, after a skills/get refresh |
| `mcp-skills/a-prompt-is-not-a-skill` | a server's prompt is never listed, fetched or loaded as a skill |

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
| `companies/agent-sends-a-company-none-of-its-correlators` | the calls a workflow makes to two Companies carry none of agentd's instance, run, context, principal or label keys, and no trace or idempotency key seen at one is seen at the other; the handshake and tool list carry no trace context, no clientInfo.title and no AAuth signature |
| `companies/agent-acts-at-one-company-for-another-only-with-the-user` | a run that read one Company's answer sends another Company nothing when the User cannot be asked, with agent.approval and the ask_human fallback both auto |
| `companies/agent-waits-out-a-companys-retry-after` | against the retry-after variant, whose token endpoint and MCP server each answer 429 with Retry-After, nothing agentd sends reaches the Company before the wait is over, and it joins and calls once it is |
| `companies/agent-never-repeats-a-call-that-may-have-run` | a tools/call that got no answer in time, or a 502, after it was sent is not sent again — by the transport or by the step's retry: — so no run's call reaches the Company twice: each run's calls go out in one trace of its own there, and no trace id is seen twice |
| `companies/agent-keeps-tokens-out-of-its-log` | no Session Token or Bearer token the Company issued appears in agentd's log, at debug level with content logging on |
| `companies/agent-signs-in-only-as-the-user-does` | a workflow's company.sign_in starts a Device Sign-In that asks for its scopes with the client assertion, never opens the User's verification page itself, polls an interval apart for the Session it signs in — a new one of the account's own — and once the User has approved is signed in: the account call it makes next is answered |
| `companies/agent-keeps-account-tokens-secret` | the Account Token a sign-in gave agentd reaches the Company only in the refresh_token member of a token request — never in a URL, a header or another body — and appears in no line of agentd's log and no file of its state |
| `companies/agent-uses-the-newest-account-token` | against the rotation variant, agentd uses each Account Token the Company returns from then on, across a restart: no token request it makes presents a spent one |
| `companies/agent-signs-out-by-revoking` | company.sign_out revokes the Account Token at the revocation endpoint (token_type_hint refresh_token, the client assertion), and the Session goes on signed out: the next token is the assertion's |
| `companies/agent-retries-a-message-with-its-id` | a message whose answer was lost — a 502 after the Company took it — is sent again with the same id, and the Company answers it as the message it has; another message has an id of its own |
| `companies/agent-says-who-wrote-each-message` | every message agentd sends in a conversation says sender agent, and none sends role |
| `companies/agent-skips-what-it-does-not-recognize` | agentd talks to the Company Agent over the poppy protocol, never the type it does not speak listed before it, and the stream it appends the conversation's events to holds no event of the type the draft does not define and no text-delta piece |
| `companies/agent-reconnects-a-stream-after-the-last-event` | a stream of a conversation's events that the Company ends is opened again with Last-Event-ID the last event agentd handled, and no cursor |
| `companies/agent-reads-on-from-the-cursor-it-saved` | after a restart agentd's first read of an open conversation names the cursor it saved, the last event it handled |
| `companies/agent-says-the-user-is-not-available` | when the Company Agent asks to talk with the User and nobody can ask them, agentd says in the conversation that the User is not available, once, as the agent, and starts no Direct Conversation |
| `companies/agent-sends-a-session-token-with-every-request` | every conversation request, and every API call an http step or company.api makes, carries the DPoP Session Token with a proof over it that the fake validated |
| `companies/agent-reuses-an-idempotency-key-on-retry` | an API write whose operation declares Idempotency-Key, its answer lost, is sent again with the same version 4 UUID, and gets the first answer |
| `companies/agent-lists-the-operations-extension` | with client.extensions [operations], the client metadata document the fake fetched lists operations at version 1, and the fake proposes the exchange as an operation; without it, the exchange is made, and nothing agentd sends confirms anything |
| `companies/agent-confirms-only-what-the-user-approved` | a run's confirm of a proposed operation, with nobody to ask and agent.approval and the ask_human fallback auto, sends nothing to the operations endpoint; with the User asked at their gate — its text naming the Company and quoting the revision's summary — and answering approve, the confirm goes once, approved_by user, with the revision the gate showed |
| `companies/agent-asks-again-when-the-terms-change` | a confirm the Company answers terms_changed is not sent again under the old approval: the User is asked about the new revision, and only their approve of it sends a confirm, of that revision |
| `companies/agent-reads-an-operation-in-progress-after-retry-after` | a confirm answered in_progress with Retry-After is followed by a read of the operation no sooner than the wait, with the same operation id, until it is final |
| `companies/agent-retries-a-confirm-with-the-same-operation` | a confirm whose answer is lost after the Company took it is sent again with the same operation id and revision, and the tool that proposed the operation is not called again |
| `companies/agent-asks-the-user-before-a-direct-conversation` | when the Company Agent asks to talk with the User, agentd asks the User at their gate and starts no Direct Conversation until they answer; their answer is the first message, as they wrote it, sender human, with parent_conversation_id the conversation the Company asked in |
| `companies/agent-says-a-decline-in-the-conversation` | when the User declines to talk with the Company directly, agentd says so in the conversation the Company asked in, once, as the agent, and starts no Direct Conversation |

### `ema`

| Check | What it proves |
|---|---|
| `ema/exchange-form-refresh-token-private-key-jwt` | with subject refresh_token and private_key_jwt, the exchange sends exactly grant_type, requested_token_type (id-jag), subject_token, subject_token_type (refresh_token), audience and resource (the pins), scope, client_id and the client assertion; the redemption is the jwt-bearer grant; the MCP server is sent the token, the EMA extension in initialize, and the ID-JAG's sub as agent/acting_for |
| `ema/exchange-form-id-token-client-secret-basic` | with subject id_token and client_secret_basic, a refresh_token grant with no scope comes first, and the exchange presents its ID token (subject_token_type id_token) with the secret in an Authorization: Basic header and no client assertion |
| `ema/id-jag-mismatches-refused` | an ID-JAG whose typ, aud, client_id, iss, sub or resource is not the pinned one, or that grants a scope not asked for, is refused and never redeemed |
| `ema/refresh-token-in-exchange` | a refresh_token in the exchange answer is refused with subject id_token (unexpected_refresh_token), and kept as a rotation with subject refresh_token: the rotated token is the one presented after a restart |
| `ema/idp-issuer-equal-to-ras-refused` | a delegated server whose authorization server is the IdP itself is refused by --validate-config (ID-JAG §9.3) |
| `ema/split-idp-metadata` | enrolment reads the IdP's RFC 8414 and OpenID documents, which split jwks_uri and the identity-chaining member as Okta's do, and verifies the ID token with the key set only the OpenID document names |
| `ema/invalid-grant-live-probe` | invalid_grant from an exchange runs the liveness probe; alive, the grant stays, another server still mints, and only that (grant, server) is held back |
| `ema/invalid-grant-dead-probe` | invalid_grant from an exchange with a dead probe revokes the grant: tombstoned, purged, and every later call revoked — after a restart too, for as long as the register keeps the revocation |
| `ema/ras-invalid-grant-is-a-resource-fault` | invalid_grant from the authorization server on a fresh ID-JAG is resource_fault, held back, and never read as a reuse signal |
| `ema/rotating-idp-concurrent-mints` | with a rotating IdP that revokes a token family on reuse, two runs minting for two servers at once both succeed, and the grant survives |
| `ema/step-up-uses-the-fresh-id-token` | insufficient_user_authentication waits on a sign-in gate, which the IdP's device grant answers; the call is then made with the fresh ID token, and the ID-JAG carries its auth_time |
| `ema/re-presentation-and-fallback` | a 401 on a token not just minted re-presents the cached ID-JAG with no new exchange; an authorization server that refuses a re-presented one is minted for instead |
| `ema/freshness-kill-switch` | once the IdP stops answering, no delegated call succeeds after the liveness interval: idp_unavailable, never an old token |
| `ema/no-mint-at-boot` | enrolment mints once per templated server, to list its tools; a restart mints nothing |
| `ema/slow-idp-delays-nothing-else` | while a mint waits on a slow IdP, an unrelated run starts and finishes |
| `ema/no-token-reaches-a-worker-or-a-prompt` | a turn's delegated call answers its worker — and so the prompt — with every live token value scrubbed, and no frame to a child carries a token or the supervisor's reserved variables |
| `ema/loopback-caller-gets-no-delegation` | a delegated server on a loopback listener with no verifier is refused at load; with one, the operator a bearer names is not the enrolled person, and its run is no_delegation |
| `ema/timed-out-gate-never-judged` | a consent gate nobody answers denies at its deadline, with ask_human_fallback auto, and no judge is spawned |
| `ema/approved-write-end-to-end` | a delegated create_issue gates; --delegation-approve, bound to the gate's nonce and arguments, makes the call; --delegation-deny refuses one without making it |
| `ema/document-may-not-delegate` | a :::!mcp fragment declaring grant id_jag, or referencing a reserved supervisor-only secret by any spelling of it, is refused — in the agent's instruction and through a subagent template |
| `ema/another-principals-work-mints-nothing` | work for anyone but the enrolled person — the bare operator in a conversation of the person's — mints nothing under the person's grant and is no_delegation |
| `ema/egress-from-delegated-data-gates` | a workflow's own http POST of what a delegated read returned waits for the person: denied, nothing is sent; approved, it is sent as they saw it |
| `ema/rotation-kept-when-the-id-jag-is-refused` | with a rotating IdP, the refresh token an exchange answers with is kept even when its ID-JAG fails a check: another server still mints, and the grant survives |
| `ema/a-subagents-run-is-not-delegated` | a run a subagent starts through a workflow tool granted to it, as the person, makes no delegated call — nor does a run that one starts, or a run its signal starts: delegation_not_permitted, and nothing reaches the server |
| `ema/autonomous-work-is-not-delegated` | a scheduled firing's delegated step is no_delegation and mints nothing; identity.autonomous_as naming the person is refused at load |
| `ema/an-echoed-token-is-scrubbed-from-an-error` | a delegated server's error that quotes the token presented reaches the run, the log and delegation.denied with the token scrubbed |
| `ema/resource-metadata-confirms-the-pins` | a delegated server whose RFC 9728 metadata names another resource, or not the pinned authorization server, is refused (config) before anything is minted for it |
| `ema/a-narrowed-template-narrows-the-grant` | after a restart under a template that allows less, a grant enrolled under more is held to it: scope_beyond_ceiling, delegation_not_permitted for a server it dropped |
| `ema/enrolling-again-replaces-a-grant` | an expired grant, or one enrolled under a pin since changed, is replaced by signing in again: a new epoch, the new identity, the old refresh token revoked at the IdP |
| `ema/keys-held-by-an-external-signer` | under key_custody external the IdP's and the authorization server's client assertions are signed by the external signer — each verifies with the public key it serves — and the refresh token is sealed under a data key the signer wraps: no key or seal key is configured, and no file of the store holds the refresh token |
| `ema/signer-down-sends-nothing` | with the external signer down the daemon starts and says so, and a delegated call fails signer_unavailable before any request reaches the IdP or the authorization server |
| `ema/rotation-kept-when-the-signer-fails` | under key_custody external, with a rotating IdP and a signer that fails right after it signed an IdP call's assertion, the rotated refresh token is kept — its data key was wrapped before the IdP was asked — so the next call presents it and completes, and the grant survives |
| `ema/signer-of-another-uid-refused` | a signer socket whose peer is not the configured uid refuses the start (config), and the signer is sent nothing |
| `ema/restored-vault-refused-by-the-audit-chain` | with observability.audit.chain set, a store restored from before a revocation — the grant and the register both — is refused as revoked at load through the register the chain anchors, so the person's calls are revoked and nothing is sent to the IdP; the enrolment, mint, revocation and register records are in the chain |
| `ema/ema-declared-on-both-dialects` | the EMA extension is declared in initialize's capabilities at 2025-11-25, and in every request's _meta client capabilities at 2026-07-28 |
| `ema/taint-gates-the-delegated-write-and-the-egress` | a run that read nothing makes its delegated sensitive write unasked; after a delegated read it is tainted and holds the person's data, so the same write asks under the tainted rule, saying why, and the run's own http POST asks under the egress rule: denied, neither is made |
| `ema/an-untagged-servers-read-gates-the-delegated-write` | a server that declares no tags counts as untrusted_input: after a run reads it, the delegated sensitive write asks under the tainted rule, naming the server, and denied is never made; after a read of a server tagged trusted the same write goes ahead unasked |
| `ema/trusted-output-holds-data-and-taints-nothing` | with auth.trusted_output, a delegated read leaves the run holding the person's data and untainted: the sensitive write and the http POST after it go ahead unasked |
| `ema/consent-is-the-persons-alone` | a message on the task a consent gate holds is refused and decides nothing, and an operator who is not the person is refused the answer though it holds the gate's nonce and hash; both are recorded, the gate denies at its deadline, and the call is never made |
| `ema/the-signer-signs-only-an-assertion` | the reference signer denies a payload that is no client assertion, and the authorization server's assertion when its key signs for another audience: the call fails signer_unavailable and nothing reaches the authorization server |
| `ema/forgotten-content-unreadable-chain-verifies` | what a consent gate showed is in the audit chain only sealed under the person's key and reads back whole; after --delegation-forget it is unreadable, and the chain still verifies at the next start |
| `ema/a-message-carries-what-its-sender-read` | a run that read the person's data sends a message step, and an agent step's model message.send, into a new conversation: that conversation's delegated sensitive write asks under the tainted rule, while the same write from a conversation the person opened goes ahead unasked; denied, the carried writes are never made |
| `ema/a-cached-step-keeps-no-delegated-data` | a run holding the person's delegated data keeps nothing in the step cache: the second run of a cached delegated read reads the server again, and each says its output was not kept |
| `ema/forgetting-closes-the-persons-gates` | under the external signer the person's key is opened off the loop when their gate opens, so an approval is sealed and reads back; --delegation-forget then denies the call waiting on their open gate at once, records it under the key it destroys, and every record of theirs is unreadable |
| `ema/a-login-is-the-enrolment` | a display client signs in at a2a.login through a start link and its cookie: its session is user:oidc:<iss>#<sub>, source oidc, carrying the IdP proof, and where a template admits the person the same sign-in kept their grant, so their run's delegated call is made with their own ID-JAG |
| `ema/two-people-two-grants` | two people signed in through a2a.login: each run is minted for with its own person's refresh token and subject, each lists only their own grant, and one is refused the other's gate — which the other approves |
| `ema/tools-are-offered-only-with-a-grant` | a delegated server's tools are offered only in work for a person who holds a usable grant there, through the session it acts through: a signed-in person whose grant was revoked is handed none of them, the person who holds hers is |
| `ema/a-forwarded-sign-in-fails-at-the-callback` | the IdP URL a start link redirects to, forwarded to another browser, returns without the start link's cookie: the callback refuses it (403), the login grant answers access_denied, nobody is signed in; and a start link opens once |
| `ema/no-template-no-grant` | a person no template admits signs in and is kept no grant — the refresh token revoked at the IdP — and their run is not_enrolled; a device session no template admits is refused its enrolment before the IdP is asked for a code |
| `ema/an-unpinned-device-needs-first-binding` | a device name with no pin, device_first_binding off, is refused its enrolment (principal_not_delegable) by device code and by start link alike, and the IdP is asked nothing |
| `ema/a-pinned-device-binds-its-pin-alone` | a pinned device name's enrolment by a device code the IdP approved for another subject is binding_mismatch and keeps nothing; signed in as the pin it is bound (bound_by pin) and its session's run is made |
| `ema/one-device-name-one-binding` | two device sessions approved under one name, first binding on: the one that signed in binds the name and its run is made; the other is not_enrolled, and its sign-in as another subject is binding_mismatch with the grant untouched |
| `ema/a-gate-is-answered-through-a-fresh-sign-in` | a person's gate is answered only through a session that signed in at the IdP as them, recently: a device session under the name that never signed in is refused, the person's own once its sign-in is older than consent_session_max_age, and the answer is taken once they sign in again; an operator the template lists among its approvers answers too |
| `ema/an-ended-session-ends-its-delegation` | auth.sessions.revoke closes the gate the session's run waits on at once, revoked and never made; and the gate of a session-bound run a restart finds closes revoked too |
| `ema/a-gate-survives-a-restart` | a consent gate the per-person operator's run waits on is asked again after a restart, with the same id, and their approval then makes the call |
| `ema/the-login-listener-rules` | a2a.login beside a2a.tls.client_ca and a workflow whose act_for names a device principal are refused at load; a login-only loopback listener declares bearer, refuses a caller who presents nothing, and serves its login endpoint and /oauth2/revoke with no device grant |
| `ema/a-reload-narrows-then-revokes` | a reload narrowing the person's template refuses their write at the next call (scope_beyond_ceiling) while their read is made; a reload removing it revokes the grant at once — at the IdP, tombstoned — and their next call is revoked |
| `ema/a-subagent-inherits-what-it-declares` | with subagents: inherit, a run a subagent starts is made with the person's grant at the server the subagent declared, refused (delegation_not_permitted) at one it did not, and no frame to a child carries a token |
| `ema/a-persons-own-message-is-theirs` | a signed-in person's own start is no outside text to work acting with their grant — their delegated sensitive write goes ahead unasked — while a delegated read's result still taints it, and what they started reaches another person's run as outside text, naming their message |
| `ema/the-persons-rules-ask-as-shipped-and-configured` | a delegated egress call asks at its first use per run and tool: the approval passes the run's second call unasked and the next run asks again; an operator's to: principal rule on the mode fact asks the person as security.policies[i], and a rule on a fact the call does not have holds nothing |
| `ema/gates-of-one-shape-are-answered-together-and-paced` | three parallel writes' gates share one shape and are approved together, each recorded; a batch naming a gate not open decides nothing; past identity.delegation.consent_per_minute a call is refused rather than asked, and the refusal recorded |
| `ema/agent-mode-servers-are-hidden-from-delegated-work` | work acting with a person's grant is refused an agent-mode server tagged egress (mode_mixing), which is sent nothing; listed in the template's agent_mode_servers, the call is made and recorded as an agent-mode call from delegated work, and after a delegated read it asks the person (egress_from_tainted_work_holding_delegated_data) |
| `ema/a-documents-workflow-asks-unless-listed` | a workflow an instruction document defined asks before every delegated egress call (document_sourced_delegated_egress); listed in the template's document_workflows by name and digest, it asks at the first use per run and tool alone |
| `ema/a-documents-subagent-is-its-workflows-work` | under subagents: inherit, a subagent a document's workflow spawns asks before its delegated egress call under document_sourced_delegated_egress, naming the workflow, as the run's own step would; denied, the call is not made |
| `ema/pairwise-subjects-are-recorded-not-compared` | under subjects: pairwise an ID-JAG naming another sub is taken: agent/acting_for carries its sub, and agent/grant is a random handle per server — stable at one, another at the next, never the grant's id |
| `ema/a-persons-mints-are-paced` | with budget.per_minute 1 and a token per scope set, the read the catalogue mint covers is made and the write needing a second mint within the minute is budget_exhausted before the IdP is asked |
| `ema/an-elicitation-asks-the-grants-person` | a delegated server's elicitation is put to the grant's person alone, in agentd's words with the server's quoted: an operator's answer is refused and the gate stays open; the person's answer is shaped to the schema and handed to the server, and the call goes on |
| `ema/an-elicitation-is-answered-through-the-grant` | of two devices approved under one name, the one that never signed in at the IdP as the name's identity is refused an answer to the grant's elicitation (no_idp_sign_in), the gate left open and the server handed nothing; the bound one's answer is taken |
| `ema/a-person-revokes-their-own-grant` | a person revokes their own grant from their own session and their next call is revoked; another person may not revoke it for them; an operator revokes anyone's; a device under the same name that never proved the binding revokes nothing |
| `ema/a-certificate-principal-delegates` | a principal rule admitting one client certificate's key, with an idp_subject pin, delegates under a template admitting x509: over that certificate it enrols with the IdP's device grant and its run is made with its own grant, the key's |
| `ema/a-command-beside-a-delegation-runs-isolated-and-asks` | security.exec beside a delegation is refused without spawn.uid, naming it, and loads as spawn.uid inside Landlock with every supervisor-only path closed to that uid; a command from a run holding the person's data asks them (exec_holding_delegated_data), then runs as the worker uid, and a command copying the seal key or the store copies nothing (as root; skipped otherwise) |
| `ema/a-stale-instruction-makes-no-delegated-call` | once the instruction's registry stops answering past its freshness deadline (unavailable: freeze), a model turn in a run already in flight is refused its delegated call (policy_denied) and the server is sent nothing, as are the operator's workflow the turn starts and the run its signal wakes, and a conversation's turn writes no workflow; the operator's own run of their workflow still makes its call |
| `ema/a-worker-uses-only-what-it-was-offered` | a turn worker and an inheriting subagent each asking the supervisor for a delegated tool their step or spawn did not offer are refused (tool.refused, why: not_offered) and the server is sent nothing, while the tool they were offered is made |
| `ema/inbound-token-names-its-person` | an enterprise access token (RFC 9068, Okta's profile) names its person, user:oidc:<iss>#<uid>: the run they start acts for them and names the token's act chain, the operator lists the inbound session it is kept as — issuer, client, scopes and act chain, never the token — and an act chain past the issuer's bound is a 401 |
| `ema/inbound-card-and-metadata-name-the-issuer` | the card declares the issuer as an oauth2 scheme with its oauth2MetadataUrl and the authorization-code flow with PKCE, every security requirement names a declared scheme, the RFC 9728 metadata names the resource and the issuer, and a request with no credential is a 401 whose challenge names that metadata |
| `ema/inbound-token-of-another-issuer-refused` | a token signed by the IdP's key but naming another issuer is a 401 invalid_token naming the protected-resource metadata, and starts nothing |
| `ema/inbound-token-for-another-audience-refused` | a token whose aud is not a2a.resource is a 401 invalid_token, logged with why and never with the token |
| `ema/inbound-expired-token-refused` | an expired token is a 401 invalid_token |
| `ema/inbound-unsigned-token-refused` | a token with alg none, in any case, is a 401 invalid_token |
| `ema/inbound-hmac-token-refused` | a token HMAC-signed with the issuer's public key set as the secret is a 401 invalid_token: no HMAC algorithm is ever accepted |
| `ema/inbound-token-of-a-forbidden-typ-refused` | a token whose typ is not an access token's under the issuer's profile (JWT, an Okta-internal type) is a 401 invalid_token |
| `ema/inbound-id-token-refused` | the front end's ID token presented as an access token — typ JWT with a nonce, and with no typ at all — is a 401 invalid_token: its aud is a client |
| `ema/inbound-token-of-an-unlisted-client-refused` | a token issued to a client the issuer's clients do not list is a 401 invalid_token |
| `ema/inbound-client-token-names-no-user` | a client_credentials token, issued to the client alone, names no user: with require_user_token it is a 401 invalid_token |
| `ema/inbound-scope-reaches-the-skill` | a send no scope of the token maps to is a 403 whose challenge says insufficient_scope and names the scope that would; the same token reaches a conversation its scope maps to, and another token's scope the workflow |
| `ema/inbound-scope-answers-its-own-run` | a token whose scope reaches workflows alone answers its run's human step with a plain message on the run's task, while a message there that would become a conversation turn is a 403 insufficient_scope |
| `ema/inbound-callers-tasks-are-theirs-alone` | another person's token reads, cancels, continues and lists none of a caller's task (TaskNotFound), while its caller reads it |
| `ema/admission-waits-on-the-callers-sign-in` | a token's caller asking for a workflow acting with a grant they lack waits in TASK_STATE_AUTH_REQUIRED with a start link, no step running, a message on it refused and another person finding nothing; signed in at the IdP through the link, the run goes on with no follow-up message and acts with the grant kept; the sign-in authorizes no later message — the caller's expired token is a 401, another person's finds no task, the page hands out no credential — and the next run asks nothing |
| `ema/admission-cancelled-starts-nothing` | a task cancelled while it waits on its caller's admission sign-in ends the sign-in: finished in the browser tab already open, the callback says it has ended, no code is redeemed, and the cancelled workflow never runs |
| `ema/step-up-waits-in-auth-required-for-one-call` | a step-up the IdP asks for puts the run's task in TASK_STATE_AUTH_REQUIRED with a start link and refuses a message on it; signed in through the link, the call is made with the fresh ID token and the run goes on with no message; past the IdP's max_age, the next run's call asks again |
| `ema/peer-auth-required-waits-for-its-person` | a peer's TASK_STATE_AUTH_REQUIRED puts the run's task in it, quoting the peer; the wait survives a restart without sending the objective again and ends once the person authorizes the peer; the run is an inbound token's caller's, whose token agentd holds, and neither it nor any credential reaches the peer or any other mock |
| `ema/peer-auth-required-beside-a-question` | a human step of a parallel branch, asking on the run's task while the peer's sign-in holds it in TASK_STATE_AUTH_REQUIRED, takes its answer, and the run completes once the peer is authorized |
| `ema/token-exchange-of-the-callers-token` | subject inbound: the caller's token is exchanged on their behalf (RFC 8693, access_token for access_token) at the IdP that issued it and sent nowhere else, for the pinned audience and resource; the MCP server is sent the result, for the person; an exchange the IdP refuses with invalid_grant is subject_expired |
| `ema/token-exchange-pinned-audience` | a token the exchange issues for another audience than the pinned one is refused and never sent to the MCP server |
| `ema/token-exchange-of-the-grant` | subject grant: the person's grant is refreshed at the IdP for an access token for agentd, exchanged there on their behalf for the pinned audience; the MCP server is sent the result, for them |
| `ema/webhook-token-never-taken-twice` | a webhook start with auth.jwt runs for the person a short-lived token for its own audience names; the same token again (its jti seen) and a token for the listener are refused; after a restart, under another idempotency key, the token is answered duplicate and starts nothing |
| `ema/webhook-token-named-by-the-listeners-rules` | a webhook token's person is the one the listener's a2a.principals rules name: a rule naming the issuer that admits nobody the token names admits no delivery; a delivery keeps a session of its own, and the session the person's A2A calls keep is as it was |
| `ema/policies-reload-whole-or-not-at-all` | a security.policies list that does not load refuses the reload and the old rule still holds; one that loads replaces it from the next call, logged |
| `ema/first-use-approval-outlives-a-restart` | a person's first-use approval is kept with the run: its second use of the tool after a restart asks nothing |
| `ema/first-use-approval-ends-with-its-template` | a first-use approval is bound to the templates the work acts under: after a restart onto a changed template the run's second use asks again |
| `ema/ciba-firing-acts-on-its-persons-approval` | a scheduled act_for firing, via ciba, asks its person by CIBA — an ID token fresh from a refresh as the hint, openid alone, agentd's binding message, the expiry — waits through pending and slow_down, and on approval exchanges with the approval's ID token; the run completes, the mint is recorded unattended, and the refresh token the IdP issued with the approval is revoked at the run's end |
| `ema/ciba-firing-denied-or-expired-fails-closed` | a firing its person denies, or whose request expires unanswered, fails ciba_denied and is exchanged for nothing; past ciba.per_day requests a firing asks no more |
| `ema/ciba-hint-falls-back-and-binds-its-person` | an IdP that returns no ID token on refresh is sent the login hint the person enrolled with, kept sealed; an approval by another identity than the grant's fails the firing closed (binding_mismatch) |
| `ema/ciba-firing-asks-once-more-after-a-restart` | a restart loses a via ciba firing's request, or the approval it gave, and the firing asks its person exactly once more, saying again — the step that waited waits on the new request, a later call asks; approved, the run completes; denied, it fails ciba_denied |
| `ema/ciba-firing-fails-closed-on-a-second-restart` | a second restart while the request a restart made a via ciba firing send is out fails the firing closed (ciba_denied), and no third request is sent |
| `ema/ciba-firing-refused-asks-no-more-after-a-restart` | a via ciba firing whose request its person denied, or whose wait they cancelled, goes on past the refused call with its run's record saying refused; after a restart its next call fails subject_expired and no second request is sent |
| `ema/ciba-refresh-token-revoked-after-a-crash` | the refresh token an IdP issues with a CIBA approval is kept sealed in the store, never in the clear, while the run lasts; after a crash the next start revokes it at the IdP and forgets it, before any new request is sent |
| `ema/ciba-refresh-token-revoked-once-the-signer-is-back` | under key_custody external, an approval's refresh token a crash left in the store stays, unrevoked, while the signer is down at the next start, and no request is sent; once the signer is back a later pass revokes it at the IdP and forgets it with no CIBA request to make it |
| `ema/ciba-exchange-binds-to-the-approval` | a via ciba firing's same-domain exchange (grant: token_exchange) presents its approval's ID token as the subject, never the grant's access token; an authorization server that takes no ID token subject fails the call config, and under profile okta the call is refused before the person is asked |
| `ema/act-for-on-a-webhook-refused` | act_for beside a webhook start, or a signal start, is refused by --validate-config (act_for_trigger_not_allowed); beside a schedule it loads |
| `ema/standing-under-okta-needs-the-override` | unattended: standing at a profile okta IdP is refused by --validate-config without allow_against_idp_guidance (standing_against_idp_guidance), and loads with it, warned |
| `ema/changed-digest-suspends-unattended-use` | a via standing firing acts with its person's grant, recorded unattended; after a reload changes the workflow its firings start nothing and mint nothing (unattended_suspended) until the person signs in again |
| `ema/unattended-egress-denied-by-default` | a firing's delegated egress tool is refused unasked and never made unless the template lists it in unattended_tools; listed, it asks the person and is made once approved |
| `ema/on-revoked-ask-waits-for-re-enrolment` | with on_revoked: ask a workflow step whose grant was revoked waits, among the person's gates; once they enrol again the step is made again and the run completes |
| `ema/step-up-held-to-its-max-age` | a step-up sign-in older than the IdP's max_age, or whose ID token carries no auth_time, is refused (step_up); a fresh one is the retried call's subject |
| `ema/dpop-where-advertised` | where the MCP server's and its authorization server's metadata advertise ES256 DPoP, the redemption carries a proof (again with the nonce asked for) and every MCP request presents a DPoP token with a proof, the one refused for want of the MCP server's nonce sent again at once with it; otherwise neither leg has one; a server that takes bound tokens alone behind an authorization server without DPoP is refused before anything is redeemed |
| `ema/firing-ended-mid-call-mints-nothing` | a via ciba firing whose run ends while its approved call proves the grant alive mints nothing: the call keeps the firing it was admitted as, fails subject_expired, and never falls back to the person's refresh token |
| `ema/work-a-firing-starts-acts-only-as-consented` | a run a firing's work starts is a firing of its own: one of a workflow declaring no act_for for the person fails unattended_suspended and mints nothing; once it declares one and they sign in again, it acts for them |
| `ema/ciba-firing-waits-on-one-request-at-a-time` | a second call a via ciba firing makes while its request waits waits on that request, and both are made on one approval; once a firing's wait closes unanswered another firing of the workflow may ask, though the first firing's run goes on |

### `payments`

| Check | What it proves |
|---|---|
| `payments/pays-in-payment-signature-and-reads-payment-response` | [X-HTTP] "Clients send payment data using the `PAYMENT-SIGNATURE` HTTP header" and "Servers communicate payment settlement results using the `PAYMENT-RESPONSE` header": the server recovers the wallet from the signature, and the settlement is the step's |
| `payments/skips-a-payment-flow-it-does-not-recognize` | [X §5.1.2] "Clients MUST NOT construct a payment for a `paymentFlow` they do not recognize, and SHOULD skip such `accepts[]` entries when selecting": an `upfront` entry is passed over and the next one paid, echoed exactly as `accepted` |
| `payments/refused-unsent-outside-the-caps-and-allow-lists` | ADR 0003: "caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent": over a cap and to an unlisted payee, nothing reaches the server |
| `payments/settlement-pending-is-never-paid-again` | ADR 0003: "a payment whose outcome is unknown is never made a second time"; [X §9] `settlement_pending` "the transaction may still confirm on chain": the step's retry sends nothing |
| `payments/the-approver-alone-answers-above-the-threshold` | ADR 0003: "the User's approval above a threshold, through an addressed human gate no model or judge answers": an operator who is not the approver is refused, and the approver's approve pays once |
| `payments/every-payment-is-an-audit-record` | ADR 0003: "every payment an audit record, tied to the request that asked for it": authorized before it is sent, then settled, in the hash-chained audit log, naming the run, the step and the nonce |
| `payments/no-signature-in-a-log-or-a-childs-payload` | ADR 0003: "the signing key held by the supervisor alone, sealed at rest, never in a model's context, a child's payload or a log": the key file is sealed and 0600, and the signature and the signed header are in no log line and no frame to a child |
| `payments/svm-pays-the-exact-transfer-within-the-policy` | [EXACT-SVM] "a client MUST effect an on-chain payment of an exact amount of a specified asset to a specified recipient": a Solana entry within the policy is paid from the wallet's base58 key with the transfer the sponsor verifies — the mint, the amount, the payee — and its settlement is the step's |
| `payments/svm-refused-unsent-outside-the-policy` | ADR 0003: "caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent": a Solana entry past per_payment and one to a key no allow-list names are refused, and the sponsor sees no transaction |
| `payments/mcp-pays-in-meta-and-reads-the-payment-response` | [X-MCP] "Clients send payment data using the MCP `_meta` field with key `x402/payment`" and "Servers communicate payment settlement results using the `_meta["x402/payment-response"]` field": a paying mcp.tool step calls the tool once more with the payment in its `_meta`, the tool verifies it, and the settlement is the step's |
| `payments/mcp-refused-unsent-outside-the-policy` | ADR 0003: "caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent": a tool asking past per_payment, and one asking to be paid to an unlisted payee, are refused, and neither tool sees a payment |
| `payments/mcp-the-approver-alone-answers-above-the-threshold` | ADR 0003: "the User's approval above a threshold, through an addressed human gate no model or judge answers": a tool's payment above approve_above waits for the approver, an operator who is not the approver is refused, and the approver's approve pays the tool once |
| `payments/mcp-unknown-outcome-is-never-paid-again` | ADR 0003: "a payment whose outcome is unknown is never made a second time": a paid tool call that gets no answer within the step's timeout is unknown, and the step's retries call nothing |
| `payments/a2a-pays-on-the-peers-task` | [X-A2A] "Clients send payment data using the A2A message metadata with task correlation": a peer's task that asks to be paid is answered on that task, a2a-x402 activated, with `payment-submitted` and the payload the peer's facilitator verifies, and the peer's receipt is the step's settlement |
| `payments/a2a-payment-rejected-when-it-will-not-pay` | [A2A-X402 v0.2 §5.4] "The client rejects all PaymentRequirements options and responds to the server agent with a message setting the message metadata field x402.payment.status to payment-rejected": a peer asking past per_payment is answered on its task with `payment-rejected` and nothing else, and nothing is paid |

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
request carrying a Session Token whether a newer one had replaced it, the
claims of the assertions and proofs a token request carried, and what each MCP
request said of its sender beside its credential — its trace, its client's
title, an AAuth signature, its idempotency key — is what the
`companies/agent-*` checks count: they declare the fake as agentd's Company,
host the identity `agentd --print-company-client` writes, and hold the real
binary to the draft through the fake's record alone. The sign-in checks give
the binary a unix A2A listener to ask the User on; the fake's scripted User
signs in at the Company, as a person would, nobody answers agentd's gate, and
the fake records when each poll came and whether an Account Token it issued
was anywhere in a request but the one form member that may carry it. The
Operations and Direct Conversation checks give it a loopback listener with no
credential — its caller the operator, who is the User — answer the User's
gate there as the User would, and count the confirms, the operation reads and
the messages the fake recorded, with the revision, `approved_by` and `sender`
each carried. Every
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

The `mcp-tasks` family holds agentd to SEP-2663 as a client, against the
built-in mock at MCP 2026-07-28, which makes a task only for a call that
declares the extension and refuses a `tasks/*` request without `Mcp-Name`
or the extension. What agentd did is read from its own log and from the
mock's record of each request that reached it.

The `mcp-skills` family holds agentd to SEP-2640 as a client, against the
same mock, which declares the Skills extension, lists its skills a page at
a time, and serves one `SKILL.md` with other bytes than its entry lists.

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
