# ADR 0001: MCP extensions — agentd implements the client side only

**Status:** Accepted, 2026-10-18 (the user: "we are not implementing the mcp
server capability… but the mcp client capabilities of the new mcp extensions…
so agentd can create tasks, use events, etc")
**Context:** RFC 0048

## Context

MCP's extensions — Tasks, Skills, Events, enterprise-managed authorization —
each define a client side and a server side. agentd reaches every tool and
resource through MCP servers that others run; nothing reaches into agentd over
MCP.

## Decision

For every MCP extension, agentd implements what an MCP **client** does: it
declares the extension, sends its requests, tracks its state, handles its
errors and applies the client's security rules. It does not serve any
extension, and it is not an MCP server.

## Consequences

- MCP Tasks: agentd creates and follows tasks on servers; it does not run
  tasks for anyone.
- Skills: agentd loads skills a server serves; it does not serve its own.
- Enterprise-managed authorization and CIMD: agentd obtains and presents
  tokens, and publishes its client metadata; it validates no one else's.
- A track that would need a server role (MCP Apps hosting, serving skills)
  comes back to the user as a question, not a design.
