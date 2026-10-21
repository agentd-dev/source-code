// SPDX-License-Identifier: AGPL-3.0-only
//! **Internal tool execution**: the runtime is the single place
//! internal tools run — for a turn worker's `ToolRequest` (answered with
//! `ToolResult`), for a workflow step (`tool` / `memory.*` / … kinds) and for
//! A2A commands. Keeping them here means every state change is made by the
//! state owner. Arguments are validated against the contract's input
//! schema before dispatch and results against the output schema after
//! (schema failure ⇒ a tool error, never a panic). Some tools are **deferred**
//! (`sleep`, `subagent.run sync`, `subagent.await`, `await`, `think`,
//! `context.compact`, `workflow.run wait`, `workflow.wait`): the request is
//! parked in `pending` and answered when its wait resolves. Mapped tools
//! (overrides) run on an executor thread against the runtime's own MCP
//! connection.

use super::children::ChildKind;
use super::events::Event;
use super::reactor::{PendingKind, PendingTool, Runtime, Target};
use crate::context::ROOT;
use crate::context::plan::{self, Plan};
use crate::registry::{Registry, Route};
use crate::state::now_ms;
use crate::subagent::protocol::ControlMsg;
use crate::supervisor::tree::NodeId;
use serde_json::{Value, json};
use std::time::Duration;

/// Who is calling (derived from the child kind or the step).
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolCaller {
    pub node: Option<NodeId>,
    pub req: u64,
    pub ctx: Option<String>,
    pub run: Option<String>,
    pub step: Option<String>,
    pub principal: Option<String>,
    pub subagent: Option<String>,
    /// The message-hop depth of the work this call belongs to (see
    /// `RunState::msg_depth`). Carried so `message.send` can refuse to extend a
    /// chain that has already gone too deep.
    pub msg_depth: u32,
    /// The RFC 0043 session the work acts through, when a signed-in person
    /// started it (RFC 0044 §5.12): a delegated call is admitted against
    /// that session as it is now.
    pub session: Option<String>,
}

impl ToolCaller {
    /// Who this call acts for, as a delegated call is admitted: the
    /// principal, through the session the work acts through.
    #[cfg(feature = "xaa")]
    pub(crate) fn acting(&self) -> crate::delegation::Acting<'_> {
        crate::delegation::Acting::new(self.principal.as_deref(), self.session.as_deref())
    }

    fn label(&self) -> String {
        if let Some(s) = &self.subagent {
            return format!("subagent:{s}");
        }
        if let (Some(r), Some(s)) = (&self.run, &self.step) {
            return format!("step:{r}/{s}");
        }
        format!("ctx:{}", self.ctx.as_deref().unwrap_or(ROOT))
    }
    /// The context whose plan/skills a `plan.*`/`skills.*` call addresses.
    pub(crate) fn context_id(&self) -> String {
        self.ctx.clone().unwrap_or_else(|| ROOT.to_string())
    }
    fn ctx_value(&self, instance: &str) -> Value {
        json!({"instance": instance, "ctx": self.ctx, "run": self.run, "step": self.step, "principal": self.principal, "subagent": self.subagent})
    }
}

impl Runtime {
    /// A tool call's `agent/idempotency_key` ([`call_key`]).
    pub(crate) fn idempotency_key(&self, caller: &ToolCaller) -> String {
        call_key(&self.instance, &self.process_nonce, caller)
    }
}

/// The idempotency key of `caller`'s call in the instance `instance`, in the
/// process `nonce`. A workflow step's own call says which call it is by its
/// run and step alone, and stays the same across the step's retries and a
/// restart's replay: the attempt is not in it. A turn's — a model's call
/// inside a step included — or a subagent's carries the process and the
/// child: a child's request ids begin at 1 in every child, so two turns of
/// one conversation, two attempts of one step's turn and two lives of the
/// instance sent the same key.
fn call_key(instance: &str, nonce: &str, caller: &ToolCaller) -> String {
    if caller.node.is_none()
        && caller.subagent.is_none()
        && caller.run.is_some()
        && caller.step.is_some()
    {
        return format!("{instance}/{}", caller.label());
    }
    format!(
        "{instance}/{}@{nonce}.{}#{}",
        caller.label(),
        caller.node.map_or(0, |n| n.0),
        caller.req
    )
}

/// Why a worker may not use tool `name` — `delegated`, a delegated
/// server's — when what it was started with did not offer it: the refusal
/// said to the model. `None` for any other tool, and for one it was
/// offered.
fn use_unauthorised(
    offered: &std::collections::BTreeSet<String>,
    name: &str,
    delegated: bool,
) -> Option<String> {
    (delegated && !offered.contains(name)).then(|| {
        format!(
            "{name}: refused: what this worker was started with did not offer it — a delegated server's tool is used only where a turn's plan, or a subagent's spawn, offers it by its tools and servers"
        )
    })
}

/// The result of executing a tool.
pub(crate) enum ToolOutcome {
    Ready(Value, bool),
    Deferred(PendingKind),
    /// Running on an executor thread; the reply arrives as an event.
    Executing,
}

impl Runtime {
    /// A child asked for an internal tool.
    pub(crate) fn on_tool_request(&mut self, node: NodeId, id: u64, name: &str, args: Value) {
        self.counters.tool_calls += 1;
        let Some(caller) = self.child_caller(node, id) else {
            return;
        };
        // A turn worker handing over the MCP task its call left open: the
        // worker's own request, never a tool the model can name.
        if name == super::mcp_tasks::HAND_OVER {
            let outcome = self.mcp_task_hand_over(&caller, args);
            self.child_tool_outcome(node, id, name, outcome);
            return;
        }
        self.child_log(node).info("tool.request", json!({"node": node.0, "req": id, "tool": name, "caller": caller.label(), "args": if self.log.content_capture() { args.clone() } else { Value::Null }}));
        // A delegated server's tool is used where the turn's plan or the
        // subagent's spawn offered it alone — `tools:` and `servers:` held
        // at every use, a pooled token or no (RFC 0044 §5.7): a worker
        // asking for one it was not offered is refused before any check or
        // mint.
        if let Some(refused) = self
            .children
            .get(node)
            .and_then(|c| c.offered.as_ref())
            .and_then(|offered| use_unauthorised(offered, name, self.delegated_tool(name)))
        {
            self.child_log(node).warn(
                "tool.refused",
                json!({"node": node.0, "req": id, "tool": name, "why": "not_offered"}),
            );
            self.child_tool_outcome(
                node,
                id,
                name,
                ToolOutcome::Ready(Value::String(refused), true),
            );
            return;
        }
        let outcome = self.execute_tool(&caller, name, args);
        self.child_tool_outcome(node, id, name, outcome);
    }

    /// Who a child's tool request `id` is from, by the kind of child asking;
    /// `None` when the child is gone.
    fn child_caller(&self, node: NodeId, id: u64) -> Option<ToolCaller> {
        Some(match self.children.get(node).map(|c| c.kind.clone())? {
            ChildKind::RootTurn {
                ctx,
                msg_depth,
                session,
                ..
            } => ToolCaller {
                node: Some(node),
                req: id,
                ctx: Some(ctx.clone()),
                principal: self.contexts.get(&ctx).and_then(|c| c.principal.clone()),
                msg_depth,
                session,
                ..Default::default()
            },
            ChildKind::StepTurn { run, step, .. } => ToolCaller {
                node: Some(node),
                req: id,
                run: Some(run.clone()),
                step: Some(step),
                ctx: self.runs.get(&run).and_then(|r| r.conversation.clone()),
                principal: self.runs.get(&run).and_then(|r| r.principal.clone()),
                msg_depth: self.runs.get(&run).map(|r| r.msg_depth).unwrap_or(0),
                session: self.runs.get(&run).and_then(|r| r.session.clone()),
                ..Default::default()
            },
            // A subagent acts for whoever it works for, exactly as the turn
            // that spawned it did: a caller with no principal is the runtime,
            // so leaving it unset would let a user's subagent read and do
            // what only the operator may (`status` hands the runtime the
            // whole instance).
            ChildKind::Subagent { handle } => ToolCaller {
                node: Some(node),
                req: id,
                principal: self
                    .subagents
                    .get(&handle)
                    .and_then(|s| s.principal.clone()),
                session: self.subagents.get(&handle).and_then(|s| s.session.clone()),
                subagent: Some(handle),
                ..Default::default()
            },
            // A think child acts through the session of the turn that asked
            // for it, when one did.
            ChildKind::Think { ctx, reply_to, .. } => ToolCaller {
                node: Some(node),
                req: id,
                principal: ctx
                    .as_ref()
                    .and_then(|c| self.contexts.get(c))
                    .and_then(|c| c.principal.clone()),
                session: reply_to
                    .and_then(|(asker, _)| self.children.get(asker))
                    .and_then(|c| match &c.kind {
                        ChildKind::RootTurn { session, .. } => session.clone(),
                        _ => None,
                    }),
                ctx,
                ..Default::default()
            },
        })
    }

    /// Deliver a child's tool outcome: answer it now, park it, or leave it to
    /// the executor thread that will.
    pub(crate) fn child_tool_outcome(
        &mut self,
        node: NodeId,
        id: u64,
        name: &str,
        outcome: ToolOutcome,
    ) {
        match outcome {
            ToolOutcome::Ready(v, err) => self.reply_tool(node, id, v, err),
            ToolOutcome::Deferred(kind) => {
                // Report the unit as parked on a wait, not thinking, so the
                // live-activity feed does not show it burning a model call.
                self.activity_park(node, name);
                self.push_pending(PendingTool {
                    target: Target::Child(node, id),
                    name: name.to_string(),
                    kind,
                    started_ms: now_ms(),
                });
            }
            ToolOutcome::Executing => {}
        }
    }

    /// Answer a child's tool request.
    pub(crate) fn reply_tool(&mut self, node: NodeId, req: u64, result: Value, is_error: bool) {
        self.child_log(node).debug(
            "tool.reply",
            json!({"node": node.0, "req": req, "is_error": is_error}),
        );
        if !self.children.send(
            node,
            &ControlMsg::ToolResult {
                id: req,
                result,
                is_error,
            },
        ) {
            self.log
                .debug("tool.reply.dropped", json!({"node": node.0, "req": req}));
        }
    }

    /// Answer a request a think child (`think`, `decide`) or a compaction
    /// was deferred on — and stop waiting on it. Its pending entry goes with
    /// the answer: left behind, the worker's exit would answer the same
    /// request again, with an error, and a requester reading both could take
    /// the error.
    pub(crate) fn answer_deferred(
        &mut self,
        node: NodeId,
        req: u64,
        result: Value,
        is_error: bool,
    ) {
        self.pending
            .retain(|p| p.target != Target::Child(node, req));
        self.reply_tool(node, req, result, is_error);
    }

    /// Answer a deferred request wherever it came from.
    pub(crate) fn reply(&mut self, target: &Target, result: Value, is_error: bool) {
        match target {
            Target::Child(node, req) => self.reply_tool(*node, *req, result, is_error),
            Target::Step(run, step) => {
                let (run, step) = (run.clone(), step.clone());
                let error = is_error.then(|| match &result {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                self.on_step_done(&run, &step, result, is_error, error, 0);
            }
            Target::McpTask(h, key) => {
                let (h, key) = (h.clone(), key.clone());
                self.mcp_task_answered(&h, &key, result, is_error);
            }
            Target::Elicit(id) => {
                #[cfg(feature = "xaa")]
                self.elicit_answered(id, result, is_error);
                #[cfg(not(feature = "xaa"))]
                let _ = (id, result, is_error);
            }
        }
    }

    /// Execute an internal (or mapped) tool for `caller`.
    pub(crate) fn execute_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) -> ToolOutcome {
        self.execute_tool_as(caller, name, args, super::reactor::Passed::default())
    }

    /// [`Runtime::execute_tool`], with every gate applied but those `passed`
    /// says the call was approved past — a policy gate's addressee, or the
    /// User for a Company gate: the grant, the argument schema, every other
    /// gate and the result schema still are.
    fn execute_tool_as(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
        passed: super::reactor::Passed,
    ) -> ToolOutcome {
        // Grant + availability.
        let allowed = match (&caller.subagent, &caller.run) {
            (Some(_), _) => self
                .registry
                .allowed(&crate::registry::Caller::Subagent { allow: None }, name),
            (None, Some(_)) => self
                .registry
                .allowed(&crate::registry::Caller::Workflow, name),
            _ => self.registry.allowed(&crate::registry::Caller::Root, name),
        };
        if !allowed {
            let reason = match self.registry.get(name) {
                None => format!("no such tool {name:?}"),
                Some(t) if t.disabled => format!("tool {name:?} is disabled by configuration"),
                Some(t) if !t.is_available() => {
                    format!("tool {name:?} has no implementation (map it with tools.overrides)")
                }
                Some(_) => format!("tool {name:?} is not granted to {}", caller.label()),
            };
            return ToolOutcome::Ready(Value::String(reason), true);
        }
        if let Err(e) = self.registry.validate_args(name, &args) {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        // A Company's tool acts for its one User, and a subagent reaches one
        // of a Company's servers only by a grant its spawn made (RFC 0047
        // §5.6).
        if self.is_company_tool(name) {
            if let Err(e) = self.company_work_admitted(caller.principal.as_deref()) {
                return ToolOutcome::Ready(Value::String(e), true);
            }
            if let Some(h) = &caller.subagent
                && self.company_server_of(name).is_some()
            {
                let granted = self
                    .subagents
                    .get(h)
                    .and_then(|r| r.payload.as_ref())
                    .and_then(|p| p["gated_tools"].as_array())
                    .is_some_and(|g| g.iter().any(|t| t.as_str() == Some(name)));
                if !granted {
                    return ToolOutcome::Ready(
                        Value::String(format!("tool {name:?} is not granted to subagent:{h}")),
                        true,
                    );
                }
            }
        }
        // The instance's own controls are the operator's, whichever door a
        // request uses. Every root turn is offered them, including one a
        // `user` or `agent` principal drives — so without this, a principal
        // narrowed to one workflow could have the model redefine that
        // workflow, define an armed one that runs as the runtime, or disarm
        // somebody else's. A call that names a run (`workflow.pause {run}`)
        // acts on an object, not the instance, and the tool's own owner check
        // decides it.
        if self.registry.instance_wide(name)
            && args.get("run").is_none()
            && let Some(p) = self.acting_principal(caller.principal.as_deref())
            && !p.is_operator()
        {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "tool {name:?} acts on the whole instance and is not permitted for {}",
                    p.id
                )),
                true,
            );
        }
        // A call to one Company from work holding another's content asks the
        // User first (RFC 0047 §6.2), before any rule is consulted: no
        // policy can answer for the User.
        #[cfg(feature = "companies")]
        if let Some(outcome) = self.company_cross_check(caller, name, &args, passed) {
            return outcome;
        }
        // Work acting with a person's grant does not reach agentd's own,
        // broader authority at an agent-mode server tagged `sensitive` or
        // `egress` its template does not list, and every agent-mode call it
        // makes is recorded as one (RFC 0044 §5.3, T10).
        #[cfg(feature = "xaa")]
        if !passed.policy
            && !passed.consent
            && let Some(server) = self.agent_mode_server_of(name)
            && let Some(e) = self.mode_mixing(caller, &server, Some(name), name)
        {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        // The policy verdict, at the one chokepoint every call passes — and
        // deliberately AFTER `validate_args`, so an argument guard judges
        // arguments that already conform to the tool's schema rather than
        // whatever the model happened to emit.
        if !passed.policy
            && !self.settings.security.policies.is_empty()
            && let Some(outcome) = self.apply_policy(caller, name, &args, passed)
        {
            return outcome;
        }
        // The enrolled person's consent, after every rule the operator wrote
        // (RFC 0044 §5.10): a delegated sensitive or egress call, or an
        // egress call from work that holds delegated data, waits for them.
        #[cfg(feature = "xaa")]
        if !passed.consent
            && let Some(outcome) = self.consent_check(caller, name, &args, passed)
        {
            return outcome;
        }
        let route = self.registry.route(name).map(|r| match r {
            Route::Internal => RouteKind::Internal,
            Route::Mapped(m) => RouteKind::Mapped(m.clone()),
            Route::Code => RouteKind::Code,
            Route::Mcp { server, tool } => RouteKind::Mcp(server.to_string(), tool.to_string()),
            Route::Workflow { workflow, sync } => RouteKind::Workflow(workflow.to_string(), sync),
        });
        let out = match route {
            None => {
                ToolOutcome::Ready(Value::String(format!("tool {name:?} is unavailable")), true)
            }
            Some(RouteKind::Internal) => self.builtin(caller, name, args),
            Some(RouteKind::Mapped(m)) => self.run_mapped(caller, name, &m, args),
            Some(RouteKind::Code) => match crate::tools::call(name, &args) {
                Some(Ok(v)) => ToolOutcome::Ready(v, false),
                Some(Err(e)) => ToolOutcome::Ready(Value::String(e), true),
                None => {
                    ToolOutcome::Ready(Value::String(format!("code tool {name:?} vanished")), true)
                }
            },
            Some(RouteKind::Mcp(server, tool)) => {
                self.run_mcp_call(caller, name, &server, &tool, args)
            }
            // A workflow tool IS `workflow.run`, which is the point: the
            // caller sees one typed verb while the engine supplies retry,
            // breaker, idempotency, a human gate and restart-survival.
            Some(RouteKind::Workflow(workflow, sync)) => {
                let mut wargs = json!({"name": workflow, "inputs": args});
                if sync {
                    wargs["wait"] = json!(true);
                }
                self.workflow_tool(caller, "workflow.run", wargs)
            }
        };
        // A run's or a child's result read back is withheld from a caller
        // holding both other legs when it may carry outside input
        // (`withhold`). The read-back table names the contracts whose reply
        // carries one; a wait that defers is withheld where it is answered.
        let out = match out {
            ToolOutcome::Ready(mut v, false)
                if crate::registry::internal::read_back(name).is_some() =>
            {
                self.withhold_reply(self.reader_of(caller).as_ref(), &mut v, name);
                ToolOutcome::Ready(v, false)
            }
            other => other,
        };
        // Output validation for ready results.
        match out {
            ToolOutcome::Ready(v, false) => match self.registry.validate_result(name, &v) {
                Ok(()) => ToolOutcome::Ready(v, false),
                Err(e) => {
                    self.log
                        .warn("tool.result.schema", json!({"tool": name, "err": e}));
                    ToolOutcome::Ready(Value::String(e), true)
                }
            },
            other => other,
        }
    }

    // ---- executors ---------------------------------------------------------

    /// A mapped (override) tool: render args → MCP call on an executor thread → map result.
    fn run_mapped(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        m: &crate::registry::Mapping,
        args: Value,
    ) -> ToolOutcome {
        let ctx = caller.ctx_value(&self.instance);
        let mcp_args = match Registry::map_args(m, &args, &ctx) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::Ready(Value::String(e), true),
        };
        if let Some(e) = self.browser_refused(&m.server, &mcp_args) {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        #[cfg(feature = "xaa")]
        if self.is_delegated_server(&m.server) {
            return self.delegated_call(caller, name, &m.server, &m.tool, mcp_args);
        }
        if let Err(e) = self.service_rate_take(&m.server) {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        let Some(client) = self.mcp.get(&m.server).cloned() else {
            return ToolOutcome::Ready(
                Value::String(format!("server {:?} for {name} is not connected", m.server)),
                true,
            );
        };
        let mapping = m.clone();
        let tool_name = name.to_string();
        // `_meta` carried run, step, instance, idempotency key and attempt —
        // and nothing about WHO the work is for, so a server could neither
        // authorize nor attribute per person.
        let mut meta = json!({"agent/idempotency_key": self.idempotency_key(caller), "agent/instance": self.instance});
        self.attribute(&mut meta, caller.principal.as_deref());
        crate::mcp::scope(&mut meta, caller.principal.as_deref());
        let call = crate::mcp::stamp_trace(&mut meta, self.caller_trace(caller).as_ref());
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let call_ctx = ctx.clone();
        std::thread::Builder::new()
            .name(format!("tool:{tool_name}"))
            .spawn(move || {
                // No bound of our own: the server's own `timeout` caps each
                // silence on its socket, and a live call — a long tool, an
                // operator answering an elicitation — is left to finish. A
                // fixed default here would cut off a server configured
                // with a longer `timeout` than the default.
                let start = crate::obs::otel::now_unix_nanos();
                let res = client.call_tool_with_meta(&mapping.tool, Some(mcp_args), meta);
                crate::obs::otel::mcp_span(
                    call.as_ref(),
                    "tools/call",
                    &mapping.tool,
                    &mapping.server,
                    res.as_ref().is_ok_and(|r| !r.is_error()),
                    start,
                );
                let (result, is_error) = match res {
                    Ok(r) => {
                        // The result mapping sees `result`, the original `args` and `ctx`.
                        let mut ctx = crate::store::mcp::result_ctx(&r);
                        ctx["args"] = args;
                        ctx["ctx"] = call_ctx;
                        if r.is_error() {
                            (Value::String(format!("{tool_name}: {}", r.text())), true)
                        } else {
                            match Registry::map_result(&mapping, &ctx) {
                                Ok(v) => (v, false),
                                Err(e) => (Value::String(e), true),
                            }
                        }
                    }
                    Err(e) => (
                        Value::String(format!("{tool_name}: transport error: {e}")),
                        true,
                    ),
                };
                target.send(&tx, result, is_error);
            })
            .ok();
        ToolOutcome::Executing
    }

    /// A plain MCP tool called through the runtime (workflow steps / A2A commands).
    fn run_mcp_call(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        server: &str,
        tool: &str,
        args: Value,
    ) -> ToolOutcome {
        // A Company's operation tool is `company.api` for its operation
        // (RFC 0047 §9, P5a), and goes the way that tool goes.
        #[cfg(feature = "companies")]
        if crate::config::settings::companies::is_api_server(&self.settings, server) {
            return self.company_operation_call(caller, name, server, tool, args);
        }
        // A delegated server's call is the supervisor's, with the enrolled
        // person's minted token (RFC 0044).
        #[cfg(feature = "xaa")]
        if self.is_delegated_server(server) {
            return self.delegated_call(caller, name, server, tool, args);
        }
        // Pace calls toward a rated catalog service. A dry bucket answers with
        // a tool error the model can absorb and retry, never a hang.
        if let Err(e) = self.service_rate_take(server) {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        if let Some(e) = self.browser_refused(server, &args) {
            return ToolOutcome::Ready(Value::String(e), true);
        }
        let Some(client) = self.mcp.get(server).cloned() else {
            return ToolOutcome::Ready(
                Value::String(format!("server {server:?} for {name} is not connected")),
                true,
            );
        };
        // A Company call is sent nothing before the Company's not-before
        // instant, holds one of its places in flight for as long as it runs,
        // and its failure is said as what it means.
        #[cfg(feature = "companies")]
        let company = match self
            .companies
            .as_ref()
            .and_then(|c| c.company_of_server(server))
        {
            Some(c) => {
                if let Err(e) = c.pace().check() {
                    crate::obs::metrics::record_company_request(&c.name, "mcp", "unsent");
                    return ToolOutcome::Ready(
                        Value::String(format!("{e}; the call was not sent")),
                        true,
                    );
                }
                match crate::companies::Company::enter(&c) {
                    Some(slot) => Some(slot),
                    None => {
                        crate::obs::metrics::record_company_request(&c.name, "mcp", "unsent");
                        return ToolOutcome::Ready(
                            Value::String(format!(
                                "company {} has {} calls in flight (security.companies.limits.in_flight); retry when one ends",
                                c.name,
                                c.limit()
                            )),
                            true,
                        );
                    }
                }
            }
            None => None,
        };
        // A step's call to a Company is on its record before it leaves: a
        // restart that finds the step running does not send it again.
        #[cfg(feature = "companies")]
        if let (Some(slot), Some(run), Some(step), None) =
            (&company, &caller.run, &caller.step, &caller.subagent)
            && let Some(r) = self.runs.get_mut(run)
            && let Some(st) = r.steps.get_mut(step)
        {
            st.company = Some(slot.company().to_string());
            r.dirty = true;
            self.checkpoint(false);
        }
        let tool = tool.to_string();
        // A Company under the strict privacy profile (RFC 0047 §5.10) sees
        // none of agentd's correlators: its `_meta` is built on the call's
        // thread from the (User, Company) pair's secret — the idempotency
        // key keyed, the trace re-rooted — and the call is a span of the
        // unit's in agentd's own telemetry, linked to the trace the Company
        // sees.
        #[cfg(feature = "companies")]
        let strict = company.as_ref().is_some_and(|slot| slot.strict());
        #[cfg(not(feature = "companies"))]
        let strict = false;
        #[cfg(feature = "companies")]
        let private = strict.then(|| (self.idempotency_key(caller), self.caller_trace(caller)));
        #[cfg(feature = "companies")]
        let reads = self.reads_of(caller);
        #[cfg(feature = "companies")]
        let sup = self.companies.clone();
        let (meta, call) = if strict {
            (
                json!({}),
                self.caller_trace(caller)
                    .as_ref()
                    .map(crate::obs::trace::TraceContext::child),
            )
        } else {
            // `_meta` carried run, step, instance, idempotency key and attempt
            // — and nothing about WHO the work is for, so a server could
            // neither authorize nor attribute per person.
            let mut meta = json!({"agent/idempotency_key": self.idempotency_key(caller), "agent/instance": self.instance});
            self.attribute(&mut meta, caller.principal.as_deref());
            crate::mcp::scope(&mut meta, caller.principal.as_deref());
            let call = crate::mcp::stamp_trace(&mut meta, self.caller_trace(caller).as_ref());
            (meta, call)
        };
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        // Only a workflow step's call may declare the extension: its task
        // becomes a durable wait (RFC 0050).
        let calling = match &target {
            ExecTarget::Step { .. } => self.mcp_task_slot(server),
            ExecTarget::Tool { .. } => None,
        };
        let tasks = calling.is_some();
        let server = server.to_string();
        // The call as an operation it proposes would name it (RFC 0047
        // §5.8.1): its registered name and its arguments.
        #[cfg(feature = "companies")]
        let proposing = company.as_ref().map(|_| {
            (
                name.to_string(),
                crate::companies::operations::call_digest(name, &args),
            )
        });
        std::thread::Builder::new()
            .name(format!("mcp:{server}.{tool}"))
            .spawn(move || {
                // Unbounded as a whole, for the reason `run_mapped` gives.
                let start = crate::obs::otel::now_unix_nanos();
                // The request that proposed an operation that is not final
                // is not sent again (Operations §6).
                #[cfg(feature = "companies")]
                if let (Some(slot), Some(sup), Some((_, digest))) = (&company, &sup, &proposing)
                    && let Some(c) = sup.company(slot.company())
                    && let Some(why) = sup.repeats(
                        &c,
                        c.issuer().and_then(|i| sup.mcp_account(&i)).as_deref(),
                        digest,
                    )
                {
                    crate::obs::metrics::record_company_request(slot.company(), "mcp", "unsent");
                    target.send(&tx, Value::String(why), true);
                    return;
                }
                #[cfg(feature = "companies")]
                let (meta, link) = match (&company, &private, &sup) {
                    (Some(slot), Some((key, unit)), Some(sup)) => {
                        match sup.private_meta(slot, key, unit.as_ref()) {
                            Ok(m) => m,
                            Err(e) => {
                                crate::obs::metrics::record_company_request(
                                    slot.company(),
                                    "mcp",
                                    "unsent",
                                );
                                target.send(&tx, Value::String(e), true);
                                return;
                            }
                        }
                    }
                    _ => (meta, None),
                };
                #[cfg(not(feature = "companies"))]
                let link: Option<crate::obs::trace::TraceContext> = None;
                // A workflow step's call that becomes a task makes the step
                // a durable wait on it (RFC 0050); any other caller's call
                // follows its task inside the call.
                let res = match &target {
                    ExecTarget::Step { run, step } => {
                        match client.call_tool_called(&tool, Some(args), Some(meta), None, tasks) {
                            Ok(::mcp::wire::Called::Result(r)) => Ok(r),
                            Ok(::mcp::wire::Called::Task(task)) => {
                                crate::obs::otel::mcp_span(
                                    call.as_ref(),
                                    "tools/call",
                                    &tool,
                                    &server,
                                    true,
                                    start,
                                );
                                let _ = tx.send(Event::McpTask(Box::new(
                                    super::mcp_tasks::Note::Created {
                                        run: run.clone(),
                                        step: step.clone(),
                                        server,
                                        tool,
                                        task,
                                        trace: call,
                                        registry: true,
                                        calling,
                                    },
                                )));
                                return;
                            }
                            Err(f) => Err(f),
                        }
                    }
                    ExecTarget::Tool { .. } => client.call_tool_reaching(&tool, Some(args), meta),
                };
                // A Company's answer is its content: the context it reaches
                // carries the Company's origin from then on, written before
                // the answer is read — and so does a failure whose error may
                // hold what the Company sent.
                #[cfg(feature = "companies")]
                let failure = res
                    .as_ref()
                    .err()
                    .map(|f| crate::companies::mcp::Failure::of(f.reach));
                #[cfg(feature = "companies")]
                if let Some(slot) = &company {
                    let (outcome, read) = match (&res, failure) {
                        (Ok(r), _) if r.is_error() => ("error", true),
                        (_, Some(f)) => (f.outcome(), f.read_the_company()),
                        _ => ("ok", true),
                    };
                    crate::obs::metrics::record_company_request(slot.company(), "mcp", outcome);
                    if read && let (Some(into), Some(issuer)) = (reads.clone(), slot.issuer()) {
                        let _ = tx.send(super::events::Event::Read {
                            into,
                            carries: crate::companies::carry::read(&issuer, slot.company()),
                        });
                    }
                    // An operation the tool proposes in place of performing
                    // the action (Operations §4.2): kept, and nothing done.
                    if let (Ok(r), Some(sup), Some((name, digest))) = (&res, &sup, &proposing)
                        && !r.is_error()
                        && let Some(op) = r
                            .structured_content
                            .as_ref()
                            .and_then(|s| s.get(crate::companies::wire::operations::MEMBER))
                        && let Some(c) = sup.company(slot.company())
                    {
                        // In the account the Company's MCP servers act with.
                        let account = c.issuer().and_then(|i| sup.mcp_account(&i));
                        sup.recognise(
                            &c,
                            crate::companies::ledger::Channel::Tool { name: name.clone() },
                            account.as_deref(),
                            Some(digest.clone()),
                            op,
                        );
                    }
                }
                let (result, is_error) = match res {
                    Ok(r) => (super::worker::tool_result_value(&r), r.is_error()),
                    Err(f) => {
                        let text = format!("transport error: {}", f.error);
                        // A refusal the Company's server sent is said as what
                        // it means, and a call that may have run is `outcome
                        // unknown`, never sent again; a tool's own error is
                        // the Company's answer, and stays as it wrote it.
                        #[cfg(feature = "companies")]
                        let text = match (&company, failure) {
                            (Some(slot), Some(failure)) => crate::companies::mcp::call_failed(
                                slot.company(),
                                &format!("{server}.{tool}"),
                                failure,
                                &text,
                            ),
                            _ => text,
                        };
                        (Value::String(text), true)
                    }
                };
                #[cfg(feature = "companies")]
                drop(company);
                crate::obs::otel::mcp_span_linked(
                    call.as_ref(),
                    "tools/call",
                    &tool,
                    &server,
                    !is_error,
                    start,
                    link.as_ref(),
                );
                target.send(&tx, result, is_error);
            })
            .ok();
        ToolOutcome::Executing
    }

    // ---- built-ins ---------------------------------------------------------

    fn builtin(&mut self, caller: &ToolCaller, name: &str, args: Value) -> ToolOutcome {
        let ok = |v: Value| ToolOutcome::Ready(v, false);
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let by = caller.label();
        match name {
            // ---- instruction ----
            "instruction.read" => ok(
                json!({"text": self.instruction.text, "source": self.instruction.source, "uri": self.instruction.uri, "version": self.instruction.version.to_string()}),
            ),
            "instruction.subscribe" => {
                // No uri given: re-read from the server that served the
                // instruction, not whichever connected server answers the uri.
                match self.instruction.subscribe_target(&args) {
                    None => err(
                        "instruction.subscribe: the instruction is static text; give a uri".into(),
                    ),
                    Some(u) => {
                        #[cfg(feature = "mcp-events")]
                        let was = (
                            self.instruction.server.clone(),
                            self.instruction.uri.clone(),
                        );
                        match self.subscribe_instruction(&u) {
                            Ok(()) => {
                                // Its MCP Events follow it to where it is now.
                                #[cfg(feature = "mcp-events")]
                                if was
                                    != (
                                        self.instruction.server.clone(),
                                        self.instruction.uri.clone(),
                                    )
                                {
                                    self.events_instruction_moved();
                                }
                                ok(json!({"subscribed": true, "uri": u}))
                            }
                            Err(e) => err(e),
                        }
                    }
                }
            }
            // ---- memory ----
            // An entry keeps what its writer carried — its taint and its
            // delegated data — and every reader takes it on (RFC 0044 §5.13).
            "memory.get" => match self
                .memory
                .get_carrying(&self.durable, args["key"].as_str().unwrap_or(""))
            {
                Ok((v, held)) => {
                    self.hand_caller(caller, &held);
                    ok(v)
                }
                Err(e) => err(e),
            },
            "memory.set" => {
                let ttl = match args.get("ttl").and_then(Value::as_str) {
                    Some(t) => match crate::config::parse_duration(t) {
                        Ok(d) => Some(d.as_millis() as u64),
                        Err(e) => return err(format!("memory.set: ttl: {e}")),
                    },
                    None => None,
                };
                let carries = self.caller_carries(caller);
                match self.memory.set_carrying(
                    &self.durable,
                    args["key"].as_str().unwrap_or(""),
                    args["value"].clone(),
                    ttl,
                    Some(&by),
                    &carries,
                ) {
                    Ok(v) => ok(v),
                    Err(e) => err(e),
                }
            }
            "memory.list" => match self.memory.list(
                &self.durable,
                args.get("prefix").and_then(Value::as_str),
                args.get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| l as usize),
            ) {
                Ok(v) => ok(v),
                Err(e) => err(e),
            },
            "memory.push" => {
                let carries = self.caller_carries(caller);
                match self.memory.push_carrying(
                    &self.durable,
                    args["key"].as_str().unwrap_or(""),
                    args.get("value").cloned().unwrap_or(Value::Null),
                    Some(&by),
                    &carries,
                ) {
                    Ok(v) => ok(v),
                    Err(e) => err(e),
                }
            }
            "memory.shift" => match self.memory.shift(
                &self.durable,
                args["key"].as_str().unwrap_or(""),
                Some(&by),
            ) {
                Ok((v, held)) => {
                    self.hand_caller(caller, &held);
                    ok(v)
                }
                Err(e) => err(e),
            },
            "memory.pop" => {
                match self
                    .memory
                    .pop(&self.durable, args["key"].as_str().unwrap_or(""), Some(&by))
                {
                    Ok((v, held)) => {
                        self.hand_caller(caller, &held);
                        ok(v)
                    }
                    Err(e) => err(e),
                }
            }
            "memory.delete" => match self
                .memory
                .delete(&self.durable, args["key"].as_str().unwrap_or(""))
            {
                Ok(v) => ok(v),
                Err(e) => err(e),
            },
            // ---- artifacts ----
            "artifact.create" => {
                let content = match (
                    args.get("content"),
                    args.get("from_step").and_then(Value::as_str),
                ) {
                    (Some(c), _) => c.clone(),
                    (None, Some(step)) => match caller
                        .run
                        .as_ref()
                        .and_then(|r| self.runs.get(r))
                        .and_then(|r| r.steps.get(step))
                        .and_then(|s| s.output.clone())
                    {
                        Some(o) => o,
                        None => {
                            return err(format!(
                                "artifact.create: from_step {step:?} has no output"
                            ));
                        }
                    },
                    (None, None) => {
                        return err("artifact.create: content or from_step is required".into());
                    }
                };
                let owner = caller.run.clone().or_else(|| caller.ctx.clone());
                // The content is what the caller read: its readers take on
                // what the caller carries (RFC 0044 §5.13).
                let carries = self.caller_carries(caller);
                match self.artifacts.create(
                    &self.durable,
                    super::artifacts::NewArtifact {
                        name: args["name"].as_str().unwrap_or(""),
                        mime: args.get("mime").and_then(Value::as_str),
                        content,
                        created_by: Some(&by),
                        sensitive: args
                            .get("sensitive")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        owner: owner.as_deref(),
                        carries: &carries,
                    },
                ) {
                    Ok(v) => ok(v),
                    Err(e) => err(e),
                }
            }
            "artifact.get" => {
                let id = args["id"].as_str().unwrap_or("");
                match self.artifacts.get_value(id) {
                    Ok(v) => {
                        let held = self
                            .artifacts
                            .get(id)
                            .map(|a| a.carries.clone())
                            .unwrap_or_default();
                        self.hand_caller(caller, &held);
                        ok(v)
                    }
                    Err(e) => err(e),
                }
            }
            "artifact.delete" => match self
                .artifacts
                .delete(&self.durable, args["id"].as_str().unwrap_or(""))
            {
                Ok(v) => ok(v),
                Err(e) => err(e),
            },
            "artifact.list" => ok(self.artifacts.list(
                args.get("prefix").and_then(Value::as_str),
                args.get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| l as usize),
                None,
            )),
            // ---- plan ----
            "plan.create" => {
                let ctx_id = caller.context_id();
                let max = self
                    .settings
                    .context
                    .plan
                    .max_items
                    .unwrap_or(plan::DEFAULT_MAX_ITEMS as u32) as usize;
                let items: Vec<Value> = args["items"].as_array().cloned().unwrap_or_default();
                match Plan::create(args["goal"].as_str().unwrap_or(""), &items, max) {
                    Ok(p) => {
                        let v = p.to_value();
                        self.context_for(&ctx_id, caller.principal.as_deref()).plan = Some(p);
                        self.context_for(&ctx_id, caller.principal.as_deref())
                            .touch();
                        self.log
                            .info("plan.updated", json!({"ctx": ctx_id, "op": "create"}));
                        ok(v)
                    }
                    Err(e) => err(e),
                }
            }
            "plan.get" => {
                let ctx_id = caller.context_id();
                match self.contexts.get(&ctx_id).and_then(|c| c.plan.as_ref()) {
                    Some(p) => ok(json!({"plan": p.to_value(), "progress": p.progress()})),
                    None => ok(json!({"plan": null, "progress": "no plan"})),
                }
            }
            "plan.update" => {
                let ctx_id = caller.context_id();
                let max = self
                    .settings
                    .context
                    .plan
                    .max_items
                    .unwrap_or(plan::DEFAULT_MAX_ITEMS as u32) as usize;
                let c = self.context_for(&ctx_id, caller.principal.as_deref());
                match c.plan.as_mut() {
                    None => err("plan.update: no plan (call plan.create first)".into()),
                    Some(p) => match p.update(&args, max) {
                        Ok(()) => {
                            let mut v = p.to_value();
                            v["progress"] = json!(p.progress());
                            c.touch();
                            self.log
                                .info("plan.updated", json!({"ctx": ctx_id, "op": "update"}));
                            ok(v)
                        }
                        Err(e) => err(e),
                    },
                }
            }
            "plan.clear" => {
                let ctx_id = caller.context_id();
                let c = self.context_for(&ctx_id, caller.principal.as_deref());
                let had = c.plan.take().is_some();
                c.touch();
                self.log
                    .info("plan.updated", json!({"ctx": ctx_id, "op": "clear"}));
                ok(json!({"ok": had}))
            }
            // ---- skills ----
            "skills.list" => ok(self.skills.list_value()),
            "skills.load" => self.skills_load_tool(caller, &args),
            "skills.read" => self.skills_read_tool(caller, &args),
            "skills.unload" => self.skills_unload_tool(caller, &args),
            // ---- status ----
            // The model is its caller's deputy: it reads what that caller may.
            "status" => ok(self.status_for(caller.principal.as_deref())),
            // ---- companies ----
            #[cfg(feature = "companies")]
            "company.status" => ok(self.company_status(false)),
            #[cfg(feature = "companies")]
            "company.sign_in" => self.company_sign_in_tool(caller, &args),
            #[cfg(feature = "companies")]
            "company.sign_out" => self.company_sign_out_tool(caller, &args),
            #[cfg(feature = "companies")]
            n if crate::config::settings::companies::is_operation_tool(n) => {
                self.company_operation_tool(caller, n, &args)
            }
            #[cfg(feature = "companies")]
            crate::config::settings::companies::DIRECT_TOOL => {
                self.company_direct_tool(caller, &args)
            }
            #[cfg(feature = "companies")]
            n if crate::config::settings::companies::is_interface_tool(n) => {
                self.company_call_tool(caller, n, &args)
            }
            // ---- time ----
            "sleep" => {
                let d = match crate::config::parse_duration(args["duration"].as_str().unwrap_or(""))
                {
                    Ok(d) => d,
                    Err(e) => return err(format!("sleep: {e}")),
                };
                let deadline = now_ms() + d.as_millis() as u64;
                let owner = match caller.node {
                    Some(n) => {
                        json!({"kind": "tool", "node": n.0, "req": caller.req, "tool": "sleep"})
                    }
                    None => json!({"kind": "step", "run": caller.run, "step": caller.step}),
                };
                match self.timers.arm(
                    &self.durable,
                    deadline,
                    owner,
                    json!({"slept_ms": d.as_millis() as u64}),
                ) {
                    Ok(id) => ToolOutcome::Deferred(PendingKind::Timer { id }),
                    Err(e) => err(format!("sleep: {e}")),
                }
            }
            "await" => {
                let cond = args["condition"].as_str().unwrap_or("").to_string();
                if let Err(e) =
                    crate::cel::compile_check(cond.trim().trim_start_matches("CEL:").trim())
                {
                    return err(format!("await: {e}"));
                }
                let timeout = args
                    .get("timeout")
                    .and_then(Value::as_str)
                    .and_then(|t| crate::config::parse_duration(t).ok())
                    .unwrap_or(Duration::from_secs(600));
                ToolOutcome::Deferred(PendingKind::Await {
                    condition: cond,
                    deadline_ms: now_ms() + timeout.as_millis() as u64,
                })
            }
            // ---- context ----
            "context.compact" => {
                let ctx_id = caller.context_id();
                let keep_last = args
                    .get("keep_last")
                    .and_then(Value::as_u64)
                    .map(|k| k as usize)
                    .unwrap_or(self.settings.context.keep_last.unwrap_or(12) as usize);
                let target = args.get("target_tokens").and_then(Value::as_u64);
                let trace = self.caller_trace(caller);
                match caller.node {
                    // Deferred only when a compaction child will answer; one
                    // answered on the spot (nothing to fold, one already
                    // running, the fallback) is not waited on.
                    Some(node) => {
                        if self.start_compaction(
                            &ctx_id,
                            keep_last,
                            target,
                            Some((node, caller.req)),
                            trace.as_ref(),
                        ) {
                            ToolOutcome::Deferred(PendingKind::Think {
                                child: NodeId(u64::MAX),
                            })
                        } else {
                            ToolOutcome::Executing
                        }
                    }
                    None => err("context.compact needs a calling turn".into()),
                }
            }
            "think" => match caller.node {
                Some(node) => match self.start_think(caller, &args, Some((node, caller.req))) {
                    Ok(child) => ToolOutcome::Deferred(PendingKind::Think { child }),
                    Err(e) => err(e),
                },
                None => err("think as a step is the `think` kind".into()),
            },
            "decide" => match caller.node {
                Some(node) => match self.start_decide(caller, &args, (node, caller.req)) {
                    Ok(child) => ToolOutcome::Deferred(PendingKind::Think { child }),
                    Err(e) => err(e),
                },
                None => err("decide as a step is the `decide` kind".into()),
            },
            // ---- lifecycle ----
            "finish" => {
                // The turn worker records the finish itself and reports it in
                // its `TurnDone`, so the runtime only acknowledges here. A
                // workflow step finishes through the `finish` kind instead.
                ok(json!({"ok": true}))
            }
            // Human-in-the-loop: gate on the A2A task the asker's caller owns
            // (or, where configured, on a new one), else apply the configured
            // fallback (fail | wait | auto judge).
            "ask_human" => self.ask_human_tool(caller, args),
            // ---- subagents ----
            "subagent.run" | "subagent.send" | "subagent.kill" | "subagent.status"
            | "subagent.await" | "subagent.list" | "subagent.retire" => {
                self.subagent_tool(caller, name, args)
            }
            // ---- conversations ----
            "message.send" => self.message_send_tool(caller, args),
            // ---- workflows ----
            "workflow.run" | "workflow.list" | "workflow.status" | "workflow.cancel"
            | "workflow.wait" | "workflow.create" | "workflow.update" | "workflow.delete"
            | "workflow.pause" | "workflow.resume" | "workflow.signal" => {
                self.workflow_tool(caller, name, args)
            }
            // ---- MCP Events (RFC 0045 §5.15) ----
            #[cfg(feature = "mcp-events")]
            "events.list" | "events.subscribe" | "events.unsubscribe" | "events.subscriptions" => {
                self.events_tool(caller, name, args)
            }
            // ---- MCP Tasks (RFC 0050) ----
            "mcp.task.get" | "mcp.task.cancel" => self.mcp_task_tool(caller, name, args),
            // ---- guarded local command runner (default-OFF) ----
            #[cfg(feature = "exec")]
            "exec" => self.exec_tool(caller, args),
            other => err(format!(
                "internal tool {other:?} has no built-in implementation"
            )),
        }
    }

    /// Which policy caller this invocation counts as.
    pub(crate) fn policy_caller(caller: &ToolCaller) -> crate::config::settings::PolicyCaller {
        use crate::config::settings::PolicyCaller;
        match (&caller.subagent, &caller.run) {
            (Some(_), _) => PolicyCaller::Subagent,
            (None, Some(_)) => PolicyCaller::Workflow,
            _ => PolicyCaller::Root,
        }
    }

    /// Apply the policy list to one call. `None` means proceed.
    fn apply_policy(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        passed: super::reactor::Passed,
    ) -> Option<ToolOutcome> {
        use crate::config::settings::PolicyAction;
        let tags = self
            .registry
            .tags_of(std::slice::from_ref(&name.to_string()));
        let who = Self::policy_caller(caller);
        // A Company's operation tool's call is `company.api`'s call of its
        // operation (RFC 0047 §9, P5a), and is judged as that too: a rule on
        // `company.api` — its name, its arguments — holds it as it holds
        // that tool's.
        #[cfg(feature = "companies")]
        let view = match self.operation_view(name, args) {
            Some(Err(e)) => {
                return Some(ToolOutcome::Ready(
                    Value::String(format!("{name}: {e}")),
                    true,
                ));
            }
            v => v.and_then(Result::ok),
        };
        #[cfg(not(feature = "companies"))]
        let view: Option<Value> = None;
        // What the taint engine and the delegation know of the call and the
        // work making it (RFC 0044 §5.13 item 6): read only when a rule asks.
        let facts = if self.settings.security.policies.iter().any(|p| {
            let m = &p.matcher;
            m.tainted.is_some()
                || m.holds_delegated_data.is_some()
                || m.unattended.is_some()
                || m.document_sourced.is_some()
                || m.mode.is_some()
        }) {
            self.policy_facts(caller, name)
        } else {
            crate::sec::policy::Facts {
                delegated: self.delegated_tool(name),
                ..Default::default()
            }
        };
        let as_api = view.as_ref().map(|v| crate::sec::policy::Call {
            tool: "company.api",
            tags: &tags,
            caller: who,
            principal: caller.principal.as_deref(),
            args: v,
            also: None,
            facts,
        });
        let call = crate::sec::policy::Call {
            tool: name,
            tags: &tags,
            caller: who,
            principal: caller.principal.as_deref(),
            args,
            also: as_api.as_ref(),
            facts,
        };
        // A `to: principal` rule whose first use in this unit the person
        // approved is past for every later call of the tool there.
        let policies = &self.settings.security.policies;
        let passed_once = |i: usize| {
            #[cfg(feature = "xaa")]
            {
                policies[i].once_per.is_some() && self.rule_once_passed(caller, name, i, args)
            }
            #[cfg(not(feature = "xaa"))]
            {
                let _ = i;
                false
            }
        };
        let verdict = match crate::sec::policy::evaluate_past(policies, &call, &passed_once) {
            Ok(None) => return None,
            Ok(Some(v)) => v,
            Err(rule) => {
                // Fail closed and say which rule could not be judged.
                crate::obs::metrics::record_refusal("policy");
                self.log.error(
                    "tool.policy.error",
                    json!({"tool": name, "rule": rule, "caller": caller.label()}),
                );
                return Some(ToolOutcome::Ready(
                    Value::String(format!(
                        "refused: security.policies[{rule}] has an argument guard that could not be evaluated"
                    )),
                    true,
                ));
            }
        };
        match verdict.action {
            PolicyAction::Allow => None,
            PolicyAction::Deny | PolicyAction::Shadow => {
                let held = verdict.action == PolicyAction::Shadow;
                crate::obs::metrics::record_refusal("policy");
                self.log.info(
                    "tool.policy.refused",
                    json!({"tool": name, "rule": verdict.rule, "caller": caller.label(),
                           "action": if held { "shadow" } else { "deny" }}),
                );
                self.audit(super::audit::AuditEvent {
                    action: "tool.policy",
                    target: json!({"tool": name, "rule": verdict.rule}),
                    outcome: if held { "shadow" } else { "deny" },
                    principal: caller.principal.as_deref(),
                    role: None,
                    request_id: None,
                    sid: None,
                    trace: self.caller_trace(caller).as_ref(),
                    link: None,
                });
                // Shadow mode says plainly that the call was HELD, never
                // returning a synthetic success. A schema-conformant fake is
                // reasoned over as real, and every later decision is then
                // built on a fabricated observation — which is a strange thing
                // for a fail-closed runtime to ship, and worse than refusing.
                let msg = if held {
                    format!(
                        "held by security.policies[{}]: this call was NOT executed and no result exists. \
                         Treat it as not done — do not assume an outcome.",
                        verdict.rule
                    )
                } else {
                    format!("denied by security.policies[{}]", verdict.rule)
                };
                Some(ToolOutcome::Ready(Value::String(msg), true))
            }
            // The person's consent, asked as the shipped rules ask it.
            #[cfg(feature = "xaa")]
            PolicyAction::Ask
                if verdict.to == Some(crate::config::settings::PolicyTo::Principal) =>
            {
                Some(self.rule_consent(caller, name, args, &verdict, passed))
            }
            PolicyAction::Ask => self.policy_gate(caller, name, args, &verdict, passed),
        }
    }

    /// For a Company's operation tool, the `company.api` call `args` make of
    /// it now — what a gate holding the call asks about, and what the call
    /// must still make when it is approved ([`super::reactor::PolicyCall::judged`]).
    pub(crate) fn judged_call(&self, name: &str, args: &Value) -> Option<Value> {
        #[cfg(feature = "companies")]
        return self.operation_view(name, args).and_then(Result::ok);
        #[cfg(not(feature = "companies"))]
        {
            let _ = (name, args);
            None
        }
    }

    /// An `action: ask` verdict: put it to a person. `None` means proceed —
    /// only when nobody can be asked and the rule's `on_timeout` is `allow`.
    ///
    /// Deliberately NOT routed through `agent.approval`. That setting decides
    /// how asks the MODEL requested are handled, and its `auto` mode answers
    /// them with a model judge — letting the agent approve the operator's own
    /// security gate. An operator-declared gate goes to a human or it does not
    /// pass.
    ///
    /// The gate holds the call itself ([`super::reactor::PolicyCall`]) and
    /// asks for a decision in a declared shape: `approve` runs the call and
    /// the asker gets its real result ([`Runtime::policy_settle`]); `deny` is
    /// an error; anything else is asked again.
    fn policy_gate(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: &Value,
        verdict: &crate::sec::policy::Verdict,
        passed: super::reactor::Passed,
    ) -> Option<ToolOutcome> {
        use crate::config::settings::PolicyAction;
        let question = crate::sec::policy::question(
            verdict.question.as_deref(),
            name,
            Self::policy_caller(caller),
            args,
        );
        // The same ownership test `ask_human` uses: the caller who owns the
        // asking unit is on core A2A and can be put in front of the gate; an
        // unowned call gates only where the operator said they will be asked.
        let channel = self.human_channel(caller);
        #[cfg(feature = "a2a")]
        {
            let on = match &channel {
                super::human::HumanChannel::Owned(t) => {
                    Some(super::human::GateOn::Owned(t.clone()))
                }
                super::human::HumanChannel::Unowned
                    if self.settings.agent.ask_human_unowned
                        == crate::config::settings::AskHumanUnowned::Gate =>
                {
                    Some(super::human::GateOn::Unit)
                }
                _ => None,
            };
            if let Some(on) = on {
                self.log.info(
                    "tool.policy.ask",
                    json!({"tool": name, "rule": verdict.rule, "caller": caller.label()}),
                );
                let deadline =
                    now_ms() + verdict.timeout_ms.unwrap_or(super::human::ASK_TIMEOUT_MS);
                // Addressed to the rule's `to:`, and to the OPERATOR when it
                // names nobody. The gate usually lands on the task of the very
                // caller whose call is being judged, and whoever holds a task
                // may answer an unaddressed gate on it — so an unaddressed
                // policy gate would be approved by the party it exists to
                // check. A policy is the operator wanting a say.
                let to = verdict
                    .to
                    .as_ref()
                    .and_then(crate::config::settings::PolicyTo::addressee)
                    .cloned()
                    .unwrap_or_else(|| crate::a2a::principals::Addressee {
                        role: Some(crate::config::settings::Role::Operator),
                        ..Default::default()
                    });
                let held = super::reactor::PolicyCall {
                    tool: name.to_string(),
                    args: args.clone(),
                    rule: verdict.rule,
                    on_timeout: verdict.on_timeout,
                    company: None,
                    passed,
                    judged: self.judged_call(name, args),
                    consent: false,
                };
                let question = format!(
                    "{question}\n\nAnswer `{}` to run this call or `{}` to refuse it.",
                    super::human::POLICY_APPROVE,
                    super::human::POLICY_DENY
                );
                return Some(self.human_gate(
                    caller,
                    on,
                    question,
                    deadline,
                    Some(super::human::policy_answer_schema()),
                    Some(to),
                    Some(held),
                ));
            }
        }
        #[cfg(not(feature = "a2a"))]
        let _ = passed;
        // Nobody to ask. `on_timeout` decides, and it defaults to deny: a gate
        // that cannot be answered has not been approved, and quietly running
        // the call because nobody can be asked would make the policy a
        // suggestion.
        let fallback = verdict.on_timeout;
        let why = channel.why_unanswerable();
        // The question goes in the log even though nobody can answer it: an
        // operator reading this needs to know what they were not asked.
        self.log.warn(
            "tool.policy.unanswerable",
            json!({"tool": name, "rule": verdict.rule, "question": question,
                   "fallback": format!("{fallback:?}").to_lowercase(),
                   "note": format!("nobody can be asked: {why}")}),
        );
        // `allow` PROCEEDS: the call runs and its own result is returned. A
        // bare success in its place would be a result for a call that never
        // ran — the fabricated observation `shadow` refuses to produce.
        if fallback == PolicyAction::Allow {
            return None;
        }
        Some(ToolOutcome::Ready(
            Value::String(format!(
                "denied by security.policies[{}]: a person had to approve this call and nobody can be asked ({why})",
                verdict.rule
            )),
            true,
        ))
    }

    /// A policy gate was decided: run the call it held and hand the asker its
    /// result, or refuse it. `by` names who decided (the answering principal,
    /// or `on_timeout` when the rule's fallback did).
    pub(crate) fn policy_settle(
        &mut self,
        target: &Target,
        call: &super::reactor::PolicyCall,
        approved: bool,
        by: &str,
    ) {
        // The User's gate says so in its own words, and its own record.
        #[cfg(feature = "companies")]
        if let Some(ask) = &call.company {
            let answer = if approved { "approve" } else { "deny" };
            self.company_gate_said(target, ask, &call.tool, answer, Some(by));
            if !approved {
                let msg = format!(
                    "company {}: the User refused this call — it was NOT executed",
                    ask.company
                );
                self.reply(target, Value::String(msg), true);
                return;
            }
            self.run_approved(target, call);
            return;
        }
        let trace = self.target_trace(target);
        self.log.in_trace(trace.as_ref()).info(
            "tool.policy.decided",
            json!({"tool": call.tool, "rule": call.rule, "approved": approved, "by": by}),
        );
        self.audit(super::audit::AuditEvent {
            action: "tool.policy",
            target: json!({"tool": call.tool, "rule": call.rule}),
            outcome: if approved { "approved" } else { "refused" },
            principal: Some(by),
            role: None,
            request_id: None,
            sid: None,
            trace: trace.as_ref(),
            link: None,
        });
        if !approved {
            let msg = format!(
                "denied by security.policies[{}]: {by} refused this call — it was NOT executed",
                call.rule
            );
            self.reply(target, Value::String(msg), true);
            return;
        }
        self.run_approved(target, call);
    }

    /// Run the call an approved gate held, past that gate and those it had
    /// passed already, and hand the asker its outcome.
    pub(crate) fn run_approved(&mut self, target: &Target, call: &super::reactor::PolicyCall) {
        self.run_again(target, call, call.passed.and(call));
    }

    /// The call a gate held, made again past the checks `passed` says it
    /// passed.
    pub(crate) fn run_again(
        &mut self,
        target: &Target,
        call: &super::reactor::PolicyCall,
        passed: super::reactor::Passed,
    ) {
        if let Some(judged) = &call.judged
            && self.judged_call(&call.tool, &call.args).as_ref() != Some(judged)
        {
            let msg = format!(
                "{}: the Company changed this operation while the call waited for approval, so it would no longer send the request approved — it was NOT executed; call it again to be asked about the request it sends now",
                call.tool
            );
            self.reply(target, Value::String(msg), true);
            return;
        }
        match target {
            Target::Child(node, req) => {
                let (node, req) = (*node, *req);
                // The asker was checked to be alive when the answer landed.
                let Some(caller) = self.child_caller(node, req) else {
                    return;
                };
                let outcome = self.execute_tool_as(&caller, &call.tool, call.args.clone(), passed);
                self.child_tool_outcome(node, req, &call.tool, outcome);
            }
            Target::Step(run, step) => {
                let (run, step) = (run.clone(), step.clone());
                let Some(caller) = self.resume_gated_step(&run, &step) else {
                    return;
                };
                let outcome = self.execute_tool_as(&caller, &call.tool, call.args.clone(), passed);
                self.step_tool_outcome(&run, &step, &call.tool, outcome);
            }
            // A task's question is a gate of its own, never one holding a
            // call.
            Target::McpTask(..) | Target::Elicit(_) => {}
        }
    }

    /// `message.send`: deliver into one of this instance's own conversations.
    ///
    /// The mirror of the `message` node, for callers that are not a workflow
    /// step — a subagent reporting something worth thinking about, or a turn
    /// handing work to another context. It returns as soon as the delivery is
    /// durable; the turn it causes runs on its own schedule, so a caller never
    /// blocks on the agent it just woke.
    ///
    /// Two refusals matter. A caller may not deliver into the conversation it
    /// is itself running in — that is a turn talking to itself, and it is a
    /// loop whichever way the reply goes. And the hop cap applies here exactly
    /// as it does to the node, so a chain routed through a subagent is not a
    /// way around it.
    fn message_send_tool(&mut self, caller: &ToolCaller, args: Value) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let text = args["text"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            return err("message.send: text is required".into());
        }
        let to = args["to"].as_str().unwrap_or(ROOT).trim();
        let ctx = if to.eq_ignore_ascii_case("new") {
            format!("msg-{}", crate::state::ulid::new())
        } else if to.is_empty() {
            ROOT.to_string()
        } else {
            to.to_string()
        };
        if caller.ctx.as_deref() == Some(ctx.as_str()) {
            return err(format!(
                "message.send: {ctx:?} is this caller's own conversation — a turn cannot message itself"
            ));
        }
        let depth = caller.msg_depth + 1;
        let cap = self.settings.limits.message_depth();
        if depth > cap {
            self.log.warn(
                "message.too_deep",
                json!({"caller": caller.label(), "conversation": ctx, "depth": depth, "max": cap}),
            );
            return err(format!(
                "message.send refused: {depth} chained deliveries exceeds limits.max_message_depth ({cap})"
            ));
        }
        // A turn a principal drives is that principal speaking, whatever its
        // model chose for `to`: a caller who may not send into the root, or
        // into a conversation it does not own, must not reach one by asking
        // its model to. So the delivery is held to the owner rule a listener
        // message is, at the turn it would start. An operator's reach is any
        // conversation, and work acting for nobody is the instance's own.
        let owner_checked = self
            .acting_principal(caller.principal.as_deref())
            .is_some_and(|p| !p.is_operator());
        let mut payload = json!({"text": text, "context_id": ctx, "msg_depth": depth});
        if owner_checked {
            payload["owner_checked"] = Value::Bool(true);
        }
        // The text may hold whatever the sender read: the conversation it
        // reaches carries the Companies whose content the sender holds (RFC
        // 0047 §6.2), its delegated data and its taint (RFC 0044 §5.13) —
        // what a conversation keeps of a record (`Carries::kept_by_holder`).
        let carried = self.caller_carries(caller).kept_by_holder(true);
        if !carried.is_empty() {
            payload["carries"] = json!(carried);
        }
        // The turn it starts continues the sender's trace.
        let trace = self.caller_trace(caller);
        match self.accept_event_in(
            super::events::kinds::A2A_MESSAGE,
            caller.principal.clone(),
            caller.session.clone(),
            payload,
            trace.as_ref(),
        ) {
            Ok(_) => ToolOutcome::Ready(
                json!({"delivered": true, "conversation": ctx, "depth": depth}),
                false,
            ),
            Err(e) => err(format!("message.send: {e}")),
        }
    }

    /// The `exec` tool: run one allow-listed command with the `security.exec`
    /// controls on an executor thread (never the reactor). Reached only when the
    /// runner is enabled — otherwise `exec` is mapping-only and this never routes
    /// here. Every guard is re-checked here (defense in depth), not just at build.
    #[cfg(feature = "exec")]
    fn exec_tool(&mut self, caller: &ToolCaller, args: Value) -> ToolOutcome {
        use super::exec;
        let cfg = self.settings.security.exec.clone();
        if !cfg.enabled {
            return ToolOutcome::Ready(
                Value::String("exec: local execution is disabled (security.exec.enabled)".into()),
                true,
            );
        }
        let cmd = args["cmd"].as_str().unwrap_or_default().to_string();
        if cmd.is_empty() {
            return ToolOutcome::Ready(Value::String("exec: `cmd` is required".into()), true);
        }
        // Allow-list (argv[0]); empty allow-list denies everything.
        if !cfg.allow.iter().any(|a| a == &cmd) {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "exec: command {cmd:?} is not in security.exec.allow"
                )),
                true,
            );
        }
        let Some(workdir) = cfg.workdir.clone() else {
            return ToolOutcome::Ready(
                Value::String("exec: security.exec.workdir must be set".into()),
                true,
            );
        };
        let cwd = match exec::resolve_cwd(std::path::Path::new(&workdir), args["cwd"].as_str()) {
            Ok(c) => c,
            Err(e) => return ToolOutcome::Ready(Value::String(format!("exec: {e}")), true),
        };
        let argv: Vec<String> = args["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let stdin = args["cmd_stdin"]
            .as_str()
            .or_else(|| args["stdin"].as_str())
            .map(String::from);
        // Timeout: min(requested, configured max); output cap; env passthrough.
        let max_timeout = cfg.timeout.map(|d| d.0).unwrap_or(Duration::from_secs(30));
        let req = args["timeout"]
            .as_str()
            .and_then(|s| crate::config::parse_duration(s).ok());
        let timeout = req.map(|d| d.min(max_timeout)).unwrap_or(max_timeout);
        let max_output = cfg.max_output.unwrap_or(1 << 20) as usize;
        let env_pass = cfg.env.clone();

        self.log.in_trace(self.caller_trace(caller).as_ref()).info(
            "exec.run",
            json!({"cmd": cmd, "argc": argv.len(), "cwd": cwd.display().to_string(), "timeout_ms": timeout.as_millis() as u64, "caller": caller.label()}),
        );
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        // The command is a span of its own under the caller's.
        let trace = self
            .caller_trace(caller)
            .unwrap_or_else(|| self.trace.clone())
            .child();
        std::thread::Builder::new()
            .name("tool:exec".into())
            .spawn(move || {
                let started = crate::obs::otel::now_unix_nanos();
                let (result, is_error) = match exec::run_command(
                    &cmd,
                    &argv,
                    &cwd,
                    stdin.as_deref(),
                    timeout,
                    max_output,
                    &env_pass,
                    Some(&trace),
                ) {
                    Ok(v) => (v, false),
                    Err(e) => (Value::String(format!("exec: {e}")), true),
                };
                // Exported as the span its `TRACEPARENT` named, so what the
                // command exports has its parent in the collector.
                crate::obs::otel::span(
                    Some(&trace),
                    &format!("exec {cmd}"),
                    crate::obs::otel::Kind::Internal,
                    started,
                )
                .attr("process.executable.name", cmd.as_str())
                .attr_opt("process.exit.code", result["exit_code"].as_i64())
                .end(!is_error && result["exit_code"] == 0);
                target.send(&tx, result, is_error);
            })
            .ok();
        ToolOutcome::Executing
    }

    /// The context a caller addresses (created on demand).
    pub(crate) fn context_for(
        &mut self,
        ctx_id: &str,
        principal: Option<&str>,
    ) -> &mut crate::context::ContextState {
        if ctx_id == ROOT {
            self.contexts.root()
        } else {
            self.contexts.conversation(ctx_id, principal)
        }
    }

    /// Launch a `decide` child for a tool request (RFC 0046 §7): the decision
    /// on `roles.decide`, its answers handed back as the tool's result. An
    /// answer under the call's `confidence` is asked again on the decision
    /// model's `escalate` model when it names one, else marked `unsure` —
    /// the agent asked, and it reads the mark; failing the call would only
    /// hide the answers from it.
    pub(crate) fn start_decide(
        &mut self,
        caller: &ToolCaller,
        args: &Value,
        reply_to: (NodeId, u64),
    ) -> Result<NodeId, String> {
        use crate::subagent::protocol::{DecisionJob, OnUnsure, TurnKind, TurnSpec};
        let decision = crate::wire::decision::Decision {
            state: args.get("state").cloned().unwrap_or(Value::Null),
            questions: crate::wire::decision::Decision::parse_questions(
                args.get("questions").unwrap_or(&Value::Null),
            )
            .map_err(|e| format!("decide: {e}"))?,
        };
        let reference = self
            .settings
            .intelligence
            .reference(crate::config::settings::ModelRole::Decide);
        let escalate = reference
            .as_deref()
            .and_then(|r| self.settings.intelligence.tier(r))
            .and_then(|t| t.escalate.clone())
            .map(|e| self.intel_config(Some(&e)));
        let sure = args
            .get("confidence")
            .and_then(Value::as_f64)
            .or_else(|| {
                self.settings
                    .intelligence
                    .role_confidence(crate::config::settings::ModelRole::Decide)
            })
            .map(|confidence| crate::subagent::protocol::Sureness {
                confidence: confidence.clamp(0.0, 1.0),
                on_unsure: if escalate.is_some() {
                    OnUnsure::Escalate
                } else {
                    OnUnsure::Continue
                },
            });
        let ctx_id = caller.context_id();
        let spec = TurnSpec {
            kind: TurnKind::Decide,
            budget_admission: self.governor.is_active(),
            turn_id: self.next_id("decide"),
            decision: Some(Box::new(DecisionJob {
                decision,
                shape: crate::wire::decision::Shape::Decide,
                escalate,
            })),
            sure,
            ..Default::default()
        };
        let launch = super::turns::TurnLaunch {
            spec,
            kind: ChildKind::Think {
                purpose: "decide".into(),
                ctx: Some(ctx_id.clone()),
                reply_to: Some(reply_to),
                extra: Value::Null,
                reservation: None,
            },
            servers: Vec::new(),
            max_steps: 1,
            max_tokens: self.settings.limits.run.tokens(),
            deadline_ms: 300_000,
            agent_path: format!("decide/{ctx_id}"),
            model: None,
            role: crate::config::settings::ModelRole::Decide,
            effort: None,
            trace: self.caller_trace(caller),
        };
        self.spawn_turn(launch)
    }

    /// Launch a `think` child for a tool request / a step.
    pub(crate) fn start_think(
        &mut self,
        caller: &ToolCaller,
        args: &Value,
        reply_to: Option<(NodeId, u64)>,
    ) -> Result<NodeId, String> {
        let prompt = args["prompt"].as_str().unwrap_or("").to_string();
        if prompt.trim().is_empty() {
            return Err("think: prompt must be non-empty".into());
        }
        // `effort` asks a reasoning model to think harder or less; a model
        // that does not reason has nothing to set it on.
        let effort = match args.get("effort") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let e = v
                    .as_str()
                    .and_then(crate::wire::intel::Effort::parse)
                    .ok_or("think: effort is low, medium or high")?;
                let reasons = self
                    .settings
                    .intelligence
                    .reference(crate::config::settings::ModelRole::Think)
                    .is_some_and(|r| {
                        self.settings.intelligence.kind_of(&r)
                            == crate::config::settings::ModelKind::Reasoning
                    });
                if !reasons {
                    return Err(
                        "think: effort needs a reasoning model, and roles.think is not one".into(),
                    );
                }
                Some(e)
            }
        };
        let ctx_id = caller.context_id();
        let mut messages = Vec::new();
        // `reads`: memory keys folded into the prompt.
        if let Some(reads) = args.get("reads").and_then(Value::as_array) {
            for k in reads.iter().filter_map(Value::as_str) {
                if let Ok((v, held)) = self.memory.get_carrying(&self.durable, k)
                    && v["found"] == json!(true)
                {
                    messages.push(crate::context::Msg::system(format!(
                        "memory[{k}] = {}",
                        v["value"]
                    )));
                    self.hand_caller(caller, &held);
                }
            }
        }
        messages.push(crate::context::Msg::user(prompt, None));
        let output_schema = args.get("output_schema").cloned();
        let system = format!(
            "You are the reasoning module of {}. Think carefully about the request and reply with {}. No tools are available.",
            self.instance,
            if output_schema.is_some() {
                "ONLY one JSON object matching the schema"
            } else {
                "your conclusion (a JSON object when the request asks for structure)"
            }
        );
        let spec = crate::subagent::protocol::TurnSpec {
            kind: crate::subagent::protocol::TurnKind::Think,
            system,
            messages,
            tools: Vec::new(),
            internal: Vec::new(),
            mcp_routes: Default::default(),
            output_schema,
            max_rounds: 3,
            budget_admission: self.governor.is_active(),
            idempotency_prefix: String::new(),
            tool_meta: None,
            temperature: Some(0.0),
            max_tokens_per_call: 0,
            turn_id: self.next_id("think"),
            decision: None,
            sure: None,
        };
        let launch = super::turns::TurnLaunch {
            spec,
            kind: ChildKind::Think {
                purpose: "tool".into(),
                ctx: Some(ctx_id.clone()),
                reply_to,
                extra: Value::Null,
                reservation: None,
            },
            servers: Vec::new(),
            max_steps: 4,
            max_tokens: self.settings.limits.run.tokens(),
            deadline_ms: 300_000,
            agent_path: format!("think/{ctx_id}"),
            model: None,
            role: crate::config::settings::ModelRole::Think,
            effort,
            trace: self.caller_trace(caller),
        };
        self.spawn_turn(launch)
    }

    /// Resolve deferred requests: timers are answered on fire (`on_timer`),
    /// subagents on their result, thinks on their TurnDone; `await`
    /// conditions and run waits are polled here.
    pub(crate) fn poll_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let now = now_ms();
        // Collect by TARGET, never by index: `reply` re-enters the reactor (a
        // step outcome cascades through `finish_step` into
        // `cancel_scoped_children`, which prunes `pending` itself), so an index
        // remembered across a reply addresses a different entry by the time we
        // get to it — or one past the end, panicking the reactor thread and
        // taking the daemon with it. Two `race` branches waiting on the same
        // deadline are the live case: both land in `done` in one pass, the
        // winner's reply cancels the loser's branch.
        let mut done: Vec<(Target, Value, bool)> = Vec::new();
        for p in self.pending.iter() {
            let t = &p.target;
            match &p.kind {
                PendingKind::Await {
                    condition,
                    deadline_ms,
                } => {
                    let data = self.await_data();
                    let vars: Vec<(&str, &Value)> =
                        data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    match crate::cel::eval_bool(
                        condition.trim().trim_start_matches("CEL:").trim(),
                        &vars,
                    ) {
                        Ok(true) => done.push((t.clone(), json!({"satisfied": true}), false)),
                        Ok(false) if now >= *deadline_ms => done.push((
                            t.clone(),
                            json!({"satisfied": false, "timed_out": true}),
                            false,
                        )),
                        Ok(false) => {}
                        Err(e) => {
                            done.push((t.clone(), Value::String(format!("await: {e}")), true))
                        }
                    }
                }
                // A run's terminal step answers its waiters itself
                // (`on_run_terminal`, withheld there), so a terminal run is
                // only found here by a wait that step missed. The withholding
                // below is a safety net no test reaches; a child's is reached
                // by an instance child in `mode: sync`, whose report arrives
                // while it keeps running.
                PendingKind::Run { run, deadline_ms } => match self.runs.get(run) {
                    Some(r) if r.status.is_terminal() => {
                        let mut v = json!({"run": run, "status": r.status, "output": r.output, "error": r.error});
                        self.withhold_reply(Some(&self.reader_at(t)), &mut v, &p.name);
                        done.push((t.clone(), v, false))
                    }
                    Some(_) if now >= *deadline_ms => done.push((
                        t.clone(),
                        json!({"run": run, "status": "running", "timed_out": true}),
                        false,
                    )),
                    Some(_) => {}
                    None => done.push((
                        t.clone(),
                        Value::String(format!("run {run:?} does not exist")),
                        true,
                    )),
                },
                PendingKind::Subagent { handle } => {
                    // A terminal child always resolves the wait; an instance
                    // child in `mode: sync` ALSO resolves as soon as its
                    // reporter delivers the declared workflow's first result,
                    // because the child then keeps running under its own
                    // lifecycle and would never reach a terminal status.
                    if let Some(s) = self.subagents.get(handle)
                        && (super::reactor::is_terminal_status(&s.status)
                            || (s.tier.as_deref() == Some("instance")
                                && s.mode == "sync"
                                && s.result.is_some()))
                    {
                        let mut v = json!({"handle": handle, "status": s.status, "result": s.result, "error": s.error});
                        self.withhold_reply(Some(&self.reader_at(t)), &mut v, &p.name);
                        done.push((t.clone(), v, false));
                    }
                }
                // Human gates run their own pass (auto-judge + prune + timeout),
                // and so do Company sign-ins, approvals and Direct Conversations.
                PendingKind::Timer { .. }
                | PendingKind::Think { .. }
                | PendingKind::Human { .. }
                | PendingKind::CompanySignIn { .. }
                | PendingKind::CompanyOperation { .. }
                | PendingKind::CompanyDirect { .. }
                | PendingKind::Consent { .. } => {}
            }
        }
        for (target, v, e) in done {
            // A reentrant prune may already have removed (and cancelled) this
            // entry while we were replying to an earlier one. Such an entry is
            // not waiting on anything, so answering it would resurrect a
            // cancelled branch — skip it when the retain took nothing out.
            let len = self.pending.len();
            self.pending.retain(|p| p.target != target);
            if self.pending.len() == len {
                continue;
            }
            self.reply(&target, v, e);
        }
        self.poll_pending_human();
    }

    /// The variables an `await` condition sees: memory (by key), runs, subagents.
    fn await_data(&self) -> crate::engine::template::Data {
        let mut d = crate::engine::template::Data::new();
        d.insert(
            "runs".into(),
            Value::Object(
                self.runs
                    .iter()
                    .map(|(k, r)| (k.clone(), json!({"status": r.status, "output": r.output})))
                    .collect(),
            ),
        );
        d.insert(
            "subagents".into(),
            Value::Object(
                self.subagents
                    .iter()
                    .map(|(k, s)| (k.clone(), json!({"status": s.status, "result": s.result})))
                    .collect(),
            ),
        );
        d.insert("now_ms".into(), json!(now_ms()));
        d
    }

    /// A durable timer fired.
    pub(crate) fn on_timer(&mut self, t: crate::state::TimerRecord) {
        let owner = &t.owner;
        match owner["kind"].as_str() {
            Some("tool") => {
                let node = NodeId(owner["node"].as_u64().unwrap_or(0));
                let req = owner["req"].as_u64().unwrap_or(0);
                self.pending
                    .retain(|p| p.target != Target::Child(node, req));
                self.reply_tool(node, req, t.payload.clone(), false);
            }
            Some("step") | Some("step_budget") => {
                let run = owner["run"].as_str().unwrap_or("").to_string();
                let step = owner["step"].as_str().unwrap_or("").to_string();
                self.on_step_timer(
                    &run,
                    &step,
                    owner["kind"].as_str() == Some("step_budget"),
                    &t.payload,
                );
            }
            Some("goal") => self.on_goal_check(&t.payload),
            Some("freshness") => self.on_freshness_check(&t.payload),
            other => self
                .log
                .warn("timer.unknown_owner", json!({"id": t.id, "owner": other})),
        }
    }

    /// An executor thread answered a child's mapped/MCP request.
    pub(crate) fn on_tool_done(&mut self, node: NodeId, req: u64, result: Value, is_error: bool) {
        self.reply_tool(node, req, result, is_error);
    }

    /// Take one token from a catalogued service's `rate:` pacing bucket,
    /// erroring when the bucket is dry.
    /// Thin wrapper over the process-global registry [`crate::mcp::pace`],
    /// seeded at client construction — one mechanism for the reactor's step
    /// path, the mapped-tool path, and (in their own processes) the turn
    /// worker's and flat subagent's in-loop calls.
    pub(crate) fn service_rate_take(&self, server: &str) -> Result<(), String> {
        crate::mcp::pace::take(server)
    }
}

#[derive(Debug, Clone)]
enum RouteKind {
    Internal,
    Mapped(crate::registry::Mapping),
    Code,
    Mcp(String, String),
    /// A workflow run: `(workflow name, wait for it)`.
    Workflow(String, bool),
}

/// Where an executor thread's result goes.
pub(super) enum ExecTarget {
    Tool { node: NodeId, req: u64 },
    Step { run: String, step: String },
}

impl ExecTarget {
    pub(super) fn from(caller: &ToolCaller) -> ExecTarget {
        match (caller.node, &caller.run, &caller.step) {
            (Some(node), _, _) => ExecTarget::Tool {
                node,
                req: caller.req,
            },
            (None, Some(run), Some(step)) => ExecTarget::Step {
                run: run.clone(),
                step: step.clone(),
            },
            _ => ExecTarget::Tool {
                node: NodeId(0),
                req: caller.req,
            },
        }
    }
    pub(super) fn send(self, tx: &std::sync::mpsc::Sender<Event>, result: Value, is_error: bool) {
        let ev = match self {
            ExecTarget::Tool { node, req } => Event::ToolDone {
                node,
                req,
                result,
                is_error,
            },
            ExecTarget::Step { run, step } => Event::StepDone {
                run,
                step,
                error: is_error.then(|| result.to_string()),
                output: result,
                is_error,
                tokens: 0,
            },
        };
        let _ = tx.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn uses a delegated server's tool only where its plan offered
    /// it; a tool of no delegated server is not this check's.
    #[test]
    fn a_turn_uses_only_the_delegated_tools_its_plan_offered() {
        let offered: std::collections::BTreeSet<String> = ["linear.list_issues".to_string()].into();
        assert_eq!(use_unauthorised(&offered, "linear.list_issues", true), None);
        let refused = use_unauthorised(&offered, "linear.create_issue", true);
        assert!(
            refused
                .as_deref()
                .is_some_and(|r| r.starts_with("linear.create_issue: refused")),
            "{refused:?}"
        );
        assert_eq!(use_unauthorised(&offered, "status", false), None);
    }

    /// A step's own call keeps its key across its attempts; a model's call
    /// inside the step is its child's, so two attempts — two children, each
    /// counting its requests from 1 — and two lives never share one.
    #[test]
    fn a_steps_call_keeps_its_key_and_a_models_call_in_it_has_its_own() {
        let step = |req: u64| ToolCaller {
            run: Some("r1".into()),
            step: Some("ask".into()),
            req,
            ..Default::default()
        };
        let key = call_key("inst", "p1", &step(1));
        assert_eq!(key, "inst/step:r1/ask");
        assert_eq!(key, call_key("inst", "p1", &step(2)), "a retry");
        assert_eq!(key, call_key("inst", "p2", &step(2)), "a replay");
        assert_ne!(
            key,
            call_key(
                "inst",
                "p1",
                &ToolCaller {
                    step: Some("other".into()),
                    ..step(1)
                }
            )
        );
        let turn = |node: u64, nonce: &str| {
            call_key(
                "inst",
                nonce,
                &ToolCaller {
                    node: Some(NodeId(node)),
                    ..step(1)
                },
            )
        };
        assert_eq!(turn(3, "p1"), turn(3, "p1"), "a retry of one call");
        assert_ne!(turn(3, "p1"), turn(4, "p1"), "the step's next attempt");
        assert_ne!(turn(3, "p1"), turn(3, "p2"), "the next life");
        assert_ne!(turn(3, "p1"), key);
    }
}
