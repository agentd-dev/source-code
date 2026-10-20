// SPDX-License-Identifier: AGPL-3.0-only
//! The **subagent registry** behind the `subagent.*` tools: flat
//! children spawned from the one chokepoint (caps: depth/breadth/total/rate),
//! recorded durably as `subagent/<handle>` (payload, mode, status, result),
//! with `sync` (the caller waits), `async` (a handle; `subagent.await`),
//! `detached` (fire and forget) and `warm` (stays alive; `subagent.send`).

use super::children::ChildKind;
use super::reactor::{PendingKind, Runtime, SubagentRecord, is_terminal_status};
use super::tools::{ToolCaller, ToolOutcome};
use crate::agentloop::stop::Outcome;
use crate::context::Msg;
use crate::state::now_ms;
use crate::subagent::protocol::{
    ControlMsg, IntelConfig, Limits, Role, SeedMessage, SpawnPayload, Telemetry,
};
use crate::supervisor::tree::{NodeId, TokenBucket};
use serde_json::{Value, json};
use std::time::Duration;

/// Distillation cap for a subagent result carried back to a caller.
const DISTILL_CAP: usize = 8_000;

impl Runtime {
    /// The `subagent.*` built-ins.
    pub(crate) fn subagent_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        // A tool that names a subagent acts on it only for its owner: the
        // model driving a user's turn must not steer, read or kill another
        // principal's children by guessing a handle, any more than that user
        // could over A2A. Not-yours reads as unknown, and is asked before
        // anything that could tell "not running" or "not warm" apart.
        if let Some(handle) = args.get("handle").and_then(Value::as_str)
            && name != "subagent.run"
            && !self.tool_owns_subagent(caller, handle)
        {
            return err(format!("no such subagent {handle:?}"));
        }
        match name {
            "subagent.run" => self.subagent_run(caller, &args),
            "subagent.send" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                let message = args["message"].as_str().unwrap_or("").to_string();
                // An instance-tier child is a separate agent, not a worker in
                // this process's tree, so it receives over its A2A socket and
                // the message lands in its own conversation surface.
                // The message is the sender's text: the child carries what
                // the sender does from here on (`withhold`), so a result read
                // back after it is judged with it — whoever spawned the child.
                let carries = self.caller_carries(caller);
                if self
                    .subagents
                    .get(&handle)
                    .is_some_and(|s| s.tier.as_deref() == Some("instance"))
                {
                    // Recorded before the send, which may answer later: a
                    // send that then fails leaves the child carrying more
                    // than it was handed, which errs toward withholding.
                    self.steer_carries(&handle, &carries);
                    let trace = self.caller_trace(caller);
                    return self.instance_send(&handle, &message, trace.as_ref());
                }
                let Some(node) = self.subagents.get(&handle).and_then(|s| s.node) else {
                    return err(format!("subagent {handle:?} is not running"));
                };
                if !self
                    .subagents
                    .get(&handle)
                    .is_some_and(|s| s.mode == "warm")
                {
                    return err(format!("subagent {handle:?} is not a warm subagent"));
                }
                if self.children.send(node, &ControlMsg::Inject { message }) {
                    self.steer_carries(&handle, &carries);
                    ToolOutcome::Ready(json!({"ok": true, "handle": handle}), false)
                } else {
                    err(format!("subagent {handle:?}: send failed"))
                }
            }
            "subagent.retire" => {
                // Begin graceful retirement of an instance child: SIGTERM, then
                // the child drains its own runs and exits cleanly.
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                match self.subagents.get(&handle) {
                    None => err(format!("no such subagent {handle:?}")),
                    Some(s) if s.tier.as_deref() != Some("instance") => err(format!(
                        "subagent {handle:?} is not an instance-tier child (use subagent.kill for flat workers)"
                    )),
                    Some(_) => {
                        if self.retire_instance(&handle, "subagent.retire") {
                            ToolOutcome::Ready(
                                json!({"ok": true, "handle": handle, "status": "retiring"}),
                                false,
                            )
                        } else {
                            err(format!(
                                "subagent {handle:?} is not retirable (already terminal?)"
                            ))
                        }
                    }
                }
            }
            "subagent.kill" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("killed by request")
                    .to_string();
                let Some(node) = self.subagents.get(&handle).and_then(|s| s.node) else {
                    return err(format!("subagent {handle:?} is not running"));
                };
                // `Cancel` now, which a child between steps stops on; the kill
                // ladder after, for one inside a model or MCP tool call, which
                // cannot read it: `SIGTERM` to its process group at the grace,
                // `SIGKILL` after the kill grace (`supervisor::kill`).
                self.children.stop_for_good(node, &reason);
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "cancelled".into();
                    s.error = Some(reason);
                    s.killed = true;
                    s.updated = now_ms();
                    s.dirty = true;
                }
                self.subagent_log(&handle)
                    .info("subagent.kill", json!({"handle": handle}));
                ToolOutcome::Ready(json!({"ok": true, "handle": handle}), false)
            }
            "subagent.status" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                match self.subagents.get(&handle) {
                    // `error` only when there is one: the contract says it is a
                    // string, and a reply that breaks its own contract reaches
                    // the model as a schema error instead of the status.
                    Some(s) => {
                        let mut v = json!({"handle": handle, "status": s.status, "mode": s.mode, "result": s.result, "tokens": s.tokens});
                        if let Some(e) = &s.error {
                            v["error"] = json!(e);
                        }
                        ToolOutcome::Ready(v, false)
                    }
                    None => err(format!("no such subagent {handle:?}")),
                }
            }
            "subagent.await" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                match self.subagents.get(&handle) {
                    None => err(format!("no such subagent {handle:?}")),
                    Some(s) if is_terminal_status(&s.status) => {
                        let mut v =
                            json!({"handle": handle, "status": s.status, "result": s.result});
                        if let Some(e) = &s.error {
                            v["error"] = json!(e);
                        }
                        ToolOutcome::Ready(v, false)
                    }
                    Some(_) => ToolOutcome::Deferred(PendingKind::Subagent { handle }),
                }
            }
            "subagent.list" => {
                let acting = self.acting_principal(caller.principal.as_deref());
                ToolOutcome::Ready(
                    json!({"subagents": self.subagents.values()
                        .filter(|s| acting.as_ref().is_none_or(|p| super::reactor::may_act_on(p, s.principal.as_deref())))
                        .map(|s| json!({"handle": s.handle, "mode": s.mode, "status": s.status, "instruction": s.instruction.chars().take(80).collect::<String>(), "created": s.created})).collect::<Vec<_>>()}),
                    false,
                )
            }
            _ => err(format!("unknown subagent tool {name}")),
        }
    }

    /// `subagent.run`: caps → durable record → spawn, in that order, so a
    /// child that survives a crash always has a record to be reconciled
    /// against. A `template:` reference resolves first — a flat template merges
    /// into the args below; an instance template takes the daemon-child path.
    fn subagent_run(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let (eff_args, tmeta) = match self.resolve_spawn_args(args) {
            Ok(x) => x,
            Err(e) => return err(e),
        };
        if let Some((tname, tier)) = &tmeta
            && tier == "instance"
        {
            let tname = tname.clone();
            return self.instance_run(caller, &tname, &eff_args);
        }
        let args = &eff_args;
        let instruction = args["instruction"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if instruction.is_empty() {
            return err("subagent.run: instruction must be non-empty".into());
        }
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("sync")
            .to_string();
        if !matches!(mode.as_str(), "sync" | "async" | "detached" | "warm") {
            return err("subagent.run: mode must be sync|async|detached|warm".into());
        }
        // Caps: breadth (live), total (lifetime), rate, depth. A subagent
        // asking for a subagent goes through this same chokepoint, so the
        // logical depth is the requester's depth + 1 and no branch of the tree
        // can grow past the configured limits by recursing.
        let live = self
            .subagents
            .values()
            .filter(|s| !is_terminal_status(&s.status))
            .count() as u32;
        let breadth = self.settings.limits.subagents.breadth.unwrap_or(8);
        if live >= breadth {
            crate::obs::metrics::record_refusal("breadth");
            return err(format!(
                "subagent.run refused: {live} subagents live (limits.subagents.breadth = {breadth})"
            ));
        }
        let total = self.settings.limits.subagents.total.unwrap_or(64) as usize;
        if self.subagents.len() >= total {
            crate::obs::metrics::record_refusal("total");
            return err(format!(
                "subagent.run refused: {} subagents spawned (limits.subagents.total = {total})",
                self.subagents.len()
            ));
        }
        let depth = caller
            .subagent
            .as_ref()
            .and_then(|h| self.subagents.get(h))
            .map(|s| {
                s.requested_by
                    .as_ref()
                    .and_then(|r| r["depth"].as_u64())
                    .unwrap_or(0) as u32
                    + 1
            })
            .unwrap_or(0);
        let max_depth = self.settings.limits.subagents.depth.unwrap_or(3);
        if depth >= max_depth {
            crate::obs::metrics::record_refusal("depth");
            return err(format!(
                "subagent.run refused: delegation depth {depth} reaches limits.subagents.depth = {max_depth}"
            ));
        }
        if !self.spawn_bucket_take() {
            crate::obs::metrics::record_refusal("rate");
            return err("subagent.run refused: spawn rate exceeded (limits.subagents.rate)".into());
        }
        if self.pressure.shedding() {
            return err(format!(
                "subagent.run refused: {} pressure (shedding new work; in-flight work drains)",
                self.pressure.cause()
            ));
        }
        // Tools / servers narrowing.
        let allow: Option<Vec<String>> = args.get("tools").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
        let servers: Vec<String> = match args.get("servers").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| self.mcp_specs.contains_key(*s))
                .map(str::to_string)
                .collect(),
            None => self.mcp_specs.keys().cloned().collect(),
        };
        // The trifecta gate over the narrowed grant.
        let tags: Vec<crate::sec::scope::TrifectaTag> = servers
            .iter()
            .filter_map(|s| self.mcp_specs.get(s))
            .flat_map(|s| s.tags.iter().copied())
            .collect();
        if crate::sec::scope::check_trifecta(
            tags.iter().copied(),
            self.settings.security.allow_trifecta,
        )
        .is_refused()
        {
            crate::obs::metrics::record_refusal("trifecta");
            return err("subagent.run refused: the requested MCP servers form a lethal trifecta (untrusted input + sensitive + egress); set security.allow_trifecta to override".into());
        }
        let handle = self.next_id("sub");
        let limits = args.get("limits").cloned().unwrap_or(json!({}));
        let steps = limits
            .get("steps")
            .and_then(Value::as_u64)
            .map(|s| s as u32)
            .unwrap_or(self.settings.limits.run.steps());
        let tokens = limits
            .get("tokens")
            .and_then(Value::as_u64)
            .unwrap_or(self.settings.limits.run.tokens());
        let deadline_ms = limits
            .get("deadline")
            .and_then(Value::as_str)
            .and_then(|d| crate::config::parse_duration(d).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(self.settings.limits.run.deadline().as_millis() as u64);
        // OS-level allocation (RLIMIT_AS / RLIMIT_CPU, applied fork→exec) and
        // the priority→niceness mapping. Parse errors refuse the spawn — a cap
        // that silently failed to parse is a cap that silently does not exist.
        let memory_bytes = match limits.get("memory").and_then(Value::as_str) {
            None => None,
            Some(m) => match crate::runtime::pressure::parse_bytes(m) {
                Ok(b) => Some(b),
                Err(e) => return err(format!("subagent.run: limits.memory: {e}")),
            },
        };
        let cpu_seconds = match limits.get("cpu").and_then(Value::as_str) {
            None => None,
            Some(c) => match crate::config::parse_duration(c) {
                Ok(d) => Some(d.as_secs().max(1)),
                Err(e) => return err(format!("subagent.run: limits.cpu: {e}")),
            },
        };
        let priority = match crate::engine::model::Priority::from_spec(args.get("priority")) {
            Ok(p) => p,
            Err(e) => return err(format!("subagent.run: {e}")),
        };
        if let Some(cause) = self
            .pressure
            .refusal(priority == crate::engine::model::Priority::Low)
        {
            return err(format!("subagent.run refused: {cause}"));
        }
        let context_seed: Vec<SeedMessage> = args
            .get("context")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        let role = m["role"].as_str()?;
                        // The tool grant is minted below from `tools:` alone —
                        // a caller cannot smuggle one in through `context`
                        // (a forged one could only narrow its own child, but the
                        // supervisor owns the grant, so there is one source).
                        if role == crate::subagent::protocol::ALLOWED_TOOLS_ROLE {
                            return None;
                        }
                        Some(SeedMessage {
                            role: role.to_string(),
                            content: m["content"].as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let output_contract = args
            .get("output_contract")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                args.get("output_schema").map(|s| {
                    format!("Reply with ONLY one JSON object matching this JSON Schema: {s}")
                })
            });
        // Which of this child's tools a policy rule might touch. It calls its
        // MCP servers directly, so anything named here has to come back to the
        // supervisor or the rule silently never applies to the caller an
        // operator is most likely narrowing. Of the MCP tools, only those of
        // the servers it was handed: the supervisor serves a gated call over
        // its own connections, so an MCP tool gated beyond those would be one
        // the child's model could call by name and reach a server the spawn
        // never gave it. (An internal tool a rule might touch is served the
        // same way, deliberately: `status`, `ask_human` and the rest are
        // granted to subagents, and the stream-taint check counts them.)
        let mut gated_tools: Vec<String> = if self.settings.security.policies.is_empty() {
            Vec::new()
        } else {
            self.registry
                .defs_for(&crate::registry::Caller::Subagent { allow: None }, None)
                .iter()
                .map(|d| d.name.clone())
                .filter(|n| match self.registry.get(n).map(|t| &t.imp) {
                    Some(crate::registry::Impl::Mcp { server, .. }) => servers.contains(server),
                    _ => true,
                })
                .filter(|n| {
                    crate::sec::policy::could_apply(
                        &self.settings.security.policies,
                        n,
                        &self.registry.tags_of(std::slice::from_ref(n)),
                        crate::config::settings::PolicyCaller::Subagent,
                    )
                })
                .collect()
        };
        // Where a person's grant is delegated, every egress tool on its
        // servers comes back too (`Runtime::egress_round_trips`).
        for d in self
            .registry
            .defs_for(&crate::registry::Caller::Subagent { allow: None }, None)
        {
            let on_its_servers = matches!(
                self.registry.get(&d.name).map(|t| &t.imp),
                Some(crate::registry::Impl::Mcp { server, .. }) if servers.contains(server)
            );
            if on_its_servers
                && !gated_tools.contains(&d.name)
                && self.egress_round_trips(&self.registry.tags_of(std::slice::from_ref(&d.name)))
            {
                gated_tools.push(d.name);
            }
        }
        // A Company's tools on the servers it was handed come back to the
        // supervisor whatever the policies say, and only in work the
        // Company's User owns (RFC 0047 §5.6): the child has no connection
        // to a Company, so it is handed their definitions to offer its model,
        // and its calls round-trip.
        let company_tools: Vec<crate::wire::intel::ToolDef> = if self
            .company_work_admitted(caller.principal.as_deref())
            .is_ok()
        {
            self.registry
                .defs_for(&crate::registry::Caller::Subagent { allow: None }, None)
                .into_iter()
                .filter(|d| {
                    self.company_server_of(&d.name)
                        .is_some_and(|s| servers.contains(&s))
                })
                .collect()
        } else {
            Vec::new()
        };
        for d in &company_tools {
            if !gated_tools.contains(&d.name) {
                gated_tools.push(d.name.clone());
            }
        }
        // The child's MCP catalogue, by the registry's names
        // (`SpawnPayload::mcp_routes`): each MCP tool the registry offers a
        // subagent on the servers it was handed, and a profile server's
        // contract tools under the contract's name where the mapping is a
        // plain rename the child can make itself. Never a Company's.
        let profiles = crate::registry::profile_servers(&self.settings);
        let mcp_routes: std::collections::BTreeMap<String, (String, String)> = self
            .registry
            .defs_for(&crate::registry::Caller::Subagent { allow: None }, None)
            .iter()
            .filter_map(|d| match self.registry.get(&d.name).map(|t| &t.imp) {
                Some(crate::registry::Impl::Mcp { server, tool })
                    if servers.contains(server) && !self.is_company_server(server) =>
                {
                    Some((d.name.clone(), (server.clone(), tool.clone())))
                }
                Some(crate::registry::Impl::Mapped(m))
                    if servers.contains(&m.server)
                        && m.args.is_none()
                        && m.result.is_none()
                        && profiles
                            .get(d.name.split('.').next().unwrap_or(""))
                            .is_some_and(|p| *p == m.server) =>
                {
                    Some((d.name.clone(), (m.server.clone(), m.tool.clone())))
                }
                _ => None,
            })
            .collect();
        // A declared browser's tools come back to the supervisor too, where
        // the browser guard keeps them off every Company's hosts (RFC 0047
        // §5.5.9); the child still lists them from its own connection.
        for (name, (server, _)) in &mcp_routes {
            if self.is_browser_server(server) && !gated_tools.contains(name) {
                gated_tools.push(name.clone());
            }
        }
        let mut payload = SpawnPayload {
            instruction: instruction.clone(),
            output_contract,
            context_seed,
            gated_tools,
            supervisor_tools: company_tools,
            mcp_routes: Some(mcp_routes),
            // An explicit `model:` (from the call, the template or the spawn
            // defaults) overrides the parent's turn model; otherwise the child
            // runs on it. Resolved like every other model reference — its
            // provider, its wire name, its fallbacks — so a caller naming a
            // model does not leak that name to a provider as if it were one.
            intelligence: self.intel_config(args.get("model").and_then(Value::as_str)),
            mcp_servers: servers.iter().filter_map(|s| self.spawn_spec(s)).collect(),
            a2a_peers: Vec::new(),
            tls_ca: self.settings.security.tls_ca.clone(),
            aauth: None,
            limits: Limits {
                max_steps: steps,
                max_tokens: tokens,
                deadline_ms: deadline_ms.max(1000),
                max_depth: max_depth.saturating_sub(depth + 1),
                memory_bytes,
                cpu_seconds,
                nice: priority.nice(),
            },
            telemetry: Telemetry {
                run_id: self.run_id.clone(),
                instance: self.instance.clone(),
                agent_id: handle.clone(),
                agent_path: format!("sub/{handle}"),
                traceparent: None,
                tracestate: None,
                log_level: self
                    .settings
                    .observability
                    .log_level
                    .clone()
                    .unwrap_or_else(|| "info".into()),
                log_content: self.settings.observability.log_content,
                otel_endpoint: self.settings.observability.otel.endpoint.clone(),
                attribution: Default::default(),
            },
            depth: depth + 1,
            warm: mode == "warm",
            role: Role::Agent,
            turn: None,
        };
        // The `tools:` narrowing is a GRANT, not a note: minted into the payload
        // here and ENFORCED by the child, which filters both its catalogue and
        // its dispatch against it (`agentloop::runner::Session::prepare`). Without
        // the mint the argument would be recorded and ignored, and a parent
        // bounding an untrusted sub-task would silently get a child holding
        // everything, when a child's authority must only ever narrow.
        if let Some(a) = &allow {
            payload.narrow_tools(a);
        }
        // The child is a span of the unit that spawned it — the asking
        // worker's, else the run's — and so is everything it does.
        let trace = self
            .caller_trace(caller)
            .unwrap_or_else(|| self.trace.clone());
        payload.telemetry.set_trace(&trace);
        // A durable record BEFORE the spawn (restore re-spawns pending ones).
        let mut record = SubagentRecord {
            handle: handle.clone(),
            instruction: instruction.clone(),
            mode: mode.clone(),
            status: "spawned".into(),
            attempt: 1,
            result: None,
            error: None,
            requested_by: Some(
                json!({"caller": caller.node.map(|n| n.0), "ctx": caller.ctx, "run": caller.run, "step": caller.step, "subagent": caller.subagent, "depth": depth}),
            ),
            principal: caller.principal.clone(),
            // What the caller carries, the child is handed (`withhold`).
            carries: self.caller_carries(caller),
            tokens: 0,
            created: now_ms(),
            updated: now_ms(),
            payload: Some(secret_free_payload(&payload)),
            trace: Some(trace.clone()),
            template: tmeta.as_ref().map(|(n, _)| n.clone()),
            tier: tmeta.as_ref().map(|(_, t)| t.clone()),
            pid: None,
            config_path: None,
            socket: None,
            retire_at: None,
            retiring_since: None,
            durable: args
                .get("durable")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| self.work_durable_default()),
            node: None,
            killed: false,
            dirty: true,
        };
        record.principal = self.spawn_principal(&record);
        // It signs as the agent, holding the Case C tokens of whom it works
        // for alone.
        payload.aauth = aauth_handoff(record.principal.as_deref());
        // Its direct MCP calls say whom they are for, as the supervisor's do.
        let mut attribution = json!({});
        self.attribute(&mut attribution, record.principal.as_deref());
        crate::mcp::scope(&mut attribution, record.principal.as_deref());
        if let Value::Object(m) = attribution {
            payload.telemetry.attribution = m;
            record.payload = Some(secret_free_payload(&payload));
        }
        match self.children.spawn(
            &payload,
            ChildKind::Subagent {
                handle: handle.clone(),
            },
            Some(trace.clone()),
            Duration::from_millis(deadline_ms),
        ) {
            Ok(node) => {
                record.node = Some(node);
                record.status = "running".into();
                self.log.in_trace(Some(&trace)).info("subagent.spawn", json!({"handle": handle, "mode": mode, "node": node.0, "pid": self.children.pid_of(node), "depth": depth + 1, "servers": servers.len(), "priority": priority.as_str(), "memory_bytes": memory_bytes, "cpu_seconds": cpu_seconds}));
                let persist = record.durable;
                self.subagents.insert(handle.clone(), record);
                if persist {
                    let _ = self.durable.put(
                        crate::state::Kind::Subagent,
                        &handle,
                        serde_json::to_value(self.subagents.get(&handle).unwrap())
                            .unwrap_or(Value::Null),
                        None,
                    );
                }
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.dirty = false;
                }
                match mode.as_str() {
                    "sync" => ToolOutcome::Deferred(PendingKind::Subagent { handle }),
                    _ => ToolOutcome::Ready(json!({"handle": handle, "status": "running"}), false),
                }
            }
            Err(e) => {
                record.status = "failed".into();
                record.error = Some(format!("spawn: {e}"));
                self.subagents.insert(handle.clone(), record);
                err(format!("subagent.run: spawn failed: {e}"))
            }
        }
    }

    /// Resolve `template:`/`params:` into effective spawn args.
    /// Returns the args plus `(template, tier)` when a template was named.
    /// Freeform spawns pass through with defaults applied — unless
    /// `subagents.allow_freeform: false` refuses them.
    fn resolve_spawn_args(
        &self,
        args: &Value,
    ) -> Result<(Value, Option<(String, String)>), String> {
        use crate::config::templates as tpl;
        let defaults = self.settings.subagents.defaults.clone();
        let mut eff = args.clone();
        let Some(o) = eff.as_object_mut() else {
            return Err("subagent.run: args must be an object".into());
        };
        let tname = o
            .get("template")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(tname) = tname else {
            if self.settings.subagents.allow_freeform == Some(false) {
                return Err(
                    "subagent.run refused: freeform spawns are disabled (subagents.allow_freeform: false) — instantiate a declared template".into(),
                );
            }
            apply_spawn_defaults(o, &defaults);
            return Ok((eff, None));
        };
        if o.get("instruction")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return Err("subagent.run: `template` and `instruction` are mutually exclusive".into());
        }
        for k in ["tools", "servers"] {
            if o.contains_key(k) {
                return Err(format!(
                    "subagent.run: `{k}` may not be passed with `template` — the template defines the grant"
                ));
            }
        }
        let compiled = tpl::compile_templates(&self.settings)
            .map_err(|es| format!("subagent.run: template compile: {}", es.join("; ")))?;
        let Some(t) = compiled.get(&tname) else {
            return Err(format!(
                "subagent.run: no template '{tname}' (subagents.templates declares: {:?})",
                compiled.keys().collect::<Vec<_>>()
            ));
        };
        let params = tpl::validate_params(&t.spec.params, o.get("params").unwrap_or(&Value::Null))
            .map_err(|e| format!("subagent.run: template '{tname}': {e}"))?;
        let folded = tpl::fold_params(&t.cleaned, &params);
        if tpl::params_introduced_machinery(&t.cleaned, &folded) {
            return Err(format!(
                "subagent.run refused: params for template '{tname}' introduced directive machinery"
            ));
        }
        if t.tier == tpl::Tier::Instance {
            // `instance_run` recompiles and refolds; hand it the validated
            // params so refusals there are consistent with here.
            o.insert("params".into(), Value::Object(params));
            return Ok((eff, Some((tname, "instance".into()))));
        }
        // Flat: the template's fields merge under the call site's.
        let spec = &t.spec;
        o.insert("instruction".into(), json!(folded));
        if let Some(v) = &spec.tools {
            o.insert("tools".into(), json!(v));
        }
        if let Some(v) = &spec.servers {
            o.insert("servers".into(), json!(v));
        }
        for (key, val) in [
            ("limits", spec.limits.clone()),
            ("skills", spec.skills.clone()),
            ("output_schema", spec.output_schema.clone()),
        ] {
            if !o.contains_key(key)
                && let Some(v) = val
            {
                o.insert(key.into(), v);
            }
        }
        for (key, val) in [
            ("mode", spec.mode.clone()),
            ("priority", spec.priority.clone()),
            ("model", spec.model.clone()),
            ("output_contract", spec.output_contract.clone()),
        ] {
            if !o.contains_key(key)
                && let Some(v) = val
            {
                o.insert(key.into(), json!(v));
            }
        }
        if !o.contains_key("durable")
            && let Some(v) = spec.durable
        {
            o.insert("durable".into(), json!(v));
        }
        // Template context seeds first, the call site's appended after.
        if let Some(tc) = spec.context.as_ref().and_then(Value::as_array) {
            let mut merged = tc.clone();
            if let Some(cc) = o.get("context").and_then(Value::as_array) {
                merged.extend(cc.clone());
            }
            o.insert("context".into(), json!(merged));
        }
        apply_spawn_defaults(o, &defaults);
        Ok((eff, Some((tname, "flat".into()))))
    }

    fn spawn_bucket_take(&mut self) -> bool {
        // A process-lifetime bucket parsed from `limits.subagents.rate` ("8/2s").
        static BUCKET: std::sync::Mutex<Option<TokenBucket>> = std::sync::Mutex::new(None);
        let mut g = BUCKET.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let (burst, per_sec) = parse_rate(
                self.settings
                    .limits
                    .subagents
                    .rate
                    .as_deref()
                    .unwrap_or("8/2s"),
            );
            *g = Some(TokenBucket::new(burst, per_sec));
        }
        g.as_mut().map(|b| b.try_take()).unwrap_or(true)
    }

    /// A warm subagent finished a turn (non-terminal). Its result is noted to
    /// the root only when the wake policy asks for subagent results, as a
    /// finished child's is, and withheld there as any read-back is
    /// (`withhold`).
    pub(crate) fn on_subagent_turn(&mut self, node: NodeId, outcome: Outcome) {
        let Some(ChildKind::Subagent { handle }) = self.children.get(node).map(|c| c.kind.clone())
        else {
            return;
        };
        // A turn a killed child finishes in its grace is not taken: the kill
        // closed the record.
        if self.subagents.get(&handle).is_some_and(|s| s.killed) {
            return;
        }
        if let Some(s) = self.subagents.get_mut(&handle) {
            s.result = Some(distill(&outcome.result));
            s.updated = now_ms();
            s.dirty = true;
        }
        self.subagent_log(&handle).info(
            "subagent.turn",
            json!({"handle": handle, "status": outcome.status.as_str()}),
        );
        if self
            .settings
            .agent
            .wake_on()
            .contains(&crate::config::settings::WakeEvent::SubagentResult)
        {
            let text = self.withhold_text(
                &super::withhold::Reader::Conversation(Some(crate::context::ROOT.to_string())),
                super::withhold::Source::Child(&handle),
                distill_text(&outcome.result),
                "agent.wake_on",
            );
            self.note_root(format!("subagent {handle} finished a turn: {text}"));
        }
    }

    /// A subagent finished (result or failure).
    pub(crate) fn on_subagent_result(&mut self, node: NodeId, outcome: Result<Outcome, String>) {
        // The child may already be gone: a child that died without a result
        // reaches here from the reap path AFTER `Children::on_reaped` removed
        // it, and returning then left its record `running` for ever — no
        // result, no answer to a waiter. The reaped record says which
        // subagent it was.
        let Some(ChildKind::Subagent { handle }) = self
            .children
            .get(node)
            .map(|c| c.kind.clone())
            .or_else(|| self.children.reaped_kind(node))
        else {
            return;
        };
        let tokens = self
            .children
            .get(node)
            .map(|c| c.tokens)
            .or_else(|| self.subagents.get(&handle).map(|s| s.tokens))
            .unwrap_or(0);
        let (status, result, error) = match (&outcome, self.subagents.get(&handle)) {
            // A child `subagent.kill` stopped ends as the kill recorded it: a
            // result it reports in its grace, or its reap by the ladder's
            // signal, does not complete, fail or crash it.
            (_, Some(s)) if s.killed => {
                ("cancelled".to_string(), s.result.clone(), s.error.clone())
            }
            (Ok(o), _) => (
                o.status.as_str().to_string(),
                Some(distill(&o.result)),
                None,
            ),
            (Err(e), _) => ("failed".to_string(), None, Some(e.clone())),
        };
        if let Some(s) = self.subagents.get_mut(&handle) {
            s.status = status.clone();
            s.result = result.clone();
            s.error = error.clone();
            s.tokens = tokens;
            s.node = None;
            s.updated = now_ms();
            s.dirty = true;
        }
        self.subagent_log(&handle).info(
            "subagent.result",
            json!({"handle": handle, "status": status, "tokens": tokens, "err": error}),
        );
        // Answer waiters.
        let waiting: Vec<(super::reactor::Target, String)> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Subagent { handle: h } if *h == handle))
            .map(|p| (p.target.clone(), p.name.clone()))
            .collect();
        self.pending
            .retain(|p| !matches!(&p.kind, PendingKind::Subagent { handle: h } if *h == handle));
        for (t, name) in waiting {
            self.reply_read_back(
                &t,
                json!({"handle": handle, "status": status, "result": result, "error": error}),
                &name,
            );
        }
        // Settle whatever plan item this subagent was bound to, then note the
        // outcome to the root only when the wake policy asks for subagent
        // results — an unwanted note would wake a root that has nothing to do.
        let ok = status == "completed";
        let note = result
            .as_ref()
            .map(distill_text)
            .or(error.clone())
            .unwrap_or_default();
        let source = super::withhold::Source::Child(&handle);
        self.settle_plan_bindings(&plan_binding_subagent(&handle), source, ok, &note);
        if self
            .settings
            .agent
            .wake_on()
            .contains(&crate::config::settings::WakeEvent::SubagentResult)
        {
            let note = self.withhold_text(
                &super::withhold::Reader::Conversation(Some(crate::context::ROOT.to_string())),
                source,
                note,
                "agent.wake_on",
            );
            self.note_root(format!("subagent {handle} {status}: {note}"));
        }
    }

    /// Append a note to the root context (durable; the next root turn sees it).
    pub(crate) fn note_root(&mut self, text: String) {
        let window = self.model_window();
        let c = self.contexts.root();
        c.model_window = window;
        c.append(Msg::note(text));
    }

    /// Auto-advance plan items bound to a finished run/subagent (every context).
    /// The note is withheld as any read-back is (`withhold`): a plan lives in
    /// a conversation, whose model reads it every turn after.
    pub(crate) fn settle_plan_bindings(
        &mut self,
        binding: &crate::context::plan::Binding,
        source: super::withhold::Source,
        ok: bool,
        note: &str,
    ) {
        let reader = super::withhold::Reader::Conversation(None);
        let withheld = self.withheld(&reader, source).filter(|_| !note.is_empty());
        let note = withheld.as_deref().unwrap_or(note);
        // A note handed whole hands the conversation what it keeps of the
        // source's record: the Companies, the delegated data and the taint.
        let kept = if withheld.is_none() {
            self.kept_of(source)
        } else {
            Default::default()
        };
        let mut settled = false;
        for id in self.contexts.ids() {
            if let Some(c) = self.contexts.get_mut(&id)
                && let Some(p) = c.plan.as_mut()
            {
                let advanced = p.settle_binding(binding, ok, Some(note));
                if !advanced.is_empty() {
                    settled = true;
                    c.carries.union(&kept);
                    c.touch();
                    self.log.info(
                        "plan.updated",
                        json!({"ctx": id, "op": "auto", "items": advanced}),
                    );
                }
            }
        }
        if settled && let Some(marker) = &withheld {
            self.log_withheld(&reader, source, marker, "plan.update");
        }
    }

    /// A reload moved the intelligence: hand each running subagent its model
    /// resolved again — the connection it dials, the credential, the
    /// fallbacks — to apply at its next turn boundary under
    /// `intelligence.swap_policy`. A warm subagent otherwise dialed the
    /// connection it was spawned with for as long as it lived.
    pub(crate) fn swap_subagent_intelligence(&mut self) {
        let policy = self
            .settings
            .intelligence
            .swap_policy
            .as_deref()
            .and_then(crate::config::SwapPolicy::parse)
            .unwrap_or_default();
        let live: Vec<(NodeId, Option<String>)> = self
            .subagents
            .values()
            .filter(|s| !is_terminal_status(&s.status))
            .filter_map(|s| {
                let reference = s
                    .payload
                    .as_ref()
                    .and_then(|p| p["intelligence"]["reference"].as_str())
                    .map(str::to_string);
                Some((s.node?, reference))
            })
            .collect();
        for (node, reference) in &live {
            let swap = crate::subagent::protocol::SwapIntel {
                intelligence: self.intel_config(reference.as_deref()),
                policy,
            };
            self.children
                .send(*node, &ControlMsg::SwapIntel(Box::new(swap)));
        }
        if !live.is_empty() {
            self.log.info(
                "intel.swap.sent",
                json!({"subagents": live.len(), "policy": policy.as_str()}),
            );
        }
    }

    /// Restore: re-spawn non-detached, non-terminal subagents (`attempt + 1`),
    /// and every one a drain parked — a detached one too, which a crash does
    /// not revive but a drain's own stopping must not lose.
    pub(crate) fn respawn_restored_subagents(&mut self) {
        let handles: Vec<String> = self
            .subagents
            .values()
            .filter(|s| {
                !is_terminal_status(&s.status) && (s.mode != "detached" || s.status == "parked")
            })
            .map(|s| s.handle.clone())
            .collect();
        for handle in handles {
            let Some(payload_v) = self.subagents.get(&handle).and_then(|s| s.payload.clone())
            else {
                continue;
            };
            let Ok(mut payload) = serde_json::from_value::<SpawnPayload>(payload_v) else {
                self.subagent_log(&handle)
                    .warn("subagent.restore.bad_payload", json!({"handle": handle}));
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "failed".into();
                    s.error = Some("payload not restorable".into());
                    s.dirty = true;
                }
                continue;
            };
            // The model the child was spawned on, resolved again against the
            // live settings: its connection and credential may have moved
            // since, and a stored one is never replayed.
            payload.intelligence = self.intel_config(payload.intelligence.reference.as_deref());
            // And it signs as this life's agent: the record kept no identity.
            payload.aauth = aauth_handoff(
                self.subagents
                    .get(&handle)
                    .and_then(|s| s.principal.clone())
                    .as_deref(),
            );
            // It carries on in the trace it was spawned in, as the
            // supervisor's lines about it do.
            let trace = self.subagents.get(&handle).and_then(|s| s.trace.clone());
            if let Some(t) = &trace {
                payload.telemetry.set_trace(t);
            }
            let deadline = Duration::from_millis(payload.limits.deadline_ms.max(1000));
            match self.children.spawn(
                &payload,
                ChildKind::Subagent {
                    handle: handle.clone(),
                },
                trace,
                deadline,
            ) {
                Ok(node) => {
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.node = Some(node);
                        s.attempt += 1;
                        s.status = "running".into();
                        s.dirty = true;
                    }
                    crate::obs::metrics::record_subagent_restart("restored");
                    self.subagent_log(&handle).info(
                        "subagent.respawn",
                        json!({"handle": handle, "node": node.0}),
                    );
                }
                Err(e) => {
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.status = "failed".into();
                        s.error = Some(format!("respawn: {e}"));
                        s.dirty = true;
                    }
                }
            }
        }
    }
}

fn plan_binding_subagent(handle: &str) -> crate::context::plan::Binding {
    crate::context::plan::Binding::Subagent {
        handle: handle.to_string(),
    }
}

/// `"<burst>/<per>s"` → (burst, per_sec).
pub fn parse_rate(s: &str) -> (u32, f64) {
    let (b, p) = s.split_once('/').unwrap_or(("8", "2s"));
    let burst = b.trim().parse::<u32>().unwrap_or(8).max(1);
    let per = crate::config::parse_duration(p.trim())
        .map(|d| d.as_secs_f64())
        .unwrap_or(2.0)
        .max(0.001);
    (burst, burst as f64 / per)
}

/// Apply `subagents.defaults` to a spawn: each field lands only when neither
/// the call site nor the template set it, so a default never overrides an
/// explicit choice.
fn apply_spawn_defaults(
    o: &mut serde_json::Map<String, Value>,
    d: &crate::config::settings::SubagentDefaults,
) {
    for (key, val) in [
        ("mode", d.mode.clone()),
        ("model", d.model.clone()),
        ("priority", d.priority.clone()),
    ] {
        if !o.contains_key(key)
            && let Some(v) = val
        {
            o.insert(key.into(), json!(v));
        }
    }
    if !o.contains_key("limits")
        && let Some(l) = &d.limits
    {
        o.insert("limits".into(), l.clone());
    }
    if !o.contains_key("durable")
        && let Some(v) = d.durable
    {
        o.insert("durable".into(), json!(v));
    }
}

/// The AAuth identity this process signs with, as a turn worker or a
/// subagent is handed it; `None` with no identity configured, or in a build
/// without `aauth`.
pub(crate) fn aauth_handoff(
    principal: Option<&str>,
) -> Option<crate::subagent::protocol::AAuthHandoff> {
    #[cfg(feature = "aauth")]
    {
        crate::aauth::handoff(principal)
    }
    #[cfg(not(feature = "aauth"))]
    {
        let _ = principal;
        None
    }
}

/// The payload as stored: of the intelligence, only the model REFERENCE —
/// a restore resolves it again ([`Runtime::intel_config`]). The connection
/// is not stored: its token and its headers, resolved, are credentials. Nor
/// is the AAuth identity, whose agent token is one: a restore hands the
/// child the live one ([`aauth_handoff`]).
fn secret_free_payload(p: &SpawnPayload) -> Value {
    let mut clean = p.clone();
    clean.intelligence = IntelConfig {
        model: p.intelligence.model.clone(),
        reference: p.intelligence.reference.clone(),
        ..Default::default()
    };
    clean.aauth = None;
    let mut v = serde_json::to_value(&clean).unwrap_or(Value::Null);
    // The record keeps surfacing the narrowed grant for audits/`subagent.status`
    // — read back OFF THE PAYLOAD that actually carries it, so the record can
    // never claim a confinement the child was not given. The payload itself
    // carries it into a restore-time respawn.
    if let Some(a) = p.allowed_tools() {
        v["allowed_tools"] = json!(a);
    }
    v
}

fn distill(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > DISTILL_CAP => Value::String(format!(
            "{}… [truncated]",
            &s[..{
                let mut cut = DISTILL_CAP;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                cut
            }]
        )),
        Value::String(s) => {
            serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        other => other.clone(),
    }
}

fn distill_text(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > 400 {
        format!("{}…", s.chars().take(400).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_distillation() {
        assert_eq!(parse_rate("8/2s"), (8, 4.0));
        assert_eq!(parse_rate("1/1s"), (1, 1.0));
        assert_eq!(parse_rate("garbage").0, 8);
        assert_eq!(distill(&json!("{\"a\":1}")), json!({"a": 1}));
        assert!(
            distill(&Value::String("x".repeat(9000)))
                .as_str()
                .unwrap()
                .ends_with("[truncated]")
        );
        assert!(distill_text(&json!({"k": "v"})).contains("\"k\""));
    }

    /// The stored payload keeps no AAuth identity: its agent token is a
    /// credential, and a restore hands the child the live one.
    #[test]
    fn the_stored_payload_keeps_no_identity() {
        let payload: SpawnPayload = serde_json::from_value(json!({
            "instruction": "work",
            "intelligence": {"uri": "https://intel.example", "token": "sk-1", "model": "m"},
            "aauth": {"provider": "https://apd.example", "key": {"seed": "c2VlZA"},
                      "thumbprint": "t", "enrollment_token": "enroll-1",
                      "token": {"token": "agent-tok", "good_until_ms": 1}},
            "limits": {"max_steps": 1, "max_tokens": 1, "deadline_ms": 1000, "max_depth": 1},
            "telemetry": {"run_id": "r", "instance": "i", "agent_id": "a", "agent_path": "a",
                          "log_level": "info"},
            "depth": 1
        }))
        .unwrap();
        assert!(payload.aauth.is_some());
        let stored = secret_free_payload(&payload).to_string();
        for secret in ["sk-1", "c2VlZA", "enroll-1", "agent-tok"] {
            assert!(!stored.contains(secret), "{secret} stored: {stored}");
        }
        let back: SpawnPayload = serde_json::from_str(&stored).unwrap();
        assert!(back.aauth.is_none());
    }
}
