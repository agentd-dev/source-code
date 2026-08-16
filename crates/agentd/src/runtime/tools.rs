// SPDX-License-Identifier: Apache-2.0
//! **Internal tool execution** (RFC 0028 §3): the runtime is the single place
//! internal tools run — for a turn worker's `ToolRequest` (answered with
//! `ToolResult`), for a workflow step (`tool` / `memory.*` / … kinds) and for
//! A2A commands (P5). Arguments are validated against the contract's input
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
}

impl ToolCaller {
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
        let caller = match self.children.get(node).map(|c| c.kind.clone()) {
            Some(ChildKind::RootTurn { ctx, .. }) => ToolCaller {
                node: Some(node),
                req: id,
                ctx: Some(ctx.clone()),
                principal: self.contexts.get(&ctx).and_then(|c| c.principal.clone()),
                ..Default::default()
            },
            Some(ChildKind::StepTurn { run, step, .. }) => ToolCaller {
                node: Some(node),
                req: id,
                run: Some(run.clone()),
                step: Some(step),
                ctx: self.runs.get(&run).and_then(|r| r.conversation.clone()),
                principal: self.runs.get(&run).and_then(|r| r.principal.clone()),
                ..Default::default()
            },
            Some(ChildKind::Subagent { handle }) => ToolCaller {
                node: Some(node),
                req: id,
                subagent: Some(handle),
                ..Default::default()
            },
            Some(ChildKind::Think { ctx, .. }) => ToolCaller {
                node: Some(node),
                req: id,
                ctx,
                ..Default::default()
            },
            None => return,
        };
        self.log.info("tool.request", json!({"node": node.0, "req": id, "tool": name, "caller": caller.label(), "args": if self.log.content_capture() { args.clone() } else { Value::Null }}));
        match self.execute_tool(&caller, name, args) {
            ToolOutcome::Ready(v, err) => self.reply_tool(node, id, v, err),
            ToolOutcome::Deferred(kind) => {
                self.pending.push(PendingTool {
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
        self.log.debug(
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
        }
    }

    /// Execute an internal (or mapped) tool for `caller`.
    pub(crate) fn execute_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
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
        let route = self.registry.route(name).map(|r| match r {
            Route::Internal => RouteKind::Internal,
            Route::Mapped(m) => RouteKind::Mapped(m.clone()),
            Route::Code => RouteKind::Code,
            Route::Mcp { server, tool } => RouteKind::Mcp(server.to_string(), tool.to_string()),
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
        let Some(client) = self.mcp.get(&m.server).cloned() else {
            return ToolOutcome::Ready(
                Value::String(format!("server {:?} for {name} is not connected", m.server)),
                true,
            );
        };
        let mapping = m.clone();
        let tool_name = name.to_string();
        let meta = json!({"agent/idempotency_key": format!("{}/{}#{}", self.instance, caller.label(), caller.req), "agent/instance": self.instance});
        let timeout = self
            .settings
            .mcp
            .default_timeout
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(60));
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        let call_ctx = ctx.clone();
        std::thread::Builder::new()
            .name(format!("tool:{tool_name}"))
            .spawn(move || {
                let res =
                    client.call_tool_with_meta_within(&mapping.tool, Some(mcp_args), meta, timeout);
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
        let Some(client) = self.mcp.get(server).cloned() else {
            return ToolOutcome::Ready(
                Value::String(format!("server {server:?} for {name} is not connected")),
                true,
            );
        };
        let tool = tool.to_string();
        let meta = json!({"agent/idempotency_key": format!("{}/{}#{}", self.instance, caller.label(), caller.req), "agent/instance": self.instance});
        let timeout = self
            .settings
            .mcp
            .default_timeout
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(60));
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        std::thread::Builder::new()
            .name(format!("mcp:{server}.{tool}"))
            .spawn(move || {
                let (result, is_error) =
                    match client.call_tool_with_meta_within(&tool, Some(args), meta, timeout) {
                        Ok(r) => (super::worker::tool_result_value(&r), r.is_error()),
                        Err(e) => (Value::String(format!("transport error: {e}")), true),
                    };
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
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.instruction.uri.clone());
                match uri {
                    None => err(
                        "instruction.subscribe: the instruction is static text; give a uri".into(),
                    ),
                    Some(u) => match self.subscribe_instruction(&u) {
                        Ok(()) => ok(json!({"subscribed": true, "uri": u})),
                        Err(e) => err(e),
                    },
                }
            }
            // ---- memory ----
            "memory.get" => match self
                .memory
                .get(&self.durable, args["key"].as_str().unwrap_or(""))
            {
                Ok(v) => ok(v),
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
                match self.memory.set(
                    &self.durable,
                    args["key"].as_str().unwrap_or(""),
                    args["value"].clone(),
                    ttl,
                    Some(&by),
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
                    },
                ) {
                    Ok(v) => ok(v),
                    Err(e) => err(e),
                }
            }
            "artifact.get" => match self.artifacts.get_value(args["id"].as_str().unwrap_or("")) {
                Ok(v) => ok(v),
                Err(e) => err(e),
            },
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
            "skills.load" => {
                let ctx_id = caller.context_id();
                let name = args["name"].as_str().unwrap_or("").to_string();
                let mcp = self.mcp.clone();
                let resolver = move |server: &str| -> Option<
                    std::sync::Arc<dyn crate::context::skills::SkillServer>,
                > {
                    mcp.get(server).map(|c| {
                        c.clone() as std::sync::Arc<dyn crate::context::skills::SkillServer>
                    })
                };
                match self
                    .skills
                    .load(&name, args.get("arguments").cloned(), &resolver)
                {
                    Ok(body) => {
                        let max_loaded = self.settings.skills.max_loaded.unwrap_or(8) as usize;
                        let c = self.context_for(&ctx_id, caller.principal.as_deref());
                        match c.load_skill(&name, &body.hash, max_loaded) {
                            Ok(()) => ok(
                                json!({"loaded": true, "name": name, "hash": body.hash, "body": body.body}),
                            ),
                            Err(e) => err(e),
                        }
                    }
                    Err(e) => err(e),
                }
            }
            "skills.unload" => {
                let ctx_id = caller.context_id();
                let c = self.context_for(&ctx_id, caller.principal.as_deref());
                ok(json!({"ok": c.unload_skill(args["name"].as_str().unwrap_or(""))}))
            }
            // ---- status ----
            "status" => ok(self.status_value()),
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
                match caller.node {
                    Some(node) => {
                        self.start_compaction(&ctx_id, keep_last, target, Some((node, caller.req)));
                        ToolOutcome::Deferred(PendingKind::Think {
                            child: NodeId(u64::MAX),
                        })
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
            // ---- lifecycle ----
            "finish" => {
                // The turn worker records the finish itself (RFC 0026 §3.2); the
                // runtime acknowledges. Steps: the `finish` kind.
                ok(json!({"ok": true}))
            }
            // Human-in-the-loop (RFC 0032 §16): gate through the interface, or
            // apply the configured fallback (fail | wait | auto judge).
            "ask_human" => self.ask_human_tool(caller, args),
            // ---- subagents ----
            "subagent.run" | "subagent.send" | "subagent.kill" | "subagent.status"
            | "subagent.await" | "subagent.list" => self.subagent_tool(caller, name, args),
            // ---- workflows ----
            "workflow.run" | "workflow.list" | "workflow.status" | "workflow.cancel"
            | "workflow.wait" | "workflow.create" | "workflow.update" | "workflow.delete"
            | "workflow.pause" | "workflow.resume" | "workflow.signal" => {
                self.workflow_tool(caller, name, args)
            }
            // ---- guarded local command runner (RFC 0028 §exec; default-OFF) ----
            #[cfg(feature = "exec")]
            "exec" => self.exec_tool(caller, args),
            other => err(format!(
                "internal tool {other:?} has no built-in implementation"
            )),
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

        self.log.info(
            "exec.run",
            json!({"cmd": cmd, "argc": argv.len(), "cwd": cwd.display().to_string(), "timeout_ms": timeout.as_millis() as u64, "caller": caller.label()}),
        );
        let tx = self.events_tx.clone();
        let target = ExecTarget::from(caller);
        std::thread::Builder::new()
            .name("tool:exec".into())
            .spawn(move || {
                let (result, is_error) = match exec::run_command(
                    &cmd,
                    &argv,
                    &cwd,
                    stdin.as_deref(),
                    timeout,
                    max_output,
                    &env_pass,
                ) {
                    Ok(v) => (v, false),
                    Err(e) => (Value::String(format!("exec: {e}")), true),
                };
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
        let ctx_id = caller.context_id();
        let mut messages = Vec::new();
        // `reads`: memory keys folded into the prompt.
        if let Some(reads) = args.get("reads").and_then(Value::as_array) {
            for k in reads.iter().filter_map(Value::as_str) {
                if let Ok(v) = self.memory.get(&self.durable, k)
                    && v["found"] == json!(true)
                {
                    messages.push(crate::context::Msg::system(format!(
                        "memory[{k}] = {}",
                        v["value"]
                    )));
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
        let mut done: Vec<(usize, Value, bool)> = Vec::new();
        for (i, p) in self.pending.iter().enumerate() {
            match &p.kind {
                PendingKind::Await { condition, deadline_ms } => {
                    let data = self.await_data();
                    let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    match crate::cel::eval_bool(condition.trim().trim_start_matches("CEL:").trim(), &vars) {
                        Ok(true) => done.push((i, json!({"satisfied": true}), false)),
                        Ok(false) if now >= *deadline_ms => done.push((i, json!({"satisfied": false, "timed_out": true}), false)),
                        Ok(false) => {}
                        Err(e) => done.push((i, Value::String(format!("await: {e}")), true)),
                    }
                }
                PendingKind::Run { run, deadline_ms } => match self.runs.get(run) {
                    Some(r) if r.status.is_terminal() => done.push((i, json!({"run": run, "status": r.status, "output": r.output, "error": r.error}), false)),
                    Some(_) if now >= *deadline_ms => done.push((i, json!({"run": run, "status": "running", "timed_out": true}), false)),
                    Some(_) => {}
                    None => done.push((i, Value::String(format!("run {run:?} does not exist")), true)),
                },
                PendingKind::Subagent { handle } => {
                    if let Some(s) = self.subagents.get(handle)
                        && super::reactor::is_terminal_status(&s.status)
                    {
                        done.push((i, json!({"handle": handle, "status": s.status, "result": s.result, "error": s.error}), false));
                    }
                }
                // Human gates run their own pass (auto-judge + prune + timeout).
                PendingKind::Timer { .. }
                | PendingKind::Think { .. }
                | PendingKind::Human { .. } => {}
            }
        }
        for (i, v, e) in done.into_iter().rev() {
            let p = self.pending.remove(i);
            self.reply(&p.target, v, e);
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
            other => self
                .log
                .warn("timer.unknown_owner", json!({"id": t.id, "owner": other})),
        }
    }

    /// An executor thread answered a child's mapped/MCP request.
    pub(crate) fn on_tool_done(&mut self, node: NodeId, req: u64, result: Value, is_error: bool) {
        self.reply_tool(node, req, result, is_error);
    }
}

#[derive(Debug, Clone)]
enum RouteKind {
    Internal,
    Mapped(crate::registry::Mapping),
    Code,
    Mcp(String, String),
}

/// Where an executor thread's result goes.
enum ExecTarget {
    Tool { node: NodeId, req: u64 },
    Step { run: String, step: String },
}

impl ExecTarget {
    fn from(caller: &ToolCaller) -> ExecTarget {
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
    fn send(self, tx: &std::sync::mpsc::Sender<Event>, result: Value, is_error: bool) {
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
