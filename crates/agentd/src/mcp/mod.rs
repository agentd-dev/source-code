pub mod client;
pub mod mock;

// agentd serving its own MCP over a unix socket (composability, RFC 0005).
// Feature-gated, no deps (blocking UnixListener).
#[cfg(feature = "serve-mcp")]
pub mod server;
