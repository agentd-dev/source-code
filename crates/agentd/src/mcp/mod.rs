pub mod client;
// Built-in mock MCP server (the hidden `--internal-mock-mcp` mode) for the test
// + conformance suites. In debug it's always present (so `cargo test` works with
// no flag); in release it ships only under `internal-mocks`, so the production
// binary carries no test scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;

// agentd serving its own MCP over a unix socket (composability, RFC 0005).
// Feature-gated, no deps (blocking UnixListener).
#[cfg(feature = "serve-mcp")]
pub mod server;
