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
//! Either way the sign-in must be the grant's own `iss` + `sub`, and a
//! refusal or an expiry denies the call. Denying the gate cancels it; no
//! approval answers it.

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
}

impl How {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            How::Authorize => "authorize",
            How::Device => "device",
        }
    }
    fn of(s: &str) -> Option<How> {
        match s {
            "authorize" => Some(How::Authorize),
            "device" => Some(How::Device),
            _ => None,
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
    json!({"how": s.how.as_str(), "idp": s.idp, "server": s.server, "grant": s.grant,
           "max_age": s.max_age,
           "device": s.device.as_ref().map(|d| json!({
               "user_code": d.user_code, "verification_uri": d.verification_uri,
               "interval_ms": d.interval_ms, "expires_ms": d.expires_ms, "sealed": d.sealed}))})
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
        d.stepping_up(&id, &grant);
        let mut link = json!({});
        let mut signin = SignIn {
            how,
            idp: idp.clone(),
            server: server.clone(),
            grant: grant.clone(),
            max_age,
            start_uri: None,
            device: None,
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
    /// action gate's is, its `consentGate` saying where to sign in.
    fn signin_task(
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
        t.transition(State::InputRequired, Some(signin_text(id, rendered, link)));
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
            t.transition(crate::a2a::tasks::State::InputRequired, Some(text));
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
    /// off the loop, and one past its expiry denies its call.
    pub(crate) fn poll_step_ups(&mut self) {
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
                    g.signin.as_ref().map(|s| (s.idp.clone(), s.max_age)),
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
            let Some((idp, max_age)) = idp else { continue };
            let (d, tx) = (std::sync::Arc::clone(&d), self.events_tx.clone());
            std::thread::Builder::new()
                .name("delegation-step-up-poll".into())
                .spawn(move || {
                    let ev = match d.poll_step_up(&id, &idp, &code, max_age) {
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
    fn close_signin(&mut self, gate: &str, why: &str) {
        let Some(g) = self.consents.remove(gate) else {
            return;
        };
        self.consent_task_closed(g.task.as_deref(), gate);
        self.log.info(
            "delegation.step_up",
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
                    "{}: delegation refused (step_up): {why}; it was NOT executed",
                    g.call.tool
                )),
                true,
            );
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
    ) -> Option<SignIn> {
        let how = How::of(w["how"].as_str()?)?;
        let s = SignIn {
            how,
            idp: w["idp"].as_str()?.to_string(),
            server: w["server"].as_str().unwrap_or_default().to_string(),
            grant: w["grant"].as_str()?.to_string(),
            max_age: w["max_age"].as_u64().unwrap_or(0),
            start_uri: None,
            device: None,
        };
        let d = self.delegation.clone()?;
        d.stepping_up(id, &s.grant);
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
    /// with the last life.
    pub(crate) fn end_rebuilt_authorize(&mut self, ids: &[String]) {
        for id in ids {
            self.close_signin(
                id,
                "a restart ended the sign-in it asked for, and the session it was for with it",
            );
        }
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
