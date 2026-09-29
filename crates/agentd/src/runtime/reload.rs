// SPDX-License-Identifier: AGPL-3.0-only
//! **Hot reload** of the configuration: SIGHUP or `lifecycle.watch_config`
//! re-merges the files and re-validates. The reload is all-or-nothing — if any
//! restart-only path changed the whole reload is refused as
//! `restart_required` and the running configuration stays, so the daemon never
//! ends up half on one configuration and half on another.
//!
//! The reloadable partition applies at the loop's quiesce boundary. The flat
//! child tree makes most of it trivial: every turn worker is spawned fresh
//! from the live settings, so a new intelligence endpoint, model, instruction,
//! budget, tool override or workflow definition takes effect for the next unit
//! of work without touching the units already in flight. Live workflow runs
//! keep the definition they started with, pinned by hash, so a run cannot
//! change shape halfway through.

use super::reactor::Runtime;
use crate::config::settings as cfg;
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
        // Audit the reload: reconfiguring a running daemon is an operator
        // action, and a refused reload is as worth recording as an applied one.
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
            sid: None,
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
                // Tell attached clients what moved, so one that shows a
                // setting (the introspection panes, say) re-reads it instead
                // of rendering what the file said before. The same event an
                // `admin.set` pushes; only the source differs.
                #[cfg(feature = "a2a")]
                if changed.iter().any(|c| *c != "nothing") {
                    self.feed_push(
                        "config",
                        crate::runtime::a2a_server::FeedVis::All,
                        json!({"paths": changed, "source": "reload"}),
                    );
                }
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

        // Intelligence is hot-swappable: workers in flight keep dialing the
        // endpoint they were spawned with, the next spawned worker uses this.
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
        // The refresh cadence: re-arm so a changed interval takes effect now
        // rather than at the next restart. `arm_freshness` disarms the old
        // timer first, so this replaces rather than stacks — and it is what
        // makes `agent.instruction.refresh` honestly RELOADABLE, including
        // the case where it was `off` and no timer existed to notice.
        if old.agent.instruction_spec.refresh != new.agent.instruction_spec.refresh {
            self.arm_freshness();
            changed.push("agent.instruction.refresh");
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
                        version_id: None,
                        delivered_digest: None,
                    };
                }
                None => {
                    self.instruction = super::reactor::Instruction {
                        text: String::new(),
                        source: "static",
                        uri: None,
                        server: None,
                        version: self.instruction.version + 1,
                        version_id: None,
                        delivered_digest: None,
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
        // Runtime-settable: an `admin.set` changed it in memory, and the
        // reload puts the file's value back. That is a change, and it is
        // announced as one — otherwise the feed that said `admin.set` moved
        // the path would never hear that a reload moved it back.
        if old.agent.approval != new.agent.approval {
            changed.push("agent.approval");
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
                        c.set_tool_meta(crate::mcp::tool_meta(
                            &self.run_id,
                            &self.instance,
                            self.trace_id.as_deref(),
                        ));
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
                    // The rebuild failed, so the daemon stays on the tool
                    // configuration it is already running: put back the tools
                    // settings to match the registry that is still installed,
                    // or the two would disagree about what is callable.
                    self.settings.tools = old.tools.clone();
                    return Err(ReloadRefused::Invalid(errs));
                }
            }
        }
        // Skills sources — the config section, or the instruction's inline
        // `:::!skill` definitions (they live on `agent`, but they land in this
        // catalogue).
        // …and rebuilt whenever a LOCAL folder is configured, even with the
        // section unchanged: the skills live in files, and comparing the
        // setting only would make "edit a skill, send SIGHUP" a reload that
        // reports success and changes nothing.
        if old.skills != new.skills
            || old.agent.inline_skills != new.agent.inline_skills
            || new.skills.dir.is_some()
        {
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
            if let Some(dir) = &new.skills.dir {
                cat.add_dir(std::path::Path::new(dir));
            }
            cat.add_inline(&new.agent.inline_skills);
            // Rebuilt on every reload when a local folder is configured, so
            // the report has to compare rather than assume: saying "skills"
            // changed on a reload that changed nothing is the same dishonesty
            // in the other direction.
            let differs = !cat.same_skills_as(&self.skills);
            self.skills = cat;
            if differs {
                changed.push("skills");
            }
        }
        // Workflows: reload definitions. Retirement (runtime::retire) gives
        // every old version the same exit — unsubscribe what nothing else
        // wants, pin for live runs, apply its own `unload:` policy — whether
        // it was removed outright or replaced by a new hash.
        // Re-read whenever an entry names an external DOCUMENT, not only when
        // the entries themselves differ: `file:`/`dir:`/`uri:`/`url:` point at
        // content that changes without the config changing, and the entry
        // comparison cannot see that. `load_workflows` re-reads, and the
        // retirement loop below already keys off each definition's HASH, so an
        // unchanged document reloads to the same hash and nothing churns.
        let external = new.workflows.iter().any(|w| {
            ["file", "dir", "uri", "url"]
                .iter()
                .any(|k| w.get(*k).is_some())
        });
        if old.workflows != new.workflows || external {
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
            // Say "workflows" only when the loaded SET actually differs. A
            // re-read of unchanged documents must not report a change it did
            // not make — the reverse of the defect above, and just as
            // misleading in a reload log.
            let same = previous.len() == self.workflows.len()
                && previous
                    .iter()
                    .all(|(n, w)| self.workflows.get(n).is_some_and(|nw| nw.hash == w.hash));
            if !same {
                changed.push("workflows");
            }
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
        // Principals: rebuild the rules and swap them into the live listener.
        //
        // The rules compile into a `Resolver` (glob patterns, resolved bearer
        // secrets), which is why this was restart-only until now: the listener
        // held one built at startup. A rebuild that FAILS — an unresolvable
        // `{{secret:…}}`, a malformed matcher — must not take the listener's
        // working rules away, so the old resolver stays and the reload says so
        // rather than falling open on an empty rule set.
        //
        // The listener's posture is part of the resolver, so this one swap also
        // moves it: a no-auth loopback daemon given its first rule stops
        // treating local callers as the operator on the very next request.
        // Nothing else holds a posture for a reload to miss.
        //
        // The new rule ids are claimed in the identity registry before the
        // swap, and a `user`-role id an approved device already owns refuses
        // the change exactly as a failed rebuild does: the rule would be that
        // device's principal and inherit its history. The registry is
        // durable, so this holds for a name whose sessions are long gone, and
        // across a restart between the approval and the declaration.
        #[cfg(feature = "a2a")]
        if old.a2a.principals != new.a2a.principals
            && let Some(bridge) = self.a2a_serving.as_ref().map(|s| &s.bridge)
        {
            let env = self.env.clone();
            let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            // Built first, so a set that cannot compile claims no names.
            let built = crate::a2a::Resolver::build(&new.a2a, &envmap).and_then(|r| {
                crate::runtime::identities::register_rules(&self.durable, &new.a2a)
                    .map(|()| r)
                    .map_err(|refused| {
                        if let Some(line) = refused.collision_line() {
                            self.log.warn("identity.collision", line);
                        }
                        refused.to_string()
                    })
            });
            match built {
                Ok(r) => {
                    bridge.set_resolver(r);
                    // Who the model acts for moves with the rules, in the
                    // same step: an entry kept from the old rules would let a
                    // narrowed or removed principal's in-flight work keep its
                    // old reach until it called again — and a removed one
                    // never does. Callers named by their evidence alone are
                    // re-indexed when next seen; until then they fail closed.
                    self.principal_index = crate::a2a::principals::declared_principals(&new.a2a);
                    changed.push("a2a.principals");
                }
                Err(e) => {
                    self.log.warn(
                        "config.reload.principals",
                        json!({"err": e, "kept": "the principal rules in force before this reload"}),
                    );
                }
            }
        } else if old.a2a.principals != new.a2a.principals {
            // No listener to swap, but restored work still acts for its
            // owners through the model, with the rules as they now read.
            self.principal_index = crate::a2a::principals::declared_principals(&new.a2a);
        }
        // Webhook routes: rebuild from the (already reloaded) workflows and the
        // current `default_auth`, and install them.
        //
        // This runs unconditionally rather than under a `webhooks != webhooks`
        // guard, because a route's identity is spread across TWO sections: its
        // auth can come from `webhooks.default_auth` while the node itself
        // lives in `workflows[]`. Gating on either alone reintroduces exactly
        // the silent no-op this replaces. Rebuilding is cheap and carries live
        // per-route state across.
        #[cfg(feature = "a2a")]
        if let Some(handler) = self.webhook_handler.clone() {
            let nodes = self.webhook_nodes();
            let env = self.env.clone();
            let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            match handler.reload_routes(nodes, &self.settings.webhooks, &envmap) {
                Ok(paths) => {
                    if old.webhooks != new.webhooks || old.workflows != new.workflows {
                        self.log.info(
                            "webhooks.routes",
                            json!({"routes": paths, "reason": "reload"}),
                        );
                        changed.push("webhooks");
                    }
                }
                // A bad route definition leaves the SERVING table in place —
                // the listener keeps working under the rules it had.
                Err(e) => self.log.warn(
                    "config.reload.webhooks",
                    json!({"err": e, "kept": "the routes in force before this reload"}),
                ),
            }
        }
        // `a2a.introspection.enabled` also lives as an atomic on the feed (it
        // is runtime-settable through `admin.set`), so a reload has to move
        // BOTH or the two disagree: the settings gate would pass while the
        // feed still filtered introspection frames out. And turning it ON arms
        // the log ring the reads tail, or `debug.events` would answer that the
        // ring is not installed until the next restart.
        #[cfg(feature = "a2a")]
        if old.a2a.introspection.enabled != new.a2a.introspection.enabled {
            let on = new.a2a.introspection.enabled;
            if let Some(feed) = &self.a2a_feed {
                feed.set_introspection(on);
            }
            if on {
                self.arm_introspection_ring();
            }
            changed.push("a2a.introspection.enabled");
        }
        // The browser CORS allowlist: replaced in the live listener.
        //
        // The origin list was once neither rebuilt nor restart-only, so
        // removing an origin to revoke a web client reported success and
        // revoked nothing. The list has no external source to re-read, so
        // unlike the principals and the routes it is simply swapped.
        #[cfg(feature = "a2a")]
        if old.a2a.cors.origins != new.a2a.cors.origins
            && let Some(serving) = &self.a2a_serving
        {
            revise_origins(
                &serving.origins,
                &new.a2a.cors.origins,
                serving.launch.as_deref(),
            );
            changed.push("a2a.cors.origins");
        }
        if changed.is_empty() {
            changed.push("nothing");
        }
        Ok(changed)
    }
}

/// Replace the live CORS allowlist with `configured` — and the UI a launcher
/// started in this process, which is not configuration and so is in no
/// reloaded file. Swapping in the file's list alone would be the "reported
/// success, changed nothing" defect turned inside out: a reload that added
/// one origin would silently lock out the tab the operator is looking at.
#[cfg(feature = "a2a")]
fn revise_origins(
    live: &crate::a2a::serve::OriginList,
    configured: &[String],
    launch: Option<&crate::a2a::oauth::LaunchSlot>,
) {
    *live.write().unwrap_or_else(|e| e.into_inner()) =
        crate::a2a::oauth::admitted_origins(configured, launch);
}

/// Why a reload did not apply.
enum ReloadRefused {
    Invalid(Vec<String>),
    RestartRequired(Vec<String>),
}

#[cfg(all(test, feature = "a2a"))]
mod tests {
    use super::*;
    use crate::a2a::oauth::{LaunchSlot, admitted_origins};

    /// A reload that replaces `a2a.cors.origins` applies the new list and
    /// keeps the UI a launcher started admitted — it is in no file, so a
    /// reload that took the file's list alone would lock its tab out.
    #[test]
    fn the_launched_origin_survives_a_cors_reload() {
        let launched = "http://127.0.0.1:4555";
        let slot = LaunchSlot::new(Some(launched)).unwrap();
        let live: crate::a2a::serve::OriginList = Arc::new(std::sync::RwLock::new(
            admitted_origins(&["https://old.example".into()], Some(&slot)),
        ));
        revise_origins(&live, &["https://new.example".into()], Some(&slot));
        assert_eq!(
            *live.read().unwrap(),
            ["https://new.example", launched],
            "the new list applies and the launched UI stays"
        );
        // Emptied in the file, the launched UI is still the one admitted.
        revise_origins(&live, &[], Some(&slot));
        assert_eq!(*live.read().unwrap(), [launched]);
        // Without a launcher, the list is exactly what the file says.
        revise_origins(&live, &["https://new.example".into()], None);
        assert_eq!(*live.read().unwrap(), ["https://new.example"]);
    }
}
