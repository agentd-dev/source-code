//! MCP integration (RFC §12).
//!
//! MCP reaches the runtime through a small [`McpClient`] trait.
//! Phase 5 ships one real transport — a persistent child process
//! speaking NDJSON JSON-RPC 2.0 over stdio — plus a mock for tests.
//!
//! Two node handlers register against the engine:
//!
//! - `call_mcp_tool` → [`handler::CallMcpToolHandler`]
//! - `read_mcp_resource` → [`handler::ReadMcpResourceHandler`]
//!
//! Both consult an [`allowlist::McpAllowlist`] before dispatch so
//! "an MCP server exposing a tool" never automatically implies
//! "the workflow is allowed to call it" (RFC §12.2).

pub mod allowlist;
pub mod client;
pub mod config;
pub mod handler;
pub mod protocol;
pub mod registry;

pub use allowlist::McpAllowlist;
pub use client::{McpClient, McpClientRef, MockMcpClient, StdioMcpClient};
pub use config::McpServerDef;
pub use handler::{CallMcpToolHandler, ReadMcpResourceHandler};
pub use registry::{McpRegistry, McpRegistryRef, McpServerHandle};
