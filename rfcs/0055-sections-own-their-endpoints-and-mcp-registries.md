# RFC 0055: Each section owns its endpoints; MCP registries

**Status:** Accepted 2026-11-01 by the user (decisions in §8)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-11-01
**Supersedes:** RFC 0037's `services:` section (the egress policy stays, re-pointed)
**Builds on:** RFC 0031 (endpoint auth), RFC 0037 (egress), RFC 0047 (Companies), RFC 0052 (Client ID Metadata Document), instruction/1's `:::mcp` object

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

Two changes, asked for by the user on 2026-11-01:

1. **`services:` goes away.** Every endpoint is declared in the section that
   uses it: MCP under `mcp:`, models under `intelligence:`, peers under
   `a2a:`, Companies under `companies:`, plain HTTP under a new `http:`.
   What a catalogue entry carried (auth, headers, tags, a tool ceiling,
   rate, timeout, breaker) moves onto the entry that is dialed. `security.egress:
   closed` keeps its meaning: agentd dials only what the operator declared,
   each kind checked against its own section. No compatibility path:
   `services:` is refused by the schema like any unknown key.

2. **MCP registries.** The MCP project's registry
   (`https://registry.modelcontextprotocol.io`, API `v0.1`) and any registry
   that serves the same API become a source of MCP servers.
   `mcp.registries` names the registries the operator trusts and the servers
   it approves from each. A server from a registry can be used from a
   workflow step, from the operator's `mcp.servers`, or from an instruction's
   `:::mcp{registry=… server=…}`. It is dialed when first used, and signs in
   then, when it needs to.

## 2. Motivation

- `services:` splits one endpoint across two places. An MCP server is a
  `mcp.servers` entry, but its trust floor, tool ceiling and shared
  credential sit in a `services` entry it may or may not reference, matched
  by URL prefix. The demo kits show the cost: approving DeepWiki for a
  document meant a `services` entry of `kind: mcp` that nothing references,
  and the user read it as MCP configured in the wrong place.
- The MCP ecosystem now publishes servers through registries: namespaced,
  versioned `server.json` records with their remote URLs. agentd can only
  dial a URL that someone copied into its configuration.
- instruction/1 already lets a document name a server by registry
  (`:::mcp{registry=… server=…}`). agentd holds those as text today.

## 3. Configuration after this RFC

### 3.1 `mcp:`

```yaml
mcp:
  servers:
    - name: billing                       # a connection, as today
      endpoint: https://billing.internal/mcp
      auth: {kind: static, token: "{{secret:BILLING_MCP_TOKEN}}"}
      tags: {"*": [sensitive]}
      allow: [charge_lookup, "invoice_*"]
      rate: "60/1m"
      breaker: {failures: 3, cooldown: 30s}
    - name: deepwiki                      # approved, dialed when first used
      endpoint: https://mcp.deepwiki.com/mcp
      connect: on_use
    - name: github                        # from a registry, by name
      registry: official
      server: io.github.github/github-mcp-server
      version: latest                     # or "2.0.3"
      connect: on_use
  registries:
    - name: official
      url: https://registry.modelcontextprotocol.io
      servers: ["io.github.github/*", "com.deepwiki/*"]   # what the operator approves
      tags: {"*": [untrusted_input]}      # the default for anything from a registry
    - name: acme
      url: https://mcp-registry.acme.internal
      auth: {kind: oauth2, grant: client_credentials, …}
      servers: ["*"]
```

- `mcp.servers[]` gains `rate`, `timeout`, `breaker` (from the catalogue),
  `connect: boot | on_use` (default `boot`), and `registry` / `server` /
  `version` as an alternative to `endpoint`.
- An operator server is the approval for a document's `:::mcp` whose `src`
  matches its endpoint (scheme, authority, path prefix on segment
  boundaries, as the catalogue matched). The document's block then **is**
  that server: one connection, the operator's auth, tags and ceiling; the
  document's `tools=` narrows what the document's guidance offers, never
  widens. A document `:::mcp` that matches nothing is held as text with a
  host-policy warning under `egress: closed`, and is its own untrusted
  server under `egress: open` (today's rule).
- `mcp.registries[]`: `name`, `url` (the API base), `auth`, `headers`,
  `servers` (globs over registry names; **the approval**: a name outside them
  is never resolved), `tags` (default `{"*": [untrusted_input]}`), `allow` /
  `exclude` (a tool ceiling for every server from it), `rate`, `timeout`,
  `refresh` (how long a resolution is reused; default `1h`).

### 3.2 The other kinds

| Was | Becomes |
|---|---|
| `services.x {kind: intelligence}` | `intelligence.providers.x` (endpoint, auth, headers, timeout, rate). The top-level `intelligence.endpoints` stays for the one-provider case. |
| `services.x {kind: peer}` | `a2a.peers[]` (gains rate, timeout, tags, headers). A2A push targets must match a peer's endpoint or `a2a.push.allow`. |
| `services.x {kind: http}` | `http.endpoints.x` (endpoint, auth, headers, methods, rate, timeout, tags). `http` steps name it (`endpoint: x`) or match it by URL. A workflow, instruction or skill fetched by URL must match one under `egress: closed`. |
| `services.x {kind: company}` | `companies.x` (every company field, unchanged). |
| `mcp.servers[].service`, `a2a.peers[].service`, `intelligence…service` | removed: the entry carries its own settings. |

`security.egress: closed` then reads: every MCP dial matches an
`mcp.servers` endpoint or is resolved through an approved registry; every
model endpoint is an `intelligence` endpoint or provider; every peer and push
target an `a2a` entry; every other HTTP request an `http.endpoints` entry;
every Company a `companies` entry. `observability.otel.endpoint` stays the one
exception.

## 4. MCP registries

### 4.1 Resolution

A name resolves through `GET {url}/v0.1/servers/{name}/versions/{version}`
(`latest` by default). The record's `remotes` give the endpoints:

- `streamable-http` is dialed. `sse` (the 2024-11-05 transport) is dialed
  only if agentd's client speaks it for that server's protocol era;
  otherwise skipped, said.
- A server with only `packages` (stdio, OCI, npm, PyPI …) is refused:
  agentd speaks MCP over HTTP and starts no local processes. The refusal
  names the server and says so.
- A remote's `headers` with `{variable}` values are filled from the
  operator's `headers` on the `mcp.servers` entry (`{{secret:…}}` as
  anywhere). A required variable left unfilled refuses the server, naming
  it. A registry never supplies a credential.
- A record whose status is `deleted` refuses; `deprecated` connects and
  warns (`mcp.registry.deprecated`).
- Each resolution is logged (`mcp.registry.resolved {registry, server,
  version, url}`), stored durably, and reused for `refresh`. A run pins the
  version it first resolved, like it pins a workflow definition, so a
  publish under a running run does not change the server it talks to.

### 4.2 Where a registry server can be used

1. **Operator config:** `mcp.servers[] {registry, server, version}` (3.1).
2. **A workflow step:** `{kind: mcp.tool, registry: official, server:
   io.github.github/github-mcp-server, tool: get_issue, args: …}`. The step
   names the server where it is used; agentd dials it on first use under
   the registry's approval and tags. The same fields work on `mcp.resource`
   and on a `wait {on: resource}`.
3. **An instruction:** `:::mcp{id=gh registry="https://registry.modelcontextprotocol.io"
   server=io.github.github/github-mcp-server tools="get_issue"}`. The
   `registry` URL must equal an `mcp.registries[].url`, and the name must
   pass its `servers`; otherwise the block is held as text with a
   host-policy warning. A `src` beside them must be one of the record's
   remotes, or the block is refused (the document and the registry disagree).
   `account` selects a credential the operator bound
   (`mcp.servers[].accounts`, a later phase); `resolver` is held.

The model chooses a registry server only where the operator opted in
(`discover: true`, §8), and then only among names `servers` approves.

### 4.3 Connecting on use, signing in on use

`connect: on_use` and every registry server dial on the first call that
needs them (a step, the turn after an instruction declares them), not at
boot. A server already connected is reused by every step and turn.

If the server needs authorization and agentd holds no usable token, agentd
signs in then, through what RFC 0031 and RFC 0052 already built:

- `auth` on the entry says how (client credentials, device grant, browser
  PKCE). With no `auth`, a 401 starts discovery: the protected-resource
  metadata (RFC 9728), the authorization server, agentd's Client ID
  Metadata Document as its client, and the device grant.
- An interactive grant raises a **sign-in gate** on the A2A interface, the
  same surface as a human approval: the TUI and web UI show the
  verification URL and code. The step waits durably (`wait`, survives a
  restart) until the token arrives, then continues; a gate unanswered by the
  step's `timeout` fails the step with `sign_in_timeout`. A turn that needs
  the server is told the server is signing in, and its tools appear once it
  has.
- Tokens are cached and refreshed as today (file or durable store).
- Events: `mcp.sign_in.required {server, url, method}`, `mcp.sign_in.done`,
  `mcp.sign_in.failed`.

### 4.4 Security

- A registry is a directory, not an authority. Its records are untrusted
  input about where to connect; the operator's `servers` globs are the
  approval, and its `tags` the trust. `trusted` on a registry entry is
  refused; the operator vouches per server with an `mcp.servers` entry.
- Names are matched exactly against globs (`*` within a segment, `**` across
  none: a name is `namespace/server`). A record whose `name` differs from the
  name asked for is refused.
- A remote URL must be `https://` to a public address (the SSRF guard),
  unless the registry entry sets `allow_private: true` (an internal
  registry).
- A document never adds a registry, never widens `servers`, never sets
  `connect`. `mcp.registries` is operator-only, reloadable.
- The lethal-trifecta accounting counts a registry's servers at their
  registry tags whether or not they are connected yet, so approving a
  registry is judged at load, not at first use.

## 5. Workflows

Per the user's rule (every capability fits workflows where it means
something):

- `mcp.tool`, `mcp.resource` and `wait {on: resource}` take `registry` +
  `server` (+ `version`) in place of a configured `server` name (4.2).
  Nothing else is needed to call a registry server from a run.
- Signing in is a durable wait inside the step (4.3), visible as a gate. No
  new node: a workflow that wants to sign in ahead of time calls a cheap
  tool on the server in an early step.
- No `registry.search` node: a workflow names its servers. Discovery is the
  operator's, at configuration time (`agentd mcp registry search <term>` on
  the CLI, Phase 3), or the model's where a registry sets `discover: true`
  (§8).

## 6. Phases

1. **P1 — sections own their endpoints.** Remove `services:`; move each
   kind; egress over the sections; `connect: on_use`; document servers
   matched against operator servers. All examples, demo kits, docs, tests
   and the conformance suite move. Breaking.
2. **P2 — sign in on use.** The sign-in gate, the durable wait, discovery
   for servers with no `auth`, the events.
3. **P3 — registries.** `mcp.registries`, resolution and its cache, the three
   uses (config, workflow step, instruction), version pinning per run, the
   CLI search, `discover`, the conformance checks against a mock registry and one
   live check against the official registry.

## 7. Alternatives considered

- **Keep `services:` and add `mcp.registries`.** Leaves the split the user
  objected to, and two places to look for one MCP server.
- **A registry server is connected at boot.** Hundreds of approved names
  would mean hundreds of dials and sign-ins nobody needs; on-use matches how
  the user described it.
- **Let the model search a whole registry and connect.** A model choosing
  its own servers from a public directory is the lethal trifecta by
  construction; `discover` keeps it inside the operator's approved names.

## 8. Decisions (the user, 2026-11-01)

1. **Registry approval default:** a missing or empty `servers` list approves
   nothing. Only names matching the operator's globs resolve.
2. **Model-driven discovery: operator opt-in.** `discover: true` on an
   `mcp.registries` entry gives the model `mcp.registry.search` (name,
   title, description, version and remote kinds of matching records) and
   `mcp.registry.connect {registry, server}`, which connects only a name the
   entry's `servers` globs already approve, at the registry's tags, through
   the same on-use dial and sign-in gate. Off by default. The trifecta
   accounting already counts every approved name (§4.4), so `discover` adds
   no reach the approval did not.
3. **Release:** one release carries instruction/1 and all three phases.
   Nothing ships on the `services:` shape.
