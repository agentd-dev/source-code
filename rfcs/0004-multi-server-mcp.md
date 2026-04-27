# RFC 0004: Multi-server MCP routing

**Status:** Accepted, implemented.
**Author:** Andrii Tsok
**Depends on:** RFC 0001 §12.

## 1. Problem

The first MCP integration assumed exactly one stdio server per
process (`--mcp-stdio CMD`), with one global allowlist under
`[policy.mcp]`. Real workflows immediately wanted two or more
servers — a filesystem server and a GitHub server, say — with
*different* trust levels. One shared allowlist forces the union of
permissions onto every server: the GitHub server inherits filesystem
patterns and vice versa. That's the wrong default for a runtime whose
posture is least-privilege.

## 2. Decision

- The workflow declares servers as first-class entries:

  ```toml
  [[mcp_servers]]
  name = "docs"
  command = ["/usr/local/bin/mcp-fs", "--root", "/srv/docs"]
  allow_tools = ["read_page"]
  allow_resources = ["docs://pages/*"]

  [[mcp_servers]]
  name = "github"
  command = ["/usr/local/bin/mcp-github"]
  allow_tools = ["comment_on_pr"]
  ```

- **Allowlists are per-server.** A tool exposed by a server is not
  callable unless that server's own allowlist names it. There is no
  inheritance and no union.
- Nodes address servers by name: `call_mcp_tool { server = "github" }`.
  The `server` field is optional only when exactly one server exists;
  with several, the **validator** rejects ambiguous nodes before
  startup rather than letting the engine guess.
- Spawn failures are bind-time errors. A workflow that declares a
  server it cannot start does not start.

## 3. Back-compat

`--mcp-stdio` survives as sugar for an implicit `{ name = "default" }`
entry carrying the legacy `[policy.mcp]` allowlist, so single-server
workflows keep their exact semantics. Declaring a TOML entry named
`default` *and* passing the flag is a name collision and refuses to
start — silent precedence between two configuration surfaces is how
operators get surprised.

## 4. Alternatives considered

- **Tool-name prefixing** (`github/comment_on_pr` routed by prefix):
  rejected — it moves routing into string convention, invisible to
  the validator, and collides with servers that themselves use
  slashes.
- **One process per server with an external mux**: cleaner isolation
  but pushes composition onto every operator for the common
  two-server case; nothing prevents that deployment shape today for
  those who want it.

## 5. Consequences

Each server handle owns its child process and its allowlist behind
atomic-swap cells, which is what later allows per-server reload
(rotate one allowlist, respawn one child) without touching the
handler registry — see RFC 0005.
