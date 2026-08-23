// SPDX-License-Identifier: AGPL-3.0-only
//! **Hot reload** of the v2 configuration (RFC 0030 §6, RFC 0017 semantics):
//! SIGHUP or `lifecycle.watch_config` re-merges the files and re-validates; the
//! restart-only paths must be unchanged (else `restart_required`, the running
//! configuration stays); the reloadable partition applies at the loop's
//! quiesce boundary — the flat tree makes most of it trivial: every turn worker
//! is spawned fresh from the live settings, so a new intelligence endpoint,
//! model, instruction, budget, tool override or workflow definition takes
//! effect for the next unit of work. Live runs keep the definition they
//! started with (pinned by hash).

use super::reactor::Runtime;
use crate::config::v2 as cfg;
use crate::governor::Governor;
use crate::registry::{Registry, ServerTools};
use crate::state::now_ms;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

impl Runtime {
    /// SIGHUP / watcher: reload, diff, apply (or refuse).
    pub(crate) fn on_reload_requested(&mut self) {
        let trigger = if crate::signals::take_reload_was_watch() {
            "watch"
        } else {
            "sighup"
        };
        crate::signals::set_reloading(true);
        let outcome = self.reload_inner();
        crate::signals::set_reloading(false);
        // Audit the reload — an operator/system reconfiguration (plan §3.11).
        let (label, atarget) = match &outcome {
            Ok(changed) => ("applied", json!({"trigger": trigger, "changed": changed})),
            Err(ReloadRefused::Invalid(errs)) => {
                ("invalid", json!({"trigger": trigger, "errors": errs}))
            }
            Err(ReloadRefused::RestartRequired(paths)) => (
                "restart_required",
                json!({"trigger": trigger, "paths": paths}),
            ),
        };
        self.audit(crate::runtime::audit::AuditEvent {
            action: "config.reload",
            target: atarget,
            outcome: label,
            principal: Some("operator"),
            role: Some("operator"),
            request_id: None,
        });
        match outcome {
            Ok(changed) => {
                self.log.info(
                    "config.reloaded",
                    json!({"trigger": trigger, "changed": changed}),
                );
                crate::obs::metrics::record_config_reload("applied");
                let mut generation = 0;
                self.durable.manifest_update(|m| {
                    generation = m.lifecycle["config_generation"].as_u64().unwrap_or(0) + 1;
                    m.lifecycle["config_generation"] = json!(generation);
                    m.lifecycle["config_reloaded_at"] = json!(now_ms());
                });
                crate::obs::metrics::set_config_generation(generation);
            }
            Err(ReloadRefused::Invalid(errs)) => {
                for e in &errs {
                    self.log.warn(
                        "config.reload.invalid",
                        json!({"trigger": trigger, "error": e}),
                    );
                }
                crate::obs::metrics::record_config_reload("invalid");
            }
            Err(ReloadRefused::RestartRequired(paths)) => {
                self.log.warn(
                    "config.reload.restart_required",
                    json!({"trigger": trigger, "paths": paths}),
                );
                crate::obs::metrics::record_config_reload("restart_required");
            }
        }
    }

    fn reload_inner(&mut self) -> Result<Vec<&'static str>, ReloadRefused> {
        let (loaded, _ask) = cfg::load(&self.args, &self.env)
            .map_err(|e| ReloadRefused::Invalid(vec![format!("{e:?}")]))?;
        let restart = cfg::restart_only_diff(&self.settings_doc, &loaded.doc);
        if !restart.is_empty() {
            return Err(ReloadRefused::RestartRequired(restart));
        }
        for w in &loaded.warnings {
            self.log.warn("config.warning", json!({"warning": w}));
        }
        let new = loaded.settings;
        let old = std::mem::replace(&mut self.settings, new.clone());
        self.settings_doc = loaded.doc;
        let mut changed = Vec::new();

        // Intelligence (RFC 0018: hot-swap — the next spawned worker uses it).
        if old.intelligence.endpoints != new.intelligence.endpoints
            || old.intelligence.model != new.intelligence.model
            || old.intelligence.token != new.intelligence.token
            || old.intelligence.token_file != new.intelligence.token_file
        {
            self.intel_uri = new.intelligence.endpoint_list().unwrap_or_default();
            self.model = new.intelligence.model.clone().unwrap_or_default();
            let env = self.env.clone();
            let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            match super::resolve_intel_token(&new, &envmap) {
                Ok(t) => self.intel_token = t,
                Err(e) => self.log.warn("config.reload.token", json!({"err": e})),
            }
            changed.push("intelligence");
        }
        // Budgets: new windows, counters carried over.
        if old.intelligence.budget != new.intelligence.budget {
            let counters = self.governor.to_value();
            let mut g = Governor::new(&new.intelligence.budget);
            g.restore(&counters, now_ms());
            self.governor = g;
            changed.push("intelligence.budget");
        }
        // Instruction (static text; a resource instruction re-subscribes).
        if old.agent.instruction != new.agent.instruction {
            match new.agent.instruction.clone() {
                Some(t) if cfg::looks_like_resource_uri(&t) => {
                    if let Err(e) = self.subscribe_instruction(&t) {
                        self.log
                            .warn("instruction.subscribe.fail", json!({"uri": t, "err": e}));
                    }
                }
                Some(t) => {
                    self.instruction = super::reactor::Instruction {
                        text: t,
                        source: "static",
                        uri: None,
                        server: None,
                        version: self.instruction.version + 1,
                    };
                }
                None => {
                    self.instruction = super::reactor::Instruction {
                        text: String::new(),
                        source: "static",
                        uri: None,
                        server: None,
                        version: self.instruction.version + 1,
                    };
                }
            }
            if new
                .agent
                .wake_on()
                .contains(&cfg::WakeEvent::InstructionUpdated)
            {
                self.note_root("instruction.updated: the configuration changed the instruction; re-read it with instruction.read".into());
            }
            changed.push("agent.instruction");
        }
        if old.agent.preflight != new.agent.preflight
            || old.agent.wake_on != new.agent.wake_on
            || old.agent.tools != new.agent.tools
            || old.agent.max_parallel_turns != new.agent.max_parallel_turns
            || old.agent.on_workflow_finished != new.agent.on_workflow_finished
            || old.agent.conversation_budget != new.agent.conversation_budget
        {
            changed.push("agent");
        }
        // MCP servers: connect added, drop removed (re-handshake).
        if old.mcp != new.mcp {
            let keep: Vec<String> = new.mcp.servers.iter().map(|s| s.name.clone()).collect();
            let removed: Vec<String> = self
                .mcp
                .keys()
                .filter(|k| !keep.contains(k))
                .cloned()
                .collect();
            for r in &removed {
                self.mcp.remove(r);
                self.mcp_specs.remove(r);
                self.skills.forget_server(r);
                self.log
                    .info("mcp.disconnect", json!({"server": r, "reason": "reload"}));
            }
            let timeout = new
                .mcp
                .default_timeout
                .map(|d| d.0)
                .unwrap_or(Duration::from_secs(60));
            for s in &new.mcp.servers {
                let spec = match s.to_spec() {
                    Ok(sp) => sp,
                    Err(e) => {
                        self.log
                            .warn("mcp.spec.invalid", json!({"server": s.name, "err": e}));
                        continue;
                    }
                };
                let same = self.mcp_specs.get(&s.name).is_some_and(|old| {
                    old.endpoint == spec.endpoint
                        && old.headers == spec.headers
                        && old.aauth == spec.aauth
                });
                if same && self.mcp.contains_key(&s.name) {
                    self.mcp_specs.insert(s.name.clone(), spec);
                    continue;
                }
                match crate::mcp::from_spec(&spec, s.timeout.map(|d| d.0).unwrap_or(timeout))
                    .and_then(|mut c| c.initialize().map(|()| c))
                {
                    Ok(mut c) => {
                        c.set_tool_meta(
                            json!({"agent/run_id": self.run_id, "agent/instance": self.instance}),
                        );
                        self.log
                            .info("mcp.connect", json!({"server": s.name, "reason": "reload"}));
                        self.mcp.insert(s.name.clone(), Arc::new(c));
                    }
                    Err(e) => self.log.warn(
                        "mcp.connect.fail",
                        json!({"server": s.name, "err": e.to_string()}),
                    ),
                }
                self.mcp_specs.insert(s.name.clone(), spec);
            }
            changed.push("mcp");
        }
        // Registry (overrides/disabled/tools) — always rebuilt when tools/mcp/knowledge/search changed.
        if old.tools != new.tools
            || old.mcp != new.mcp
            || old.knowledge != new.knowledge
            || old.search != new.search
        {
            let server_tools: Vec<ServerTools> = new
                .mcp
                .servers
                .iter()
                .filter_map(|s| {
                    let c = self.mcp.get(&s.name)?;
                    Some(ServerTools {
                        name: s.name.clone(),
                        ns: s.ns.clone(),
                        tags: self
                            .mcp_specs
                            .get(&s.name)
                            .map(|sp| sp.tags.clone())
                            .unwrap_or_default(),
                        tools: c.list_tools().unwrap_or_default(),
                    })
                })
                .collect();
            match Registry::build(&new, &server_tools) {
                Ok(r) => {
                    self.registry = r;
                    changed.push("tools");
                }
                Err(errs) => {
                    // Keep the old registry + old tools settings.
                    self.settings.tools = old.tools.clone();
                    return Err(ReloadRefused::Invalid(errs));
                }
            }
        }
        // Skills sources — the config section, or the instruction's inline
        // `:::skill` definitions (they live on `agent`, but they land in this
        // catalogue).
        if old.skills != new.skills || old.agent.inline_skills != new.agent.inline_skills {
            let mut cat = crate::context::skills::Catalogue::new(
                new.skills
                    .reference_prefix
                    .as_deref()
                    .unwrap_or(crate::context::skills::DEFAULT_PREFIX),
                new.skills.max_bytes.unwrap_or(32_768) as usize,
            );
            for src in &new.skills.sources {
                if let Some(c) = self.mcp.get(&src.server) {
                    let mode = match src.discover {
                        cfg::Discover::Prompts => crate::context::skills::Discover::Prompts,
                        cfg::Discover::Resources => crate::context::skills::Discover::Resources,
                        cfg::Discover::Auto => crate::context::skills::Discover::Auto,
                    };
                    cat.discover(&**c, mode, src.filter.as_deref());
                }
            }
            cat.add_inline(&new.agent.inline_skills);
            self.skills = cat;
            changed.push("skills");
        }
        // Workflows: reload definitions. Retirement (runtime::retire) gives
        // every old version the same exit — unsubscribe what nothing else
        // wants, pin for live runs, apply its own `unload:` policy — whether
        // it was removed outright or replaced by a new hash.
        if old.workflows != new.workflows {
            let previous = std::mem::take(&mut self.workflows);
            if let Err(errs) = self.load_workflows() {
                self.workflows = previous; // the running set stays authoritative
                return Err(ReloadRefused::Invalid(errs));
            }
            for (name, wf) in &previous {
                let survives = self
                    .workflows
                    .get(name)
                    .is_some_and(|new_wf| new_wf.hash == wf.hash);
                if survives {
                    continue;
                }
                let reason = if self.workflows.contains_key(name) {
                    "replaced"
                } else {
                    "removed"
                };
                self.retire_workflow(wf, reason);
            }
            self.arm_workflows();
            changed.push("workflows");
        }
        if old.limits != new.limits
            || old.lifecycle.idle_grace != new.lifecycle.idle_grace
            || old.observability.log_level != new.observability.log_level
            || old.observability.log_content != new.observability.log_content
            || old.memory != new.memory
            || old.context != new.context
        {
            changed.push("limits/lifecycle/observability/memory/context");
        }
        if changed.is_empty() {
            changed.push("nothing");
        }
        Ok(changed)
    }
}

/// Why a reload did not apply.
enum ReloadRefused {
    Invalid(Vec<String>),
    RestartRequired(Vec<String>),
}
