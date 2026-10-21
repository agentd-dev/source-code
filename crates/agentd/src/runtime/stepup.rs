// SPDX-License-Identifier: AGPL-3.0-only
//! Step-up sign-ins as consent gates (RFC 0044 §5.10 L4, P2): when the IdP
//! answers a mint with `insufficient_user_authentication`, the call is not
//! failed — it waits on a [`PendingKind::Consent`] gate addressed to the
//! grant's person, who signs in again, as recently as the IdP asked; the
//! call is then made again with that sign-in's fresh ID token as the
//! exchange's subject.
//!
//! The gate is a sign-in rather than an approval, by one of two kinds:
//!
//! - **`authorize`** — the person's display session is handed a single-use
//!   start link on the listener origin (the task's `consentGate`), which
//!   goes to the IdP with `max_age` and `prompt=login`; the callback, taken
//!   only with the start link's cookie, completes the gate
//!   ([`crate::delegation::federation`]).
//! - **`device`** — where the work has no display session (a per-person
//!   operator over the unix listener) and the IdP offers its device grant:
//!   agentd shows `verification_uri` and `user_code`, never the complete
//!   URI, and polls the IdP on the reactor's passes, each poll off the loop.
//!   The device code is sealed under the person's personal key in a
//!   workflow step's wait record, so a restart resumes the polling.
//!
//! Either way the sign-in must be the grant's own `iss` + `sub`, as recent
//! as the IdP asked — its ID token's `auth_time` within the `max_age`, or
//! after the ask where the IdP named none — and a refusal or an expiry
//! denies the call. Denying the gate cancels it; no approval answers it.
//!
//! Two more kinds of gate wait the same way, on something the person does
//! elsewhere ([`crate::runtime::firings`]): **`ciba`**, a firing of an
//! `act_for` workflow waiting on its person's approval on their own device;
//! and **`reenrol`**, a workflow step whose grant was revoked or expired —
//! or whose run's session ended — waiting, under `on_revoked: ask`, for the
//! person to sign in again.

// Every step-up gate is opened and answered over the A2A listener: a build
// without it keeps the types for the consent gate's record alone.
#![cfg_attr(not(feature = "a2a"), allow(dead_code))]

use serde_json::{Value, json};

#[cfg(feature = "a2a")]
use super::reactor::{PendingKind, PolicyCall, Runtime, Target};
use super::tools::ToolCaller;
#[cfg(feature = "a2a")]
use super::tools::ToolOutcome;
#[cfg(feature = "a2a")]
use crate::state::now_ms;

/// How a step-up sign-in is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    /// By authorization code, through a start link on the listener origin.
    Authorize,
    /// By the IdP's device grant, polled on the reactor's passes.
    Device,
    /// A firing's CIBA request, polled on the reactor's passes.
    Ciba,
    /// The person enrols again, or signs in through a live session.
    Reenrol,
}

impl How {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            How::Authorize => "authorize",
            How::Device => "device",
            How::Ciba => "ciba",
            How::Reenrol => "reenrol",
        }
    }
    fn of(s: &str) -> Option<How> {
        match s {
            "authorize" => Some(How::Authorize),
            "device" => Some(How::Device),
            "ciba" => Some(How::Ciba),
            "reenrol" => Some(How::Reenrol),
            _ => None,
        }
    }

    /// The code a call this gate held is refused with when it closes
    /// without the person.
    pub(crate) fn refusal(self) -> &'static str {
        match self {
            How::Authorize | How::Device => "step_up",
            How::Ciba => "ciba_denied",
            How::Reenrol => "revoked",
        }
    }
}

/// The sign-in a consent gate asks for, in place of an approval.
#[derive(Debug, Clone)]
pub(crate) struct SignIn {
    pub how: How,
    pub idp: String,
    pub server: String,
    /// The grant the call needs.
    pub grant: String,
    pub max_age: u64,
    /// The start link an `authorize` gate was given.
    pub start_uri: Option<String>,
    /// The IdP's device grant, once it answered.
    pub device: Option<DeviceWait>,
    /// A `ciba` gate's request, once the IdP took it: memory only.
    pub ciba: Option<CibaWait>,
    /// The session id of the firing a `ciba` gate asks for.
    pub firing: Option<String>,
    /// The gate whose request this `ciba` gate's call waits on: another
    /// call of the same firing asked first.
    pub follows: Option<String>,
    /// When the IdP asked for the step-up, in seconds since the epoch: a
    /// sign-in must come after it where the IdP named no `max_age`.
    pub asked_s: u64,
    /// A `reenrol` gate whose call waited before the operator's rules and
    /// the person's consent were consulted: once the person may act again
    /// it goes through them all.
    pub before_rules: bool,
}

/// A firing's CIBA request the IdP took, and its polling. The request id
/// is never written down: a restart ends the wait.
#[derive(Debug, Clone)]
pub struct CibaWait {
    pub auth_req_id: String,
    pub interval_ms: u64,
    pub next_ms: u64,
    pub expires_ms: u64,
    pub polling: bool,
}

/// A device grant the IdP answered, and its polling.
#[derive(Debug, Clone)]
pub struct DeviceWait {
    /// What agentd polls with: memory only, and in a step's wait record
    /// sealed under the person's personal key.
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_ms: u64,
    pub next_ms: u64,
    pub expires_ms: u64,
    pub polling: bool,
    pub sealed: Option<Value>,
}

/// What a poll of the IdP's device grant found that the loop acts on: the
/// person has not signed in yet, or the IdP asked agentd to slow down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polled {
    Pending,
    SlowDown,
}

/// A step-up gate's durable part, for a step's wait record: never the
/// device code but sealed.
pub(crate) fn signin_wait(s: &SignIn) -> Value {
    let mut w = json!({"how": s.how.as_str(), "idp": s.idp, "server": s.server, "grant": s.grant,
           "max_age": s.max_age, "firing": s.firing, "follows": s.follows,
           "device": s.device.as_ref().map(|d| json!({
               "user_code": d.user_code, "verification_uri": d.verification_uri,
               "interval_ms": d.interval_ms, "expires_ms": d.expires_ms, "sealed": d.sealed}))});
    if s.asked_s > 0 {
        w["asked_at"] = json!(s.asked_s);
    }
    if s.before_rules {
        w["before_rules"] = json!(true);
    }
    w
}

/// A step-up gate's sign-in as its wait record [`signin_wait`] wrote it.
pub(crate) fn signin_of(w: &Value) -> Option<SignIn> {
    Some(SignIn {
        how: How::of(w["how"].as_str()?)?,
        idp: w["idp"].as_str()?.to_string(),
        server: w["server"].as_str().unwrap_or_default().to_string(),
        grant: w["grant"].as_str()?.to_string(),
        max_age: w["max_age"].as_u64().unwrap_or(0),
        start_uri: None,
        device: None,
        ciba: None,
        firing: w["firing"].as_str().map(str::to_string),
        follows: w["follows"].as_str().map(str::to_string),
        asked_s: w["asked_at"].as_u64().unwrap_or(0),
        before_rules: w["before_rules"].as_bool() == Some(true),
    })
}

/// What a step-up gate shows, as data.
fn rendered_of(name: &str, server: &str, how: How, max_age: u64, principal: &str) -> Value {
    json!({"tool": name, "server": server, "rule": "step_up", "principal": principal,
           "kind": how.as_str(), "max_age": max_age})
}

/// What the person reads of step-up gate `id`: agentd's own words, the
/// values the work chose quoted.
pub(crate) fn signin_text(id: &str, r: &Value, link: &Value) -> String {
    use crate::sec::render::quoted;
    let s = |k: &str| r[k].as_str().map(|v| quoted(v, 128)).unwrap_or_default();
    match r["kind"].as_str() {
        Some("ciba") => {
            return [
                "agentd security gate: a firing of a workflow that acts for you with nobody there waits for your approval on your own device. Every word here is agentd's but the quoted values, which the work chose.".to_string(),
                format!("Workflow: {}, run {}", s("workflow"), s("run")),
                format!("Server: {}", s("server")),
                format!("Tool: {}", s("tool")),
                "Approve or deny it where your identity provider asks you; unanswered, the firing fails.".to_string(),
                format!("Deny gate {id} in your display client, or with agentd --delegation-deny {id}, to end it here."),
            ]
            .join("\n");
        }
        Some("reenrol") => {
            return [
                "agentd security gate: the grant this step acts with is gone — revoked, expired, or the session it acted through ended. Every word here is agentd's but the quoted values, which the work chose.".to_string(),
                format!("Server: {}", s("server")),
                format!("Tool: {}", s("tool")),
                format!(
                    "Sign in again at {} — from your display client (delegation.enrol), or with agentd --login idp:{} — and the step is made again.",
                    s("idp"),
                    r["idp"].as_str().unwrap_or_default()
                ),
                format!("Deny gate {id} in your display client, or with agentd --delegation-deny {id}, to end it."),
            ]
            .join("\n");
        }
        _ => {}
    }
    let mut lines = vec![
        "agentd security gate: your identity provider asks you to sign in again before this call. Every word here is agentd's but the quoted values, which the work chose.".to_string(),
        format!("Server: {}", s("server")),
        format!("Tool: {}", s("tool")),
        format!(
            "Sign in within the last {}s is asked for.",
            r["max_age"].as_u64().unwrap_or(0)
        ),
    ];
    if let Some(u) = link["start_uri"].as_str() {
        lines.push(format!(
            "Open {} in your browser and sign in; the call is made again once you have.",
            quoted(u, 512)
        ));
    }
    if let (Some(u), Some(c)) = (
        link["verification_uri"].as_str(),
        link["user_code"].as_str(),
    ) {
        lines.push(format!(
            "Open {} and enter the code {}; the call is made again once you have.",
            quoted(u, 512),
            quoted(c, 64)
        ));
    }
    lines.push(format!(
        "Deny gate {id} in your display client, or with agentd --delegation-deny {id}, to cancel the call."
    ));
    lines.join("\n")
}

#[cfg(feature = "a2a")]
impl Runtime {
    /// The IdP asked the person behind `caller`'s delegated call to sign in
    /// again: the call waits on a step-up gate, or — where there is no way
    /// to ask them — fails `step_up` as it came back.
    pub(crate) fn open_step_up(
        &mut self,
        caller: ToolCaller,
        name: String,
        server: String,
        args: Value,
        error: String,
    ) {
        let target = super::delegated::target_of(&caller);
        let fail = |rt: &mut Runtime, why: String| {
            rt.reply(&target, Value::String(why), true);
        };
        let Some(d) = self.delegation.clone() else {
            return fail(self, error);
        };
        let admitted = match self.delegation_admit(&d, &caller, &server) {
            Ok(a) => a,
            Err(e) => return fail(self, format!("{name}: {e}")),
        };
        // A firing acting for its person with nobody there has nobody to
        // sign in again (RFC 0044 §5.12 E).
        if admitted.who.firing.is_some() {
            return fail(
                self,
                format!(
                    "{error}; nobody is there to sign in again: a firing acting for its person with nobody there asks for no step-up"
                ),
            );
        }
        let grant = admitted.grant.grant_id.clone();
        let idp = admitted.grant.idp.clone();
        let max_age = d.step_up_of(&grant).unwrap_or(0);
        let issuer_and_signins = self
            .a2a_serving
            .as_ref()
            .and_then(|s| s.authority.clone())
            .and_then(|a| Some((a.issuer()?.to_string(), a.signins()?.clone())));
        let device_offered = d.providers.get(&idp).is_some_and(|p| {
            p.cfg
                .grants()
                .contains(&crate::config::settings::delegation::SignInGrant::Device)
        });
        // A display session signs in through a start link on the listener
        // origin; anyone else by the IdP's device grant, where it is offered
        // — safe whoever might approve the code, since the sign-in must be
        // the grant's own `iss` + `sub`.
        let how = match (&caller.session, &issuer_and_signins) {
            (Some(_), Some(_)) => How::Authorize,
            _ if device_offered => How::Device,
            _ => return fail(self, error),
        };
        let id = crate::sec::random::b64url_token(9).unwrap_or_default();
        let nonce = crate::sec::random::b64url_token(16).unwrap_or_default();
        let person = admitted.who.id.clone();
        d.stepping_up(
            &id,
            &grant,
            crate::delegation::CallOf {
                server: server.clone(),
                unit: super::delegated::call_unit(&caller),
                step: caller.step.clone(),
            },
        );
        let mut link = json!({});
        let mut signin = SignIn {
            how,
            idp: idp.clone(),
            server: server.clone(),
            grant: grant.clone(),
            max_age,
            start_uri: None,
            device: None,
            ciba: None,
            firing: None,
            follows: None,
            asked_s: d.step_up_at(&grant),
            before_rules: false,
        };
        match how {
            How::Authorize => {
                let Some((issuer, signins)) = issuer_and_signins else {
                    return fail(self, error);
                };
                match signins.begin(
                    &issuer,
                    crate::a2a::login::Purpose::StepUp { gate: id.clone() },
                    &idp,
                    Some(max_age),
                ) {
                    Ok(l) => {
                        link = json!({"start_uri": l.uri, "expires_at": now_ms() + l.expires_in * 1000});
                        signin.start_uri = Some(l.uri);
                    }
                    Err(e) => {
                        return fail(self, format!("{error}; no sign-in could be started: {e}"));
                    }
                }
            }
            How::Device => self.start_device_step_up(&d, &id, &idp, &person),
            // A step-up is signed in for, never approved or re-enrolled.
            How::Ciba | How::Reenrol => return fail(self, error),
        }
        let rendered = rendered_of(&name, &server, how, max_age, &person);
        let deadline = now_ms() + crate::a2a::login::SIGN_IN_FOR.as_millis() as u64;
        let task = self.signin_task(&caller, &id, &rendered, how, &link);
        let gate = super::delegated::ConsentGate {
            nonce,
            args_hash: super::delegated::args_hash(&name, &args),
            rendered,
            principal: person.clone(),
            deadline_ms: deadline,
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
            signin: Some(signin),
            session: caller.session.clone(),
            once: None,
            once_binding: None,
        };
        self.consents.insert(id.clone(), gate);
        self.log.info(
            "delegation.step_up",
            json!({"gate": id, "decision": "asked", "kind": how.as_str(), "server": server,
                   "tool": name, "principal": person, "max_age": max_age}),
        );
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

    /// The A2A task a step-up gate for `caller`'s work is shown on: as an
    /// action gate's is, its `consentGate` saying where to sign in — in
    /// `TASK_STATE_AUTH_REQUIRED` (A2A §7.6): the person signs in out of
    /// band, directly to agentd, and the call goes on without a message.
    pub(crate) fn signin_task(
        &mut self,
        caller: &ToolCaller,
        id: &str,
        rendered: &Value,
        how: How,
        link: &Value,
    ) -> Option<String> {
        use crate::a2a::tasks::State;
        let super::human::HumanChannel::Owned(task) = self.human_channel(caller) else {
            return None;
        };
        let t = self.tasks.get_mut(&task)?;
        if t.state.is_terminal() {
            return None;
        }
        let mut g = json!({"gate": id, "kind": how.as_str(), "server": rendered["server"]});
        if let (Some(o), Some(l)) = (g.as_object_mut(), link.as_object()) {
            o.extend(l.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        t.security_gate = true;
        t.consent_gate = Some(g);
        t.transition(State::AuthRequired, Some(signin_text(id, rendered, link)));
        self.task_persist(&task);
        self.task_sync(&task);
        Some(task)
    }

    /// Ask the IdP for a device grant for step-up gate `id`, off the loop:
    /// what it answers comes back as [`Runtime::step_up_device`], its code
    /// sealed under `person`'s personal key.
    fn start_device_step_up(
        &self,
        d: &std::sync::Arc<crate::delegation::Delegation>,
        id: &str,
        idp: &str,
        person: &str,
    ) {
        let (d, tx, id, idp, person) = (
            std::sync::Arc::clone(d),
            self.events_tx.clone(),
            id.to_string(),
            idp.to_string(),
            person.to_string(),
        );
        std::thread::Builder::new()
            .name("delegation-step-up".into())
            .spawn(move || {
                let ev = match d.start_device(&idp) {
                    Ok(start) => {
                        let sealed = d
                            .seal_personal_off_loop(
                                &person,
                                &format!("consent:{id}:device"),
                                &json!({"device_code": start.device_code}),
                            )
                            .ok();
                        let now = now_ms();
                        super::events::Event::StepUpDevice {
                            gate: id,
                            device: Box::new(DeviceWait {
                                device_code: start.device_code,
                                user_code: start.user_code,
                                verification_uri: start.verification_uri,
                                interval_ms: start.interval.max(1) * 1000,
                                next_ms: now + start.interval.max(1) * 1000,
                                expires_ms: now + start.expires_in.unwrap_or(600).min(600) * 1000,
                                polling: false,
                                sealed,
                            }),
                        }
                    }
                    Err(e) => super::events::Event::ConsentSignedIn {
                        gate: id,
                        result: Err(format!("the IdP's device grant could not be started: {e}")),
                    },
                };
                let _ = tx.send(ev);
            })
            .ok();
    }

    /// Step-up gate `gate`'s device grant, as the IdP answered it — or as a
    /// restart unsealed it: shown to the person, kept in the step's wait
    /// record, and polled from the next pass.
    pub(crate) fn step_up_device(&mut self, gate: &str, device: DeviceWait) {
        let Some(g) = self.consents.get_mut(gate) else {
            return;
        };
        let Some(s) = g.signin.as_mut() else {
            return;
        };
        let link = json!({"verification_uri": device.verification_uri, "user_code": device.user_code,
                          "expires_at": device.expires_ms});
        g.deadline_ms = g.deadline_ms.max(device.expires_ms);
        s.device = Some(device);
        let (target, task, rendered) = (g.target.clone(), g.task.clone(), g.rendered.clone());
        if let Some(t) = task.as_deref().and_then(|t| self.tasks.get_mut(t)) {
            if let Some(o) = t.consent_gate.as_mut().and_then(Value::as_object_mut)
                && let Some(l) = link.as_object()
            {
                o.extend(l.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            let text = signin_text(gate, &rendered, &link);
            t.transition(crate::a2a::tasks::State::AuthRequired, Some(text));
        }
        if let Some(task) = task {
            self.task_persist(&task);
            self.task_sync(&task);
        }
        if let Target::Step(run, step) = &target {
            let wait = self.consent_wait(gate);
            if let Some(st) = self.runs.get_mut(run).and_then(|r| r.steps.get_mut(step)) {
                st.wait = Some(wait);
            }
            if let Some(r) = self.runs.get_mut(run) {
                r.touch();
            }
        }
        self.log.info(
            "delegation.step_up",
            json!({"gate": gate, "decision": "device_code", "verification_uri": link["verification_uri"],
                   "user_code": link["user_code"]}),
        );
    }

    /// Every pass: each step-up gate's device grant due a poll is polled,
    /// off the loop, and one past its expiry denies its call; so is each
    /// firing's CIBA request, and each re-enrolment gate is checked.
    pub(crate) fn poll_step_ups(&mut self) {
        self.poll_cibas();
        self.poll_reenrols();
        let now = now_ms();
        let mut expired = Vec::new();
        let mut due = Vec::new();
        for (id, g) in self.consents.iter_mut() {
            let Some(d) = g.signin.as_mut().and_then(|s| s.device.as_mut()) else {
                continue;
            };
            if now >= d.expires_ms {
                expired.push(id.clone());
            } else if !d.polling && now >= d.next_ms {
                d.polling = true;
                due.push((
                    id.clone(),
                    d.device_code.clone(),
                    g.signin
                        .as_ref()
                        .map(|s| (s.idp.clone(), (s.max_age, s.asked_s))),
                ));
            }
        }
        for id in expired {
            self.close_signin(&id, "the sign-in at the IdP expired before it was done");
        }
        let Some(d) = self.delegation.clone() else {
            return;
        };
        for (id, code, idp) in due {
            let Some((idp, asked)) = idp else { continue };
            let (d, tx) = (std::sync::Arc::clone(&d), self.events_tx.clone());
            std::thread::Builder::new()
                .name("delegation-step-up-poll".into())
                .spawn(move || {
                    let ev = match d.poll_step_up(&id, &idp, &code, asked) {
                        Ok(Some(polled)) => super::events::Event::StepUpPolled { gate: id, polled },
                        Ok(None) => super::events::Event::ConsentSignedIn {
                            gate: id,
                            result: Ok(()),
                        },
                        Err(e) => super::events::Event::ConsentSignedIn {
                            gate: id,
                            result: Err(e),
                        },
                    };
                    let _ = tx.send(ev);
                })
                .ok();
        }
    }

    /// A poll of step-up gate `gate`'s device grant found nothing yet.
    pub(crate) fn step_up_polled(&mut self, gate: &str, polled: Polled) {
        if let Some(d) = self
            .consents
            .get_mut(gate)
            .and_then(|g| g.signin.as_mut())
            .and_then(|s| s.device.as_mut())
        {
            d.polling = false;
            if polled == Polled::SlowDown {
                d.interval_ms += 5_000;
            }
            d.next_ms = now_ms() + d.interval_ms;
        }
    }

    /// Step-up gate `gate`'s sign-in came back: the call it holds is made
    /// again, carrying the fresh sign-in — or denied, saying why.
    pub(crate) fn consent_signed_in(&mut self, gate: &str, result: Result<(), String>) {
        let Some(g) = self.consents.get(gate).cloned() else {
            return;
        };
        match result {
            Ok(()) => {
                self.consents.remove(gate);
                self.consent_task_closed(g.task.as_deref(), gate);
                let found = self.pending.iter().position(|p| {
                    p.target == g.target
                        && matches!(&p.kind, PendingKind::Consent { id } if id == gate)
                });
                self.log.info(
                    "delegation.step_up",
                    json!({"gate": gate, "decision": "signed_in", "principal": g.principal}),
                );
                if let Some(i) = found {
                    self.pending.remove(i);
                    self.run_approved(&g.target, &g.call);
                }
            }
            Err(e) => self.close_signin(gate, &format!("the step-up sign-in was refused: {e}")),
        }
    }

    /// Close step-up gate `gate` unsigned: its call is refused, `why`.
    pub(crate) fn close_signin(&mut self, gate: &str, why: &str) {
        let Some(g) = self.consents.remove(gate) else {
            return;
        };
        self.consent_task_closed(g.task.as_deref(), gate);
        let how = g.signin.as_ref().map_or(How::Authorize, |s| s.how);
        self.log.info(
            match how {
                How::Ciba => "delegation.ciba",
                How::Reenrol => "delegation.reenrol",
                How::Authorize | How::Device => "delegation.step_up",
            },
            json!({"gate": gate, "decision": "denied", "why": why}),
        );
        let found = self.pending.iter().position(|p| {
            p.target == g.target && matches!(&p.kind, PendingKind::Consent { id } if id == gate)
        });
        if let Some(i) = found {
            self.pending.remove(i);
            self.reply(
                &g.target,
                Value::String(format!(
                    "{}: delegation refused ({}): {why}; it was NOT executed",
                    g.call.tool,
                    how.refusal()
                )),
                true,
            );
        }
        // Calls that waited on this gate's request end with it.
        let followers: Vec<String> = self
            .consents
            .iter()
            .filter(|(_, f)| {
                f.signin
                    .as_ref()
                    .is_some_and(|s| s.follows.as_deref() == Some(gate))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for f in followers {
            self.close_signin(&f, why);
        }
    }

    /// A step-up gate rebuilt at start from a step's wait record: a device
    /// grant's sealed code is opened off the loop and its polling resumes;
    /// an `authorize` gate's start link, and the session it was for, ended
    /// with the last life, so the call is refused, saying so.
    pub(crate) fn rebuild_signin(
        &mut self,
        id: &str,
        w: &Value,
        principal: &str,
        (run, step): (String, String),
    ) -> Option<SignIn> {
        let s = signin_of(w)?;
        let how = s.how;
        // A firing's wait on its person, and a wait for them to sign in
        // again, need nothing reopened: the first is ended by the start
        // (`end_rebuilt_signins`), the second is checked at every pass.
        if matches!(how, How::Ciba | How::Reenrol) {
            return Some(s);
        }
        let d = self.delegation.clone()?;
        // The step's call, as its resume makes it again.
        d.stepping_up(
            id,
            &s.grant,
            crate::delegation::CallOf {
                server: s.server.clone(),
                unit: run,
                step: Some(step),
            },
        );
        let device = w.get("device").filter(|v| v.is_object());
        if let (How::Device, Some(dv)) = (how, device) {
            let (d, tx, id, principal, dv) = (
                std::sync::Arc::clone(&d),
                self.events_tx.clone(),
                id.to_string(),
                principal.to_string(),
                dv.clone(),
            );
            std::thread::Builder::new()
                .name("delegation-step-up-resume".into())
                .spawn(move || {
                    let opened = d
                        .open_personal(&principal, &format!("consent:{id}:device"), &dv["sealed"])
                        .ok()
                        .flatten();
                    let ev = match opened
                        .and_then(|o| o["device_code"].as_str().map(str::to_string))
                    {
                        Some(code) => super::events::Event::StepUpDevice {
                            gate: id,
                            device: Box::new(DeviceWait {
                                device_code: code,
                                user_code: dv["user_code"].as_str().unwrap_or_default().to_string(),
                                verification_uri: dv["verification_uri"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                                interval_ms: dv["interval_ms"].as_u64().unwrap_or(5_000),
                                next_ms: now_ms(),
                                expires_ms: dv["expires_ms"].as_u64().unwrap_or(0),
                                polling: false,
                                sealed: Some(dv["sealed"].clone()),
                            }),
                        },
                        None => super::events::Event::ConsentSignedIn {
                            gate: id,
                            result: Err("the device code was not kept across the restart".into()),
                        },
                    };
                    let _ = tx.send(ev);
                })
                .ok();
        }
        Some(s)
    }

    /// After a restart: a rebuilt `authorize` step-up gate's sign-in ended
    /// with the last life, so its call fails; and so did the request a
    /// firing's wait on its person's approval was for — the firing asks
    /// them once more ([`Runtime::ask_rebuilt_cibas`]).
    pub(crate) fn end_rebuilt_signins(&mut self, ids: &[(String, How)]) {
        let mut cibas = Vec::new();
        for (id, how) in ids {
            if *how == How::Ciba {
                cibas.push(id.clone());
                continue;
            }
            self.close_signin(
                id,
                "a restart ended the sign-in it asked for, and the session it was for with it",
            );
        }
        self.ask_rebuilt_cibas(&cibas);
    }
}

impl crate::delegation::Delegation {
    /// The `max_age` a step-up of grant `grant` was asked for, while one is.
    pub(crate) fn step_up_of(&self, grant: &str) -> Option<u64> {
        self.step_up_asked(grant)
    }
}

/// The caller and call of a delegated call the IdP wants a step-up for,
/// handed back to the loop from the call's thread.
#[derive(Debug)]
pub struct StepUpAsk {
    pub(crate) caller: ToolCaller,
    pub(crate) name: String,
    pub(crate) server: String,
    pub(crate) args: Value,
    pub(crate) error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sign-in gate's wait record keeps what a restart rebuilds it with:
    /// when the IdP asked for the step-up, and that a re-enrolment gate's
    /// call met none of the operator's rules or the person's consent yet.
    #[test]
    fn a_signin_wait_keeps_when_it_was_asked_and_what_it_met() {
        let s = SignIn {
            how: How::Device,
            idp: "okta".into(),
            server: "linear".into(),
            grant: "g".into(),
            max_age: 0,
            start_uri: None,
            device: None,
            ciba: None,
            firing: None,
            follows: None,
            asked_s: 1_700_000_000,
            before_rules: false,
        };
        let back = signin_of(&signin_wait(&s)).unwrap();
        assert_eq!(
            (back.how, back.asked_s, back.before_rules),
            (How::Device, 1_700_000_000, false)
        );
        let reenrol = SignIn {
            how: How::Reenrol,
            asked_s: 0,
            before_rules: true,
            ..s
        };
        let back = signin_of(&signin_wait(&reenrol)).unwrap();
        assert_eq!((back.how, back.before_rules), (How::Reenrol, true));
    }
}
