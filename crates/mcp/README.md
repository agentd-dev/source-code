# agentd-mcp

The Model Context Protocol layer [agentd](https://agentd.dev) talks to servers
through. It wraps the official
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK in a **blocking**
client, and runs it over a transport you control.

That second half is the reason this crate exists. `rmcp` owns the protocol —
the handshake, the typed requests and notifications, capability negotiation,
the version table — and tracking the specification upstream is exactly what you
want from a protocol implementation. What its own transport cannot do is carry
a credential your deployment requires: an AAuth request signature with its
challenge/re-sign loop, an AWS SigV4 signature computed per request, an mTLS
client identity, an OAuth token you refresh, an SSRF guard on every dial.

So `rmcp_transport` implements the SDK's `StreamableHttpClient` over
[`agentd-net`](https://crates.io/crates/agentd-net)'s HTTP stack. The SDK speaks
the protocol; you keep the socket.

```rust,no_run
use agentd_mcp::client::McpClient;
use std::time::Duration;

let mut mcp = McpClient::connect(
    "github",
    "https://mcp-github.internal/mcp",
    vec![],
    Duration::from_secs(30),
)?;
mcp.initialize()?;

for tool in mcp.list_tools()? {
    println!("{}", tool.name);
}
let out = mcp.call_tool("list_issues", None)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What's in it

- **A blocking client** over the async SDK, so a synchronous runtime does not
  need an executor of its own.
- **Server→client requests answered** — `ping` unconditionally (the spec says
  both sides MUST), `elicitation/create` delegated to a host handler that can
  ask a human, and an undeclared capability refused with `-32601` rather than
  met with silence.
- **Notifications reach you** — `resources/updated` and the list-changed family
  land in a queue you drain, which is what makes a subscribe-and-react daemon
  possible.
- **The version/era model** — the legacy and stateless revisions, and which
  subscription mechanism each defines.
- **A JSON-RPC codec and an HTTP server framework**, used by agentd's own
  listeners.

## Scope

This is agentd's MCP layer, published because the blocking-client-over-your-own-
socket shape is not something you can get from the SDK alone. The API is shaped
by what agentd needs and moves with it.

Licensed under Apache-2.0. Source and issues:
<https://github.com/agentd-dev/source-code>.
