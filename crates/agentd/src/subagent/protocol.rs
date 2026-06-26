//! The supervisor↔subagent control protocol. RFC 0005 §control-protocol,
//! RFC 0009 §spawn-payload.
//!
//! A minimal JSON-RPC *sibling* — not literal MCP (no `initialize`
//! handshake) — carried length-framed (4-byte prefix, [`crate::json::frame`])
//! over the child's stdio pipes, so payloads that contain newlines
//! (instructions, context seeds, distilled results) survive. Two directions:
//! [`ControlMsg`] flows down (supervisor→child), [`AgentMsg`] flows up.
//!
//! The control reader inside the child runs on a thread **separate from the
//! agentic loop** (RFC 0003), so `Ping`/`Pong` liveness survives a long
//! in-flight tool/model call. This module is just the wire types; the spawn
//! mechanics are `supervisor/spawn.rs`, the child side `subagent/control.rs`.

use crate::agentloop::stop::Outcome;
use crate::config::McpServerSpec;
use crate::wire::intel::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The environment variable the supervisor sets on the child so its `main`
/// takes the subagent path instead of re-parsing CLI config.
pub const SUBAGENT_ENV: &str = "AGENTD_SUBAGENT";

// ---- downward: supervisor -> subagent ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    /// The first frame: everything the child needs to run. Sent exactly once.
    Spawn(Box<SpawnPayload>),
    /// Liveness probe; the child's control thread answers [`AgentMsg::Pong`].
    Ping { seq: u64 },
    /// Ask the child to wind down at the next turn boundary (graceful).
    Cancel { reason: String },
    /// Inject a message into the child's running session (parent `send` /
    /// reactive continue, M3).
    Inject { message: String },
}

// ---- upward: subagent -> supervisor ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// Setup done (intel + scoped MCP connected); the child is about to loop.
    /// The supervisor's crash-on-spawn fast-fail waits for this (RFC 0003).
    Ready,
    /// Answer to a [`ControlMsg::Ping`].
    Pong { seq: u64 },
    /// A progress event (loop.step, tool.call, …) — also resets the
    /// no-progress watchdog (Detector B, RFC 0003). `fields` is opaque to the
    /// supervisor except for correlation.
    Event { event: String, fields: Value },
    /// Incremental token/step usage for hierarchical accounting (RFC 0003).
    Usage(Usage),
    /// Terminal: the distilled result + final status. Sent exactly once.
    Result { outcome: Outcome },
    /// Terminal: a fatal infrastructure failure (intel/mcp unreachable).
    Failed { error: String },
}

// ---- spawn payload ----

/// Everything a subagent needs to run, minted by the supervisor. The child
/// trusts none of this from its own request — `depth` in particular is
/// **minted by the supervisor** from the caller's handle (RFC 0009).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnPayload {
    /// The task. For a delegated child this is the parent's `instruction`
    /// argument; see also `output_contract`.
    pub instruction: String,
    /// Objective + required output format + boundaries — a real delegation
    /// contract, not a bare string (RFC 0009 §spawn-payload).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contract: Option<String>,
    /// The narrowed context the parent chose to share — never the full
    /// transcript (context hygiene + injection firewall, RFC 0012).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_seed: Vec<SeedMessage>,
    /// How to reach the LLM (env/flag-sourced; never logged).
    pub intelligence: IntelConfig,
    /// The child's **scoped** MCP server subset (⊆ parent's; RFC 0009).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSpec>,
    pub limits: Limits,
    pub telemetry: Telemetry,
    /// Supervisor-minted tree depth (0 = root).
    pub depth: u32,
    /// Whether the gated `exec` self-tool is available (from `--enable-exec`;
    /// inherited by children). Off by default (RFC 0012). `#[serde(default)]`
    /// keeps older frames parseable.
    #[serde(default)]
    pub enable_exec: bool,
}

/// A single seed message — a minimal {role, content} pair. Roles mirror the
/// loop's: `system` | `user` | `assistant` | `tool`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelConfig {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub max_steps: u32,
    pub max_tokens: u64,
    /// Wall-clock deadline in milliseconds from the child's start. The child
    /// arms its own deadline; the supervisor also tracks an absolute one
    /// (Detector A, RFC 0003).
    pub deadline_ms: u64,
    pub max_depth: u32,
}

/// The correlation block stamped into the child's logs (RFC 0010
/// §tree-correlation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Telemetry {
    pub run_id: String,
    pub agent_id: String,
    pub agent_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub log_level: String,
    /// Content-capture policy (RFC 0010 §2.9): when true the child logs tool
    /// args/results, not just lengths. Inherited from the parent's `--log-content`.
    #[serde(default)]
    pub log_content: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentloop::stop::TerminalStatus;
    use crate::json::frame;
    use serde_json::json;
    use std::io::Cursor;

    fn payload() -> SpawnPayload {
        SpawnPayload {
            instruction: "summarize the file".into(),
            output_contract: Some("Return a 3-bullet summary.".into()),
            context_seed: vec![SeedMessage { role: "user".into(), content: "prior note".into() }],
            intelligence: IntelConfig {
                uri: "unix:/run/intel.sock".into(),
                token: Some("secret".into()),
                model: Some("m".into()),
            },
            mcp_servers: vec![McpServerSpec { name: "fs".into(), command: vec!["mcp-fs".into()], tags: Vec::new() }],
            limits: Limits { max_steps: 20, max_tokens: 100_000, deadline_ms: 600_000, max_depth: 4 },
            telemetry: Telemetry {
                run_id: "r1".into(),
                agent_id: "0.1".into(),
                agent_path: "0.1".into(),
                trace_id: None,
                log_level: "info".into(),
                log_content: false,
            },
            depth: 1,
            enable_exec: false,
        }
    }

    #[test]
    fn control_spawn_frames_roundtrip() {
        // The whole point of length-framing: an instruction with newlines.
        let mut p = payload();
        p.instruction = "line1\nline2".into();
        let msg = ControlMsg::Spawn(Box::new(p));
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, &msg).unwrap();
        let mut cur = Cursor::new(buf);
        let bytes = frame::read_frame(&mut cur).unwrap().unwrap();
        let back: ControlMsg = serde_json::from_slice(&bytes).unwrap();
        match back {
            ControlMsg::Spawn(p) => assert_eq!(p.instruction, "line1\nline2"),
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn agent_messages_tag_correctly() {
        let result = AgentMsg::Result {
            outcome: Outcome {
                status: TerminalStatus::Completed,
                partial: false,
                result: json!("done"),
                scheduled: Vec::new(),
                subscriptions: Vec::new(),
            },
        };
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains("\"type\":\"result\""));
        assert!(s.contains("\"status\":\"completed\""));

        let pong = serde_json::to_string(&AgentMsg::Pong { seq: 7 }).unwrap();
        assert!(pong.contains("\"type\":\"pong\""));
        assert!(pong.contains("\"seq\":7"));
    }

    #[test]
    fn control_ping_cancel_tags() {
        assert!(serde_json::to_string(&ControlMsg::Ping { seq: 1 }).unwrap().contains("\"type\":\"ping\""));
        assert!(serde_json::to_string(&ControlMsg::Cancel { reason: "drain".into() })
            .unwrap()
            .contains("\"type\":\"cancel\""));
    }
}
