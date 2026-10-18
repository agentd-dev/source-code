// SPDX-License-Identifier: AGPL-3.0-only
//! A Company's agent and its OpenAPI APIs, as the runtime serves them (RFC
//! 0047 §5.5.7, §5.5.8): the `company.*` tools that reach them, the stream
//! a conversation's events are appended to, and the `http` step that calls
//! a Company's API.
//!
//! **The tools** — `company.converse`, `.read`, `.handoff`, `.close`,
//! `company.api`, `.api.describe` — are internal: every call comes back to
//! the supervisor, whatever child made it, and is sent from a thread of its
//! own, never the loop (`companies::conversation`, `companies::api`). Each
//! acts for the one User (`runtime::companies`), takes a place of the
//! Company's `limits.in_flight`, is sent nothing before its not-before
//! instant, and opens the User's gate first when the work holds another
//! Company's content (`company_cross_check`). What it answers is the
//! Company's content: the context it reaches carries the Company's origin,
//! written before the answer is read. A message's `context` holds what the
//! configuration shares — `user_available` only with `share_availability`,
//! from whether the User has a client attached to the observation feed.
//!
//! **The stream.** Each open conversation's reader hands its events to the
//! loop, which appends them to `company.<name>` carrying the Company's
//! origin — so a run that a `stream` start fires, or that a `wait {on:
//! event}` resumes, carries it too — and only then tells the reader, which
//! saves its cursor. At boot each open conversation's reader starts again
//! from its saved cursor; as the instance drains, every reader stops.
//!
//! **The `http` step** with `auth: {kind: company, company: <name>}` is a
//! `company.api` call made by the step: its `url` must be an operation of
//! the Company's description that `api_allow` lists, and every rule of the
//! tool holds — the User's work only, the in-flight limit, the cross-Company
//! gate, the schemas, the `Idempotency-Key` — its output the tool's.
//!
//! **An operation tool** (`<server>.<operation>`, RFC 0047 §9, P5a) is
//! `company.api` for the one operation it stands for: the same call, made
//! from the tool's arguments as its tools were adopted (`api::Shape`), and
//! sent the way `company.api` sends one, under every rule above. A policy
//! rule judges it as `company.api`'s call as well as its own
//! ([`Runtime::operation_view`]).

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::reactor::Runtime;
use super::tools::{ExecTarget, ToolCaller, ToolOutcome};
use crate::companies::api::{Call, Shape, as_company_api};
use crate::companies::conversation::{Delivery, Say};
use crate::companies::request::CallError;
use crate::companies::wire::conversation as conv;

impl Runtime {
    /// What `company`'s messages carry in `context` (draft §7.4): the
    /// fields the configuration shares, and `user_available` with
    /// `share_availability`.
    fn company_context(&self, company: &str) -> Map<String, Value> {
        let mut out = Map::new();
        let Some(svc) = self.settings.services.get(company) else {
            return out;
        };
        let ctx = crate::config::settings::companies::context_of(&self.settings, svc);
        if let Some(l) = ctx.locale {
            out.insert("locale".into(), json!(l));
        }
        if let Some(z) = ctx.time_zone {
            out.insert("time_zone".into(), json!(z));
        }
        if ctx.share_availability == Some(true) {
            out.insert("user_available".into(), json!(self.user_attached()));
        }
        out
    }

    /// Whether the User has a client attached to the observation feed now.
    #[cfg(feature = "a2a")]
    fn user_attached(&self) -> bool {
        let user = self.settings.security.companies.user().to_string();
        self.a2a_feed.as_ref().is_some_and(|f| {
            f.attached(|p, op| {
                if user == crate::config::settings::companies::OPERATOR {
                    op
                } else {
                    p == user
                }
            })
        })
    }

    /// Without A2A no client attaches.
    #[cfg(not(feature = "a2a"))]
    fn user_attached(&self) -> bool {
        false
    }

    /// `company.converse`, `.read`, `.handoff`, `.close`, `company.api` and
    /// `.api.describe`: off the loop, as the module says.
    pub(crate) fn company_call_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
    ) -> ToolOutcome {
        let entry = args["company"].as_str().unwrap_or_default().to_string();
        self.company_call(caller, name, &entry, args, None)
    }

    /// The operation tool `name`, `tool` of the Company OpenAPI API `server`:
    /// the `company.api` call of its operation, as the module says.
    pub(crate) fn company_operation_call(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        server: &str,
        tool: &str,
        args: Value,
    ) -> ToolOutcome {
        match self.operation_call(server, tool, &args) {
            Ok((entry, shape, call)) => {
                self.company_call(caller, name, &entry, &args, Some((shape, call)))
            }
            Err(e) => ToolOutcome::Ready(Value::String(format!("{name}: {e}")), true),
        }
    }

    /// The company entry the operation tool `tool` of the Company OpenAPI
    /// API `server` acts at, the operation it stands for as its tools were
    /// adopted, and the `company.api` call it makes of `args` — or why it
    /// makes none, refused unsent.
    fn operation_call(
        &self,
        server: &str,
        tool: &str,
        args: &Value,
    ) -> Result<(String, Shape, Call), String> {
        let entry = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == server)
            .and_then(|s| s.company.as_ref())
            .map(|c| c.entry.clone())
            .unwrap_or_default();
        let shape = self.joining.shape(server, tool).ok_or_else(|| {
            format!("company {entry}: {server}.{tool} is not offered; nothing was sent")
        })?;
        let call = shape
            .call(&format!("{server}.{tool}"), args)
            .map_err(|e| format!("company {entry}: {e}; nothing was sent"))?;
        Ok((entry, shape.clone(), call))
    }

    /// When `name` is a Company's operation tool, its call of `args` as
    /// `company.api`'s arguments — the call a policy rule keyed on
    /// `company.api` judges — or why it makes none.
    pub(crate) fn operation_view(&self, name: &str, args: &Value) -> Option<Result<Value, String>> {
        let Some(crate::registry::Route::Mcp { server, tool }) = self.registry.route(name) else {
            return None;
        };
        if !crate::config::settings::companies::is_api_server(&self.settings, server) {
            return None;
        }
        Some(
            self.operation_call(server, tool, args)
                .map(|(entry, _, call)| as_company_api(&entry, &call, args.get("expect"))),
        )
    }

    /// The call of the `company.*` tool `name` at the company entry `entry`,
    /// or — with `operation`, the operation it stands for and the call it
    /// makes — of the operation tool `name`.
    fn company_call(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        entry: &str,
        args: &Value,
        operation: Option<(Shape, Call)>,
    ) -> ToolOutcome {
        let refuse = |e: String| ToolOutcome::Ready(Value::String(format!("{name}: {e}")), true);
        let Some(sup) = self.companies.clone() else {
            return refuse("no Company is declared".into());
        };
        let entry = entry.to_string();
        let Some(company) = sup.company(&entry) else {
            return refuse(format!(
                "{entry:?} is not a company entry (services.<name>.kind: company)"
            ));
        };
        // The contract the call is: an operation tool's is `company.api`'s.
        let contract = if operation.is_some() {
            "company.api"
        } else {
            name
        };
        let interface = if crate::config::settings::companies::is_conversation_tool(contract) {
            "agent"
        } else {
            "openapi"
        };
        if let Err(e) = company.pace().check() {
            crate::obs::metrics::record_company_request(&company.name, interface, "unsent");
            return ToolOutcome::Ready(Value::String(format!("{e}; the call was not sent")), true);
        }
        let Some(slot) = crate::companies::Company::enter(&company) else {
            crate::obs::metrics::record_company_request(&company.name, interface, "unsent");
            return ToolOutcome::Ready(
                Value::String(format!(
                    "company {} has {} calls in flight (security.companies.limits.in_flight); retry when one ends",
                    company.name,
                    company.limit()
                )),
                true,
            );
        };
        let method = match &operation {
            Some((_, call)) => call.method.clone(),
            None => args["method"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_uppercase(),
        };
        // A step's API write that may run is on its record before it leaves:
        // a restart that finds the step running does not send it again. A
        // message, a read, a handoff and a close are the same whenever they
        // are sent again, and are sent again.
        if contract == "company.api"
            && !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
            && let (Some(run), Some(step), None) = (&caller.run, &caller.step, &caller.subagent)
            && let Some(r) = self.runs.get_mut(run)
            && let Some(st) = r.steps.get_mut(step)
        {
            st.company = Some(company.name.clone());
            r.dirty = true;
            self.checkpoint(false);
        }
        let key = self.idempotency_key(caller);
        let context = if name == "company.converse" {
            self.company_context(&entry)
        } else {
            Map::new()
        };
        let reads = self.reads_of(caller);
        // A span of the unit's own trace, which the request names under
        // `standard` and links to the Company's own trace under `strict`.
        let unit = self.caller_trace(caller);
        let call = unit.as_ref().map(crate::obs::trace::TraceContext::child);
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let (name, contract, args) = (name.to_string(), contract.to_string(), args.clone());
        std::thread::Builder::new()
            .name(format!("company:{}:{name}", company.name))
            .spawn(move || {
                let start = crate::obs::otel::now_unix_nanos();
                // A description is fetched with no credential, and names no
                // trace.
                let traced = if contract == "company.api.describe" {
                    Ok((None, None))
                } else {
                    sup.request_trace(&company, unit.as_ref(), call.as_ref())
                };
                let (sent, link) = match traced {
                    Ok(t) => t,
                    Err(e) => {
                        crate::obs::metrics::record_company_request(
                            slot.company(),
                            interface,
                            "unsent",
                        );
                        target.send(&tx, Value::String(format!("{name}: {e}")), true);
                        return;
                    }
                };
                let text = |k: &str| args[k].as_str().map(str::to_string);
                let wait = |default: u32| {
                    args["wait"].as_u64().map_or(default, |w| {
                        w.min(u64::from(crate::companies::conversation::MAX_WAIT)) as u32
                    })
                };
                let cid = text("conversation").unwrap_or_default();
                let res: Result<Value, CallError> = match contract.as_str() {
                    "company.converse" => sup.converse(
                        &company,
                        &Say {
                            conversation: text("conversation"),
                            text: text("text"),
                            data: args.get("data").filter(|d| d.is_object()).cloned(),
                            context,
                            wait: args["wait"].as_u64().map(|_| wait(0)),
                            key,
                            trace: sent,
                        },
                    ),
                    "company.read" => sup.read(
                        &company,
                        &cid,
                        wait(crate::companies::conversation::MAX_WAIT),
                        sent.as_ref(),
                    ),
                    "company.handoff" => {
                        sup.conversation_op(&company, &cid, conv::HANDOFF, sent.as_ref())
                    }
                    "company.close" => {
                        sup.conversation_op(&company, &cid, conv::CLOSE, sent.as_ref())
                    }
                    "company.api" => match &operation {
                        Some((shape, call)) => sup.call_operation(
                            &company,
                            shape,
                            &Call {
                                key,
                                trace: sent,
                                ..call.clone()
                            },
                        ),
                        None => sup.call_api(
                            &company,
                            &Call {
                                api: text("api"),
                                method,
                                path: text("path").unwrap_or_default(),
                                query: args["query"].as_object().cloned().unwrap_or_default(),
                                json: args.get("json").cloned(),
                                key,
                                trace: sent,
                            },
                        ),
                    },
                    _ => sup.describe_api(
                        &company,
                        text("api").as_deref(),
                        text("operation").as_deref(),
                    ),
                };
                let outcome = outcome_of(&contract, &args, &res);
                crate::obs::metrics::record_company_request(slot.company(), interface, outcome);
                // The Company's content: the context it reaches carries the
                // Company's origin, written before the answer is read — and
                // so does an error that may hold what the Company sent.
                let read = res.as_ref().map_or_else(|e| e.read_the_company(), |_| true);
                if read && let (Some(into), Some(issuer)) = (reads, slot.issuer()) {
                    let _ = tx.send(super::events::Event::CompanyRead {
                        into,
                        origin: crate::companies::carry::origin(&issuer),
                        why: crate::companies::carry::why(slot.company()),
                    });
                }
                let company_name = slot.company().to_string();
                drop(slot);
                // A status outside `expect` (default 2xx) is the Company's
                // error answer, said whole.
                let is_error = outcome != "ok";
                let result = match res {
                    Ok(v) => v,
                    Err(e) => Value::String(e.to_string()),
                };
                crate::obs::otel::company_span(
                    call.as_ref(),
                    &name,
                    &company_name,
                    !is_error,
                    start,
                    link.as_ref(),
                );
                target.send(&tx, result, is_error);
            })
            .ok();
        ToolOutcome::Executing
    }

    /// The `http` step with `auth: {kind: company}`: the step's `company.api`
    /// call.
    pub(crate) fn step_http_company(
        &mut self,
        run_id: &str,
        step_id: &str,
        caller: &ToolCaller,
        spec: &Map<String, Value>,
    ) {
        let mut args = json!({
            "company": spec.get("auth").and_then(|a| a.get("company")).cloned().unwrap_or(Value::Null),
            "method": spec.get("method").and_then(Value::as_str).unwrap_or("GET"),
            "path": spec.get("url").cloned().unwrap_or(Value::Null),
        });
        for k in ["query", "json", "expect"] {
            if let Some(v) = spec.get(k).filter(|v| !v.is_null()) {
                args[k] = v.clone();
            }
        }
        // A query's values are strings in an `http` step; the operation's
        // schema reads each.
        self.step_tool_call(run_id, step_id, caller, "company.api", args);
    }

    /// The reader's events reached the loop: appended to the stream
    /// `company.<name>`, carrying the Company's origin, and the reader told
    /// whether they were.
    pub(crate) fn on_company_delivery(&mut self, d: Delivery) {
        let stream = crate::config::settings::companies::stream_of(&d.company);
        let mut carries = super::withhold::Carries::default();
        carries.add([crate::companies::carry::origin(&d.issuer)], || {
            crate::companies::carry::why(&d.company)
        });
        // What the Company says in a Direct Conversation reaches the User
        // on its relay task (`runtime::company_direct`).
        let events: Vec<Value> = d.items.iter().map(|i| i.data["event"].clone()).collect();
        self.relay_company_events(&d.company, &d.conversation, &events);
        let mut outcome = Ok(());
        for item in d.items {
            if let Err(e) = self.append_carrying(
                &stream,
                &item.subject,
                Some(&d.conversation),
                item.data,
                &item.id,
                &format!("company:{}", d.company),
                Some(&carries),
                None,
            ) {
                outcome = Err(e.to_string());
                break;
            }
        }
        let _ = d.ack.try_send(outcome);
    }

    /// At boot: the readers' events and the supervisor's notices go to the
    /// loop, each open conversation's reader starts again from its saved
    /// cursor, and each operation not final is read again when its time
    /// comes. A Company's request to talk with the User is asked of them when
    /// the listener serves and unowned work may gate.
    pub(crate) fn resume_company_conversations(&mut self) {
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let tx = self.events_tx.clone();
        sup.deliver_to(Arc::new(move |d| {
            let _ = tx.send(super::events::Event::CompanyDelivery(Box::new(d)));
        }));
        let tx = self.events_tx.clone();
        sup.notify_to(Arc::new(move |n| {
            let _ = tx.send(super::events::Event::CompanyNotice(Box::new(n)));
        }));
        sup.set_direct_askable(
            cfg!(feature = "a2a")
                && self.settings.a2a.listen.is_some()
                && self.settings.agent.ask_human_unowned
                    == crate::config::settings::AskHumanUnowned::Gate,
        );
        sup.resume_conversations();
        sup.track_operations();
    }
}

/// Whether `company.api`'s answer has a status its `expect` names — by
/// default, a 2xx.
fn expected(args: &Value, answer: &Value) -> bool {
    let status = answer["status"].as_u64().unwrap_or(0);
    match args["expect"].as_array() {
        Some(e) => e.iter().any(|x| x.as_u64() == Some(status)),
        None => (200..300).contains(&status),
    }
}

/// The `outcome` of `agent_company_requests_total` for the tool `name`'s
/// call: an answer outside `expect` is the Company's error.
fn outcome_of(name: &str, args: &Value, res: &Result<Value, CallError>) -> &'static str {
    match res {
        Ok(v) if name == "company.api" && !expected(args, v) => "error",
        Ok(_) => "ok",
        Err(e) => e.outcome(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call is `ok` only when its answer is: a `company.api` status
    /// outside `expect` (default 2xx) is the Company's error, as the tool
    /// reports it.
    #[test]
    fn an_answer_outside_expect_is_counted_an_error() {
        let api = |status: u64, expect: Value| {
            outcome_of(
                "company.api",
                &json!({"expect": expect}),
                &Ok(json!({"status": status})),
            )
        };
        assert_eq!(api(201, Value::Null), "ok");
        assert_eq!(api(422, Value::Null), "error");
        assert_eq!(api(404, json!([200, 404])), "ok");
        assert_eq!(api(200, json!([404])), "error");
        assert_eq!(
            outcome_of("company.read", &json!({}), &Ok(json!({"status": "idle"}))),
            "ok",
            "a conversation's answer has no status of its own"
        );
        assert_eq!(
            outcome_of(
                "company.api",
                &json!({}),
                &Err(CallError::Unknown("outcome unknown: x".into()))
            ),
            "unknown"
        );
    }
}
