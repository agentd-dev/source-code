// SPDX-License-Identifier: AGPL-3.0-only
//! The **subagent registry** (RFC 0026 §6, RFC 0028 §3 `subagent.*`): flat
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
        match name {
            "subagent.run" => self.subagent_run(caller, &args),
            "subagent.send" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                let message = args["message"].as_str().unwrap_or("").to_string();
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
                    ToolOutcome::Ready(json!({"ok": true, "handle": handle}), false)
                } else {
                    err(format!("subagent {handle:?}: send failed"))
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
                self.children.cancel(node, &reason);
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "cancelled".into();
                    s.error = Some(reason);
                    s.updated = now_ms();
                    s.dirty = true;
                }
                self.log.info("subagent.kill", json!({"handle": handle}));
                ToolOutcome::Ready(json!({"ok": true, "handle": handle}), false)
            }
            "subagent.status" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                match self.subagents.get(&handle) {
                    Some(s) => ToolOutcome::Ready(
                        json!({"handle": handle, "status": s.status, "mode": s.mode, "result": s.result, "error": s.error, "tokens": s.tokens}),
                        false,
                    ),
                    None => err(format!("no such subagent {handle:?}")),
                }
            }
            "subagent.await" => {
                let handle = args["handle"].as_str().unwrap_or("").to_string();
                match self.subagents.get(&handle) {
                    None => err(format!("no such subagent {handle:?}")),
                    Some(s) if is_terminal_status(&s.status) => ToolOutcome::Ready(
                        json!({"handle": handle, "status": s.status, "result": s.result, "error": s.error}),
                        false,
                    ),
                    Some(_) => ToolOutcome::Deferred(PendingKind::Subagent { handle }),
                }
            }
            "subagent.list" => ToolOutcome::Ready(
                json!({"subagents": self.subagents.values().map(|s| json!({"handle": s.handle, "mode": s.mode, "status": s.status, "instruction": s.instruction.chars().take(80).collect::<String>(), "created": s.created})).collect::<Vec<_>>()}),
                false,
            ),
            _ => err(format!("unknown subagent tool {name}")),
        }
    }

    /// `subagent.run`: caps → durable record → spawn (RFC 0026 §6).
    fn subagent_run(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
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
        // Caps (RFC 0026 §2): breadth (live), total (lifetime), rate, depth
        // (a subagent asking for a subagent goes through the same chokepoint;
        // logical depth = the requester's depth + 1).
        let live = self
            .subagents
            .values()
            .filter(|s| !is_terminal_status(&s.status))
            .count() as u32;
        let breadth = self.settings.limits.subagents.breadth.unwrap_or(8);
        if live >= breadth {
            return err(format!(
                "subagent.run refused: {live} subagents live (limits.subagents.breadth = {breadth})"
            ));
        }
        let total = self.settings.limits.subagents.total.unwrap_or(64) as usize;
        if self.subagents.len() >= total {
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
            return err(format!(
                "subagent.run refused: delegation depth {depth} reaches limits.subagents.depth = {max_depth}"
            ));
        }
        if !self.spawn_bucket_take() {
            return err("subagent.run refused: spawn rate exceeded (limits.subagents.rate)".into());
        }
        if crate::supervisor::cgroup::under_memory_pressure() {
            return err("subagent.run refused: memory pressure".into());
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
        let mut payload = SpawnPayload {
            instruction: instruction.clone(),
            output_contract,
            context_seed,
            intelligence: IntelConfig {
                uri: self.intel_uri.clone(),
                token: self.current_intel_bearer(),
                model: Some(self.model.clone()),
                headers: self.intel_headers.clone(),
                aws_auth: self.intel_aws_auth(),
                dialect: self.intel_dialect(),
            },
            mcp_servers: servers
                .iter()
                .filter_map(|s| self.mcp_specs.get(s).cloned())
                .collect(),
            a2a_peers: Vec::new(),
            tls_ca: self.settings.security.tls_ca.clone(),
            aauth: None,
            limits: Limits {
                max_steps: steps,
                max_tokens: tokens,
                deadline_ms: deadline_ms.max(1000),
                max_depth: max_depth.saturating_sub(depth + 1),
            },
            telemetry: Telemetry {
                run_id: self.run_id.clone(),
                agent_id: handle.clone(),
                agent_path: format!("sub/{handle}"),
                trace_id: self.trace_id.clone(),
                log_level: self
                    .settings
                    .observability
                    .log_level
                    .clone()
                    .unwrap_or_else(|| "info".into()),
                log_content: self.settings.observability.log_content,
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
        // everything — the opposite of RFC 0009's monotonically narrowing scope.
        if let Some(a) = &allow {
            payload.narrow_tools(a);
        }
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
            tokens: 0,
            created: now_ms(),
            updated: now_ms(),
            payload: Some(secret_free_payload(&payload)),
            node: None,
            dirty: true,
        };
        match self.children.spawn(
            &payload,
            ChildKind::Subagent {
                handle: handle.clone(),
            },
            Duration::from_millis(deadline_ms),
        ) {
            Ok(node) => {
                record.node = Some(node);
                record.status = "running".into();
                self.log.info("subagent.spawn", json!({"handle": handle, "mode": mode, "node": node.0, "depth": depth + 1, "servers": servers.len()}));
                self.subagents.insert(handle.clone(), record);
                let _ = self.durable.put(
                    crate::state::Kind::Subagent,
                    &handle,
                    serde_json::to_value(self.subagents.get(&handle).unwrap())
                        .unwrap_or(Value::Null),
                    None,
                );
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

    /// A warm subagent finished a turn (non-terminal).
    pub(crate) fn on_subagent_turn(&mut self, node: NodeId, outcome: Outcome) {
        let Some(ChildKind::Subagent { handle }) = self.children.get(node).map(|c| c.kind.clone())
        else {
            return;
        };
        if let Some(s) = self.subagents.get_mut(&handle) {
            s.result = Some(distill(&outcome.result));
            s.updated = now_ms();
            s.dirty = true;
        }
        self.log.info(
            "subagent.turn",
            json!({"handle": handle, "status": outcome.status.as_str()}),
        );
        // Notify the root context (wake policy: subagent_result).
        self.note_root(format!(
            "subagent {handle} finished a turn: {}",
            distill_text(&outcome.result)
        ));
    }

    /// A subagent finished (result or failure).
    pub(crate) fn on_subagent_result(&mut self, node: NodeId, outcome: Result<Outcome, String>) {
        let Some(ChildKind::Subagent { handle }) = self.children.get(node).map(|c| c.kind.clone())
        else {
            return;
        };
        let tokens = self.children.get(node).map(|c| c.tokens).unwrap_or(0);
        let (status, result, error) = match &outcome {
            Ok(o) => (
                o.status.as_str().to_string(),
                Some(distill(&o.result)),
                None,
            ),
            Err(e) => ("failed".to_string(), None, Some(e.clone())),
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
        self.log.info(
            "subagent.result",
            json!({"handle": handle, "status": status, "tokens": tokens, "err": error}),
        );
        // Answer waiters.
        let waiting: Vec<super::reactor::Target> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Subagent { handle: h } if *h == handle))
            .map(|p| p.target.clone())
            .collect();
        self.pending
            .retain(|p| !matches!(&p.kind, PendingKind::Subagent { handle: h } if *h == handle));
        for t in waiting {
            self.reply(
                &t,
                json!({"handle": handle, "status": status, "result": result, "error": error}),
                false,
            );
        }
        // Plan bindings + the root note (RFC 0026 §5.3, §3.1 wake policy).
        let ok = status == "completed";
        let note = result
            .as_ref()
            .map(distill_text)
            .or(error.clone())
            .unwrap_or_default();
        self.settle_plan_bindings(&plan_binding_subagent(&handle), ok, &note);
        if self
            .settings
            .agent
            .wake_on()
            .contains(&crate::config::v2::WakeEvent::SubagentResult)
        {
            self.note_root(format!("subagent {handle} {status}: {note}"));
        }
    }

    /// Append a note to the root context (durable; the next root turn sees it).
    pub(crate) fn note_root(&mut self, text: String) {
        let window = self.model_window();
        let c = self.contexts.root();
        if c.model_window == 0 {
            c.model_window = window;
        }
        c.append(Msg::note(text));
    }

    /// Auto-advance plan items bound to a finished run/subagent (every context).
    pub(crate) fn settle_plan_bindings(
        &mut self,
        binding: &crate::context::plan::Binding,
        ok: bool,
        note: &str,
    ) {
        for id in self.contexts.ids() {
            if let Some(c) = self.contexts.get_mut(&id)
                && let Some(p) = c.plan.as_mut()
            {
                let advanced = p.settle_binding(binding, ok, Some(note));
                if !advanced.is_empty() {
                    c.touch();
                    self.log.info(
                        "plan.updated",
                        json!({"ctx": id, "op": "auto", "items": advanced}),
                    );
                }
            }
        }
    }

    /// Restore: re-spawn non-detached, non-terminal subagents (`attempt + 1`).
    pub(crate) fn respawn_restored_subagents(&mut self) {
        let handles: Vec<String> = self
            .subagents
            .values()
            .filter(|s| !is_terminal_status(&s.status) && s.mode != "detached")
            .map(|s| s.handle.clone())
            .collect();
        for handle in handles {
            let Some(payload_v) = self.subagents.get(&handle).and_then(|s| s.payload.clone())
            else {
                continue;
            };
            let Ok(mut payload) = serde_json::from_value::<SpawnPayload>(payload_v) else {
                self.log
                    .warn("subagent.restore.bad_payload", json!({"handle": handle}));
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "failed".into();
                    s.error = Some("payload not restorable".into());
                    s.dirty = true;
                }
                continue;
            };
            payload.intelligence = IntelConfig {
                uri: self.intel_uri.clone(),
                token: self.current_intel_bearer(),
                model: Some(self.model.clone()),
                headers: self.intel_headers.clone(),
                aws_auth: self.intel_aws_auth(),
                dialect: self.intel_dialect(),
            };
            let deadline = Duration::from_millis(payload.limits.deadline_ms.max(1000));
            match self.children.spawn(
                &payload,
                ChildKind::Subagent {
                    handle: handle.clone(),
                },
                deadline,
            ) {
                Ok(node) => {
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.node = Some(node);
                        s.attempt += 1;
                        s.status = "running".into();
                        s.dirty = true;
                    }
                    self.log.info(
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

/// The payload as stored (no credential: the intelligence token is re-supplied
/// from the live settings on restore).
fn secret_free_payload(p: &SpawnPayload) -> Value {
    let mut clean = p.clone();
    clean.intelligence.token = None;
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
}
