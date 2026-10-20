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
//! unit carrying the origin `delegated:<server>`, written before the result
//! is read, so every later call from that unit is judged as one from work
//! that holds a person's delegated data.
//!
//! **Consent** (§5.10, §5.13 item 6, as P1 ships it). There is no taint
//! engine yet, so every unit that touches a delegated server is treated as
//! tainted and as holding delegated data, and the shipped rules reduce to
//! two, both hard-coded:
//!
//! 1. a delegated call to a `sensitive` or `egress` tool asks the person —
//!    rules 2, 3 and 4 of §5.13 item 6 all hold for every such call;
//! 2. an `egress` call of any kind — delegated or under agentd's own
//!    credential — from a unit that holds delegated data asks the person
//!    (rule 1), so the data does not leave through another server. A
//!    workflow step that sends the run's data out by its own effect rather
//!    than a tool call is one too: an `http` call, an `a2a.send` or
//!    `a2a.delegate`, an `emit` that forwards the event out, an `mcp.tool`
//!    or `mcp.resource` at a server whose tools are tagged `egress`
//!    ([`Runtime::step_consent`]). Approved, the step runs again past the
//!    gate with the spec the person saw.
//!
//! A gate is a [`PendingKind::Consent`]: addressed to the person, answered
//! only by them over the unix A2A listener (`agentd --delegation-approve
//! <gate>` / `--delegation-deny <gate>`, recorded `via: operator_self`),
//! bound to its `nonce` and the hash of the call's canonical arguments,
//! rendered by agentd as data — server, tool, the full arguments, their
//! length, the scopes the token carries, the run, `tainted` — and never
//! answered by a judge, an operator override or a message on the asking
//! task. Unanswered within `identity.delegation.consent_timeout` it denies.
//! A workflow step's gate is kept in the step's wait record and re-armed at
//! the next start; a turn's dies with its turn.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::events::Event;
use super::reactor::{PendingKind, PolicyCall, Runtime, Target};
use super::tools::{ExecTarget, ToolCaller, ToolOutcome};
use crate::delegation::{Call, Delegation, Note};
use crate::state::now_ms;

/// The origin a delegated result carries into the unit it reaches.
pub(crate) fn origin(server: &str) -> String {
    format!("delegated:{server}")
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

    /// The consent rule a call meets, if any: a delegated `sensitive` or
    /// `egress` call, or an `egress` call from work holding delegated data.
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
        if server.is_some() && (egress || tags.contains(&TrifectaTag::Sensitive)) {
            return Some(("delegated_sensitive_or_egress", server));
        }
        if egress
            && self
                .caller_carries(caller)
                .origins
                .iter()
                .any(|o| o.starts_with("delegated:"))
        {
            return Some(("egress_holding_delegated_data", server));
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
    /// off the box waits for the person, as an egress tool call from that
    /// run does (§5.13 item 6: "otherwise the exfiltration simply moves to
    /// an `http` node"). `true` when the step was suspended on the gate.
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
        if !self.step_egress(kind, spec)
            || !self
                .run_carries(run_id)
                .origins
                .iter()
                .any(|o| o.starts_with("delegated:"))
        {
            return false;
        }
        let outcome = self.open_consent(
            &d,
            caller,
            kind,
            &Value::Object(spec.clone()),
            "egress_holding_delegated_data",
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
        let rendered = json!({
            "server": server, "tool": name, "args": args, "args_length": text.len(),
            "scope": scope, "run": caller.run, "step": caller.step, "context": caller.ctx,
            "tainted": true, "rule": rule, "principal": person,
        });
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
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": "asked", "rendered": rendered,
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
            return Err(format!(
                "gate {id} is {}'s to answer, and {by} is not them: no operator answers for the person",
                gate.principal
            ));
        }
        if nonce != Some(gate.nonce.as_str())
            || (approve && args_hash != Some(gate.args_hash.as_str()))
        {
            return Err(format!(
                "the answer to gate {id} is not bound to its nonce and arguments: nothing was decided"
            ));
        }
        self.consents.remove(id);
        let found = self.pending.iter().position(|p| {
            p.target == gate.target && matches!(&p.kind, PendingKind::Consent { id: g } if g == id)
        });
        if let Some(i) = found {
            self.pending.remove(i);
        }
        let decision = if approve { "approved" } else { "denied" };
        self.log.info(
            "delegation.consent",
            json!({"gate": id, "decision": decision, "via": "operator_self", "principal": by,
                   "args_hash": gate.args_hash, "rendered": gate.rendered}),
        );
        self.audit(super::audit::AuditEvent {
            action: "delegation.consent",
            target: json!({"gate": id, "decision": decision, "via": "operator_self",
                           "args_hash": gate.args_hash, "rendered": gate.rendered,
                           "decided_at": now_ms()}),
            outcome: decision,
            principal: Some(by),
            role: Some("operator"),
            request_id: None,
            sid: None,
            trace: None,
            link: None,
        });
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
            let Some(g) = self.consents.remove(&id) else {
                continue;
            };
            let found = self.pending.iter().position(|p| {
                p.target == g.target
                    && matches!(&p.kind, PendingKind::Consent { id: x } if *x == id)
            });
            self.log.info(
                "delegation.consent",
                json!({"gate": id, "decision": "denied", "via": "on_timeout"}),
            );
            if let Some(i) = found {
                self.pending.remove(i);
                self.reply(
                    &g.target,
                    Value::String(format!(
                        "{}: delegation refused (policy_denied): nobody approved this call in time — on_timeout deny; it was NOT executed",
                        g.call.tool
                    )),
                    true,
                );
            }
        }
    }

    #[cfg(feature = "a2a")]
    /// The open gates `who` sees: every gate, with its binding only for the
    /// person who answers it.
    pub(crate) fn consent_list(&self, who: &str) -> Vec<Value> {
        self.consents
            .iter()
            .map(|(id, g)| {
                let mut v = json!({"gate": id, "rendered": g.rendered, "principal": g.principal,
                                   "deadline_ms": g.deadline_ms});
                if g.principal == who {
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
                        // delegated data, written before the result is read.
                        if let Some(into) = reads {
                            let _ = tx.send(Event::CompanyRead {
                                into,
                                origin: origin(&server),
                                why: format!(
                                    "{server}'s delegated results are the person's own data"
                                ),
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
}
