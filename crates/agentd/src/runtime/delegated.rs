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
//! **Consent** (§5.10, §5.13 item 6). The rules read the unit's facts from
//! the taint engine — whether it is tainted, whether it holds delegated
//! data — and are hard-coded until P2 makes them configurable:
//!
//! 1. an `egress` call of any kind — delegated or under agentd's own
//!    credential — from a unit that is tainted and holds delegated data
//!    asks the person, so the data does not leave through another server.
//!    A workflow step that sends the run's data out by its own effect
//!    rather than a tool call is one too: an `http` call, an `a2a.send` or
//!    `a2a.delegate`, an `emit` that forwards the event out, an `mcp.tool`
//!    or `mcp.resource` at a server whose tools are tagged `egress`
//!    ([`Runtime::step_consent`]). Approved, the step runs again past the
//!    gate with the spec the person saw;
//! 2. a delegated call to a `sensitive` or `egress` tool from a tainted
//!    unit asks;
//! 3. a delegated `egress` call asks whatever the unit read — at every use,
//!    the first use per run and tool of §5.13 until P2's `once_per`.
//!
//! A gate is a [`PendingKind::Consent`]: addressed to the person, answered
//! only by them over the unix A2A listener (`agentd --delegation-approve
//! <gate>` / `--delegation-deny <gate>`, recorded `via: operator_self`),
//! bound to its `nonce` and the hash of the call's canonical arguments, and
//! never answered by a judge, an operator, or a message on the asking task
//! (refused there, the gate left open). What the person reads is agentd's
//! text ([`gate_text`]): its own lines, the values the work chose quoted and
//! escaped, the arguments in full up to a cap with their length, whether
//! the work is tainted and by what. Unanswered within
//! `identity.delegation.consent_timeout` it denies. Each decision is an
//! audit-chain record whose personal content — what the person was shown —
//! is sealed under their personal key (§6.4), so forgetting them leaves it
//! unreadable and the chain whole. A workflow step's gate is kept in the
//! step's wait record and re-armed at the next start; a turn's dies with
//! its turn.

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
fn rule_words(rule: &str) -> &'static str {
    match rule {
        "egress_from_tainted_work_holding_delegated_data" => {
            "the work holds your delegated data and has read outside text, and this call sends data off the box"
        }
        "delegated_call_from_tainted_work" => {
            "this call acts with your grant at a sensitive or egress tool, from work that has read outside text"
        }
        "delegated_egress" => "this call sends data off the box with your grant",
        _ => "a consent rule asks",
    }
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
        "Answer with agentd --delegation-approve {id} or agentd --delegation-deny {id}. No message is an answer."
    ));
    lines.join("\n")
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
}

/// The hash a gate is bound to: SHA-256 of `{tool, args}` as compact JSON,
/// whose object keys `serde_json` keeps sorted — so the same call hashes
/// alike however its arguments were ordered.
pub(crate) fn args_hash(tool: &str, args: &Value) -> String {
    crate::sha::sha256_hex(json!({"tool": tool, "args": args}).to_string().as_bytes())
}

fn target_of(caller: &ToolCaller) -> Target {
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

    /// The work behind `caller`, admitted for `server`: a subagent's — its
    /// own call, or a run it started, directly or through others — never is
    /// (`delegation_not_permitted`: a template's `subagents: none`, all P1
    /// has); anyone else's as [`Delegation::admit`] decides.
    pub(crate) fn delegation_admit(
        &self,
        d: &Delegation,
        caller: &ToolCaller,
        server: &str,
    ) -> Result<crate::delegation::Admitted, crate::delegation::DelegationError> {
        if let Some(h) = self.subagent_lineage(caller) {
            return Err(crate::delegation::DelegationError {
                code: crate::delegation::Code::DelegationNotPermitted,
                message: format!("subagent:{h}'s work acts with no one's delegated grant"),
            });
        }
        d.admit(caller.principal.as_deref(), server)
    }

    /// Whether `server`'s tools are offered to work for `owner`: only with a
    /// usable grant.
    pub(crate) fn delegation_usable(&self, owner: Option<&str>, server: &str) -> bool {
        self.delegation
            .as_ref()
            .is_some_and(|d| d.usable(owner, server))
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

    /// The consent rule a call meets, if any (the module's rules 1–3): an
    /// `egress` call from tainted work holding delegated data, a delegated
    /// `sensitive` or `egress` call from tainted work, a delegated `egress`
    /// call.
    fn consent_rule(
        &self,
        caller: &ToolCaller,
        name: &str,
    ) -> Option<(&'static str, Option<String>)> {
        use crate::sec::scope::TrifectaTag;
        let tags = self
            .registry
            .tags_of(std::slice::from_ref(&name.to_string()));
        let server = self.delegated_server_of(name);
        let egress = tags.contains(&TrifectaTag::Egress);
        let sensitive = tags.contains(&TrifectaTag::Sensitive);
        if !egress && !(server.is_some() && sensitive) {
            return None;
        }
        let facts = self.caller_facts(caller);
        if egress && facts.tainted && facts.holds_delegated_data {
            return Some(("egress_from_tainted_work_holding_delegated_data", server));
        }
        if server.is_some() && facts.tainted {
            return Some(("delegated_call_from_tainted_work", server));
        }
        if server.is_some() && egress {
            return Some(("delegated_egress", server));
        }
        None
    }

    /// The person's consent before `name`: `None` to go on, or the gate (or
    /// the refusal) the call meets. A delegated call that could not be made
    /// at all is refused here, before anyone is asked.
    pub(crate) fn consent_check(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        passed: super::reactor::Passed,
    ) -> Option<ToolOutcome> {
        let d = self.delegation.clone()?;
        let (rule, server) = self.consent_rule(caller, name)?;
        if let Some(s) = &server
            && let Err(e) = self.delegation_admit(&d, caller, s)
        {
            return Some(ToolOutcome::Ready(
                Value::String(format!("{name}: {e}")),
                true,
            ));
        }
        Some(self.open_consent(&d, caller, name, args, rule, server, passed, false))
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

    /// A workflow step whose own effect would send a run's delegated data
    /// off the box, from a run that is tainted, waits for the person, as an
    /// egress tool call from that run does (§5.13 item 6: "otherwise the
    /// exfiltration simply moves to an `http` node"). `true` when the step
    /// was suspended on the gate.
    pub(crate) fn step_consent(
        &mut self,
        run_id: &str,
        step_id: &str,
        kind: &str,
        spec: &Map<String, Value>,
        caller: &ToolCaller,
    ) -> bool {
        let Some(d) = self.delegation.clone() else {
            return false;
        };
        if !self.step_egress(kind, spec) {
            return false;
        }
        let facts = self.run_facts(run_id);
        if !(facts.tainted && facts.holds_delegated_data) {
            return false;
        }
        let outcome = self.open_consent(
            &d,
            caller,
            kind,
            &Value::Object(spec.clone()),
            "egress_from_tainted_work_holding_delegated_data",
            None,
            super::reactor::Passed::default(),
            true,
        );
        self.step_tool_outcome(run_id, step_id, kind, outcome);
        true
    }

    /// Open the person's gate on `name` with `args` under `rule`.
    #[allow(clippy::too_many_arguments)]
    fn open_consent(
        &mut self,
        d: &Delegation,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        rule: &'static str,
        server: Option<String>,
        passed: super::reactor::Passed,
        effect: bool,
    ) -> ToolOutcome {
        // Answered by the person: the run's or the conversation's own, who
        // may hold a grant. Work for nobody who could answer is denied.
        let Some(person) = d.person.as_ref().map(|p| p.id.clone()) else {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "{name}: delegation refused (policy_denied): the call needs the person's approval and no per-person operator is declared"
                )),
                true,
            );
        };
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
        let mut rendered = json!({
            "server": server, "tool": name, "args": args, "args_length": text.len(),
            "scope": scope, "run": caller.run, "step": caller.step, "context": caller.ctx,
            "rule": rule, "principal": person,
        });
        if let (Value::Object(r), Value::Object(f)) = (&mut rendered, facts.to_value()) {
            r.extend(f);
        }
        let timeout = self.settings.identity.delegation.consent_timeout();
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

    #[cfg(feature = "a2a")]
    /// The person's answer to gate `id`: run the call it held, or refuse it.
    /// `None` when there is no such gate; `Err` when the answer is not one
    /// the gate takes.
    pub(crate) fn consent_answer(
        &mut self,
        id: &str,
        by: &str,
        nonce: Option<&str>,
        args_hash: Option<&str>,
        approve: bool,
    ) -> Result<String, String> {
        let gate = self
            .consents
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no open consent gate is {id:?}"))?;
        if gate.principal != by {
            self.consent_refused(id, by, "not_the_person");
            return Err(format!(
                "gate {id} is {}'s to answer, and {by} is not them: no operator answers for the person",
                gate.principal
            ));
        }
        if nonce != Some(gate.nonce.as_str())
            || (approve && args_hash != Some(gate.args_hash.as_str()))
        {
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
        let found = self.pending.iter().position(|p| {
            p.target == gate.target && matches!(&p.kind, PendingKind::Consent { id: g } if g == id)
        });
        if let Some(i) = found {
            self.pending.remove(i);
        }
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": decision, "via": "operator_self", "principal": by,
                   "args_hash": gate.args_hash}),
        );
        self.audit_consent(id, &gate, decision, "operator_self", personal);
        let tool = gate.call.tool.clone();
        if found.is_none() {
            return Ok(format!(
                "gate {id} is closed: the work that asked has ended"
            ));
        }
        if approve {
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
    /// judge, never an override — and a gate whose asker is gone closes.
    pub(crate) fn poll_consents(&mut self) {
        let now = now_ms();
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
        let Some(g) = self.consents.remove(id) else {
            return;
        };
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
                    "{}: delegation refused (policy_denied): {why}; it was NOT executed",
                    g.call.tool
                )),
                true,
            );
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
    /// The open gates `who` sees: every gate, by its tool and server; what
    /// it shows — the arguments, the text the person reads — and its
    /// binding only for the person who answers it.
    pub(crate) fn consent_list(&self, who: &str) -> Vec<Value> {
        self.consents
            .iter()
            .map(|(id, g)| {
                let mut v = json!({"gate": id, "principal": g.principal,
                                   "tool": g.rendered["tool"], "server": g.rendered["server"],
                                   "deadline_ms": g.deadline_ms});
                if g.principal == who {
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
                              "effect": g.effect}),
            None => json!({"kind": "consent", "gate": id}),
        }
    }

    /// At start: each workflow step suspended on a consent gate asks again,
    /// with the same binding and deadline.
    pub(crate) fn rebuild_consents(&mut self) {
        let mut found = Vec::new();
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
        let unit = caller
            .run
            .clone()
            .unwrap_or_else(|| format!("ctx:{}", caller.context_id()));
        let reads = reads_of(caller);
        let trusted = d.servers.get(server).is_some_and(|s| s.trusted_output);
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let (server, tool, authority) = (
            server.to_string(),
            tool.to_string(),
            caller.principal.clone(),
        );
        let (run, step) = (caller.run.clone(), caller.step.clone());
        let name = name.to_string();
        std::thread::Builder::new()
            .name(format!("delegated:{server}.{tool}"))
            .spawn(move || {
                let start = crate::obs::otel::now_unix_nanos();
                let res = d.call(Call {
                    server: &server,
                    tool: &tool,
                    args,
                    meta,
                    authority: authority.as_deref(),
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
        }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "Answer with agentd --delegation-approve g1 or agentd --delegation-deny g1. No message is an answer."
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
