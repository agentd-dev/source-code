// SPDX-License-Identifier: AGPL-3.0-only
//! agentd — a minimal, MCP-native, reactive agent runtime.
//!
//! One binary that is CLI, daemon, and subagent re-exec. A **supervisor**
//! owns lifecycle, triggers, and the process tree but never reasons; the
//! **agentic loop** lives only inside subagent processes. Tools come only
//! from MCP servers; reactivity comes from MCP resource subscriptions;
//! agentd is itself an MCP server so agents compose with one protocol.
//!
//! The split is deliberate: because only subagent processes reason, a wedged
//! or runaway model can never take the supervisor down with it, and the
//! supervisor's kill/reap/budget guarantees hold no matter what a model does.
//!
//! Module map below. `agentloop` is named to avoid the `loop` keyword.

pub mod a2a; // A2A surface: principals/roles/authorization + durable tasks
#[cfg(feature = "aauth")]
pub mod aauth; // agent-side AAuth signing for AAuth-protected MCP endpoints
pub mod agentloop; // the in-child ReAct loop + terminal-status state machine
pub mod auth; // endpoint credential providers + durable token cache: OAuth, AWS SigV4, SPIFFE
pub mod cel; // CEL expression seam (feature `cel`; always compiled, fail-closed without it)
pub mod config; // precedence (built-in<file<env<flag) + validate-at-startup; config::{file,yaml,paths,watch}
pub mod context; // durable transcripts, plan, memory, compaction, skills
pub mod engine; // workflow engine: graph model, templates, durable runs + scheduler
pub mod exit; // the public exit-code table + terminal-status -> code map
pub mod governor; // token governor: windowed durable budgets + shedding tactics
pub mod identity; // instance identity from the Kubernetes downward API (env-only)
pub mod intel; // intelligence client + provider adapters
pub mod jsonschema; // dependency-free JSON Schema subset validator (tool contracts, workflow schemas)
// JSON-RPC 2.0 codec + framing lives in the reusable `mcp` crate; re-exported
// so `crate::json::*` resolves (MCP + the supervisor<->subagent channel).
pub use ::mcp::rpc as json;
pub mod mcp; // MCP client (to servers) + self-MCP server + registry/config
// Transport primitives live in the reusable `net` crate; re-exported so
// `crate::net::*` resolves across the runtime (MCP transport + intelligence).
pub use ::net;
pub mod obs; // logging, health, tracing, metrics
pub mod registry; // tool registry: internal > code > MCP, contracts, overrides, grants
pub mod runtime; // the runtime: event loop, turn workers, lifecycle
pub mod sec; // secrets, tool-scope, gated exec
pub mod sha; // dependency-free SHA-256 (content identity: workflow hashes, skill bodies, artifacts)
pub mod signals; // sigaction + self-pipe wakeup; SIGTERM/INT/CHLD/PIPE/HUP latches
pub mod state; // durable state model: entities, manifest, inbox, timers, restore
pub mod store; // remote state store adapters: a 4-op contract over MCP tools / HTTP / memory
pub mod subagent; // supervisor<->subagent control protocol
pub mod supervisor; // the reactor, process tree, spawn/reap/liveness/kill/restart
pub mod tools; // CODE-REGISTERED tools — the embedder seam
pub mod triggers; // execution modes + reactive routing + timers
pub mod wire; // MCP + intelligence wire types

/// Crate version, surfaced in logs (`agentd_build_info`) and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Announce a bound loopback listener's address through `addr_file` — the
/// discovery handshake for the built-in test mocks (`--internal-mock-llm`,
/// `--internal-mock-mcp-http`): the harness passes a fresh path, waits for the
/// file to exist, then reads `host:port` from it. Written atomically (tmp +
/// rename) so a waiter never observes a half-written address.
pub fn announce_addr(addr_file: &str, listener: &std::net::TcpListener) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let tmp = format!("{addr_file}.tmp");
    std::fs::write(&tmp, addr.to_string())?;
    std::fs::rename(&tmp, addr_file)
}
