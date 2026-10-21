// SPDX-License-Identifier: AGPL-3.0-only
//! Firings of `act_for` workflows, and the waits on a person a run makes
//! when nobody else can decide (RFC 0044 M4, §5.12 C and E).
//!
//! **A firing** of an `act_for` workflow's `schedule`, `loop` or `once`
//! start acts for the person the workflow names — only while their grant
//! holds their consent to this very definition and their template allows
//! the workflow's `via` ([`crate::delegation::Delegation::firing_ready`]).
//! Otherwise it starts no run, says so (`start.unattended.suspended`) and
//! records it: nothing runs as them, and nobody is asked. A firing's run
//! acts through a session id of its own, which every run, turn and child
//! its work starts acts through too; it is registered with the delegation
//! supervisor while the run lives, again after a restart, and ends with the
//! run — and with it the approval a `via: ciba` firing held, and any
//! refresh token an IdP issued with that approval anyway, revoked. A run
//! its work starts is a firing of its own, held to the person's consent to
//! that run's own definition ([`Runtime::firing_carried`]); a conversation
//! turn its work starts by message acts for nobody's grant.
//!
//! Its work is unattended: tainted — nobody reads what it does — so the
//! person's consent rules ask them, and deny unanswered; and a delegated
//! tool tagged `egress` it calls is refused, never asked about, unless the
//! person's template lists it in `unattended_tools` (T18).
//!
//! **A `via: ciba` firing's first delegated call at a grant** waits on a
//! gate of kind `ciba`: the delegation supervisor asks the person on their
//! own device, off the loop, and the gate's request is polled on the
//! reactor's passes until they approve — the call is then made again with
//! their approval as its subject — or deny, or it expires (`ciba_denied`).
//! The firing's run records how it asked — written before the request is
//! sent — so a second call never asks again: another call of the firing
//! made while the request waits waits on the same request. A restart loses
//! the request, or the approval it gave, and the firing asks its person
//! once more (RFC 0044 §7.P3c): a step that was waiting waits on its gate
//! again, for a new request; a later call that finds no approval asks.
//! Nothing of the old request is used, and the record says it asked again,
//! so a second restart fails the call closed; a request denied, run out or
//! never sent is never asked again. Once the wait closes, however it does,
//! the firing waits on its person no longer, and another firing of the
//! workflow may ask. The step shows suspended on the wait — `{kind:
//! consent, signin: {how: ciba}}` — for up to the request's expiry, the
//! second wait as the first; the run's task, when it has one, shows
//! `TASK_STATE_AUTH_REQUIRED` with agentd's words; the person sees the
//! request on their device — a second one says `again` — and the gate
//! among their own in `delegation.list`.
//!
//! **A step whose grant is gone** — revoked, expired, or the session its
//! run acted through ended — at a server whose `auth.on_revoked` is `ask`
//! waits on a gate of kind `reenrol`, for at most
//! `identity.delegation.consent_timeout`: only a workflow step's own call
//! from work that has read no outside text, never a model's (T33). It is
//! checked at every pass, with no network: once the person has enrolled
//! again — the same principal and IdP subject — or, for a run whose
//! session ended, once a live session of theirs holds a proof of that very
//! identity (the run moves to it, recorded), the step's call is made again.

use serde_json::{Value, json};

use super::reactor::Runtime;
#[cfg(feature = "a2a")]
use super::reactor::{PendingKind, PolicyCall, Target};
#[cfg(feature = "a2a")]
use super::stepup::{CibaWait, How, SignIn, StepUpAsk};
use super::tools::{ToolCaller, ToolOutcome};
use crate::delegation::unattended::{ActForDecl, CibaOutcome, Firing};
use crate::engine::model::{ActFor, Workflow};
#[cfg(feature = "a2a")]
use crate::engine::run::Asked;
use crate::engine::run::UNATTENDED_SID_PREFIX;
use crate::state::InboxEvent;
#[cfg(feature = "a2a")]
use crate::state::now_ms;

/// The starts whose firings act for an `act_for` workflow's person.
const FIRING_STARTS: &[&str] = &["schedule", "loop", "once"];

/// How a firing's CIBA request moved on, sent by the thread that asked.
#[derive(Debug)]
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
pub enum CibaProgress {
    /// The IdP took the request.
    Sent(crate::auth::ciba::Started),
    /// A poll's answer, or why no request was sent.
    Polled(CibaOutcome),
}

impl Runtime {
    /// Whether start event `ev` of `w`, at start `node`, is a firing that
    /// acts for a person: `Ok(Some((principal, sid, act_for)))` — the
    /// principal its run runs as and the firing's session id — `Ok(None)`
    /// when it is not one, and `Err(())` when it may not act for them now,
    /// said and recorded, and no run is to start.
    pub(crate) fn firing_of(
        &mut self,
        ev: &InboxEvent,
        w: &Workflow,
        node: &str,
    ) -> Result<Option<(String, String, ActFor)>, ()> {
        let Some(a) = w.act_for.clone() else {
            return Ok(None);
        };
        let kind = w.step(node).map(|s| s.kind.as_str()).unwrap_or_default();
        if ev.kind != super::events::kinds::START_FIRED
            || !FIRING_STARTS.contains(&kind)
            || !w.configured
            || w.document
        {
            return Ok(None);
        }
        let Some(d) = self.delegation.clone() else {
            return Ok(None);
        };
        let decl = ActForDecl {
            workflow: w.name.clone(),
            digest: w.hash.clone(),
            via: a.via,
            principal: a.principal.clone(),
        };
        match d.firing_ready(&decl) {
            Ok(principal) => {
                let sid = format!(
                    "{UNATTENDED_SID_PREFIX}{}",
                    crate::sec::random::b64url_token(12).unwrap_or_default()
                );
                Ok(Some((principal, sid, a)))
            }
            Err(e) => {
                let target = json!({"workflow": w.name, "node": node, "kind": kind,
                                    "via": a.via.as_str(), "code": e.code.as_str(),
                                    "why": e.message, "digest": &w.hash[..w.hash.len().min(12)]});
                self.log
                    .in_trace(ev.trace.as_ref())
                    .warn("start.unattended.suspended", target.clone());
                self.audit(super::audit::AuditEvent {
                    action: "delegation.unattended",
                    target,
                    outcome: "suspended",
                    principal: d.firing_principal(&a.principal).as_deref(),
                    role: None,
                    request_id: None,
                    sid: None,
                    trace: ev.trace.as_ref(),
                    link: None,
                });
                // A loop fires again when its run ends; a firing that
                // started none waits its interval — or a minute — and fires
                // again then.
                if kind == "loop" {
                    let wait = w
                        .step(node)
                        .and_then(|s| s.spec.get("interval"))
                        .and_then(Value::as_str)
                        .and_then(|d| crate::config::parse_duration(d).ok())
                        .unwrap_or(std::time::Duration::from_secs(60));
                    let mut st = self.start_state_pub(&w.name, node);
                    st["next_ms"] = json!(crate::state::now_ms() + wait.as_millis() as u64);
                    self.set_start_state_pub(&w.name, node, st);
                }
                Err(())
            }
        }
    }

    /// Whether start event `ev` of `w` comes from a firing's work — a
    /// `workflow` step, `workflow.run`, a signal or a message that starts a
    /// run — and so starts a firing of its own: `Some((principal, sid,
    /// act_for))`, its run acting for the same person through a session of
    /// its own, held to their consent to `w` at its own digest — which a
    /// workflow declaring no `act_for` naming them never has — by `w`'s own
    /// `via` where it declares one for them. `None` when it does not come
    /// from one, or that firing has ended: its run then acts through the
    /// ended firing's session, for nobody.
    pub(crate) fn firing_carried(
        &self,
        ev: &InboxEvent,
        w: &Workflow,
    ) -> Option<(String, String, ActFor)> {
        let from = ev
            .session
            .as_deref()
            .filter(|s| s.starts_with(UNATTENDED_SID_PREFIX))?;
        let d = self.delegation.as_ref()?;
        let (parent, principal) = d.firing_at(from)?;
        let via = w
            .act_for
            .as_ref()
            .filter(|a| a.principal == principal)
            .map_or(parent.via, |a| a.via);
        let runs_as = ev
            .principal
            .clone()
            .or_else(|| d.firing_principal(&principal))?;
        let sid = format!(
            "{UNATTENDED_SID_PREFIX}{}",
            crate::sec::random::b64url_token(12).unwrap_or_default()
        );
        self.log.in_trace(ev.trace.as_ref()).info(
            "start.unattended.carried",
            json!({"workflow": w.name, "from_workflow": parent.workflow, "from_run": parent.run,
                   "via": via.as_str()}),
        );
        Some((runs_as, sid, ActFor { principal, via }))
    }

    /// Run `run_id` started as a firing acting through `sid`: the delegation
    /// supervisor holds its authority from now on.
    pub(crate) fn firing_began(&self, run_id: &str, sid: &str, w: &Workflow, a: &ActFor) {
        if let Some(d) = &self.delegation {
            d.firing_started(
                Firing {
                    sid: sid.to_string(),
                    run: run_id.to_string(),
                    workflow: w.name.clone(),
                    digest: w.hash.clone(),
                    via: a.via,
                },
                a.principal.clone(),
                &Default::default(),
            );
        }
    }

    /// At start: every firing whose run a restart found still going acts
    /// again, as having asked as its run says — what its requests and
    /// approvals gave it ended with the last life; and every refresh token
    /// an approval left in the store is revoked at its IdP, off the loop,
    /// before any request is sent.
    pub(crate) fn restore_firings(&self) {
        let Some(d) = &self.delegation else {
            return;
        };
        d.sweep_left_approvals();
        for r in self.runs.values().filter(|r| !r.status.is_terminal()) {
            let (Some(u), Some(sid)) = (&r.unattended, &r.session) else {
                continue;
            };
            d.firing_started(
                Firing {
                    sid: sid.clone(),
                    run: r.id.clone(),
                    workflow: r.workflow.clone(),
                    digest: r.workflow_hash.clone(),
                    via: u.via,
                },
                u.principal.clone(),
                &u.asked,
            );
        }
    }

    /// How firing `sid`'s run's record says it asked its person at `grant`.
    #[cfg(feature = "a2a")]
    fn firing_record(&self, sid: &str, grant: &str) -> Option<Asked> {
        self.runs
            .values()
            .filter(|r| r.session.as_deref() == Some(sid))
            .find_map(|r| r.unattended.as_ref())?
            .asked
            .get(grant)
            .copied()
    }

    /// Firing `sid`'s run's record says it asked its person at `grant`
    /// `how`: written, and checkpointed, before a request is sent.
    #[cfg(feature = "a2a")]
    fn firing_records(&mut self, sid: &str, grant: &str, how: Asked) {
        let Some(r) = self
            .runs
            .values_mut()
            .find(|r| r.session.as_deref() == Some(sid) && r.unattended.is_some())
        else {
            return;
        };
        if let Some(u) = r.unattended.as_mut() {
            u.asked.insert(grant.to_string(), how);
        }
        r.touch();
        self.checkpoint(false);
    }

    /// Run `run_id` ended: when it was a firing, the firing ends with it —
    /// off the loop, since a refresh token an IdP issued with its approval
    /// is revoked there.
    pub(crate) fn firing_run_ended(&self, run_id: &str) {
        let (Some(d), Some(r)) = (&self.delegation, self.runs.get(run_id)) else {
            return;
        };
        let (Some(_), Some(sid)) = (&r.unattended, r.session.clone()) else {
            return;
        };
        let d = std::sync::Arc::clone(d);
        std::thread::Builder::new()
            .name("delegation-firing-end".into())
            .spawn(move || d.firing_ended(&sid))
            .ok();
    }

    /// Whether `caller`'s work is a firing's: nobody is there.
    pub(crate) fn firing_work(caller: &ToolCaller) -> bool {
        caller
            .session
            .as_deref()
            .is_some_and(|s| s.starts_with(UNATTENDED_SID_PREFIX))
    }

    /// A firing's delegated call of tool `name` that sends data off the box
    /// (`egress`), refused — never asked about — unless the person's
    /// template lists that very tool in `unattended_tools` (T18). `None` to
    /// go on.
    pub(crate) fn unattended_egress(
        &self,
        d: &crate::delegation::Delegation,
        caller: &ToolCaller,
        name: &str,
    ) -> Option<ToolOutcome> {
        use crate::registry::Route;
        use crate::sec::scope::TrifectaTag;
        if !Self::firing_work(caller) {
            return None;
        }
        let (server, tool) = match self.registry.route(name)? {
            Route::Mcp { server, tool } => (server.to_string(), tool.to_string()),
            Route::Mapped(m) => (m.server.clone(), m.tool.clone()),
            _ => return None,
        };
        if !d.is_delegated(&server)
            || !self
                .registry
                .tags_of(std::slice::from_ref(&name.to_string()))
                .contains(&TrifectaTag::Egress)
        {
            return None;
        }
        let admitted = self.delegation_admit(d, caller, &server).ok()?;
        let full = format!("{server}.{tool}");
        if admitted.template.unattended_tools.contains(&full) {
            return None;
        }
        let target = json!({"server": server, "tool": full, "principal": admitted.who.id,
                            "run": caller.run, "step": caller.step,
                            "template": admitted.template.name});
        self.log
            .warn("delegation.unattended.refused", target.clone());
        self.audit(super::audit::AuditEvent {
            action: "delegation.unattended",
            target,
            outcome: "refused",
            principal: Some(&admitted.who.id),
            role: None,
            request_id: None,
            sid: None,
            trace: self.caller_trace(caller).as_ref(),
            link: None,
        });
        Some(ToolOutcome::Ready(
            Value::String(format!(
                "{name}: delegation refused (policy_denied): this firing acts for {} with nobody there, and {full} sends data off the box — their template {} lists it in no unattended_tools; it was NOT executed",
                admitted.who.id, admitted.template.name
            )),
            true,
        ))
    }
}

#[cfg(feature = "a2a")]
impl Runtime {
    /// A `via: ciba` firing's call needs its person's approval: the call
    /// waits on a `ciba` gate, and the person is asked on their own device
    /// — once a firing and grant, a second call of the firing waiting on
    /// the same request.
    pub(crate) fn open_ciba(&mut self, a: StepUpAsk) {
        let StepUpAsk {
            caller,
            name,
            server,
            args,
            error,
        } = a;
        let target = super::delegated::target_of(&caller);
        let fail = |rt: &mut Runtime, why: String| rt.reply(&target, Value::String(why), true);
        let Some(d) = self.delegation.clone() else {
            return fail(self, error);
        };
        let admitted = match self.delegation_admit(&d, &caller, &server) {
            Ok(a) => a,
            Err(e) => return fail(self, format!("{name}: {e}")),
        };
        let Some(f) = admitted.who.firing.clone() else {
            return fail(self, error);
        };
        let grant = admitted.grant.grant_id.clone();
        let idp = admitted.grant.idp.clone();
        let person = admitted.who.id.clone();
        let leader = self
            .consents
            .iter()
            .find(|(_, g)| {
                g.signin.as_ref().is_some_and(|s| {
                    s.how == How::Ciba
                        && s.follows.is_none()
                        && s.firing.as_deref() == Some(f.sid.as_str())
                        && s.grant == grant
                })
            })
            .map(|(id, _)| id.clone());
        // A firing asks its person once at a grant, and once more after a
        // restart lost that request or its approval: its run's record says
        // how, written before the request is sent.
        let mut again = false;
        if leader.is_none() {
            again = match self.firing_record(&f.sid, &grant) {
                None => false,
                Some(Asked::Once) if !d.firing_asked(&f.sid, &grant) => true,
                Some(_) => {
                    return fail(
                        self,
                        format!(
                            "{name}: delegation refused (subject_expired): this firing asked {person} once — once more after a restart — and asks no more"
                        ),
                    );
                }
            };
            if let Err(e) = d.ciba_asking(&f.sid, &grant) {
                return fail(self, format!("{name}: {e}"));
            }
            self.firing_records(
                &f.sid,
                &grant,
                if again { Asked::Again } else { Asked::Once },
            );
        }
        let expiry = d.providers.get(&idp).map_or(
            crate::config::settings::delegation::CIBA_REQUEST_EXPIRY,
            |p| p.cfg.ciba.request_expiry(),
        );
        let id = crate::sec::random::b64url_token(9).unwrap_or_default();
        let nonce = crate::sec::random::b64url_token(16).unwrap_or_default();
        let rendered = json!({"tool": name, "server": server, "rule": "ciba", "principal": person,
                              "kind": "ciba", "workflow": f.workflow, "run": f.run});
        let task = self.signin_task(&caller, &id, &rendered, How::Ciba, &json!({}));
        let gate = super::delegated::ConsentGate {
            nonce,
            args_hash: super::delegated::args_hash(&name, &args),
            rendered,
            principal: person.clone(),
            deadline_ms: now_ms() + expiry.as_millis() as u64 + 60_000,
            call: PolicyCall {
                tool: name.clone(),
                args,
                rule: 0,
                on_timeout: crate::config::settings::PolicyAction::Deny,
                company: None,
                consent: true,
                passed: Default::default(),
                judged: None,
            },
            target: target.clone(),
            effect: false,
            operator_approves: false,
            task,
            signin: Some(SignIn {
                how: How::Ciba,
                idp: idp.clone(),
                server: server.clone(),
                grant: grant.clone(),
                max_age: 0,
                start_uri: None,
                device: None,
                ciba: None,
                firing: Some(f.sid.clone()),
                follows: leader.clone(),
                asked_s: 0,
                before_rules: false,
            }),
            session: caller.session.clone(),
            once: None,
            once_binding: None,
        };
        self.consents.insert(id.clone(), gate);
        self.log.info(
            "delegation.ciba",
            json!({"gate": id, "decision": match (&leader, again) {
                       (Some(_), _) => "waits",
                       (None, true) => "asked_again",
                       (None, false) => "asked",
                   },
                   "follows": leader, "workflow": f.workflow, "run": f.run, "server": server,
                   "tool": name, "principal": person}),
        );
        if leader.is_none() {
            self.send_ciba(&d, &id, &f, &grant, &name, again);
        }
        let outcome = ToolOutcome::Deferred(PendingKind::Consent { id });
        match &target {
            Target::Step(run, step) => {
                let (run, step) = (run.clone(), step.clone());
                self.step_tool_outcome(&run, &step, &name, outcome);
            }
            Target::Child(node, req) => {
                let (node, req) = (*node, *req);
                self.child_tool_outcome(node, req, &name, outcome);
            }
            _ => {}
        }
    }

    /// Ask firing `f`'s person at `grant` for gate `gate`'s call of `tool`,
    /// off the loop: the binding message their device shows is agentd's
    /// identifiers alone, `again` for the request a restart made it send.
    fn send_ciba(
        &self,
        d: &std::sync::Arc<crate::delegation::Delegation>,
        gate: &str,
        f: &Firing,
        grant: &str,
        tool: &str,
        again: bool,
    ) {
        let binding = crate::auth::ciba::binding_message(&[
            "agentd",
            &self.instance,
            &f.workflow,
            "run",
            &f.run[f.run.len().saturating_sub(8)..],
            if again { "again" } else { "" },
            tool,
        ]);
        let (d, tx, gate, sid, grant) = (
            std::sync::Arc::clone(d),
            self.events_tx.clone(),
            gate.to_string(),
            f.sid.clone(),
            grant.to_string(),
        );
        std::thread::Builder::new()
            .name("delegation-ciba".into())
            .spawn(move || {
                let progress = match d.start_ciba(&sid, &grant, &binding) {
                    Ok(st) => CibaProgress::Sent(st),
                    Err(e) => CibaProgress::Polled(CibaOutcome::Refused {
                        code: e.code,
                        why: e.message,
                    }),
                };
                let _ = tx.send(super::events::Event::Ciba { gate, progress });
            })
            .ok();
    }

    /// After a restart: the `ciba` gates workflow steps waited on, rebuilt
    /// from their wait records. The request each waited on ended with the
    /// last life, and nothing of it is used: a firing that had asked its
    /// person once asks them once more, the step waiting on the same gate
    /// for the new request — one per firing and grant, the other steps
    /// that waited on it waiting on it again — and one that had asked once
    /// more already, or whose firing is gone, fails the call closed.
    pub(crate) fn ask_rebuilt_cibas(&mut self, ids: &[String]) {
        let mut leaders: Vec<(String, String, String)> = Vec::new();
        for id in ids {
            let Some((sid, grant)) = self
                .consents
                .get(id)
                .and_then(|g| g.signin.as_ref())
                .and_then(|s| Some((s.firing.clone()?, s.grant.clone())))
            else {
                continue;
            };
            let first = leaders
                .iter()
                .find(|(_, s, g)| *s == sid && *g == grant)
                .map(|(l, _, _)| l.clone());
            if let Some(s) = self.consents.get_mut(id).and_then(|g| g.signin.as_mut()) {
                s.follows = first.clone();
            }
            if first.is_none() {
                leaders.push((id.clone(), sid, grant));
            }
        }
        let Some(d) = self.delegation.clone() else {
            return;
        };
        for (id, sid, grant) in leaders {
            let firing = d.firing_at(&sid).map(|(f, _)| f);
            let fresh = matches!(self.firing_record(&sid, &grant), None | Some(Asked::Once));
            let asking = match &firing {
                Some(_) if fresh => d.ciba_asking(&sid, &grant).map_err(|e| e.message),
                Some(_) => Err("a restart ended the wait for the person's approval of this firing, after it had asked them once more, and a firing asks no more".into()),
                None => Err("a restart ended the wait for the person's approval, and the firing it was for with it".into()),
            };
            let (Some(f), Ok(())) = (firing, &asking) else {
                self.close_signin(&id, &asking.err().unwrap_or_default());
                continue;
            };
            self.firing_records(&sid, &grant, Asked::Again);
            let expiry = d
                .providers
                .get(
                    self.consents
                        .get(&id)
                        .and_then(|g| g.signin.as_ref())
                        .map_or("", |s| s.idp.as_str()),
                )
                .map_or(
                    crate::config::settings::delegation::CIBA_REQUEST_EXPIRY,
                    |p| p.cfg.ciba.request_expiry(),
                );
            let tool = match self.consents.get_mut(&id) {
                Some(g) => {
                    g.deadline_ms = now_ms() + expiry.as_millis() as u64 + 60_000;
                    g.call.tool.clone()
                }
                None => continue,
            };
            self.log.info(
                "delegation.ciba",
                json!({"gate": id, "decision": "asked_again", "workflow": f.workflow, "run": f.run,
                       "tool": tool}),
            );
            self.send_ciba(&d, &id, &f, &grant, &tool, true);
        }
    }

    /// Gate `gate`'s CIBA request moved on.
    pub(crate) fn ciba_progress(&mut self, gate: &str, progress: CibaProgress) {
        match progress {
            CibaProgress::Sent(st) => {
                let now = now_ms();
                let expires_ms = now + st.expires_in * 1000;
                let Some(g) = self.consents.get_mut(gate) else {
                    return;
                };
                g.deadline_ms = g.deadline_ms.max(expires_ms + 30_000);
                if let Some(s) = g.signin.as_mut() {
                    s.ciba = Some(CibaWait {
                        auth_req_id: st.auth_req_id,
                        interval_ms: st.interval * 1000,
                        next_ms: now + st.interval * 1000,
                        expires_ms,
                        polling: false,
                    });
                }
                self.log.info(
                    "delegation.ciba",
                    json!({"gate": gate, "decision": "sent", "expires_in": st.expires_in,
                           "interval": st.interval}),
                );
            }
            CibaProgress::Polled(CibaOutcome::Pending) => self.ciba_waits(gate, 0),
            CibaProgress::Polled(CibaOutcome::SlowDown) => self.ciba_waits(gate, 5_000),
            CibaProgress::Polled(CibaOutcome::Approved) => self.ciba_approved(gate),
            CibaProgress::Polled(CibaOutcome::Refused { code, why }) => {
                self.ciba_refused(gate, code, &why)
            }
        }
    }

    /// Gate `gate`'s request is not answered yet: poll again after its
    /// interval, `more` milliseconds longer from now on (CIBA §11).
    fn ciba_waits(&mut self, gate: &str, more: u64) {
        if let Some(w) = self
            .consents
            .get_mut(gate)
            .and_then(|g| g.signin.as_mut())
            .and_then(|s| s.ciba.as_mut())
        {
            w.polling = false;
            w.interval_ms += more;
            w.next_ms = now_ms() + w.interval_ms;
        }
    }

    /// The gates that wait on gate `gate`'s request: it, and those that
    /// follow it.
    fn ciba_gates(&self, gate: &str) -> Vec<String> {
        let mut out = vec![gate.to_string()];
        out.extend(
            self.consents
                .iter()
                .filter(|(_, g)| {
                    g.signin
                        .as_ref()
                        .is_some_and(|s| s.follows.as_deref() == Some(gate))
                })
                .map(|(id, _)| id.clone()),
        );
        out
    }

    /// The person approved gate `gate`'s request: every call that waited on
    /// it is made again, their approval its subject.
    fn ciba_approved(&mut self, gate: &str) {
        for id in self.ciba_gates(gate) {
            let Some(g) = self.consents.remove(&id) else {
                continue;
            };
            self.consent_task_closed(g.task.as_deref(), &id);
            let found = self.pending.iter().position(|p| {
                p.target == g.target
                    && matches!(&p.kind, PendingKind::Consent { id: x } if *x == id)
            });
            self.log.info(
                "delegation.ciba",
                json!({"gate": id, "decision": "approved", "principal": g.principal}),
            );
            if let Some(i) = found {
                self.pending.remove(i);
                self.run_approved(&g.target, &g.call);
            }
        }
    }

    /// Gate `gate`'s request was refused, or never sent: every call that
    /// waited on it fails, `code`, and its firing asks no more at that
    /// grant, a restart included.
    fn ciba_refused(&mut self, gate: &str, code: crate::delegation::Code, why: &str) {
        if let Some((sid, grant)) = self
            .consents
            .get(gate)
            .and_then(|g| g.signin.as_ref())
            .and_then(|s| Some((s.firing.clone()?, s.grant.clone())))
        {
            self.firing_records(&sid, &grant, Asked::Refused);
        }
        for id in self.ciba_gates(gate) {
            let Some(g) = self.consents.remove(&id) else {
                continue;
            };
            self.consent_task_closed(g.task.as_deref(), &id);
            self.log.info(
                "delegation.ciba",
                json!({"gate": id, "decision": "denied", "code": code.as_str(), "why": why}),
            );
            let found = self.pending.iter().position(|p| {
                p.target == g.target
                    && matches!(&p.kind, PendingKind::Consent { id: x } if *x == id)
            });
            if let Some(i) = found {
                self.pending.remove(i);
                self.reply(
                    &g.target,
                    Value::String(format!(
                        "{}: delegation refused ({}): {why}; it was NOT executed",
                        g.call.tool,
                        code.as_str()
                    )),
                    true,
                );
            }
        }
    }

    /// Every pass: each firing's CIBA request due a poll is polled, off the
    /// loop, and one past its expiry fails the calls that wait on it; and a
    /// firing whose wait closed — expired here, denied in a display client,
    /// past its deadline, its session ended — waits on its person no
    /// longer, so another firing of its workflow may ask.
    pub(crate) fn poll_cibas(&mut self) {
        if let Some(d) = self.delegation.clone() {
            for (sid, grant) in d.firings_asking() {
                let open = self.consents.values().any(|g| {
                    g.signin.as_ref().is_some_and(|s| {
                        s.how == How::Ciba
                            && s.follows.is_none()
                            && s.firing.as_deref() == Some(sid.as_str())
                            && s.grant == grant
                    })
                });
                // Closed with no approval — denied in a display client,
                // past its deadline, its session ended: asked no more.
                if !open {
                    d.not_asking(&sid, &grant);
                    self.firing_records(&sid, &grant, Asked::Refused);
                }
            }
        }
        let now = now_ms();
        let mut expired = Vec::new();
        let mut due = Vec::new();
        for (id, g) in self.consents.iter_mut() {
            let Some(s) = g.signin.as_mut() else {
                continue;
            };
            let (Some(w), Some(sid)) = (s.ciba.as_mut(), s.firing.clone()) else {
                continue;
            };
            if now >= w.expires_ms {
                expired.push(id.clone());
            } else if !w.polling && now >= w.next_ms {
                w.polling = true;
                due.push((id.clone(), sid, s.grant.clone(), w.auth_req_id.clone()));
            }
        }
        for id in expired {
            self.ciba_refused(
                &id,
                crate::delegation::Code::CibaDenied,
                "the request expired before the person answered it",
            );
        }
        let Some(d) = self.delegation.clone() else {
            return;
        };
        for (gate, sid, grant, auth_req_id) in due {
            let (d, tx) = (std::sync::Arc::clone(&d), self.events_tx.clone());
            std::thread::Builder::new()
                .name("delegation-ciba-poll".into())
                .spawn(move || {
                    let outcome = d.poll_ciba(&sid, &grant, &auth_req_id);
                    let _ = tx.send(super::events::Event::Ciba {
                        gate,
                        progress: CibaProgress::Polled(outcome),
                    });
                })
                .ok();
        }
    }

    /// A workflow step's delegated call found its grant gone at a server
    /// whose `on_revoked` is `ask` (RFC 0044 §5.12 C): the step waits for
    /// its person to sign in again — only a step's own call, from work that
    /// has read no outside text; a model's call, or tainted or unattended
    /// work, fails as it came back. `before_rules`: the call was held at
    /// its dispatch, before the operator's rules and the person's consent
    /// were consulted, so it goes through them all once it is made again.
    pub(crate) fn open_reenrol(&mut self, a: StepUpAsk, before_rules: bool) {
        let StepUpAsk {
            caller,
            name,
            server,
            args,
            error,
        } = a;
        let target = super::delegated::target_of(&caller);
        let fail = |rt: &mut Runtime, why: String| rt.reply(&target, Value::String(why), true);
        if !matches!(target, Target::Step(..)) || caller.node.is_some() {
            return fail(self, error);
        }
        // The person's own message is theirs, though the grant it would
        // act with is gone; any other outside text taints the work.
        let tainted = Self::firing_work(&caller)
            || super::taint::Facts::of(
                &self.caller_carries(&caller),
                self.served_instruction().as_deref(),
                caller.principal.as_deref(),
            )
            .tainted;
        if tainted {
            return fail(
                self,
                format!(
                    "{error}; on_revoked: ask asks nobody for work that has read outside text, or that nobody is there for"
                ),
            );
        }
        let Some(d) = self.delegation.clone() else {
            return fail(self, error);
        };
        let Some(s) = d.servers.get(&server) else {
            return fail(self, error);
        };
        let idp = s.idp.clone();
        let person = d
            .who(caller.acting())
            .map(|w| w.id)
            .ok()
            .or_else(|| caller.principal.clone())
            .unwrap_or_default();
        let id = crate::sec::random::b64url_token(9).unwrap_or_default();
        let nonce = crate::sec::random::b64url_token(16).unwrap_or_default();
        let rendered = json!({"tool": name, "server": server, "rule": "reenrol", "principal": person,
                              "kind": "reenrol", "idp": idp});
        let task = self.signin_task(&caller, &id, &rendered, How::Reenrol, &json!({}));
        let timeout = self.settings.identity.delegation.consent_timeout();
        let gate = super::delegated::ConsentGate {
            nonce,
            args_hash: super::delegated::args_hash(&name, &args),
            rendered,
            principal: person.clone(),
            deadline_ms: now_ms() + timeout.as_millis() as u64,
            call: PolicyCall {
                tool: name.clone(),
                args,
                rule: 0,
                on_timeout: crate::config::settings::PolicyAction::Deny,
                company: None,
                consent: true,
                passed: Default::default(),
                judged: None,
            },
            target: target.clone(),
            effect: false,
            operator_approves: false,
            task,
            signin: Some(SignIn {
                how: How::Reenrol,
                idp: idp.clone(),
                server: server.clone(),
                grant: String::new(),
                max_age: 0,
                start_uri: None,
                device: None,
                ciba: None,
                firing: None,
                follows: None,
                asked_s: 0,
                before_rules,
            }),
            // Checked at every pass against the run's session as it is
            // then: an ended one is what this gate waits to see replaced.
            session: None,
            once: None,
            once_binding: None,
        };
        self.consents.insert(id.clone(), gate);
        let line = json!({"gate": id, "decision": "asked", "server": server, "tool": name,
                          "principal": person, "idp": idp, "why": error,
                          "timeout_ms": timeout.as_millis() as u64});
        self.log.info("delegation.reenrol", line.clone());
        self.audit(super::audit::AuditEvent {
            action: "delegation.reenrol",
            target: line,
            outcome: "asked",
            principal: Some(&person),
            role: None,
            request_id: None,
            sid: None,
            trace: self.caller_trace(&caller).as_ref(),
            link: None,
        });
        if let Target::Step(run, step) = &target {
            let (run, step) = (run.clone(), step.clone());
            self.step_tool_outcome(
                &run,
                &step,
                &name,
                ToolOutcome::Deferred(PendingKind::Consent { id }),
            );
        }
    }

    /// Every pass, with no network: each re-enrolment gate whose person
    /// may act again — enrolled again, or signed in through a live session
    /// of theirs holding a proof of the very identity their grant is bound
    /// to, which the run moves to — has its step's call made again.
    pub(crate) fn poll_reenrols(&mut self) {
        let Some(d) = self.delegation.clone() else {
            return;
        };
        let waiting: Vec<(String, String, String, String)> = self
            .consents
            .iter()
            .filter_map(|(id, g)| {
                let s = g.signin.as_ref().filter(|s| s.how == How::Reenrol)?;
                let Target::Step(run, _) = &g.target else {
                    return None;
                };
                Some((
                    id.clone(),
                    run.clone(),
                    s.server.clone(),
                    g.principal.clone(),
                ))
            })
            .collect();
        for (id, run, server, person) in waiting {
            let Some(r) = self.runs.get(&run) else {
                continue;
            };
            let (principal, session) = (r.principal.clone(), r.session.clone());
            let mut session = session;
            if let Some(sid) = session.clone()
                && !d.session_live(&sid)
                && let Some(to) = principal.as_deref().and_then(|p| d.rebind(p, &server))
            {
                if let Some(r) = self.runs.get_mut(&run) {
                    r.session = Some(to.clone());
                    r.touch();
                }
                let line = json!({"run": run, "from": sid, "to": to, "server": server});
                self.log.info("delegation.session.rebound", line.clone());
                self.audit(super::audit::AuditEvent {
                    action: "delegation.session.rebound",
                    target: line,
                    outcome: "rebound",
                    principal: principal.as_deref(),
                    role: None,
                    request_id: None,
                    sid: Some(&to),
                    trace: None,
                    link: None,
                });
                session = Some(to);
            }
            let acting = crate::delegation::Acting::new(principal.as_deref(), session.as_deref());
            if d.admit(acting, &server).is_err() {
                continue;
            }
            let Some(g) = self.consents.remove(&id) else {
                continue;
            };
            self.consent_task_closed(g.task.as_deref(), &id);
            self.log.info(
                "delegation.reenrol",
                json!({"gate": id, "decision": "signed_in", "principal": person, "server": server}),
            );
            let found = self.pending.iter().position(|p| {
                p.target == g.target
                    && matches!(&p.kind, PendingKind::Consent { id: x } if *x == id)
            });
            if let Some(i) = found {
                self.pending.remove(i);
                // A call held before the rules meets them now, the
                // person's consent included; one held after them is made.
                let passed = if g.signin.as_ref().is_some_and(|s| s.before_rules) {
                    g.call.passed
                } else {
                    g.call.passed.and(&g.call)
                };
                self.run_again(&g.target, &g.call, passed);
            }
        }
    }
}
