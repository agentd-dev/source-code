// SPDX-License-Identifier: AGPL-3.0-only
//! **Hot reload** of the configuration: SIGHUP or `lifecycle.watch_config`
//! re-merges the files and re-validates. The reload is all-or-nothing — if any
//! restart-only path changed the whole reload is refused as
//! `restart_required` and the running configuration stays, so the daemon never
//! ends up half on one configuration and half on another.
//!
//! The same holds inside the reloadable partition. A reload is staged first:
//! the new MCP servers are dialed into a connection set beside the running
//! one, and the tool registry, the workflows, the skills and the instruction
//! are built and checked against it — and so are the listener's principal
//! rules and webhook routes. Only when all of it succeeds is it committed, in
//! one step with nothing left that can fail. A reload is applied whole or
//! refused with nothing changed, and what a refused one dialed is closed.
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
        // Stage, then commit. Everything that can refuse the reload runs in
        // `stage_reload`, building the new pieces BESIDE the running ones:
        // the new MCP servers are dialed into a set of their own, and the
        // tool registry, the workflows, the skills and the instruction are
        // built and checked against that set. The settings are the one
        // running structure staging reads the new values through, and a
        // refusal puts them back — so a refused reload changes nothing. The
        // commit then switches everything in with no step left that can
        // fail, so an applied reload changed everything it reports.
        let old = std::mem::replace(&mut self.settings, loaded.settings);
        let staged = match self.stage_reload(&old) {
            Ok(staged) => staged,
            Err(errs) => {
                self.settings = old;
                return Err(ReloadRefused::Invalid(errs));
            }
        };
        self.settings_doc = loaded.doc;
        Ok(self.commit_reload(&old, staged))
    }

    /// Build and check everything the reload changes, beside the running
    /// state. `self.settings` already holds the new settings; nothing else
    /// running is touched. What this dials, a refusal closes.
    fn stage_reload(&self, old: &cfg::Settings) -> Result<StagedReload, Vec<String>> {
        let new = &self.settings;
        // Workflows first: a set that cannot load (two definitions of one
        // name, a file that does not parse, a fetch that fails) refuses the
        // reload before anything is dialed.
        //
        // Re-read whenever an entry names an external DOCUMENT, not only when
        // the entries themselves differ: `file:`/`dir:`/`uri:`/`url:` point at
        // content that changes without the config changing, and the entry
        // comparison cannot see that. The retirement at commit keys off each
        // definition's HASH, so an unchanged document reloads to the same hash
        // and nothing churns.
        let external = new.workflows.iter().any(|w| {
            ["file", "dir", "uri", "url"]
                .iter()
                .any(|k| w.get(*k).is_some())
        });
        // The channels a document's gates are announced on ride on the
        // definitions they belong to, so a document that moves only a
        // `::!human`'s channel reloads the definitions too — their hashes
        // stay, nothing retires, and the next gate is announced on the new
        // channel.
        let channels_moved = old.agent.document_gate_channels != new.agent.document_gate_channels;
        let servers_change = old.mcp != new.mcp;
        // …and whenever the servers or the registry the definitions are
        // checked against change: a step naming a tool or a server the
        // reload takes away refuses it, as it refuses a start, rather than
        // going live to fail on the day it runs.
        let registry_inputs_moved =
            old.tools != new.tools || old.knowledge != new.knowledge || old.search != new.search;
        // …and whenever anything the stream-taint check reads moves — the
        // subagent templates (what a spawn reaches, which streams a child
        // mirrors in), the service catalog an instance template's servers
        // resolve against, `security.allow_trifecta` — so a stored definition,
        // which nothing else re-checks, is held to the new line. The check
        // names its own inputs, so this cannot fall behind what it reads.
        let taint_inputs_moved = crate::config::taint::inputs_moved(old, new);
        let workflows = if old.workflows != new.workflows
            || external
            || channels_moved
            || servers_change
            || registry_inputs_moved
            || taint_inputs_moved
        {
            let mut staged = super::steps::StagedWorkflows::default();
            let docs = self.workflow_documents(&mut staged.errs);
            // A `uri:` document is read through a connected MCP server. When
            // this reload changes the servers, it is read through the set the
            // reload would run on, once that is staged below.
            let (now, after_mcp): (Vec<_>, Vec<_>) = docs
                .into_iter()
                .partition(|d| !(servers_change && reads_a_resource(&d.entry)));
            self.stage_workflows(now, &self.mcp, &mut staged);
            if !staged.errs.is_empty() {
                return Err(staged.errs);
            }
            Some((staged, after_mcp))
        } else {
            None
        };
        // The intelligence token: a file that cannot be read refuses the
        // reload, as it refuses a start. Kept as it was, the new endpoint
        // went live with the old endpoint's credential.
        let intel_token = if intelligence_moved(old, new) {
            let env = self.env.clone();
            let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            Some(
                super::resolve_intel_token(new, &envmap)
                    .map_err(|e| vec![format!("intelligence token: {e}")])?,
            )
        } else {
            None
        };
        let mcp = if servers_change {
            Some(self.stage_mcp(old)?)
        } else {
            None
        };
        let staged = self.stage_on_servers(old, mcp.as_ref(), workflows);
        #[cfg(feature = "a2a")]
        let staged = staged.and_then(|rest| {
            let listener = self.stage_listener(old, rest.workflows.as_ref())?;
            Ok(StagedReload { listener, ..rest })
        });
        match staged {
            Ok(rest) => Ok(StagedReload {
                intel_token,
                mcp,
                ..rest
            }),
            Err(errs) => {
                if let Some(m) = mcp {
                    self.discard_mcp(m);
                }
                Err(errs)
            }
        }
    }

    /// The listener's side of a reload, built and not yet served: the
    /// principal rules compiled into a resolver, their ids checked against
    /// the identity registry, and the webhook route table. Any of them that
    /// cannot be built refuses the reload — a rule set that did not compile
    /// used to be logged and skipped at commit, with the reload reported
    /// applied, the new rules in `settings` and the old ones in force, and a
    /// later reload of the same file seeing no change to retry.
    ///
    /// Staged LAST, because the rule ids are also claimed here: the claim is
    /// a durable write, and one made for a reload that something after it
    /// refused would hold a name no rule declares. Nothing after this can
    /// refuse.
    #[cfg(feature = "a2a")]
    fn stage_listener(
        &self,
        old: &cfg::Settings,
        workflows: Option<&super::steps::PreparedWorkflows>,
    ) -> Result<StagedListener, Vec<String>> {
        let new = &self.settings;
        let env = self.env.clone();
        let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        // Webhook routes: built on every reload rather than under a
        // `webhooks != webhooks` guard, because a route's identity is spread
        // across TWO sections — its auth can come from `webhooks.default_auth`
        // while the node itself lives in `workflows[]` — and a
        // `{{secret-file:…}}` rotated by a remounted Secret changes no
        // document at all. Building is cheap and carries live per-route
        // state across.
        let routes = match &self.webhook_handler {
            Some(handler) => {
                let live = workflows.map_or(&self.workflows, |p| &p.workflows);
                let nodes = super::webhooks::webhook_nodes_of(live);
                Some(
                    handler
                        .stage_routes(nodes, &new.webhooks, &envmap)
                        .map_err(|e| vec![format!("webhooks: {e}")])?,
                )
            }
            None => None,
        };
        // Principals: the rules compile into a `Resolver` (glob patterns,
        // resolved bearer secrets). A `user`-role id an approved device
        // already owns refuses them exactly as a rule that does not compile:
        // the rule would be that device's principal and inherit its history.
        // The registry is durable, so this holds for a name whose sessions
        // are long gone, and across a restart between the approval and the
        // declaration.
        let principals = if old.a2a.principals != new.a2a.principals && self.a2a_serving.is_some() {
            let resolver = crate::a2a::Resolver::build(&new.a2a, &envmap)
                .map_err(|e| vec![format!("a2a.principals: {e}")])?;
            let refused = |refused: crate::runtime::identities::Refused| {
                if let Some(line) = refused.collision_line() {
                    self.log.warn("identity.collision", line);
                }
                vec![refused.to_string()]
            };
            crate::runtime::identities::check_rules(&self.durable, &new.a2a).map_err(refused)?;
            crate::runtime::identities::claim_rules(&self.durable, &new.a2a).map_err(refused)?;
            Some(resolver)
        } else {
            None
        };
        Ok(StagedListener { principals, routes })
    }

    /// The connection set the new `mcp` section describes, built beside the
    /// running one: an unchanged server's live connection is carried over
    /// (the same connection, never re-dialed); an added or changed server is
    /// dialed and initialized into the new set. A server whose dial fails is
    /// logged and left out, exactly as at startup — and a workflow that
    /// needs it refuses the reload below.
    fn stage_mcp(&self, old: &cfg::Settings) -> Result<StagedMcp, Vec<String>> {
        let new = &self.settings;
        // Every spec first: one that cannot be built refuses the reload
        // before any server is dialed, as it refuses a start. So does an
        // endpoint closed egress does not admit — the dial-time backstop a
        // start applies behind validation, for whichever path assembled it.
        let mut specs = std::collections::BTreeMap::new();
        let mut errs = Vec::new();
        for s in &new.mcp.servers {
            if let Err(e) = cfg::egress_allows(
                &new.services,
                new.security.egress,
                cfg::ServiceKind::Mcp,
                &s.endpoint,
            ) {
                errs.push(e);
                continue;
            }
            match s.to_spec() {
                Ok(spec) => {
                    specs.insert(s.name.clone(), spec);
                }
                Err(e) => errs.push(format!("mcp server {:?}: {e}", s.name)),
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        let mut set = std::collections::BTreeMap::new();
        let mut fresh = Vec::new();
        for s in &new.mcp.servers {
            let spec = &specs[&s.name];
            // Unchanged means everything the dial reads: the endpoint and
            // headers, and also the credential (`auth`, `oauth`, `aauth`),
            // the `service` its cached login is keyed by, its `rate` and its
            // timeout. Comparing a subset carried the old connection over when
            // a credential was rotated or revoked, while the reload reported
            // `mcp`. Only the tags need no dial: they feed the registry, which
            // is rebuilt from the new spec.
            let same = self.mcp_specs.get(&s.name).is_some_and(|was| {
                untagged(was) == untagged(spec)
                    && old
                        .mcp
                        .servers
                        .iter()
                        .find(|o| o.name == s.name)
                        .is_some_and(|o| server_timeout(old, o) == server_timeout(new, s))
            });
            if same && let Some(live) = self.mcp.get(&s.name) {
                set.insert(s.name.clone(), live.clone());
                continue;
            }
            match crate::mcp::from_spec(spec, server_timeout(new, s))
                .and_then(|mut c| c.initialize().map(|()| c))
            {
                Ok(mut c) => {
                    c.set_tool_meta(crate::mcp::tool_meta(
                        &self.run_id,
                        &self.instance,
                        self.trace_id.as_deref(),
                    ));
                    set.insert(s.name.clone(), Arc::new(c));
                    fresh.push(s.name.clone());
                }
                Err(e) => {
                    self.log.warn(
                        "mcp.connect.fail",
                        json!({"server": s.name, "err": e.to_string(), "reason": "reload"}),
                    );
                    crate::obs::metrics::record_mcp_connect_failure(&s.name);
                }
            }
        }
        Ok(StagedMcp { set, specs, fresh })
    }

    /// Close what a refused reload dialed. The connections are dropped with
    /// the staged set — nothing running ever held them — and each is logged,
    /// so the log shows a server that answered a handshake did not join.
    fn discard_mcp(&self, staged: StagedMcp) {
        for name in &staged.fresh {
            self.log.info(
                "mcp.disconnect",
                json!({"server": name, "reason": "reload refused"}),
            );
        }
    }

    /// Everything built against the servers the reload would run on: the
    /// `uri:` workflow documents they serve, the tool registry, the workflow
    /// checks, the skills and a resource instruction. `mcp` is the staged
    /// connection set when the servers change; otherwise the running one
    /// serves.
    fn stage_on_servers(
        &self,
        old: &cfg::Settings,
        mcp: Option<&StagedMcp>,
        workflows: Option<(
            super::steps::StagedWorkflows,
            Vec<super::steps::WorkflowDocument>,
        )>,
    ) -> Result<StagedReload, Vec<String>> {
        let new = &self.settings;
        let conns = mcp.map_or(&self.mcp, |m| &m.set);
        let specs = mcp.map_or(&self.mcp_specs, |m| &m.specs);
        let workflows = match workflows {
            Some((mut staged, after_mcp)) => {
                self.stage_workflows(after_mcp, conns, &mut staged);
                if !staged.errs.is_empty() {
                    return Err(staged.errs);
                }
                Some(staged)
            }
            None => None,
        };
        // The registry is rebuilt when what it is built from changed — or
        // when the workflow tools did, since the configuration's workflows
        // are the only door a workflow tool has.
        let tools_moved = workflows.as_ref().is_some_and(|st| {
            workflow_tools(st.defs().iter())
                != workflow_tools(self.workflows.values().map(|w| &**w))
        });
        let rebuild = old.tools != new.tools
            || old.mcp != new.mcp
            || old.knowledge != new.knowledge
            || old.search != new.search
            || tools_moved;
        let registry = if rebuild {
            let server_tools: Vec<ServerTools> = new
                .mcp
                .servers
                .iter()
                .filter_map(|s| {
                    let c = conns.get(&s.name)?;
                    Some(ServerTools {
                        name: s.name.clone(),
                        ns: s.ns.clone(),
                        tags: specs
                            .get(&s.name)
                            .map(|sp| sp.tags.clone())
                            .unwrap_or_default(),
                        tools: c.list_tools().unwrap_or_default(),
                    })
                })
                .collect();
            Some(Registry::build(new, &server_tools)?)
        } else {
            None
        };
        // The workflows are checked against the registry and the servers
        // they will run with — the staged ones, not the running ones.
        let workflows = match workflows {
            Some(staged) => Some(self.prepare_workflows(
                staged,
                registry.as_ref().unwrap_or(&self.registry),
                conns,
            )?),
            None => None,
        };
        // A rebuilt registry carries the workflow tools of the set that will
        // be live: built from settings and servers alone, it would drop
        // every one of them.
        let registry = match registry {
            Some(mut r) => {
                let live = workflows.as_ref().map_or(&self.workflows, |p| &p.workflows);
                let defs: Vec<&crate::engine::Workflow> = live.values().map(|w| &**w).collect();
                let errs = r.register_workflow_tools(&self.settings, &defs);
                if !errs.is_empty() {
                    return Err(errs);
                }
                Some(r)
            }
            None => None,
        };
        // Skills sources — the config section, or the instruction's inline
        // `:::!skill` definitions (they live on `agent`, but they land in this
        // catalogue).
        // …and rebuilt whenever a LOCAL folder is configured, even with the
        // section unchanged: the skills live in files, and comparing the
        // setting only would make "edit a skill, send SIGHUP" a reload that
        // reports success and changes nothing.
        let skills = if old.skills != new.skills
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
                if let Some(c) = conns.get(&src.server) {
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
            Some(cat)
        } else {
            None
        };
        // The instruction: static text, or a resource read (and verified,
        // and folded) through the servers the reload would run on. One that
        // cannot be read refuses the reload, as it refuses a start — the
        // reload that reported `agent.instruction` while the old text stayed
        // is the defect this closes.
        let instruction = if old.agent.instruction != new.agent.instruction {
            Some(match new.agent.instruction.clone() {
                Some(t) if cfg::looks_like_resource_uri(&t) => {
                    let (server, res) = super::instruction_resource(&t);
                    StagedInstruction::Resource(
                        self.fetch_instruction_mcp(conns, server, res)
                            .map_err(|e| vec![format!("agent.instruction {t}: {e}")])?,
                    )
                }
                Some(t) => StagedInstruction::Text(t),
                None => StagedInstruction::Text(String::new()),
            })
        } else if let Some(m) = mcp
            && let Some(t) = new.agent.instruction.as_deref()
            && self.instruction.source == "resource"
            && cfg::looks_like_resource_uri(t)
            && self
                .instruction
                .server
                .as_deref()
                .is_some_and(|srv| !m.set.contains_key(srv) || m.fresh.iter().any(|f| f == srv))
        {
            // The instruction is unchanged, but the server it is read through
            // is not: removed, or re-dialed. Removed, the reload is refused
            // as a start with that configuration is (unless another server
            // serves a bare URI). Re-dialed, it is read again through the new
            // connection so the commit subscribes it THERE — the old
            // connection, and its subscription, go with the reload, and the
            // publisher's updates and revocations would stop arriving.
            let (server, res) = super::instruction_resource(t);
            Some(StagedInstruction::Reread(
                self.fetch_instruction_mcp(conns, server, res)
                    .map_err(|e| vec![format!("agent.instruction {t}: {e}")])?,
            ))
        } else {
            None
        };
        Ok(StagedReload {
            intel_token: None,
            mcp: None,
            registry,
            workflows,
            skills,
            instruction,
            channels_moved: old.agent.document_gate_channels != new.agent.document_gate_channels,
            #[cfg(feature = "a2a")]
            listener: StagedListener::default(),
        })
    }

    /// Switch the staged reload in. Nothing here can refuse: every piece
    /// that could fail was built in `stage_reload`, so this is swaps and
    /// in-memory rebuilds, plus best-effort notices (a resource
    /// subscription, a binding report, closing what the reload dropped)
    /// that are logged when they fail.
    fn commit_reload(&mut self, old: &cfg::Settings, staged: StagedReload) -> Vec<&'static str> {
        let new = self.settings.clone();
        let StagedReload {
            intel_token,
            mcp,
            registry,
            workflows,
            skills,
            instruction,
            channels_moved,
            #[cfg(feature = "a2a")]
            listener,
        } = staged;
        let mut changed = Vec::new();

        // Intelligence is hot-swappable: workers in flight keep dialing the
        // endpoint they were spawned with, the next spawned worker uses this.
        if let Some(token) = intel_token {
            self.intel_uri = new.intelligence.endpoint_list().unwrap_or_default();
            self.model = new.intelligence.model.clone().unwrap_or_default();
            self.intel_token = token;
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
        // MCP servers: the staged set replaces the running one whole. A
        // connection the new set does not carry — a removed server's, or a
        // changed one's — closes once the last holder lets go of it: a call
        // already in flight on it finishes there, and a step that starts
        // after this finds the server gone ("not connected").
        let mut redialed: Vec<String> = Vec::new();
        let mcp_moved = mcp.is_some();
        if let Some(StagedMcp { set, specs, fresh }) = mcp {
            let dropped: Vec<String> = self
                .mcp
                .keys()
                .filter(|k| !set.contains_key(*k))
                .cloned()
                .collect();
            redialed = fresh
                .iter()
                .filter(|f| self.mcp.contains_key(*f))
                .cloned()
                .collect();
            let previous = std::mem::replace(&mut self.mcp, set);
            self.mcp_specs = specs;
            for r in &dropped {
                if skills.is_none() {
                    self.skills.forget_server(r);
                }
                self.log
                    .info("mcp.disconnect", json!({"server": r, "reason": "reload"}));
            }
            for name in &fresh {
                self.log
                    .info("mcp.connect", json!({"server": name, "reason": "reload"}));
            }
            drop(previous);
        }
        // Instruction (static text; a resource instruction re-subscribes, on
        // the connection it was read through — the staged one).
        if let Some(StagedInstruction::Reread(fetched)) = instruction {
            // Read again only because its server was re-dialed: a change
            // only if the text is not what was running.
            let before = self.instruction.text.clone();
            self.adopt_instruction(fetched);
            if self.instruction.text != before {
                changed.push("agent.instruction");
            }
        } else if let Some(instruction) = instruction {
            match instruction {
                StagedInstruction::Resource(fetched) | StagedInstruction::Reread(fetched) => {
                    self.adopt_instruction(fetched)
                }
                StagedInstruction::Text(text) => {
                    self.instruction = super::reactor::Instruction {
                        text,
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
        if mcp_moved {
            changed.push("mcp");
        }
        // Registry (overrides/disabled/tools) — rebuilt when tools/mcp/knowledge/search
        // or the workflow tools changed.
        let registry_rebuilt = registry.is_some();
        if let Some(r) = registry {
            for w in &r.warnings {
                self.log.warn("registry.warning", json!({"warning": w}));
            }
            self.registry = r;
            changed.push("tools");
        }
        if let Some(cat) = skills {
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
        // Workflows: install the set staged above. Retirement (runtime::retire)
        // gives every old version the same exit — unsubscribe what nothing
        // else wants, pin for live runs, apply its own `unload:` policy —
        // whether it was removed outright or replaced by a new hash.
        let mut anew: Vec<String> = Vec::new();
        if let Some(prepared) = workflows {
            let previous = std::mem::take(&mut self.workflows);
            self.install_workflows(prepared);
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
            anew = self
                .workflows
                .iter()
                .filter(|(n, w)| previous.get(*n).is_none_or(|p| p.hash != w.hash))
                .map(|(n, _)| n.clone())
                .collect();
            // A replaced definition's schedule keeps its deadline when the
            // schedule itself is what it was: only an edit to the start node
            // re-arms it from now.
            let keep: Vec<(String, String)> = anew
                .iter()
                .filter_map(|n| Some((n, previous.get(n)?, self.workflows.get(n)?)))
                .flat_map(|(n, was, now)| {
                    now.start_steps()
                        .into_iter()
                        .filter(|s| s.kind == "schedule")
                        .filter(|s| {
                            was.steps
                                .get(&s.id)
                                .is_some_and(|p| p.kind == s.kind && p.spec == s.spec)
                        })
                        .map(|s| (n.clone(), s.id.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            self.arm_long_lived_starts_of(&anew, &keep);
            // Say "workflows" only when the loaded SET actually differs. A
            // re-read of unchanged documents must not report a change it did
            // not make — the reverse of the defect above, and just as
            // misleading in a reload log.
            let same = previous.len() == self.workflows.len()
                && previous
                    .iter()
                    .all(|(n, w)| self.workflows.get(n).is_some_and(|nw| nw.hash == w.hash));
            if !same || channels_moved {
                changed.push("workflows");
            }
        }
        // What the dropped connections carried: the subscriptions of the
        // unchanged workflows' `subscribe` starts and of suspended resource
        // waits, on each server this reload re-dialed. (The ones just armed
        // subscribed on the new connection already; the instruction was read
        // and subscribed through it above.)
        self.resubscribe_on(&redialed, &anew);
        if registry_rebuilt {
            self.log_workflow_tools();
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
        // Principals: the rules staged above are swapped into the live
        // listener. The listener's posture is part of the resolver, so this
        // one swap also moves it: a no-auth loopback daemon given its first
        // rule stops treating local callers as the operator on the very next
        // request. Nothing else holds a posture for a reload to miss.
        #[cfg(feature = "a2a")]
        if let Some(r) = listener.principals
            && let Some(bridge) = self.a2a_serving.as_ref().map(|s| &s.bridge)
        {
            bridge.set_resolver(r);
            // Who the model acts for moves with the rules, in the same step:
            // an entry kept from the old rules would let a narrowed or
            // removed principal's in-flight work keep its old reach until it
            // called again — and a removed one never does. Callers named by
            // their evidence alone are re-indexed when next seen; until then
            // they fail closed.
            self.principal_index = crate::a2a::principals::declared_principals(&new.a2a);
            changed.push("a2a.principals");
        } else if old.a2a.principals != new.a2a.principals {
            // No listener to swap, but restored work still acts for its
            // owners through the model, with the rules as they now read.
            self.principal_index = crate::a2a::principals::declared_principals(&new.a2a);
        }
        // Webhook routes: the table staged above, from the workflows just
        // installed and the current `default_auth`.
        #[cfg(feature = "a2a")]
        if let Some(routes) = listener.routes
            && let Some(handler) = self.webhook_handler.clone()
        {
            let paths = handler.install_routes(routes);
            if old.webhooks != new.webhooks || old.workflows != new.workflows {
                self.log.info(
                    "webhooks.routes",
                    json!({"routes": paths, "reason": "reload"}),
                );
                changed.push("webhooks");
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
        changed
    }
}

/// A reload built and checked beside the running state: what
/// `commit_reload` switches in. A `None` is a section the reload leaves as
/// it is.
struct StagedReload {
    /// The resolved intelligence token, when intelligence changed.
    intel_token: Option<Option<String>>,
    mcp: Option<StagedMcp>,
    registry: Option<Registry>,
    workflows: Option<super::steps::PreparedWorkflows>,
    skills: Option<crate::context::skills::Catalogue>,
    instruction: Option<StagedInstruction>,
    channels_moved: bool,
    #[cfg(feature = "a2a")]
    listener: StagedListener,
}

/// The connection set a reload that changes `mcp` would run on.
struct StagedMcp {
    /// Every configured server that is connected: carried-over live
    /// connections and freshly dialed ones.
    set: std::collections::BTreeMap<String, Arc<crate::mcp::client::McpClient>>,
    specs: std::collections::BTreeMap<String, crate::config::McpServerSpec>,
    /// The servers dialed for this reload: announced if it applies, closed
    /// if it is refused.
    fresh: Vec<String>,
}

enum StagedInstruction {
    Text(String),
    Resource(super::FetchedInstruction),
    /// The unchanged resource instruction, read again because the server it
    /// is read through was re-dialed.
    Reread(super::FetchedInstruction),
}

/// The listener's part of a staged reload: `None` is a part left as it is.
#[cfg(feature = "a2a")]
#[derive(Default)]
struct StagedListener {
    principals: Option<crate::a2a::Resolver>,
    routes: Option<super::webhooks::StagedRoutes>,
}

/// A spec with its tags cleared: what dialing a server reads.
fn untagged(spec: &crate::config::McpServerSpec) -> crate::config::McpServerSpec {
    crate::config::McpServerSpec {
        tags: Vec::new(),
        ..spec.clone()
    }
}

/// The timeout a server is dialed with: its own, or the section's default.
fn server_timeout(settings: &cfg::Settings, server: &cfg::McpServer) -> Duration {
    server.timeout.map(|d| d.0).unwrap_or(
        settings
            .mcp
            .default_timeout
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(60)),
    )
}

/// Whether the reload moves what a turn worker dials the model with.
fn intelligence_moved(old: &cfg::Settings, new: &cfg::Settings) -> bool {
    old.intelligence.endpoints != new.intelligence.endpoints
        || old.intelligence.model != new.intelligence.model
        || old.intelligence.token != new.intelligence.token
        || old.intelligence.token_file != new.intelligence.token_file
}

/// The workflows that register a tool, by name, with the definition hash the
/// tool is derived from (its arguments are the inputs, its tags what the
/// steps reach).
fn workflow_tools<'a>(
    defs: impl Iterator<Item = &'a crate::engine::Workflow>,
) -> std::collections::BTreeMap<&'a str, &'a str> {
    defs.filter(|w| w.tool.is_some())
        .map(|w| (w.name.as_str(), w.hash.as_str()))
        .collect()
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

/// Whether a workflow entry's document is read as an MCP resource (`uri:`),
/// the way `stage_workflows` decides it: a `file:` wins over a `uri:`.
fn reads_a_resource(entry: &serde_json::Value) -> bool {
    entry.get("file").is_none() && entry.get("uri").is_some()
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
