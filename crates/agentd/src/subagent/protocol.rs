// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor↔subagent control protocol.
//!
//! A minimal JSON-RPC *sibling* — not literal MCP (no `initialize`
//! handshake) — carried length-framed (4-byte prefix, [`crate::json::frame`])
//! over the child's stdio pipes, so payloads that contain newlines
//! (instructions, context seeds, distilled results) survive. Two directions:
//! [`ControlMsg`] flows down (supervisor→child), [`AgentMsg`] flows up.
//!
//! The control reader inside the child runs on a thread **separate from the
//! agentic loop**, so `Ping`/`Pong` liveness survives a long in-flight tool or
//! model call: if the reader shared the loop's thread, a slow model call would
//! read as a hung child and the supervisor would reap a healthy process. This
//! module is just the wire types; the spawn mechanics are `supervisor/spawn.rs`,
//! the child side `subagent/control.rs`.

use crate::agentloop::stop::Outcome;
use crate::config::{A2aPeerSpec, McpServerSpec, SwapPolicy};
use crate::wire::intel::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The environment variable the supervisor sets on the child so its `main`
/// takes the subagent path instead of re-parsing CLI config.
pub const SUBAGENT_ENV: &str = "AGENT_SUBAGENT";

// ---- downward: supervisor -> subagent ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    /// The first frame: everything the child needs to run. Sent exactly once.
    Spawn(Box<SpawnPayload>),
    /// Liveness probe; the child's control thread answers [`AgentMsg::Pong`].
    Ping { seq: u64 },
    /// Suspend the agentic loop at its next turn boundary. The child's control
    /// thread sets a `paused` flag; the loop waits between turns until
    /// [`ControlMsg::Resume`] clears it. Pausing at a boundary and never
    /// mid-turn keeps the transcript coherent — a half-finished model call is
    /// never abandoned. The control thread keeps running while the loop is
    /// suspended, so `Resume`/`Ping`/`Cancel` still arrive, and `Cancel` always
    /// wins over a pause so a paused child can still be drained.
    Pause,
    /// Clear a prior [`ControlMsg::Pause`]: the loop resumes at the next turn.
    Resume,
    /// Ask the child to wind down at the next turn boundary (graceful).
    Cancel { reason: String },
    /// Inject a message into the child's running warm session (parent `send` /
    /// reactive continue); forwarded to the loop by the control reader thread.
    Inject { message: String },
    /// Hot-swap the child's intelligence config at its next turn boundary. Sent
    /// by the supervisor's reload fan-out to every in-flight child when a
    /// reload's diff touches `intelligence`/`model`/`model_swap` — the same
    /// fan-out shape as [`ControlMsg::Pause`], with a payload. The child's
    /// control thread stores it into a child-local LIVE handle; the agentic loop
    /// reads it ONCE at the next turn boundary (where `pause_wait` sits),
    /// rebuilds its [`crate::intel::client::IntelClient`] from the new endpoint
    /// list, and adopts the new model. The rebuilt client starts with fresh
    /// health and a CLOSED breaker, because breaker state describes the endpoint
    /// that was just replaced and must not condemn the new one. An in-flight
    /// `complete_once` is NEVER torn and the transcript stays CONTINUOUS, so a
    /// swap costs no context. The `token` is a credential carried on the wire
    /// like [`SpawnPayload`]'s and is NEVER logged — the swap event and logs
    /// carry transport and endpoint index only.
    SwapIntel(Box<SwapIntel>),
    /// The answer to an [`AgentMsg::ToolRequest`] — the supervisor executed the
    /// internal tool; `result` is the tool's output (or an error message when
    /// `is_error`).
    ToolResult {
        id: u64,
        result: Value,
        #[serde(default)]
        is_error: bool,
    },
    /// The answer to an [`AgentMsg::BudgetRequest`]. `ok` means proceed now;
    /// otherwise wait `wait_ms` and ask again, or the request is refused with a
    /// `reason`. A `model` names a cheaper model to degrade to.
    BudgetGrant {
        id: u64,
        ok: bool,
        #[serde(default)]
        wait_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// The intelligence config the child rebuilds its client from on a hot-swap:
/// the endpoint-list URI, the default endpoint-1 credential, the model, and the
/// swap policy — exactly the parts [`IntelConfig`] carries plus the policy.
/// Boxed in [`ControlMsg`] to keep the enum small, as [`SpawnPayload`] is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapIntel {
    /// The new endpoint *list* URI. A list of one is the ordinary single-endpoint
    /// case; more elements are failover candidates tried in order.
    pub uri: String,
    /// Endpoint 1's resolved default credential when its env override is unset
    /// (the same role as [`IntelConfig::token`]); NEVER logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The new model (`None` ⇒ unchanged from the spawn payload's resolved model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model-swap policy: `finish-on-old` (default) or `restart-turn`. Only
    /// matters when `model` actually changed — an endpoint repoint alone never
    /// restarts a turn.
    #[serde(default)]
    pub policy: SwapPolicy,
}

// ---- upward: subagent -> supervisor ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// Setup done (intel + scoped MCP connected); the child is about to loop.
    /// The supervisor's crash-on-spawn fast-fail waits for this frame, so a
    /// child that dies during setup is detected without waiting for a deadline.
    Ready,
    /// Answer to a [`ControlMsg::Ping`].
    Pong { seq: u64 },
    /// A progress event (loop.step, tool.call, …). Arrival also resets the
    /// supervisor's no-progress watchdog, so a child doing visible work is never
    /// reaped for silence. `fields` is opaque to the supervisor except for
    /// correlation.
    Event { event: String, fields: Value },
    /// Incremental token/step usage, which the supervisor folds into the tree's
    /// hierarchical accounting so a subtree cannot outspend the tree ceiling.
    Usage(Usage),
    /// A **warm session** finished one turn (its reaction to one delivered
    /// event) and stays alive for the next. Carries that turn's distilled
    /// outcome; unlike [`AgentMsg::Result`] it is **not** terminal, so the
    /// supervisor must not reap the child on seeing it. The supervisor applies
    /// the turn's self-schedule / self-subscribe effects and may then `Inject`
    /// the next event.
    Turn { outcome: Outcome },
    /// Terminal: the distilled result + final status. Sent exactly once.
    Result { outcome: Outcome },
    /// Terminal: a fatal infrastructure failure (intel/mcp unreachable).
    Failed { error: String },
    /// A HUMAN GATE opened: a workflow `human` node suspended awaiting input.
    /// `node` is the workflow node id; `payload` is the resolved
    /// gate payload (what the human is being asked to look at). The supervisor
    /// records it (the served A2A task projects `input-required`, the gate
    /// resource serves the payload) and later fans the human's reply DOWN as
    /// [`ControlMsg::Inject`]. Non-terminal; also progress for liveness.
    Gate { node: String, payload: Value },
    /// The gate resolved (`via` = `"reply"` | `"uri"` | `"timeout"`): the
    /// supervisor clears the recorded gate and the A2A task returns to
    /// `working`. Non-terminal.
    GateClosed { node: String, via: String },
    /// The child's intelligence reachability, edge-triggered at the breaker /
    /// failover seam. Emitted ONLY on a transition: on **entering**
    /// all-endpoints-down (every configured endpoint's breaker open, the
    /// failover sweep exhausted) and on **recovering** (any endpoint usable
    /// again). Edge-triggering keeps a wedged fleet from flooding the control
    /// channel. The supervisor has no LLM of its own and no live view of a
    /// child's breaker state, so the child is the only party that can report
    /// this; the supervisor latches it into the `intel_all_down` process-global
    /// that the readiness probe, the `agentd_intel_all_down` gauge, and the
    /// `agentd://intelligence` / `capacity` bodies all read — one latched truth,
    /// eventually consistent (see [`crate::signals::set_intel_all_down`]).
    /// `active` is best-effort transport and index ONLY — never a URL or a
    /// credential, matching what the `agentd://intelligence` resource redacts.
    IntelHealth {
        all_down: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active: Option<IntelActive>,
    },
    /// A turn worker / subagent asks the supervisor to execute an **internal**
    /// tool (memory, plan, subagent.run, sleep…). These round-trip rather than
    /// run in the child because the supervisor owns that state; letting a child
    /// mutate it directly would let concurrent children race each other.
    /// Answered by [`ControlMsg::ToolResult`] with the same `id`.
    ToolRequest { id: u64, name: String, args: Value },
    /// Budget admission asked for before a model call, answered by
    /// [`ControlMsg::BudgetGrant`]. Asking first is what lets the supervisor
    /// shape spend across the whole tree instead of after the fact.
    BudgetRequest { id: u64, estimate: u64 },
    /// A `Role::Turn` worker finished its turn — terminal for that worker.
    /// Carries the transcript delta, the usage, and the outcome.
    TurnDone { turn: Box<TurnResult> },
}

/// Which endpoint is serving the child's intelligence, for
/// [`AgentMsg::IntelHealth`]. The bounded structural identity ONLY — the list
/// index and the transport scheme (`unix`/`vsock`/`https`) — never the URL, cid,
/// host, or any credential, matching what the `agentd://intelligence` resource
/// redacts. An index plus a scheme is enough to tell operators which configured
/// endpoint is live without putting an address into logs or events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelActive {
    pub index: usize,
    pub transport: String,
}

// ---- spawn payload ----

/// Everything a subagent needs to run, minted by the supervisor. The child
/// takes none of these fields from its own request — `depth` in particular is
/// derived by the supervisor from the caller's handle, so a child cannot claim a
/// shallower depth to buy itself more levels of delegation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnPayload {
    /// The task. For a delegated child this is the parent's `instruction`
    /// argument; see also `output_contract`.
    pub instruction: String,
    /// Objective, required output format, and boundaries — a real delegation
    /// contract rather than a bare string, so the child's result can be checked
    /// against something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contract: Option<String>,
    /// The narrowed context the parent chose to share — never the parent's full
    /// transcript. Passing only what the child needs keeps its context clean and
    /// stops a prompt injection landed in the parent from riding down the tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_seed: Vec<SeedMessage>,
    /// How to reach the LLM (env/flag-sourced; never logged).
    pub intelligence: IntelConfig,
    /// The child's **scoped** MCP server subset. Always a subset of the parent's,
    /// because scope narrows monotonically down the tree: no child may reach a
    /// server its parent could not.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSpec>,
    /// Declared remote-A2A delegation peers. Inherited by children like
    /// `mcp_servers` so a subagent can also delegate over A2A; the `a2a.delegate`
    /// self-tool dials these. `#[serde(default)]` so a frame that omits the field
    /// — the common case, with no peers configured — parses to an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub a2a_peers: Vec<A2aPeerSpec>,
    /// Extra PEM CA **file path** for outbound TLS trust (`--tls-ca`, the
    /// private/in-cluster PKI anchor). PUBLIC material — a path to a CA
    /// certificate, never key bytes — so it may ride the payload. The child
    /// installs it process-wide before its first dial, so no dial can escape the
    /// anchor, and passes it on to its own children. `#[serde(default)]` so a
    /// frame that omits it parses as "no extra anchor".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca: Option<String>,
    /// AAuth agent-identity settings, inherited by every subagent so the whole
    /// process tree signs MCP requests under ONE identity — a peer sees the tree
    /// as a single agent rather than a crowd of anonymous processes. The key file
    /// is a shared-fs path, like `tls_ca`, and no secret rides here: the
    /// enrollment token stays a `{{secret:…}}` template resolved in the child.
    /// `#[serde(default)]` so a frame that omits it parses as "no identity".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<crate::config::AAuthSettings>,
    pub limits: Limits,
    pub telemetry: Telemetry,
    /// Supervisor-minted tree depth (0 = root).
    pub depth: u32,
    /// Run as a **warm continue-session**: after each turn, stay alive and wait
    /// for the next injected event ([`ControlMsg::Inject`]) instead of exiting,
    /// continuing the same transcript so the agent keeps its memory of earlier
    /// events. Default (false) is a one-shot run per event, which starts each
    /// event from a clean context. `#[serde(default)]` so a frame that omits it
    /// parses as one-shot.
    #[serde(default)]
    pub warm: bool,
    /// The child's role. `agent` (default) runs the ReAct loop on `instruction`
    /// or drives a workflow; `turn` is a **turn worker** driven by `turn` below.
    /// `#[serde(default)]` so a frame that omits it parses as `agent`.
    #[serde(default)]
    pub role: Role,
    /// The turn worker's input (`role: turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<Box<TurnSpec>>,
}

/// The reserved [`SeedMessage::role`] that carries a child's **tool allow-list**
/// — `subagent.run`'s `tools:` narrowing, which is how scope narrows
/// monotonically down the tree. Minted by the supervisor with
/// [`SpawnPayload::narrow_tools`], enforced by the child in
/// [`crate::agentloop::runner::Session::prepare`], which filters its assembled
/// catalogue AND its dispatch against it.
///
/// The grant rides `context_seed` because that is the one part of the payload the
/// child forwards VERBATIM into the loop's `LoopInput` (`subagent/control.rs`), so
/// it reaches the one place the catalogue is assembled without a second adapter
/// hop. The loop CONSUMES it — it is a grant, not a message, and never enters the
/// transcript. The slash makes it uninhabitable by a real role (`system`/`user`/
/// `assistant`/`tool`), and the direction is fail-safe: a marker can only ever
/// REMOVE tools from the grant the supervisor already made, never add one.
pub const ALLOWED_TOOLS_ROLE: &str = "agentd/allowed-tools";

/// Parse an allow-list marker's body (a JSON array of registry patterns: `*`, an
/// exact name, `prefix*`). An unreadable body narrows to NOTHING rather than to
/// everything — a grant that cannot be read is not a grant (fail closed).
pub fn parse_allowed_tools(content: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(content).unwrap_or_default()
}

impl SpawnPayload {
    /// Narrow this child's tool grant to `allow`. Any allow-list entry already
    /// in the seed is dropped first, so the SUPERVISOR's mint is the only
    /// grant the child sees — a caller-supplied `context` array cannot forge or
    /// widen one. An empty `allow` is a real narrowing to nothing, not "no
    /// narrowing"; leave the marker off entirely for the unnarrowed case.
    pub fn narrow_tools(&mut self, allow: &[String]) {
        self.context_seed.retain(|m| m.role != ALLOWED_TOOLS_ROLE);
        self.context_seed.insert(
            0,
            SeedMessage {
                role: ALLOWED_TOOLS_ROLE.to_string(),
                content: serde_json::to_string(allow).unwrap_or_else(|_| "[]".to_string()),
            },
        );
    }

    /// The narrowed grant this payload carries (`None` = unnarrowed: the full
    /// catalogue the granted servers publish). Reads back what
    /// [`SpawnPayload::narrow_tools`] minted — including after a restore, which
    /// re-spawns from the stored payload.
    pub fn allowed_tools(&self) -> Option<Vec<String>> {
        self.context_seed
            .iter()
            .find(|m| m.role == ALLOWED_TOOLS_ROLE)
            .map(|m| parse_allowed_tools(&m.content))
    }
}

/// A checkpoint-resume reference: which checkpoint store (`server`) holds the
/// run under `key`, optionally pinned to a sequence number, and whether to
/// resume `force`fully past a mismatch. Parsed from `--workflow-resume`.
#[cfg(feature = "workflow")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowResumeRef {
    pub server: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default)]
    pub force: bool,
}

/// The child's role: which driver the child process runs after spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A subagent: the ReAct loop on `instruction`, or a workflow driver.
    #[default]
    Agent,
    /// A turn worker: ONE turn over a supplied context slice, with internal
    /// tools round-tripped to the supervisor rather than executed in the child.
    Turn,
}

/// What kind of turn a `Role::Turn` worker runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// A root/conversation turn: tools, may act, ends with a reply.
    #[default]
    Turn,
    /// A structured reasoning call: no tools, an object out (`think`, preflight,
    /// compaction).
    Think,
    /// A bounded agentic run for a workflow `agent` step / subagent: tools,
    /// output contract/schema, ends with a result.
    Agent,
}

/// The turn worker's input: everything the child needs to run exactly one turn
/// — the system prompt, the context slice, the tool definitions and which of
/// them round-trip to the supervisor, the output schema, and the knobs. The
/// worker holds no state of its own between turns; whatever it needs is here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TurnSpec {
    #[serde(default)]
    pub kind: TurnKind,
    /// The full system prompt (instruction + capabilities + skills + summary).
    pub system: String,
    /// The transcript slice (context messages incl. the triggering event).
    #[serde(default)]
    pub messages: Vec<crate::context::Msg>,
    /// LLM-facing tool definitions (every class).
    #[serde(default)]
    pub tools: Vec<crate::wire::intel::ToolDef>,
    /// Tool names that ROUND-TRIP to the supervisor (internal + mapped).
    #[serde(default)]
    pub internal: Vec<String>,
    /// MCP-class tools the child calls itself: tool name → (server, wire tool).
    #[serde(default)]
    pub mcp_routes: std::collections::BTreeMap<String, (String, String)>,
    /// Validate the final answer against this schema (structured turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Max model rounds in this turn (0 = the payload's `limits.max_steps`).
    #[serde(default)]
    pub max_rounds: u32,
    /// Ask the supervisor for budget admission before every model call.
    #[serde(default)]
    pub budget_admission: bool,
    /// The idempotency-key prefix for effects (`<ctx>/<turn>`). Every effect this
    /// turn issues derives its key from this prefix, so a replayed turn reuses
    /// the same keys and a retried effect is deduplicated instead of repeated.
    #[serde(default)]
    pub idempotency_prefix: String,
    /// Extra `_meta` stamped on MCP tool calls (run/ctx/principal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_meta: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Per-response completion cap (0 = default).
    #[serde(default)]
    pub max_tokens_per_call: u32,
    /// The turn id (for logs and idempotency).
    #[serde(default)]
    pub turn_id: String,
}

/// A finished turn. `messages` is the transcript DELTA — only the assistant and
/// tool messages appended during this turn, in order — which the supervisor
/// concatenates onto the context it already holds. Sending a delta rather than
/// the whole transcript keeps the frame bounded as a conversation grows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TurnResult {
    /// `completed` | `refused` | `exhausted_steps` | `exhausted_tokens` |
    /// `deadline` | `loop_detected` | `cancelled` | `failed`.
    pub status: String,
    #[serde(default)]
    pub messages: Vec<crate::context::Msg>,
    /// The final text (a reply / the answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The parsed structured value (structured turns / schema'd answers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub rounds: u32,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The `finish` call, if the model made one (`{status, output, reason, exit}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<Value>,
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
    /// The resolved `intelligence.headers` — arbitrary per-dial headers, such as
    /// a gateway routing header. A value may itself resolve a secret, so these
    /// ride the payload already resolved and are never logged. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// An `intelligence.auth: { kind: aws }` spec so the child can SigV4-sign
    /// the LLM dial. Carries no secret: the credentials are fetched from the
    /// environment, IMDS, IRSA, or the SSO cache at dial time. `None` when the
    /// endpoint is not AWS-signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_auth: Option<crate::config::AuthSpec>,
    /// The wire dialect: `openai` (default), `anthropic`, or `bedrock`. `None`
    /// means the OpenAI-compatible request shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub max_steps: u32,
    pub max_tokens: u64,
    /// Wall-clock deadline in milliseconds from the child's start. The child
    /// arms its own deadline AND the supervisor tracks an absolute one: the
    /// second copy is what still fires when the child is too wedged to honour
    /// the first.
    pub deadline_ms: u64,
    pub max_depth: u32,
    /// OS-level caps, applied between fork and exec (`setrlimit`) — real
    /// resource allocation, not protocol accounting. `None` = inherit.
    /// `memory_bytes` → `RLIMIT_AS`; `cpu_seconds` → `RLIMIT_CPU` (the kernel
    /// sends SIGXCPU at the soft cap, SIGKILL at hard = soft + 5 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<u64>,
    /// Niceness delta from `priority:` (`low` → +10, `high` → −5 best-effort —
    /// raising needs privilege and is skipped silently without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i32>,
}

/// The correlation block stamped into the child's logs, so every line a subtree
/// emits can be joined back to the run and to its position in the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Telemetry {
    pub run_id: String,
    pub agent_id: String,
    pub agent_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub log_level: String,
    /// Content-capture policy: when true the child logs tool args and results,
    /// not just their lengths. Off by default because those payloads routinely
    /// carry sensitive data. Inherited from the parent's `--log-content`.
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
                uri: "https://intel.example".into(),
                token: Some("secret".into()),
                model: Some("m".into()),
                headers: Vec::new(),
                aws_auth: None,
                dialect: None,
            },
            mcp_servers: vec![McpServerSpec {
                name: "fs".into(),
                endpoint: "unix:/mcp-fs.sock".into(),
                tags: Vec::new(),
                ..Default::default()
            }],
            a2a_peers: Vec::new(),
            tls_ca: None,
            aauth: None,
            limits: Limits {
                max_steps: 20,
                max_tokens: 100_000,
                deadline_ms: 600_000,
                max_depth: 4,
                memory_bytes: None,
                cpu_seconds: None,
                nice: None,
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
            warm: false,
            role: crate::subagent::protocol::Role::Agent,
            turn: None,
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
        // The swap frame carries the new endpoint list, model and policy. The
        // token rides the wire, as Spawn's does, but is never logged — the
        // resource body and events carry transport and index only.
        let swap = ControlMsg::SwapIntel(Box::new(SwapIntel {
            uri: "https://gw-a.example,https://gw-b.example".into(),
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
                assert_eq!(p.uri, "https://gw-a.example,https://gw-b.example");
                assert_eq!(p.model.as_deref(), Some("claude-haiku-4"));
                assert_eq!(p.policy, SwapPolicy::RestartTurn);
            }
            other => panic!("expected swap_intel, got {other:?}"),
        }
        // A frame with no model/token defaults to finish-on-old (an endpoint
        // repoint with no model change).
        let minimal: SwapIntel = serde_json::from_str(r#"{"uri":"https://a.example"}"#).unwrap();
        assert_eq!(minimal.policy, SwapPolicy::FinishOnOld);
        assert!(minimal.model.is_none() && minimal.token.is_none());
    }

    #[test]
    fn intel_health_roundtrips_and_carries_no_url_or_secret() {
        // The child→supervisor reachability report: tagged like the other
        // AgentMsgs, edge-triggered, transport and index ONLY — never a URL or
        // a credential.
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
                transport: "https".into(),
            }),
        };
        let s = serde_json::to_string(&up).unwrap();
        assert!(s.contains("\"all_down\":false"));
        assert!(s.contains("\"index\":1"));
        assert!(s.contains("\"transport\":\"https\""));
        // The structural transport scheme only — no address, cid, host or
        // credential rides this message.
        assert!(!s.contains("https://"), "no full URI in the report: {s}");
        let back: AgentMsg = serde_json::from_str(&s).unwrap();
        match back {
            AgentMsg::IntelHealth { all_down, active } => {
                assert!(!all_down);
                let a = active.unwrap();
                assert_eq!(a.index, 1);
                assert_eq!(a.transport, "https");
            }
            other => panic!("expected intel_health, got {other:?}"),
        }
    }

    #[test]
    fn narrow_tools_mints_one_supervisor_grant_and_survives_the_wire() {
        // `subagent.run`'s `tools:` is a GRANT the child enforces. It rides
        // the seed under the reserved role, exactly once, minted by the
        // supervisor — a forged entry a caller smuggled in through `context` is
        // dropped, so the child can never see two disagreeing grants.
        let mut p = payload();
        p.context_seed.insert(
            0,
            SeedMessage {
                role: ALLOWED_TOOLS_ROLE.into(),
                content: "[\"*\"]".into(),
            },
        );
        p.narrow_tools(&["knowledge.search".to_string()]);
        assert_eq!(
            p.context_seed
                .iter()
                .filter(|m| m.role == ALLOWED_TOOLS_ROLE)
                .count(),
            1,
            "one grant only — the forged `*` is gone"
        );
        assert_eq!(
            p.allowed_tools(),
            Some(vec!["knowledge.search".to_string()])
        );
        // The real seed messages are untouched by the mint.
        assert!(p.context_seed.iter().any(|m| m.content == "prior note"));

        // It survives the control frame (a restore re-spawns from this payload).
        let msg = ControlMsg::Spawn(Box::new(p));
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, &msg).unwrap();
        let back: ControlMsg =
            serde_json::from_slice(&frame::read_frame(&mut Cursor::new(buf)).unwrap().unwrap())
                .unwrap();
        match back {
            ControlMsg::Spawn(p) => assert_eq!(
                p.allowed_tools(),
                Some(vec!["knowledge.search".to_string()])
            ),
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn an_unnarrowed_payload_has_no_grant_and_a_broken_one_grants_nothing() {
        // No marker = no narrowing (the root/embedded shape) — the child keeps
        // the full catalogue its granted servers publish.
        assert_eq!(payload().allowed_tools(), None);
        // A body that will not parse is NOT read as "everything": fail closed.
        assert!(parse_allowed_tools("not json").is_empty());
        assert!(parse_allowed_tools("[\"a\",\"b.*\"]").len() == 2);
    }

    #[test]
    fn control_pause_resume_roundtrip() {
        // No-param, serde-tagged like Ready/Pong.
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
