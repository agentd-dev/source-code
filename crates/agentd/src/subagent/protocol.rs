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
use crate::config::{A2aPeerSpec, McpServerSpec, SwapPolicy};
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
    /// Suspend the agentic loop at its next turn boundary (RFC 0005 §4.3,
    /// RFC 0015 §4.3). The child's control thread sets a `paused` flag; the loop
    /// waits between turns until [`ControlMsg::Resume`] clears it. The control
    /// thread keeps running while the loop is suspended, so `Resume`/`Ping`/
    /// `Cancel` still arrive — `Cancel` always wins over a pause.
    Pause,
    /// Clear a prior [`ControlMsg::Pause`]: the loop resumes at the next turn.
    Resume,
    /// Ask the child to wind down at the next turn boundary (graceful).
    Cancel { reason: String },
    /// Inject a message into the child's running warm session (parent `send` /
    /// reactive continue); forwarded to the loop by the control reader thread.
    Inject { message: String },
    /// Hot-swap the child's intelligence config at its next turn boundary (RFC
    /// 0018 §5.2). Sent by the supervisor's reload fan-out to every in-flight
    /// child when a reload's diff touches `intelligence`/`model`/`model_swap` —
    /// the same fan-out shape as [`ControlMsg::Pause`], with a payload. The
    /// child's control thread stores it into a child-local LIVE handle; the
    /// agentic loop reads it ONCE at the next turn boundary (where `pause_wait`
    /// sits), rebuilds its [`crate::intel::client::IntelClient`] from the new
    /// endpoint list (fresh health/breaker — a repointed endpoint starts CLOSED,
    /// §5.2 step 2), and adopts the new model. An in-flight `complete_once` is
    /// NEVER torn; the transcript is CONTINUOUS (§5.3, no context reset). The
    /// `token` is a credential carried on the wire like [`SpawnPayload`]'s — it
    /// is NEVER logged (the swap event/logs carry transport+index only).
    SwapIntel(Box<SwapIntel>),
}

/// The intelligence config the child rebuilds its client from on a hot-swap (RFC
/// 0018 §5.2). The endpoint-list URI + the default endpoint-1 credential + the
/// model + the swap policy — exactly the parts [`IntelConfig`] carries plus the
/// policy. Boxed in [`ControlMsg`] to keep the enum small (like [`SpawnPayload`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapIntel {
    /// The new endpoint *list* URI (RFC 0018 §3.1) — a single element is RFC 0006.
    pub uri: String,
    /// Endpoint 1's resolved default credential when its env override is unset
    /// (the same role as [`IntelConfig::token`]); NEVER logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The new model (`None` ⇒ unchanged from the spawn payload's resolved model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model-swap policy (RFC 0018 §5.3): `finish-on-old` (default) |
    /// `restart-turn`. Only matters when `model` actually changed.
    #[serde(default)]
    pub policy: SwapPolicy,
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
    /// A **warm session** finished one turn (its reaction to one delivered
    /// event) and stays alive for the next. Carries that turn's distilled
    /// outcome; unlike [`AgentMsg::Result`] it is **not** terminal (RFC 0008
    /// §spawn-vs-continue). The supervisor applies the turn's self-schedule /
    /// self-subscribe effects and may then `Inject` the next event.
    Turn { outcome: Outcome },
    /// Terminal: the distilled result + final status. Sent exactly once.
    Result { outcome: Outcome },
    /// Terminal: a fatal infrastructure failure (intel/mcp unreachable).
    Failed { error: String },
    /// The child's intelligence reachability, edge-triggered at the breaker/
    /// failover seam (RFC 0018 §6). Emitted ONLY on a transition: on **entering**
    /// all-endpoints-down (every configured endpoint's breaker open / the failover
    /// sweep exhausted) and on **recovering** (any endpoint usable again). The
    /// supervisor has no LLM of its own and no live view of a child's breaker
    /// state, so the child reports it upward; the supervisor latches it into the
    /// `intel_all_down` process-global the readiness probe + `agentd_intel_all_down`
    /// gauge + `agentd://intelligence`/`capacity` bodies read (the one latched
    /// truth, eventually-consistent — see [`crate::signals::set_intel_all_down`]).
    /// `active` is best-effort transport+index ONLY — NEVER a URL or credential
    /// (mirrors the `agentd://intelligence` redaction, RFC 0012 §3.7).
    IntelHealth {
        all_down: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active: Option<IntelActive>,
    },
}

/// Which endpoint is serving the child's intelligence, for [`AgentMsg::IntelHealth`].
/// The bounded structural identity ONLY — the list index + the transport scheme
/// (`unix`/`vsock`/`https`) — never the URL/cid/host or any credential (RFC 0012
/// §3.7, mirroring the `agentd://intelligence` resource redaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelActive {
    pub index: usize,
    pub transport: String,
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
    /// Declared remote-A2A delegation peers (RFC 0020 §3). Inherited by children
    /// like `mcp_servers` so a subagent can also delegate over A2A; the
    /// `a2a.delegate` self-tool dials these. `#[serde(default)]` keeps older
    /// frames (and non-`a2a` peers, which simply send an empty vec) parseable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a2a_peers: Vec<A2aPeerSpec>,
    pub limits: Limits,
    pub telemetry: Telemetry,
    /// Supervisor-minted tree depth (0 = root).
    pub depth: u32,
    /// The operator allowlist of absolute binary paths the gated `exec` self-tool
    /// may invoke (from `--enable-exec <abs-path>`; inherited by children). EMPTY ⇒
    /// exec is off and the tool is never advertised (RFC 0012 §3.6: the executable
    /// is fixed by config, never model-named). `#[serde(default)]` keeps older
    /// frames (which carry no list = exec off) parseable.
    #[serde(default)]
    pub exec_allow: Vec<String>,
    /// Run as a **warm continue-session**: after each turn, stay alive and wait
    /// for the next injected event ([`ControlMsg::Inject`]) instead of exiting,
    /// continuing the same transcript (RFC 0008 §spawn-vs-continue). Default
    /// (false) = a one-shot per-event run. `#[serde(default)]` keeps older frames
    /// parseable.
    #[serde(default)]
    pub warm: bool,
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
            context_seed: vec![SeedMessage {
                role: "user".into(),
                content: "prior note".into(),
            }],
            intelligence: IntelConfig {
                uri: "unix:/run/intel.sock".into(),
                token: Some("secret".into()),
                model: Some("m".into()),
            },
            mcp_servers: vec![McpServerSpec {
                name: "fs".into(),
                command: vec!["mcp-fs".into()],
                tags: Vec::new(),
            }],
            a2a_peers: Vec::new(),
            limits: Limits {
                max_steps: 20,
                max_tokens: 100_000,
                deadline_ms: 600_000,
                max_depth: 4,
            },
            telemetry: Telemetry {
                run_id: "r1".into(),
                agent_id: "0.1".into(),
                agent_path: "0.1".into(),
                trace_id: None,
                log_level: "info".into(),
                log_content: false,
            },
            depth: 1,
            exec_allow: Vec::new(),
            warm: false,
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
        assert!(
            serde_json::to_string(&ControlMsg::Ping { seq: 1 })
                .unwrap()
                .contains("\"type\":\"ping\"")
        );
        assert!(
            serde_json::to_string(&ControlMsg::Cancel {
                reason: "drain".into()
            })
            .unwrap()
            .contains("\"type\":\"cancel\"")
        );
    }

    #[test]
    fn control_swap_intel_roundtrip_and_policy_default() {
        // RFC 0018 §5.2: the swap frame carries the new list/model/policy. The
        // token rides the wire (like Spawn) but is never logged — the resource
        // body / events carry transport+index only.
        let swap = ControlMsg::SwapIntel(Box::new(SwapIntel {
            uri: "vsock:5:9090,vsock:5:9091".into(),
            token: Some("rotated-secret".into()),
            model: Some("claude-haiku-4".into()),
            policy: SwapPolicy::RestartTurn,
        }));
        let s = serde_json::to_string(&swap).unwrap();
        assert!(s.contains("\"type\":\"swap_intel\""));
        assert!(s.contains("\"policy\":\"restart-turn\""));
        let back: ControlMsg = serde_json::from_str(&s).unwrap();
        match back {
            ControlMsg::SwapIntel(p) => {
                assert_eq!(p.uri, "vsock:5:9090,vsock:5:9091");
                assert_eq!(p.model.as_deref(), Some("claude-haiku-4"));
                assert_eq!(p.policy, SwapPolicy::RestartTurn);
            }
            other => panic!("expected swap_intel, got {other:?}"),
        }
        // A frame with no model/token defaults to finish-on-old (an endpoint
        // repoint with no model change).
        let minimal: SwapIntel = serde_json::from_str(r#"{"uri":"unix:/a"}"#).unwrap();
        assert_eq!(minimal.policy, SwapPolicy::FinishOnOld);
        assert!(minimal.model.is_none() && minimal.token.is_none());
    }

    #[test]
    fn intel_health_roundtrips_and_carries_no_url_or_secret() {
        // The child→supervisor reachability report (RFC 0018 §6): tagged like the
        // other AgentMsgs, edge-triggered, transport+index ONLY (never a URL/cred).
        let down = AgentMsg::IntelHealth {
            all_down: true,
            active: None,
        };
        let s = serde_json::to_string(&down).unwrap();
        assert!(s.contains("\"type\":\"intel_health\""));
        assert!(s.contains("\"all_down\":true"));
        // `active` is omitted when absent (the all-down report has no serving ep).
        assert!(!s.contains("active"));
        let back: AgentMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            AgentMsg::IntelHealth {
                all_down: true,
                active: None
            }
        ));

        // The recovered report carries the best-effort active transport+index.
        let up = AgentMsg::IntelHealth {
            all_down: false,
            active: Some(IntelActive {
                index: 1,
                transport: "vsock".into(),
            }),
        };
        let s = serde_json::to_string(&up).unwrap();
        assert!(s.contains("\"all_down\":false"));
        assert!(s.contains("\"index\":1"));
        assert!(s.contains("\"transport\":\"vsock\""));
        // RFC 0012 §3.7: the structural transport scheme only — no scheme-borne
        // address/cid/host/credential rides this message.
        assert!(!s.contains("vsock:"), "no full URI in the report: {s}");
        let back: AgentMsg = serde_json::from_str(&s).unwrap();
        match back {
            AgentMsg::IntelHealth { all_down, active } => {
                assert!(!all_down);
                let a = active.unwrap();
                assert_eq!(a.index, 1);
                assert_eq!(a.transport, "vsock");
            }
            other => panic!("expected intel_health, got {other:?}"),
        }
    }

    #[test]
    fn control_pause_resume_roundtrip() {
        // No-param, serde-tagged like Ready/Pong (RFC 0005 §4.3 / RFC 0015 §4.3).
        let pause = serde_json::to_string(&ControlMsg::Pause).unwrap();
        assert_eq!(pause, "{\"type\":\"pause\"}");
        let resume = serde_json::to_string(&ControlMsg::Resume).unwrap();
        assert_eq!(resume, "{\"type\":\"resume\"}");
        assert!(matches!(
            serde_json::from_str::<ControlMsg>(&pause).unwrap(),
            ControlMsg::Pause
        ));
        assert!(matches!(
            serde_json::from_str::<ControlMsg>(&resume).unwrap(),
            ControlMsg::Resume
        ));
    }
}
