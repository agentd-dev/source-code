// SPDX-License-Identifier: Apache-2.0
pub mod client;
// The Streamable HTTP client transport (RFC 0004) now lives in the reusable `mcp`
// crate as `mcp::http`; `client` uses it directly (`::mcp::http`).
// Auth material resolution for remote MCP endpoints (RFC 0012 §3.7): materialize
// secret-free `{{secret:…}}` header templates into wire headers at connect time.
pub mod auth;
// OAuth 2.1 client-credentials (M2M) token source for endpoints behind an OAuth
// gateway (RFC 0006 §auth). Feature-gated; dependency-free.
#[cfg(feature = "oauth")]
pub mod oauth;
// Built-in Streamable HTTP mock MCP server (the hidden `--internal-mock-mcp-http`
// mode, v2.0.0) for the test + conformance suites: serves a one-resource reactive
// MCP over a unix socket, so the harness drives agentd's HTTP transport end to end.
// In debug it's always present (so `cargo test` works with no flag); in release it
// ships only under `internal-mocks`, so the production binary carries no test
// scaffolding.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock_http;

// agentd serving its own MCP over a unix socket (composability, RFC 0005) and
// over vsock (the agentctl management transport, RFC 0015 §3). Feature-gated,
// no deps (blocking listener, thread-per-connection).
#[cfg(feature = "serve-mcp")]
pub mod server;

// The A2A (Agent2Agent) v1.0 unary method surface, served over the same self-MCP
// listener (RFC 0020). A thin binding onto the served-run machinery — a Task IS a
// served run. Feature-gated (`a2a = ["serve-mcp"]`), no deps (reuses the RFC 0004
// JSON-RPC codec + the vsock/unix management transport).
#[cfg(feature = "a2a")]
pub mod a2a;

// agentd-as-A2A-client: the remote-A2A-agent delegation backend (RFC 0020 §3).
// Connects to a declared peer over the unix/vsock transport + the RFC 0004
// JSON-RPC codec, runs `a2a.SendMessage` then polls `a2a.GetTask` to a terminal
// state, and returns the distillate. Reuses the wire types from `a2a`; no deps.
#[cfg(feature = "a2a")]
pub mod a2a_client;
