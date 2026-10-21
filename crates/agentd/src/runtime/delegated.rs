// SPDX-License-Identifier: AGPL-3.0-only
//! Delegated servers as the runtime serves them (RFC 0044 P1): the calls
//! that come back to the supervisor, the person's consent gates, and what
//! the delegation supervisor tells the loop.
//!
//! **A call** to a delegated server's tool is internal whatever child made
//! it: no worker is handed the server. The loop checks who the work is for
//! and their grant with no network ([`crate::delegation::Delegation::admit`]),
//! and asks the person when a rule below says so; the mint and the call then
//! run on a thread of their own. What the server answers reaches the asking
//! unit carrying the origin `delegated:<server>` — and, unless the operator
//! declared `auth.trusted_output`, its taint — written before the result is
//! read (`runtime::taint`).
//!
//! **Consent** (§5.10, §5.13 item 6). Every call is held, after the
//! operator's `security.policies`, to the person's consent rules agentd
//! ships (`sec::policy::consent_rules`), on the facts of the call and of the
//! work making it ([`Runtime::policy_facts`]): an `egress` call from tainted
//! work holding the person's data, a delegated `sensitive` or `egress` call
//! from tainted work, a command from work holding the data, a delegated
//! `egress` call from a document's workflow its template does not list, and
//! a delegated `egress` call at its first use per run and tool. A workflow
//! step that sends the run's data out by its own effect rather than a tool
//! call meets them as an `egress` call: an `http` call, an `a2a.send` or
//! `a2a.delegate`, an `emit` that forwards the event out, an `mcp.tool` or
//! `mcp.resource` at a server whose tools are tagged `egress`
//! ([`Runtime::step_consent`]); approved, the step runs again past the gate
//! with the spec the person saw. An operator's own `to: principal` rule
//! asks the person the same way.
//!
//! A gate is a [`PendingKind::Consent`]: addressed to the person, answered
//! by them — over the unix A2A listener (`agentd --delegation-approve
//! <gate>` / `--delegation-deny <gate>`, `via: operator_self`), or through a
//! display session recently signed in at the IdP (`via: gate`) — or by an
//! operator their template lists among its approvers; bound to its `nonce`
//! and the hash of the call's canonical arguments, and never answered by a
//! judge, by anyone else, or by a message on the asking task (refused
//! there, the gate left open). Gates of one shape — rule, server, tool and
//! argument names alike — may be answered together, each still bound to
//! its own nonce and hash. At most `identity.delegation.consent_per_minute`
//! gates open for one person a minute; a call past that is refused, never
//! asked. What the person reads is agentd's text ([`gate_text`]): its own
//! lines, the values the work chose quoted and escaped, the arguments in
//! full up to a cap with their length, whether the work is tainted and by
//! what. Unanswered within `identity.delegation.consent_timeout` it denies.
//! Each decision is an audit-chain record whose personal content — what the
//! person was shown — is sealed under their personal key (§6.4), so
//! forgetting them leaves it unreadable and the chain whole. A workflow
//! step's gate is kept in the step's wait record and re-armed at the next
//! start; a turn's dies with its turn. An approval of a first use is
//! remembered for the unit, the tool, the rule and the gate's shape —
//! durably, so a restart does not ask again — bound to the grants the work
//! acts with and the templates that allow them as they were then: a
//! revoked or re-enrolled grant, or a changed template, drops it. It lives
//! a day at most, and goes with its run.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::events::Event;
use super::reactor::{PendingKind, PolicyCall, Runtime, Target};
use super::tools::{ExecTarget, ToolCaller, ToolOutcome};
use crate::delegation::{Call, Delegation, Note};
use crate::state::now_ms;

/// The origin a delegated result carries into the unit it reaches.
pub(crate) fn origin(server: &str) -> String {
    format!("{}{server}", crate::engine::run::DELEGATED)
}

/// What a delegated server's result hands the unit it reaches: the person's
/// data, and — unless the operator vouched for what the server answers —
/// taint, since anyone can write a Linear issue body.
pub(crate) fn read_carries(server: &str, trusted: bool) -> crate::engine::run::Carries {
    let mut c = crate::engine::run::Carries::default();
    c.add([origin(server)], || {
        format!("{server}'s delegated results are the person's own data")
    });
    if !trusted {
        c.taint([origin(server)]);
    }
    c
}

/// The longest stretch of a call's arguments a gate's text shows; the rest
/// is in the person's `delegation.list`.
pub(crate) const ARGS_SHOWN: usize = 4000;

/// What each consent rule asks about, in the gate's words.
fn rule_words(rule: &str) -> String {
    match rule {
        "egress_from_tainted_work_holding_delegated_data" => {
            "the work holds your delegated data and has read outside text, and this call sends data off the box".into()
        }
        "delegated_call_from_tainted_work" => {
            "this call acts with your grant at a sensitive or egress tool, from work that has read outside text".into()
        }
        "exec_holding_delegated_data" => {
            "the work holds your delegated data, and this call runs a command on the box, which can send it anywhere".into()
        }
        "document_sourced_delegated_egress" => {
            "this call sends data off the box with your grant, from a workflow an instruction document defined that your template does not list".into()
        }
        "delegated_egress" => {
            "this call sends data off the box with your grant, its first use of this tool in this work".into()
        }
        r if r.starts_with("security.policies[") => {
            format!("the operator's rule {r} asks for your consent before this call")
        }
        _ => "a consent rule asks".into(),
    }
}

/// The key a first use the person approved is remembered by: the unit of
/// work, the tool, the rule (a shipped one's name, an operator's by its
/// content — [`Runtime::policy_rule_id`]) and the gate's shape — the names
/// of the call's arguments, never their values.
pub(crate) fn once_key(unit: &str, tool: &str, rule: &str, args: &Value) -> String {
    let mut names: Vec<&str> = args
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    names.sort_unstable();
    let shape = json!({"rule": rule, "tool": tool, "args": names});
    let shape = crate::sha::sha256_hex(shape.to_string().as_bytes());
    format!("{unit}|{tool}|{rule}|{}", &shape[..16])
}

/// Where a unit's remembered first uses are kept, in the store.
const ONCE_STORE: &str = "delegation/once/";
/// The most first uses kept for one unit.
const ONCE_PER_UNIT: usize = 256;

/// The store record of `unit`'s remembered first uses.
fn once_record_id(unit: &str) -> String {
    format!(
        "{ONCE_STORE}{}",
        &crate::sha::sha256_hex(unit.as_bytes())[..32]
    )
}

/// The unit of work a delegated call is made in, as its budget and a
/// step-up's sign-in count it (`delegation::Call::unit`): the run, or the
/// conversation.
pub(crate) fn call_unit(caller: &ToolCaller) -> String {
    caller
        .run
        .clone()
        .unwrap_or_else(|| format!("ctx:{}", caller.context_id()))
}

/// The unit of work a call is made in, as a first use is counted: the run,
/// the subagent, or the conversation.
pub(crate) fn unit_of(caller: &ToolCaller) -> String {
    match (&caller.subagent, &caller.run) {
        (Some(h), _) => format!("subagent:{h}"),
        (None, Some(r)) => format!("run:{r}"),
        _ => format!("ctx:{}", caller.context_id()),
    }
}

/// The shape of a gate, which gates answered together share: its rule,
/// server and tool, and the names of its arguments — never their values.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
pub(crate) fn shape_of(rendered: &Value) -> String {
    let mut names: Vec<&str> = rendered["args"]
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    names.sort_unstable();
    let s = json!({"rule": rendered["rule"], "server": rendered["server"],
                   "tool": rendered["tool"], "args": names});
    crate::sha::sha256_hex(s.to_string().as_bytes())[..16].to_string()
}

/// How long a remembered first use is kept, at most.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
const ONCE_KEPT_MS: u64 = 24 * 60 * 60 * 1000;
/// The most first uses remembered at once.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
const ONCE_CAP: usize = 4096;
/// How often the store's first-use records are swept of the units that
/// can use none of theirs any more.
const ONCE_SWEEP_EVERY_MS: u64 = 60 * 60 * 1000;

/// The first-use records in `d` no unit can still use at `now`: each whose
/// every approval is older than [`ONCE_KEPT_MS`]. A run's go when it ends;
/// a conversation's or a subagent's that outlived its day go here.
fn stale_once_records(d: &crate::state::Durable, now: u64) -> Vec<String> {
    let Ok(list) = d.list(crate::state::Kind::Cred) else {
        return Vec::new();
    };
    list.into_iter()
        .filter_map(|ks| {
            let (_, id) = crate::store::parse_key(d.prefix(), d.instance(), &ks.key)?;
            id.starts_with(ONCE_STORE).then(|| id.to_string())
        })
        .filter(|id| {
            d.get(crate::state::Kind::Cred, id)
                .ok()
                .flatten()
                .and_then(|e| e.state.get("approved").cloned())
                .and_then(|a| {
                    serde_json::from_value::<std::collections::BTreeMap<String, (u64, String)>>(a)
                        .ok()
                })
                .is_none_or(|kept| {
                    kept.values()
                        .all(|(at, _)| now.saturating_sub(*at) >= ONCE_KEPT_MS)
                })
        })
        .collect()
}

/// What the person reads of gate `id`, rendered `r` (RFC 0044 §5.10 rule
/// 5, T29): every line agentd's own, each value the work chose — a server,
/// a tool, a scope, where it came from, the arguments — quoted and escaped
/// to printable ASCII (`sec::render`), the arguments in full up to
/// [`ARGS_SHOWN`] characters with their length. Never markdown, never a
/// template a value is put into.
pub(crate) fn gate_text(id: &str, r: &Value) -> String {
    use crate::sec::render::{ascii_json, capped, quoted};
    let s = |k: &str| r[k].as_str().map(|v| quoted(v, 128));
    let mut lines = vec![
        "agentd security gate: your consent is asked before this call. Every word here is agentd's but the quoted values, which the work chose.".to_string(),
        format!("Why: {}.", rule_words(r["rule"].as_str().unwrap_or_default())),
    ];
    if let Some(v) = s("server") {
        lines.push(format!("Server: {v}"));
    }
    lines.push(format!("Tool: {}", s("tool").unwrap_or_default()));
    if r["document_sourced"] == true {
        lines.push(format!(
            "Workflow: {}, defined by an instruction document",
            s("workflow").unwrap_or_default()
        ));
    }
    if let Some(sc) = r["scope"].as_array() {
        let shown: Vec<String> = sc
            .iter()
            .filter_map(Value::as_str)
            .map(|v| quoted(v, 64))
            .collect();
        lines.push(format!("Scopes the token carries: {}", shown.join(", ")));
    }
    match (s("run"), s("step"), s("context")) {
        (Some(run), Some(step), _) => lines.push(format!("Run: {run}, step {step}")),
        (Some(run), None, _) => lines.push(format!("Run: {run}")),
        (None, _, Some(ctx)) => lines.push(format!("Conversation: {ctx}")),
        _ => {}
    }
    let sources: Vec<String> = r["taint"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|v| quoted(v, 96))
        .collect();
    lines.push(if r["tainted"] == true {
        format!(
            "Tainted: yes, the work has read outside text: {}",
            sources.join(", ")
        )
    } else {
        "Tainted: no".into()
    });
    lines.push(format!(
        "Holds your delegated data: {}",
        if r["holds_delegated_data"] == true {
            "yes"
        } else {
            "no"
        }
    ));
    let args = ascii_json(&r["args"]);
    let (shown, whole) = capped(&args, ARGS_SHOWN);
    lines.push(format!(
        "Arguments ({} bytes{}):",
        r["args_length"].as_u64().unwrap_or(0),
        if whole {
            String::new()
        } else {
            format!(
                ", the first {ARGS_SHOWN} characters shown; agentd --delegation-list holds them all"
            )
        }
    ));
    lines.push(if whole {
        shown.to_string()
    } else {
        format!("{shown}...")
    });
    lines.push(format!(
        "Answer by approving or denying gate {id} in your display client, or with agentd --delegation-approve {id} or --delegation-deny {id}. No message is an answer."
    ));
    lines.join("\n")
}

/// Who answers a consent gate, and through what (RFC 0044 §5.10 rules 1,
/// 2 and 7): the person through a display session whose own IdP sign-in is
/// recent enough (`gate`), a per-person operator over the unix listener
/// (`operator_self`), or an operator the person's template lists among its
/// `approvers` (`operator`).
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answerer {
    pub id: String,
    pub via: &'static str,
}

/// A delegated server's connection waiting on its person's answer.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
pub(crate) struct ElicitWait {
    pub reply: std::sync::mpsc::SyncSender<Option<Value>>,
    /// The grant the connection was made with, which the answerer must
    /// hold.
    pub grant: String,
}

/// One open consent gate.
#[derive(Debug, Clone)]
pub(crate) struct ConsentGate {
    pub nonce: String,
    pub args_hash: String,
    /// What the person is shown: agentd's own words, as data.
    pub rendered: Value,
    /// Who answers it.
    pub principal: String,
    pub deadline_ms: u64,
    pub call: PolicyCall,
    pub target: Target,
    /// A workflow step's own effect, not a tool call: approved, the step
    /// runs again with `call.args` as its spec.
    pub effect: bool,
    /// Whether the person's template lists `approvers: [principal,
    /// operator]`: an operator may then answer, recorded `via: operator`.
    pub operator_approves: bool,
    /// The A2A task the gate is shown on, when the work is a caller's own:
    /// `input-required`, badged, naming the gate, until it closes.
    pub task: Option<String>,
    /// A step-up sign-in the gate asks for in place of an approval
    /// (`runtime::stepup`).
    pub signin: Option<super::stepup::SignIn>,
    /// The RFC 0043 session the work acts through: once it has ended —
    /// revoked, expired, or lost to a restart — the gate closes `revoked`
    /// at the next pass, since no answer could make the call.
    pub session: Option<String>,
    /// The first use an approval stands for, when the rule asks at the
    /// first use per run and tool alone ([`once_key`]).
    pub once: Option<String>,
    /// What that approval is bound to ([`Runtime::once_binding`]), as the
    /// gate opened.
    pub once_binding: Option<String>,
}

/// What a gate about to open asks under.
struct Ask {
    /// The rule's name: a shipped one's, or `security.policies[<i>]`.
    rule: String,
    /// The delegated server the call goes to, when it does.
    server: Option<String>,
    /// The gates the call passed before this one asked.
    passed: super::reactor::Passed,
    /// A workflow step's own effect, not a tool call.
    effect: bool,
    /// The first use an approval stands for.
    once: Option<String>,
    /// The rule's own `timeout`, over `identity.delegation.consent_timeout`.
    timeout_ms: Option<u64>,
}

/// The hash a gate is bound to: SHA-256 of `{tool, args}` as compact JSON,
/// whose object keys `serde_json` keeps sorted — so the same call hashes
/// alike however its arguments were ordered.
pub(crate) fn args_hash(tool: &str, args: &Value) -> String {
    crate::sha::sha256_hex(json!({"tool": tool, "args": args}).to_string().as_bytes())
}

pub(crate) fn target_of(caller: &ToolCaller) -> Target {
    match (caller.node, &caller.run, &caller.step) {
        (Some(node), _, _) => Target::Child(node, caller.req),
        (None, Some(run), Some(step)) => Target::Step(run.clone(), step.clone()),
        _ => Target::Child(crate::supervisor::tree::NodeId(0), caller.req),
    }
}

/// The context a call's result is read into.
fn reads_of(caller: &ToolCaller) -> Option<super::companies::Reads> {
    use super::companies::Reads;
    if let Some(h) = &caller.subagent {
        return Some(Reads::Child(h.clone()));
    }
    if let Some(r) = &caller.run {
        return Some(Reads::Run(r.clone()));
    }
    (caller.node.is_some() || caller.ctx.is_some())
        .then(|| Reads::Conversation(caller.context_id()))
}

impl Runtime {
    /// Whether `server` is a delegated server.
    pub(crate) fn is_delegated_server(&self, server: &str) -> bool {
        self.delegation
            .as_ref()
            .is_some_and(|d| d.is_delegated(server))
    }

    /// The work behind `caller`, admitted for `server` as
    /// [`Delegation::admit`] decides — and a subagent's, its own call or a
    /// run it started, directly or through others, only where the person's
    /// template says `subagents: inherit` and the subagent itself declared
    /// `server` (RFC 0044 §5.12 F): it acts with their grant, intersected
    /// with what it declares, and holds no token — the supervisor makes the
    /// call. Otherwise `delegation_not_permitted`.
    pub(crate) fn delegation_admit(
        &self,
        d: &Delegation,
        caller: &ToolCaller,
        server: &str,
    ) -> Result<crate::delegation::Admitted, crate::delegation::DelegationError> {
        let not = |message: String| crate::delegation::DelegationError {
            code: crate::delegation::Code::DelegationNotPermitted,
            message,
        };
        // A revoked document makes no delegated call (T27): while the
        // instruction is frozen — its source not confirmed current before
        // its deadline — nothing it wrote acts with a person's grant: no
        // model turn or subagent, whose prompt holds it; no run a model
        // asked for, whose inputs are the model's; and no run of a workflow
        // it defined, nor a subagent such a run spawned.
        let steered = self.freshness_frozen
            && (caller.node.is_some()
                || self.subagent_lineage(caller).is_some()
                || self.originating_run(caller).is_some_and(|r| {
                    r.by_model
                        || self
                            .definition_for_run_ref(&r.workflow, &r.workflow_hash)
                            .is_some_and(|w| w.document)
                }));
        if steered {
            return Err(crate::delegation::DelegationError {
                code: crate::delegation::Code::PolicyDenied,
                message: format!(
                    "the instruction document could not be confirmed current (agent.instruction.trust freshness): nothing it wrote acts with a person's grant at {server} until it is"
                ),
            });
        }
        let admitted = d.admit(caller.acting(), server);
        let Some(h) = self.subagent_lineage(caller) else {
            return admitted;
        };
        let admitted = admitted?;
        if admitted.template.subagents
            != crate::config::settings::delegation::SubagentsMode::Inherit
        {
            return Err(not(format!(
                "subagent:{h}'s work acts with no one's delegated grant: template {} says subagents: none",
                admitted.template.name
            )));
        }
        let declared = self
            .subagents
            .get(&h)
            .is_some_and(|s| s.delegated_servers.iter().any(|x| x == server));
        if !declared {
            return Err(not(format!(
                "subagent:{h} does not declare {server} among its servers, so it acts with no grant there"
            )));
        }
        Ok(admitted)
    }

    /// Whether `server`'s tools are offered to work `acting` is for: only
    /// with a grant usable through the session it acts through.
    pub(crate) fn delegation_usable(
        &self,
        acting: crate::delegation::Acting<'_>,
        server: &str,
    ) -> bool {
        self.delegation
            .as_ref()
            .is_some_and(|d| d.usable(acting, server))
    }

    /// The delegated server of tool `name`, when it is one.
    fn delegated_server_of(&self, name: &str) -> Option<String> {
        match self.registry.route(name)? {
            crate::registry::Route::Mcp { server, .. } if self.is_delegated_server(server) => {
                Some(server.to_string())
            }
            crate::registry::Route::Mapped(m) if self.is_delegated_server(&m.server) => {
                Some(m.server.clone())
            }
            _ => None,
        }
    }

    /// The templates of the grants `caller`'s work acts with, when it acts
    /// with any (RFC 0044 §5.3): the person's, through the session the work
    /// acts through — and for a subagent's work, or a run one started, only
    /// those that say `subagents: inherit`.
    pub(crate) fn work_templates(
        &self,
        caller: &ToolCaller,
    ) -> Option<Vec<crate::config::settings::delegation::Template>> {
        let mut t = self
            .delegation
            .as_ref()?
            .acting_templates(caller.acting())?;
        if self.subagent_lineage(caller).is_some() {
            t.retain(|t| {
                t.subagents == crate::config::settings::delegation::SubagentsMode::Inherit
            });
        }
        (!t.is_empty()).then_some(t)
    }

    /// The agent-mode MCP server tool `name` is served by, when it is one —
    /// not a delegated server, nor a Company's, whose tools act for its
    /// User alone.
    pub(crate) fn agent_mode_server_of(&self, name: &str) -> Option<String> {
        let server = match self.registry.route(name)? {
            crate::registry::Route::Mcp { server, .. } => server.to_string(),
            crate::registry::Route::Mapped(m) => m.server.clone(),
            _ => return None,
        };
        (!self.is_delegated_server(&server) && !self.is_company_server(&server)).then_some(server)
    }

    /// Whether agent-mode `server` — its tool `name`, or for a step any of
    /// its tools — is hidden from work acting with `templates` (RFC 0044
    /// §5.3, T10): tagged `sensitive` or `egress`, and listed in none of
    /// their `agent_mode_servers`. Agentd's own credential there is broader
    /// than the person's grant, and a steered model would reach for it.
    pub(crate) fn agent_mode_hidden(
        &self,
        templates: &[crate::config::settings::delegation::Template],
        server: &str,
        name: Option<&str>,
    ) -> bool {
        use crate::sec::scope::TrifectaTag;
        if templates
            .iter()
            .any(|t| t.agent_mode_servers.iter().any(|s| s == server))
        {
            return false;
        }
        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => self
                .registry
                .iter()
                .filter(|t| t.server.as_deref() == Some(server))
                .map(|t| t.name.clone())
                .collect(),
        };
        let mut tags = self.registry.tags_of(&names);
        if name.is_none()
            && let Some(declared) = self
                .settings
                .mcp
                .servers
                .iter()
                .find(|s| s.name == server)
                .and_then(|s| s.tag_set().ok())
        {
            tags.extend(declared);
        }
        tags.iter()
            .any(|t| matches!(t, TrifectaTag::Sensitive | TrifectaTag::Egress))
    }

    /// A call of agent-mode `server` from `caller`'s work, which acts with
    /// an enrolled person's grant: refused when the server is hidden from it
    /// (`mode_mixing`), else recorded as such (`delegation.agent_mode_call`)
    /// and let through. `None` for work acting with nobody's grant. `name`
    /// is the tool, or `None` for a step's `mcp.tool` or `mcp.resource`.
    pub(crate) fn mode_mixing(
        &self,
        caller: &ToolCaller,
        server: &str,
        name: Option<&str>,
        what: &str,
    ) -> Option<String> {
        let templates = self.work_templates(caller)?;
        let principal = caller.principal.as_deref().unwrap_or_default();
        if self.agent_mode_hidden(&templates, server, name) {
            let target = json!({"server": server, "tool": what, "principal": principal,
                                "run": caller.run, "step": caller.step, "why": "mode_mixing"});
            self.log
                .warn("delegation.agent_mode.refused", target.clone());
            self.audit(super::audit::AuditEvent {
                action: "delegation.agent_mode_call",
                target,
                outcome: "refused",
                principal: Some(principal),
                role: None,
                request_id: None,
                sid: None,
                trace: self.caller_trace(caller).as_ref(),
                link: None,
            });
            return Some(format!(
                "{what}: refused (mode_mixing): {server} is an agent-mode server tagged sensitive or egress, and this work acts with {principal}'s delegated grant — agentd's own credential there is hidden from it unless the person's template lists {server} in agent_mode_servers"
            ));
        }
        self.audit(super::audit::AuditEvent {
            action: "delegation.agent_mode_call",
            target: json!({"server": server, "tool": what, "principal": principal,
                           "run": caller.run, "step": caller.step}),
            outcome: "admitted",
            principal: Some(principal),
            role: None,
            request_id: None,
            sid: None,
            trace: self.caller_trace(caller).as_ref(),
            link: None,
        });
        None
    }

    /// What a first use `caller`'s work approved is bound to: the grants it
    /// acts with, their epochs, and the templates that admit its person, as
    /// they are now.
    fn once_binding(&self, caller: &ToolCaller) -> String {
        self.delegation
            .as_ref()
            .and_then(|d| d.acting_binding(caller.acting()))
            .unwrap_or_default()
    }

    /// Whether `caller`'s unit is past the first use of `name` with `args`
    /// that `rule` asks about: the person approved one of that shape, within
    /// a day, under the grants and templates the work acts with now — read
    /// from memory, or after a restart from the store.
    fn once_passed(&self, caller: &ToolCaller, name: &str, rule: &str, args: &Value) -> bool {
        let unit = unit_of(caller);
        let key = once_key(&unit, name, rule, args);
        let binding = self.once_binding(caller);
        let now = now_ms();
        let fresh = |at: u64, b: &str| now.saturating_sub(at) < ONCE_KEPT_MS && b == binding;
        if let Some((at, b)) = self.consented_once.get(&key) {
            return fresh(*at, b);
        }
        self.stored_once(&unit)
            .get(&key)
            .is_some_and(|(at, b)| fresh(*at, b))
    }

    /// `unit`'s remembered first uses, as the store keeps them.
    fn stored_once(&self, unit: &str) -> std::collections::BTreeMap<String, (u64, String)> {
        self.durable
            .get(crate::state::Kind::Cred, &once_record_id(unit))
            .ok()
            .flatten()
            .and_then(|e| e.state.get("approved").cloned())
            .and_then(|a| serde_json::from_value(a).ok())
            .unwrap_or_default()
    }

    /// Remember the first use gate `g` stood for, approved: in memory, and
    /// in the store under its unit, with what it is bound to.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    fn remember_once(&mut self, g: &ConsentGate) {
        let (Some(k), Some(binding)) = (&g.once, &g.once_binding) else {
            return;
        };
        let now = now_ms();
        self.consented_once
            .retain(|_, (at, _)| now.saturating_sub(*at) < ONCE_KEPT_MS);
        if self.consented_once.len() >= ONCE_CAP
            && let Some(oldest) = self
                .consented_once
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            self.consented_once.remove(&oldest);
        }
        self.consented_once
            .insert(k.clone(), (now, binding.clone()));
        let unit = k.split('|').next().unwrap_or_default().to_string();
        let mut kept = self.stored_once(&unit);
        kept.retain(|_, (at, _)| now.saturating_sub(*at) < ONCE_KEPT_MS);
        while kept.len() >= ONCE_PER_UNIT {
            let Some(oldest) = kept
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            kept.remove(&oldest);
        }
        kept.insert(k.clone(), (now, binding.clone()));
        if let Err(e) = self.durable.put(
            crate::state::Kind::Cred,
            &once_record_id(&unit),
            json!({"unit": unit, "approved": kept}),
            None,
        ) {
            self.log.warn(
                "delegation.consent.once_unsaved",
                json!({"unit": unit, "err": e.to_string(),
                       "msg": "the approval holds in memory, and a restart asks again"}),
            );
        }
    }

    /// At most once every [`ONCE_SWEEP_EVERY_MS`] — the first pass of a
    /// life included — drop the stored first-use records no unit can still
    /// use.
    pub(crate) fn sweep_once(&mut self) {
        let now = now_ms();
        if self.once_swept_ms != 0 && now.saturating_sub(self.once_swept_ms) < ONCE_SWEEP_EVERY_MS {
            return;
        }
        self.once_swept_ms = now;
        for id in stale_once_records(&self.durable, now) {
            let _ = self.durable.delete(crate::state::Kind::Cred, &id);
        }
    }

    /// Forget every first use `unit` approved: its work ended.
    pub(crate) fn forget_once(&mut self, unit: &str) {
        let prefix = format!("{unit}|");
        let before = self.consented_once.len();
        self.consented_once.retain(|k, _| !k.starts_with(&prefix));
        let id = once_record_id(unit);
        if before != self.consented_once.len()
            || self
                .durable
                .get(crate::state::Kind::Cred, &id)
                .ok()
                .flatten()
                .is_some()
        {
            let _ = self.durable.delete(crate::state::Kind::Cred, &id);
        }
    }

    /// An operator rule's identity in a remembered first use: its content,
    /// so a reload that moves the rule keeps what was approved under it and
    /// one that changes it asks again.
    pub(crate) fn policy_rule_id(&self, i: usize) -> String {
        match self.settings.security.policies.get(i) {
            Some(p) => format!(
                "security.policies#{}",
                &crate::sha::sha256_hex(format!("{p:?}").as_bytes())[..16]
            ),
            None => format!("security.policies[{i}]"),
        }
    }

    /// The person's consent before `name`, after every rule of the
    /// operator's (§5.13 item 6): `None` to go on, or the gate (or the
    /// refusal) the first of the shipped consent rules it meets opens — on
    /// the facts of the call and of the work making it. A delegated call
    /// that could not be made at all is refused here, before anyone is
    /// asked.
    pub(crate) fn consent_check(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        passed: super::reactor::Passed,
    ) -> Option<ToolOutcome> {
        let d = self.delegation.clone()?;
        // A firing sends nothing off the box with the person's grant unless
        // their template lists the very tool (RFC 0044 T18).
        if let Some(refused) = self.unattended_egress(&d, caller, name) {
            return Some(refused);
        }
        let tags = self
            .registry
            .tags_of(std::slice::from_ref(&name.to_string()));
        let facts = self.policy_facts(caller, name);
        let call = crate::sec::policy::Call {
            tool: name,
            tags: &tags,
            caller: Self::policy_caller(caller),
            principal: caller.principal.as_deref(),
            args,
            also: None,
            facts,
        };
        let (rule, once) =
            crate::sec::policy::consent_rule(&call, &|r| self.once_passed(caller, name, r, args))?;
        let server = self.delegated_server_of(name);
        if let Some(s) = &server
            && let Err(e) = self.delegation_admit(&d, caller, s)
        {
            return Some(ToolOutcome::Ready(
                Value::String(format!("{name}: {e}")),
                true,
            ));
        }
        let once = once.then(|| once_key(&unit_of(caller), name, rule, args));
        Some(self.open_consent(
            &d,
            caller,
            name,
            args,
            Ask {
                rule: rule.to_string(),
                server,
                passed,
                effect: false,
                once,
                timeout_ms: None,
            },
        ))
    }

    /// An operator's `to: principal` rule asked about `name` (`verdict`):
    /// the person's consent gate, as a shipped rule's, the rule named
    /// `security.policies[<i>]`. Approved, the call runs past the
    /// operator's rules and the person's alike.
    pub(crate) fn rule_consent(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        verdict: &crate::sec::policy::Verdict,
        passed: super::reactor::Passed,
    ) -> ToolOutcome {
        let rule = format!("security.policies[{}]", verdict.rule);
        let Some(d) = self.delegation.clone() else {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "{name}: refused by {rule}: it asks the person the work acts for, and no delegation is configured"
                )),
                true,
            );
        };
        let server = self.delegated_server_of(name);
        if let Some(s) = &server
            && let Err(e) = self.delegation_admit(&d, caller, s)
        {
            return ToolOutcome::Ready(Value::String(format!("{name}: {e}")), true);
        }
        let once = verdict.once_per.map(|_| {
            once_key(
                &unit_of(caller),
                name,
                &self.policy_rule_id(verdict.rule),
                args,
            )
        });
        self.open_consent(
            &d,
            caller,
            name,
            args,
            Ask {
                rule,
                server,
                passed: super::reactor::Passed {
                    policy: true,
                    ..passed
                },
                effect: false,
                once,
                timeout_ms: verdict.timeout_ms,
            },
        )
    }

    /// Whether `caller`'s unit is past the first use of `name` with `args`
    /// the operator's rule `i` asks about.
    pub(crate) fn rule_once_passed(
        &self,
        caller: &ToolCaller,
        name: &str,
        i: usize,
        args: &Value,
    ) -> bool {
        self.once_passed(caller, name, &self.policy_rule_id(i), args)
    }

    /// Whether a workflow step of `kind`, its spec rendered, sends the run's
    /// data off the box by its own effect: an `http` call, an A2A send or
    /// delegation, an event forwarded out, a call to or a read from a server
    /// any of whose tools is tagged `egress` (as the registry's reach of a
    /// workflow counts a named server). A delegated server's `mcp.tool` is a
    /// tool call, judged as one.
    fn step_egress(&self, kind: &str, spec: &Map<String, Value>) -> bool {
        use crate::sec::scope::TrifectaTag;
        match kind {
            "http" | "a2a.send" | "a2a.delegate" => true,
            "emit" => spec.get("forward").is_some_and(|f| !f.is_null()),
            "mcp.tool" | "mcp.resource" => {
                let Some(server) = spec.get("server").and_then(Value::as_str) else {
                    return false;
                };
                if self.is_delegated_server(server) {
                    return false;
                }
                let names: Vec<String> = self
                    .registry
                    .iter()
                    .filter(|t| t.server.as_deref() == Some(server))
                    .map(|t| t.name.clone())
                    .collect();
                self.registry.tags_of(&names).contains(&TrifectaTag::Egress)
            }
            _ => false,
        }
    }

    /// A workflow step whose own effect sends the run's data off the box —
    /// an `egress` call made by the step itself — meets the person's
    /// consent rules as an `egress` tool call from that run does (§5.13
    /// item 6: "otherwise the exfiltration simply moves to an `http`
    /// node"): from a run that is tainted and holds their data, it waits
    /// for them. `true` when the step was suspended on the gate.
    pub(crate) fn step_consent(
        &mut self,
        run_id: &str,
        step_id: &str,
        kind: &str,
        spec: &Map<String, Value>,
        caller: &ToolCaller,
    ) -> bool {
        use crate::sec::scope::TrifectaTag;
        let Some(d) = self.delegation.clone() else {
            return false;
        };
        if !self.step_egress(kind, spec) {
            return false;
        }
        let args = Value::Object(spec.clone());
        let facts = crate::sec::policy::Facts {
            delegated: false,
            ..self.policy_facts(caller, kind)
        };
        let call = crate::sec::policy::Call {
            tool: kind,
            tags: &[TrifectaTag::Egress],
            caller: crate::config::settings::PolicyCaller::Workflow,
            principal: caller.principal.as_deref(),
            args: &args,
            also: None,
            facts,
        };
        let Some((rule, once)) =
            crate::sec::policy::consent_rule(&call, &|r| self.once_passed(caller, kind, r, &args))
        else {
            return false;
        };
        let once = once.then(|| once_key(&unit_of(caller), kind, rule, &args));
        let outcome = self.open_consent(
            &d,
            caller,
            kind,
            &args,
            Ask {
                rule: rule.to_string(),
                server: None,
                passed: super::reactor::Passed::default(),
                effect: true,
                once,
                timeout_ms: None,
            },
        );
        self.step_tool_outcome(run_id, step_id, kind, outcome);
        true
    }

    /// Open the person's gate on `name` with `args`, as `ask` says.
    fn open_consent(
        &mut self,
        d: &Delegation,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        ask: Ask,
    ) -> ToolOutcome {
        let Ask {
            rule,
            server,
            passed,
            effect,
            once,
            timeout_ms,
        } = ask;
        // Answered by the person the work is for, as their session or the
        // configuration names them now. Work for nobody who could answer
        // is denied.
        let person = match d.who(caller.acting()) {
            Ok(who) => who.id,
            Err(e) => {
                return ToolOutcome::Ready(
                    Value::String(format!(
                        "{name}: delegation refused (policy_denied): the call needs the person's approval, and nobody who could give it is behind this work ({e})"
                    )),
                    true,
                );
            }
        };
        // Not a firehose (§5.10 rule 6): past the person's gates a minute,
        // a call is refused rather than asked.
        if let Some(refused) = self.consent_rate(&person, name, &rule) {
            return refused;
        }
        let id = crate::sec::random::b64url_token(9).unwrap_or_default();
        let nonce = crate::sec::random::b64url_token(16).unwrap_or_default();
        let scope = server.as_ref().and_then(|s| {
            let g = self.delegation_admit(d, caller, s).ok()?.grant;
            d.servers
                .get(s)?
                .scope_set(name.rsplit('.').next().unwrap_or(name), g.ceilings.get(s)?)
                .ok()
        });
        let text = args.to_string();
        let facts = self.caller_facts(caller);
        let workflow = self.originating_run(caller).map(|r| r.workflow.clone());
        let mut rendered = json!({
            "server": server, "tool": name, "args": args, "args_length": text.len(),
            "scope": scope, "run": caller.run, "step": caller.step, "context": caller.ctx,
            "rule": rule, "principal": person, "workflow": workflow,
            "document_sourced": self.document_sourced(caller, server.clone()),
            "unattended": self.unattended(caller),
        });
        if let (Value::Object(r), Value::Object(f)) = (&mut rendered, facts.to_value()) {
            r.extend(f);
        }
        let timeout = timeout_ms.map_or_else(
            || self.settings.identity.delegation.consent_timeout(),
            std::time::Duration::from_millis,
        );
        // An operator answers too only where the person's own template for
        // the server says so.
        let operator_approves = server.as_ref().is_some_and(|s| {
            self.delegation_admit(d, caller, s)
                .is_ok_and(|a| a.template.operator_approves())
        });
        let task = self.consent_task(caller, &id, &rendered);
        let once_binding = once.as_ref().map(|_| self.once_binding(caller));
        let gate = ConsentGate {
            nonce,
            args_hash: args_hash(name, args),
            rendered: rendered.clone(),
            principal: person.clone(),
            deadline_ms: now_ms() + timeout.as_millis() as u64,
            call: PolicyCall {
                tool: name.to_string(),
                args: args.clone(),
                rule: 0,
                on_timeout: crate::config::settings::PolicyAction::Deny,
                company: None,
                consent: true,
                passed,
                judged: None,
            },
            target: target_of(caller),
            effect,
            operator_approves,
            task,
            signin: None,
            session: caller.session.clone(),
            once,
            once_binding,
        };
        self.consents.insert(id.clone(), gate);
        // Its record is sealed under the person's key on the loop: held by
        // an external signer, the key is opened now, off it.
        if let Some(d) = &self.delegation {
            d.warm(&person);
        }
        // The arguments are the person's content: on the log only where the
        // operator captures content, as a tool request's are.
        let mut said = rendered.clone();
        if !self.log.content_capture()
            && let Some(o) = said.as_object_mut()
        {
            o.remove("args");
        }
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": "asked", "rendered": said,
                   "answer": format!("agentd --delegation-approve {id} | --delegation-deny {id}")}),
        );
        ToolOutcome::Deferred(PendingKind::Consent { id })
    }

    /// The refusal of a gate for `person` past
    /// `identity.delegation.consent_per_minute`, said and recorded; `None`,
    /// and the gate counted, while under it.
    fn consent_rate(&mut self, person: &str, name: &str, rule: &str) -> Option<ToolOutcome> {
        let per_minute = self.settings.identity.delegation.consent_per_minute() as usize;
        let now = now_ms();
        let asked = self.consents_asked.entry(person.to_string()).or_default();
        while asked
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= 60_000)
        {
            asked.pop_front();
        }
        if asked.len() < per_minute {
            asked.push_back(now);
            return None;
        }
        let n = asked.len();
        let target = json!({"principal": person, "tool": name, "rule": rule, "asked": n,
                            "per_minute": per_minute});
        self.log
            .warn("delegation.consent.rate_limited", target.clone());
        self.audit(super::audit::AuditEvent {
            action: "delegation.consent.rate_limited",
            target,
            outcome: "refused",
            principal: Some(person),
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
        Some(ToolOutcome::Ready(
            Value::String(format!(
                "{name}: delegation refused (policy_denied): {person} was asked for consent {n} times in the last minute (identity.delegation.consent_per_minute), so this call is refused rather than asked — it was NOT executed"
            )),
            true,
        ))
    }

    /// The A2A task a gate for `caller`'s work is shown on, when a caller
    /// owns that work: flipped to `input-required` with agentd's own text,
    /// badged as agentd's security gate and naming gate `id`, so the
    /// person's display client offers its approve and deny — as an
    /// `ask_human` addressed to them is shown to them (RFC 0044 §5.10).
    fn consent_task(&mut self, caller: &ToolCaller, id: &str, rendered: &Value) -> Option<String> {
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::tasks::State;
            let super::human::HumanChannel::Owned(task) = self.human_channel(caller) else {
                return None;
            };
            let t = self.tasks.get_mut(&task)?;
            if t.state.is_terminal() {
                return None;
            }
            t.security_gate = true;
            t.consent_gate = Some(json!({"gate": id, "kind": "action"}));
            t.transition(State::InputRequired, Some(gate_text(id, rendered)));
            self.task_persist(&task);
            self.task_sync(&task);
            Some(task)
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (caller, id, rendered);
            None
        }
    }

    /// The gate on task `task` closed: the task's work goes on.
    pub(crate) fn consent_task_closed(&mut self, task: Option<&str>, id: &str) {
        #[cfg(feature = "a2a")]
        if let Some(task) = task
            && let Some(t) = self.tasks.get_mut(task)
        {
            let ours = t
                .consent_gate
                .as_ref()
                .is_some_and(|g| g["gate"].as_str() == Some(id));
            if ours {
                t.consent_gate = None;
                t.security_gate = false;
                if matches!(
                    t.state,
                    crate::a2a::tasks::State::InputRequired
                        | crate::a2a::tasks::State::AuthRequired
                ) {
                    t.transition(crate::a2a::tasks::State::Working, None);
                }
                let task = task.to_string();
                self.task_persist(&task);
                self.task_sync(&task);
            }
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (task, id);
    }

    #[cfg(feature = "a2a")]
    /// Who answers gate `gate` for the A2A caller `principal`, who is `who`
    /// when agentd acts for them (RFC 0044 §5.10 rule 1): the person
    /// through the unix listener (`operator_self`), or through a display
    /// session whose own IdP sign-in is younger than
    /// `identity.delegation.consent_session_max_age` (`gate`); or an
    /// operator the person's template lists among its approvers
    /// (`operator`). Anyone else is refused, and why is said and recorded.
    pub(crate) fn consent_answerer(
        &self,
        d: &Delegation,
        principal: &crate::a2a::Principal,
        who: Option<&crate::delegation::Who>,
        gate: &str,
    ) -> Result<Answerer, (&'static str, String)> {
        let _ = d;
        let Some(g) = self.consents.get(gate) else {
            // No such gate: the answer fails as one to a gate that is not.
            return Ok(Answerer {
                id: principal.id.clone(),
                via: "gate",
            });
        };
        if let Some(w) = who.filter(|w| w.id == g.principal) {
            return self.person_answers(w, &format!("gate {gate}"));
        }
        if principal.is_operator() && g.operator_approves {
            return Ok(Answerer {
                id: principal.id.clone(),
                via: "operator",
            });
        }
        Err((
            "not_the_person",
            format!(
                "gate {gate} is {}'s to answer, and {} is not them: no operator answers for the person",
                g.principal, principal.id
            ),
        ))
    }

    #[cfg(feature = "a2a")]
    /// How person `w` answers `what` — one of their own gates: over their
    /// own listener (`operator_self`, `certificate`), or through a display
    /// session whose own IdP sign-in is younger than
    /// `identity.delegation.consent_session_max_age` (`gate`); a session
    /// that never signed in there, or did too long ago, is refused, and why
    /// is said.
    fn person_answers(
        &self,
        w: &crate::delegation::Who,
        what: &str,
    ) -> Result<Answerer, (&'static str, String)> {
        let Some(_sid) = &w.session else {
            return Ok(Answerer {
                id: w.id.clone(),
                via: w.via_self(),
            });
        };
        let Some(proof) = &w.proof else {
            return Err((
                "no_idp_sign_in",
                format!(
                    "{what} is {}'s, and this session has not signed in at the IdP: a session answers the person's gates only once it has proved, at the IdP, that it is them",
                    w.id
                ),
            ));
        };
        let max_age = self
            .settings
            .identity
            .delegation
            .consent_session_max_age()
            .as_millis() as u64;
        if now_ms().saturating_sub(proof.at_ms) > max_age {
            return Err((
                "stale_session",
                format!(
                    "this session's IdP sign-in is older than identity.delegation.consent_session_max_age ({}s): sign in at the IdP again to answer {what}",
                    max_age / 1000
                ),
            ));
        }
        Ok(Answerer {
            id: w.id.clone(),
            via: "gate",
        })
    }

    #[cfg(feature = "a2a")]
    /// The person's answer to gate `id`, given by `by`: run the call it
    /// held, or refuse it. `Err` when there is no such gate, or the answer
    /// is not one the gate takes.
    pub(crate) fn consent_answer(
        &mut self,
        id: &str,
        by: &Answerer,
        nonce: Option<&str>,
        args_hash: Option<&str>,
        approve: bool,
    ) -> Result<String, String> {
        let gate = self
            .consents
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no open consent gate is {id:?}"))?;
        let via = by.via;
        let by = by.id.as_str();
        if gate.signin.is_some() && approve {
            return Err(format!(
                "gate {id} asks for a sign-in at the IdP, not an approval: sign in where it says, or deny it to cancel the call"
            ));
        }
        if gate.principal != by && !(via == "operator" && gate.operator_approves) {
            self.consent_refused(id, by, "not_the_person");
            return Err(format!(
                "gate {id} is {}'s to answer, and {by} is not them: no operator answers for the person",
                gate.principal
            ));
        }
        if nonce != Some(gate.nonce.as_str())
            || (approve && args_hash != Some(gate.args_hash.as_str()))
        {
            // A sign-in gate is cancelled by its person without its
            // nonce: they never read it, there being nothing to approve.
            if gate.signin.is_some() && !approve && gate.principal == by {
                self.close_consent(id, via, "the person cancelled the sign-in it asked for");
                return Ok(format!(
                    "cancelled {}: the call was not made",
                    gate.call.tool
                ));
            }
            self.consent_refused(id, by, "not_bound");
            return Err(format!(
                "the answer to gate {id} is not bound to its nonce and arguments: nothing was decided"
            ));
        }
        let decision = if approve { "approved" } else { "denied" };
        // The record is kept before the answer takes effect, and what the
        // person saw in it only sealed under their key: an approval whose
        // record cannot be kept so decides nothing.
        let personal = self.seal_gate(id, &gate);
        if approve && let Err(e) = &personal {
            return Err(format!(
                "the record of gate {id}'s approval could not be sealed ({e}): nothing was decided, and the gate is open"
            ));
        }
        self.consents.remove(id);
        self.consent_task_closed(gate.task.as_deref(), id);
        let found = self.pending.iter().position(|p| {
            p.target == gate.target && matches!(&p.kind, PendingKind::Consent { id: g } if g == id)
        });
        if let Some(i) = found {
            self.pending.remove(i);
        }
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": decision, "via": via, "principal": by,
                   "args_hash": gate.args_hash}),
        );
        self.audit_consent(id, &gate, decision, via, personal);
        let tool = gate.call.tool.clone();
        if found.is_none() {
            return Ok(format!(
                "gate {id} is closed: the work that asked has ended"
            ));
        }
        if approve {
            self.remember_once(&gate);
            if gate.effect {
                self.approve_step(&gate);
            } else {
                self.run_approved(&gate.target, &gate.call);
            }
            Ok(format!("approved {tool}: the call runs"))
        } else {
            self.reply(
                &gate.target,
                Value::String(format!(
                    "{tool}: delegation refused (policy_denied): {by} denied this call — it was NOT executed"
                )),
                true,
            );
            Ok(format!("denied {tool}: the call was not made"))
        }
    }

    #[cfg(feature = "a2a")]
    /// An approved step gate: the step is pending again, and runs with the
    /// spec the person saw ([`Runtime::approved_spec`]) as the attempt that
    /// asked, which made no effect. A denied one stays an attempt, so a
    /// `retry:` asks again only as often as it allows.
    fn approve_step(&mut self, gate: &ConsentGate) {
        let Target::Step(run, step) = &gate.target else {
            return;
        };
        if let Value::Object(spec) = &gate.call.args {
            self.consented_steps
                .insert(format!("{run}/{step}"), spec.clone());
        }
        if let Some(r) = self.runs.get_mut(run) {
            if let Some(st) = r.steps.get_mut(step) {
                st.status = crate::engine::run::StepStatus::Pending;
                st.wait = None;
                st.attempt = st.attempt.saturating_sub(1);
            }
            if !r.status.is_terminal() {
                r.status = crate::engine::run::RunStatus::Running;
            }
            r.touch();
        }
    }

    /// The spec a step the person approved runs with, once: what they saw,
    /// not a fresh render.
    pub(crate) fn approved_spec(
        &mut self,
        run_id: &str,
        step_id: &str,
    ) -> Option<Map<String, Value>> {
        self.consented_steps.remove(&format!("{run_id}/{step_id}"))
    }

    /// What gate `id` showed the person, sealed under their personal key
    /// (RFC 0044 §6.4) for its record: `{key, sealed}`. A key an external
    /// signer holds that is not open yet — the signer did not answer when
    /// the gate opened — is opened again off the loop, for the next answer.
    fn seal_gate(&self, id: &str, gate: &ConsentGate) -> Result<Value, String> {
        let d = self
            .delegation
            .as_ref()
            .ok_or("no delegation is configured")?;
        let sealed = d.seal_personal(
            &gate.principal,
            &format!("consent:{id}"),
            &json!({"rendered": gate.rendered, "text": gate_text(id, &gate.rendered)}),
        );
        if sealed.is_err() {
            d.warm(&gate.principal);
        }
        sealed
    }

    /// The audit-chain record of gate `id`'s `decision`, made `via` (§5.10
    /// rule 7): who, the run, step, tool, server, scope and rule, the
    /// arguments' hash and length, whether the work was tainted and held
    /// delegated data, when — and what the person was shown, only as
    /// `personal` sealed it. A record whose content could not be sealed
    /// says so, and holds none of it.
    fn audit_consent(
        &self,
        id: &str,
        gate: &ConsentGate,
        decision: &'static str,
        via: &str,
        personal: Result<Value, String>,
    ) {
        let r = &gate.rendered;
        let mut target = json!({"gate": id, "decision": decision, "via": via,
            "args_hash": gate.args_hash, "decided_at": now_ms(),
            "tool": r["tool"], "server": r["server"], "run": r["run"], "step": r["step"],
            "context": r["context"], "scope": r["scope"], "rule": r["rule"],
            "args_length": r["args_length"], "tainted": r["tainted"],
            "holds_delegated_data": r["holds_delegated_data"]});
        match personal {
            Ok(p) => target["personal"] = p,
            Err(e) => {
                self.log.warn(
                    "delegation.consent.unsealed",
                    json!({"gate": id, "err": e,
                           "msg": "the consent record keeps no content of the gate"}),
                );
                target["personal"] = json!({"unsealed": true});
            }
        }
        self.audit(super::audit::AuditEvent {
            action: "delegation.consent",
            target,
            outcome: decision,
            principal: Some(&gate.principal),
            role: Some("operator"),
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
    }

    #[cfg(feature = "a2a")]
    /// An answer to gate `id` refused, `why`: said and recorded, and the
    /// gate left open (T7, T8).
    pub(crate) fn consent_refused(&self, id: &str, by: &str, why: &str) {
        self.log.warn(
            "delegation.consent.refused",
            json!({"gate": id, "by": by, "why": why}),
        );
        self.audit(super::audit::AuditEvent {
            action: "delegation.consent.refused",
            target: json!({"gate": id, "by": by, "why": why}),
            outcome: "refused",
            principal: Some(by),
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
    }

    /// An answer `by` gave to elicitation gate `id`, refused for `why`, the
    /// gate left open: said and recorded.
    #[cfg(feature = "a2a")]
    pub(crate) fn elicitation_refused(&self, id: &str, by: &str, why: &str) {
        self.log.warn(
            "delegation.elicitation.refused",
            json!({"gate": id, "by": by, "why": why}),
        );
        self.audit(super::audit::AuditEvent {
            action: "delegation.elicitation.refused",
            target: json!({"gate": id, "by": by, "why": why}),
            outcome: "refused",
            principal: Some(by),
            role: None,
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
    }

    /// The open consent gate the A2A task `task` waits behind, when its
    /// work is one: a message on it is no answer (§5.10 rule 1).
    #[cfg(feature = "a2a")]
    pub(crate) fn consent_on_task(&self, task: &str) -> Option<String> {
        let t = self.tasks.get(task)?;
        self.consents.iter().find_map(|(id, g)| {
            let waits = match (&g.target, &t.link) {
                (Target::Step(run, _), crate::a2a::tasks::Link::Run { id }) => run == id,
                (Target::Child(..), crate::a2a::tasks::Link::Turn { ctx }) => {
                    matches!(self.reader_at(&g.target), super::withhold::Reader::Conversation(Some(c)) if &c == ctx)
                }
                _ => false,
            };
            waits.then(|| id.clone())
        })
    }

    /// Every pass: an unanswered gate past its deadline denies — never a
    /// judge, never an override — and a gate whose asker is gone closes;
    /// a step-up gate's device grant is polled.
    pub(crate) fn poll_consents(&mut self) {
        #[cfg(feature = "a2a")]
        self.poll_step_ups();
        let now = now_ms();
        // The work of a session that has ended acts for nobody: its gates
        // close at once, whoever might still answer them.
        let ended: Vec<String> = self
            .consents
            .iter()
            .filter(|(_, g)| {
                g.session.as_deref().is_some_and(|sid| {
                    !self
                        .delegation
                        .as_ref()
                        .is_some_and(|d| d.session_live(sid))
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            self.close_consent_as(
                &id,
                "revoked",
                "revoked",
                "the session this work acts through has ended — revoked, expired or lost to a restart — and its delegated access with it",
            );
        }
        let due: Vec<String> = self
            .consents
            .iter()
            .filter(|(id, g)| {
                now >= g.deadline_ms
                    || !self.pending.iter().any(|p| {
                        p.target == g.target
                            && matches!(&p.kind, PendingKind::Consent { id: x } if x == *id)
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.close_consent(
                &id,
                "on_timeout",
                "nobody approved this call in time — on_timeout deny",
            );
        }
    }

    /// Close gate `id` unanswered, `via` — `on_timeout`, `forgotten`: a
    /// denial, recorded while its asker is still there (a gate whose asker
    /// ended decides nothing), and the asker told `why`.
    fn close_consent(&mut self, id: &str, via: &str, why: &str) {
        self.close_consent_as(id, via, "policy_denied", why);
    }

    /// [`Self::close_consent`], the asker told `code`.
    fn close_consent_as(&mut self, id: &str, via: &str, code: &str, why: &str) {
        let Some(g) = self.consents.remove(id) else {
            return;
        };
        self.consent_task_closed(g.task.as_deref(), id);
        let found = self.pending.iter().position(|p| {
            p.target == g.target && matches!(&p.kind, PendingKind::Consent { id: x } if x == id)
        });
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": "denied", "via": via}),
        );
        if found.is_some() {
            let personal = self.seal_gate(id, &g);
            self.audit_consent(id, &g, "denied", via, personal);
        }
        if let Some(i) = found {
            self.pending.remove(i);
            self.reply(
                &g.target,
                Value::String(format!(
                    "{}: delegation refused ({code}): {why}; it was NOT executed",
                    g.call.tool
                )),
                true,
            );
        }
        // The calls that waited on this gate's request end with it.
        let followers: Vec<String> = self
            .consents
            .iter()
            .filter(|(_, f)| {
                f.signin
                    .as_ref()
                    .is_some_and(|s| s.follows.as_deref() == Some(id))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for f in followers {
            self.close_consent_as(&f, via, code, why);
        }
    }

    /// `principal` is being forgotten (§6.4): every gate open for them is
    /// closed now, denied, its record sealed under the key that is about to
    /// be destroyed — none waits to be recorded after it, under a key made
    /// for them again.
    #[cfg(feature = "a2a")]
    pub(crate) fn forget_consents(&mut self, principal: &str) {
        let theirs: Vec<String> = self
            .consents
            .iter()
            .filter(|(_, g)| g.principal == principal)
            .map(|(id, _)| id.clone())
            .collect();
        for id in theirs {
            self.close_consent(
                &id,
                "forgotten",
                "the person this call was for was forgotten",
            );
        }
    }

    #[cfg(feature = "a2a")]
    /// The shape of open gate `id`, when it is open ([`shape_of`]).
    pub(crate) fn consent_shape(&self, id: &str) -> Option<String> {
        self.consents.get(id).map(|g| shape_of(&g.rendered))
    }

    #[cfg(feature = "a2a")]
    /// The open gates `who` sees — every gate, or only `mine`'s: each by
    /// its tool and server, and its shape, which gates answered together
    /// share; what it shows — the arguments, the text the person reads —
    /// and its binding only for whoever may answer it: the person, and an
    /// `operator` where the person's template lists one among its
    /// approvers.
    pub(crate) fn consent_list(&self, who: &str, operator: bool, mine: Option<&str>) -> Vec<Value> {
        self.consents
            .iter()
            .filter(|(_, g)| mine.is_none_or(|m| g.principal == m))
            .map(|(id, g)| {
                let mut v = json!({"gate": id, "principal": g.principal,
                                   "tool": g.rendered["tool"], "server": g.rendered["server"],
                                   "shape": shape_of(&g.rendered), "deadline_ms": g.deadline_ms});
                if g.principal == who || (operator && g.operator_approves) {
                    v["rendered"] = g.rendered.clone();
                    v["text"] = json!(gate_text(id, &g.rendered));
                    v["nonce"] = json!(g.nonce);
                    v["args_hash"] = json!(g.args_hash);
                }
                v
            })
            .collect()
    }

    /// A gate's durable form, in a step's wait record.
    pub(crate) fn consent_wait(&self, id: &str) -> Value {
        match self.consents.get(id) {
            Some(g) => json!({"kind": "consent", "gate": id, "nonce": g.nonce,
                              "args_hash": g.args_hash, "rendered": g.rendered,
                              "principal": g.principal, "deadline_ms": g.deadline_ms,
                              "policy": super::human::policy_call_json(&g.call),
                              "effect": g.effect, "operator_approves": g.operator_approves,
                              "task": g.task, "session": g.session, "once": g.once,
                              "once_binding": g.once_binding,
                              "signin": g.signin.as_ref().map(super::stepup::signin_wait)}),
            None => json!({"kind": "consent", "gate": id}),
        }
    }

    /// At start: each workflow step suspended on a consent gate asks again,
    /// with the same binding and deadline.
    pub(crate) fn rebuild_consents(&mut self) {
        let mut found = Vec::new();
        let mut signins = Vec::new();
        for (run, r) in &self.runs {
            for (step, s) in &r.steps {
                let Some(w) = s.wait.as_ref().filter(|w| w["kind"] == "consent") else {
                    continue;
                };
                if s.status != crate::engine::run::StepStatus::Suspended {
                    continue;
                }
                let Some(call) = w.get("policy").and_then(super::human::policy_call_of) else {
                    continue;
                };
                if w.get("signin").is_some_and(|s| s.is_object()) {
                    signins.push((
                        w["gate"].as_str().unwrap_or_default().to_string(),
                        w["signin"].clone(),
                        w["principal"].as_str().unwrap_or_default().to_string(),
                        (run.clone(), step.clone()),
                    ));
                }
                found.push((
                    w["gate"].as_str().unwrap_or_default().to_string(),
                    ConsentGate {
                        nonce: w["nonce"].as_str().unwrap_or_default().to_string(),
                        args_hash: w["args_hash"].as_str().unwrap_or_default().to_string(),
                        rendered: w["rendered"].clone(),
                        principal: w["principal"].as_str().unwrap_or_default().to_string(),
                        deadline_ms: w["deadline_ms"].as_u64().unwrap_or(0),
                        call: PolicyCall {
                            consent: true,
                            ..call
                        },
                        target: Target::Step(run.clone(), step.clone()),
                        effect: w["effect"].as_bool() == Some(true),
                        operator_approves: w["operator_approves"].as_bool() == Some(true),
                        task: w["task"].as_str().map(str::to_string),
                        signin: None,
                        session: w["session"].as_str().map(str::to_string),
                        once: w["once"].as_str().map(str::to_string),
                        once_binding: w["once_binding"].as_str().map(str::to_string),
                    },
                ));
            }
        }
        for (id, g) in found {
            if id.is_empty() || g.nonce.is_empty() {
                continue;
            }
            self.push_pending(super::reactor::PendingTool {
                target: g.target.clone(),
                name: g.call.tool.clone(),
                kind: PendingKind::Consent { id: id.clone() },
                started_ms: now_ms(),
            });
            self.consents.insert(id, g);
        }
        // A step-up gate comes back as the sign-in it asks for: a device
        // grant's polling resumes once its sealed code is opened; an
        // `authorize` one ended with the session it was for.
        #[cfg(feature = "a2a")]
        {
            use super::stepup::How;
            let mut ended = Vec::new();
            for (id, w, principal, at) in signins {
                let rebuilt = self.rebuild_signin(&id, &w, &principal, at);
                let ends = match rebuilt.as_ref().map(|s| s.how) {
                    None => Some(How::Authorize),
                    Some(h @ (How::Authorize | How::Ciba)) => Some(h),
                    Some(How::Device | How::Reenrol) => None,
                };
                if let Some(g) = self.consents.get_mut(&id) {
                    g.signin = rebuilt;
                }
                if let Some(h) = ends {
                    ended.push((id, h));
                }
            }
            self.end_rebuilt_signins(&ended);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = signins;
    }

    /// A delegated server's tool call: admitted on the loop, minted and made
    /// off it.
    pub(crate) fn delegated_call(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        server: &str,
        tool: &str,
        args: Value,
    ) -> ToolOutcome {
        let Some(d) = self.delegation.clone() else {
            return ToolOutcome::Ready(
                Value::String(format!("{name}: no delegation is configured")),
                true,
            );
        };
        let refused = |e: crate::delegation::DelegationError| {
            ToolOutcome::Ready(Value::String(format!("{name}: {e}")), true)
        };
        if let Err(e) = self.delegation_admit(&d, caller, server) {
            d.denied(
                caller.principal.as_deref(),
                server,
                tool,
                &e,
                caller.run.clone(),
                caller.step.clone(),
            );
            return refused(e);
        }
        let mut meta = json!({"agent/idempotency_key": self.idempotency_key(caller), "agent/instance": self.instance});
        let trace = crate::mcp::stamp_trace(&mut meta, self.caller_trace(caller).as_ref());
        let unit = call_unit(caller);
        let reads = reads_of(caller);
        let trusted = d.servers.get(server).is_some_and(|s| s.trusted_output);
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let (server, tool, authority, session) = (
            server.to_string(),
            tool.to_string(),
            caller.principal.clone(),
            caller.session.clone(),
        );
        let (run, step) = (caller.run.clone(), caller.step.clone());
        let name = name.to_string();
        #[cfg(feature = "a2a")]
        let (mut step_up_caller, mut step_up_args) = (Some(caller.clone()), Some(args.clone()));
        std::thread::Builder::new()
            .name(format!("delegated:{server}.{tool}"))
            .spawn(move || {
                let start = crate::obs::otel::now_unix_nanos();
                let res = d.call(Call {
                    server: &server,
                    tool: &tool,
                    args,
                    meta,
                    acting: crate::delegation::Acting::new(
                        authority.as_deref(),
                        session.as_deref(),
                    ),
                    unit,
                    run,
                    step,
                });
                crate::obs::otel::mcp_span(
                    trace.as_ref(),
                    "tools/call",
                    &tool,
                    &server,
                    res.as_ref().is_ok_and(|r| !r.is_error()),
                    start,
                );
                // The IdP wants the person to sign in again: the call waits
                // on a step-up gate rather than failing (`runtime::stepup`).
                // A firing that has not asked its person waits on their
                // approval; a workflow step whose grant is gone, at a server
                // that says `on_revoked: ask`, may wait for them to sign in
                // again (`runtime::firings`).
                #[cfg(feature = "a2a")]
                if let Err(e) = &res
                    && let Some(caller) = step_up_caller.take()
                {
                    use crate::config::settings::delegation::OnRevoked;
                    use crate::delegation::Code;
                    let gone = matches!(e.code, Code::Revoked | Code::NeedsReenroll)
                        && d.servers
                            .get(&server)
                            .is_some_and(|s| s.on_revoked == OnRevoked::Ask);
                    let mut ask = |caller| {
                        Box::new(super::stepup::StepUpAsk {
                            caller,
                            name: name.clone(),
                            server: server.clone(),
                            args: step_up_args.take().unwrap_or(Value::Null),
                            error: format!("{name}: {e}"),
                        })
                    };
                    match e.code {
                        Code::StepUp => {
                            let _ = tx.send(Event::StepUpAsked(ask(caller)));
                            return;
                        }
                        Code::CibaRequired => {
                            let _ = tx.send(Event::CibaAsked(ask(caller)));
                            return;
                        }
                        _ if gone => {
                            let _ = tx.send(Event::Reenrol(ask(caller)));
                            return;
                        }
                        _ => {}
                    }
                }
                let (result, is_error) = match res {
                    Ok(r) => {
                        // The person's SaaS content: the unit it reaches holds
                        // delegated data, and is tainted by it unless the
                        // operator vouched for the server — written before
                        // the result is read.
                        if let Some(into) = reads {
                            let _ = tx.send(Event::Read {
                                into,
                                carries: read_carries(&server, trusted),
                            });
                        }
                        (super::worker::tool_result_value(&r), r.is_error())
                    }
                    Err(e) => (Value::String(format!("{name}: {e}")), true),
                };
                target.send(&tx, result, is_error);
            })
            .ok();
        ToolOutcome::Executing
    }

    /// What the delegation supervisor told the loop.
    pub(crate) fn on_delegation_note(&mut self, n: Note) {
        match n {
            Note::Audit {
                action,
                outcome,
                principal,
                target,
            } => self.audit_chained(super::audit::AuditEvent {
                action,
                target,
                outcome,
                principal: principal.as_deref(),
                role: None,
                request_id: None,
                sid: None,
                trace: None,
                link: None,
            }),
            Note::Catalogue { server, tools } => {
                let count = tools.len();
                match self.registry_with(&server, tools) {
                    Ok(Some(registry)) => {
                        self.registry = registry;
                        self.log.info(
                            "delegation.catalogue",
                            json!({"server": server, "tools": count}),
                        );
                        self.log_workflow_tools();
                    }
                    Ok(None) => {}
                    Err(errors) => self.log.warn(
                        "delegation.catalogue.refused",
                        json!({"server": server, "errors": errors}),
                    ),
                }
            }
            Note::Elicit(e) => self.delegated_elicit(*e),
        }
    }

    /// A delegated server asks the person whose grant its connection
    /// carries (RFC 0044 §5.13 item 10, T24): a gate of its own, owned by
    /// and addressed to that person — no operator answers it for them, no
    /// judge and no approval policy does — in agentd's words, the server's
    /// quoted as the server's. Unanswered within the server's wait it is
    /// cancelled; nobody to ask, cancelled at once.
    pub(crate) fn delegated_elicit(&mut self, e: crate::delegation::Elicitation) {
        #[cfg(feature = "a2a")]
        {
            use crate::sec::render::quoted;
            let id = self.next_id("elicit");
            let question = format!(
                "agentd: the delegated server {} asks you, through your grant — its words follow, quoted; agentd does not vouch for them, and nobody answers this for you: {}",
                quoted(&e.server, 128),
                quoted(&e.message, 2000)
            );
            let caller = ToolCaller {
                principal: Some(e.principal.clone()),
                ctx: Some(format!("elicit-{id}")),
                ..ToolCaller::default()
            };
            let to = crate::a2a::principals::Addressee {
                id: Some(e.principal.clone()),
                ..Default::default()
            };
            let deadline = now_ms() + e.timeout.as_millis() as u64;
            let asked = if self.a2a_sink.is_some() {
                self.human_gate(
                    &caller,
                    super::human::GateOn::Unit,
                    question,
                    deadline,
                    Some(e.schema.clone()),
                    Some(to),
                    None,
                )
            } else {
                ToolOutcome::Ready(Value::Null, true)
            };
            self.log.info(
                "delegation.elicitation",
                json!({"gate": id, "server": e.server, "principal": e.principal,
                       "asked": matches!(asked, ToolOutcome::Deferred(_))}),
            );
            match asked {
                ToolOutcome::Deferred(kind) => {
                    self.elicits.insert(
                        id.clone(),
                        ElicitWait {
                            reply: e.reply,
                            grant: e.grant,
                        },
                    );
                    self.push_pending(super::reactor::PendingTool {
                        target: Target::Elicit(id),
                        name: "elicitation".into(),
                        kind,
                        started_ms: now_ms(),
                    });
                }
                _ => {
                    let _ = e.reply.try_send(None);
                }
            }
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = e.reply.try_send(None);
        }
    }

    /// The person's answer to elicitation gate `id`, or its end unanswered
    /// (`is_error`, or timed out): handed to the connection that waits.
    pub(crate) fn elicit_answered(&mut self, id: &str, result: Value, is_error: bool) {
        let Some(wait) = self.elicits.remove(id) else {
            return;
        };
        let answered = !is_error && result.get("timed_out").and_then(Value::as_bool) != Some(true);
        let answer = answered.then(|| result.get("reply").cloned().unwrap_or(result));
        let _ = wait.reply.try_send(answer);
    }

    #[cfg(feature = "a2a")]
    /// Whether `principal` may answer elicitation gate `id` (RFC 0044 T24):
    /// held to the answerer rule of a consent gate — the grant's person,
    /// through a session whose own IdP sign-in is recent enough, or over
    /// their own listener — and to the grant itself: a device session under
    /// the same name that never proved the identity the grant is bound to
    /// answers nothing, for its answer would act as the person. Why not,
    /// when not. A gate no connection waits on any more is not judged here.
    pub(crate) fn elicit_answerer(
        &self,
        id: &str,
        principal: &crate::a2a::Principal,
    ) -> Result<(), (&'static str, String)> {
        let (Some(wait), Some(d)) = (self.elicits.get(id), &self.delegation) else {
            return Ok(());
        };
        let acting =
            crate::delegation::Acting::new(Some(&principal.id), principal.session.as_deref());
        let who = d.who(acting).map_err(|e| {
            (
                "not_the_person",
                format!(
                    "elicitation {id} is for the person whose grant the server asked through, and {} is not them: {e}",
                    principal.id
                ),
            )
        })?;
        self.person_answers(&who, &format!("elicitation {id}"))?;
        d.holds(&who, &wait.grant).map_err(|e| {
            (
                "not_the_holder",
                format!(
                    "elicitation {id} was asked through a grant this session does not hold: {e}"
                ),
            )
        })
    }

    /// Where the delegation supervisor's notes go: the loop — and each
    /// audit record first to the audit chain, from the thread that made it,
    /// so it is on the disk before that thread hands on the token it
    /// minted or the answer of the call it made.
    pub(crate) fn delegation_wire(&mut self) {
        if let Some(d) = &self.delegation {
            let tx = self.events_tx.clone();
            let chain = self.audit_chain.clone();
            let (instance, trace, log) =
                (self.instance.clone(), self.trace.clone(), self.log.clone());
            d.notify_to(Arc::new(move |n| {
                if let (
                    Some(c),
                    Note::Audit {
                        action,
                        outcome,
                        principal,
                        target,
                    },
                ) = (&chain, &n)
                {
                    let ev = super::audit::AuditEvent {
                        action,
                        target: target.clone(),
                        outcome,
                        principal: principal.as_deref(),
                        role: None,
                        request_id: None,
                        sid: None,
                        trace: None,
                        link: None,
                    };
                    super::audit::chain_record(
                        c,
                        super::audit::record_of(&instance, &trace, &ev),
                        &log,
                    );
                }
                let _ = tx.send(Event::Delegation(Box::new(n)));
            }));
        }
    }

    /// Every pass: liveness probes of the grants in use, and the gates'
    /// deadlines.
    pub(crate) fn delegation_tick(&mut self) {
        if let Some(d) = &self.delegation {
            Delegation::tick(d);
        }
        self.poll_consents();
        #[cfg(feature = "a2a")]
        self.expire_admissions();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit's stored first uses are swept once every one of them is past
    /// its day, and not before; nothing else in the store is touched.
    #[test]
    fn first_use_records_past_their_day_are_swept() {
        use crate::state::{Durable, Kind, Policy};
        let d = Durable::new(
            Arc::new(crate::store::memory::MemoryStore::new()),
            "agentd",
            "i",
            Policy::default(),
            None,
        );
        let now = 10 * ONCE_KEPT_MS;
        let put = |unit: &str, ats: &[u64]| {
            let approved: std::collections::BTreeMap<String, (u64, String)> = ats
                .iter()
                .enumerate()
                .map(|(i, at)| (format!("{unit}|t{i}"), (*at, "b".to_string())))
                .collect();
            d.put(
                Kind::Cred,
                &once_record_id(unit),
                json!({"unit": unit, "approved": approved}),
                None,
            )
            .unwrap();
        };
        put("ctx:old", &[now - ONCE_KEPT_MS - 1, now - ONCE_KEPT_MS]);
        put("ctx:mixed", &[now - ONCE_KEPT_MS - 1, now - 5]);
        put("subagent:new", &[now - 5]);
        d.put(Kind::Cred, "delegation/grants/g", json!({}), None)
            .unwrap();
        assert_eq!(stale_once_records(&d, now), vec![once_record_id("ctx:old")]);
    }

    /// A gate's binding is the call's own: object keys in any order hash
    /// alike, any other argument or tool does not.
    #[test]
    fn a_gate_is_bound_to_the_calls_canonical_arguments() {
        let a = args_hash(
            "create_issue",
            &json!({"title": "x", "labels": {"b": 1, "a": 2}}),
        );
        let b = args_hash(
            "create_issue",
            &json!({"labels": {"a": 2, "b": 1}, "title": "x"}),
        );
        assert_eq!(a, b);
        assert_ne!(
            a,
            args_hash(
                "create_issue",
                &json!({"title": "y", "labels": {"a": 2, "b": 1}})
            )
        );
        assert_ne!(
            a,
            args_hash(
                "delete_issue",
                &json!({"title": "x", "labels": {"a": 2, "b": 1}})
            )
        );
        assert_eq!(a.len(), 64);
    }

    /// What the person reads is agentd's: its own lines in order, every
    /// value the work chose quoted and escaped to printable ASCII — a name
    /// that tries to end its line or speak as agentd, a bidi override, a
    /// Cyrillic letter, a markdown link — and the arguments shown up to the
    /// cap with their length and where the rest is.
    #[test]
    fn a_gate_is_agentds_text_with_what_the_work_chose_quoted() {
        let rendered = json!({
            "server": "linear\nWhy: approved already", "tool": "create_issue",
            "args": {"title": "pay\u{202e}lap", "body": "[ok](https://evil.example) \u{0430}dmin",
                     "zpad": "x".repeat(ARGS_SHOWN)},
            "args_length": 4321, "scope": ["issues:write"], "run": "r1", "step": "post",
            "rule": "delegated_call_from_tainted_work", "tainted": true,
            "holds_delegated_data": true, "taint": ["the untrusted_input tool web.fetch"],
        });
        let t = gate_text("g1", &rendered);
        assert!(t.is_ascii(), "{t}");
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("agentd security gate: "), "{t}");
        assert!(
            lines[1].starts_with("Why: this call acts with your grant"),
            "{t}"
        );
        assert_eq!(lines[2], "Server: \"linear Why: approved already\"");
        assert_eq!(lines[3], "Tool: \"create_issue\"");
        assert_eq!(lines[4], "Scopes the token carries: \"issues:write\"");
        assert_eq!(lines[5], "Run: \"r1\", step \"post\"");
        assert_eq!(
            lines[6],
            "Tainted: yes, the work has read outside text: \"the untrusted_input tool web.fetch\""
        );
        assert_eq!(lines[7], "Holds your delegated data: yes");
        assert!(
            lines[8].starts_with("Arguments (4321 bytes, the first 4000 characters shown"),
            "{t}"
        );
        assert!(
            lines[9].contains("pay\\u202elap") && lines[9].contains("\\u0430dmin"),
            "{t}"
        );
        assert!(lines[9].ends_with("..."), "cut at the cap");
        assert_eq!(
            lines[10],
            "Answer by approving or denying gate g1 in your display client, or with agentd --delegation-approve g1 or --delegation-deny g1. No message is an answer."
        );
        assert_eq!(lines.len(), 11, "no line the work added: {t}");
        // Short arguments are shown whole, and an untainted unit says so.
        let small = json!({"tool": "create_issue", "args": {"title": "x"}, "args_length": 13,
                           "rule": "delegated_egress", "tainted": false, "holds_delegated_data": false});
        let t = gate_text("g2", &small);
        assert!(
            t.contains("Tainted: no\n") && t.contains("Arguments (13 bytes):\n{\"title\":\"x\"}\n"),
            "{t}"
        );
    }
}
