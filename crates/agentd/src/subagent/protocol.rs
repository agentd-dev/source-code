// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor↔subagent control protocol.
//!
//! A minimal JSON-RPC *sibling* — not literal MCP (no `initialize`
//! handshake) — carried length-framed (4-byte prefix, [`::mcp::rpc::frame`])
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
pub const SUBAGENT_ENV: &str = "AGENTD_SUBAGENT";

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
    /// `reason`. A `degrade` is the cheaper model to continue on
    /// (`budget.degrade.model`), resolved to its connection like any other.
    BudgetGrant {
        id: u64,
        ok: bool,
        #[serde(default)]
        wait_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        degrade: Option<Box<IntelConfig>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// What a running child rebuilds its client from when a reload moves its
/// model: the model resolved again, whole — connection, credential, wire
/// model, fallbacks — and the swap policy. Boxed in [`ControlMsg`] to keep
/// the enum small, as [`SpawnPayload`] is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapIntel {
    /// The model as it now resolves; its credential is NEVER logged.
    pub intelligence: IntelConfig,
    /// The model-swap policy: `finish-on-old` (default) or `restart-turn`. Only
    /// matters when the model actually changed — a repoint alone never
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
    /// that the readiness probe and the `agent_intel_all_down` gauge read — one
    /// latched truth, eventually consistent (see
    /// [`crate::signals::set_intel_all_down`]). `active` is best-effort transport
    /// and index ONLY — never a URL or a credential.
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
    /// What the child counted since its last frame — model calls and their
    /// errors, the endpoint's reachability, loop steps — sent ahead of the
    /// next frame, since a child has no `/metrics` of its own
    /// ([`crate::obs::metrics::take_child_metrics`]).
    Metrics(crate::obs::metrics::ChildMetrics),
    /// A credential the child presents to login target `target` could not
    /// be produced (`err` is the cause) or was produced again: every failure,
    /// and a success that is news — the target's first in this child, or its
    /// first since a failure. The supervisor keeps the one record per target
    /// for the tree and says it (`auth.unavailable`, `auth.recovered`), so a
    /// failure every turn worker meets is one line a minute, not one a turn.
    /// `err` names references, files and status codes, never a secret.
    Credential {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        err: Option<String>,
    },
    /// A token the child earned for `authority`: the `AAuth-Access` token the
    /// server issued (Case B), or the user-scoped auth token the Person
    /// Server issued for it (Case C). The supervisor keeps it, and hands it
    /// to the children it spawns after, so the tree earns each token once. A
    /// credential, carried on the wire like [`SpawnPayload`]'s and never
    /// logged.
    #[serde(rename = "aauth_token")]
    AAuthToken {
        authority: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token: Option<String>,
        /// The principal a Case C `auth_token` was earned for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
    },
}

/// Which endpoint is serving the child's intelligence, for
/// [`AgentMsg::IntelHealth`]. The bounded structural identity ONLY — the list
/// index and the transport scheme (`https`, or `http` on loopback) — never the
/// URL, host, or any credential. An index plus a scheme is enough to tell operators which configured
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
    /// The supervisor's AAuth identity, handed to every turn worker and
    /// subagent so the whole process tree signs under ONE identity — a peer
    /// sees the tree as a single agent rather than a crowd of anonymous
    /// processes. Carries the agent token the supervisor holds, so a child
    /// asks the provider for nothing until that token is due a refresh. A
    /// credential: the durable record never keeps it. `#[serde(default)]` so a
    /// frame that omits it parses as "no identity".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aauth: Option<AAuthHandoff>,
    /// Tool names this child must NOT call directly, routing them up to the
    /// supervisor instead.
    ///
    /// A subagent connects to its granted MCP servers itself and calls their
    /// tools without the supervisor ever seeing the call, which would put
    /// every `security.policies` rule out of reach for exactly the caller the
    /// operator is most likely to be narrowing. A policy table that covered
    /// root turns but not subagent turns would be worse than none, because the
    /// operator would believe they were covered — so the supervisor names the
    /// tools a rule might touch and the child round-trips those through the
    /// existing `ToolRequest` channel. Everything else keeps its direct
    /// connection.
    ///
    /// This is a grant the supervisor makes, not a promise the child keeps: a
    /// gated tool is still refused parent-side if the child ignores the list,
    /// because the parent evaluates the policy when the request arrives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gated_tools: Vec<String>,
    /// The definitions of gated tools the child has no connection to list
    /// itself — a Company's (RFC 0047 §5.6), whose servers no child is given
    /// — for it to offer its model. Each is in `gated_tools`, so every call
    /// comes back to the supervisor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supervisor_tools: Vec<crate::wire::intel::ToolDef>,
    /// The MCP tools the child offers, by the names the supervisor's
    /// registry gives them, each with the server and the server's own tool
    /// name: the server's `allow`/`exclude`, its `ns`, a profile server's
    /// contract names and a disabled tool are the registry's decisions, so
    /// a `tools` grant, a policy rule and the gated list name what the child
    /// shows. `None` (a loop run with no registry) offers each server's tools
    /// as advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_routes: Option<std::collections::BTreeMap<String, (String, String)>>,
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

/// The supervisor's AAuth identity as a child is handed it: the same key and
/// provider, and the agent token the supervisor holds now, so the child signs
/// as the supervisor does without asking the provider for anything. A child
/// asks the provider only when the token it was handed is due a refresh, or it
/// was handed none, and only once the provider's metadata is verified — by
/// the supervisor, or by the child itself when the supervisor could not. It
/// rides the private spawn pipe, never the environment or argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AAuthHandoff {
    /// The Agent Provider base URL.
    pub provider: String,
    /// Where the signing key is.
    pub key: AAuthKey,
    /// The key's RFC 7638 thumbprint. A child refuses a key file that no
    /// longer holds this key rather than sign as someone else.
    pub thumbprint: String,
    /// The enrollment token, resolved, for a child that enrolls because the
    /// supervisor has not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_token: Option<String>,
    /// The enrollment-assertion file, read fresh at each enroll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enroll_assertion_file: Option<String>,
    /// The Person Server for user-scoped access (Case C).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_server: Option<String>,
    /// The supervisor verified the provider's metadata.
    #[serde(default)]
    pub verified: bool,
    /// The identity the supervisor enrolled as, so the child does not enroll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The supervisor's agent token and when it stops being good (unix ms,
    /// a minute before it expires), when it holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<HandedToken>,
    /// What the tree has learned of each server it signs for, by authority,
    /// so a child neither asks a server's discovery document again nor earns
    /// a token the tree already holds.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub servers: std::collections::BTreeMap<String, HandedServer>,
    /// The principal the child works for: its Case C tokens are held for
    /// it, and those of `servers` are its.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// What the tree holds for one server: the `AAuth-Access` token it issued
/// (Case B), the user-scoped auth token the Person Server issued for it
/// (Case C), and whether it requires a `content-digest` cover — `None` when
/// it was not discovered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandedServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<bool>,
}

/// Where a child finds the AAuth signing key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AAuthKey {
    /// The key file the supervisor loaded it from. The child reads it and
    /// never creates one.
    File(String),
    /// The base64url seed of a key that has no file.
    Seed(String),
}

/// An agent token a child is handed, and when it stops being good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandedToken {
    pub token: String,
    pub good_until_ms: u64,
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
    /// One decision (RFC 0046 §5.5): no conversation, typed answers out —
    /// native on a decision model, emulated on a chat model.
    Decide,
}

/// A `Decide` turn's work: the decision, how its answers are handed back,
/// and the model an unsure one is asked again on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionJob {
    pub decision: crate::wire::decision::Decision,
    #[serde(default)]
    pub shape: crate::wire::decision::Shape,
    /// The `escalate` model, resolved; asked once when an answer is unsure
    /// and `on_unsure` is `escalate`, and final whatever its confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate: Option<IntelConfig>,
}

/// The least sureness an answer is acted on with, and what an answer under
/// it does (RFC 0046 §5.6) — a `confidence` on a deciding node.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sureness {
    pub confidence: f64,
    pub on_unsure: OnUnsure,
}

/// What an answer under the threshold does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnUnsure {
    /// Ask the same decision again on the model's `escalate` model; its
    /// answer is final.
    Escalate,
    /// Fail the node.
    Fail,
    /// Hand the answer on, marked `unsure: true`.
    Continue,
}

impl OnUnsure {
    pub fn parse(s: &str) -> Option<OnUnsure> {
        match s {
            "escalate" => Some(OnUnsure::Escalate),
            "fail" => Some(OnUnsure::Fail),
            "continue" => Some(OnUnsure::Continue),
            _ => None,
        }
    }
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
    /// A `Decide` turn's decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Box<DecisionJob>>,
    /// A deciding node's `confidence` and `on_unsure`: checked against a
    /// decision's answers, or a structured answer's own `confidence`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sure: Option<Sureness>,
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

/// One model as a worker dials it: the connection, the wire model, and the
/// models to ask when it is unavailable. Built by the runtime from a model
/// reference ([`crate::runtime::Runtime::intel_config`]), never assembled by
/// hand, so every caller gets the same provider, credential and chain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// The provider this connection is (`intelligence.providers.<name>`);
    /// `None` is the top-level connection. Names the credential cache an
    /// `agentd --login` filled: `intelligence:<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The ceiling on one model call (`timeout`); `None` is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The model REFERENCE this was resolved from — a model name or a
    /// literal — so a restore resolves it again against the live settings
    /// instead of replaying a connection that may have moved. `None`: the
    /// `turn` role's model, whichever that is when it is resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The models asked, in order, when this one is unavailable — each on
    /// its own connection (`intelligence.models.<n>.fallback`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<IntelConfig>,
    /// A reasoning model's controls for every call on it (RFC 0046 §5.7):
    /// its defaults, with a node's or a `think` call's `effort` over them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<crate::wire::intel::ReasoningAsk>,
    /// This connection's `structured_output: prompt` (RFC 0046 §5.8): a call
    /// that carries an answer schema is asked in the prompt alone, with no
    /// format on the wire. `false` — `auto` — asks natively wherever the
    /// dialect and the schema allow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub structured_prompt: bool,
    /// Why the credential this connection presents could not be produced
    /// when this was resolved — no login cached, a token endpoint that did
    /// not answer, a token file that could not be read. Every call on it
    /// fails with this, unsent; the next resolution tries again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_error: Option<String>,
}

impl IntelConfig {
    /// The credential cache this connection's login filled.
    fn login_target(&self) -> String {
        match &self.provider {
            Some(p) => format!("intelligence:{p}"),
            None => "intelligence".into(),
        }
    }

    /// The client that dials this model and then, when it is unavailable,
    /// each fallback in order: the one place a model's connection becomes a
    /// client, for a worker and the runtime's own judges alike.
    pub fn client(
        &self,
    ) -> Result<crate::intel::client::IntelClient, crate::intel::client::IntelError> {
        let mut client = self.client_alone()?;
        if let Some((last, rest)) = self.fallbacks.split_last() {
            // Built tail first: each link holds the rest of the chain.
            let mut tail = last.client_alone()?;
            let mut tail_model = last.model.clone().unwrap_or_default();
            for link in rest.iter().rev() {
                tail = link.client_alone()?.with_fallback(tail, tail_model);
                tail_model = link.model.clone().unwrap_or_default();
            }
            client = client.with_fallback(tail, tail_model);
        }
        Ok(client)
    }

    /// This link's own client, without its fallbacks.
    fn client_alone(
        &self,
    ) -> Result<crate::intel::client::IntelClient, crate::intel::client::IntelError> {
        let base = match &self.provider {
            None => crate::intel::client::IntelClient::from_parts(&self.uri, self.token.clone()),
            Some(_) => {
                crate::intel::client::IntelClient::from_provider(&self.uri, self.token.clone())
            }
        };
        let c = base?
            .with_headers(self.headers.clone())
            .with_dialect(self.dialect.as_deref())
            .with_timeout(self.timeout_ms.map(std::time::Duration::from_millis))
            .with_reasoning(self.reasoning)
            .with_structured_prompt(self.structured_prompt)
            .with_credential_error(self.credential_error.clone());
        // `auth: {kind: aws}` SigV4-signs the dial, with credentials fetched
        // at dial time; `source: sso` reads the cache its login filled. A
        // signer that cannot be built fails the calls, never sends them
        // unsigned.
        let Some(aws) = &self.aws_auth else {
            return Ok(c);
        };
        #[cfg(feature = "oauth")]
        let signed = crate::auth::aws::SigV4Signer::from_spec(aws, &self.login_target())
            .map(|s| s as std::sync::Arc<dyn ::mcp::http::RequestSigner>);
        #[cfg(not(feature = "oauth"))]
        let signed: Result<std::sync::Arc<dyn ::mcp::http::RequestSigner>, String> = {
            let _ = aws;
            Err("auth: kind aws needs a build with --features oauth".to_string())
        };
        Ok(match signed {
            Ok(s) => c.with_signer(Some(s)),
            Err(e) => c.with_credential_error(Some(format!("{}: {e}", self.login_target()))),
        })
    }
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
    /// The instance the run belongs to; the child stamps it on its MCP calls
    /// as the reactor does on its own.
    pub instance: String,
    pub agent_id: String,
    pub agent_path: String,
    /// The unit of work the child serves, as W3C Trace Context: the
    /// `traceparent` names the span that spawned it — the step's, the turn's,
    /// the spawning call's — with the unit's flags, and the child makes a span
    /// of its own under it. Its log lines, its MCP requests and its model calls
    /// all carry the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
    pub log_level: String,
    /// Content-capture policy: when true the child logs tool args and results,
    /// not just their lengths. Off by default because those payloads routinely
    /// carry sensitive data. Inherited from the parent's `--log-content`.
    #[serde(default)]
    pub log_content: bool,
    /// Where the child exports its OTLP spans: `observability.otel.endpoint`.
    /// Absent, it falls back to the inherited `OTEL_EXPORTER_OTLP_ENDPOINT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel_endpoint: Option<String>,
    /// Who the child's work is for — `agent/acting_for` and `agent/labels`,
    /// as the supervisor stamps them — carried on every MCP call the child
    /// makes on its own connections.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attribution: serde_json::Map<String, serde_json::Value>,
}

impl Telemetry {
    /// The spawning span, when one was handed down.
    pub fn trace(&self) -> Option<crate::obs::trace::TraceContext> {
        crate::obs::trace::TraceContext::parse(
            self.traceparent.as_deref()?,
            self.tracestate.as_deref(),
        )
    }

    /// Hand the child `t` as its spawning span.
    pub fn set_trace(&mut self, t: &crate::obs::trace::TraceContext) {
        self.traceparent = Some(t.traceparent());
        self.tracestate = t.tracestate.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentloop::stop::TerminalStatus;
    use ::mcp::rpc::frame;
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
            gated_tools: Vec::new(),
            supervisor_tools: Vec::new(),
            mcp_routes: None,
            intelligence: IntelConfig {
                uri: "https://intel.example".into(),
                token: Some("secret".into()),
                model: Some("m".into()),
                ..Default::default()
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
                instance: "i1".into(),
                agent_id: "0.1".into(),
                agent_path: "0.1".into(),
                traceparent: None,
                tracestate: None,
                log_level: "info".into(),
                log_content: false,
                otel_endpoint: None,
                attribution: Default::default(),
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
        // The swap frame carries the model as it now resolves — connection,
        // credential, fallbacks — and the policy. The token rides the wire,
        // as Spawn's does, but is never logged.
        let swap = ControlMsg::SwapIntel(Box::new(SwapIntel {
            intelligence: IntelConfig {
                uri: "https://gw-a.example,https://gw-b.example".into(),
                token: Some("rotated-secret".into()),
                model: Some("claude-haiku-4".into()),
                provider: Some("second".into()),
                fallbacks: vec![IntelConfig {
                    uri: "https://gw-c.example".into(),
                    model: Some("small".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            policy: SwapPolicy::RestartTurn,
        }));
        let s = serde_json::to_string(&swap).unwrap();
        assert!(s.contains("\"type\":\"swap_intel\""));
        assert!(s.contains("\"policy\":\"restart-turn\""));
        let back: ControlMsg = serde_json::from_str(&s).unwrap();
        match back {
            ControlMsg::SwapIntel(p) => {
                assert_eq!(
                    p.intelligence.uri,
                    "https://gw-a.example,https://gw-b.example"
                );
                assert_eq!(p.intelligence.model.as_deref(), Some("claude-haiku-4"));
                assert_eq!(p.intelligence.provider.as_deref(), Some("second"));
                assert_eq!(p.intelligence.fallbacks[0].model.as_deref(), Some("small"));
                assert_eq!(p.policy, SwapPolicy::RestartTurn);
            }
            other => panic!("expected swap_intel, got {other:?}"),
        }
        // A frame with no policy is finish-on-old.
        let minimal: SwapIntel =
            serde_json::from_str(r#"{"intelligence": {"uri":"https://a.example"}}"#).unwrap();
        assert_eq!(minimal.policy, SwapPolicy::FinishOnOld);
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
    fn a_credential_outcome_rides_one_frame() {
        // A failure carries its cause; a success is the target alone.
        let failed = AgentMsg::Credential {
            target: "intelligence".into(),
            err: Some("AWS_ACCESS_KEY_ID is not set".into()),
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"type": "credential", "target": "intelligence",
                   "err": "AWS_ACCESS_KEY_ID is not set"})
        );
        let back: AgentMsg =
            serde_json::from_str(r#"{"type":"credential","target":"mcp:s"}"#).unwrap();
        match back {
            AgentMsg::Credential { target, err } => {
                assert_eq!(target, "mcp:s");
                assert_eq!(err, None);
            }
            other => panic!("expected credential, got {other:?}"),
        }
    }

    #[test]
    fn a_childs_counts_ride_one_frame() {
        // The child's counts go up as one tagged frame, the counts it has
        // flat beside the tag, and come back as written.
        let mut m = crate::obs::metrics::ChildMetrics {
            intel_calls: 2,
            intel_up: Some(true),
            loop_steps: 3,
            ..Default::default()
        };
        m.intel_errors.insert("timeout".into(), 1);
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, &AgentMsg::Metrics(m.clone())).unwrap();
        let wire: serde_json::Value =
            serde_json::from_slice(&buf[4..]).expect("one JSON frame after the length");
        assert_eq!(
            wire,
            json!({"type": "metrics", "intel_calls": 2, "intel_errors": {"timeout": 1},
                   "intel_up": true, "loop_steps": 3})
        );
        let bytes = frame::read_frame(&mut Cursor::new(buf)).unwrap().unwrap();
        let back: AgentMsg = serde_json::from_slice(&bytes).unwrap();
        match back {
            AgentMsg::Metrics(got) => assert_eq!(got, m),
            other => panic!("expected metrics, got {other:?}"),
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

    #[test]
    fn a_server_spec_carries_what_reached_the_server() {
        // A worker starts its dial the way the supervisor reached the server;
        // a spec without the field (nothing reached it yet) parses to none.
        let spec = McpServerSpec {
            name: "gw".into(),
            endpoint: "https://gw/mcp".into(),
            first_request: Some(::mcp::client::FirstRequest::Initialize),
            ..Default::default()
        };
        let back: McpServerSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
        let bare: McpServerSpec =
            serde_json::from_value(serde_json::json!({"name": "gw", "endpoint": "https://gw/mcp"}))
                .unwrap();
        assert_eq!(bare.first_request, None);
    }
}
