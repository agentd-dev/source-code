// SPDX-License-Identifier: Apache-2.0
//! agentd — a minimal, MCP-native, reactive agent runtime.
//!
//! One binary that is CLI, daemon, and subagent re-exec. A **supervisor**
//! owns lifecycle, triggers, and the process tree but never reasons; the
//! **agentic loop** lives only inside subagent processes. Tools come only
//! from MCP servers; reactivity comes from MCP resource subscriptions;
//! agentd is itself an MCP server so agents compose with one protocol.
//!
//! Architecture: `rfcs/0001-mcp-native-agent-runtime.md` (front door) and
//! `rfcs/0002`–`0013`. Binding decisions: `docs/design/00-architecture-assessment.md`.
//! Build order: `docs/design/PLAN.md`.
//!
//! Module map (assessment §4.0). `agentloop` is named to avoid the `loop`
//! keyword.

pub mod agentd_uri; // the agentd:// resource scheme (self-state + async completion)
pub mod agentloop; // the ReAct loop + terminal-status state machine
pub mod capabilities; // the capabilities manifest — the agentctl control-plane spine (RFC 0015)
#[cfg(feature = "cluster")]
pub mod cluster; // horizontal scaling: sharding + autoscaling signals + capacity (RFC 0019)
pub mod config; // precedence (built-in<file<env<flag) + validate-at-startup
pub mod config_file; // the declarative config FILE (JSON) + JSON Schema export (RFC 0017 §3/§4)
#[cfg(all(unix, feature = "config-watch"))]
pub mod config_watch; // inotify file-watch reload trigger (RFC 0017 §5.2)
pub mod exit; // the public exit-code table + terminal-status -> code map
pub mod identity; // instance identity from the k8s downward API (env-only, RFC 0015 §6)
pub mod intel; // intelligence client + provider adapters
// JSON-RPC 2.0 codec + framing now lives in the reusable `mcp` crate; re-export
// so `crate::json::*` keeps resolving (MCP + the supervisor↔subagent channel).
pub use ::mcp::rpc as json;
pub mod mcp; // MCP client (to servers) + self-MCP server + registry/config
pub mod net; // hand-rolled HTTP/1.1 (non-streaming), unix-socket, (tls/vsock gated)
pub mod obs; // logging, health, tracing, metrics
pub mod report; // run-outcome reports — the kubectl-agents-results backend (RFC 0016 §6)
pub mod sec; // secrets, tool-scope, gated exec
pub mod signals;
pub mod subagent; // supervisor<->subagent control protocol
pub mod supervisor; // the reactor, process tree, spawn/reap/liveness/kill/restart
pub mod triggers; // execution modes + reactive routing + timers
pub mod wire; // MCP + intelligence wire types // sigaction + self-pipe; SIGTERM/INT/CHLD/PIPE

/// Crate version, surfaced in logs (`agentd_build_info`) and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
