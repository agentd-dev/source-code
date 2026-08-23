// SPDX-License-Identifier: AGPL-3.0-only
//! **Configuration schema v2** (RFC 0030) — the agentd 2.0 settings document.
//!
//! One nested document (YAML or JSON; several files merge in order) whose
//! every path is also `AGENTD_<PATH>` / `AGENT_<PATH>` / `<PATH>` and
//! `--<path>`. This module holds the typed [`Settings`], its JSON Schema
//! ([`schema::schema`]), the load pipeline (files → env → flags → typed →
//! validated), the legacy **alias** table (`--instruction`, `--intelligence`,
//! `--model`, `--mcp`, …), the `agentd --instruction X` **sugar**, v1/v2
//! **detection**, and the reload partition (restart-only paths).
//!
//! Layering (RFC 0011 §2.1 / RFC 0017 §3.2, unchanged): `built-in < files <
//! env < flags`. Files compose with JSON-Merge-Patch semantics; env sets a
//! path (lists/maps replaced); flags apply in argument order — a generic
//! `--<path>` SETS, a named repeatable alias (`--mcp`, `--a2a-peer`) ADDS.
//!
//! The 2.0 runtime consumes [`Settings`]; the v1 [`super::Config`] keeps
//! serving the v1 runtime until the cut-over (plan §6 P5).

pub mod schema;

use super::file::{self, Format};
use super::paths::{self, Binding};
use super::{ConfigError, usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

/// A duration deserialized from `"10m"` / `"500ms"` / bare seconds (string or
/// integer). Displays in the same string form.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dur(pub Duration);

impl fmt::Debug for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(s) => Ok(Dur(Duration::from_secs(s))),
            Raw::Text(t) => super::parse_duration(&t)
                .map(Dur)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// A credential-bearing string: from a FILE it must be a `{{secret:…}}` /
/// `{{secret-file:…}}` reference (§5 validation over the file document); from
/// env/flags it may be inline. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Deserialize, Default)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// `all` | `none` | an explicit list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ToolSelect {
    Keyword(SelectKeyword),
    List(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectKeyword {
    All,
    None,
}

impl Default for ToolSelect {
    fn default() -> Self {
        ToolSelect::Keyword(SelectKeyword::All)
    }
}

impl ToolSelect {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            ToolSelect::Keyword(SelectKeyword::All) => true,
            ToolSelect::Keyword(SelectKeyword::None) => false,
            ToolSelect::List(l) => l.iter().any(|n| n == name),
        }
    }
}

fn string_or_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        List(Vec<String>),
        One(String),
    }
    Ok(match Raw::deserialize(d)? {
        Raw::List(l) => l,
        Raw::One(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// The typed v2 settings document. Every object is `deny_unknown_fields`;
/// every section defaults so a minimal document (`agent.instruction` alone)
/// is complete.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub config_version: Option<String>,
    /// Operator-defined constants, referenced anywhere in this document and in
    /// workflow definitions as `{{config.NAME}}` (dotted paths reach into
    /// nested values). The template prefix is `config.` and NOT `vars.`
    /// because `vars.*` already names a RUN's own variables — two things
    /// called `vars` in one template language would be a permanent trap.
    ///
    /// Substitution is fail-closed: a reference to an undefined name refuses
    /// startup naming every unresolved reference at once. In workflows the
    /// values fold in at LOAD time, so they participate in the definition hash
    /// — a var change is a definition change, and in-flight runs stay pinned
    /// to the definition they started with.
    /// Named durable event streams (RFC 0035 Phase A): `{name: {retention:
    /// {max_events, max_age}}}`. A stream must be declared before an `emit`
    /// or `stream` node may reference it — fail-closed at startup.
    #[serde(default)]
    pub streams: BTreeMap<String, StreamCfg>,
    /// The **service catalog** (RFC 0037): the named external services this
    /// deployment may use. Entries carry connection settings, one shared
    /// credential, authoritative trifecta tags and a tool-surface ceiling;
    /// `mcp.servers` entries reference them via `service:`. Absent ⇒ no
    /// catalog, today's behavior exactly.
    #[serde(default)]
    pub services: BTreeMap<String, Service>,
    pub vars: BTreeMap<String, Value>,
    pub agent: Agent,
    pub intelligence: Intelligence,
    pub mcp: Mcp,
    pub tools: Tools,
    pub store: Store,
    pub memory: Memory,
    pub context: Context,
    pub knowledge: Knowledge,
    pub search: Search,
    pub skills: Skills,
    /// Inline dialect-3 definitions or `{name, file|uri}` references — kept as
    /// raw documents here; the workflow engine (RFC 0027) types them.
    pub workflows: Vec<Value>,
    pub limits: Limits,
    pub lifecycle: Lifecycle,
    /// Subagent templates + spawn policy (RFC 0036): operator-declared
    /// definitions the model may instantiate (filling declared `params` only),
    /// section-wide defaults, and the freeform-spawn switch.
    pub subagents: Subagents,
    pub a2a: A2a,
    /// The display-client surface (RFC 0032): opt-in TUI/web-UI methods on the
    /// A2A listener (the global `SubscribeToEvents` feed + interface read ops).
    pub interface: Interface,
    /// The inbound webhook HTTP surface (RFC 0027): a dedicated listener for
    /// `webhook` start nodes and `wait: {on: webhook}` callbacks.
    pub webhooks: Webhooks,
    /// The self-correcting goal watchdog (RFC 0026): a periodic check of whether
    /// the configured goal is achieved (or the agent is stuck).
    pub goal: Option<Goal>,
    pub observability: Observability,
    pub security: Security,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Agent {
    pub name: Option<String>,
    /// Static text, or a single-token URI a configured MCP server serves
    /// (read + subscribed) — one field, parsed (RFC 0028 §3).
    pub instruction: Option<String>,
    /// A **one-shot task** (`--prompt`). With no workflows configured this is
    /// what the generated run executes, while `instruction` stays the standing
    /// policy (it becomes the run's system prompt). Given alone, the prompt is
    /// the whole job — `agentd --prompt "…" --intelligence …` runs it once and
    /// exits with the answer on stdout.
    pub prompt: Option<String>,
    /// Skills defined by `:::skill` directives in the instruction — DERIVED
    /// (never a config key): `Settings::from_document` extracts them, the
    /// runtime feeds them to the catalogue. In the struct so a reload diff
    /// sees an edited inline skill as an agent change.
    #[serde(skip)]
    pub inline_skills: Vec<crate::config::directives::InlineSkill>,
    pub preflight: Preflight,
    pub wake_on: Option<Vec<WakeEvent>>,
    pub on_workflow_finished: OnWorkflowFinished,
    pub tools: AgentTools,
    pub max_parallel_turns: Option<u32>,
    pub conversation_budget: Option<Budget>,
    /// What `ask_human` does when NO human channel can answer — the interface
    /// is disabled — and, for `auto`, when a gate times out unanswered
    /// (RFC 0032 §16): `fail` (default; the ask errors immediately), `wait`
    /// (park until the ask timeout), or `auto` (an LLM judge answers on the
    /// operator's behalf, conservatively, marked as auto).
    pub ask_human_fallback: AskHumanFallback,
    /// What a gate does when a human COULD answer (RFC 0032 §16).
    ///
    /// `ask_human_fallback` governs the case where nobody can answer;
    /// this governs whether to ask at all. They are separate because they are
    /// separate questions: "there is no channel" is a fact about deployment,
    /// "do not interrupt me" is a policy about attention.
    pub approval: Approval,
}

/// How much a person wants to be asked (RFC 0032 §16).
///
/// Runtime-settable, because the right answer changes with what the agent is
/// doing: you supervise closely while it is somewhere unfamiliar and stop
/// wanting to be asked once it is doing something you have watched it do
/// twenty times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    /// Ask a person and wait. The default: a gate exists because someone
    /// wanted a decision, so the decision is theirs unless told otherwise.
    #[default]
    #[serde(alias = "await", alias = "human")]
    Ask,
    /// An LLM judge decides whether it is safe to proceed, conservatively, and
    /// the answer is marked `via: auto` so nobody mistakes it for a person's.
    Auto,
    /// Take the recommendation without asking.
    ///
    /// Only usable when the ask CARRIES one — a `recommend` argument or a
    /// schema `default`. With neither there is nothing to accept, and inventing
    /// an answer would be worse than the interruption, so it degrades to
    /// `auto` rather than guessing.
    #[serde(alias = "accept_all", alias = "yes")]
    Accept,
}

/// The `ask_human` fallback disposition (RFC 0032 §16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AskHumanFallback {
    /// Park the ask until its timeout (then it fails).
    #[serde(alias = "pause", alias = "idle")]
    Wait,
    /// Error immediately — the caller (model / workflow policy) decides.
    #[default]
    #[serde(alias = "finish", alias = "stop")]
    Fail,
    /// An LLM judge answers on the operator's behalf (also fires when an
    /// interface-served gate times out unanswered). `UNDECIDED` ⇒ fail.
    Auto,
}

impl Agent {
    /// The default wake set (RFC 0026 §3.1).
    pub fn wake_on(&self) -> Vec<WakeEvent> {
        self.wake_on.clone().unwrap_or_else(|| {
            vec![
                WakeEvent::A2aMessage,
                WakeEvent::HumanReply,
                WakeEvent::SubagentResult,
                WakeEvent::WorkflowFailed,
            ]
        })
    }
    pub fn max_parallel_turns(&self) -> u32 {
        self.max_parallel_turns.unwrap_or(4)
    }
    /// Whether the instruction is a resource reference (a single-token URI).
    pub fn instruction_is_uri(&self) -> bool {
        self.instruction
            .as_deref()
            .is_some_and(looks_like_resource_uri)
    }
}

/// `scheme://…` with no whitespace, and a scheme that is not a bare `http(s)`
/// URL to a web page… — any `<alpha><alnum+.->://` single token counts; the
/// registry decides which server serves it (RFC 0028 §3).
pub fn looks_like_resource_uri(s: &str) -> bool {
    let t = s.trim();
    if t.contains(char::is_whitespace) {
        return false;
    }
    let Some((scheme, rest)) = t.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && !rest.is_empty()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Preflight {
    Never,
    #[default]
    Auto,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeEvent {
    A2aMessage,
    HumanReply,
    SubagentResult,
    WorkflowFinished,
    WorkflowFailed,
    InstructionUpdated,
    BudgetResumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnWorkflowFinished {
    Ignore,
    #[default]
    Note,
    Think,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AgentTools {
    pub internal: ToolSelect,
    pub mcp: ToolSelect,
    pub code: ToolSelect,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Intelligence {
    #[serde(deserialize_with = "string_or_list")]
    pub endpoints: Vec<String>,
    pub model: Option<String>,
    /// The wire dialect (RFC 0031 §8): `openai` (default), `anthropic`, or
    /// `bedrock` (native Amazon Bedrock Converse — pair with `auth: {kind: aws,
    /// service: bedrock}`). Unset ⇒ OpenAI-compatible.
    pub dialect: Option<String>,
    pub token: Option<Secret>,
    pub token_file: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// A unified credential provider (RFC 0031 §5) for the LLM endpoint — e.g.
    /// `oauth2` device-login for an enterprise gateway. Obtained via
    /// `agentd login intelligence`; the resolved bearer overrides `token`.
    pub auth: Option<Auth>,
    pub swap_policy: Option<String>,
    pub structured_output: StructuredOutput,
    pub budget: Budget,
    pub pricing: BTreeMap<String, Pricing>,
    pub timeout: Option<Dur>,
}

impl Intelligence {
    pub fn timeout(&self) -> Duration {
        self.timeout.map(|d| d.0).unwrap_or(Duration::from_secs(60))
    }
    /// The comma-joined endpoint list URI the v1 intelligence client speaks.
    pub fn endpoint_list(&self) -> Option<String> {
        if self.endpoints.is_empty() {
            None
        } else {
            Some(self.endpoints.join(","))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    #[default]
    Auto,
    JsonSchema,
    Tool,
    Prompt,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Budget {
    pub windows: Vec<BudgetWindow>,
    pub lifetime_tokens: Option<u64>,
    pub scope: Option<Vec<BudgetScope>>,
    pub on_exhausted: BudgetTactic,
    pub slow: Slow,
    pub degrade: Degrade,
    pub reserve: Reserve,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BudgetWindow {
    pub per: WindowUnit,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub requests: Option<u64>,
    #[serde(default)]
    pub reset: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
}

impl WindowUnit {
    pub fn duration(self) -> Duration {
        match self {
            WindowUnit::Second => Duration::from_secs(1),
            WindowUnit::Minute => Duration::from_secs(60),
            WindowUnit::Hour => Duration::from_secs(3600),
            WindowUnit::Day => Duration::from_secs(86_400),
            WindowUnit::Week => Duration::from_secs(7 * 86_400),
        }
    }
    /// Calendar windows reset at a wall-clock time; rolling windows are buckets.
    pub fn is_calendar(self) -> bool {
        matches!(self, WindowUnit::Day | WindowUnit::Week)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetScope {
    Instance,
    Run,
    Conversation,
    Principal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BudgetTactic {
    #[default]
    Wait,
    Slow,
    Degrade,
    Refuse,
    Fail,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Slow {
    pub factor: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Degrade {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Reserve {
    pub estimate: ReserveEstimate,
    pub fixed: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReserveEstimate {
    #[default]
    Context,
    Fixed,
    None,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Pricing {
    pub input_per_1k: Option<f64>,
    pub output_per_1k: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Mcp {
    pub servers: Vec<McpServer>,
    pub default_timeout: Option<Dur>,
}

/// A **service catalog entry** (RFC 0037 §4): a named external service this
/// deployment may use — connection settings, one shared credential,
/// authoritative trifecta tags (a floor for any matching endpoint, §3
/// Decision 3), and a tool-surface ceiling consumers can only narrow.
/// The catalog dials nothing; `mcp.servers` entries reference it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub kind: ServiceKind,
    pub endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Authoritative trifecta tags: unioned into any consumer whose endpoint
    /// matches this entry — referencing or inline, `open` or `closed` mode.
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
    /// The CEILING: the widest advertised-tool surface any consumer may get.
    /// A consumer `allow` pattern not subsumed by this list is refused.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub auth: Option<Auth>,
    /// Per-instance pacing toward the service (`<burst>/<per>`, e.g. `60/1m`),
    /// shared by every consumer of the entry in this process.
    #[serde(default)]
    pub rate: Option<String>,
    #[serde(default)]
    pub timeout: Option<Dur>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    #[default]
    Mcp,
}

/// RFC 0037 §4: resolve `service:` references against the catalog and apply
/// the unconditional tag floor (Decision 3). Mutates `mcp.servers` in place —
/// after this, every server carries its effective endpoint, auth, headers,
/// admission lists and tag set, so validation, the trifecta gate and the
/// runtime all see the *outcome*. Returns the resolution errors (aggregated
/// with validation's).
pub fn resolve_services(s: &mut Settings) -> Vec<String> {
    let services = s.services.clone();
    let mut errs = Vec::new();
    for srv in &mut s.mcp.servers {
        let Some(name) = srv.service.clone() else {
            continue;
        };
        let Some(entry) = services.get(&name) else {
            errs.push(format!(
                "mcp server '{}' references unknown service '{name}' (services.{name} is not declared)",
                srv.name
            ));
            continue;
        };
        // Consumers reference, never restate (Decision 2): connection settings
        // live in the catalog only.
        for (restated, what) in [
            (!srv.endpoint.is_empty(), "endpoint"),
            (srv.auth.is_some(), "auth"),
            (srv.oauth.is_some(), "oauth"),
            (!srv.headers.is_empty(), "headers"),
        ] {
            if restated {
                errs.push(format!(
                    "mcp server '{}' references service '{name}' and restates `{what}` — a referencing consumer inherits connection settings from the catalog",
                    srv.name
                ));
            }
        }
        srv.endpoint = entry.endpoint.clone();
        srv.auth = entry.auth.clone();
        srv.headers = entry.headers.clone();
        if srv.timeout.is_none() {
            srv.timeout = entry.timeout;
        }
        // The ceiling: consumer `allow` may only narrow (every consumer
        // pattern must be subsumed by some catalog pattern); absent consumer
        // `allow` inherits the ceiling itself. `exclude` unions.
        match (&entry.allow, &mut srv.allow) {
            (Some(ceil), Some(mine)) => {
                for p in mine.iter() {
                    if !ceil.iter().any(|c| pattern_subsumes(p, c)) {
                        errs.push(format!(
                            "mcp server '{}': allow pattern '{p}' widens the ceiling of service '{name}' (catalog allow: {ceil:?})",
                            srv.name
                        ));
                    }
                }
            }
            (Some(ceil), mine @ None) => *mine = Some(ceil.clone()),
            _ => {}
        }
        for e in &entry.exclude {
            if !srv.exclude.contains(e) {
                srv.exclude.push(e.clone());
            }
        }
        union_tags(&mut srv.tags, &entry.tags);
    }
    // Decision 3 — the unconditional tag floor: ANY server whose endpoint
    // matches a catalog entry gets the entry's tags unioned in, referencing or
    // inline, open or closed. This is the tag-laundering fix.
    for srv in &mut s.mcp.servers {
        if srv.endpoint.is_empty() {
            continue;
        }
        if let Some((_, entry)) = service_match(&services, &srv.endpoint) {
            union_tags(&mut srv.tags, &entry.tags);
        }
    }
    errs
}

/// Does catalog pattern `ceiling` cover consumer pattern `p`? Patterns are the
/// registry's trailing-`*` globs. A literal ceiling covers only itself; a
/// glob ceiling covers any pattern whose fixed prefix extends the ceiling's.
fn pattern_subsumes(p: &str, ceiling: &str) -> bool {
    match ceiling.strip_suffix('*') {
        Some(prefix) => p.strip_suffix('*').unwrap_or(p).starts_with(prefix),
        None => p == ceiling,
    }
}

/// Union `from` into `into` (per tool-pattern key; tag lists dedup).
fn union_tags(into: &mut BTreeMap<String, Vec<String>>, from: &BTreeMap<String, Vec<String>>) {
    for (k, list) in from {
        let slot = into.entry(k.clone()).or_default();
        for t in list {
            if !slot.contains(t) {
                slot.push(t.clone());
            }
        }
    }
}

/// Match a URL against the catalog (RFC 0037 §4): scheme + authority equal
/// (host case-insensitive), and the URL's path extends the entry's path on a
/// segment boundary. Returns the matching entry, refusing nothing — the
/// caller decides what a non-match means (`Egress::Closed` refuses it).
pub fn service_match<'a>(
    services: &'a BTreeMap<String, Service>,
    url: &str,
) -> Option<(&'a String, &'a Service)> {
    let (scheme, authority, path) = split_url(url)?;
    services.iter().find(|(_, e)| {
        let Some((es, ea, ep)) = split_url(&e.endpoint) else {
            return false;
        };
        scheme == es
            && authority.eq_ignore_ascii_case(&ea)
            && (ep.is_empty()
                || ep == "/"
                || path == ep
                || (path.starts_with(&ep)
                    && (ep.ends_with('/') || path.as_bytes().get(ep.len()) == Some(&b'/'))))
    })
}

/// `scheme://authority/path` → (scheme, authority, path). `unix:` sockets
/// have no authority; the socket path is the authority for matching purposes.
fn split_url(url: &str) -> Option<(String, String, String)> {
    if let Some(rest) = url
        .strip_prefix("unix://")
        .or_else(|| url.strip_prefix("unix:"))
    {
        return Some(("unix".into(), rest.to_string(), String::new()));
    }
    let (scheme, rest) = url.split_once("://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, String::new()),
    };
    Some((scheme.to_string(), authority.to_string(), path))
}

/// RFC 0037 §5: the dial-time egress check. `Open` always passes; `Closed`
/// requires the URL to match a catalog entry.
pub fn egress_allows(
    services: &BTreeMap<String, Service>,
    egress: Egress,
    url: &str,
) -> Result<(), String> {
    if egress == Egress::Open || service_match(services, url).is_some() {
        return Ok(());
    }
    Err(format!(
        "security.egress is `closed` and {url} matches no services: catalog entry — catalog the endpoint to allow it"
    ))
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    /// Either a literal URL, or empty when `service:` references a catalog
    /// entry (RFC 0037 §4) — resolution fills it before anything dials.
    #[serde(default)]
    pub endpoint: String,
    /// Reference a `services:` catalog entry: inherit its connection settings
    /// (restating `endpoint`/`auth`/`headers` here is refused) and narrow its
    /// tool ceiling.
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub ns: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
    /// Tool admission control, on the server's ADVERTISED names (before any
    /// `ns` prefixing): with `allow`, only matching tools register; anything
    /// matching `exclude` never registers, and exclude beats allow. Globs are
    /// the registry's `pattern_matches` (trailing `*`).
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub aauth: Option<bool>,
    #[serde(default)]
    pub oauth: Option<McpOauth>,
    /// A unified credential provider (RFC 0031 §5) — `static` / `oauth2` (device
    /// login, refresh). Interactive providers obtain their token via
    /// `agentd login mcp:<name>`; the daemon reads the cached token. Coexists
    /// with the legacy `oauth` shortcut (client-credentials).
    #[serde(default)]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub timeout: Option<Dur>,
}

impl McpServer {
    /// The flattened, deduplicated trifecta tag set (RFC 0012 §3.1).
    pub fn tag_set(&self) -> Result<Vec<crate::sec::scope::TrifectaTag>, String> {
        let mut out = Vec::new();
        for list in self.tags.values() {
            for t in list {
                let tag = crate::sec::scope::TrifectaTag::parse(t).ok_or_else(|| {
                    format!("mcp server '{}' has unknown trifecta tag '{t}'", self.name)
                })?;
                if !out.contains(&tag) {
                    out.push(tag);
                }
            }
        }
        Ok(out)
    }

    /// The v1 runtime spec (the MCP client / spawn payload shape).
    pub fn to_spec(&self) -> Result<super::McpServerSpec, String> {
        Ok(super::McpServerSpec {
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            headers: self
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            tags: self.tag_set()?,
            aauth: self.aauth,
            // RFC 0031: carry the OAuth client-credentials config to the runtime
            // spec (previously dropped here, leaving `mcp.servers[].oauth` inert).
            oauth: self.oauth.as_ref().map(|o| super::McpOauthSpec {
                token_url: o.token_url.clone(),
                client_id: o.client_id.clone(),
                client_secret: o.client_secret.0.clone(),
                scope: o.scope.clone(),
            }),
            auth: self.auth.as_ref().map(|a| a.to_spec()),
            service: self.service.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpOauth {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Secret,
    #[serde(default)]
    pub scope: Option<String>,
}

/// A unified per-endpoint authentication provider (RFC 0031 §5). A flat,
/// `kind`-discriminated record: only the fields relevant to the chosen `kind`
/// are set; semantic validation (§14) enforces which are required. The provider
/// kinds land incrementally — `static`/`oauth2` first, then `aws`/`spiffe`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub kind: AuthKind,
    // --- oauth2 / oidc (RFC 0031 §7) ---
    /// Issuer base URL for `.well-known` metadata discovery (RFC 8414 / OIDC),
    /// used to fill the token / device-authorization endpoints when unset.
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub device_authorization_url: Option<String>,
    #[serde(default)]
    pub authorization_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    /// A confidential client's secret (`{{secret:…}}`); omit for a public client
    /// (the device grant needs no secret).
    #[serde(default)]
    pub client_secret: Option<Secret>,
    /// `device` (default, interactive), `authorization_code`, or
    /// `client_credentials` (headless M2M).
    #[serde(default)]
    pub grant: Option<OAuthGrant>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub audience: Option<String>,
    // --- static (RFC 0031 §6) ---
    /// A static bearer (`{{secret:…}}`) → `Authorization: Bearer …`.
    #[serde(default)]
    pub token: Option<Secret>,
    /// A static credential under an arbitrary header name (paired with `value`).
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub value: Option<Secret>,
    // --- aws (SigV4, RFC 0031 §8) ---
    #[serde(default)]
    pub region: Option<String>,
    /// The AWS service to sign for (e.g. `bedrock`, `execute-api`).
    #[serde(default)]
    pub service: Option<String>,
    /// The credential source: `env` / `static` / `sso` (IAM Identity Center
    /// interactive login → temporary credentials). (`imds`/`irsa` are follow-ups.)
    #[serde(default)]
    pub source: Option<String>,
    /// aws `source: sso` — the IAM Identity Center portal start URL, the account,
    /// and the permission-set role to assume (via `agentd login`).
    #[serde(default)]
    pub sso_start_url: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub role_name: Option<String>,
    // --- spiffe (workload identity, RFC 0031 §9) ---
    /// The SVID type: `jwt` (a rotating JWT-SVID bearer, the file-SVID MVP) or
    /// `x509` (mTLS — a follow-up).
    #[serde(default)]
    pub svid: Option<String>,
    /// Path to the SPIRE-written JWT-SVID token file (re-read per request, so a
    /// rotation is picked up).
    #[serde(default)]
    pub jwt_svid_file: Option<String>,
    /// Paths to the X.509-SVID cert + key (for `svid: x509`).
    #[serde(default)]
    pub svid_file: Option<String>,
    #[serde(default)]
    pub key_file: Option<String>,
}

impl Auth {
    /// Lower to the secret-free runtime [`AuthSpec`](super::AuthSpec) (spawn
    /// payload). Secrets stay as `{{secret:…}}` templates.
    pub fn to_spec(&self) -> super::AuthSpec {
        super::AuthSpec {
            kind: match self.kind {
                AuthKind::Static => "static",
                AuthKind::Oauth2 => "oauth2",
                AuthKind::Aws => "aws",
                AuthKind::Spiffe => "spiffe",
            }
            .to_string(),
            grant: self.grant.map(|g| {
                match g {
                    OAuthGrant::Device => "device",
                    OAuthGrant::AuthorizationCode => "authorization_code",
                    OAuthGrant::ClientCredentials => "client_credentials",
                }
                .to_string()
            }),
            issuer: self.issuer.clone(),
            token_url: self.token_url.clone(),
            device_authorization_url: self.device_authorization_url.clone(),
            authorization_url: self.authorization_url.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.as_ref().map(|s| s.0.clone()),
            scopes: self.scopes.clone(),
            audience: self.audience.clone(),
            token: self.token.as_ref().map(|s| s.0.clone()),
            header: self.header.clone(),
            value: self.value.as_ref().map(|s| s.0.clone()),
            region: self.region.clone(),
            service: self.service.clone(),
            source: self.source.clone(),
            sso_start_url: self.sso_start_url.clone(),
            account_id: self.account_id.clone(),
            role_name: self.role_name.clone(),
            svid: self.svid.clone(),
            jwt_svid_file: self.jwt_svid_file.clone(),
            svid_file: self.svid_file.clone(),
            key_file: self.key_file.clone(),
        }
    }
}

/// The authentication provider family (RFC 0031 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// A static bearer/header credential (today's behavior, made explicit).
    Static,
    /// OAuth 2.1 / OIDC — device grant, authorization-code, or client-credentials.
    Oauth2,
    /// AWS Signature Version 4 (RFC 0031 §8) — SigV4-signed requests.
    Aws,
    /// SPIFFE/SPIRE workload identity (RFC 0031 §9) — a JWT-SVID bearer (or
    /// X.509-SVID mTLS).
    Spiffe,
}

/// The OAuth 2.1 grant type (RFC 0031 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrant {
    /// RFC 8628 device authorization — the interactive default.
    Device,
    /// RFC 7636 authorization-code + PKCE (browser loopback).
    AuthorizationCode,
    /// The headless machine-to-machine grant.
    ClientCredentials,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Tools {
    pub disabled: Vec<String>,
    pub overrides: BTreeMap<String, ToolOverride>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolOverride {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Store {
    pub kind: StoreKind,
    pub prefix: Option<String>,
    pub mcp: Option<StoreMcp>,
    pub http: Option<StoreHttp>,
    pub file: Option<StoreFile>,
    pub checkpoint: Checkpoint,
    pub durability: Durability,
    pub retention: Retention,
    pub on_error: StoreOnError,
    pub audit: bool,
    pub timeout: Option<Dur>,
}

impl Store {
    pub fn prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("agentd")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Mcp,
    Http,
    /// The local filesystem (RFC 0033): one file per key under a root
    /// directory, single-writer, durable to whatever the filesystem is.
    File,
    Memory,
    #[default]
    None,
}

/// `store.file` (RFC 0033 §4). The only setting is where the state lives; the
/// adapter needs nothing else, so the block itself is optional — `kind: file`
/// with no block resolves the root from the environment ([`file_store_root`]).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoreFile {
    #[serde(default)]
    pub path: Option<String>,
    /// Shed new work when the store's filesystem has less than this free
    /// (`256MB`, `1.5GiB`, plain bytes; `"0"` disables). Warn at twice it.
    /// Default 256MB: a checkpoint failure at ENOSPC HALTS the daemon, so with
    /// under a quarter-gig free, refusing new runs while draining the current
    /// ones is almost certainly what the operator would have chosen — and the
    /// alternative was choosing nothing and dying mid-write.
    #[serde(default)]
    pub min_free: Option<String>,
}

/// One declared event stream (RFC 0035).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StreamCfg {
    pub retention: StreamRetention,
}

/// Retention: whichever bound trims first. Neither set = the 10k default —
/// an unbounded stream on a disk the pressure system guards would be a
/// self-inflicted shed.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct StreamRetention {
    pub max_events: Option<u64>,
    pub max_age: Option<Dur>,
}

impl StreamCfg {
    pub fn max_events(&self) -> u64 {
        self.retention.max_events.unwrap_or(10_000)
    }
    pub fn max_age_ms(&self) -> Option<u64> {
        self.retention.max_age.map(|d| d.0.as_millis() as u64)
    }
}

/// The `file` store's root directory (RFC 0033 §4), first that applies:
/// `store.file.path`, `$AGENTD_STATE_DIR`, `$XDG_STATE_HOME/agentd/state`,
/// `$HOME/.local/state/agentd/state`, else the OS temp dir.
///
/// This is deliberately the same chain — and the same order — that
/// [`crate::auth::cache::default_dir`] uses for the credential cache, one
/// sibling over (`state` beside `creds`): an operator who has learned where
/// agentd keeps its tokens already knows where it keeps its state, and one
/// `XDG_STATE_HOME` moves both. Resolution lives here, next to the schema, so
/// the startup log, `--capabilities` and [`crate::store::open`] all name the
/// one directory instead of each re-deriving it.
///
/// The last resort is the OS temp dir: a store that is *there* survives a
/// process restart but not a reboot, which is why the runtime logs the
/// resolved path and whether it was defaulted (RFC 0033 §5.1) rather than
/// letting a user believe more than the filesystem delivers.
pub fn file_store_root(store: &Store) -> std::path::PathBuf {
    file_store_root_in(store, &|k| std::env::var_os(k))
}

/// [`file_store_root`] with the environment injected. The chain is the part
/// worth testing and the process env is shared by every test in this binary,
/// so the lookup is a parameter — the same shape `unresolved_secret_ref` uses.
fn file_store_root_in(
    store: &Store,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(p) = store.file.as_ref().and_then(|f| f.path.as_deref()) {
        return PathBuf::from(p);
    }
    if let Some(d) = env("AGENTD_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = env("XDG_STATE_HOME") {
        return PathBuf::from(d).join("agentd").join("state");
    }
    if let Some(h) = env("HOME") {
        return PathBuf::from(h)
            .join(".local")
            .join("state")
            .join("agentd")
            .join("state");
    }
    std::env::temp_dir().join("agentd").join("state")
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoreMcp {
    pub server: String,
    #[serde(default)]
    pub put: Option<StoreOp>,
    #[serde(default)]
    pub get: Option<StoreOp>,
    #[serde(default)]
    pub list: Option<StoreOp>,
    #[serde(default)]
    pub delete: Option<StoreOp>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoreOp {
    pub tool: String,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub ok: Option<String>,
    #[serde(default)]
    pub conflict: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub keys: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StoreHttp {
    pub base_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub get: Option<HttpOp>,
    #[serde(default)]
    pub put: Option<HttpOp>,
    #[serde(default)]
    pub list: Option<HttpOp>,
    #[serde(default)]
    pub delete: Option<HttpOp>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpOp {
    #[serde(default)]
    pub method: Option<String>,
    pub url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub keys: Option<String>,
    #[serde(default)]
    pub conflict_status: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Checkpoint {
    pub debounce_ms: Option<u64>,
}

/// What to keep once a run is over.
///
/// A long-lived instance accumulates one durable record per run forever. On a
/// laptop that is the difference between an agent that runs for a month and one
/// that fills a disk — and the store had no eviction at all, so "forever" was
/// literal.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    pub runs: RunRetention,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RunRetention {
    /// Keep at most this many terminal runs (newest first).
    pub keep_last: Option<u32>,
    /// Drop a terminal run older than this.
    pub ttl: Option<Dur>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Durability {
    pub a2a: Option<DurabilityLevel>,
    pub steps: Option<DurabilityLevel>,
    /// The default durability CLASS for work (runs + subagent records):
    /// `durable` (the default — everything checkpoints and survives a
    /// restart) or `ephemeral` (nothing persists unless a workflow says
    /// `durable: true` / a spawn passes `durable: true` — the fast path for
    /// deployments that treat work as recomputable). The inbox, tasks,
    /// memory and credentials stay durable regardless.
    pub work: Option<WorkDurability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkDurability {
    Durable,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurabilityLevel {
    Strict,
    Eventual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StoreOnError {
    #[default]
    Halt,
    Degrade,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Memory {
    pub max_value_bytes: Option<u64>,
    pub list_default_limit: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Context {
    pub compact_at: Option<f64>,
    pub keep_last: Option<u32>,
    /// The model's context window in tokens (overrides the value inferred
    /// from `intelligence.model`) — the base of the compaction threshold.
    pub model_window: Option<u64>,
    pub plan: Plan,
    /// Which environment cards the system prompt carries (`workflows`,
    /// `skills`, `memory`, `services`, `streams`, `signals`, `peers`,
    /// `templates`). Unset = all. A node overrides per step with
    /// `context: {cards: [...]}`.
    pub cards: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Plan {
    pub max_items: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Knowledge {
    pub server: Option<String>,
    pub auto_context: AutoContext,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct AutoContext {
    pub on: AutoContextOn,
    pub top_k: Option<u32>,
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoContextOn {
    Turn,
    #[default]
    Never,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Search {
    pub server: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Skills {
    pub sources: Vec<SkillSource>,
    pub reference_prefix: Option<String>,
    pub max_loaded: Option<u32>,
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SkillSource {
    pub server: String,
    #[serde(default)]
    pub discover: Discover,
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Discover {
    Prompts,
    Resources,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_runs: Option<u32>,
    pub run: RunLimits,
    pub subagents: SubagentLimits,
    pub inline_max_bytes: Option<u64>,
    pub step_timeout: Option<Dur>,
    pub workflow: WorkflowLimits,
}

/// Ceilings a workflow definition is checked against at load time.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WorkflowLimits {
    /// The most concurrent lanes a `foreach`/`batch` body may use. A definition
    /// asking for more is REFUSED at load rather than quietly clamped: silent
    /// clamping is how a workflow ends up running eight-wide while its author
    /// believes it runs fifty, and the whole point of the field whitelist is
    /// that a knob either does what it says or fails loudly.
    pub fan_out: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct RunLimits {
    pub steps: Option<u32>,
    pub tokens: Option<u64>,
    pub deadline: Option<Dur>,
}

impl RunLimits {
    pub fn steps(&self) -> u32 {
        self.steps.unwrap_or(500)
    }
    pub fn tokens(&self) -> u64 {
        self.tokens.unwrap_or(2_000_000)
    }
    pub fn deadline(&self) -> Duration {
        self.deadline
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(3600))
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SubagentLimits {
    pub depth: Option<u32>,
    pub breadth: Option<u32>,
    pub total: Option<u32>,
    pub rate: Option<String>,
    /// Instance-tier children (RFC 0036 §4): heavier than flat workers, so
    /// they carry their own caps. Defaults 2 live / 8 lifetime / `4/1h`.
    pub instances: InstanceLimits,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct InstanceLimits {
    pub breadth: Option<u32>,
    pub total: Option<u32>,
    pub rate: Option<String>,
}

/// The `subagents:` section (RFC 0036 §4).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Subagents {
    /// `false` makes templates the ONLY spawn path — every child the model can
    /// create is a definition the operator reviewed. Default `true` (freeform
    /// flat-tier spawns keep working; there is no freeform INSTANCE spawn
    /// regardless).
    pub allow_freeform: Option<bool>,
    /// Applied to every spawn (flat and templated) unless overridden at the
    /// template or call site.
    pub defaults: SubagentDefaults,
    /// Named, operator-authored definitions. A template whose `instruction`
    /// carries no config-defining directives spawns the flat RFC 0009 worker;
    /// one that defines machinery (`:::workflow`/`:::mcp`/`:::stream`/
    /// `:::config`/`:::tools`) spawns an instance-tier child.
    pub templates: BTreeMap<String, SubagentTemplate>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SubagentDefaults {
    pub model: Option<String>,
    pub priority: Option<String>,
    pub mode: Option<String>,
    /// Default durability class for spawns (see `SubagentTemplate::durable`).
    pub durable: Option<bool>,
    /// The per-spawn limits object the `subagent` step takes (`max_tokens`,
    /// `deadline`, `memory`, `cpu`, `max_steps`) — kept raw; the spawn path
    /// parses it exactly like call-site limits.
    pub limits: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SubagentTemplate {
    /// A full RFC 0034 instruction document. Directive extraction runs ONCE,
    /// at boot, on this operator-authored text; `params` fold in at spawn as
    /// data and are never re-parsed for directives (RFC 0036 §8.2).
    pub instruction: String,
    /// The ONLY holes the model may fill, schema-validated at spawn.
    #[serde(default)]
    pub params: BTreeMap<String, ParamSpec>,
    /// Flat tier only: narrowing grants from the parent's server/tool set.
    #[serde(default)]
    pub servers: Option<Vec<String>>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Flat tier: the full per-spawn limits object. Instance tier: OS caps
    /// only (`memory`, `cpu`) — token ceilings live in `budget`.
    #[serde(default)]
    pub limits: Option<Value>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub skills: Option<Value>,
    #[serde(default)]
    pub context: Option<Value>,
    #[serde(default)]
    pub output_contract: Option<String>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// Instance tier only: the child's own budget (the RFC 0030 `Budget`
    /// shape), enforced in-child with `on_exhausted: refuse`.
    #[serde(default)]
    pub budget: Option<Value>,
    /// Instance tier only: graceful retirement triggers (RFC 0034 §7 path).
    #[serde(default)]
    pub ttl: Option<Dur>,
    /// A signal name (templated over params) whose delivery IN THE CHILD
    /// retires it.
    #[serde(default)]
    pub until: Option<String>,
    /// One live child at a time; its A2A peer alias is the template name.
    #[serde(default)]
    pub singleton: bool,
    /// Durability class: `false` ⇒ the spawn's record is memory-only (and an
    /// instance child runs on a memory store — no restore-respawn). Absent ⇒
    /// the deployment default (`store.durability.work`).
    #[serde(default)]
    pub durable: Option<bool>,
}

/// One declared template parameter (RFC 0036 §5).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    /// `string` (default) | `number` | `integer` | `boolean`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(rename = "enum", default)]
    pub one_of: Option<Vec<Value>>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Lifecycle {
    pub run_until: RunUntil,
    pub idle_grace: Option<Dur>,
    pub drain_timeout: Option<Dur>,
    pub run_id: Option<String>,
    pub exit_code_map: BTreeMap<String, i32>,
    pub watch_config: bool,
    /// RFC 0036 §6: delivery of this signal begins graceful shutdown — the
    /// retirement trigger a parent composes into an instance-tier child
    /// (`until:` on the template), and available to any daemon that should
    /// drain when a named signal arrives.
    pub until_signal: Option<String>,
}

impl Lifecycle {
    pub fn drain_timeout(&self) -> Duration {
        self.drain_timeout
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(25))
    }
    pub fn idle_grace(&self) -> Duration {
        self.idle_grace
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(5))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunUntil {
    #[default]
    Auto,
    Idle,
    Drained,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct A2a {
    pub listen: Option<String>,
    pub tls: A2aTls,
    pub bearer: Option<Secret>,
    pub principals: Vec<Principal>,
    pub peers: Vec<A2aPeer>,
    pub conversation_ttl: Option<Dur>,
    pub push: A2aPush,
}

/// **Push notifications**: a caller registers a webhook and agentd POSTs its
/// task's updates there instead of holding a stream open.
///
/// Default-OFF, because the URL comes from the caller: every delivery is an
/// outbound request to an address a *peer* chose, which is the shape of an SSRF.
/// Enabling it says you are willing to make that request; `allow_private` says
/// you are willing to make it to a private or loopback address, which is a
/// separate and larger decision (a peer could otherwise reach agentd's own
/// surfaces, or a cloud metadata endpoint).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct A2aPush {
    /// Accept `CreateTaskPushNotificationConfig` and deliver on transitions.
    pub enabled: bool,
    /// Permit webhook targets on private / loopback addresses.
    pub allow_private: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct A2aTls {
    pub cert: Option<String>,
    pub key: Option<String>,
    pub client_ca: Option<String>,
}

/// The **display-client interface** (RFC 0032): the opt-in surface a thin
/// TUI/web-UI client rides — the global `SubscribeToEvents` feed and the
/// `interface.*`/debug read ops, served on the existing A2A listener (no new
/// socket). Default-OFF: with `enabled: false` those methods answer
/// UNSUPPORTED_OPERATION and the core A2A surface is byte-identical. `debug`
/// additionally exposes internals (conversation transcripts, per-step run
/// detail, the live log ring, audit records on the feed) — operator-grade
/// information; leave it off in production unless you need it. `origins` lets a
/// hosted web UI (a non-loopback browser origin) through the DNS-rebind guard
/// with CORS; loopback origins are always accepted.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Interface {
    /// Serve the interface methods (`SubscribeToEvents`, `interface.info`, …).
    pub enabled: bool,
    /// Expose extra debug information (transcripts, run step detail, the log
    /// ring, audit feed events). Clients render their debug panes only when
    /// this is on. Runtime-togglable over the wire via `config.set` (operator).
    pub debug: bool,
    /// Extra allowed browser origins (`scheme://host[:port]`, exact match) for
    /// a hosted web UI. Loopback origins never need listing.
    pub origins: Vec<String>,
    /// What the display clients render in their chrome (RFC 0032 §12) — the
    /// daemon decides; every attached client renders the same layout.
    pub display: Display,
    /// Pairing-code login (RFC 0032 §13): a rotating short code shown to the
    /// operator that a client exchanges for a session token — the low-friction
    /// alternative to copying a bearer.
    pub pairing: Pairing,
}

/// The client-chrome layout (RFC 0032 §12): ordered item lists for the top
/// (header) and bottom (status bar) edges. `None` ⇒ the built-in default;
/// unknown items are skipped by clients (forward compatibility). The item
/// vocabulary lives in [`DISPLAY_ITEMS`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Display {
    pub top: Option<Vec<String>>,
    pub bottom: Option<Vec<String>>,
}

/// The display items a client knows how to render (RFC 0032 §12).
pub const DISPLAY_ITEMS: &[&str] = &[
    "name",     // agent name (card)
    "version",  // agentd version
    "instance", // instance identity
    "model",    // intelligence.model
    "endpoint", // the endpoint the client dialed
    "conn",     // connection state (live/polling/error)
    "debug",    // the debug badge
    "draining", // the DRAINING notice
    "active",   // active task count
    "turns",    // counter
    "tokens",   // tokens in/out
    "tool_calls",
    "runs",          // run count
    "subagents",     // subagent count
    "conversations", // conversation count
    "screen",        // current screen name (tui)
    "keys",          // key hints (tui)
    "clock",         // local time
];

/// Pairing-code login (RFC 0032 §13). The code is a 6-digit value derived
/// from a per-process random seed and the current 60-second window — shown
/// only to operators (`pairing.code`), verified with the previous window's
/// grace, rate-limited, and exchanged (`Pair`) for a high-entropy session
/// token that lives in memory until `ttl` (or restart).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Pairing {
    pub enabled: bool,
    /// The role a paired session gets: `operator` (default — whoever can read
    /// the code can already see the operator console) or `user`.
    pub role: Option<Role>,
    /// Session-token lifetime (default 12h).
    pub ttl: Option<Dur>,
}

/// The webhook inbound HTTP surface (RFC 0027): a dedicated listener serving the
/// `webhook` start nodes and `wait: {on: webhook}` callbacks. Auth is **per
/// node** (each `webhook` declares its own verification); a listener-wide default
/// may be set here and is used by nodes that declare no `auth`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Webhooks {
    /// `https://host:port` (loopback `http://` for dev). Required when any
    /// `webhook` start node or `wait: {on: webhook}` is used.
    pub listen: Option<String>,
    pub tls: A2aTls,
    /// A default auth applied to `webhook` nodes that declare none.
    pub default_auth: Option<WebhookAuth>,
}

/// A webhook's inbound authentication. Best practice (and the default guidance)
/// is HMAC over the raw body; a required-header or bearer match are alternatives;
/// `none: true` is an explicit loopback-only dev opt-out.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WebhookAuth {
    /// HMAC signature verification over the raw request body (GitHub/Stripe-style).
    pub hmac: Option<Hmac>,
    /// A shared bearer token (`Authorization: Bearer …`), constant-time matched.
    pub bearer: Option<Secret>,
    /// A required header exact-match (`{name, equals}`).
    pub header: Option<HeaderMatch>,
    /// Loopback-only, no auth (dev). Explicit opt-in.
    pub none: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Hmac {
    pub secret: Option<Secret>,
    /// The header carrying the signature (default `X-Signature`).
    pub header: Option<String>,
    /// Digest algorithm — `sha256` (default; the only supported algorithm).
    pub algo: Option<String>,
    /// A prefix stripped before the constant-time hex compare (e.g. `sha256=`).
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct HeaderMatch {
    pub name: Option<String>,
    pub equals: Option<Secret>,
}

/// The self-correcting goal watchdog (RFC 0026). A supervisor-level periodic
/// check of whether the configured `statement` is achieved (or the agent is
/// stuck), with a configurable disposition. It never blocks the agent loop.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Goal {
    /// The goal in natural language (the LLM judge reads it).
    pub statement: Option<String>,
    pub check: GoalCheck,
    /// N consecutive no-progress checks ⇒ self-correct (default 3).
    pub stuck_after: Option<u32>,
    /// What to do when the goal is achieved (default: `finish`).
    pub on_achieved: Option<GoalAction>,
    /// What to do when stuck (default: `replan`).
    pub on_stuck: Option<GoalAction>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct GoalCheck {
    /// The check cadence (default `5m`).
    pub every: Option<Dur>,
    /// An optional cheap CEL predicate over durable state, evaluated first.
    pub condition: Option<String>,
    /// `both` (default: CEL then LLM), `condition` (CEL only), or `agent` (LLM only).
    pub via: Option<String>,
}

/// A goal disposition. Deserialized from a bare string
/// (`finish`/`idle`/`replan`/`escalate`) or `{ workflow: <name> }`.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalAction {
    Finish,
    Idle,
    Replan,
    Escalate,
    Workflow(String),
}

impl<'de> Deserialize<'de> for GoalAction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match Value::deserialize(d)? {
            Value::String(s) => match s.as_str() {
                "finish" => Ok(GoalAction::Finish),
                "idle" => Ok(GoalAction::Idle),
                "replan" => Ok(GoalAction::Replan),
                "escalate" => Ok(GoalAction::Escalate),
                other => Err(D::Error::custom(format!(
                    "unknown goal action '{other}' (want finish|idle|replan|escalate|{{workflow: <name>}})"
                ))),
            },
            Value::Object(m) => match m.get("workflow").and_then(Value::as_str) {
                Some(w) => Ok(GoalAction::Workflow(w.to_string())),
                None => Err(D::Error::custom(
                    "a goal action object must be { workflow: <name> }",
                )),
            },
            _ => Err(D::Error::custom(
                "a goal action must be a string or { workflow: <name> }",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    #[serde(rename = "match")]
    pub matcher: PrincipalMatch,
    pub role: Role,
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default)]
    pub quotas: Option<Quotas>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct PrincipalMatch {
    pub san: Option<String>,
    pub sub: Option<String>,
    pub bearer_ref: Option<String>,
    pub aauth_agent: Option<String>,
    pub any: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Operator,
    User,
    Agent,
    Anonymous,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Quotas {
    pub rate: Option<String>,
    pub budget: Option<Budget>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct A2aPeer {
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub client_cert: Option<String>,
    #[serde(default)]
    pub client_key: Option<String>,
    /// A unified credential provider (RFC 0031 §5) for the peer — `static` /
    /// `oauth2` (device-login) / `spiffe` (jwt). Resolved to a bearer at dial
    /// time. (`aws` SigV4 for A2A is a follow-up — it needs per-request signing.)
    #[serde(default)]
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Observability {
    pub log_level: Option<String>,
    pub log_content: bool,
    pub otel: Otel,
    pub metrics_addr: Option<String>,
    pub health_file: Option<String>,
    pub report_file: Option<String>,
    pub events_ring: Option<u32>,
    pub audit: Audit,
    pub traceparent: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Otel {
    pub endpoint: Option<String>,
    pub traces: Option<bool>,
    pub metrics: Option<bool>,
    pub logs: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Audit {
    pub sink: Option<Vec<AuditSink>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditSink {
    Log,
    Store,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Security {
    pub allow_trifecta: bool,
    pub tls_ca: Option<String>,
    pub aauth: Option<AAuth>,
    pub cgroup: Cgroup,
    pub exec: Exec,
    pub workflows: WorkflowSecurity,
    /// RFC 0037 §5: `closed` ⇒ an outbound MCP dial whose URL matches no
    /// `services:` catalog entry is refused (boot-time for configured servers,
    /// dial-time for everything else). Default `open`.
    pub egress: Egress,
}

/// The egress policy (RFC 0037 §5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Egress {
    #[default]
    Open,
    Closed,
}

/// Whether the agent may rewrite its own workflows.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WorkflowSecurity {
    /// Refuse `workflow.create` / `.update` / `.delete` at runtime.
    ///
    /// Workflows are the agent's *standing instructions* — what it does when a
    /// schedule fires or a webhook lands, unattended. An agent that can rewrite
    /// them can quietly change what happens next time, and the change survives
    /// the conversation that caused it. Anywhere a definition is reviewed
    /// before it ships — a file in git, a config a deploy applies — self-update
    /// is not a feature, it is a hole in that review.
    ///
    /// Off by default, because the runtime-created workflow is a real workflow
    /// (`docs/workflows.md`); turn it on and definitions become read-only, from
    /// the config and the store, for everyone: the model, a subagent, and an
    /// operator over A2A alike. Loading is unaffected — the daemon still reads
    /// files, URLs and directories at startup.
    pub immutable: bool,
}

/// The local command-runner controls (RFC 0028 §exec). agentd's default posture
/// is **no local execution** (RFC 0012); this is off unless an operator both
/// builds with `--features exec` AND sets `enabled: true` — and even then runs
/// only allow-listed commands, in a confined directory, with a minimal env. The
/// `exec` tool is otherwise **mapping-only** (delegate off-box via
/// `tools.overrides`). It carries the `sensitive` + `egress` trifecta tags.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Exec {
    /// Enable a LOCAL runner. Requires the `exec` build feature too; default OFF.
    pub enabled: bool,
    /// Allow-listed command names (`argv[0]`); anything else is refused. Empty =
    /// deny all (so `enabled` alone runs nothing).
    pub allow: Vec<String>,
    /// The directory commands run in; a requested `cwd` must resolve inside it.
    pub workdir: Option<String>,
    /// Max wall-clock per command (a longer requested `timeout` is clamped). 30s.
    pub timeout: Option<Dur>,
    /// Cap on captured stdout+stderr bytes (default 1 MiB).
    pub max_output: Option<u64>,
    /// Environment variable NAMES passed through to the child (default none — a
    /// minimal env; the agent's own env/secrets are never inherited).
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AAuth {
    pub provider: String,
    #[serde(default)]
    pub key_file: Option<String>,
    #[serde(default)]
    pub enroll_token: Option<Secret>,
    #[serde(default)]
    pub enroll_assertion_file: Option<String>,
    #[serde(default)]
    pub person_server: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Cgroup {
    pub spec: Option<String>,
    pub memory_max: Option<String>,
    pub pids_max: Option<String>,
}

/// One `{{…}}` reference found by [`scan_references`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FoundRef {
    /// `secret` | `secret-file` | `config`.
    pub kind: &'static str,
    pub name: String,
    /// Where it sat (a dotted path into the document).
    pub at: String,
}

/// Collect every `{{secret:…}}`, `{{secret-file:…}}` and `{{config.…}}`
/// reference in `value`, with where each sits.
///
/// This exists so a deployment can be checked in ONE pass: the alternative —
/// failing on whichever reference happens to be evaluated first — turns
/// configuring a new instance into a guessing game played one restart at a
/// time, entering values one failure per attempt.
pub fn scan_references(value: &Value, at: &str, out: &mut Vec<FoundRef>) {
    match value {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(open) = rest.find("{{") {
                let after = &rest[open + 2..];
                let Some(close) = after.find("}}") else { break };
                let token = after[..close].trim();
                if let Some(n) = token.strip_prefix("secret:") {
                    out.push(FoundRef {
                        kind: "secret",
                        name: n.trim().into(),
                        at: at.into(),
                    });
                } else if let Some(p) = token.strip_prefix("secret-file:") {
                    out.push(FoundRef {
                        kind: "secret-file",
                        name: p.trim().into(),
                        at: at.into(),
                    });
                } else if let Some(c) = token.strip_prefix("config.") {
                    out.push(FoundRef {
                        kind: "config",
                        name: c.trim().into(),
                        at: at.into(),
                    });
                }
                rest = &after[close + 2..];
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                scan_references(v, &format!("{at}[{i}]"), out);
            }
        }
        Value::Object(o) => {
            for (k, v) in o {
                scan_references(v, &format!("{at}.{k}"), out);
            }
        }
        _ => {}
    }
}

/// The references in `value` that would NOT resolve right now — secrets against
/// the environment (and any interactively-entered values), secret-files against
/// the filesystem, `config.*` against `vars`. One message per missing
/// reference, deduplicated, every location listed.
pub fn missing_references(value: &Value, at: &str, vars: &BTreeMap<String, Value>) -> Vec<String> {
    let mut found = Vec::new();
    scan_references(value, at, &mut found);
    let mut by_ref: BTreeMap<(&'static str, String), Vec<String>> = BTreeMap::new();
    for r in found {
        let missing = match r.kind {
            "secret" => !crate::sec::secret::secret_available(&r.name),
            "secret-file" => std::fs::metadata(&r.name).is_err(),
            "config" => {
                let mut parts = r.name.split('.');
                let mut cur = parts.next().and_then(|p| vars.get(p));
                for p in parts {
                    cur = cur.and_then(|v| v.get(p));
                }
                cur.is_none()
            }
            _ => false,
        };
        if missing {
            by_ref.entry((r.kind, r.name)).or_default().push(r.at);
        }
    }
    by_ref
        .into_iter()
        .map(|((kind, name), ats)| {
            let what = match kind {
                "secret" => format!("{{{{secret:{name}}}}} is not set in the environment"),
                "secret-file" => format!("{{{{secret-file:{name}}}}} is not readable"),
                _ => format!("config.{name} is not defined in vars"),
            };
            format!("{what} (referenced at {})", ats.join(", "))
        })
        .collect()
}

/// Substitute `{{config.NAME}}` tokens in `value` from `vars`, appending every
/// unresolved reference to `errs` (with `at` naming where it sat).
///
/// A string that IS exactly one token takes the variable's typed value — a
/// number stays a number. A token embedded in a longer string is stringified
/// into place. There is no escape syntax: an unresolved reference is an error
/// rather than a literal, because a URL that still contains `{{config.region}}`
/// at runtime is a bug wherever it was headed.
pub fn substitute_config_vars(
    value: &mut Value,
    vars: &BTreeMap<String, Value>,
    at: &str,
    errs: &mut Vec<String>,
) {
    fn lookup<'a>(vars: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
        let mut parts = path.split('.');
        let mut cur = vars.get(parts.next()?)?;
        for p in parts {
            cur = cur.get(p)?;
        }
        Some(cur)
    }
    fn token_at(s: &str, from: usize) -> Option<(usize, usize, String)> {
        let start = s[from..].find("{{config.")? + from;
        let end = s[start..].find("}}")? + start + 2;
        let name = s[start + 9..end - 2].trim().to_string();
        Some((start, end, name))
    }
    match value {
        Value::String(s) => {
            // The whole string is one token: keep the value's TYPE.
            if let Some((0, end, name)) = token_at(s, 0)
                && end == s.len()
            {
                match lookup(vars, &name) {
                    Some(v) => *value = v.clone(),
                    None => errs.push(format!("{at}: config.{name} is not defined in vars")),
                }
                return;
            }
            let mut out = String::new();
            let mut pos = 0;
            while let Some((start, end, name)) = token_at(s, pos) {
                out.push_str(&s[pos..start]);
                match lookup(vars, &name) {
                    Some(Value::String(v)) => out.push_str(v),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {
                        errs.push(format!("{at}: config.{name} is not defined in vars"));
                        out.push_str(&s[start..end]);
                    }
                }
                pos = end;
            }
            if pos > 0 {
                out.push_str(&s[pos..]);
                *s = out;
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter_mut().enumerate() {
                substitute_config_vars(v, vars, &format!("{at}[{i}]"), errs);
            }
        }
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                substitute_config_vars(v, vars, &format!("{at}.{k}"), errs);
            }
        }
        _ => {}
    }
}

impl Settings {
    /// Type a settings document. `source` names it in errors.
    ///
    /// `{{config.*}}` substitution happens here, before typing — so a var can
    /// sit anywhere a string can: an endpoint, a path, a header. The
    /// `workflows` array is deliberately left alone; workflow documents are
    /// substituted at LOAD time instead (`load_workflows`), where the ones
    /// arriving from files, URLs and directories can be treated identically to
    /// inline ones.
    pub fn from_document(mut doc: Value, source: &str) -> Result<Settings, String> {
        let vars: BTreeMap<String, Value> = doc
            .get("vars")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let workflows = doc.as_object_mut().and_then(|o| o.remove("workflows"));
        let mut errs = Vec::new();
        substitute_config_vars(&mut doc, &vars, source, &mut errs);
        if let (Some(o), Some(w)) = (doc.as_object_mut(), workflows) {
            o.insert("workflows".into(), w);
        }
        if !errs.is_empty() {
            return Err(format!(
                "{} unresolved config var reference(s):\n  {}",
                errs.len(),
                errs.join("\n  ")
            ));
        }
        // Colon-fence directives in the instruction (operator-authored text —
        // this is the ONLY surface extraction runs on; conversation text is
        // never parsed). `:::workflow` bodies join `workflows:` exactly as
        // inline entries — same folding, validation, hashing, retirement —
        // and the model reads the CLEANED text, where each block became a
        // one-line note instead of machinery it might paraphrase. Extraction
        // runs BEFORE deserialization because the config-defining blocks
        // (`:::config`/`:::mcp`/`:::stream`/`:::tools`) contribute a fragment
        // that merges UNDER the explicit document — an instruction file alone
        // can define the whole agent, and an explicit key still wins.
        let mut extraction = None;
        if let Some(instr) = doc
            .get("agent")
            .and_then(|a| a.get("instruction"))
            .and_then(Value::as_str)
            .map(str::to_string)
            && !looks_like_resource_uri(&instr)
            && instr.lines().any(|l| l.starts_with(":::"))
        {
            match crate::config::directives::extract(&instr) {
                Ok(ex) => {
                    // (`{{config.*}}` inside a block already resolved: the doc
                    // passed substitute_config_vars with the instruction in it.
                    // A nameless block is refused by parse_workflow, the one
                    // authority on that message.)
                    if let Some(a) = doc.get_mut("agent").and_then(Value::as_object_mut) {
                        a.insert("instruction".into(), Value::String(ex.cleaned.clone()));
                    }
                    if let (Some(o), Value::Object(fragment)) =
                        (doc.as_object_mut(), ex.config.clone())
                    {
                        crate::config::directives::merge_missing(o, fragment, false);
                    }
                    extraction = Some(ex);
                }
                Err(errs) => {
                    return Err(format!(
                        "{source}: agent.instruction directives:
  {}",
                        errs.join(
                            "
  "
                        )
                    ));
                }
            }
        }
        let mut settings: Settings =
            serde_json::from_value(doc).map_err(|e| format!("{source} parse error: {e}"))?;
        if let Some(ex) = extraction {
            settings.agent.inline_skills = ex.skills;
            settings.workflows.extend(ex.workflows);
        }
        Ok(settings)
    }

    /// The `agent.name` fallback chain: config › downward-API instance ›
    /// hostname › `agentd`.
    pub fn instance_name(&self) -> String {
        if let Some(n) = &self.agent.name {
            return n.clone();
        }
        let id =
            crate::identity::Identity::from_env(self.lifecycle.run_id.as_deref().unwrap_or(""));
        if let Some(inst) = id.instance.filter(|i| !i.trim().is_empty()) {
            return inst;
        }
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "agentd".to_string())
    }

    /// Whether this instance OUTLIVES a single run — it serves A2A or webhooks,
    /// watches a goal, or owns a workflow with a long-lived start node
    /// (`loop`/`schedule`/`subscribe`/`signal`/`event`/`a2a`/`webhook`).
    ///
    /// This is the durability predicate (RFC 0025 §durability, RFC 0033 §5): a
    /// job-shaped run can lose its state and simply be re-run, an instance that
    /// keeps running cannot. Two callers need the same answer — [`load`], which
    /// defaults such an instance to the file store, and [`validate`], which
    /// refuses an EXPLICIT `store.kind: none` here — so the predicate lives in
    /// one place rather than being spelled twice and drifting.
    pub fn is_long_lived(&self) -> bool {
        self.a2a.listen.is_some()
            || self.webhooks.listen.is_some()
            || self.goal.is_some()
            || self.workflows.iter().any(workflow_is_long_lived)
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// v2-only top-level keys (RFC 0030 §2). `limits` exists in both schemas
/// (neutral); `intelligence` is a v1 STRING (the endpoint list) but a v2
/// OBJECT — decided by shape in [`detect`].
pub const V2_KEYS: &[&str] = &[
    "agent",
    "store",
    "workflows",
    "tools",
    "a2a",
    "lifecycle",
    "observability",
    "security",
    "knowledge",
    "search",
    "skills",
    "memory",
    "context",
    "vars",
    "streams",
];

/// v1 (flat) top-level keys.
pub const V1_KEYS: &[&str] = &[
    "intelligence_headers",
    "model_swap",
    "model",
    "max_tokens",
    "mcp_servers",
    "subscribe",
    "a2a_peers",
    "log_level",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// No document at all (no config files).
    Empty,
    /// The v1 flat schema.
    V1,
    /// The v2 nested schema.
    V2,
    /// Both key families present — refused.
    Mixed,
}

/// Decide which schema a merged document speaks.
pub fn detect(doc: &Value) -> Detected {
    let Some(obj) = doc.as_object() else {
        return Detected::Empty;
    };
    if obj.is_empty() {
        return Detected::Empty;
    }
    let version = obj.get("config_version").and_then(Value::as_str);
    let intel_is_object = obj.get("intelligence").is_some_and(Value::is_object);
    let intel_is_string = obj.get("intelligence").is_some_and(Value::is_string);
    let has_v2 = version == Some(schema::CONFIG_VERSION)
        || intel_is_object
        || obj.keys().any(|k| V2_KEYS.contains(&k.as_str()));
    let has_v1 = intel_is_string
        || obj.keys().any(|k| V1_KEYS.contains(&k.as_str()))
        || matches!(version, Some(v) if v != schema::CONFIG_VERSION);
    match (has_v1, has_v2) {
        (true, true) => Detected::Mixed,
        (false, true) => Detected::V2,
        (true, false) => Detected::V1,
        // Only `config_version` absent + neither family (e.g. `{}` with a
        // comment) or `intelligence`… every key was matched above; anything
        // else is a v1 document for the v1 loader to judge.
        (false, false) => Detected::V1,
    }
}

// ---------------------------------------------------------------------------
// Aliases (RFC 0030 §3 alias column + §7)
// ---------------------------------------------------------------------------

/// How a legacy flag maps onto the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    /// `--flag <value>` sets `path` (typed by the schema binding of `path`).
    Set,
    /// `--flag` (no value) sets `path` to `true`.
    SetTrue,
    /// `--flag <value>` appends a parsed element to the array at `path`.
    Append,
    /// `--flag <value>` reads the FILE at `<value>` and sets `path` to its text.
    SetFromFile,
    /// Handled by dedicated code (`--mcp-tags`, `--budget-exit-code`).
    Special,
}

/// A legacy flag → v2 path alias.
#[derive(Debug, Clone, Copy)]
pub struct Alias {
    pub flag: &'static str,
    pub path: &'static str,
    pub kind: AliasKind,
}

/// The alias table (RFC 0030 §3). Order irrelevant; flags apply in argument
/// order.
pub const ALIASES: &[Alias] = &[
    Alias {
        flag: "--instruction",
        path: "agent.instruction",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--instruction-file",
        path: "agent.instruction",
        kind: AliasKind::SetFromFile,
    },
    Alias {
        flag: "--prompt",
        path: "agent.prompt",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--prompt-file",
        path: "agent.prompt",
        kind: AliasKind::SetFromFile,
    },
    Alias {
        flag: "--intelligence",
        path: "intelligence.endpoints",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--intelligence-token",
        path: "intelligence.token",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--intelligence-token-file",
        path: "intelligence.token_file",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--model",
        path: "intelligence.model",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--model-swap",
        path: "intelligence.swap_policy",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--budget-tokens-lifetime",
        path: "intelligence.budget.lifetime_tokens",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--mcp",
        path: "mcp.servers",
        kind: AliasKind::Append,
    },
    Alias {
        flag: "--mcp-tags",
        path: "mcp.servers",
        kind: AliasKind::Special,
    },
    Alias {
        flag: "--a2a-peer",
        path: "a2a.peers",
        kind: AliasKind::Append,
    },
    Alias {
        flag: "--workflow",
        path: "workflows",
        kind: AliasKind::Append,
    },
    Alias {
        flag: "--max-steps",
        path: "limits.run.steps",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--max-tokens",
        path: "limits.run.tokens",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--deadline",
        path: "limits.run.deadline",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--max-depth",
        path: "limits.subagents.depth",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--run-id",
        path: "lifecycle.run_id",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--drain-timeout",
        path: "lifecycle.drain_timeout",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--watch-config",
        path: "lifecycle.watch_config",
        kind: AliasKind::SetTrue,
    },
    Alias {
        flag: "--budget-exit-code",
        path: "lifecycle.exit_code_map",
        kind: AliasKind::Special,
    },
    Alias {
        flag: "--listen",
        path: "a2a.listen",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--serve-mcp",
        path: "a2a.listen",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--serve-cert",
        path: "a2a.tls.cert",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--serve-key",
        path: "a2a.tls.key",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--serve-client-ca",
        path: "a2a.tls.client_ca",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--serve-bearer",
        path: "a2a.bearer",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--log-level",
        path: "observability.log_level",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--log-content",
        path: "observability.log_content",
        kind: AliasKind::SetTrue,
    },
    Alias {
        flag: "--metrics-addr",
        path: "observability.metrics_addr",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--health-file",
        path: "observability.health_file",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--report-file",
        path: "observability.report_file",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--events-ring",
        path: "observability.events_ring",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--traceparent",
        path: "observability.traceparent",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--allow-trifecta",
        path: "security.allow_trifecta",
        kind: AliasKind::SetTrue,
    },
    Alias {
        flag: "--tls-ca",
        path: "security.tls_ca",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--aauth-provider",
        path: "security.aauth.provider",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--aauth-key-file",
        path: "security.aauth.key_file",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--aauth-enroll-token",
        path: "security.aauth.enroll_token",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--aauth-enroll-assertion-file",
        path: "security.aauth.enroll_assertion_file",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--aauth-person-server",
        path: "security.aauth.person_server",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--cgroup",
        path: "security.cgroup.spec",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--cgroup-memory-max",
        path: "security.cgroup.memory_max",
        kind: AliasKind::Set,
    },
    Alias {
        flag: "--cgroup-pids-max",
        path: "security.cgroup.pids_max",
        kind: AliasKind::Set,
    },
];

/// Legacy env names → v2 paths (the derived `AGENTD_<PATH>` names are the
/// primary surface; these keep the quickstart and the 1.x k8s manifests
/// working). Branded (`AGENTD_`) and neutral (`AGENT_`) prefixes both apply.
pub const ENV_ALIASES: &[(&str, &str)] = &[
    ("INSTRUCTION", "agent.instruction"),
    ("PROMPT", "agent.prompt"),
    ("INTELLIGENCE", "intelligence.endpoints"),
    ("INTELLIGENCE_TOKEN", "intelligence.token"),
    ("INTELLIGENCE_TOKEN_FILE", "intelligence.token_file"),
    ("MODEL", "intelligence.model"),
    ("MODEL_SWAP", "intelligence.swap_policy"),
    ("BUDGET_TOKENS", "intelligence.budget.lifetime_tokens"),
    ("MAX_STEPS", "limits.run.steps"),
    ("MAX_TOKENS", "limits.run.tokens"),
    ("DEADLINE", "limits.run.deadline"),
    ("RUN_ID", "lifecycle.run_id"),
    ("DRAIN_TIMEOUT", "lifecycle.drain_timeout"),
    ("LOG_LEVEL", "observability.log_level"),
    ("LOG_CONTENT", "observability.log_content"),
    ("METRICS_ADDR", "observability.metrics_addr"),
    ("TRACEPARENT", "observability.traceparent"),
    ("SERVE_MCP", "a2a.listen"),
    ("SERVE_BEARER", "a2a.bearer"),
    ("TLS_CA", "security.tls_ca"),
    ("ALLOW_TRIFECTA", "security.allow_trifecta"),
    ("WATCH_CONFIG", "lifecycle.watch_config"),
];

/// Flags removed in 2.0 with the migration hint (RFC 0030 §7).
pub const REMOVED_FLAGS: &[(&str, &str)] = &[
    (
        "--mode",
        "modes are gone: give the workflow a start node (`once` | `loop` | `schedule` | `subscribe` | `signal` | `event` | `a2a` | `manual`) and set `lifecycle.run_until` if needed",
    ),
    (
        "--subscribe",
        "use a `subscribe` start node: `{kind: subscribe, server: <name>, uri: <uri>}`",
    ),
    (
        "--continue",
        "use a `subscribe` start node with `deliver: wait` (or a warm subagent)",
    ),
    (
        "--interval",
        "use a `loop` start node with `interval`, or a `schedule` start node with `every`",
    ),
    ("--cron", "use a `schedule` start node with `cron`"),
    // Clustering was removed, not migrated: agentd has no coordination protocol
    // of its own. A fleet partitions upstream — one subscription per replica, or
    // the queue's own lease semantics from a workflow step (docs/scaling.md).
    (
        "--shard",
        "agentd does not partition work; give each replica its own subscription (docs/scaling.md)",
    ),
    (
        "--claim",
        "call the queue's own claim/lease tools from a workflow step (docs/scaling.md §2c)",
    ),
    ("--claim-ttl", "it went with --claim"),
    ("--claim-renew-fraction", "it went with --claim"),
    (
        "--standby",
        "there is no standby pool; a worker replica is an ordinary instance with its own subscription",
    ),
    ("--assign-from", "it went with --standby"),
    (
        "--workflow-resume",
        "automatic: runs resume from the store on restart (`resume_policy` per workflow)",
    ),
    (
        "--workflow-resume-force",
        "set `resume_policy: force` on the workflow",
    ),
];

// ---------------------------------------------------------------------------
// Load pipeline
// ---------------------------------------------------------------------------

/// The result of a v2 load: the typed settings plus the documents they came
/// from (the merged FILE document is kept for secret-provenance validation and
/// for the reload diff).
#[derive(Debug, Clone)]
pub struct Loaded {
    pub settings: Settings,
    /// The effective document (files ← env ← flags), what `Settings` typed.
    pub doc: Value,
    /// The merged FILE layer alone (before env/flags).
    pub file_doc: Value,
    pub files: Vec<(String, Format)>,
    /// Every non-fatal advisory collected during load (surfaced by
    /// `--validate-config` and logged at startup).
    pub warnings: Vec<String>,
}

/// What the loader was asked to do besides loading (short-circuits the CLI
/// handles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Run,
    Help,
    Version,
    Schema,
    WorkflowSchema,
    Validate,
    Capabilities,
    /// `--login <target>` (RFC 0031 §12): complete the interactive OAuth device
    /// flow for a configured endpoint and cache the token.
    Login(String),
    /// `--logout <target>`: evict a cached credential.
    Logout(String),
}

/// Probe the invocation without side effects: which schema the config files
/// speak (`Detected`), so `main` can route to the v2 runtime.
pub fn probe(args: &[String], env: &[(String, String)]) -> Result<Detected, ConfigError> {
    let env = super::debrand_env(env);
    let envmap: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    // A flag/env `config_version: "2"` selects the 2.0 runtime for a flag-only
    // invocation (`agentd --config-version 2 --instruction …`).
    let flag_v2 = args
        .windows(2)
        .any(|w| matches!(w[0].as_str(), "--config-version" | "--config_version") && w[1] == "2")
        || args
            .iter()
            .any(|a| a == "--config-version=2" || a == "--config_version=2")
        || envmap
            .get("AGENTD_CONFIG_VERSION")
            .or_else(|| envmap.get("CONFIG_VERSION"))
            .is_some_and(|v| *v == "2");
    let paths = super::config_paths_from_map(args, &envmap).paths;
    if paths.is_empty() {
        return Ok(if flag_v2 {
            Detected::V2
        } else {
            Detected::Empty
        });
    }
    let (doc, _) = file::read_documents_checked(&paths, &|_, _| Ok(())).map_err(usage)?;
    let d = detect(&doc);
    Ok(match (d, flag_v2) {
        (Detected::Empty, true) => Detected::V2,
        (Detected::V1, true) => Detected::Mixed,
        (d, _) => d,
    })
}

/// Load, layer and validate a v2 document from `args` (excluding the program
/// name) and `env`. Returns `(Loaded, Ask)`; `Ask` tells the caller what the
/// invocation wants (`--help`, `--config-schema`, `--validate-config`, …).
/// Errors are `ConfigError::Usage` (exit 2), before any side effect.
pub fn load(args: &[String], env: &[(String, String)]) -> Result<(Loaded, Ask), ConfigError> {
    let env = super::debrand_env(env);
    let envmap: HashMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let schema = schema::schema();
    let bindings = paths::bindings_of(&schema);
    let mut warnings = Vec::new();

    // --- FILE layer: several files, later wins (JSON Merge Patch) ---
    let super::ConfigPaths {
        paths: config_paths,
        discovered,
    } = super::config_paths_from_map(args, &envmap);
    // Two discovered spellings at once: refuse rather than pick. Whichever one
    // agentd chose, somebody would be editing the other and wondering why
    // nothing changed. Only DISCOVERY is ambiguous this way — naming two files
    // that happen to be spelled `.agentd.yml` and `.agentd.yaml` (`--config a/.agentd.yml
    // --config b/.agentd.yaml`) states an order, so layering them is legal.
    if discovered && config_paths.len() > 1 {
        return Err(usage(format!(
            "both {} and {} are present; keep one (or name the file with --config)",
            super::DISCOVERED_CONFIG_NAMES[0],
            super::DISCOVERED_CONFIG_NAMES[1]
        )));
    }
    let (file_doc, files) = if config_paths.is_empty() {
        (Value::Object(Map::new()), Vec::new())
    } else {
        file::read_documents_checked(&config_paths, &|doc, source| {
            // A v1/mixed file is judged after the merge (a clear migration
            // message); a v2 file is typed here so an unknown key names ITS file.
            match detect(doc) {
                Detected::V2 | Detected::Empty => {
                    Settings::from_document(doc.clone(), source).map(|_| ())
                }
                _ => Ok(()),
            }
        })
        .map_err(usage)?
    };
    match detect(&file_doc) {
        Detected::Mixed => {
            return Err(usage(
                "config file mixes v1 keys (model/subscribe/mcp_servers/…) with v2 sections (agent/intelligence/…); \
                 migrate the v1 keys (docs/configuration.md §migration)"
                    .into(),
            ));
        }
        Detected::V1 => {
            return Err(usage(
                "config file speaks the v1 schema; the 2.0 loader needs `config_version: \"2\"` or v2 sections".into(),
            ));
        }
        _ => {}
    }
    // A DISCOVERED config governs an invocation that never named it: `cd` into a
    // repo you cloned, type `agentd --prompt …`, and that repo's `.agentd.yml`
    // decides where your credentials go. Convenience is worth that only while the
    // file cannot RELAX a security control, so an unnamed file setting one is
    // exit 2 with the file and the setting named. An explicit `--config` keeps
    // its full power — naming the file IS the deliberate act, and that is the
    // whole distinction being drawn here.
    if discovered {
        let file = config_paths.first().map_or("", String::as_str);
        if let Some((_, label)) = DISCOVERY_FORBIDDEN_RELAXATIONS
            .iter()
            .find(|(ptr, _)| file_doc.pointer(ptr).and_then(Value::as_bool) == Some(true))
        {
            return Err(usage(format!(
                "{file} was discovered, not named, and it sets {label}: a config found in the \
                 working directory may not relax a security control. Pass `--config {file}` if \
                 you meant to run under that file's grant."
            )));
        }
        // …and whatever else it wired that bears on security is named at startup
        // (option (c) of the containment): an adopted dotfile is never silent
        // about the endpoints, peers and powers it just chose for this process.
        let touched = discovered_security_settings(&file_doc);
        if !touched.is_empty() {
            warnings.push(format!(
                "adopted the discovered config {file} (no --config given); it sets {}",
                touched.join(", ")
            ));
        }
    }
    let mut doc = file_doc.clone();

    // --- ENV layer: derived path names, then legacy aliases (path names win) ---
    let mut env_doc = Value::Object(Map::new());
    for (name, path) in ENV_ALIASES {
        let candidates = [
            format!("AGENTD_{name}"),
            format!("AGENT_{name}"),
            (*name).to_string(),
        ];
        if let Some(raw) = candidates.iter().find_map(|k| envmap.get(k.as_str())) {
            let binding = binding_for(&bindings, path)
                .ok_or_else(|| usage(format!("internal: alias path {path} not in schema")))?;
            let v = binding
                .coerce(raw)
                .map_err(|e| usage(format!("invalid {}: {e}", candidates[0])))?;
            paths::set_path(&mut env_doc, path, v);
        }
    }
    let (derived, _applied) = paths::env_document_in(&bindings, &envmap).map_err(usage)?;
    file::merge_into(&mut env_doc, derived);
    file::merge_into(&mut doc, env_doc);

    // --- FLAG layer: aliases + generic path flags, in argument order ---
    let mut ask = Ask::Run;
    let mut mcp_tags: Vec<(String, Vec<String>)> = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        let a = arg.as_str();
        match a {
            "-h" | "--help" => ask = Ask::Help,
            "-V" | "--version" => ask = Ask::Version,
            "--config-schema" | "--config-schema=2" => ask = Ask::Schema,
            "--workflow-schema" => ask = Ask::WorkflowSchema,
            "--validate-config" => ask = Ask::Validate,
            "--capabilities" => ask = Ask::Capabilities,
            "--login" => {
                let t = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("--login requires a target (e.g. mcp:<name>)".into()))?;
                ask = Ask::Login(t);
            }
            "--logout" => {
                let t = it
                    .next()
                    .cloned()
                    .ok_or_else(|| usage("--logout requires a target (e.g. mcp:<name>)".into()))?;
                ask = Ask::Logout(t);
            }
            "--config" | "-c" => {
                it.next(); // consumed by the FILE layer
            }
            // `--config=a.yaml` / `-c=a.yaml`: the FILE layer already took it.
            _ if matches!(
                crate::config::config_flag(a),
                crate::config::ConfigFlag::Inline(_)
            ) => {}
            _ => {
                if let Some((flag, hint)) = REMOVED_FLAGS.iter().find(|(f, _)| *f == a) {
                    return Err(usage(format!("{flag} was removed in agentd 2.0: {hint}")));
                }
                if let Some(alias) = ALIASES.iter().find(|al| al.flag == a) {
                    apply_alias(&mut doc, &bindings, alias, &mut it, &mut mcp_tags)?;
                    continue;
                }
                match paths::resolve_flag_in(&bindings, a).map_err(usage)? {
                    Some(target) => {
                        let raw = if matches!(target.value_kind(), paths::Kind::Boolean)
                            && !it.peek().is_some_and(|n| !n.starts_with("--"))
                        {
                            "true".to_string()
                        } else {
                            it.next()
                                .cloned()
                                .ok_or_else(|| usage(format!("{a} requires a value")))?
                        };
                        let value = paths::coerce(target.value_kind(), &raw)
                            .map_err(|e| usage(format!("invalid {a}: {e}")))?;
                        file::merge_into(&mut doc, target.document(value));
                    }
                    None => return Err(usage(format!("unknown argument: {a}"))),
                }
            }
        }
    }
    // `--mcp-tags name=tags` after every `--mcp` is known.
    for (name, tags) in mcp_tags {
        let Some(servers) = doc
            .pointer_mut("/mcp/servers")
            .and_then(Value::as_array_mut)
        else {
            return Err(usage(format!(
                "--mcp-tags references unknown server '{name}'"
            )));
        };
        match servers
            .iter_mut()
            .find(|s| s.get("name").and_then(Value::as_str) == Some(name.as_str()))
        {
            Some(s) => {
                s["tags"] = json!({ "*": tags });
            }
            None => {
                return Err(usage(format!(
                    "--mcp-tags references unknown server '{name}'"
                )));
            }
        }
    }

    // --- sugar: `agentd --instruction X` with no workflows ---
    if ask == Ask::Run || ask == Ask::Validate {
        apply_instruction_sugar(&mut doc);
    }

    // --- env substitution: `${VAR}` / `${VAR:-default}` in any string value of
    //     the merged document (config + workflows), from the process env. Distinct
    //     from `{{secret:…}}` (which resolves a redacted credential). ---
    if let Err(e) = substitute_env(&mut doc, &envmap) {
        return Err(usage(e));
    }

    // --- type + validate ---
    let mut settings = Settings::from_document(doc.clone(), "config").map_err(usage)?;
    // --- RFC 0033 §5: durability a laptop already satisfies ---
    //
    // A long-lived instance that names no store used to exit 2 and ask for a
    // coordination backend before the operator had run anything. It now gets the
    // FILE adapter: durable to whatever filesystem it lands on, and the runtime
    // says so at startup (`store.file`) rather than implying more (§5.1).
    //
    // "Absent" is read off the effective DOCUMENT, not off `settings.store.kind`
    // — `StoreKind` derives `Default = None`, so the typed value cannot tell a
    // config that said nothing from one that said `none`. `doc` is the merged
    // file ← env ← flag layers, so `--store-kind none` / `AGENTD_STORE_KIND=none`
    // count as explicit exactly like the YAML key does. That distinction is the
    // whole point: an operator who WROTE `none` on a long-lived instance still
    // gets the diagnostic (validate, below), because silently overriding a
    // stated choice is worse than refusing to start.
    //
    // A one-shot instance is deliberately untouched and keeps `none`: a job that
    // suddenly began writing state to disk would surprise every existing user of
    // it, and re-running it is already the recovery story.
    if doc.pointer("/store/kind").is_none() && settings.is_long_lived() {
        settings.store.kind = StoreKind::File;
    }
    // RFC 0037: resolve `service:` references + apply the tag floor BEFORE
    // validation and the trifecta gate, so both see effective servers.
    let service_errors = resolve_services(&mut settings);
    let mut loaded = Loaded {
        settings,
        doc,
        file_doc,
        files,
        warnings: Vec::new(),
    };
    // `--prompt-missing`, before validation: the person is standing at a
    // terminal ready to supply what is missing, so ask FIRST and let the
    // validation that follows see the values — otherwise the aggregate error
    // below exits before a prompt could ever appear. Only in run mode: a
    // `--validate-config` must stay side-effect-free and report, not converse.
    if ask == Ask::Run && crate::config::prompt::prompt_missing_requested() {
        let mut found = Vec::new();
        scan_references(&loaded.doc, "config", &mut found);
        let mut names: Vec<String> = found
            .into_iter()
            .filter(|r| r.kind == "secret" && !crate::sec::secret::secret_available(&r.name))
            .map(|r| r.name)
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            match crate::config::prompt::read_secret_from_tty(&format!("{name} (secret)")) {
                Ok(v) => crate::sec::secret::set_prompted(&name, v),
                // A failed prompt (no terminal, EOF) falls through to the
                // normal aggregate refusal below, which names what is missing.
                Err(_) => break,
            }
        }
    }
    let mut diags = validate(&loaded);
    diags.errors.splice(0..0, service_errors);
    warnings.extend(diags.warnings);
    loaded.warnings = warnings;
    if ask != Ask::Validate
        && ask != Ask::Help
        && ask != Ask::Version
        && ask != Ask::Schema
        && ask != Ask::WorkflowSchema
        && !matches!(ask, Ask::Login(_) | Ask::Logout(_))
        && let Some(first) = diags.errors.first()
    {
        // ALL of them, not the first. Failing on whichever error happens to
        // sort first turns fixing a config into a loop of restart, read one
        // line, fix one thing — the aggregate report is the whole point of
        // validating everything up front.
        let msg = if diags.errors.len() == 1 {
            first.clone()
        } else {
            format!(
                "{} configuration errors:\n  - {}",
                diags.errors.len(),
                diags.errors.join("\n  - ")
            )
        };
        return Err(usage(msg));
    }
    if ask == Ask::Validate && !diags.errors.is_empty() {
        return Err(ConfigError::Validate(Err(diags
            .errors
            .iter()
            .map(|d| super::config_invalid_line(d))
            .collect::<Vec<_>>()
            .join("\n"))));
    }
    Ok((loaded, ask))
}

/// The security controls a config file can **relax** — the two booleans that
/// widen what this process may do (lift the lethal-trifecta refusal, RFC 0012
/// §3.2; turn on the local command runner, RFC 0028 §exec). A file the operator
/// NAMED may set them; a file merely discovered in the working directory may
/// not. Narrowing settings are deliberately absent: a dotfile that takes power
/// away needs no ceremony.
const DISCOVERY_FORBIDDEN_RELAXATIONS: [(&str, &str); 2] = [
    ("/security/allow_trifecta", "security.allow_trifecta"),
    ("/security/exec/enabled", "security.exec.enabled"),
];

/// The settings that decide where this agent's credentials go, who may reach
/// it, and what it may call. A DISCOVERED config that sets any of them has them
/// named in a startup `config.warning`, so adopting a dotfile is visible in the
/// log rather than inferred from behaviour. Pointer + the dotted label to print:
/// NAMES only, never values (a value may be a `{{secret:…}}` template — RFC 0012
/// §3.7).
const DISCOVERY_SECURITY_SETTINGS: [(&str, &str); 12] = [
    ("/intelligence/endpoints", "intelligence.endpoints"),
    ("/intelligence/token", "intelligence.token"),
    ("/intelligence/token_file", "intelligence.token_file"),
    ("/intelligence/headers", "intelligence.headers"),
    ("/intelligence/auth", "intelligence.auth"),
    ("/mcp/servers", "mcp.servers"),
    ("/tools/overrides", "tools.overrides"),
    ("/store", "store"),
    ("/a2a/listen", "a2a.listen"),
    ("/a2a/peers", "a2a.peers"),
    ("/webhooks/listen", "webhooks.listen"),
    ("/security", "security"),
];

/// Which of [`DISCOVERY_SECURITY_SETTINGS`] the file layer actually set, in
/// declaration order. `null` counts as unset (RFC 7396 unsets with `null`).
fn discovered_security_settings(file_doc: &Value) -> Vec<&'static str> {
    DISCOVERY_SECURITY_SETTINGS
        .iter()
        .filter(|(ptr, _)| file_doc.pointer(ptr).is_some_and(|v| !v.is_null()))
        .map(|(_, label)| *label)
        .collect()
}

fn binding_for<'a>(bindings: &'a [Binding], path: &str) -> Option<&'a Binding> {
    bindings.iter().find(|b| b.path == path)
}

fn apply_alias(
    doc: &mut Value,
    bindings: &[Binding],
    alias: &Alias,
    it: &mut std::iter::Peekable<std::slice::Iter<'_, String>>,
    mcp_tags: &mut Vec<(String, Vec<String>)>,
) -> Result<(), ConfigError> {
    let mut take = || -> Result<String, ConfigError> {
        it.next()
            .cloned()
            .ok_or_else(|| usage(format!("{} requires a value", alias.flag)))
    };
    match alias.kind {
        AliasKind::Set => {
            let raw = take()?;
            let b = binding_for(bindings, alias.path).ok_or_else(|| {
                usage(format!("internal: alias path {} not in schema", alias.path))
            })?;
            let v = b
                .coerce(&raw)
                .map_err(|e| usage(format!("invalid {}: {e}", alias.flag)))?;
            let mut patch = Value::Object(Map::new());
            paths::set_path(&mut patch, alias.path, v);
            file::merge_into(doc, patch);
        }
        AliasKind::SetTrue => {
            let mut patch = Value::Object(Map::new());
            paths::set_path(&mut patch, alias.path, Value::Bool(true));
            file::merge_into(doc, patch);
        }
        AliasKind::SetFromFile => {
            let path = take()?;
            let text = super::read_file(&path)?;
            let mut patch = Value::Object(Map::new());
            paths::set_path(&mut patch, alias.path, Value::String(text));
            file::merge_into(doc, patch);
        }
        AliasKind::Append => {
            let raw = take()?;
            let element = match alias.flag {
                "--mcp" => {
                    let (name, endpoint) = raw
                        .split_once('=')
                        .ok_or_else(|| usage(format!("--mcp: want name=endpoint (got: {raw})")))?;
                    json!({ "name": name.trim(), "endpoint": endpoint.trim() })
                }
                "--a2a-peer" => {
                    let (name, endpoint) = raw.split_once('=').ok_or_else(|| {
                        usage(format!("--a2a-peer: want name=endpoint (got: {raw})"))
                    })?;
                    json!({ "name": name.trim(), "endpoint": endpoint.trim() })
                }
                "--workflow" => {
                    let name = std::path::Path::new(&raw)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("workflow")
                        .to_string();
                    json!({ "name": name, "file": raw })
                }
                other => return Err(usage(format!("internal: no append rule for {other}"))),
            };
            append_at(doc, alias.path, element);
        }
        AliasKind::Special => match alias.flag {
            "--mcp-tags" => {
                let raw = take()?;
                let (name, tags) = raw
                    .split_once('=')
                    .ok_or_else(|| usage(format!("--mcp-tags: want name=tag,tag (got: {raw})")))?;
                mcp_tags.push((
                    name.trim().to_string(),
                    tags.split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .collect(),
                ));
            }
            "--budget-exit-code" => {
                let raw = take()?;
                let n: i64 = raw
                    .trim()
                    .parse()
                    .ok()
                    .filter(|n| (0..=255).contains(n))
                    .ok_or_else(|| {
                        usage(format!("invalid --budget-exit-code: {raw} (want 0..=255)"))
                    })?;
                let mut patch = Value::Object(Map::new());
                paths::set_path(
                    &mut patch,
                    "lifecycle.exit_code_map",
                    json!({ "3": n, "7": n }),
                );
                file::merge_into(doc, patch);
            }
            other => return Err(usage(format!("internal: no special rule for {other}"))),
        },
    }
    Ok(())
}

/// Push `element` onto the array at dotted `path` (creating it).
fn append_at(doc: &mut Value, path: &str, element: Value) {
    let pointer = format!("/{}", path.replace('.', "/"));
    if doc.pointer(&pointer).is_none() {
        let mut patch = Value::Object(Map::new());
        paths::set_path(&mut patch, path, Value::Array(Vec::new()));
        file::merge_into(doc, patch);
    }
    if let Some(arr) = doc.pointer_mut(&pointer) {
        if !arr.is_array() {
            *arr = Value::Array(Vec::new());
        }
        arr.as_array_mut().expect("array").push(element);
    }
}

/// `agentd --instruction X` (or `agent.instruction` alone) with no workflows ⇒
/// the one-node workflow `once → agent → finish` (RFC 0030 §7).
///
/// A `--prompt` deliberately does NOT come here: a prompt is a **message to
/// the agent**, delivered into its root context at startup, not a canned
/// workflow step. That is what lets it set itself up — workflow-authoring
/// tools are root-scoped, so a prompt running as a step could never define the
/// loop/schedule it was asked for (`Caller::Workflow` vs `Caller::Root` in
/// the registry).
fn apply_instruction_sugar(doc: &mut Value) {
    let has_workflows = doc
        .pointer("/workflows")
        .and_then(Value::as_array)
        .is_some_and(|w| !w.is_empty());
    let nonblank = |p: &str| {
        doc.pointer(p)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    let has_instruction = nonblank("/agent/instruction");
    // An instruction that CARRIES a `:::workflow` directive has authored its
    // machinery explicitly — extraction (from_document) will add it to
    // `workflows:`, so generating a sugar `main` here would bolt a model loop
    // onto a config that declared none.
    let carries_workflow = doc
        .pointer("/agent/instruction")
        .and_then(Value::as_str)
        .is_some_and(|t| t.lines().any(|l| l.starts_with(":::workflow")));
    // A prompt runs as a root turn, so an instruction+prompt pair needs no
    // sugar workflow at all — the prompt IS the job.
    if has_workflows || carries_workflow || !has_instruction || nonblank("/agent/prompt") {
        return;
    }
    let work = json!({
        "kind": "agent",
        "depends_on": ["start"],
        "instruction": "{{env.instruction}}",
    });
    let mut patch = Value::Object(Map::new());
    paths::set_path(
        &mut patch,
        "workflows",
        json!([{
            "name": "main",
            "version": 3,
            "steps": {
                "start": { "kind": "once" },
                "work":  work,
                "done":  { "kind": "finish", "depends_on": ["work"], "status": "completed", "output": "{{steps.work.output}}" }
            }
        }]),
    );
    file::merge_into(doc, patch);
}

/// Substitute `${VAR}` / `${VAR:-default}` references in **every string value**
/// of the merged document (config sections *and* inline workflows) from the
/// process environment. Braces are required — a bare `$VAR` (or a `$` not
/// followed by `{`) is left verbatim, and `$${` yields a literal `${` — so
/// shell snippets and prices survive untouched. An unset variable with no
/// default is a hard error (fail-closed). This is intentionally distinct from
/// `{{secret:NAME}}` / `{{secret-file:PATH}}` (which resolve a *redacted*
/// credential and are never echoed): `${VAR}` is for plain, loggable values
/// like hostnames, ports, and paths that differ per environment.
fn substitute_env(v: &mut Value, env: &HashMap<&str, &str>) -> Result<(), String> {
    match v {
        Value::String(s) => {
            if s.as_bytes().contains(&b'$') {
                *s = expand_env_str(s, env)?;
            }
            Ok(())
        }
        Value::Array(a) => a.iter_mut().try_for_each(|item| substitute_env(item, env)),
        Value::Object(m) => m.values_mut().try_for_each(|val| substitute_env(val, env)),
        _ => Ok(()),
    }
}

/// Expand a single string's `${…}` references (see [`substitute_env`]).
fn expand_env_str(s: &str, env: &HashMap<&str, &str>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // `$` is ASCII and can never appear inside a multi-byte UTF-8 sequence,
        // so scanning for it byte-wise is safe; the fallthrough advances by whole
        // chars to keep every slice on a boundary.
        if b[i] == b'$' {
            if b.get(i + 1) == Some(&b'$') {
                out.push('$'); // `$$` -> literal `$`
                i += 2;
                continue;
            }
            if b.get(i + 1) == Some(&b'{') {
                let start = i + 2;
                let Some(rel) = s[start..].find('}') else {
                    return Err(format!("unterminated `${{` in config value {s:?}"));
                };
                let end = start + rel;
                let expr = &s[start..end];
                let (name, default) = match expr.split_once(":-") {
                    Some((n, d)) => (n.trim(), Some(d)),
                    None => (expr.trim(), None),
                };
                if name.is_empty() {
                    return Err(format!("empty `${{}}` reference in config value {s:?}"));
                }
                if !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    return Err(format!(
                        "invalid environment variable name {name:?} in `${{{expr}}}`"
                    ));
                }
                match env.get(name) {
                    Some(val) => out.push_str(val),
                    None => match default {
                        Some(d) => out.push_str(d),
                        None => {
                            return Err(format!(
                                "environment variable ${{{name}}} is not set (referenced in config); \
                                 set it or write ${{{name}:-default}}"
                            ));
                        }
                    },
                }
                i = end + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Validation (RFC 0030 §5)
// ---------------------------------------------------------------------------

/// Collected diagnostics: `errors` fail the load (exit 2); `warnings` are
/// advisory (logged, printed by `--validate-config`).
#[derive(Debug, Default, Clone)]
pub struct Diagnostics {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Every check, collected (never fast-fails) so `--validate-config` reports
/// all problems at once. Pure.
/// Validate a unified `auth:` block (RFC 0031 §5) — the required fields per
/// `kind`/`grant`, and secret-freedom for credential fields. Returns error
/// strings prefixed with `ctx` (e.g. `mcp server 'github'`).
fn validate_auth_block(auth: &Auth, ctx: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Credential fields must be `{{secret:…}}` references, never inline.
    for (field, s) in [
        ("client_secret", &auth.client_secret),
        ("token", &auth.token),
        ("value", &auth.value),
    ] {
        if let Some(sec) = s
            && !sec.0.trim().is_empty()
            && !crate::sec::secret::has_secret_ref(&sec.0)
        {
            out.push(format!(
                "{ctx}: auth.{field} carries an inline credential; use a {{{{secret:…}}}} reference"
            ));
        }
    }
    match auth.kind {
        AuthKind::Static => {
            let has_bearer = auth.token.is_some();
            let has_header = auth.header.is_some() && auth.value.is_some();
            if !has_bearer && !has_header {
                out.push(format!(
                    "{ctx}: auth.kind static needs `token` (a bearer) or `header` + `value`"
                ));
            }
        }
        AuthKind::Aws => {
            if auth.region.is_none() {
                out.push(format!("{ctx}: auth.kind aws needs `region`"));
            }
            if auth.service.is_none() {
                out.push(format!(
                    "{ctx}: auth.kind aws needs `service` (e.g. bedrock, execute-api)"
                ));
            }
            match auth.source.as_deref() {
                Some("sso") => {
                    if auth.sso_start_url.is_none()
                        || auth.account_id.is_none()
                        || auth.role_name.is_none()
                    {
                        out.push(format!(
                            "{ctx}: aws source sso needs `sso_start_url` + `account_id` + `role_name`"
                        ));
                    }
                }
                Some(src) if !matches!(src, "env" | "static" | "imds" | "irsa") => {
                    out.push(format!(
                        "{ctx}: auth.source '{src}' is not a known AWS source (env|static|imds|irsa|sso)"
                    ));
                }
                _ => {}
            }
        }
        AuthKind::Spiffe => match auth.svid.as_deref().unwrap_or("jwt") {
            "jwt" => {
                if auth.jwt_svid_file.is_none() {
                    out.push(format!(
                        "{ctx}: auth.kind spiffe (svid jwt) needs `jwt_svid_file`"
                    ));
                }
            }
            "x509" => {
                if auth.svid_file.is_none() || auth.key_file.is_none() {
                    out.push(format!(
                        "{ctx}: auth.kind spiffe (svid x509) needs `svid_file` + `key_file`"
                    ));
                }
            }
            other => out.push(format!("{ctx}: auth.svid '{other}' (want jwt|x509)")),
        },
        AuthKind::Oauth2 => {
            if auth.client_id.is_none() {
                out.push(format!("{ctx}: auth.kind oauth2 needs `client_id`"));
            }
            if auth.token_url.is_none() && auth.issuer.is_none() {
                out.push(format!(
                    "{ctx}: auth oauth2 needs `token_url` or `issuer` (for discovery)"
                ));
            }
            match auth.grant.unwrap_or(OAuthGrant::Device) {
                OAuthGrant::Device => {
                    if auth.device_authorization_url.is_none() && auth.issuer.is_none() {
                        out.push(format!(
                            "{ctx}: the device grant needs `device_authorization_url` or `issuer`"
                        ));
                    }
                }
                OAuthGrant::ClientCredentials => {
                    if auth.client_secret.is_none() {
                        out.push(format!(
                            "{ctx}: the client_credentials grant needs `client_secret`"
                        ));
                    }
                }
                OAuthGrant::AuthorizationCode => {
                    if auth.authorization_url.is_none() && auth.issuer.is_none() {
                        out.push(format!(
                            "{ctx}: the authorization_code grant needs `authorization_url` or `issuer`"
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Why a declared header value's `{{secret:NAME}}` / `{{secret-file:PATH}}` ref
/// does not resolve, or `None` when it does (or when the value carries no ref).
///
/// This is the security half of header validation, and it is not cosmetic: a
/// header whose ref does not resolve is a header that is **not sent**, so
/// without this check the process starts and dials the endpoint with no
/// credential at all. Validate-before-side-effect (RFC 0011 §2) means that is
/// exit 2 at startup, naming the ref — the same rule, and the same resolver, the
/// runtime uses at the moment of use. The message names the ref, never the
/// resolved value (RFC 0012 §3.7).
fn unresolved_secret_ref(value: &str) -> Option<String> {
    if !crate::sec::secret::has_secret_ref(value) {
        return None;
    }
    // Interactively-entered values (`--prompt-missing`) count as resolvable:
    // by the time the runtime dereferences the ref, the prompted store answers.
    crate::sec::secret::refs_resolvable(value, &|k| {
        crate::sec::secret::prompted_of(k).or_else(|| std::env::var(k).ok())
    })
    .err()
}

pub fn validate(loaded: &Loaded) -> Diagnostics {
    let s = &loaded.settings;
    let mut d = Diagnostics::default();
    let err = |d: &mut Diagnostics, m: String| d.errors.push(m);

    // config_version
    if let Some(v) = &s.config_version
        && v != schema::CONFIG_VERSION
    {
        err(
            &mut d,
            format!(
                "config_version must be \"{}\" (got {v:?})",
                schema::CONFIG_VERSION
            ),
        );
    }

    // intelligence
    for e in &s.intelligence.endpoints {
        if let Err(e) = super::validate_intelligence_uri(e) {
            err(&mut d, e.to_string());
        }
    }
    if let Some(p) = &s.intelligence.swap_policy
        && super::SwapPolicy::parse(p).is_none()
    {
        err(
            &mut d,
            format!("intelligence.swap_policy: {p:?} (want finish-on-old|restart-turn)"),
        );
    }
    if s.intelligence.token.is_some() && s.intelligence.token_file.is_some() {
        d.warnings.push(
            "intelligence.token and intelligence.token_file are both set; the inline token wins"
                .into(),
        );
    }
    if let Some(auth) = &s.intelligence.auth {
        for e in validate_auth_block(auth, "intelligence") {
            err(&mut d, e);
        }
    }
    if let Some(dialect) = &s.intelligence.dialect {
        if crate::intel::client::Provider::from_dialect(Some(dialect)).is_none() {
            err(
                &mut d,
                format!("intelligence.dialect: {dialect:?} (want openai|anthropic|bedrock)"),
            );
        }
        // Native Bedrock authenticates by SigV4 — an `auth: {kind: aws}` is
        // required (creds come from env/imds/irsa/sso at dial time).
        if dialect == "bedrock"
            && !matches!(
                s.intelligence.auth.as_ref().map(|a| a.kind),
                Some(AuthKind::Aws)
            )
        {
            err(
                &mut d,
                "intelligence.dialect: bedrock requires intelligence.auth.kind = aws (SigV4)"
                    .into(),
            );
        }
    }
    validate_budget(&s.intelligence.budget, "intelligence.budget", &mut d);
    if let Some(b) = &s.agent.conversation_budget {
        validate_budget(b, "agent.conversation_budget", &mut d);
    }
    for (name, value) in &s.intelligence.headers {
        if super::is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(value) {
            err(
                &mut d,
                format!(
                    "intelligence.headers['{name}'] looks like a credential but has an inline value; use {{{{secret:NAME}}}} / {{{{secret-file:PATH}}}}"
                ),
            );
        } else if let Some(e) = unresolved_secret_ref(value) {
            err(&mut d, format!("intelligence.headers['{name}']: {e}"));
        }
    }

    // mcp servers
    let mut names = std::collections::HashSet::new();
    for srv in &s.mcp.servers {
        if srv.name.trim().is_empty() {
            err(&mut d, "mcp.servers[]: a server has an empty name".into());
        }
        if !names.insert(srv.name.as_str()) {
            err(
                &mut d,
                format!("mcp.servers[]: duplicate server name '{}'", srv.name),
            );
        }
        if srv.name == "code" {
            err(
                &mut d,
                "mcp.servers[]: the server name 'code' is reserved for code-registered tools"
                    .into(),
            );
        }
        if srv.endpoint.is_empty() {
            // A `service:` reference is filled by resolution (an unknown name
            // already errored there); a server with NEITHER is malformed.
            if srv.service.is_none() {
                err(
                    &mut d,
                    format!(
                        "mcp server '{}' needs an `endpoint` or a `service:` catalog reference",
                        srv.name
                    ),
                );
            }
        } else if let Err(e) = super::mcp_endpoint_scheme_ok(&srv.endpoint) {
            err(&mut d, format!("mcp server '{}': {e}", srv.name));
        }
        if let Err(e) = srv.tag_set() {
            err(&mut d, e);
        }
        for (h, v) in &srv.headers {
            if super::is_secret_shaped_key(h) && !crate::sec::secret::has_secret_ref(v) {
                err(
                    &mut d,
                    format!(
                        "mcp server '{}' header '{h}' looks like a credential but has an inline value; use a {{{{secret:…}}}} reference",
                        srv.name
                    ),
                );
            } else if let Some(e) = unresolved_secret_ref(v) {
                err(
                    &mut d,
                    format!("mcp server '{}' header '{h}': {e}", srv.name),
                );
            }
        }
        if let Some(auth) = &srv.auth {
            for e in validate_auth_block(auth, &format!("mcp server '{}'", srv.name)) {
                err(&mut d, e);
            }
        }
    }
    // services catalog (RFC 0037 §6)
    for (name, svc) in &s.services {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            err(
                &mut d,
                format!("services: entry name '{name}' must be [a-zA-Z0-9_-]+"),
            );
        }
        if let Err(e) = super::mcp_endpoint_scheme_ok(&svc.endpoint) {
            err(&mut d, format!("services.{name}: {e}"));
        }
        for list in svc.tags.values() {
            for t in list {
                if crate::sec::scope::TrifectaTag::parse(t).is_none() {
                    err(
                        &mut d,
                        format!("services.{name} has unknown trifecta tag '{t}'"),
                    );
                }
            }
        }
        for (h, v) in &svc.headers {
            if super::is_secret_shaped_key(h) && !crate::sec::secret::has_secret_ref(v) {
                err(
                    &mut d,
                    format!(
                        "services.{name} header '{h}' looks like a credential but has an inline value; use a {{{{secret:…}}}} reference"
                    ),
                );
            } else if let Some(e) = unresolved_secret_ref(v) {
                err(&mut d, format!("services.{name} header '{h}': {e}"));
            }
        }
        if let Some(auth) = &svc.auth {
            for e in validate_auth_block(auth, &format!("services.{name}")) {
                err(&mut d, e);
            }
        }
        if let Some(r) = &svc.rate
            && let Err(e) = crate::supervisor::tree::parse_rate(r)
        {
            err(&mut d, format!("services.{name}.rate: {e}"));
        }
    }
    // Matching must be unambiguous: no entry's endpoint may itself match
    // another entry (identical or prefix-comparable endpoints).
    {
        let entries: Vec<(&String, &Service)> = s.services.iter().collect();
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                let one = BTreeMap::from([(entries[i].0.clone(), entries[i].1.clone())]);
                let other = BTreeMap::from([(entries[j].0.clone(), entries[j].1.clone())]);
                if service_match(&one, &entries[j].1.endpoint).is_some()
                    || service_match(&other, &entries[i].1.endpoint).is_some()
                {
                    err(
                        &mut d,
                        format!(
                            "services.{} and services.{} have prefix-comparable endpoints — URL matching must be unambiguous",
                            entries[i].0, entries[j].0
                        ),
                    );
                }
            }
        }
    }
    // Egress policy (RFC 0037 §5): closed ⇒ every configured MCP dial must be
    // catalogued; surfaces the policy does not yet cover are named out loud.
    if s.security.egress == Egress::Closed {
        for srv in &s.mcp.servers {
            if !srv.endpoint.is_empty() && service_match(&s.services, &srv.endpoint).is_none() {
                err(
                    &mut d,
                    format!(
                        "security.egress is closed and mcp server '{}' ({}) matches no services: catalog entry — catalog the endpoint to allow it",
                        srv.name, srv.endpoint
                    ),
                );
            }
        }
        let mut uncovered = Vec::new();
        if !s.intelligence.endpoints.is_empty() {
            uncovered.push("intelligence.endpoints");
        }
        if !s.a2a.peers.is_empty() {
            uncovered.push("a2a.peers");
        }
        if s.store.kind == StoreKind::Http {
            uncovered.push("store.http");
        }
        if s.workflows.iter().any(|w| w.get("url").is_some()) {
            uncovered.push("workflows[].url");
        }
        if !uncovered.is_empty() {
            d.warnings.push(format!(
                "security.egress: closed covers MCP dials (and A2A push targets) in this phase; NOT yet covered: {} (RFC 0037 Phase B)",
                uncovered.join(", ")
            ));
        }
    }
    // subagent templates (RFC 0036): extraction, tier resolution and the
    // instance-machinery checks all run here, so a bad template refuses the
    // PARENT's startup with the template named.
    if let Err(errs) = crate::config::templates::compile_templates(s) {
        for e in errs {
            err(&mut d, e);
        }
    }
    if !s.subagents.templates.is_empty() && s.a2a.listen.is_none() {
        d.warnings.push(
            "subagents.templates are declared but a2a.listen is unset — instance-tier children get no `parent` peer (they cannot call home)".into(),
        );
    }
    let server_known = |n: &str| s.mcp.servers.iter().any(|x| x.name == n);

    // tools
    for (name, ov) in &s.tools.overrides {
        if !server_known(&ov.server) {
            err(
                &mut d,
                format!(
                    "tools.overrides['{name}'] references undeclared MCP server '{}'",
                    ov.server
                ),
            );
        }
        if s.tools.disabled.iter().any(|x| x == name) {
            err(
                &mut d,
                format!("tool '{name}' is both disabled and overridden"),
            );
        }
        for (label, tpl) in [("args", &ov.args), ("result", &ov.result)] {
            if let Some(t) = tpl
                && let Some(expr) = t.strip_prefix("CEL:")
                && let Err(e) = crate::cel::compile_check(expr.trim())
            {
                err(&mut d, format!("tools.overrides['{name}'].{label}: {e}"));
            }
        }
    }

    // store
    match s.store.kind {
        StoreKind::Mcp => match &s.store.mcp {
            None => err(&mut d, "store.kind is mcp but store.mcp is not set".into()),
            Some(m) => {
                if !server_known(&m.server) {
                    err(
                        &mut d,
                        format!(
                            "store.mcp.server '{}' is not a declared MCP server",
                            m.server
                        ),
                    );
                }
                for (label, op) in [
                    ("put", &m.put),
                    ("get", &m.get),
                    ("list", &m.list),
                    ("delete", &m.delete),
                ] {
                    if let Some(op) = op {
                        for (f, t) in [
                            ("args", &op.args),
                            ("ok", &op.ok),
                            ("conflict", &op.conflict),
                            ("value", &op.value),
                            ("keys", &op.keys),
                        ] {
                            if let Some(t) = t
                                && let Some(expr) = t.strip_prefix("CEL:")
                                && let Err(e) = crate::cel::compile_check(expr.trim())
                            {
                                err(&mut d, format!("store.mcp.{label}.{f}: {e}"));
                            }
                        }
                    }
                }
            }
        },
        StoreKind::Http => match &s.store.http {
            None => err(
                &mut d,
                "store.kind is http but store.http is not set".into(),
            ),
            Some(h) => {
                if !(h.base_url.starts_with("https://") || h.base_url.starts_with("http://")) {
                    err(
                        &mut d,
                        format!(
                            "store.http.base_url must be an http(s) URL (got {})",
                            h.base_url
                        ),
                    );
                }
                if h.get.is_none() || h.put.is_none() {
                    err(
                        &mut d,
                        "store.http needs at least `get` and `put` operations".into(),
                    );
                }
                for (name, v) in &h.headers {
                    if super::is_secret_shaped_key(name) && !crate::sec::secret::has_secret_ref(v) {
                        err(
                            &mut d,
                            format!(
                                "store.http.headers['{name}'] looks like a credential but has an inline value"
                            ),
                        );
                    } else if let Some(e) = unresolved_secret_ref(v) {
                        err(&mut d, format!("store.http.headers['{name}']: {e}"));
                    }
                }
            }
        },
        StoreKind::File => {
            // No block is required: `kind: file` alone resolves a root from the
            // environment (RFC 0033 §4). The one thing that cannot work is an
            // explicit empty path — it would resolve to the process's working
            // directory, so it is refused here rather than discovered as a
            // state directory nobody meant to create.
            if let Some(f) = &s.store.file
                && f.path.as_deref().is_some_and(|p| p.trim().is_empty())
            {
                err(
                    &mut d,
                    "store.file.path is empty — set a directory, or omit the field to use $AGENTD_STATE_DIR / $XDG_STATE_HOME/agentd/state".into(),
                );
            }
        }
        StoreKind::Memory => {
            d.warnings.push(
                "store.kind is memory: state does not survive the process (dev/test only)".into(),
            );
        }
        StoreKind::None => {
            // A job-shaped instance (one-shot workflows, no listener) may run
            // without a store — a crash re-runs it. Anything long-lived MUST
            // be durable (RFC 0025): an A2A listener or a long-lived start node.
            //
            // Reaching here with a long-lived instance now means the operator
            // WROTE `none` (RFC 0033 §5): the absent case was defaulted to the
            // file store back in `load`. So the message says how to take the
            // default back, not just which backends exist.
            if s.is_long_lived() {
                err(&mut d, "store.kind is none but the instance is long-lived (serves A2A / webhooks / a goal watchdog / has a loop|schedule|subscribe|signal|event|a2a|webhook start node) — configure a durable store (store.kind: file | mcp | http), or drop store.kind to get the local file store by default".into());
            } else if !s.workflows.is_empty() {
                d.warnings.push("store.kind is none: this one-shot run is not durable (a crash re-runs it from scratch); set store.kind for durability".into());
            }
        }
    }
    // The mirror of the checks above: each adapter validates the block it needs,
    // so a block that belongs to an adapter that is not selected is dead config.
    // Silence would be the wrong answer — `store.file.path` set beside
    // `kind: mcp` reads like state on disk and is not — but so would refusing to
    // start, since the block does no harm; the operator is told it is ignored.
    if s.store.file.is_some() && s.store.kind != StoreKind::File {
        d.warnings.push(format!(
            "store.file is set but store.kind is {} — the file adapter is not in use and the block is ignored",
            // The Debug name lowercased is exactly the YAML spelling of the
            // variant (`serde(rename_all = "lowercase")`), so the warning
            // quotes back what the operator wrote.
            format!("{:?}", s.store.kind).to_lowercase()
        ));
    }
    if let Some(ms) = s.store.checkpoint.debounce_ms
        && ms > 60_000
    {
        d.warnings.push(format!(
            "store.checkpoint.debounce_ms is {ms} (> 60s): progress may lag far behind reality"
        ));
    }

    // knowledge / search / skills servers
    if let Some(k) = &s.knowledge.server
        && !server_known(k)
    {
        err(
            &mut d,
            format!("knowledge.server '{k}' is not a declared MCP server"),
        );
    }
    if let Some(k) = &s.search.server
        && !server_known(k)
    {
        err(
            &mut d,
            format!("search.server '{k}' is not a declared MCP server"),
        );
    }
    for src in &s.skills.sources {
        if !server_known(&src.server) {
            err(
                &mut d,
                format!(
                    "skills.sources[] references undeclared MCP server '{}'",
                    src.server
                ),
            );
        }
    }
    if let Some(c) = s.context.compact_at
        && !(c > 0.0 && c <= 1.0)
    {
        err(
            &mut d,
            format!("context.compact_at must be in (0, 1] (got {c})"),
        );
    }

    // workflows (structural minimum here; RFC 0027 validation lives in the engine)
    let mut wf_names = std::collections::HashSet::new();
    for (i, w) in s.workflows.iter().enumerate() {
        let Some(obj) = w.as_object() else {
            err(&mut d, format!("workflows[{i}] must be an object"));
            continue;
        };
        // A `{dir}` entry names no workflow: it expands into one per matching
        // file, and each file carries its own name. Requiring one here would
        // mean inventing a name for a set.
        if obj.contains_key("dir") {
            continue;
        }
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
        if name.trim().is_empty() {
            err(&mut d, format!("workflows[{i}] has no name"));
        } else if !wf_names.insert(name.to_string()) {
            err(
                &mut d,
                format!("workflows[]: duplicate workflow name '{name}'"),
            );
        }
        // One entry, one source. `dir` is not in this list because a dir entry
        // returned above — it names a SET, and each file it expands to gets
        // checked as its own entry.
        let sources = ["file", "uri", "url", "steps"]
            .iter()
            .filter(|k| obj.contains_key(**k))
            .count();
        if sources != 1 {
            err(
                &mut d,
                format!(
                    "workflows['{name}'] must have exactly one of file | uri | url | steps (dir is a separate entry shape)"
                ),
            );
        }
        if let Some(f) = obj.get("file").and_then(Value::as_str)
            && !std::path::Path::new(f).exists()
        {
            err(
                &mut d,
                format!("workflows['{name}'].file {f:?} does not exist"),
            );
        }
    }

    // lifecycle
    for (k, v) in &s.lifecycle.exit_code_map {
        if k != "3" && k != "7" {
            err(
                &mut d,
                format!(
                    "lifecycle.exit_code_map: only the policy codes 3 and 7 are remappable (got key {k:?})"
                ),
            );
        }
        if !(0..=255).contains(v) {
            err(
                &mut d,
                format!("lifecycle.exit_code_map[{k}] must be 0..=255 (got {v})"),
            );
        }
    }
    if s.lifecycle.watch_config && loaded.files.is_empty() {
        err(
            &mut d,
            "lifecycle.watch_config requires a config file (--config / AGENTD_CONFIG)".into(),
        );
    }

    // a2a
    if let Some(l) = &s.a2a.listen {
        match super::ServeTarget::parse(l) {
            Ok(super::ServeTarget::Http { bind, tls }) => {
                let loopback = crate::net::http::is_loopback_host(super::serve_host_of(&bind));
                if tls && (s.a2a.tls.cert.is_none() || s.a2a.tls.key.is_none()) {
                    err(
                        &mut d,
                        "a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set".into(),
                    );
                }
                if !loopback
                    && s.a2a.tls.client_ca.is_none()
                    && s.a2a.bearer.is_none()
                    && !s.interface.pairing.enabled
                {
                    err(&mut d, "a2a.listen on a non-loopback address needs client auth: a2a.tls.client_ca, a2a.bearer, and/or interface.pairing".into());
                }
                if !tls && !loopback {
                    err(
                        &mut d,
                        "a2a.listen plaintext http:// is allowed for loopback only; use https://"
                            .into(),
                    );
                }
            }
            Ok(super::ServeTarget::Unix { .. }) => {
                // The kernel is the authenticator: only same-uid (or root)
                // peers may connect, so TLS material is meaningless here and
                // configuring it is a sign of a misunderstood posture.
                if s.a2a.tls.cert.is_some()
                    || s.a2a.tls.key.is_some()
                    || s.a2a.tls.client_ca.is_some()
                {
                    err(
                        &mut d,
                        "a2a.listen is unix:// — the kernel authenticates peers (same-uid); a2a.tls does not apply and must be unset".into(),
                    );
                }
            }
            Err(e) => err(&mut d, format!("a2a.listen: {e}")),
        }
    }

    // interface (RFC 0032 display-client surface — rides the A2A listener)
    if s.interface.enabled && s.a2a.listen.is_none() {
        err(
            &mut d,
            "interface.enabled requires a2a.listen (the interface is served on the A2A listener)"
                .into(),
        );
    }
    if s.interface.debug && !s.interface.enabled {
        d.warnings
            .push("interface.debug has no effect while interface.enabled is false".into());
    }
    for o in &s.interface.origins {
        // An origin is `scheme://host[:port]` — no path, no trailing slash.
        let ok = o
            .split_once("://")
            .map(|(scheme, rest)| {
                matches!(scheme, "http" | "https") && !rest.is_empty() && !rest.contains('/')
            })
            .unwrap_or(false);
        if !ok {
            err(
                &mut d,
                format!(
                    "interface.origins: {o:?} is not an origin (want scheme://host[:port], no path)"
                ),
            );
        }
    }
    // Display items: unknown names are skipped by clients — warn, don't refuse
    // (forward compatibility across client versions).
    for (edge, items) in [
        ("top", &s.interface.display.top),
        ("bottom", &s.interface.display.bottom),
    ] {
        for item in items.iter().flatten() {
            // `memory:<key>` renders whatever a WORKFLOW wrote to that key —
            // the extension point that lets the status line show a branch, a PR
            // number or a deploy state without the daemon learning to compute
            // any of them. The key still has to be a legal memory key, so a
            // typo is caught here rather than silently never rendering.
            if let Some(key) = item.strip_prefix("memory:") {
                if key.is_empty() {
                    d.errors.push(format!(
                        "interface.display.{edge}: {item:?} names no memory key"
                    ));
                } else if let Err(e) = crate::context::memory::Memory::check_key(key) {
                    d.errors
                        .push(format!("interface.display.{edge}: {item:?}: {e}"));
                }
                continue;
            }
            if !DISPLAY_ITEMS.contains(&item.as_str()) {
                d.warnings.push(format!(
                    "interface.display.{edge}: unknown item {item:?} (clients skip it); known: {}, \
                     or memory:<key> for a value a workflow maintains",
                    DISPLAY_ITEMS.join(", ")
                ));
            }
        }
    }
    // Pairing (RFC 0032 §13).
    if s.interface.pairing.enabled {
        if !s.interface.enabled {
            err(
                &mut d,
                "interface.pairing.enabled requires interface.enabled (pairing rides the interface surface)".into(),
            );
        }
        if let Some(role) = s.interface.pairing.role
            && !matches!(role, Role::Operator | Role::User)
        {
            err(
                &mut d,
                "interface.pairing.role must be operator or user".into(),
            );
        }
    }

    // webhooks (RFC 0027 inbound HTTP surface)
    let uses_webhook = s.workflows.iter().any(workflow_uses_webhook);
    if uses_webhook && s.webhooks.listen.is_none() {
        err(&mut d, "a `webhook` node (start or wait) is used but webhooks.listen is not set — configure webhooks.listen (https://host:port)".into());
    }
    if let Some(l) = &s.webhooks.listen {
        match super::ServeTarget::parse(l) {
            Ok(super::ServeTarget::Unix { .. }) => {
                err(
                    &mut d,
                    "webhooks.listen does not support unix:// (webhooks are an external surface); use https://".into(),
                );
            }
            Ok(super::ServeTarget::Http { bind, tls }) => {
                let loopback = crate::net::http::is_loopback_host(super::serve_host_of(&bind));
                if tls && (s.webhooks.tls.cert.is_none() || s.webhooks.tls.key.is_none()) {
                    err(
                        &mut d,
                        "webhooks.listen is https:// but webhooks.tls.cert / webhooks.tls.key are not set"
                            .into(),
                    );
                }
                if !tls && !loopback {
                    err(
                        &mut d,
                        "webhooks.listen plaintext http:// is allowed for loopback only; use https://"
                            .into(),
                    );
                }
                // Symmetric with the `a2a.listen` refusal above: both are inbound
                // listeners that TRIGGER work, so a reachable one must authenticate
                // its callers — an open webhook route hands the agent's workflows to
                // anyone who can reach the port. Auth is resolved per route
                // (`runtime::webhooks::build_verify`: the node's own `auth`, else the
                // listener `default_auth`), so refuse only when a route would really
                // end up unverified — a listener whose every node signs is fine.
                // `none: true` is the documented loopback-only dev opt-out, not
                // authentication, so it does not buy an open public bind; the schema
                // offers no other way to ask for one, and this deliberately does not
                // invent one.
                if !loopback && !webhook_default_verifies(s.webhooks.default_auth.as_ref()) {
                    let mut open: Vec<String> = Vec::new();
                    let mut nodes = 0usize;
                    for w in &s.workflows {
                        let wf = w.get("name").and_then(Value::as_str).unwrap_or("?");
                        for (node, auth) in webhook_nodes(w) {
                            nodes += 1;
                            if !webhook_auth_verifies(auth) {
                                open.push(format!("{wf}/{node}"));
                            }
                        }
                    }
                    if !open.is_empty() {
                        err(
                            &mut d,
                            format!(
                                "webhooks.listen on a non-loopback address needs auth: set webhooks.default_auth (hmac, bearer or header), or give every `webhook` node its own `auth` (HMAC recommended) — unauthenticated: {}",
                                open.join(", ")
                            ),
                        );
                    } else if nodes == 0 {
                        // Nothing is reachable yet (every path answers 404), so this
                        // is not a live hole — but the next node added would be one.
                        d.warnings.push("webhooks.listen is non-loopback with no webhooks.default_auth — every webhook node must declare its own `auth` (HMAC recommended)".into());
                    }
                }
            }
            Err(e) => err(&mut d, format!("webhooks.listen: {e}")),
        }
    }

    // goal watchdog (RFC 0026)
    if let Some(g) = &s.goal {
        let via = g.check.via.as_deref().unwrap_or("both");
        if via == "condition" && g.check.condition.is_none() {
            err(
                &mut d,
                "goal.check.via is 'condition' but goal.check.condition is not set".into(),
            );
        }
        for (label, act) in [("on_achieved", &g.on_achieved), ("on_stuck", &g.on_stuck)] {
            if let Some(GoalAction::Workflow(name)) = act
                && !s
                    .workflows
                    .iter()
                    .any(|w| w.get("name").and_then(Value::as_str) == Some(name.as_str()))
            {
                err(
                    &mut d,
                    format!(
                        "goal.{label} references workflow '{name}', which is not defined in workflows"
                    ),
                );
            }
        }
    }

    let mut peer_names = std::collections::HashSet::new();
    for p in &s.a2a.peers {
        if !peer_names.insert(p.name.as_str()) {
            err(
                &mut d,
                format!("a2a.peers[]: duplicate peer name '{}'", p.name),
            );
        }
        let unix_peer = p.endpoint.starts_with("unix://") || p.endpoint.starts_with("unix:");
        if unix_peer && !cfg!(unix) {
            err(
                &mut d,
                format!("a2a peer '{}': unix:// endpoints are unix-only", p.name),
            );
        }
        if !unix_peer && !p.endpoint.starts_with("https://") && !p.endpoint.starts_with("http://") {
            err(
                &mut d,
                format!(
                    "a2a peer '{}': endpoint must be http(s):// (or unix:///path for a co-located peer)",
                    p.name
                ),
            );
        }
        if p.client_cert.is_some() != p.client_key.is_some() {
            err(
                &mut d,
                format!(
                    "a2a peer '{}': client_cert and client_key must be set together",
                    p.name
                ),
            );
        }
        if let Some(auth) = &p.auth {
            for e in validate_auth_block(auth, &format!("a2a peer '{}'", p.name)) {
                err(&mut d, e);
            }
            if auth.kind == AuthKind::Aws {
                err(
                    &mut d,
                    format!(
                        "a2a peer '{}': SigV4 (auth kind aws) is a follow-up",
                        p.name
                    ),
                );
            }
        }
        for (h, v) in &p.headers {
            if super::is_secret_shaped_key(h) && !crate::sec::secret::has_secret_ref(v) {
                err(
                    &mut d,
                    format!(
                        "a2a peer '{}' header '{h}' looks like a credential but has an inline value",
                        p.name
                    ),
                );
            } else if let Some(e) = unresolved_secret_ref(v) {
                err(&mut d, format!("a2a peer '{}' header '{h}': {e}", p.name));
            }
        }
    }
    for (i, pr) in s.a2a.principals.iter().enumerate() {
        let m = &pr.matcher;
        if m.san.is_none()
            && m.sub.is_none()
            && m.bearer_ref.is_none()
            && m.aauth_agent.is_none()
            && !m.any
        {
            err(
                &mut d,
                format!(
                    "a2a.principals[{i}]: match needs one of san | sub | bearer_ref | aauth_agent | any"
                ),
            );
        }
        if m.any && pr.role == Role::Operator {
            err(
                &mut d,
                format!("a2a.principals[{i}]: `any` cannot grant the operator role"),
            );
        }
    }

    // observability
    if let Some(l) = &s.observability.log_level
        && crate::obs::log::Level::parse(l).is_none()
    {
        err(
            &mut d,
            format!("observability.log_level: {l:?} (want trace|debug|info|warn|error)"),
        );
    }

    // secrets provenance: the FILE layer must not carry inline secrets
    for m in secret_violations(&loaded.file_doc) {
        err(&mut d, m);
    }
    for f in &s.observability.audit.sink.clone().unwrap_or_default() {
        if *f == AuditSink::Store && s.store.kind == StoreKind::None {
            err(
                &mut d,
                "observability.audit.sink includes `store` but store.kind is none".into(),
            );
        }
    }

    // trifecta over the root grant (RFC 0012 §3.2)
    let mut tags = Vec::new();
    for srv in &s.mcp.servers {
        match srv.tag_set() {
            Ok(t) if t.is_empty() => tags.push(crate::sec::scope::TrifectaTag::UntrustedInput),
            Ok(t) => tags.extend(t),
            Err(_) => {}
        }
    }
    // The local command runner is a capability like any other, and it carries
    // the two heaviest legs: it can touch anything inside `workdir`
    // (`sensitive`) and it can talk to the network if the allow-list lets it
    // (`egress`). The registry tags it that way, but the registry is built
    // after validation — so the tags have to be contributed here, or enabling
    // `exec` next to an untrusted-input server assembles the whole trifecta
    // and starts anyway. Gated on the feature: without it `exec` is
    // mapping-only, and whichever MCP server provides it carries its own tags.
    #[cfg(feature = "exec")]
    if s.security.exec.enabled {
        tags.push(crate::sec::scope::TrifectaTag::Sensitive);
        tags.push(crate::sec::scope::TrifectaTag::Egress);
    }
    use crate::sec::scope::{TrifectaVerdict, check_trifecta};
    if check_trifecta(tags, s.security.allow_trifecta) == TrifectaVerdict::RefusedTrifecta {
        err(&mut d, "lethal-trifecta refused: the root grant wires untrusted_input + sensitive + egress into one agent; narrow the tags or set security.allow_trifecta (audited)".into());
    }

    // Workflow definitions — the SAME strict parse the runtime runs at startup
    // (`load_workflows`). Without it, `--validate-config` passes a config that
    // then exits 2 on the first real start: a typo'd step field (`prompt:` on
    // an `agent` node) validated clean and failed in production, which is what
    // the pre-flight check exists to prevent. Reported after the structural
    // checks above so the more basic error still leads. `file:`/`uri:` refs
    // resolve at startup, so only inline definitions are checkable here.
    // `store.durability.{a2a,steps}` is parsed, published in the schema, and
    // surfaced in the manifest — and read by no writer. `eventual` therefore
    // promised a weaker-but-faster durability that was never implemented, on the
    // one guarantee agentd exists to make. Refusing the value is better than
    // honouring it: a durability dial nobody wired is a lie, and implementing it
    // would trade away the property the product is for. `strict` (the default)
    // is what the engine already does, so only a config that asked for the
    // unimplemented setting fails, and it fails saying so.
    for (path, level) in [
        ("store.durability.a2a", s.store.durability.a2a),
        ("store.durability.steps", s.store.durability.steps),
    ] {
        if level == Some(DurabilityLevel::Eventual) {
            d.errors.push(format!(
                "{path}: `eventual` is not implemented — every durable write is strict \
                 (checkpoint-before-effect). Remove the key; `strict` is the default and \
                 the only behaviour."
            ));
        }
    }
    // The reference preflight, aggregated: every secret, secret-file and config
    // var an INLINE workflow mentions, checked now and reported TOGETHER.
    // (Definitions arriving from files, URLs and directories get the same check
    // at startup, after they are fetched.) Failing on whichever reference
    // happens to be evaluated first turns configuring a deployment into a
    // guessing game played one restart at a time.
    for (i, w) in s.workflows.iter().enumerate() {
        if w.get("steps").is_some() {
            for msg in missing_references(w, &format!("workflows[{i}]"), &s.vars) {
                err(&mut d, msg);
            }
        }
    }
    for w in &s.workflows {
        // A reference — file, uri, url or dir — resolves at startup, so there
        // is nothing to parse here. Only inline definitions are checkable.
        if w.get("steps").is_none() {
            // A credential in a `headers` value must be a reference, the same
            // rule every other header in this config follows.
            if let Some(h) = w.get("headers").and_then(Value::as_object) {
                for (k, v) in h {
                    if let Some(val) = v.as_str()
                        && super::is_secret_shaped_key(k)
                        && !crate::sec::secret::has_secret_ref(val)
                    {
                        d.errors.push(format!(
                            "workflows: headers[{k:?}] looks like a credential — use {{{{secret:NAME}}}} rather than a literal"
                        ));
                    }
                }
            }
            continue;
        }
        if let Err(errs) = crate::engine::model::parse_workflow(w) {
            // The parser's messages already name the workflow and the step.
            d.errors.extend(errs);
        }
        // Fan-out is checked HERE rather than in the parser because the ceiling
        // is a config value the parser cannot see.
        let cap = s
            .limits
            .workflow
            .fan_out
            .unwrap_or(crate::engine::model::MAX_BATCH_PARALLEL as u32);
        let wname = w.get("name").and_then(Value::as_str).unwrap_or("?");
        if let Some(steps) = w.get("steps").and_then(Value::as_object) {
            for (sid, step) in steps {
                let want = step.get("parallel").and_then(Value::as_u64).or_else(|| {
                    step.get("batch")
                        .and_then(|b| b.get("parallel"))
                        .and_then(Value::as_u64)
                });
                if let Some(want) = want
                    && want > cap as u64
                {
                    d.errors.push(format!(
                        "workflow {wname:?} step {sid:?}: parallel {want} exceeds \
                         limits.workflow.fan_out ({cap}) — raise the limit or lower the step"
                    ));
                }
            }
        }
    }
    d
}

/// Long-lived start-node kinds (RFC 0027 §4) — an instance running one needs a
/// durable store (RFC 0026 §8 lifecycle: `run_until: drained`).
pub const LONG_LIVED_STARTS: &[&str] = &[
    "loop",
    "schedule",
    "subscribe",
    "signal",
    "event",
    "a2a",
    "webhook",
];

/// Whether a raw workflow document has a long-lived start node.
pub fn workflow_is_long_lived(w: &Value) -> bool {
    w.get("steps")
        .and_then(Value::as_object)
        .is_some_and(|steps| {
            steps.values().any(|st| {
                st.get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|k| LONG_LIVED_STARTS.contains(&k))
            })
        })
}

/// Whether a raw workflow document uses the inbound webhook surface — a
/// `webhook` start node, or a `wait: {on: webhook}` callback (either needs
/// `webhooks.listen`).
pub fn workflow_uses_webhook(w: &Value) -> bool {
    w.get("steps")
        .and_then(Value::as_object)
        .is_some_and(|steps| {
            steps.values().any(|st| {
                let kind = st.get("kind").and_then(Value::as_str);
                kind == Some("webhook")
                    || (matches!(kind, Some("wait") | Some("await"))
                        && st.get("on").and_then(Value::as_str) == Some("webhook"))
            })
        })
}

/// The inbound-webhook routes a raw workflow document arms, as
/// `(node id, declared auth)`. Two shapes, matching what the listener reads: a
/// `webhook` start node carries its `auth` at the top level, while a
/// `wait: {on: webhook}` callback carries it under `webhook.auth`
/// (`runtime::webhooks::webhook_wait`).
fn webhook_nodes(w: &Value) -> Vec<(&str, Option<&Value>)> {
    let Some(steps) = w.get("steps").and_then(Value::as_object) else {
        return Vec::new();
    };
    steps
        .iter()
        .filter_map(|(id, st)| {
            let kind = st.get("kind").and_then(Value::as_str);
            if kind == Some("webhook") {
                Some((id.as_str(), st.get("auth")))
            } else if matches!(kind, Some("wait") | Some("await"))
                && st.get("on").and_then(Value::as_str) == Some("webhook")
            {
                Some((id.as_str(), st.get("webhook").and_then(|c| c.get("auth"))))
            } else {
                None
            }
        })
        .collect()
}

/// Whether a node's declared `auth` actually verifies the caller. This mirrors
/// `runtime::webhooks::build_verify` INCLUDING its type tests — there a
/// non-object `hmac`/`header` or a non-string `bearer` is not a verifier and
/// falls through, so counting it as auth here would bless a route the listener
/// serves open. `none: true` short-circuits to `Verify::None`, so it is the
/// opposite of authentication.
fn webhook_auth_verifies(auth: Option<&Value>) -> bool {
    let Some(a) = auth else { return false };
    if a.get("none").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    a.get("hmac").and_then(Value::as_object).is_some()
        || a.get("header").and_then(Value::as_object).is_some()
        || a.get("bearer").and_then(Value::as_str).is_some()
}

/// The same question for the listener-wide `default_auth` (the typed twin,
/// `runtime::webhooks::build_verify_typed`): `none` wins over everything, and a
/// declared-but-incomplete verifier still counts — the listener refuses to spawn
/// on it, which fails closed.
fn webhook_default_verifies(d: Option<&WebhookAuth>) -> bool {
    d.is_some_and(|d| !d.none && (d.hmac.is_some() || d.bearer.is_some() || d.header.is_some()))
}

fn validate_budget(b: &Budget, at: &str, d: &mut Diagnostics) {
    for (i, w) in b.windows.iter().enumerate() {
        if w.tokens.is_none() && w.requests.is_none() {
            d.errors
                .push(format!("{at}.windows[{i}]: set tokens and/or requests"));
        }
        if let Some(r) = &w.reset {
            // `HH:MMZ` is ASCII by construction, and the ASCII test must come
            // BEFORE the byte slices: `r.len()` is bytes, so a multi-byte char
            // (`0é:0Z` is six bytes) would otherwise make `r[..2]` land inside a
            // character and panic. A config error is exit 2, never a panic.
            let ok = r.len() == 6
                && r.is_ascii()
                && r.ends_with('Z')
                && r[..2].parse::<u32>().is_ok_and(|h| h < 24)
                && &r[2..3] == ":"
                && r[3..5].parse::<u32>().is_ok_and(|m| m < 60);
            if !ok {
                d.errors.push(format!(
                    "{at}.windows[{i}].reset must be HH:MMZ (got {r:?})"
                ));
            }
            if !w.per.is_calendar() {
                d.warnings.push(format!(
                    "{at}.windows[{i}].reset is only meaningful for day/week windows"
                ));
            }
        }
    }
    if let Some(f) = b.slow.factor
        && !(f > 0.0 && f <= 1.0)
    {
        d.errors
            .push(format!("{at}.slow.factor must be in (0, 1] (got {f})"));
    }
    if b.on_exhausted == BudgetTactic::Degrade && b.degrade.model.is_none() {
        d.errors.push(format!(
            "{at}.on_exhausted is degrade but {at}.degrade.model is not set"
        ));
    }
    if b.reserve.estimate == ReserveEstimate::Fixed && b.reserve.fixed.is_none() {
        d.errors.push(format!(
            "{at}.reserve.estimate is fixed but {at}.reserve.fixed is not set"
        ));
    }
}

/// Secret-bearing paths that must be REFERENCES when they come from a file.
const FILE_SECRET_PATHS: &[&str] = &[
    "/intelligence/token",
    "/a2a/bearer",
    "/security/aauth/enroll_token",
];

/// Inline (non-reference) credentials in the FILE document (RFC 0030 §5).
fn secret_violations(file_doc: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for p in FILE_SECRET_PATHS {
        if let Some(Value::String(v)) = file_doc.pointer(p)
            && !crate::sec::secret::has_secret_ref(v)
        {
            out.push(format!(
                "config file: {} carries an inline credential; use {{{{secret:NAME}}}} / {{{{secret-file:PATH}}}} (or set it from env/flag)",
                p.trim_start_matches('/').replace('/', ".")
            ));
        }
    }
    if let Some(servers) = file_doc.pointer("/mcp/servers").and_then(Value::as_array) {
        for s in servers {
            if let Some(Value::String(v)) = s.pointer("/oauth/client_secret")
                && !crate::sec::secret::has_secret_ref(v)
            {
                out.push(format!(
                    "config file: mcp server '{}' oauth.client_secret carries an inline credential; use a {{{{secret:…}}}} reference",
                    s.get("name").and_then(Value::as_str).unwrap_or("?")
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reload partition (RFC 0030 §6)
// ---------------------------------------------------------------------------

/// Restart-only path prefixes: a live reload whose effective document differs
/// under any of these is refused (`restart_required`).
pub const RESTART_ONLY_PATHS: &[&str] = &[
    "config_version",
    "agent.name",
    "store.kind",
    "store.prefix",
    "store.mcp",
    "store.http",
    // Moving the state directory under a running instance would strand every
    // key it has written, so it joins the other store paths as restart-only.
    "store.file",
    "lifecycle.run_until",
    "lifecycle.drain_timeout",
    "lifecycle.run_id",
    "lifecycle.exit_code_map",
    "lifecycle.watch_config",
    "a2a.listen",
    "a2a.tls",
    "a2a.bearer",
    "observability.otel",
    "observability.metrics_addr",
    "observability.health_file",
    "observability.events_ring",
    "observability.traceparent",
    "security",
];

/// The restart-only paths whose values differ between two effective documents.
pub fn restart_only_diff(running: &Value, candidate: &Value) -> Vec<String> {
    RESTART_ONLY_PATHS
        .iter()
        .filter(|p| {
            let ptr = format!("/{}", p.replace('.', "/"));
            running.pointer(&ptr) != candidate.pointer(&ptr)
        })
        .map(|p| (*p).to_string())
        .collect()
}

/// The `--help` section for the v2 paths.
pub fn help_section() -> String {
    paths::help_section_in(&paths::bindings_of(&schema::schema()))
}

/// The v2 `--help` text: usage, the alias flags, the removed flags, and every
/// config path (flag · env).
pub fn help_text() -> String {
    let mut out = format!(
        "agentd {ver} — a durable, workflow-driven agent (config schema v2)\n\
         \n\
         USAGE:\n\
         \x20 agentd --config <settings.yaml> [--config <overlay.yaml> …] [--<path> <value> …]\n\
         \x20 agentd --prompt <TEXT> --intelligence <URL>                    # one-shot: ask, answer, exit\n\
         \x20 agentd --instruction <TEXT> --intelligence <URL> [--mcp name=endpoint …]   # one-shot sugar\n\
         \x20 agentd tui|ui --config <settings.yaml> [--<path> <value> …]   # + a display client\n\
         \n\
         Every setting is a document path (YAML/JSON file, AGENTD_<PATH> env, --<path> flag);\n\
         several files merge in order (later wins). Precedence: built-in < files < env < flags.\n\
         \n\
         ALIASES (legacy spellings of paths):\n",
        ver = crate::VERSION
    );
    for a in ALIASES {
        let shape = match a.kind {
            AliasKind::Set | AliasKind::SetFromFile => "<value>",
            AliasKind::SetTrue => "",
            AliasKind::Append => "<value>  (adds one)",
            AliasKind::Special => "<value>",
        };
        out.push_str(&format!("  {:<32} {} → {}\n", a.flag, shape, a.path));
    }
    out.push_str(
        "\nSUBCOMMANDS (run the daemon with a display client attached; RFC 0032):\n\
         \x20 tui                        + the terminal UI (fullscreen; --inline for in-place)\n\
         \x20 ui                         + the web UI, opened in a browser\n\
         \x20                            both need `interface.enabled: true`, which the\n\
         \x20                            subcommand sets for you; the client exits with the daemon.\n\
         \x20                            Detached instead: run `agentd -c …`, then `agentd-tui\n\
         \x20                            --endpoint <url>` (npm i -g @agentd-dev/cli).\n\
         \nCONTROL:\n\
         \x20 -c, --config <PATH>        a settings file (repeatable; `=` form too; or AGENT_CONFIG=a.yaml:b.yaml)\n\
         \x20 --validate-config          load+validate everything, print the verdict, exit 0/2\n\
         \x20 --config-schema=2          print the settings JSON Schema (v2) and exit\n\
         \x20 --workflow-schema          print the workflow (dialect 3) JSON Schema + node registry and exit\n\
         \x20 --capabilities             print the capabilities manifest and exit\n\
         \x20 --login <target>           complete an OAuth device-login for an endpoint (e.g. mcp:<name>) and cache the token\n\
         \x20 --logout <target>          evict a cached credential\n\
         \x20 --prompt-missing           ask interactively (echo off, on /dev/tty) for each {{secret:NAME}} the startup preflight finds missing; refused without a controlling terminal\n\
         \x20 --env <FILE>               load a dotenv file into this process's environment (repeatable; real env wins, later files win)\n\
         \x20 -h, --help / -V, --version\n\
         \nREMOVED IN 2.0:\n",
    );
    for (flag, hint) in REMOVED_FLAGS {
        out.push_str(&format!("  {flag:<32} {hint}\n"));
    }
    out.push('\n');
    out.push_str(&help_section());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn write_tmp(contents: &str, ext: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn base_env() -> Vec<(String, String)> {
        vec![(
            "AGENTD_INTELLIGENCE_ENDPOINTS".into(),
            "https://intel.example/v1".into(),
        )]
    }

    // ---- schema ↔ struct drift ---------------------------------------------

    /// serde's `deny_unknown_fields` error names the expected fields; that
    /// list IS the struct's field set — compare it with the schema properties
    /// at every object, so neither can drift from the other.
    fn struct_fields_at(doc_path: &str) -> Vec<String> {
        // Build a document that is empty except for a probe key at `doc_path`.
        let mut probe = Value::Object(Map::new());
        let path = if doc_path.is_empty() {
            "__probe__".to_string()
        } else {
            format!("{doc_path}.__probe__")
        };
        paths::set_path(&mut probe, &path, json!(1));
        let err = Settings::from_document(probe, "t").expect_err("probe must be rejected");
        // "… unknown field `__probe__`, expected one of `a`, `b`, `c` …" (or
        // "expected `a` or `b`" for two, "expected `a`" for one).
        let after = err.split("expected").nth(1).unwrap_or("");
        let mut out: Vec<String> = after
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        out.sort();
        out
    }

    fn schema_props_at(schema: &Value, doc_path: &str) -> Vec<String> {
        let mut node = schema.clone();
        let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
        for seg in doc_path.split('.').filter(|s| !s.is_empty()) {
            let props = node.get("properties").cloned().unwrap_or(Value::Null);
            node = props.get(seg).cloned().unwrap_or(Value::Null);
            if let Some(r) = node.get("$ref").and_then(Value::as_str)
                && let Some(name) = r.strip_prefix("#/$defs/")
            {
                node = defs.get(name).cloned().unwrap_or(Value::Null);
            }
        }
        let mut out: Vec<String> = node
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    #[test]
    fn schema_matches_struct_at_every_object() {
        let schema = schema::schema();
        for path in [
            "",
            "agent",
            "agent.tools",
            "intelligence",
            "intelligence.auth",
            "intelligence.budget",
            "intelligence.budget.slow",
            "intelligence.budget.degrade",
            "intelligence.budget.reserve",
            "mcp",
            "tools",
            "store",
            "store.checkpoint",
            "store.durability",
            "memory",
            "context",
            "context.plan",
            "knowledge",
            "knowledge.auto_context",
            "search",
            "skills",
            "limits",
            "limits.run",
            "limits.subagents",
            "limits.subagents.instances",
            "subagents",
            "subagents.defaults",
            "lifecycle",
            "a2a",
            "a2a.tls",
            "observability",
            "observability.otel",
            "observability.audit",
            "security",
            "security.cgroup",
            "security.exec",
        ] {
            let s = schema_props_at(&schema, path);
            let f = struct_fields_at(path);
            assert_eq!(s, f, "schema/struct drift at `{path}`");
        }
    }

    #[test]
    fn every_schema_path_deserializes_a_sample() {
        // Every binding, given a kind-appropriate sample, must be accepted by
        // the typed Settings (proves the schema names real fields with the
        // right shapes — the paths mechanism depends on it).
        for b in paths::bindings_of(&schema::schema()) {
            let sample = match &b.kind {
                paths::Kind::String => match b.path.as_str() {
                    "config_version" => json!("2"),
                    _ => json!("x"),
                },
                paths::Kind::Integer => json!(1),
                paths::Kind::Number => json!(0.5),
                paths::Kind::Boolean => json!(true),
                paths::Kind::Enum(vs) => json!(vs[0]),
                paths::Kind::Array(item) => match (**item).clone() {
                    paths::Kind::Object => match b.path.as_str() {
                        "mcp.servers" => {
                            json!([{"name": "a", "endpoint": "https://a.example/mcp"}])
                        }
                        "workflows" => json!([{"name": "w", "steps": {}}]),
                        "a2a.principals" => json!([{"match": {"any": true}, "role": "user"}]),
                        "a2a.peers" => json!([{"name": "p", "endpoint": "https://p.example"}]),
                        "skills.sources" => json!([{"server": "s"}]),
                        "intelligence.budget.windows" | "agent.conversation_budget.windows" => {
                            json!([{"per": "hour", "tokens": 1}])
                        }
                        other => panic!("no sample for object list {other}"),
                    },
                    paths::Kind::Enum(vs) => json!([vs[0]]),
                    _ => json!(["s"]),
                },
                paths::Kind::Object => match b.path.as_str() {
                    "intelligence.pricing" => json!({"m": {"input_per_1k": 1.0}}),
                    "tools.overrides" => json!({"memory.get": {"server": "s", "tool": "t"}}),
                    "store.mcp" => json!({"server": "s"}),
                    "streams" => json!({"orders": {"retention": {"max_events": 1}}}),
                    "services" => json!({"billing": {"endpoint": "https://b.example/mcp"}}),
                    "subagents.templates" => json!({"t": {"instruction": "do the thing"}}),
                    "subagents.defaults.limits" => json!({"max_tokens": 1000}),
                    "store.http" => json!({"base_url": "https://s"}),
                    "security.aauth" => json!({"provider": "https://apd"}),
                    "lifecycle.exit_code_map" => json!({"3": 0}),
                    _ => json!({"k": "v"}),
                },
                paths::Kind::Any => match b.path.as_str() {
                    "intelligence.endpoints" => json!("https://a,https://b"),
                    "goal.on_achieved" | "goal.on_stuck" => json!("finish"),
                    p if p.ends_with("timeout")
                        || p.ends_with("deadline")
                        || p.ends_with("_grace")
                        || p.ends_with("ttl")
                        || p.ends_with("every") =>
                    {
                        json!("10s")
                    }
                    p if p.starts_with("agent.tools.") => json!("all"),
                    _ => json!("x"),
                },
            };
            let mut doc = Value::Object(Map::new());
            paths::set_path(&mut doc, &b.path, sample);
            fill_required(&mut doc, &schema::schema(), &b.path);
            Settings::from_document(doc, "t")
                .unwrap_or_else(|e| panic!("path {} does not deserialize: {e}", b.path));
        }
    }

    /// Along `path`, every schema object with `required` gets its required
    /// properties filled with a sample (so a lone leaf under `store.mcp` still
    /// types — the runtime validation reports the missing siblings instead).
    fn fill_required(doc: &mut Value, schema: &Value, path: &str) {
        let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
        let resolve = |v: &Value| -> Value {
            match v
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                Some(name) => defs.get(name).cloned().unwrap_or(Value::Null),
                None => v.clone(),
            }
        };
        let mut node = schema.clone();
        let mut prefix = String::new();
        let segs: Vec<&str> = path.split('.').collect();
        for (i, seg) in segs.iter().enumerate() {
            let props = node.get("properties").cloned().unwrap_or(Value::Null);
            node = resolve(&props.get(*seg).cloned().unwrap_or(Value::Null));
            prefix = if prefix.is_empty() {
                (*seg).to_string()
            } else {
                format!("{prefix}.{seg}")
            };
            if i + 1 == segs.len() {
                break;
            }
            if let Some(req) = node.get("required").and_then(Value::as_array) {
                let props = node.get("properties").cloned().unwrap_or(Value::Null);
                for r in req.iter().filter_map(Value::as_str) {
                    let p = format!("{prefix}.{r}");
                    if doc.pointer(&format!("/{}", p.replace('.', "/"))).is_none() {
                        // Honor an enum-typed required field (e.g. `auth.kind`) so
                        // the filled sample is a valid variant, not `"x"`.
                        let sample = match props
                            .get(r)
                            .and_then(|f| f.get("enum"))
                            .and_then(Value::as_array)
                            .filter(|a| !a.is_empty())
                        {
                            Some(vs) => vs[0].clone(),
                            None => match r {
                                "provider" | "base_url" | "url" => json!("https://x.example"),
                                _ => json!("x"),
                            },
                        };
                        paths::set_path(doc, &p, sample);
                    }
                }
            }
        }
    }

    #[test]
    fn env_and_flag_names_derive_from_the_v2_paths() {
        let bs = paths::bindings_of(&schema::schema());
        let model = bs.iter().find(|b| b.path == "intelligence.model").unwrap();
        assert_eq!(model.env_names()[0], "AGENTD_INTELLIGENCE_MODEL");
        assert_eq!(model.env_names()[2], "INTELLIGENCE_MODEL");
        assert_eq!(model.flag(), "--intelligence-model");
        let steps = bs.iter().find(|b| b.path == "limits.run.steps").unwrap();
        assert_eq!(steps.env_names()[0], "AGENTD_LIMITS_RUN_STEPS");
        // Uniqueness of the derived names across the whole v2 schema.
        let mut seen = std::collections::HashSet::new();
        for b in &bs {
            assert!(seen.insert(b.flag()), "duplicate flag {}", b.flag());
        }
    }

    // ---- detection ------------------------------------------------------------

    #[test]
    fn detects_v1_v2_mixed_and_empty() {
        assert_eq!(detect(&json!({})), Detected::Empty);
        assert_eq!(detect(&json!({"model": "m"})), Detected::V1);
        assert_eq!(detect(&json!({"config_version": "2"})), Detected::V2);
        assert_eq!(
            detect(&json!({"agent": {"instruction": "x"}})),
            Detected::V2
        );
        assert_eq!(detect(&json!({"agent": {}, "model": "m"})), Detected::Mixed);
        assert_eq!(
            detect(&json!({"config_version": "1.0", "model": "m"})),
            Detected::V1
        );
        // `limits` is neutral; `intelligence` decides by shape.
        assert_eq!(
            detect(&json!({"model": "m", "limits": {"max_steps": 1}})),
            Detected::V1
        );
        assert_eq!(
            detect(&json!({"intelligence": "https://x", "limits": {}})),
            Detected::V1
        );
        assert_eq!(
            detect(&json!({"intelligence": {"model": "m"}, "limits": {}})),
            Detected::V2
        );
        assert_eq!(detect(&json!({"limits": {"max_steps": 1}})), Detected::V1);
    }

    // ---- load: layering, aliases, sugar --------------------------------------

    #[cfg(feature = "exec")]
    #[test]
    fn enabling_exec_next_to_untrusted_input_assembles_the_trifecta() {
        // `exec` is tagged sensitive+egress in the tool registry — but the
        // registry is built AFTER validation, so for a long time those tags
        // never reached the check and this config started happily. It is the
        // whole lethal trifecta: untrusted input, sensitive powers, an egress
        // path.
        let cfg = "config_version: \"2\"\nstore: {kind: memory}\n\
                   mcp:\n  servers:\n    - name: web\n      endpoint: https://mcp-web.internal/mcp\n      tags: {\"*\": [untrusted_input]}\n\
                   security:\n  exec: {enabled: true, workdir: /tmp, allow: [git]}\n";
        let f = write_tmp(cfg, "yaml");
        let e = load(
            &args(&["--config", f.path().to_str().unwrap(), "--validate-config"]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("lethal-trifecta refused"), "{e}");

        // The documented override still lets an operator take the risk.
        load(
            &args(&[
                "--config",
                f.path().to_str().unwrap(),
                "--validate-config",
                "--allow-trifecta",
            ]),
            &base_env(),
        )
        .expect("--allow-trifecta is the escape hatch");

        // exec WITHOUT an untrusted-input source is only two legs: still fine.
        let alone = write_tmp(
            "config_version: \"2\"\nstore: {kind: memory}\n\
             security:\n  exec: {enabled: true, workdir: /tmp, allow: [git]}\n",
            "yaml",
        );
        load(
            &args(&[
                "--config",
                alone.path().to_str().unwrap(),
                "--validate-config",
            ]),
            &base_env(),
        )
        .expect("two legs are not the trifecta");
    }

    #[test]
    fn validate_config_catches_workflow_body_errors_the_runtime_would_refuse() {
        // The pre-flight check must not pass a config that then exits 2 on the
        // first real start. A typo'd step field used to validate clean and be
        // refused by `load_workflows` at startup — the worst possible split.
        let f = write_tmp(
            "config_version: \"2\"\nstore: {kind: memory}\nworkflows:\n  - name: w\n    version: 3\n    steps:\n      s: {kind: once}\n      a: {kind: agent, depends_on: [s], prompt: \"typo — agent steps take `instruction`\"}\n      f: {kind: finish, depends_on: [a], status: completed}\n",
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap(), "--validate-config"]),
            &base_env(),
        )
        .unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("unknown field"), "{msg}");
        assert!(msg.contains("prompt"), "{msg}");
        assert!(
            msg.contains("instruction"),
            "names the allowed fields: {msg}"
        );

        // The same workflow, spelled correctly, still validates.
        let ok = write_tmp(
            "config_version: \"2\"\nstore: {kind: memory}\nworkflows:\n  - name: w\n    version: 3\n    steps:\n      s: {kind: once}\n      a: {kind: agent, depends_on: [s], instruction: \"do it\"}\n      f: {kind: finish, depends_on: [a], status: completed}\n",
            "yaml",
        );
        load(
            &args(&["--config", ok.path().to_str().unwrap(), "--validate-config"]),
            &base_env(),
        )
        .expect("a correct workflow validates");
    }

    #[test]
    fn a_prompt_is_a_message_not_a_sugar_workflow() {
        // A prompt is delivered into the agent's ROOT context at startup, so
        // it authors no workflow — that is what gives it root-scoped tools and
        // lets it set the instance up (workflow.create) rather than only
        // answering a canned step.
        let (l, ask) = load(&args(&["--prompt", "do the thing"]), &base_env()).unwrap();
        assert_eq!(ask, Ask::Run);
        assert_eq!(l.settings.agent.prompt.as_deref(), Some("do the thing"));
        assert!(
            l.settings.workflows.is_empty(),
            "a prompt needs no workflow: {:?}",
            l.settings.workflows
        );

        // An instruction alone still gets the one-shot sugar workflow…
        let (only_instr, _) = load(&args(&["--instruction", "be terse"]), &base_env()).unwrap();
        assert_eq!(only_instr.settings.workflows.len(), 1);

        // …but a prompt alongside it means the prompt is the job: the
        // instruction stays standing policy, and no step is synthesized.
        let (both, _) = load(
            &args(&["--prompt", "do the thing", "--instruction", "be terse"]),
            &base_env(),
        )
        .unwrap();
        assert!(both.settings.workflows.is_empty());
        assert_eq!(both.settings.agent.instruction.as_deref(), Some("be terse"));

        // The env spelling works too (12-factor).
        let mut env = base_env();
        env.push(("AGENTD_AGENT_PROMPT".into(), "from env".into()));
        let (from_env, _) = load(&args(&[]), &env).unwrap();
        assert_eq!(from_env.settings.agent.prompt.as_deref(), Some("from env"));
    }

    #[test]
    fn minimal_instruction_run_gets_the_sugar_workflow() {
        let (l, ask) = load(&args(&["--instruction", "do it"]), &base_env()).unwrap();
        assert_eq!(ask, Ask::Run);
        assert_eq!(l.settings.agent.instruction.as_deref(), Some("do it"));
        assert_eq!(
            l.settings.intelligence.endpoints,
            vec!["https://intel.example/v1"]
        );
        assert_eq!(l.settings.workflows.len(), 1, "sugar workflow synthesized");
        assert_eq!(l.settings.workflows[0]["name"], json!("main"));
        assert_eq!(
            l.settings.workflows[0]["steps"]["start"]["kind"],
            json!("once")
        );
        // A one-shot job may run without a store — with a warning.
        assert!(
            l.warnings.iter().any(|w| w.contains("not durable")),
            "{:?}",
            l.warnings
        );
    }

    #[test]
    fn a_long_lived_instance_defaults_to_the_file_store_but_an_explicit_none_is_refused() {
        // An A2A listener is long-lived ⇒ the file store, not the old exit 2.
        let (l, _) = load(
            &args(&[
                "--instruction",
                "x",
                "--a2a.listen",
                "http://127.0.0.1:8443",
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(l.settings.store.kind, StoreKind::File);
        // A long-lived start node ⇒ the same default.
        let f = write_tmp(
            "config_version: \"2\"\nworkflows:\n  - name: w\n    steps:\n      s: {kind: schedule, cron: \"* * * * *\"}\n      f: {kind: finish, depends_on: [s], status: completed}\n",
            "yaml",
        );
        let (l, _) = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(l.settings.store.kind, StoreKind::File);
        // …but a STATED `none` is still refused: the default fills a silence, it
        // does not overrule an operator (RFC 0033 §5).
        let e = load(
            &args(&[
                "--config",
                f.path().to_str().unwrap(),
                "--store.kind",
                "none",
            ]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("long-lived"), "{e}");
        // A one-shot job keeps `none` — the default deliberately does not move
        // for the shape that can simply be re-run.
        let (l, _) = load(&args(&["--instruction", "x"]), &base_env()).unwrap();
        assert_eq!(l.settings.store.kind, StoreKind::None);
        // memory is accepted (with a warning).
        let (l, _) = load(
            &args(&["--instruction", "x", "--store.kind", "memory"]),
            &base_env(),
        )
        .unwrap();
        assert!(
            l.warnings.iter().any(|w| w.contains("memory")),
            "{:?}",
            l.warnings
        );
    }

    // ---- env substitution: `${VAR}` / `${VAR:-default}` --------------------

    #[test]
    fn expand_env_str_covers_the_forms() {
        let env: HashMap<&str, &str> = [("HOST", "db.internal"), ("PORT", "5432")]
            .into_iter()
            .collect();
        // A plain reference; multiple in one string.
        assert_eq!(
            expand_env_str("${HOST}:${PORT}", &env).unwrap(),
            "db.internal:5432"
        );
        // A default applies only when the variable is unset.
        assert_eq!(
            expand_env_str("${MISSING:-fallback}", &env).unwrap(),
            "fallback"
        );
        assert_eq!(
            expand_env_str("${HOST:-fallback}", &env).unwrap(),
            "db.internal"
        );
        // Braces are required: a bare `$VAR` and a lone `$` pass through.
        assert_eq!(
            expand_env_str("$HOST costs $5", &env).unwrap(),
            "$HOST costs $5"
        );
        // `$$` escapes to a literal `$` and does not open a reference.
        assert_eq!(expand_env_str("$${HOST}", &env).unwrap(), "${HOST}");
        // An unset variable with no default is a hard error (fail-closed).
        assert!(
            expand_env_str("${NOPE}", &env)
                .unwrap_err()
                .contains("NOPE")
        );
        // A malformed reference is rejected, not silently passed through.
        assert!(expand_env_str("${HOST", &env).is_err());
        assert!(expand_env_str("${bad-name}", &env).is_err());
    }

    #[test]
    fn config_vars_fold_typed_values_and_collect_every_miss() {
        let file = write_tmp(
            "config_version: \"2\"\n\
             vars:\n  region: eu-1\n  port: 8443\n  team:\n    name: platform\n\
             agent:\n  name: \"svc-{{config.region}}\"\n  instruction: serve\n  preflight: never\n\
             intelligence:\n  endpoints: [https://x/v1]\n  model: m\n\
             store:\n  kind: memory\n\
             limits:\n  step_timeout: \"{{config.port}}s\"\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {kind: once}\n\
             \x20     c: {kind: http, depends_on: [s], url: \"https://api.{{config.region}}.example\", headers: {x-team: \"{{config.team.name}}\"}}\n\
             \x20     f: {kind: finish, depends_on: [c]}\n",
            "yaml",
        );
        let (l, _) = load(
            &args(&["--config", file.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        // Embedded token: stringified into place, in config values…
        assert_eq!(l.settings.agent.name.as_deref(), Some("svc-eu-1"));
        // A folded scalar keeps its place in typed config too.
        assert_eq!(
            l.settings
                .limits
                .step_timeout
                .as_ref()
                .map(|d| d.0.as_secs()),
            Some(8443)
        );
        // Workflow docs are deliberately NOT folded here: `load_workflows`
        // folds them (so URL/file/dir sources get the identical treatment and
        // the definition hash pins the RESOLVED doc) — the e2e proves that leg.
        let wf = &l.settings.workflows[0];
        assert_eq!(
            wf.pointer("/steps/c/url").and_then(Value::as_str),
            Some("https://api.{{config.region}}.example")
        );

        // Every unresolved reference is reported, in ONE refusal.
        let bad = write_tmp(
            "config_version: \"2\"\n\
             vars:\n  set: yes\n\
             agent:\n  name: \"{{config.gone}}\"\n  instruction: serve\n  preflight: never\n\
             intelligence:\n  endpoints: [\"https://{{config.also_gone}}/v1\"]\n  model: m\n\
             store:\n  kind: memory\n",
            "yaml",
        );
        let err = load(
            &args(&["--config", bad.path().to_str().unwrap()]),
            &base_env(),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("config.gone"), "{err}");
        assert!(err.contains("config.also_gone"), "{err}");
        assert!(
            err.contains("2 unresolved config var reference"),
            "all misses in one report: {err}"
        );
    }

    #[test]
    fn missing_references_name_every_gap_with_its_locations() {
        let doc = serde_json::json!({
            "a": "{{secret:SUB_WINDOW_TEST_UNSET}}",
            "b": {"c": ["{{secret-file:/definitely/not/here}}", "{{config.gone}}"]},
            "d": "{{secret:SUB_WINDOW_TEST_UNSET}} again",
        });
        let vars: std::collections::BTreeMap<String, Value> =
            [("present".to_string(), serde_json::json!(1))].into();
        let missing = missing_references(&doc, "cfg", &vars);
        assert_eq!(missing.len(), 3, "{missing:?}");
        let all = missing.join("\n");
        assert!(
            all.contains("{{secret:SUB_WINDOW_TEST_UNSET}} is not set"),
            "{all}"
        );
        assert!(
            all.contains("cfg.a") && all.contains("cfg.d"),
            "both locations: {all}"
        );
        assert!(
            all.contains("{{secret-file:/definitely/not/here}} is not readable"),
            "{all}"
        );
        assert!(all.contains("config.gone is not defined in vars"), "{all}");
        // A resolvable reference is not noise.
        let ok = serde_json::json!({"x": "{{config.present}}"});
        assert!(missing_references(&ok, "cfg", &vars).is_empty());
    }

    #[test]
    fn env_substitution_reaches_config_values_and_workflows() {
        let file = write_tmp(
            "config_version: \"2\"\n\
             agent:\n  name: ${SVC_NAME}\n  instruction: serve\n  preflight: never\n\
             intelligence:\n  endpoints: [https://x/v1]\n  model: m\n\
             store:\n  kind: memory\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {kind: once}\n\
             \x20     c: {kind: http, depends_on: [s], url: \"https://api.${REGION:-us}.example/${SVC_NAME}\"}\n\
             \x20     f: {kind: finish, depends_on: [c]}\n",
            "yaml",
        );
        let mut env = base_env();
        env.push(("SVC_NAME".into(), "billing".into()));
        // REGION is deliberately unset -> the `:-us` default applies.
        let (l, _) = load(&args(&["--config", file.path().to_str().unwrap()]), &env).unwrap();
        // A plain config value is substituted.
        assert_eq!(
            l.settings.agent.name.as_deref(),
            Some("billing"),
            "the `${{SVC_NAME}}` in a config value was substituted"
        );
        // A value nested inside an inline workflow is substituted too, honouring
        // the `:-default` for the unset REGION and the set SVC_NAME.
        let url = l.settings.workflows[0]
            .pointer("/steps/c/url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            url, "https://api.us.example/billing",
            "the workflow value was substituted (default + set var)"
        );
    }

    #[test]
    fn mcp_server_oauth_is_carried_to_the_runtime_spec() {
        // RFC 0031: `mcp.servers[].oauth` was silently dropped by `to_spec()`,
        // leaving OAuth client-credentials inert. It must reach the runtime spec
        // (as a secret-free template) so the connect path can build the signer.
        let s = McpServer {
            name: "gh".into(),
            endpoint: "https://mcp.example".into(),
            service: None,
            ns: None,
            headers: BTreeMap::new(),
            tags: BTreeMap::new(),
            allow: None,
            exclude: Vec::new(),
            aauth: None,
            oauth: Some(McpOauth {
                token_url: "https://auth.example/token".into(),
                client_id: "cid".into(),
                client_secret: Secret("{{secret:CS}}".into()),
                scope: Some("mcp:read".into()),
            }),
            auth: None,
            timeout: None,
        };
        let spec = s.to_spec().unwrap();
        let o = spec.oauth.expect("oauth reaches the runtime spec");
        assert_eq!(o.token_url, "https://auth.example/token");
        assert_eq!(o.client_id, "cid");
        // The secret stays a template — never resolved into the spec/payload.
        assert_eq!(o.client_secret, "{{secret:CS}}");
        assert_eq!(o.scope.as_deref(), Some("mcp:read"));
    }

    #[test]
    fn files_env_flags_layer_in_order_with_aliases() {
        let base = write_tmp(
            "config_version: \"2\"\nagent:\n  instruction: from-file\nintelligence:\n  endpoints: [https://file.example/v1]\n  model: file-model\nlimits:\n  run:\n    steps: 10\nstore: { kind: memory }\n",
            "yaml",
        );
        let over = write_tmp("intelligence:\n  model: over-model\n", "yml");
        let mut env = base_env();
        env.clear();
        env.push(("AGENTD_LIMITS_RUN_STEPS".into(), "20".into())); // derived path name
        env.push(("AGENT_MODEL".into(), "env-model".into())); // legacy alias
        env.push(("INSTRUCTION".into(), "env-instruction".into())); // bare legacy alias
        let (l, _) = load(
            &args(&[
                "--config",
                base.path().to_str().unwrap(),
                "--config",
                over.path().to_str().unwrap(),
                "--max-steps",
                "30",
                "--mcp",
                "fs=https://fs.example/mcp",
                "--mcp-tags",
                "fs=sensitive",
                "--intelligence.headers.x-team",
                "ops",
            ]),
            &env,
        )
        .unwrap();
        let s = &l.settings;
        assert_eq!(
            s.agent.instruction.as_deref(),
            Some("env-instruction"),
            "env > file"
        );
        assert_eq!(
            s.intelligence.model.as_deref(),
            Some("env-model"),
            "env alias > later file"
        );
        assert_eq!(s.limits.run.steps(), 30, "flag alias > env");
        assert_eq!(s.mcp.servers.len(), 1);
        assert_eq!(s.mcp.servers[0].name, "fs");
        assert_eq!(s.mcp.servers[0].tags["*"], vec!["sensitive"]);
        assert_eq!(
            s.intelligence.headers.get("x-team").map(String::as_str),
            Some("ops")
        );
        assert_eq!(l.files.len(), 2);
        // Path env beats the legacy alias for the same field.
        let env2: Vec<(String, String)> = vec![
            ("AGENT_MODEL".into(), "legacy".into()),
            ("AGENTD_INTELLIGENCE_MODEL".into(), "path".into()),
            ("AGENTD_INTELLIGENCE_ENDPOINTS".into(), "https://i".into()),
        ];
        let (l2, _) = load(
            &args(&["--instruction", "x", "--store.kind", "memory"]),
            &env2,
        )
        .unwrap();
        assert_eq!(l2.settings.intelligence.model.as_deref(), Some("path"));
    }

    #[test]
    fn removed_flags_name_their_replacement() {
        for (flag, _) in REMOVED_FLAGS {
            let e = load(&args(&[flag, "x"]), &base_env()).unwrap_err();
            assert!(
                format!("{e}").contains("removed in agentd 2.0"),
                "{flag}: {e}"
            );
        }
        let e = load(&args(&["--mode", "reactive"]), &base_env()).unwrap_err();
        assert!(format!("{e}").contains("start node"), "{e}");
    }

    #[test]
    fn mixed_and_v1_files_are_refused_by_the_v2_loader() {
        let mixed = write_tmp("agent: {instruction: x}\nmodel: m\n", "yaml");
        let e = load(
            &args(&["--config", mixed.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("mixes v1"), "{e}");
        let v1 = write_tmp("model: m\n", "yaml");
        let e = load(
            &args(&["--config", v1.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("v1 schema"), "{e}");
    }

    #[test]
    fn budget_exit_code_and_instruction_file_aliases() {
        let f = write_tmp("read me from a file", "txt");
        let (l, _) = load(
            &args(&[
                "--instruction-file",
                f.path().to_str().unwrap(),
                "--budget-exit-code",
                "9",
                "--store.kind",
                "memory",
            ]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(
            l.settings.agent.instruction.as_deref(),
            Some("read me from a file")
        );
        assert_eq!(l.settings.lifecycle.exit_code_map.get("3"), Some(&9));
        assert_eq!(l.settings.lifecycle.exit_code_map.get("7"), Some(&9));
    }

    // ---- validation -----------------------------------------------------------

    fn load_doc(yaml: &str) -> Result<Loaded, ConfigError> {
        let f = write_tmp(yaml, "yaml");
        load(&args(&["--config", f.path().to_str().unwrap()]), &[]).map(|(l, _)| l)
    }

    #[test]
    fn validation_collects_the_rfc_0030_rules() {
        // A file with an inline credential is refused; the same value from env is fine.
        let e = load_doc(
            "config_version: \"2\"\nintelligence:\n  endpoints: [https://i]\n  token: sk-inline\n",
        )
        .unwrap_err();
        assert!(format!("{e}").contains("inline credential"), "{e}");
        let (l, _) = load(
            &args(&[
                "--intelligence",
                "https://i",
                "--intelligence-token",
                "sk-inline",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            l.settings.intelligence.token.as_ref().map(|s| s.0.as_str()),
            Some("sk-inline")
        );
        assert!(
            !format!("{:?}", l.settings).contains("sk-inline"),
            "Debug redacts"
        );

        // Undeclared servers referenced by tools/store/knowledge/skills: the
        // startup path fast-fails on the first problem (exit 2)…
        let e = load_doc(
            "config_version: \"2\"\nstore: {kind: mcp, mcp: {server: nope}}\nknowledge: {server: kb}\nskills: {sources: [{server: sk}]}\ntools: {overrides: {memory.get: {server: mem, tool: t}}, disabled: [memory.get]}\n",
        )
        .unwrap_err();
        assert!(matches!(e, ConfigError::Usage(_)), "{e}");

        // --validate-config collects EVERYTHING.
        let f = write_tmp(
            "config_version: \"2\"\nstore: {kind: mcp, mcp: {server: nope}}\nknowledge: {server: kb}\nskills: {sources: [{server: sk}]}\ntools: {overrides: {memory.get: {server: mem, tool: t}}, disabled: [memory.get]}\nlifecycle: {exit_code_map: {\"4\": 300}}\n",
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap(), "--validate-config"]),
            &[],
        )
        .unwrap_err();
        let ConfigError::Validate(Err(lines)) = e else {
            panic!("expected a validate verdict, got {e:?}")
        };
        for needle in [
            "store.mcp.server 'nope'",
            "knowledge.server 'kb'",
            "skills.sources[]",
            "tools.overrides['memory.get']",
            "both disabled and overridden",
            "only the policy codes 3 and 7",
            "0..=255",
        ] {
            assert!(lines.contains(needle), "missing {needle} in:\n{lines}");
        }

        // A2A listener rules.
        let e = load_doc("config_version: \"2\"\nstore: {kind: memory}\na2a: {listen: \"https://0.0.0.0:8443\"}\n").unwrap_err();
        assert!(format!("{e}").contains("a2a.tls.cert"), "{e}");
        let e = load_doc("config_version: \"2\"\nstore: {kind: memory}\na2a: {listen: \"http://0.0.0.0:8080\"}\n").unwrap_err();
        assert!(format!("{e}").contains("loopback"), "{e}");
        // Principals: `any` cannot be operator.
        let e = load_doc(
            "config_version: \"2\"\na2a: {principals: [{match: {any: true}, role: operator}]}\n",
        )
        .unwrap_err();
        assert!(format!("{e}").contains("operator role"), "{e}");
        // Budget rules.
        let e = load_doc("config_version: \"2\"\nintelligence: {budget: {windows: [{per: hour}], on_exhausted: degrade}}\n").unwrap_err();
        assert!(format!("{e}").contains("tokens and/or requests"), "{e}");
        // Trifecta over the root grant.
        let e = load_doc(
            "config_version: \"2\"\nmcp:\n  servers:\n    - {name: fs, endpoint: https://fs/mcp, tags: {\"*\": [untrusted_input, sensitive, egress]}}\n",
        )
        .unwrap_err();
        assert!(format!("{e}").contains("lethal-trifecta"), "{e}");
    }

    #[test]
    fn restart_only_diff_names_changed_paths() {
        let a = json!({"agent": {"name": "x", "instruction": "i"}, "store": {"kind": "mcp"}, "a2a": {"listen": "https://l"}});
        let b = json!({"agent": {"name": "y", "instruction": "j"}, "store": {"kind": "mcp"}, "a2a": {"listen": "https://l"}});
        assert_eq!(restart_only_diff(&a, &b), vec!["agent.name".to_string()]);
        let c = json!({"agent": {"name": "x", "instruction": "changed"}, "store": {"kind": "mcp"}, "a2a": {"listen": "https://l"}});
        assert!(
            restart_only_diff(&a, &c).is_empty(),
            "instruction is reloadable"
        );
    }

    #[test]
    fn duration_and_tool_select_scalars() {
        let s = Settings::from_document(
            json!({"limits": {"run": {"deadline": "90s"}, "step_timeout": 5}, "agent": {"tools": {"mcp": "none", "internal": ["memory.get"]}}}),
            "t",
        )
        .unwrap();
        assert_eq!(s.limits.run.deadline(), Duration::from_secs(90));
        assert_eq!(s.limits.step_timeout, Some(Dur(Duration::from_secs(5))));
        assert!(!s.agent.tools.mcp.allows("fs.read"));
        assert!(s.agent.tools.internal.allows("memory.get"));
        assert!(!s.agent.tools.internal.allows("finish"));
        assert!(s.agent.tools.code.allows("anything"));
        assert!(
            Settings::from_document(json!({"limits": {"run": {"deadline": "soon"}}}), "t").is_err()
        );
    }

    #[test]
    fn instruction_config_directives_define_the_agent_and_explicit_keys_win() {
        let instr = ":::config\nlimits: {max_runs: 9}\n:::\n\
                     :::stream{name=orders}\nretention: {max_events: 50}\n:::\n\
                     :::mcp{name=fs}\nendpoint: \"https://fs.internal/mcp\"\nexclude: [\"delete_*\"]\n:::\n\
                     Do the work.";
        let s = Settings::from_document(
            json!({"agent": {"instruction": instr}, "limits": {"max_runs": 3}}),
            "t",
        )
        .unwrap();
        assert_eq!(
            s.limits.max_runs,
            Some(3),
            "an explicit key beats the fragment"
        );
        assert_eq!(
            s.streams.get("orders").map(|c| c.max_events()),
            Some(50),
            "the fragment fills what the config left unsaid"
        );
        let srv = s
            .mcp
            .servers
            .iter()
            .find(|m| m.name == "fs")
            .expect("declared");
        assert_eq!(srv.endpoint, "https://fs.internal/mcp");
        assert_eq!(srv.exclude, vec!["delete_*"]);
        let cleaned = s.agent.instruction.as_deref().unwrap();
        assert!(cleaned.contains("Do the work."));
        assert!(
            !cleaned.contains("endpoint"),
            "machinery never reaches the model"
        );
        // A fragment with a bogus section is refused by the SAME deserializer
        // that guards the config file — no parallel, laxer path.
        assert!(
            Settings::from_document(
                json!({"agent": {"instruction": ":::config\nnot_a_section: 1\n:::\nx"}}),
                "t"
            )
            .is_err()
        );
    }

    // ---- the file store (RFC 0033) --------------------------------------------

    #[test]
    fn file_store_root_walks_the_chain_in_order() {
        use std::ffi::OsString;
        use std::path::PathBuf;
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |k: &str| -> Option<OsString> {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        let all = vec![
            ("AGENTD_STATE_DIR", "/state-dir"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/a"),
        ];
        let with_file = |path: Option<&str>| Store {
            file: Some(StoreFile {
                path: path.map(str::to_string),
                min_free: None,
            }),
            ..Store::default()
        };

        // 1. store.file.path wins over every environment variable.
        assert_eq!(
            file_store_root_in(&with_file(Some("/var/lib/agentd")), &env(all.clone())),
            PathBuf::from("/var/lib/agentd")
        );
        // 2. $AGENTD_STATE_DIR is taken verbatim — an operator naming the
        //    directory does not get `agentd/state` appended to it.
        assert_eq!(
            file_store_root_in(&with_file(None), &env(all.clone())),
            PathBuf::from("/state-dir")
        );
        // 3. $XDG_STATE_HOME, with the agentd/state suffix (`creds` sibling).
        assert_eq!(
            file_store_root_in(&Store::default(), &env(all[1..].to_vec())),
            PathBuf::from("/xdg/agentd/state")
        );
        // 4. $HOME/.local/state/… — the XDG default spelled out.
        assert_eq!(
            file_store_root_in(&Store::default(), &env(all[2..].to_vec())),
            PathBuf::from("/home/a/.local/state/agentd/state")
        );
        // 5. Last resort: the OS temp dir (non-durable; the runtime says so).
        assert_eq!(
            file_store_root_in(&Store::default(), &env(vec![])),
            std::env::temp_dir().join("agentd").join("state")
        );
        // The chain is the credential cache's, one sibling over: same order,
        // same suffix shape, `state` where `creds` is.
        assert!(
            file_store_root_in(&Store::default(), &env(all[1..].to_vec()))
                .ends_with("agentd/state")
        );
    }

    #[test]
    fn file_store_validation_diagnostics() {
        // `kind: file` needs no block at all.
        let l = load_doc("config_version: \"2\"\nstore: {kind: file}\n").unwrap();
        assert_eq!(l.settings.store.kind, StoreKind::File);
        assert!(validate(&l).errors.is_empty(), "{:?}", validate(&l).errors);
        // …and a long-lived instance is satisfied by it (no `store.kind is none`).
        let l = load_doc(
            "config_version: \"2\"\nstore: {kind: file, file: {path: /var/lib/agentd}}\na2a: {listen: \"http://127.0.0.1:8080\"}\n",
        )
        .unwrap();
        assert!(validate(&l).errors.is_empty(), "{:?}", validate(&l).errors);
        assert_eq!(
            file_store_root(&l.settings.store),
            std::path::PathBuf::from("/var/lib/agentd")
        );

        // An explicitly empty path would resolve to the working directory.
        let e = load_doc("config_version: \"2\"\nstore: {kind: file, file: {path: \"\"}}\n")
            .unwrap_err();
        assert!(format!("{e}").contains("store.file.path is empty"), "{e}");

        // A block belonging to an adapter that is not selected is dead config:
        // a warning (it is ignored), not a refusal (it does no harm).
        let l = load_doc(
            "config_version: \"2\"\nstore: {kind: memory, file: {path: /var/lib/agentd}}\n",
        )
        .unwrap();
        let d = validate(&l);
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("store.file is set but store.kind is memory")),
            "{:?}",
            d.warnings
        );
        // No warning when the file adapter IS the selected one.
        let l =
            load_doc("config_version: \"2\"\nstore: {kind: file, file: {path: /var/lib/agentd}}\n")
                .unwrap();
        assert!(
            !validate(&l)
                .warnings
                .iter()
                .any(|w| w.contains("store.file")),
            "{:?}",
            validate(&l).warnings
        );
        // Changing the state directory under a running instance is restart-only.
        assert_eq!(
            restart_only_diff(
                &json!({"store": {"kind": "file", "file": {"path": "/a"}}}),
                &json!({"store": {"kind": "file", "file": {"path": "/b"}}})
            ),
            vec!["store.file".to_string()]
        );
    }

    #[test]
    fn instruction_uri_detection() {
        assert!(looks_like_resource_uri("mcp://docs/agent-instruction"));
        assert!(looks_like_resource_uri("docs://agent"));
        assert!(!looks_like_resource_uri("You are a helpful agent."));
        assert!(!looks_like_resource_uri(
            "see https://x.example for details"
        ));
        assert!(!looks_like_resource_uri("://nope"));
    }

    #[test]
    fn help_and_schema_asks_short_circuit_validation() {
        let (_, ask) = load(&args(&["--help"]), &[]).unwrap();
        assert_eq!(ask, Ask::Help);
        let (_, ask) = load(&args(&["--config-schema=2"]), &[]).unwrap();
        assert_eq!(ask, Ask::Schema);
        // `--workflow-schema` is a static, side-effect-free dump: it must resolve
        // even with no config file present (no intelligence endpoint, etc.).
        let (_, ask) = load(&args(&["--workflow-schema"]), &[]).unwrap();
        assert_eq!(ask, Ask::WorkflowSchema);
        assert!(help_section().contains("intelligence.model"));
    }

    // ---- RFC 0037: service catalog & egress policy -------------------------

    const CATALOG: &str = "config_version: \"2\"\nstore: {kind: memory}\nservices:\n  billing:\n    endpoint: https://billing.example/mcp\n    auth: {kind: static, token: \"{{secret:BILLING}}\"}\n    headers: {X-Env: prod}\n    tags: {\"*\": [sensitive]}\n    allow: [charge_lookup, invoice_*]\n    exclude: [invoice_purge]\n";

    #[test]
    fn service_reference_inherits_and_narrows() {
        let f = write_tmp(
            &format!(
                "{CATALOG}mcp:\n  servers:\n    - {{name: money, service: billing, allow: [charge_lookup], ns: fin}}\n"
            ),
            "yaml",
        );
        let (loaded, _) = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        let s = &loaded.settings.mcp.servers[0];
        assert_eq!(s.endpoint, "https://billing.example/mcp", "inherited");
        assert!(s.auth.is_some(), "inherited auth");
        assert_eq!(s.headers["X-Env"], "prod", "inherited headers");
        assert_eq!(s.allow.as_deref(), Some(&["charge_lookup".to_string()][..]));
        assert_eq!(
            s.exclude,
            vec!["invoice_purge".to_string()],
            "exclude unions"
        );
        assert_eq!(s.tags["*"], vec!["sensitive"], "tag floor applied");
        assert_eq!(s.ns.as_deref(), Some("fin"), "consumer-local ns kept");
    }

    #[test]
    fn service_reference_without_allow_inherits_the_ceiling() {
        let f = write_tmp(
            &format!("{CATALOG}mcp:\n  servers:\n    - {{name: money, service: billing}}\n"),
            "yaml",
        );
        let (loaded, _) = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        let s = &loaded.settings.mcp.servers[0];
        assert_eq!(
            s.allow.as_deref(),
            Some(&["charge_lookup".to_string(), "invoice_*".to_string()][..]),
            "absent consumer allow inherits the catalog ceiling"
        );
    }

    #[test]
    fn service_reference_refuses_restated_connection_settings() {
        let f = write_tmp(
            &format!(
                "{CATALOG}mcp:\n  servers:\n    - {{name: money, service: billing, endpoint: \"https://other.example\"}}\n"
            ),
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("restates `endpoint`"), "{msg}");
    }

    #[test]
    fn service_allow_widening_is_refused() {
        let f = write_tmp(
            &format!(
                "{CATALOG}mcp:\n  servers:\n    - {{name: money, service: billing, allow: [refund_all]}}\n"
            ),
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("widens the ceiling"), "{msg}");
    }

    #[test]
    fn unknown_service_reference_is_refused() {
        let f = write_tmp(
            "config_version: \"2\"\nstore: {kind: memory}\nmcp:\n  servers:\n    - {name: x, service: nope}\n",
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("unknown service 'nope'"), "{e}");
    }

    #[test]
    fn tag_floor_applies_to_inline_matching_servers() {
        // Decision 3: an INLINE server pointing under a catalogued endpoint
        // gets the entry's tags unioned in — under-tagging cannot launder a
        // sensitive endpoint past the trifecta gate.
        let f = write_tmp(
            &format!(
                "{CATALOG}mcp:\n  servers:\n    - {{name: sneaky, endpoint: \"https://billing.example/mcp/sub\"}}\n"
            ),
            "yaml",
        );
        let (loaded, _) = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap();
        assert_eq!(
            loaded.settings.mcp.servers[0].tags["*"],
            vec!["sensitive"],
            "the catalog's tags are a floor for any matching endpoint"
        );
    }

    #[test]
    fn egress_closed_refuses_uncatalogued_and_admits_catalogued() {
        let f = write_tmp(
            &format!(
                "{CATALOG}security: {{egress: closed}}\nmcp:\n  servers:\n    - {{name: rogue, endpoint: \"https://rogue.example/mcp\"}}\n"
            ),
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("matches no services: catalog entry"), "{msg}");

        let ok = write_tmp(
            &format!(
                "{CATALOG}security: {{egress: closed}}\nmcp:\n  servers:\n    - {{name: money, service: billing}}\n"
            ),
            "yaml",
        );
        load(
            &args(&["--config", ok.path().to_str().unwrap()]),
            &base_env(),
        )
        .expect("a catalogued reference passes closed egress");
    }

    #[test]
    fn ambiguous_catalog_endpoints_are_refused() {
        let f = write_tmp(
            "config_version: \"2\"\nstore: {kind: memory}\nservices:\n  a: {endpoint: \"https://s.example/mcp\"}\n  b: {endpoint: \"https://s.example/mcp/deeper\"}\n",
            "yaml",
        );
        let e = load(
            &args(&["--config", f.path().to_str().unwrap()]),
            &base_env(),
        )
        .unwrap_err();
        assert!(format!("{e}").contains("prefix-comparable"), "{e}");
    }

    #[test]
    fn service_match_respects_segment_boundaries() {
        let mut services = BTreeMap::new();
        services.insert(
            "a".to_string(),
            Service {
                kind: ServiceKind::Mcp,
                endpoint: "https://s.example/api".into(),
                headers: BTreeMap::new(),
                tags: BTreeMap::new(),
                allow: None,
                exclude: Vec::new(),
                auth: None,
                rate: None,
                timeout: None,
            },
        );
        assert!(service_match(&services, "https://s.example/api").is_some());
        assert!(service_match(&services, "https://s.example/api/v2").is_some());
        assert!(
            service_match(&services, "https://s.example/apiary").is_none(),
            "prefix match is on segment boundaries, not string prefixes"
        );
        assert!(service_match(&services, "https://other.example/api").is_none());
        assert!(
            service_match(&services, "http://s.example/api").is_none(),
            "scheme must match"
        );
    }

    #[test]
    fn pattern_subsumption_covers_the_glob_grammar() {
        assert!(pattern_subsumes("charge_lookup", "charge_lookup"));
        assert!(pattern_subsumes("charge_lookup", "charge_*"));
        assert!(pattern_subsumes("charge_*", "charge_*"));
        assert!(pattern_subsumes("charge_x_*", "charge_*"));
        assert!(!pattern_subsumes("charge_*", "charge_lookup"));
        assert!(!pattern_subsumes("refund_all", "charge_*"));
        assert!(pattern_subsumes("anything", "*"));
    }
}
