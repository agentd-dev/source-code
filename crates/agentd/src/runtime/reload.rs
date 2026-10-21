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
            trace: None,
            link: None,
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
                crate::obs::metrics::record_config_reload("rejected");
            }
            Err(ReloadRefused::RestartRequired(paths)) => {
                self.log.warn(
                    "config.reload.restart_required",
                    json!({"trigger": trigger, "paths": paths}),
                );
                crate::obs::metrics::record_config_reload("rejected");
            }
        }
    }

    fn reload_inner(&mut self) -> Result<Vec<&'static str>, ReloadRefused> {
        let (loaded, _ask) = cfg::load(&self.args, &self.env)
            .map_err(|e| ReloadRefused::Invalid(vec![e.to_string()]))?;
        // Over every layer, the instruction's `:::!config` included: a
        // document that changes `lifecycle.run_until` would otherwise reload
        // into a process whose job shape was settled at startup — and with it
        // generate, or retire, the workflow its instruction runs as a job.
        let mut restart = cfg::restart_only_diff(
            &cfg::with_document_config(&self.settings_doc, &self.settings),
            &cfg::with_document_config(&loaded.doc, &loaded.settings),
        );
        // A company entry keys the Sessions, the User IDs and the servers
        // made from it, so adding, removing or changing one is a restart.
        restart.extend(company_entries_moved(&self.settings, &loaded.settings));
        // A delegated server's pins, client and catalogue are the delegation
        // supervisor's, built at start (RFC 0044): in P1 a change to one is
        // a restart, which purges every pooled token with the process.
        restart.extend(delegated_servers_moved(&self.settings, &loaded.settings));
        restart.extend(person_moved(&self.settings, &loaded.settings));
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
        crate::sec::harden::set_reserved(self.settings.supervisor_only_env());
        crate::sec::harden::set_passthrough(self.settings.spawn.env_passthrough.clone());
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
        // …and whenever the skills catalogue is built again: a step's
        // reference that a new skill makes ambiguous refuses the reload, as
        // it refuses a start. A local folder builds it on every reload, its
        // files changing without the config; unchanged definitions reload to
        // the same hash. (A source whose server the reload dials again moves
        // the servers, above.)
        let skills_inputs_moved = skills_rebuilt(old, new);
        let workflows = if old.workflows != new.workflows
            || external
            || channels_moved
            || servers_change
            || registry_inputs_moved
            || taint_inputs_moved
            || skills_inputs_moved
        {
            let mut staged = super::steps::StagedWorkflows::default();
            let docs = super::steps::workflow_documents(&self.settings, &mut staged.errs);
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
        // The intelligence connections: a token that does not resolve, or a
        // file that cannot be read, refuses the reload, as it refuses a
        // start. Rebuilt whole when any connection moved — the endpoints, the
        // credential, the headers, the login — so nothing about one is a
        // value copied at startup that a reload leaves behind.
        let intel = if connections_moved(old, new) {
            let env = self.env.clone();
            let envmap = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            Some(super::intel::Conns::build(new, &envmap).map_err(|e| vec![e])?)
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
            Ok(rest) => Ok(StagedReload { intel, mcp, ..rest }),
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
        // before any server is dialed, as it refuses a start.
        let specs = dial_specs(new)?;
        let mut set = std::collections::BTreeMap::new();
        let mut fresh = Vec::new();
        for s in &new.mcp.servers {
            let spec = &specs[&s.name];
            // A Company's server keeps its connection: its entry cannot
            // change under a reload (`company_entries_moved`), and only the
            // supervisor's join dials it.
            if spec.company.is_some() {
                if let Some(live) = self.mcp.get(&s.name) {
                    set.insert(s.name.clone(), live.clone());
                }
                continue;
            }
            // A delegated server has no connection of the loop's to carry:
            // its pool is the delegation supervisor's (RFC 0044).
            if spec.delegated {
                continue;
            }
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
                .map(super::supervisor_client)
                .and_then(|mut c| c.initialize().map(|()| c))
            {
                Ok(mut c) => {
                    c.set_tool_meta(crate::mcp::tool_meta(&self.run_id, &self.instance));
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
                        tools: {
                            let tools = c.list_tools().unwrap_or_default();
                            crate::runtime::report_dropped_tools(&self.log, &s.name, c);
                            tools
                        },
                    })
                })
                // A delegated server's catalogue, as last adopted: the server
                // cannot change under a reload (`delegated_servers_moved`).
                .chain(
                    self.registry
                        .built_from()
                        .iter()
                        .filter(|t| specs.get(&t.name).is_some_and(|sp| sp.delegated))
                        .cloned(),
                )
                // A Company's operation tools, as last adopted: its entry
                // cannot change under a reload (`company_entries_moved`).
                .chain(self.joining.offered().filter_map(|(server, o)| {
                    let s = new.mcp.servers.iter().find(|s| s.name == *server)?;
                    Some(ServerTools {
                        name: server.clone(),
                        ns: s.ns.clone(),
                        tags: Vec::new(),
                        tools: o.tools.clone(),
                    })
                }))
                .collect();
            Some(Registry::build(new, &server_tools)?)
        } else {
            None
        };
        // Skills sources ([`skills_rebuilt`]); a source whose server the
        // reload dials again is listed again too.
        let redialed = mcp.is_some_and(|m| {
            new.skills
                .sources
                .iter()
                .any(|src| m.fresh.contains(&src.server))
        });
        let skills = if skills_rebuilt(old, new) || redialed {
            Some(super::skills::build(new, conns))
        } else {
            None
        };
        // The workflows are checked against the registry they will run with
        // — the staged one, not the running one — the servers and streams
        // the new configuration declares, and the skills catalogue staged
        // above.
        let workflows = match workflows {
            Some(staged) => Some(self.prepare_workflows(
                staged,
                registry.as_ref().unwrap_or(&self.registry),
                skills.as_ref().map_or(&self.skills, |(c, _)| c),
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
                Some((r, true))
            }
            // Workflows re-staged on a registry that is kept still move what
            // the workflow tools and the read-back contracts carry — the taint
            // of the runs whose result they return, which the whole set
            // decides — and what a read-back withholds from whom. Re-derived
            // on a copy, swapped in when either moved, and reported as a
            // change of the tools only when a tag moved, so a reload that
            // changed nothing callers see still says so.
            None if workflows.is_some() => {
                let live = workflows.as_ref().map_or(&self.workflows, |p| &p.workflows);
                let defs: Vec<&crate::engine::Workflow> = live.values().map(|w| &**w).collect();
                let mut r = self.registry.clone();
                let errs = r.register_workflow_tools(&self.settings, &defs);
                if !errs.is_empty() {
                    return Err(errs);
                }
                let tools_moved = !r.same_tools(&self.registry);
                (tools_moved || !r.same_withholding(&self.registry)).then_some((r, tools_moved))
            }
            None => None,
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
        } else if let Some(t) = new.agent.instruction.as_deref()
            && self.instruction.source == "resource"
            && cfg::looks_like_resource_uri(t)
            && old.agent.instruction_facts != new.agent.instruction_facts
        {
            // The instruction is unchanged, but the facts its variants are
            // chosen by are not: the model moved, and with it the `agent`
            // family a `:::when{agent=…}` names. A static or `oci:` text is
            // folded again by the new load; a resource one is read again
            // here (with the new settings' facts: they are the running ones
            // while a reload is staged), or the running text would keep the
            // variants chosen for the old model while turns go to the new
            // one, and the reload report the new model as applied.
            let (server, res) = super::instruction_resource(t);
            Some(StagedInstruction::Reread(
                self.fetch_instruction_mcp(conns, server, res)
                    .map_err(|e| vec![format!("agent.instruction {t}: {e}")])?,
            ))
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
            intel: None,
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
            intel,
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
        let intel_moved = intel.is_some();

        // Intelligence is hot-swappable: workers in flight keep dialing the
        // connection they were spawned with, the next spawned worker uses
        // this. Which model each job runs on is not held here at all — it
        // resolves against the settings at each call — so a reload that only
        // moves a role or a model takes effect without anything to swap.
        let models_moved = old.intelligence.model != new.intelligence.model
            || old.intelligence.models != new.intelligence.models
            || old.intelligence.roles != new.intelligence.roles;
        if let Some(conns) = intel {
            self.intel = conns;
        }
        if intel_moved || models_moved {
            // Running subagents are handed theirs; a turn worker is one turn
            // long, and the next one is built from the settings as they are.
            self.swap_subagent_intelligence();
            changed.push("intelligence");
        }
        // Routes are read as each turn is launched, so a reload that moves
        // only them has nothing to rebuild — but it changed something, and
        // says so rather than reporting nothing.
        if old.intelligence.routes != new.intelligence.routes {
            changed.push("intelligence.routes");
        }
        // Budgets: new windows, counters carried over.
        if old.intelligence.budget != new.intelligence.budget {
            let counters = self.governor.to_value();
            let mut g = Governor::new(&new.intelligence.budget);
            g.restore(&counters, now_ms());
            self.governor = g;
            changed.push("intelligence.budget");
        }
        // Who may delegate, for what (RFC 0044 §5.14): the templates, device
        // pins and first binding take effect at once — a narrowed ceiling at
        // the next mint, since every call is held to its template as it is
        // now — and what they no longer allow is revoked off the loop (RFC
        // 7009 retries): its epoch tombstoned, its pooled tokens purged, so
        // runs that held it fail at their next delegated call.
        #[cfg(feature = "xaa")]
        if let Some(d) = self.delegation.clone() {
            let (o, n) = (&old.identity.delegation, &new.identity.delegation);
            if o.templates != n.templates
                || o.device_pins != n.device_pins
                || o.device_first_binding != n.device_first_binding
            {
                changed.push("identity.delegation");
                let (revoke, cleared) = d.reload(&new);
                if !cleared.is_empty() {
                    self.log.info(
                        "delegation.bindings.cleared",
                        json!({"names": cleared, "msg": "their sessions' IdP proofs are dropped"}),
                    );
                }
                if !revoke.is_empty() {
                    std::thread::Builder::new()
                        .name("delegation-reload-revoke".into())
                        .spawn(move || {
                            for (g, why) in revoke {
                                d.revoke_grant(&g, "reload", Some(&why));
                            }
                        })
                        .ok();
                }
            }
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
        // The servers this reload dialed — in place of a running connection,
        // or for the first time (added, or down until now) — and the ones it
        // dropped: their owners' subscriptions are restored, or their waits
        // fail or stay parked, below.
        let mut touched: Vec<String> = Vec::new();
        let mcp_moved = mcp.is_some();
        if let Some(StagedMcp { set, specs, fresh }) = mcp {
            let dropped: Vec<String> = self
                .mcp
                .keys()
                .filter(|k| !set.contains_key(*k))
                .cloned()
                .collect();
            touched = fresh.iter().chain(&dropped).cloned().collect();
            for t in &touched {
                // What a pending retry would ask for is asked for below.
                self.resubscribe_retry.remove(t);
            }
            let previous = std::mem::replace(&mut self.mcp, set);
            self.mcp_specs = specs;
            // A replaced or dropped connection's webhook subscriptions are
            // let go on that connection (RFC 0045 §5.6.3).
            #[cfg(feature = "mcp-events")]
            for name in &touched {
                if let Some(old) = previous.get(name) {
                    self.events_connection_replaced(name, std::sync::Arc::clone(old));
                }
            }
            // A dropped server is no skills source any more (validation
            // refuses one that names it), so the catalogue was rebuilt.
            for r in &dropped {
                self.log
                    .info("mcp.disconnect", json!({"server": r, "reason": "reload"}));
            }
            for name in &fresh {
                let protocol = self.mcp.get(name).and_then(|c| c.protocol_version());
                if let Some(c) = self.mcp.get(name) {
                    crate::runtime::report_dropped_tools(&self.log, name, c);
                }
                self.log.info(
                    "mcp.connect",
                    json!({"server": name, "reason": "reload", "protocol": protocol}),
                );
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
        // The AGENTS.md files the load read again: the next turn's prompt
        // renders them from the live settings.
        if old.agent.workspace != new.agent.workspace
            || old.agent.workspace_notes != new.agent.workspace_notes
        {
            changed.push("agent.workspace");
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
        let registry_rebuilt = registry.as_ref().is_some_and(|(_, moved)| *moved);
        if let Some((r, moved)) = registry {
            for w in &r.warnings {
                self.log.warn("registry.warning", json!({"warning": w}));
            }
            self.registry = r;
            if moved {
                changed.push("tools");
            }
        }
        if let Some((mut cat, notes)) = skills {
            // A context keeps the body it loaded until it loads another — of
            // a skill the new catalogue still holds. One whose source the
            // reload removed, or no longer admits, is let go of everywhere.
            cat.adopt_bodies(&mut self.skills);
            // Rebuilt on every reload when a local folder is configured, so
            // the report has to compare rather than assume: saying "skills"
            // changed on a reload that changed nothing is the same dishonesty
            // in the other direction.
            let differs = !cat.same_skills_as(&self.skills);
            self.skills = cat;
            super::skills::say(&self.log, notes);
            self.release_skills();
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
        // What the replaced connections carried, and what a server dialed
        // for the first time was never given: the subscriptions of the
        // unchanged workflows' `subscribe` starts and of suspended resource
        // waits. A dropped server's waits fail, or stay parked while it is
        // still configured, by the rule boot applies. (The workflows just
        // armed subscribed on the new connections already; the instruction
        // was read and subscribed through its connection above.)
        self.resubscribe_on(&touched, &anew);
        // MCP Events bindings follow `mcp.servers` (RFC 0045 §5.7): an edited
        // binding applies in place, a removed one stops, and a server dialed
        // afresh has its event types listed again. A binding is not part of
        // the dial spec, so editing one dials nothing.
        #[cfg(feature = "mcp-events")]
        {
            self.events_sync("reload", &touched);
            let declared = |s: &cfg::Settings| {
                let servers = s
                    .mcp
                    .servers
                    .iter()
                    .map(|x| (x.name.clone(), x.events.clone(), x.event_grants.clone()))
                    .collect::<Vec<_>>();
                let instruction = s
                    .agent
                    .instruction_spec
                    .mcp
                    .as_ref()
                    .map(|m| m.events().to_vec());
                (servers, instruction)
            };
            if declared(old) != declared(&new) {
                changed.push("mcp.events");
            }
        }
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
    /// The intelligence connections, rebuilt when any of them moved.
    intel: Option<super::intel::Conns>,
    mcp: Option<StagedMcp>,
    /// The registry to switch in, and whether the tools callers see moved
    /// (a rebuild is always reported); `None` keeps the running one.
    registry: Option<(Registry, bool)>,
    workflows: Option<super::steps::PreparedWorkflows>,
    skills: Option<(crate::context::skills::Catalogue, Vec<super::skills::Note>)>,
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

/// The `services.<name>` paths of the company entries a reload would add,
/// remove or change.
fn company_entries_moved(old: &cfg::Settings, new: &cfg::Settings) -> Vec<String> {
    let companies = |s: &cfg::Settings| -> Vec<(String, cfg::Service)> {
        s.services
            .iter()
            .filter(|(_, v)| v.kind == cfg::ServiceKind::Company)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let (a, b) = (companies(old), companies(new));
    let mut moved: Vec<String> = a
        .iter()
        .filter(|(k, v)| !b.iter().any(|(k2, v2)| k == k2 && v == v2))
        .chain(b.iter().filter(|(k, _)| !a.iter().any(|(k2, _)| k == k2)))
        .map(|(k, _)| format!("services.{k}"))
        .collect();
    moved.dedup();
    moved
}

/// The delegated servers (`grant: id_jag`) a reload adds, removes or
/// changes, named as restart-only paths.
fn delegated_servers_moved(old: &cfg::Settings, new: &cfg::Settings) -> Vec<String> {
    let delegated = |s: &cfg::Settings| -> Vec<cfg::McpServer> {
        cfg::delegation::delegated_servers(s).cloned().collect()
    };
    let (a, b) = (delegated(old), delegated(new));
    let mut moved: Vec<String> = a
        .iter()
        .filter(|m| !b.contains(m))
        .chain(b.iter().filter(|m| !a.iter().any(|o| o.name == m.name)))
        .map(|m| format!("mcp.servers.{} (a delegated server)", m.name))
        .collect();
    // A server that stops being delegated while keeping its name.
    moved.extend(
        a.iter()
            .filter(|m| {
                new.mcp
                    .servers
                    .iter()
                    .any(|n| n.name == m.name && !cfg::delegation::is_delegated(n))
            })
            .map(|m| format!("mcp.servers.{} (no longer delegated)", m.name)),
    );
    moved.sort();
    moved.dedup();
    moved
}

/// The pinned persons' `a2a.principals` rules — per-person operators and
/// certificate principals — a reload changes while the running process
/// delegates (RFC 0044 P1, P2): the delegation supervisor holds every grant
/// to the rules as it read them at start — the uid or the certificate's
/// key, the `idp_subject` pin, the id and the labels — so a change to one is
/// a restart, after which a grant enrolled under the old rule is unusable
/// (`not_enrolled`) rather than lent to whoever the new one names.
fn person_moved(old: &cfg::Settings, new: &cfg::Settings) -> Vec<String> {
    let delegates = cfg::delegation::delegated_servers(old).next().is_some()
        || !old.identity.providers.is_empty();
    let persons = |s: &cfg::Settings| cfg::delegation::pinned(s).cloned().collect::<Vec<_>>();
    if !delegates || persons(old) == persons(new) {
        return Vec::new();
    }
    vec!["a2a.principals (a per-person operator's rule)".to_string()]
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
/// Whether any intelligence connection's settings differ: the top-level
/// one's or any provider's, added, removed or changed.
/// Whether a reload builds the skills catalogue again: the skills section
/// or the instruction's inline `:::!skill` definitions (they live on
/// `agent`, but they land in this catalogue) moved — or a LOCAL folder is
/// configured, even with the section unchanged: the skills live in files,
/// and comparing the setting only would make "edit a skill, send SIGHUP" a
/// reload that reports success and changes nothing.
fn skills_rebuilt(old: &cfg::Settings, new: &cfg::Settings) -> bool {
    old.skills != new.skills
        || old.agent.inline_skills != new.agent.inline_skills
        || new.skills.dir.is_some()
        || new.skills.project_dir.is_some()
}

fn connections_moved(old: &cfg::Settings, new: &cfg::Settings) -> bool {
    let (o, n) = (&old.intelligence, &new.intelligence);
    o.endpoints != n.endpoints
        || o.dialect != n.dialect
        || o.token != n.token
        || o.token_file != n.token_file
        || o.headers != n.headers
        || o.auth != n.auth
        || o.timeout != n.timeout
        || o.structured_output != n.structured_output
        || o.providers != n.providers
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

/// Every configured MCP server's dial spec, or every reason one cannot be
/// dialed: a spec that cannot be built, and an endpoint closed egress does
/// not admit — the dial-time backstop behind validation, for whichever path
/// assembled the settings. Startup and a reload both ask it before any server
/// is dialed.
pub(crate) fn dial_specs(
    s: &cfg::Settings,
) -> Result<std::collections::BTreeMap<String, crate::config::McpServerSpec>, Vec<String>> {
    let mut specs = std::collections::BTreeMap::new();
    let mut errs = Vec::new();
    for srv in &s.mcp.servers {
        // A Company's server has no endpoint until discovery, and every URL
        // its documents name is held to the entry's `hosts` instead.
        if srv.company.is_none()
            && let Err(e) = cfg::egress_allows(
                &s.services,
                s.security.egress,
                cfg::ServiceKind::Mcp,
                &srv.endpoint,
            )
        {
            errs.push(e);
            continue;
        }
        match srv.to_spec() {
            Ok(mut spec) => {
                // An `oauth2` block with no client_id presents agentd's
                // Client ID Metadata Document (RFC 0052), and every token
                // request names the server as its RFC 8707 `resource`.
                spec.auth = srv
                    .auth
                    .as_ref()
                    .map(|a| s.auth_spec_at(a, &srv.endpoint, true));
                specs.insert(srv.name.clone(), spec);
            }
            Err(e) => errs.push(format!("mcp server {:?}: {e}", srv.name)),
        }
    }
    if errs.is_empty() {
        Ok(specs)
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod dial_tests {
    use super::*;

    /// The dial-time egress backstop refuses an endpoint closed egress does
    /// not admit, whatever assembled the settings — validation refuses the
    /// same configuration first, so these are settings it never saw — and
    /// admits one the service catalogue names.
    #[test]
    fn closed_egress_refuses_an_uncatalogued_endpoint_before_any_dial() {
        let settings = |services: &str| {
            cfg::Settings::from_document(
                crate::config::yaml::parse(&format!(
                    "security: {{egress: closed}}\n{services}mcp:\n  servers:\n    - {{name: crm, endpoint: \"https://crm.example/mcp\"}}\n"
                ))
                .unwrap(),
                "t",
            )
            .unwrap()
        };
        let errs = dial_specs(&settings("")).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("crm.example"), "{errs:?}");
        let specs = dial_specs(&settings(
            "services:\n  crm: {kind: mcp, endpoint: \"https://crm.example/mcp\"}\n",
        ))
        .unwrap();
        assert!(specs.contains_key("crm"));
    }

    /// A reload that adds, removes or changes a company entry is a restart:
    /// the entry keys the Sessions, the User IDs and the servers made from
    /// it. Any other entry is the reload's to apply.
    #[test]
    fn a_company_entry_moved_by_a_reload_is_named_for_a_restart() {
        let settings = |services: &str| {
            cfg::Settings::from_document(
                crate::config::yaml::parse(&format!("services:\n{services}")).unwrap(),
                "t",
            )
            .unwrap()
        };
        let shop = "  shop: {kind: company, endpoint: \"https://shop.example\"}\n";
        let crm = "  crm: {kind: mcp, endpoint: \"https://crm.example/mcp\"}\n";
        let base = settings(&format!("{shop}{crm}"));
        assert!(company_entries_moved(&base, &base).is_empty());
        let crm_moved = settings(&format!(
            "{shop}  crm: {{kind: mcp, endpoint: \"https://crm2.example/mcp\"}}\n"
        ));
        assert!(company_entries_moved(&base, &crm_moved).is_empty());
        let changed = settings(&format!(
            "  shop: {{kind: company, endpoint: \"https://shop.example\", hosts: [shop.example]}}\n{crm}"
        ));
        assert_eq!(company_entries_moved(&base, &changed), ["services.shop"]);
        let removed = settings(crm);
        assert_eq!(company_entries_moved(&base, &removed), ["services.shop"]);
        let added = settings(&format!(
            "{shop}{crm}  mall: {{kind: company, endpoint: \"https://mall.example\"}}\n"
        ));
        assert_eq!(company_entries_moved(&base, &added), ["services.mall"]);
    }

    /// A reload that changes a per-person operator's rule — its uid, its
    /// pin, its id — or adds or removes one while the process delegates is a
    /// restart; any other principal rule, or the same rule moved in the
    /// list, is the reload's.
    #[test]
    fn the_per_person_rule_moved_by_a_reload_is_named_for_a_restart() {
        let settings = |rules: &str| {
            cfg::Settings::from_document(
                crate::config::yaml::parse(&format!(
                    "identity:\n  providers:\n    okta: {{issuer: \"https://acme.okta.example\", client_id: c}}\na2a:\n  principals:\n{rules}"
                ))
                .unwrap(),
                "t",
            )
            .unwrap()
        };
        let person = |uid: u32, sub: &str| {
            format!(
                "    - {{match: {{uid: {uid}}}, role: operator, id: alice, idp_subject: {{iss: \"https://acme.okta.example\", sub: {sub}}}}}\n"
            )
        };
        let other = "    - {match: {bearer_ref: \"{{secret:B}}\"}, role: user, id: bob}\n";
        let base = settings(&format!("{}{other}", person(1000, "00u1alice")));
        assert!(person_moved(&base, &base).is_empty());
        let reordered = settings(&format!("{other}{}", person(1000, "00u1alice")));
        assert!(person_moved(&base, &reordered).is_empty());
        let other_moved = settings(&format!(
            "{}    - {{match: {{bearer_ref: \"{{{{secret:B}}}}\"}}, role: operator, id: bob}}\n",
            person(1000, "00u1alice")
        ));
        assert!(person_moved(&base, &other_moved).is_empty());
        let second = "    - {match: {uid: 1001}, role: operator, id: bob, idp_subject: {iss: \"https://acme.okta.example\", sub: 00u2bob}}\n";
        for moved in [
            person(0, "00u1alice"),
            person(1000, "00u2bob"),
            String::new(),
            format!("{}{second}", person(1000, "00u1alice")),
        ] {
            assert_eq!(
                person_moved(&base, &settings(&format!("{moved}{other}"))),
                ["a2a.principals (a per-person operator's rule)"],
                "{moved}"
            );
        }
        // A process that delegates nothing holds no grant to a rule.
        let plain = |rules: &str| {
            cfg::Settings::from_document(
                crate::config::yaml::parse(&format!("a2a:\n  principals:\n{rules}")).unwrap(),
                "t",
            )
            .unwrap()
        };
        assert!(
            person_moved(
                &plain(&person(1000, "00u1alice")),
                &plain(&person(0, "00u1alice"))
            )
            .is_empty()
        );
    }
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
