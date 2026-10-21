// SPDX-License-Identifier: AGPL-3.0-only
//! **Runs and steps**: arming start nodes,
//! turning start events into durable runs, scheduling ready steps every tick,
//! executing the step kinds (data steps in-loop, MCP calls on executor
//! threads, `agent`/`think` in turn workers, `sleep` on durable timers,
//! `finish` closing the run), retries + `on_error` routing, and the
//! `workflow.*` tools.

use super::children::ChildKind;
use super::events::kinds;
use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ToolCaller, ToolOutcome};
use crate::config::fileset::{Dir, expand_dir_ordered};
use crate::config::settings::{ModelRole, substitute_config_vars};
use crate::context::Msg;
use crate::engine::model::{OnError, Step, Workflow, parse_workflow};
use crate::engine::run::{
    self, Next, RunState, RunStatus, Start, StepStatus, env_view, render_spec,
};
use crate::engine::template;
use crate::governor::Admission;
use crate::obs::trace::TraceContext;
use crate::registry::Caller;
use crate::state::{InboxEvent, Kind, now_ms, ulid};
use crate::subagent::protocol::{TurnKind, TurnResult, TurnSpec};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};

/// The memory key prefix runtime-created workflow definitions are stored under.
const WORKFLOW_DEF_PREFIX: &str = "_workflows/";

/// One configured workflow entry as a single document to resolve — a `dir:`
/// entry becomes one per matching file.
pub(crate) struct WorkflowDocument {
    pub(crate) entry: Value,
    /// Where it came from, in the words a refusal names it by.
    pub(crate) source: String,
    /// Whether the instruction document contributed it. A document's
    /// reference (`file:`, `url:`, `uri:`, `dir:`) resolves only here, and the
    /// definition it resolves to is held to the document's grants exactly as
    /// its inline machinery was at config load.
    pub(crate) from_document: bool,
}

/// Configured workflow definitions resolved and parsed but not yet live.
/// Staging is side-effect free (it reads files, fetches and resources, and
/// installs and logs nothing), which is what lets a reload refuse a set that
/// cannot load BEFORE it applies any section: a refused reload changes
/// nothing, rather than changing everything except the workflows.
#[derive(Default)]
pub(crate) struct StagedWorkflows {
    /// The definitions, in source order.
    defs: Vec<Workflow>,
    /// name → the source that defined it first. A configured name is ONE
    /// definition: installing by name would otherwise let whichever source
    /// came later replace the other without a word — a folder's file and a
    /// document's block sharing a name, say, which no check on the entries
    /// can see before the files are read.
    defined: HashMap<String, String>,
    pub(crate) errs: Vec<String>,
}

impl StagedWorkflows {
    /// The staged definitions, in source order.
    pub(crate) fn defs(&self) -> &[Workflow] {
        &self.defs
    }
}

/// A staged set checked against the registry and servers it will run with and
/// composed with the runtime-created definitions: what `install_workflows`
/// makes live without a step left that can fail.
pub(crate) struct PreparedWorkflows {
    pub(crate) workflows: BTreeMap<String, std::sync::Arc<Workflow>>,
    defined: HashMap<String, String>,
    /// `workflow.loaded` lines, logged once the set is live.
    loaded: Vec<Value>,
    /// Stored definitions a configured name shadows: discarded once live.
    shadowed: Vec<(String, Value)>,
    /// Warnings about the set, logged once it is live.
    warnings: Vec<(&'static str, Value)>,
}

/// The role whose model an intelligence node runs on when it names none
/// (RFC 0046 §6): a `think` node thinks, a `classify`, `route` or `judge`
/// node decides, and everything that writes for the agent — `agent`,
/// `extract`, `summarize` — runs on the turn's model.
pub(crate) fn node_role(kind: &str) -> ModelRole {
    match kind {
        "think" => ModelRole::Think,
        k if crate::engine::model::DECIDING_KINDS.contains(&k) => ModelRole::Decide,
        _ => ModelRole::Turn,
    }
}

/// What a step turn does beyond its prompt: a decision to answer instead,
/// and a deciding node's `confidence`/`on_unsure`.
#[derive(Default)]
pub(crate) struct StepWork {
    pub decision: Option<Box<crate::subagent::protocol::DecisionJob>>,
    pub sure: Option<crate::subagent::protocol::Sureness>,
}

impl Runtime {
    // ---- definitions -----------------------------------------------------------

    /// The breaker policy a step actually runs under: its own `breaker:` wins;
    /// an `mcp.tool` step against a server that names a catalog `service:`
    /// otherwise inherits that entry's `breaker:` default. The admission gate
    /// and the outcome recorder both call this, so they can never disagree
    /// about which policy a given step is being judged by.
    pub(crate) fn effective_breaker(
        &self,
        step: &crate::engine::model::Step,
    ) -> Option<super::breaker::Config> {
        if let Some(cfg) = super::breaker::Config::of(step.spec.get("breaker")) {
            return Some(cfg);
        }
        if step.kind != "mcp.tool" {
            return None;
        }
        let server = step.field_str("server")?;
        let svc = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == server)
            .and_then(|s| s.service.as_ref())?;
        super::breaker::Config::of(self.settings.services.get(svc)?.breaker.as_ref())
    }

    /// Resolve a parsed definition's durability class against the store
    /// default (`store.durability.work`): an explicit `durable:` wins; absent,
    /// `ephemeral` deployments run everything memory-only.
    pub(crate) fn fill_durable_default(&self, w: &mut crate::engine::model::Workflow) {
        if w.durable.is_none() {
            w.durable = Some(self.work_durable_default());
        }
    }

    /// Load the configured workflows (inline / file / uri) plus the
    /// runtime-created ones from the store. Errors are collected rather than
    /// returned on the first failure, so one bad definition is refused with a
    /// message instead of hiding the rest.
    pub(crate) fn load_workflows(&mut self) -> Result<(), Vec<String>> {
        let mut staged = StagedWorkflows::default();
        let docs = workflow_documents(&self.settings, &mut staged.errs);
        self.stage_workflows(docs, &self.mcp, &mut staged);
        let prepared = self.prepare_workflows(staged, &self.registry, &self.skills)?;
        self.install_workflows(prepared);
        Ok(())
    }

    /// Resolve and parse workflow documents into `staged`: read each `file:`,
    /// fetch each `url:`, read each `uri:` resource through `mcp`, fold
    /// `{{config.*}}` and parse. Installs nothing and logs nothing, so a
    /// reload can stage the whole set BEFORE it applies any section and
    /// refuse with nothing changed; `prepare_workflows` checks a staged set
    /// and `install_workflows` makes it live.
    ///
    /// The connection set is a parameter because a reload reads through the
    /// servers it is about to run on — the ones it has only just dialed
    /// beside the running set — not the ones running now.
    pub(crate) fn stage_workflows(
        &self,
        docs: Vec<WorkflowDocument>,
        mcp: &BTreeMap<String, std::sync::Arc<crate::mcp::client::McpClient>>,
        staged: &mut StagedWorkflows,
    ) {
        let errs = &mut staged.errs;
        for WorkflowDocument {
            entry: doc,
            source,
            from_document,
        } in docs
        {
            // `{{config.*}}` folds in at load, in two passes: the ENTRY first —
            // so a var can sit in a `file:`, `url:` or `dir:` reference and in
            // the headers that fetch it — and the RESOLVED document after, so a
            // definition arriving from a file or URL is treated exactly like an
            // inline one. Folding here (rather than at render time) puts the
            // substituted values in the definition hash: a var change is a
            // definition change, and in-flight runs stay pinned to what they
            // started with.
            let mut doc = doc;
            // The entry pass covers only REFERENCE entries (a var in a `file:`,
            // `url:` or the headers that fetch it). An inline definition skips
            // it — the resolved pass below sees the same document, and running
            // both would report every unresolved reference twice.
            if doc.get("steps").is_none() {
                substitute_config_vars(&mut doc, &self.settings.vars, "workflow entry", errs);
            }
            let resolved = match (
                doc.get("file").and_then(Value::as_str),
                doc.get("uri").and_then(Value::as_str),
            ) {
                (Some(path), _) => match read_workflow_file(path, &doc) {
                    Ok(d) => d,
                    Err(e) => {
                        errs.push(format!("workflow file {path}: {e}"));
                        continue;
                    }
                },
                // A `url:` is fetched over HTTP(S), with operator-declared
                // headers — the shape people already have for a definitions
                // service or a raw git URL. Distinct from `uri:`, which is an
                // MCP resource: both name "somewhere else", but only one of
                // them makes the daemon dial.
                (None, None) if doc.get("url").is_some() => {
                    let url = doc["url"].as_str().unwrap_or_default().to_string();
                    match self
                        .fetch_workflow_url(&doc, &url)
                        .and_then(|d| as_definition(d, &doc))
                    {
                        Ok(d) => d,
                        // Named by scheme, host and path, like every other
                        // place a `url:` source is named: a definitions URL
                        // can carry its credential in its userinfo or query,
                        // and this line reaches the log and a reload's
                        // refusal. The fetch error is scrubbed the same way —
                        // a URL that does not parse is echoed whole, and a
                        // host the resolver names carries the userinfo.
                        Err(e) => {
                            errs.push(format!(
                                "workflow url {}: {}",
                                crate::config::settings::url_locator(&url),
                                scrub_url(&e, &url)
                            ));
                            continue;
                        }
                    }
                }
                (None, Some(uri)) => match read_resource_any(mcp, uri, &|n| {
                    self.is_company_server(n)
                }) {
                    Ok(text) => match crate::config::file::parse_document(
                        &text,
                        crate::config::file::Format::detect(Some(std::path::Path::new(uri)), &text),
                    )
                    .and_then(|d| as_definition(d, &doc))
                    {
                        Ok(d) => d,
                        Err(e) => {
                            errs.push(format!("workflow uri {uri}: {e}"));
                            continue;
                        }
                    },
                    Err(e) => {
                        errs.push(format!("workflow uri {uri}: {e}"));
                        continue;
                    }
                },
                _ => doc.clone(),
            };
            let mut resolved = resolved;
            substitute_config_vars(&mut resolved, &self.settings.vars, "workflow", errs);
            // A document's reference is the document's machinery one hop
            // away: `url:` names content the document chose, so a route it
            // defines faces the grant an inline one faced at config load.
            if from_document {
                let refused = crate::config::settings::document_route_refusals(
                    std::iter::once(&resolved),
                    &self.settings.agent.document_grants,
                    &source,
                );
                if !refused.is_empty() {
                    errs.extend(refused);
                    continue;
                }
            }
            match parse_workflow(&resolved) {
                Ok(mut w) => {
                    self.fill_durable_default(&mut w);
                    // A configured definition is the only kind that carries
                    // the instruction document's gate channels: bound to THIS
                    // definition, not to its name, so whatever later replaces
                    // it under the name carries none.
                    if let Some(ch) = self.settings.agent.document_gate_channels.get(&w.name) {
                        w.gate_channels = ch.clone();
                    }
                    // A document's definition: its runs' calls are
                    // `document_sourced` (RFC 0044 §5.12 I).
                    w.document = from_document;
                    if let Some(first) = staged.defined.get(&w.name) {
                        errs.push(crate::config::settings::duplicate_workflow(
                            &w.name, first, &source,
                        ));
                        continue;
                    }
                    staged.defined.insert(w.name.clone(), source);
                    staged.defs.push(w);
                }
                Err(e) => errs.extend(e),
            }
        }
    }

    /// Check a staged set and compose it with the runtime-created definitions
    /// from the store, against the tool registry it will run with and the
    /// servers and streams the configuration declares, and the skills
    /// catalogue. Changes nothing: the set comes back ready for
    /// `install_workflows`, which cannot fail, so a reload can check its
    /// workflows against the registry and catalogue it has only STAGED and
    /// refuse with the running ones untouched.
    pub(crate) fn prepare_workflows(
        &self,
        staged: StagedWorkflows,
        registry: &crate::registry::Registry,
        skills: &crate::context::skills::Catalogue,
    ) -> Result<PreparedWorkflows, Vec<String>> {
        let StagedWorkflows {
            defs,
            defined,
            mut errs,
        } = staged;
        let mut workflows: BTreeMap<String, std::sync::Arc<Workflow>> = BTreeMap::new();
        let mut loaded: Vec<Value> = Vec::new();
        // Stored definitions under a configured name: (store id, logged detail).
        let mut shadowed: Vec<(String, Value)> = Vec::new();
        let mut warnings: Vec<(&'static str, Value)> = Vec::new();
        // Runtime-created definitions, checked apart from the configured ones.
        let mut stored: Vec<Workflow> = Vec::new();
        for w in defs {
            loaded.push(json!({"name": w.name, "hash": &w.hash[..12], "steps": w.steps.len(), "durable": w.durable, "starts": w.start_steps().iter().map(|s| s.kind.clone()).collect::<Vec<_>>()}));
            workflows.insert(w.name.clone(), std::sync::Arc::new(w));
        }
        // Runtime-created definitions (durable under memory/_workflows/<name>).
        if let Ok(list) = self.durable.list(Kind::Memory) {
            for ks in list {
                let Some((_, id)) = crate::store::parse_key(
                    self.durable.prefix(),
                    self.durable.instance(),
                    &ks.key,
                ) else {
                    continue;
                };
                let Some(name) = id.strip_prefix(WORKFLOW_DEF_PREFIX) else {
                    continue;
                };
                // A configured name is one definition, and the configuration
                // is where it lives: a stored one of the same name (a workflow
                // created at runtime under a name the configuration has since
                // taken, or a runtime edit of a configured one from before
                // such edits were refused) does not load — and is discarded
                // once this set is accepted. Kept, it would be armed again the
                // day the configuration drops the name: removing a workflow
                // from the configuration would bring back an old runtime
                // edit of it as a live standing instruction. Said out loud,
                // because whoever wrote it would otherwise find it gone
                // without a word.
                if let Some(configured) = defined.get(name) {
                    let stored = self.durable.get(Kind::Memory, id).ok().flatten();
                    let hash = stored
                        .as_ref()
                        .and_then(|env| env.state.get("value"))
                        .and_then(|def| parse_workflow(def).ok())
                        .map(|w| w.hash[..12].to_string());
                    shadowed.push((
                        id.to_string(),
                        json!({"name": name, "configured": configured, "hash": hash,
                               "by": stored.as_ref().and_then(|env| env.state.get("by")),
                               "note": "a runtime-stored definition (workflow.create/update) of this name is discarded; the configured one loads, and while it does the name is changed only in the configuration"}),
                    ));
                    continue;
                }
                if let Ok(Some(env)) = self.durable.get(Kind::Memory, id)
                    && let Some(def) = env.state.get("value")
                {
                    // A route needs the operator's grant at every load, not
                    // only when it was written: the grant is restart-only, so
                    // a boot without it leaves out what an earlier grant let
                    // the agent store — kept in the store, and loaded again
                    // once the grant is back. The document's rule, applied
                    // the way a stored definition is held to every rule: it
                    // cannot refuse the operator's boot, so it stays out.
                    let routes = crate::config::settings::stored_route_refusals(
                        def,
                        &self.settings.agent.document_capabilities,
                    );
                    if !routes.is_empty() {
                        warnings.push((
                            "workflow.stored.invalid",
                            json!({"name": name, "errors": routes,
                                   "note": "not loaded; the stored definition is kept and loads once the operator grants what it needs"}),
                        ));
                        continue;
                    }
                    match parse_workflow(def) {
                        Ok(mut w) => {
                            self.fill_durable_default(&mut w);
                            stored.push(w);
                        }
                        Err(e) => warnings.push((
                            "workflow.stored.invalid",
                            json!({"name": name, "errors": e}),
                        )),
                    }
                }
            }
        }
        // Validate tool/server/stream references against the registry and
        // servers the set will run with. A configured definition that names
        // what they lack refuses the start or the reload — the operator wrote
        // both, and a reload that disables a tool a step calls is a mistake to
        // say now, not on the day the step runs.
        for w in workflows.values() {
            errs.extend(self.reference_errors(w, registry, skills));
        }
        // The stream-taint trifecta check over the whole configured set, now
        // that file, URL and directory definitions have resolved: `validate`
        // could only see the inline ones.
        errs.extend(crate::config::taint::refusals(
            &self.settings,
            &workflows.values().map(AsRef::as_ref).collect::<Vec<_>>(),
        ));
        // A stored one (workflow.create/update) cannot refuse anything: only
        // the operator's configuration may. Checked the same way it would
        // otherwise veto the operator — a definition the agent wrote naming
        // a tool would keep that tool enabled against every reload that
        // disables it, and a server connected against every reload that
        // removes it. It is left out of the set instead, said out loud, and
        // kept in the store: it loads again once what it names is back.
        for w in stored {
            let mut problems = self.reference_errors(&w, registry, skills);
            // …and the taint check over the set it would join: a stored
            // definition that feeds a configured consumer's stream, or reads
            // one into an agent holding sensitive + egress, stays out.
            if problems.is_empty() {
                problems = crate::config::taint::refusals(
                    &self.settings,
                    &workflows
                        .values()
                        .map(AsRef::as_ref)
                        .chain(std::iter::once(&w))
                        .collect::<Vec<_>>(),
                );
            }
            if problems.is_empty() {
                loaded.push(json!({"name": w.name, "source": "store"}));
                workflows.insert(w.name.clone(), std::sync::Arc::new(w));
            } else {
                warnings.push((
                    "workflow.stored.invalid",
                    json!({"name": w.name, "errors": problems,
                           "note": "not loaded; the stored definition is kept and loads once what it names is available"}),
                ));
            }
        }
        // Mixed durability is legal but has one sharp edge worth a loud line:
        // a DURABLE parent step waiting on a NON-durable child run resumes
        // after a restart to find the run gone (the wait fails with "run does
        // not exist"). Said at load, where the shape is still a choice.
        for w in workflows.values() {
            if w.durable == Some(false) {
                continue;
            }
            for st in w.steps.values() {
                if st.kind == "workflow"
                    && st.field_str("mode").unwrap_or("sync") != "detached"
                    && let Some(target) = st.field_str("name")
                    && workflows
                        .get(target)
                        .is_some_and(|t| t.durable == Some(false))
                {
                    warnings.push((
                        "workflow.durability_mix",
                        json!({"workflow": w.name, "step": st.id, "child": target,
                               "note": "a durable parent waits on a non-durable child; after a restart the wait fails (the child run does not survive)"}),
                    ));
                }
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(PreparedWorkflows {
            workflows,
            defined,
            loaded,
            shadowed,
            warnings,
        })
    }

    /// Re-derive the workflow tools and the read-back contracts' tags from
    /// the set installed now — and what a read-back withholds from whom.
    /// They carry the taint of the runs whose result they return, which the
    /// whole set decides — so a definition the agent writes or deletes,
    /// which is never a tool itself, can still move them.
    /// A retired definition's live runs count until the last one lands
    /// ([`Self::retired_with_live_runs`]). Logged only when something moved.
    pub(crate) fn retag_workflow_tools(&mut self) {
        let mut registry = self.registry.clone();
        let retired = self.retired_with_live_runs();
        let defs: Vec<&Workflow> = self
            .workflows
            .values()
            .map(AsRef::as_ref)
            .chain(&retired)
            .collect();
        let errs = registry.register_workflow_tools(&self.settings, &defs);
        // The tool names are the installed set's, already registered once,
        // so a collision cannot appear here; were one to, the running
        // registry stays rather than losing a tool. What a read-back
        // withholds moves with the set too, and is switched in with it.
        if !errs.is_empty() {
            return;
        }
        let tools_moved = !registry.same_tools(&self.registry);
        if tools_moved || !registry.same_withholding(&self.registry) {
            self.registry = registry;
        }
        if tools_moved {
            self.log_workflow_tools();
        }
    }

    /// The retired definitions a live run still executes against (pinned by
    /// `retire_workflow`), for the taint the read-back
    /// contracts carry: those runs' results stay readable until the last one
    /// lands, so a policy matching that taint keeps applying to reads of
    /// them. Each without its `tool:` — a retired definition registers
    /// nothing. `retire_sweep` re-derives when a pin is released.
    pub(crate) fn retired_with_live_runs(&self) -> Vec<Workflow> {
        self.pinned
            .values()
            .map(|w| Workflow {
                tool: None,
                ..(**w).clone()
            })
            .collect()
    }

    /// What `w` names that the registry it would run with, or the
    /// configuration, does not have: a tool, a mapped contract, an MCP server,
    /// a stream — or names ambiguously: a skill.
    /// One check for a configured definition, a stored one and one the agent
    /// is writing now, so none of them is held to a different line.
    pub(crate) fn reference_errors(
        &self,
        w: &Workflow,
        registry: &crate::registry::Registry,
        skills: &crate::context::skills::Catalogue,
    ) -> Vec<String> {
        let mut errs = Vec::new();
        // At any depth: a `tool` step's MCP task fields and a step's skills.
        fn walk<'a>(
            steps: impl Iterator<Item = &'a crate::engine::model::Step>,
            out: &mut Vec<&'a crate::engine::model::Step>,
        ) {
            for st in steps {
                out.push(st);
                for body in st.body.iter().chain(st.branches.values()) {
                    walk(body.steps.values(), out);
                }
            }
        }
        let mut deep = Vec::new();
        walk(w.steps.values(), &mut deep);
        for s in deep {
            // `detach` and `input` are about the MCP task a call becomes
            // (RFC 0054): a `tool` step's call is an MCP server's only on its
            // route.
            if s.kind == "tool"
                && (s.field("detach").is_some() || s.field("input").is_some())
                && let Some(n) = s.field_str("name")
                && registry.allowed(&Caller::Workflow, n)
                && !matches!(registry.route(n), Some(crate::registry::Route::Mcp { .. }))
            {
                errs.push(format!("workflow {:?} step {:?}: `detach` and `input` are about the MCP task a call becomes, and tool {n:?} is not on an MCP server's route", w.name, s.id));
            }
            // A reference two skills answer to names neither (RFC 0051 D2):
            // refused here, naming both, rather than told to the model when
            // the step runs. One that names none is told then: its server
            // may be down now, or serve it only through `skills/get`.
            for r in s
                .skills
                .iter()
                .filter(|r| !crate::engine::template::is_templated(&Value::String(r.to_string())))
            {
                if let Err(e @ crate::context::skills::LoadError::Ambiguous(..)) = skills.resolve(r)
                {
                    errs.push(format!(
                        "workflow {:?} step {:?}: skills: {e}",
                        w.name, s.id
                    ));
                }
            }
        }
        for s in w.steps.values() {
            match s.kind.as_str() {
                "tool" => {
                    let Some(n) = s.field_str("name") else {
                        continue;
                    };
                    // A workflow's tool is refused whether or not the registry
                    // checked against holds it yet. At startup and on a reload
                    // that rebuilds the registry it does not (the workflow
                    // tools are registered after this check); on one that
                    // keeps it, it does — so allowing it here would accept a
                    // configuration the next start refuses.
                    if registry
                        .get(n)
                        .is_some_and(|t| t.class == crate::registry::ToolClass::Workflow)
                    {
                        errs.push(format!("workflow {:?} step {:?}: tool {n:?} is a workflow's tool; run that workflow with a `workflow` step", w.name, s.id));
                    } else if !registry.allowed(&Caller::Workflow, n)
                        && !self.company_tool_to_come(n, registry)
                    {
                        errs.push(format!("workflow {:?} step {:?}: tool {n:?} is unknown, disabled or not granted to workflows", w.name, s.id));
                    }
                }
                k if (k.starts_with("memory.")
                    || k.starts_with("artifact.")
                    || k.starts_with("knowledge.")
                    || k.starts_with("search."))
                    && !registry.allowed(&Caller::Workflow, k) =>
                {
                    errs.push(format!("workflow {:?} step {:?}: {k} is unavailable (map it with tools.overrides or configure its server)", w.name, s.id));
                }
                _ => {}
            }
        }
        // A server or a stream the configuration does not declare, at any
        // depth — the load's own check, so a definition the runtime reads,
        // stores or the agent writes is held to the same line. A declared
        // server that is down joins when it comes up (`runtime::join`); a
        // step calling it before then fails when it runs.
        errs.extend(crate::config::settings::undeclared_references(
            &self.settings,
            w,
        ));
        // A Company's server is reached through its tools alone.
        errs.extend(crate::config::settings::companies::workflow_refusals(
            &self.settings,
            w,
        ));
        // A wait that subscribes, at any depth, names an operator grant.
        errs.extend(crate::config::settings::events::subscribe_refusals(
            &self.settings,
            w,
        ));
        // Every model a step names, at any depth, is one the catalogue has,
        // of a kind the step can use.
        errs.extend(crate::config::settings::model_refusals(&self.settings, w));
        errs
    }

    /// Make a prepared set live: it replaces the installed set whole. Nothing
    /// here can fail — every check ran in `prepare_workflows` — so a reload
    /// that got this far applies its workflows with the rest of its sections.
    /// `workflow.loaded` is logged only now: a line for a definition that a
    /// later refusal discards reads as if it went live.
    pub(crate) fn install_workflows(&mut self, prepared: PreparedWorkflows) {
        let PreparedWorkflows {
            workflows,
            defined,
            loaded,
            shadowed,
            warnings,
        } = prepared;
        self.workflows = workflows;
        for (event, detail) in warnings {
            self.log.warn(event, detail);
        }
        for l in loaded {
            self.log.info("workflow.loaded", l);
        }
        for (id, detail) in shadowed {
            let _ = self.durable.delete(Kind::Memory, &id);
            self.log.warn("workflow.stored.shadowed", detail);
        }
        // Only once the set is accepted: a refused reload keeps the running
        // set, and with it the names that set's configuration owns.
        self.configured_workflows = defined;
    }

    /// Fetch a workflow definition over HTTP(S).
    ///
    /// Startup-time and fail-closed: an unreachable definitions service is a
    /// daemon that would otherwise come up with a schedule silently missing, so
    /// it refuses to start and says which URL. That matches how a required MCP
    /// server behaves and is the safer half of the trade.
    ///
    /// Headers are operator-declared and resolve `{{secret:…}}` like every
    /// other credential, so a token never sits in the config file. The URL is
    /// SSRF-guarded like any other outbound request — an operator-chosen URL is
    /// far more trustworthy than a model-chosen one, but `allow_private` is
    /// still an explicit decision rather than an assumption.
    fn fetch_workflow_url(&self, doc: &Value, url: &str) -> Result<Value, String> {
        let headers: Vec<(String, String)> = doc
            .get("headers")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let headers =
            crate::mcp::auth::resolve_headers(&headers).map_err(|e| format!("headers: {e}"))?;
        let timeout = doc
            .get("timeout")
            .and_then(Value::as_str)
            .and_then(|t| crate::config::parse_duration(t).ok())
            .unwrap_or(std::time::Duration::from_secs(20));
        let allow_private = doc
            .get("allow_private")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = crate::runtime::http_node::fetch_text(url, &headers, timeout, allow_private)?;
        crate::config::file::parse_document(
            &text,
            crate::config::file::Format::detect(Some(std::path::Path::new(url)), &text),
        )
    }

    /// Arm start nodes. `once` fires immediately unless a live run of the
    /// workflow came back from the store (`policy: ensure`, the default), which
    /// keeps a restart from starting a second copy of work already in flight;
    /// `policy: always` fires regardless.
    pub(crate) fn arm_workflows(&mut self) {
        let names: Vec<String> = self.workflows.keys().cloned().collect();
        for name in names {
            let Some(w) = self.workflows.get(&name) else {
                continue;
            };
            if !w.armed {
                continue;
            }
            let starts: Vec<(String, String, Map<String, Value>)> = w
                .start_steps()
                .iter()
                .map(|s| (s.id.clone(), s.kind.clone(), s.spec.clone()))
                .collect();
            for (id, kind, spec) in starts {
                match kind.as_str() {
                    "once" => {
                        let policy = spec
                            .get("policy")
                            .and_then(Value::as_str)
                            .unwrap_or("ensure");
                        let live = self
                            .runs
                            .values()
                            .any(|r| r.workflow == name && !r.status.is_terminal());
                        let ever = self
                            .runs
                            .values()
                            .any(|r| r.workflow == name && r.start.node == id);
                        // A replayed (still pending) firing counts too — never fire twice.
                        let pending = self.inbox_queue.iter().any(|e| {
                            e.kind == kinds::START_FIRED
                                && e.payload["workflow"] == name.as_str()
                                && e.payload["node"] == id.as_str()
                        });
                        if policy == "ensure" && (live || ever || pending) {
                            self.log.info("start.once.skipped", json!({"workflow": name, "node": id, "live": live, "pending": pending}));
                            continue;
                        }
                        let inputs = spec.get("inputs").cloned().unwrap_or(json!({}));
                        // A `once` start is autonomous work like any other
                        // trigger, so it is attributed the same way.
                        let acting = self.settings.identity.autonomous_id().to_string();
                        // What the process was started to do: a span of the
                        // process trace in a job or under a caller's trace
                        // (`Runtime::boot_parent`), else a trace of its own.
                        let parent = self.boot_parent();
                        let _ = self.accept_event(kinds::START_FIRED, Some(acting), json!({"workflow": name, "node": id, "payload": {"fired_at": now_ms()}, "inputs": inputs}), parent.as_ref());
                    }
                    "manual" => {}
                    // Long-lived starts are armed by arm_long_lived_starts.
                    _ => {}
                }
            }
        }
    }

    /// A start event → a run. Returns `true` when the event is consumed.
    pub(crate) fn on_start_event(&mut self, ev: &InboxEvent) -> bool {
        let name = ev.payload["workflow"].as_str().unwrap_or("").to_string();
        let Some(w) = self.workflows.get(&name).cloned() else {
            self.log.in_trace(ev.trace.as_ref()).warn(
                "start.unknown_workflow",
                json!({"inbox_event": ev.id, "workflow": name}),
            );
            return true;
        };
        let node = ev.payload["node"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| default_start(&w).unwrap_or_default());
        // Concurrency: a new run must fit both the workflow's own `max_runs`
        // and the instance-wide `limits.max_runs`; whichever binds first
        // decides the `on_overflow` outcome.
        //
        // `scope: key` counts only the runs about the SAME THING, which is the
        // difference between a queue and a lock: `max_runs: 1` under
        // `scope: workflow` serialises every customer behind one run, so
        // per-entity ordering used to mean one workflow definition per entity.
        // A firing whose key did not render counts under the workflow scope —
        // sharing an "unkeyed" bucket with every other such firing would
        // silently serialise unrelated work.
        let this_key = ev.payload["key"].as_str();
        let keyed = w.concurrency.scope == crate::engine::model::ConcurrencyScope::Key
            && this_key.is_some();
        let live = self
            .runs
            .values()
            .filter(|r| r.workflow == name && !r.status.is_terminal())
            .filter(|r| !keyed || r.key.as_deref() == this_key)
            .count() as u32;
        let global_live = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .count() as u32;
        if live >= w.concurrency.max_runs
            || global_live >= self.settings.limits.max_runs.unwrap_or(8)
        {
            match w.concurrency.on_overflow {
                crate::engine::model::OnOverflow::Queue => {
                    // Keep the event pending; it is retried on a LATER tick.
                    // Nothing this tick can relieve the cap — only a live run
                    // reaching a terminal status in `schedule_runs` does, and
                    // that step has not run yet — so this must not be re-offered
                    // now. `process_inbox` drains a snapshot for exactly that
                    // reason: this push lands on the next tick's queue.
                    self.inbox_queue.push_back(ev.clone());
                    return false;
                }
                crate::engine::model::OnOverflow::Drop => {
                    // In the trace the run would have had, naming the id an
                    // answer may already have given it (a webhook's `202`).
                    self.log.in_trace(ev.trace.as_ref()).warn(
                        "run.dropped",
                        json!({"workflow": name, "run": ev.payload["run_id"].as_str(),
                               "reason": "concurrency"}),
                    );
                    return true;
                }
                crate::engine::model::OnOverflow::Replace => {
                    if let Some(oldest) = self
                        .runs
                        .values()
                        .filter(|r| r.workflow == name && !r.status.is_terminal())
                        .min_by_key(|r| r.created)
                        .map(|r| r.id.clone())
                    {
                        self.cancel_run(&oldest, "replaced by a newer run");
                    }
                }
            }
        }
        // Inputs.
        let inputs = ev.payload.get("inputs").cloned().unwrap_or(json!({}));
        if let Some(schema) = &w.inputs_schema
            && let Err(e) = crate::jsonschema::validate(schema, &inputs)
        {
            self.log.in_trace(ev.trace.as_ref()).warn(
                "run.inputs.invalid",
                json!({"workflow": name, "run": ev.payload["run_id"].as_str(), "errors": e}),
            );
            return true;
        }
        // A2A pre-generates the run id so its task can link before the run starts.
        let run_id = ev
            .payload
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}-{}", name, ulid::new()));
        // The definition this run starts under survives us durably: a restart
        // that also changed or removed the workflow still finishes this run.
        self.ensure_pin(&w);
        let mut run = RunState::new(
            &run_id,
            &w,
            Start {
                node: node.clone(),
                payload: ev.payload.get("payload").cloned().unwrap_or(Value::Null),
                ts: now_ms(),
            },
            inputs,
        );
        run.principal = ev.principal.clone();
        run.session = ev.session.clone();
        run.subagent = ev
            .payload
            .get("subagent")
            .and_then(Value::as_str)
            .map(str::to_string);
        run.by_model = ev.payload.get("by_model") == Some(&Value::Bool(true));
        run.parent = ev.payload.get("parent").cloned().filter(|p| !p.is_null());
        // What the run carries, on its own record from the start
        // (`withhold`): what its definition can be handed, and what whoever
        // started it handed it — an A2A peer, a run, a child, a conversation
        // — which the start event recorded when it was accepted.
        if let Some(t) = self.registry.withholding().run(&name) {
            run.carries.add(t.origins.iter().cloned(), || t.why.clone());
        }
        if let Some(c) = ev
            .payload
            .get("carries")
            .and_then(|c| serde_json::from_value::<super::withhold::Carries>(c.clone()).ok())
        {
            run.carries.union(&c);
        }
        run.conversation = ev
            .payload
            .get("conversation")
            .and_then(Value::as_str)
            .map(str::to_string);
        run.task = ev
            .payload
            .get("task")
            .and_then(Value::as_str)
            .map(str::to_string);
        // A run inherits the message-hop depth of whatever asked for it, so a
        // `message` inside it extends that chain rather than starting a fresh
        // one. Triggers carry nothing and so start at 0.
        run.msg_depth = ev.payload["msg_depth"].as_u64().unwrap_or(0) as u32;
        run.key = ev.payload["key"].as_str().map(str::to_string);
        // The run is the span its start event was accepted as. An event
        // written before events carried one starts a trace here.
        run.trace = Some(ev.trace.clone().unwrap_or_else(TraceContext::mint));
        // Durable before anything runs — unless the workflow opted out of the
        // class entirely (`durable: false`): a memory-only run writes nothing,
        // here or at any checkpoint.
        if run.durable
            && let Err(e) = self.durable.put(
                Kind::Run,
                &run_id,
                serde_json::to_value(&run).unwrap_or(Value::Null),
                Some(w.hash.clone()),
            )
        {
            self.log.error(
                "run.create.fail",
                json!({"workflow": name, "err": e.to_string()}),
            );
            // The event stays pending in the DURABLE inbox (only a consumed
            // start is marked done), so dropping it from the in-memory queue
            // here would make the start silently vanish until a restart
            // replays it. Requeue it like an overflow: the next tick retries.
            self.inbox_queue.push_back(ev.clone());
            return false;
        }
        run.dirty = false;
        // The actor is on the line: "every effect names the human or the
        // schedule that caused it" is only true if you can read it back.
        self.log.in_trace(run.trace.as_ref()).info(
            "run.start",
            json!({"run": run_id, "workflow": name, "node": node, "inbox_event": ev.id,
                   "acting_for": run.principal, "key": run.key}),
        );
        self.counters.runs_started += 1;
        crate::obs::metrics::record_run_started();
        if node_kind(&w, &node) == Some("once") && self.job_shape {
            self.job_runs.push(run_id.clone());
        }
        // Answer a `workflow.run` waiter that asked for the id.
        if ev.kind == kinds::WORKFLOW_RUN
            && let Some(req) = ev.payload.get("request").and_then(Value::as_object)
        {
            let target = match (
                req.get("node").and_then(Value::as_u64),
                req.get("req").and_then(Value::as_u64),
                req.get("run").and_then(Value::as_str),
                req.get("step").and_then(Value::as_str),
            ) {
                (Some(n), Some(r), _, _) => {
                    Some(Target::Child(crate::supervisor::tree::NodeId(n), r))
                }
                (None, None, Some(r), Some(s)) => Some(Target::Step(r.to_string(), s.to_string())),
                _ => None,
            };
            if let Some(t) = target {
                if req.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                    let deadline = now_ms()
                        + req
                            .get("timeout_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or(3_600_000);
                    self.push_pending(super::reactor::PendingTool {
                        target: t,
                        name: "workflow.run".into(),
                        kind: PendingKind::Run {
                            run: run_id.clone(),
                            deadline_ms: deadline,
                        },
                        started_ms: now_ms(),
                    });
                } else {
                    self.reply(
                        &t,
                        json!({"run": run_id, "status": "running", "workflow": name}),
                        false,
                    );
                }
            }
        }
        self.runs.insert(run_id, run);
        true
    }

    // ---- scheduling ------------------------------------------------------------

    /// Every tick: advance every live run.
    pub(crate) fn schedule_runs(&mut self) {
        // Finished A2A tasks age out on the tick, as runs do on their finish:
        // a `ttl` passes with no transition to notice it. Ahead of the pause,
        // because a paused instance still owes its store the bound.
        #[cfg(feature = "a2a")]
        self.sweep_terminal_tasks();
        if self.paused {
            return; // operator hold (admin.pause) — steps park until resume
        }
        // Higher-priority runs schedule first each tick, so under contention
        // (fan-out slots, per-tick capacity) their ready steps win. Stable
        // within a priority: BTreeMap order = name, then creation (ULID).
        let mut ids: Vec<(std::cmp::Reverse<crate::engine::model::Priority>, String)> = self
            .runs
            .iter()
            .filter(|(_, r)| !r.status.is_terminal() && r.status != RunStatus::Paused)
            .map(|(id, r)| {
                let pr = self
                    .workflows
                    .get(&r.workflow)
                    .map(|w| w.priority)
                    .unwrap_or_default();
                (std::cmp::Reverse(pr), id.clone())
            })
            .collect();
        ids.sort_by_key(|a| a.0);
        for (_, id) in ids {
            self.schedule_run(&id);
        }
    }

    /// [`Self::definition_for_run`] addressed by `(name, hash)` — for callers
    /// that already hold the run record.
    pub(crate) fn definition_for_run_ref(&self, workflow: &str, hash: &str) -> Option<&Workflow> {
        if let Some(w) = self.workflows.get(workflow)
            && w.hash == hash
        {
            return Some(w.as_ref());
        }
        self.pinned.get(hash).map(|w| w.as_ref())
    }

    /// The definition a run executes against: the one it started with,
    /// identified by hash — the current definition while it is unchanged, else
    /// the copy a reload pinned. A restored run whose definition changed
    /// underneath it matches neither, and `resume_policy: refuse` then stops
    /// it rather than silently running it against a different graph.
    pub(crate) fn definition_for_run(&self, run_id: &str) -> Option<std::sync::Arc<Workflow>> {
        let run = self.runs.get(run_id)?;
        if let Some(w) = self.workflows.get(&run.workflow)
            && w.hash == run.workflow_hash
        {
            // An Arc clone: a refcount bump, not a graph copy — this is on
            // the per-step hot path (measured ~10% of a chain's cycles as a
            // deep clone).
            return Some(w.clone());
        }
        self.pinned.get(&run.workflow_hash).cloned()
    }

    fn schedule_run(&mut self, run_id: &str) {
        let Some(wf) = self.definition_for_run(run_id) else {
            // The definition vanished or changed (hash mismatch): refuse to
            // continue the run (`resume_policy: refuse`).
            let (name, hash) = self
                .runs
                .get(run_id)
                .map(|r| (r.workflow.clone(), r.workflow_hash.clone()))
                .unwrap_or_default();
            let reason = if self.workflows.contains_key(&name) {
                format!(
                    "workflow {name:?} definition changed (run pinned to hash {}); resume_policy refuse",
                    &hash[..hash.len().min(12)]
                )
            } else {
                format!("workflow {name:?} definition is gone")
            };
            self.run_log(run_id)
                .warn("run.refused", json!({"run": run_id, "reason": reason}));
            if let Some(r) = self.runs.get_mut(run_id) {
                r.finish(RunStatus::Refused, None, Some(reason));
            }
            self.on_run_terminal(run_id);
            return;
        };
        if let Some(r) = self.runs.get(run_id)
            && run::deadline_passed(r)
        {
            self.run_log(run_id)
                .warn("run.deadline", json!({"run": run_id}));
            self.cancel_children_of_run(run_id, "run deadline");
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some("deadline exceeded".into()),
            );
            self.on_run_terminal(run_id);
            return;
        }
        // A workflow that declares no budget inherits the instance's
        // `limits.run.*`, so a definition silent about limits is still bounded.
        // An unbounded run is the one shape where a single mistake can spend
        // the whole day's tokens, and the instance-wide knob exists precisely
        // to cap that.
        let step_cap = wf.limits.steps.or(self.settings.limits.run.steps);
        let token_cap = wf.limits.tokens.or(self.settings.limits.run.tokens);
        if let Some(cap) = step_cap
            && self.runs.get(run_id).is_some_and(|r| r.steps_run >= cap)
        {
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some(format!("exhausted steps: limit {cap}")),
            );
            self.on_run_terminal(run_id);
            return;
        }
        if let Some(cap) = token_cap
            && self.runs.get(run_id).is_some_and(|r| r.tokens >= cap)
        {
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some(format!("exhausted tokens: limits.tokens = {cap}")),
            );
            self.on_run_terminal(run_id);
            return;
        }
        let data = self.run_data(run_id);
        let next = {
            let run = self.runs.get_mut(run_id).expect("present");
            run::schedule(&wf, run, &data)
        };
        // Nested parents in flight advance every tick (rate pacing, timeouts,
        // fresh iterations).
        let nested: Vec<String> = self
            .runs
            .get(run_id)
            .map(|r| {
                r.steps
                    .iter()
                    .filter(|(_, st)| {
                        st.status == StepStatus::Running
                            && st.wait.as_ref().is_some_and(|w| {
                                matches!(
                                    w["kind"].as_str(),
                                    Some("foreach")
                                        | Some("batch")
                                        | Some("iterate")
                                        | Some("parallel")
                                        | Some("race")
                                        | Some("subgraph")
                                )
                            })
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in nested {
            self.nested_advance(run_id, &id);
        }
        // Draining stops starting new steps — with one deliberate exception:
        // a workflow that declares a `lifecycle.shutdown` start exists to run
        // DURING the drain (deregister the webhook, flush the summary), and
        // the drain gate waits for it. Everything else parks where it is,
        // checkpointed, and resumes next life. Asked before each step: one
        // that starts may begin the drain (a lifetime ceiling it exhausts).
        match next {
            Ok(Next::Ready(steps)) => {
                for s in steps {
                    if self.drain_holds(&wf) {
                        return;
                    }
                    // A step that finished inline may have driven the run on,
                    // and that pass may already have run the rest of this list.
                    if !self.still_pending(run_id, &s) {
                        continue;
                    }
                    self.execute_step(run_id, &s);
                }
            }
            Ok(Next::Waiting) | Ok(Next::Terminal) => {}
            Ok(Next::Stalled) => {
                // "No ready step" is a symptom, not a diagnosis. Almost always
                // something upstream failed and its dependents could never
                // become ready — so name the first failed ancestor rather than
                // leaving whoever reads this to walk the graph themselves.
                let culprit = self.first_failed_step(run_id);
                let why = match &culprit {
                    Some((sid, err)) => format!(
                        "no ready step and no finish reached — step {sid:?} failed first: {err}"
                    ),
                    None => "no ready step and no finish reached".to_string(),
                };
                self.run_log(run_id).warn(
                    "run.stalled",
                    json!({"run": run_id, "blocked_by": culprit.as_ref().map(|(s, _)| s)}),
                );
                // A stall caused by a failure is a FAILED run, not a stalled
                // one: the distinction matters to an exit code and to a caller.
                let status = if culprit.is_some() {
                    RunStatus::Failed
                } else {
                    RunStatus::Stalled
                };
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .finish(status, None, Some(why));
                self.on_run_terminal(run_id);
            }
            Err(e) => {
                self.runs.get_mut(run_id).expect("present").finish(
                    RunStatus::Failed,
                    None,
                    Some(e),
                );
                self.on_run_terminal(run_id);
            }
        }
    }

    /// Refuse a definition-mutating tool when `security.workflows.immutable`.
    ///
    /// Applies to everyone — the model, a subagent, an operator over A2A —
    /// because the point is that definitions are reviewed before they ship, and
    /// a lock one caller can talk its way past is not a lock. Says how to change
    /// them properly rather than only saying no.
    fn workflows_locked(&self, tool: &str) -> Option<super::tools::ToolOutcome> {
        if !self.settings.security.workflows.immutable {
            return None;
        }
        self.log.warn("workflow.locked", json!({"tool": tool}));
        Some(super::tools::ToolOutcome::Ready(
            json!({"error": format!(
                "{tool}: workflow definitions are immutable \
                 (security.workflows.immutable) — edit the config, file or \
                 directory they load from and reload"
            )}),
            true,
        ))
    }

    /// Refuse a definition-mutating tool on a name the configuration defines.
    ///
    /// The configuration is where that name lives: whatever the tool wrote,
    /// the next restart or workflow reload loads the configured definition in
    /// its place (a stored one is shadowed, a deleted one comes back). A
    /// success that is quietly undone later is worse than a refusal now, so
    /// the refusal names the source to change instead.
    fn workflow_configured(&self, tool: &str, name: &str) -> Option<super::tools::ToolOutcome> {
        let source = self.configured_workflows.get(name)?;
        self.log.warn(
            "workflow.refused",
            json!({"tool": tool, "name": name, "configured": source}),
        );
        Some(super::tools::ToolOutcome::Ready(
            json!({"error": format!(
                "{tool}: workflow {name:?} is defined in the configuration \
                 ({source}) — change it there and reload"
            )}),
            true,
        ))
    }

    /// The earliest step of a run that failed, with its error.
    ///
    /// This is what explains a stall: a run with no ready step is usually a run
    /// whose dependency chain is blocked behind a failure that was routed away
    /// from `on_error: fail`, and that failure is what a person needs to see.
    fn first_failed_step(&self, run_id: &str) -> Option<(String, String)> {
        let run = self.runs.get(run_id)?;
        run.steps
            .iter()
            .filter(|(_, st)| {
                matches!(
                    st.status,
                    StepStatus::Failed | StepStatus::Timeout | StepStatus::Cancelled
                )
            })
            .min_by_key(|(_, st)| st.finished.unwrap_or(u64::MAX))
            .map(|(id, st)| {
                (
                    id.clone(),
                    st.error
                        .clone()
                        .unwrap_or_else(|| "no error recorded".into()),
                )
            })
    }

    /// The template data a run's specs render against: the `env` view, the
    /// run's own fields, and `memory` as a read-through `{key: value}` map
    /// holding exactly the `memory.<key>` references the definition names.
    pub(crate) fn run_data(&mut self, run_id: &str) -> template::Data {
        let env = env_view(
            &self.instance,
            run_id,
            Some(&self.instruction.text),
            self.settings.agent.prompt.as_deref(),
        );
        // Memory read-through: resolve every `memory.<key>` the definition
        // names. The key scan walks the whole definition, so it is memoized
        // per content hash — this runs per STEP, and re-walking an unchanged
        // definition on every one of them lands squarely on the hot path.
        let mut memory = Map::new();
        let hash = self
            .runs
            .get(run_id)
            .map(|r| r.workflow_hash.clone())
            .unwrap_or_default();
        if !hash.is_empty() {
            if !self.memory_keys.contains_key(&hash) {
                let mut keys: Vec<String> = Vec::new();
                if let Some(wf) = self.definition_for_run(run_id) {
                    for s in wf.steps.values() {
                        for (_, v) in &s.spec {
                            collect_memory_keys(v, &mut keys);
                        }
                        if let Some(w) = &s.when {
                            collect_memory_keys(&Value::String(w.clone()), &mut keys);
                        }
                    }
                }
                self.memory_keys.insert(hash.clone(), keys);
            }
            let mut held = crate::engine::run::Carries::default();
            for k in self.memory_keys.get(&hash).cloned().unwrap_or_default() {
                if let Ok((v, c)) = self.memory.get_carrying(&self.durable, &k)
                    && v["found"] == json!(true)
                {
                    memory.insert(k, v["value"].clone());
                    held.union(&c);
                }
            }
            // What the entries a step's templates read carried, its run
            // carries (RFC 0044 §5.13).
            if !held.is_empty() {
                self.hand_run(run_id, &held);
            }
        }
        let mut data = self
            .runs
            .get(run_id)
            .map(|r| r.data(env, Value::Object(memory)))
            .unwrap_or_default();
        // Artifact-backed values (`{"$artifact": id}`) dereference transparently
        // so a template sees the content, not the reference — a step never has
        // to know whether an upstream output was spilled to an artifact. What
        // each artifact's maker carried, the run now does.
        let mut held = super::withhold::Carries::default();
        for key in ["steps", "vars", "inputs"] {
            if let Some(v) = data.get_mut(key) {
                self.deref_artifacts(v, &mut held);
            }
        }
        if !held.is_empty() {
            self.hand_run(run_id, &held);
        }
        data
    }

    /// Replace `{"$artifact": id, …}` objects with the artifact's content,
    /// adding to `held` what each one carries.
    pub(crate) fn deref_artifacts(&self, v: &mut Value, held: &mut super::withhold::Carries) {
        match v {
            Value::Object(o) => {
                if let Some(id) = o.get("$artifact").and_then(Value::as_str) {
                    if let Some(a) = self.artifacts.get(id) {
                        held.union(&a.carries);
                        *v = a.content.clone();
                    }
                    return;
                }
                for x in o.values_mut() {
                    self.deref_artifacts(x, held);
                }
            }
            Value::Array(a) => {
                for x in a.iter_mut() {
                    self.deref_artifacts(x, held);
                }
            }
            _ => {}
        }
    }

    // ---- execution -------------------------------------------------------------

    /// Execute one ready step of a run.
    pub(crate) fn execute_step_pub(&mut self, run_id: &str, step_id: &str) {
        self.execute_step(run_id, step_id)
    }

    fn execute_step(&mut self, run_id: &str, step_id: &str) {
        if self.runs.get(run_id).is_none_or(|r| r.status.is_terminal()) {
            return;
        }
        let Some(wf) = self
            .runs
            .get(run_id)
            .and_then(|_| self.definition_for_run(run_id))
        else {
            return;
        };
        let Some((step, scope)) = self
            .runs
            .get(run_id)
            .and_then(|r| self.resolve_step(&wf, r, step_id))
        else {
            return;
        };
        // Outbound throttling (`rate:` on a remote-effect kind): consulted
        // BEFORE `begin_step`, because a parked step has not attempted
        // anything — waiting for a token must consume neither an attempt nor
        // a retry. On an empty bucket the step suspends on a durable timer one
        // token-interval out and re-enters this gate when it fires.
        if matches!(
            step.kind.as_str(),
            "http" | "mcp.tool" | "a2a.send" | "a2a.delegate"
        ) && let Some(rate) = step.spec.get("rate").and_then(Value::as_str)
            && let Ok((burst, secs)) = crate::supervisor::tree::parse_rate(rate)
        {
            let workflow = self
                .runs
                .get(run_id)
                .map(|r| r.workflow.clone())
                .unwrap_or_default();
            let key = super::breaker::key(&workflow, step_id);
            let (bucket, window_s, b) = self.step_rates.entry(key.clone()).or_insert_with(|| {
                (
                    crate::supervisor::tree::TokenBucket::new(burst, burst as f64 / secs),
                    secs,
                    burst,
                )
            });
            if !bucket.try_take() {
                // One token-interval, floored so a tight rate still parks
                // meaningfully rather than hot-looping the scheduler.
                let wait = ((*window_s * 1000.0) / (*b).max(1) as f64).max(20.0) as u64;
                match self.timers.arm(
                    &self.durable,
                    now_ms() + wait,
                    json!({"kind": "step_budget", "run": run_id, "step": step_id}),
                    Value::Null,
                ) {
                    Ok(id) => {
                        self.run_log(run_id).info(
                            "step.rate_wait",
                            json!({"run": run_id, "step": step_id, "rate": rate, "wait_ms": wait}),
                        );
                        self.runs
                            .get_mut(run_id)
                            .expect("present")
                            .suspend_step(step_id, json!({"kind": "rate_wait", "timer": id}));
                        self.checkpoint(false);
                        return;
                    }
                    Err(e) => {
                        // A store that cannot arm the wait must not turn a
                        // throttle into a hot loop; proceed unthrottled and say so.
                        self.run_log(run_id).warn(
                            "step.rate_wait_fail",
                            json!({"run": run_id, "step": step_id, "err": e.to_string()}),
                        );
                    }
                }
            }
        }
        let attempt = self
            .runs
            .get_mut(run_id)
            .expect("present")
            .begin_step(step_id);
        // A breakpoint set with `workflow.pause {before_step}` stops here — the
        // step has not begun, so the run can be inspected in the state it is in
        // rather than one effect later.
        if self
            .runs
            .get(run_id)
            .and_then(|r| r.break_before.as_deref())
            == Some(step_id)
        {
            if let Some(r) = self.runs.get_mut(run_id) {
                r.status = RunStatus::Paused;
                r.break_before = None;
                r.dirty = true;
                r.steps.entry(step_id.to_string()).or_default().status = StepStatus::Pending;
            }
            self.run_log(run_id)
                .info("run.paused", json!({"run": run_id, "before_step": step_id}));
            self.checkpoint(true);
            return;
        }
        // Mark the step durably `running` BEFORE its effect leaves the process,
        // so a crash replays an effect that may already have happened rather
        // than losing one entirely. A pure data step has no effect to guard —
        // a crash replays it deterministically from the last checkpoint — so an
        // inline chain batches into the tick's single checkpoint instead of
        // paying a serialize+write per step.
        crate::state::kill_point("step.running");
        if !crate::engine::model::pure_data_kind(&step.kind) {
            self.checkpoint(false);
        }
        self.step_log(run_id, step_id).info(
            "step.start",
            json!({"run": run_id, "step": step_id, "kind": step.kind, "attempt": attempt}),
        );
        // One feed event per step transition: run-level counts alone ("3 done,
        // 1 running") tell a display client that a run is moving but never WHAT
        // is moving. Operator-scoped, because a step id and its kind describe
        // the workflow's internals. The feed is the A2A interface surface, so
        // without that feature there is nothing to push to.
        #[cfg(feature = "a2a")]
        self.feed_push(
            "step",
            crate::runtime::a2a_server::FeedVis::Operator,
            json!({"run": run_id, "step": step_id, "kind": step.kind,
                   "phase": "start", "attempt": attempt,
                   "trace_id": self.run_trace(run_id).map(|t| t.trace_id)}),
        );
        let mut data = match &scope {
            Some(sc) => self.scoped_data(run_id, sc),
            None => self.run_data(run_id),
        };
        // Step-scoped env: the identity a RETRY of this step shares. Anything a
        // template derives from these is stable across attempts — which is what
        // makes `env.idempotency_key` an idempotency key and not a fresh id per
        // try. (`env.ts` already exists for when a per-attempt value is what
        // you want; the two must not be confused.)
        if let Some(env) = data.get_mut("env") {
            env["step"] = json!(step_id);
            env["attempt"] = json!(attempt);
            env["idempotency_key"] = json!(crate::engine::run::idempotency_key(run_id, step_id));
        }
        let spec = match render_spec(&step, &data) {
            Ok(s) => s,
            Err(e) => {
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        };
        let step_caller = ToolCaller {
            run: Some(run_id.to_string()),
            step: Some(step_id.to_string()),
            req: attempt as u64,
            principal: self.runs.get(run_id).and_then(|r| r.principal.clone()),
            ctx: self.runs.get(run_id).and_then(|r| r.conversation.clone()),
            msg_depth: self.runs.get(run_id).map(|r| r.msg_depth).unwrap_or(0),
            session: self.runs.get(run_id).and_then(|r| r.session.clone()),
            ..Default::default()
        };
        // `cache {key, ttl}`: a fresh memoized output skips the effect.
        let cache_key = match self.cache_lookup(&step, &spec, &data) {
            Some((_key, Some((hit, carried)))) => {
                self.run_log(run_id)
                    .info("step.cache_hit", json!({"run": run_id, "step": step_id}));
                // What the run that produced it carried, this one now does,
                // before a step after it reads the output.
                self.hand_run(run_id, &carried);
                self.finish_step(run_id, step_id, StepStatus::Done, Some(hit), None, 0);
                return;
            }
            Some((key, None)) => Some(key),
            None => None,
        };
        if let Some(k) = cache_key
            && let Some(st) = self
                .runs
                .get_mut(run_id)
                .and_then(|r| r.steps.get_mut(step_id))
        {
            st.cache_key = Some(k);
        }
        // A run acting with a person's grant does not reach agentd's own,
        // broader authority at an agent-mode server tagged `sensitive` or
        // `egress` the person's template does not list (RFC 0044 §5.3), and
        // its every step there is recorded as an agent-mode call made from
        // delegated work.
        #[cfg(feature = "xaa")]
        if matches!(step.kind.as_str(), "mcp.tool" | "mcp.resource")
            && !self
                .consented_steps
                .contains_key(&format!("{run_id}/{step_id}"))
            && let Some(server) = spec.get("server").and_then(Value::as_str)
            && !self.is_delegated_server(server)
            && !self.is_company_server(server)
        {
            let tool = spec.get("tool").and_then(Value::as_str);
            let name = tool.and_then(|t| {
                self.registry
                    .iter()
                    .find(|d| {
                        matches!(&d.imp, crate::registry::Impl::Mcp { server: s, tool: n }
                            if s == server && n == t)
                    })
                    .map(|d| d.name.clone())
            });
            let what = format!("{server}.{}", tool.unwrap_or(step.kind.as_str()));
            if let Some(e) = self.mode_mixing(&step_caller, server, name.as_deref(), &what) {
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        }
        // A step whose own effect would send a run's delegated data off the
        // box waits for the person (RFC 0044 §5.13 item 6); approved, it runs
        // with the spec they saw.
        #[cfg(feature = "xaa")]
        let spec = match self.approved_spec(run_id, step_id) {
            Some(approved) => approved,
            None if self.step_consent(run_id, step_id, &step.kind, &spec, &step_caller) => {
                return;
            }
            None => spec,
        };
        // The circuit breaker (`breaker:` on a remote-effect kind): consulted
        // BEFORE the effect is dispatched, on the loop, so an open circuit
        // costs a map lookup instead of a connection + timeout. A fast-fail is
        // an ordinary step failure carrying `breaker::OPEN_ERR` — retry and
        // on_error compose with it; the recorder in `finish_step` skips it.
        if matches!(
            step.kind.as_str(),
            "http" | "mcp.tool" | "a2a.send" | "a2a.delegate"
        ) && let Some(cfg) = self.effective_breaker(&step)
        {
            let workflow = self
                .runs
                .get(run_id)
                .map(|r| r.workflow.clone())
                .unwrap_or_default();
            let key = super::breaker::key(&workflow, step_id);
            let mut st = self
                .durable
                .manifest()
                .breakers
                .get(&key)
                .cloned()
                .unwrap_or_else(|| json!({}));
            match super::breaker::gate(&mut st, cfg, now_ms()) {
                super::breaker::Gate::Proceed => {}
                super::breaker::Gate::Probe => {
                    // This attempt claimed the half-open probe slot; the claim
                    // is durable so a concurrent run (or a restart) sees it.
                    self.durable.manifest_update(|m| {
                        m.breakers.insert(key.clone(), st);
                    });
                    self.run_log(run_id)
                        .info("breaker.probe", json!({"breaker": key, "run": run_id}));
                }
                super::breaker::Gate::FastFail { retry_in_ms } => {
                    self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!(
                            "{} — failing fast; next probe in {}ms",
                            super::breaker::OPEN_ERR,
                            retry_in_ms
                        )),
                        0,
                    );
                    return;
                }
            }
        }
        match step.kind.as_str() {
            "checkpoint" => {
                // Documented as "force a durable checkpoint here rather than at
                // the next natural boundary", and implemented as an alias for
                // `noop` — so the one step whose entire purpose is to write did
                // not write. `true` forces the write rather than letting the
                // policy decide.
                self.checkpoint(true);
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    Some(Value::Null),
                    None,
                    0,
                );
            }
            "noop" => self.finish_step(
                run_id,
                step_id,
                StepStatus::Done,
                Some(Value::Null),
                None,
                0,
            ),
            "assign" | "transform" => {
                let value = spec.get("value").cloned().unwrap_or(Value::Null);
                let key = spec
                    .get("writes")
                    .and_then(Value::as_str)
                    .unwrap_or(step_id)
                    .to_string();
                // A declared `state` key carries a schema; a write that breaks
                // it fails the step where the bad value is produced, rather
                // than three steps later where a template reads a shape nobody
                // expected. This is the whole reason to declare state.
                if let Some(schema) = self
                    .definition_for_run(run_id)
                    .and_then(|wf| wf.state.get(&key).and_then(|d| d.schema.clone()))
                    && let Err(errs) = crate::jsonschema::validate(&schema, &value)
                {
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!(
                            "assign: value does not match the schema declared for state \
                             {key:?}: {}",
                            errs.join("; ")
                        )),
                        0,
                    );
                    return;
                }
                let mode = spec
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("overwrite")
                    .to_string();
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .write_var(&key, value.clone(), &mode);
                self.finish_step(run_id, step_id, StepStatus::Done, Some(value), None, 0);
            }
            "template" => {
                let out = spec
                    .get("text")
                    .cloned()
                    .or_else(|| spec.get("value").cloned())
                    .unwrap_or(Value::String(String::new()));
                self.finish_step(run_id, step_id, StepStatus::Done, Some(out), None, 0);
            }
            "validate" => {
                let value = spec.get("value").cloned().unwrap_or(Value::Null);
                let schema = spec.get("schema").cloned().unwrap_or(json!({}));
                match crate::jsonschema::validate(&schema, &value) {
                    Ok(()) => {
                        self.finish_step(run_id, step_id, StepStatus::Done, Some(value), None, 0)
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(value),
                        Some(format!(
                            "validation failed: {}",
                            crate::jsonschema::explain(&e)
                        )),
                        0,
                    ),
                }
            }
            "assert" => {
                let cond = step
                    .field_str("condition")
                    .unwrap_or("false")
                    .trim()
                    .trim_start_matches("CEL:")
                    .trim()
                    .to_string();
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                match crate::cel::eval_bool(&cond, &vars) {
                    Ok(true) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Done,
                        Some(json!(true)),
                        None,
                        0,
                    ),
                    Ok(false) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(json!(false)),
                        Some(
                            spec.get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("assertion failed: {cond}")),
                        ),
                        0,
                    ),
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("assert: {e}")),
                        0,
                    ),
                }
            }
            "fail" => {
                let msg = spec
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("deliberate failure")
                    .to_string();
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    spec.get("code").cloned(),
                    Some(msg),
                    0,
                );
            }
            "emit" => {
                // With `stream:` this publishes an event to that stream;
                // without one it emits a note/audit record. One step name,
                // addressed by which fields are present.
                if let Some(stream) = spec.get("stream").and_then(Value::as_str) {
                    let stream = stream.to_string();
                    let subject = spec
                        .get("subject")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let correlation = spec
                        .get("correlation")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let data = spec.get("data").cloned().unwrap_or(Value::Null);
                    // The event id IS the step's derived idempotency key: a
                    // crash-replayed emit appends a second copy under the same
                    // id, and consumers drop it from their recent-id ring, so
                    // delivery is at-least-once without a duplicate surviving.
                    let id = crate::engine::run::idempotency_key(run_id, step_id);
                    let source = self
                        .runs
                        .get(run_id)
                        .map(|r| r.workflow.clone())
                        .unwrap_or_default();
                    // What this run carries goes on the event, for whatever
                    // consumes it — and so does the span it was emitted in, a
                    // child of the step's: a `stream` start's run continues it.
                    let carries = self.run_carries(run_id);
                    let emitted = self.step_trace(run_id, step_id).map(|t| t.child());
                    let emit_start = crate::obs::otel::now_unix_nanos();
                    match self
                        .append_carrying(
                            &stream,
                            &subject,
                            correlation.as_deref(),
                            data,
                            &id,
                            &source,
                            Some(&carries),
                            emitted.as_ref(),
                        )
                        .map_err(|e| e.to_string())
                    {
                        Ok(seq) => {
                            self.log.in_trace(emitted.as_ref()).info(
                                "stream.emit",
                                json!({"run": run_id, "step": step_id, "stream": stream,
                                       "subject": subject, "seq": seq}),
                            );
                            // The emit is the event's PRODUCER span: the run a
                            // consumer starts for it is its child.
                            crate::obs::otel::span(
                                emitted.as_ref(),
                                &format!("emit {stream}"),
                                crate::obs::otel::Kind::Producer,
                                emit_start,
                            )
                            .attr("messaging.operation.type", "send")
                            .attr("messaging.destination.name", &stream)
                            .attr("agentd.stream.seq", seq)
                            .end(true);
                            // `forward: {webhook: URL}` — push the event out as
                            // it appends (RFC 0035 §5). The DURABLE COPY is the
                            // source of truth and the push is only the
                            // notification, so a failed forward is logged and
                            // does not fail the step: the event is on the
                            // stream either way, and a consumer that missed the
                            // push still gets it from the offset.
                            let fwd = crate::runtime::streams::Forwarded {
                                stream: &stream,
                                subject: &subject,
                                id: &id,
                                seq,
                                run_id,
                                step_id,
                                trace: emitted.as_ref(),
                            };
                            if let Some(url) = spec
                                .get("forward")
                                .and_then(|f| f.get("webhook"))
                                .and_then(Value::as_str)
                            {
                                // Same posture as the `http` node: reaching a
                                // private/loopback address is a per-node
                                // opt-in, never a default.
                                let allow_private = spec
                                    .get("forward")
                                    .and_then(|f| f.get("allow_private"))
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                self.forward_event(url, &fwd, allow_private);
                            }
                            #[cfg(feature = "a2a")]
                            if let Some(peer) = spec
                                .get("forward")
                                .and_then(|f| f.get("peer"))
                                .and_then(Value::as_str)
                            {
                                let data = spec.get("data").cloned().unwrap_or(Value::Null);
                                self.forward_event_peer(peer, &fwd, &data);
                            }
                            self.finish_step(
                                run_id,
                                step_id,
                                StepStatus::Done,
                                Some(json!({"id": id, "seq": seq, "stream": stream,
                                           "subject": subject})),
                                None,
                                0,
                            );
                        }
                        Err(e) => {
                            self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0)
                        }
                    }
                    return;
                }
                if let Some(n) = spec.get("note").and_then(Value::as_str) {
                    // The note is rendered from the run's own data, so it is
                    // the run's text in the root transcript: a read-back,
                    // withheld like the note a finished run leaves.
                    let n = self.withhold_text(
                        &super::withhold::Reader::Conversation(Some(
                            crate::context::ROOT.to_string(),
                        )),
                        super::withhold::Source::Run(run_id),
                        n.to_string(),
                        "emit note",
                    );
                    self.note_root(format!("run {run_id}: {n}"));
                }
                if let Some(a) = spec.get("audit") {
                    self.run_log(run_id).info(
                        "audit.emit",
                        json!({"run": run_id, "step": step_id, "audit": a}),
                    );
                }
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    spec.get("value").cloned().or(Some(Value::Null)),
                    None,
                    0,
                );
            }
            "finish" => {
                let status = match spec
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                {
                    "completed" => RunStatus::Completed,
                    "refused" => RunStatus::Refused,
                    "cancelled" => RunStatus::Cancelled,
                    _ => RunStatus::Failed,
                };
                let output = spec.get("output").cloned();
                // `outputs.schema` was checked for well-formedness at parse time
                // and then never applied — a workflow could declare the shape of
                // its result and return anything at all. Enforce it here, where
                // the result actually exists. A completed run whose output does
                // not match what it promised is a FAILED run: a caller reading
                // the declared shape is the whole reason to declare one.
                if matches!(status, RunStatus::Completed)
                    && let Some(schema) = self
                        .definition_for_run(run_id)
                        .and_then(|wf| wf.outputs_schema.clone())
                {
                    let value = output.clone().unwrap_or(Value::Null);
                    if let Err(errs) = crate::jsonschema::validate(&schema, &value) {
                        self.finish_step_pub(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!(
                                "finish: output does not match the workflow's declared \
                                 outputs.schema: {}",
                                errs.join("; ")
                            )),
                            0,
                        );
                        return;
                    }
                }
                let reason = spec
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.runs.get_mut(run_id).expect("present").end_step(
                    step_id,
                    StepStatus::Done,
                    output.clone(),
                    None,
                );
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .finish(status, output, reason);
                self.on_run_terminal(run_id);
            }
            "sleep" => {
                let ms = spec
                    .get("duration")
                    .map(crate::engine::model::duration_ms)
                    .unwrap_or(Ok(0))
                    .unwrap_or(0);
                match self.timers.arm(
                    &self.durable,
                    now_ms() + ms,
                    json!({"kind": "step", "run": run_id, "step": step_id}),
                    json!({"slept_ms": ms}),
                ) {
                    Ok(id) => {
                        self.runs.get_mut(run_id).expect("present").suspend_step(
                            step_id,
                            json!({"kind": "sleep", "timer": id, "deadline_ms": now_ms() + ms}),
                        );
                        self.checkpoint(false);
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("sleep: {e}")),
                        0,
                    ),
                }
            }
            "tool" => {
                let name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = spec.get("args").cloned().unwrap_or(json!({}));
                self.step_tool_call(run_id, step_id, &step_caller, &name, args);
            }
            // A Company's API, called as `company.api` (RFC 0047 §5.5.8).
            "http"
                if spec
                    .get("auth")
                    .and_then(|a| a.get("kind"))
                    .and_then(Value::as_str)
                    == Some("company") =>
            {
                #[cfg(feature = "companies")]
                self.step_http_company(run_id, step_id, &step_caller, &spec);
                #[cfg(not(feature = "companies"))]
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some("http: auth kind company needs the `companies` feature".into()),
                    0,
                );
            }
            "http" => self.step_http(run_id, step_id, &spec),
            k if k.starts_with("memory.")
                || k.starts_with("artifact.")
                || k.starts_with("knowledge.")
                || k.starts_with("search.") =>
            {
                let mut args = spec.clone();
                // `ttl` etc. pass through as-is; the contract validates.
                args.retain(|_, v| !v.is_null());
                self.step_tool_call(run_id, step_id, &step_caller, k, Value::Object(args));
            }
            "mcp.tool" => {
                let server = spec
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = spec
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = spec.get("args").cloned().unwrap_or(json!({}));
                // A templated name the definition check could not see.
                if self.is_company_server(&server) {
                    let e = crate::config::settings::companies::direct_use_refused(&server);
                    self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                    return;
                }
                if let Some(e) = self.browser_refused(&server, &args) {
                    self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                    return;
                }
                // A delegated server's call goes the delegated way, as its
                // tool (RFC 0044): the person's consent, a mint, the
                // supervisor's pool — never a connection of the loop's.
                #[cfg(feature = "xaa")]
                if self.is_delegated_server(&server) {
                    let name = self
                        .registry
                        .iter()
                        .find(|t| {
                            matches!(&t.imp, crate::registry::Impl::Mcp { server: s, tool: n }
                                if *s == server && *n == tool)
                        })
                        .map(|t| t.name.clone());
                    // Who the work is for decides before what the catalogue
                    // holds: work for nobody's delegate is refused as that.
                    let admitted = self
                        .delegation
                        .as_ref()
                        .map(|d| self.delegation_admit(d, &step_caller, &server).map(|_| ()))
                        .unwrap_or(Ok(()));
                    match (name, admitted) {
                        (_, Err(e)) => self.finish_step(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!("{server}.{tool}: {e}")),
                            0,
                        ),
                        (Some(name), Ok(())) => {
                            self.step_tool_call(run_id, step_id, &step_caller, &name, args)
                        }
                        (None, Ok(())) => self.finish_step(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!(
                                "{server}.{tool} is not in the delegated server's tool catalogue: the person enrols (`agentd --login idp:<name>`), or the operator pins it (auth.tools_from: file)"
                            )),
                            0,
                        ),
                    }
                    return;
                }
                // Pace calls toward a rated catalog service. A dry bucket fails
                // the step — a refusal the workflow's `retry:` can absorb —
                // rather than blocking the single-writer loop.
                if let Err(e) = self.service_rate_take(&server) {
                    self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                    return;
                }
                let Some(client) = self.mcp.get(&server).cloned() else {
                    self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("mcp server {server:?} is not connected")),
                        0,
                    );
                    return;
                };
                // The key must NOT vary by attempt: a retry that presents a
                // fresh key is exactly the duplicate the key exists to prevent,
                // so nothing per-attempt may enter it.
                // The attempt rides separately for servers that want to
                // OBSERVE retries without keying on them, and `idempotency:
                // {value: …}` substitutes an application-level key (an order
                // id) when one exists — which beats any run-derived key, since
                // it also collides two different RUNS attempting the same
                // real-world operation.
                let key = spec
                    .get("idempotency")
                    .and_then(|i| i.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::engine::run::idempotency_key(run_id, step_id));
                let mut meta = json!({"agent/idempotency_key": key, "agent/attempt": attempt, "agent/instance": self.instance, "agent/run": run_id});
                let principal = self.runs.get(run_id).and_then(|r| r.principal.clone());
                self.attribute(&mut meta, principal.as_deref());
                crate::mcp::scope(&mut meta, principal.as_deref());
                // A span of the step's: a retry or a replay is another span
                // in the same trace, the idempotency key unchanged.
                let call =
                    crate::mcp::stamp_trace(&mut meta, self.step_trace(run_id, step_id).as_ref());
                let timeout = step
                    .timeout_ms
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(
                        self.settings
                            .limits
                            .step_timeout
                            .map(|d| d.0)
                            .unwrap_or(std::time::Duration::from_secs(600)),
                    );
                let tx = self.events_tx.clone();
                let (r, s) = (run_id.to_string(), step_id.to_string());
                // A call that becomes a task makes the step a durable wait on
                // it (RFC 0050) — unless the server's open tasks and the
                // declaring calls still out to it already reach the limit,
                // when the call goes out without the extension.
                let calling = self.mcp_task_slot(&server);
                let tasks = calling.is_some();
                self.executing
                    .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
                std::thread::Builder::new()
                    .name(format!("step:{server}.{tool}"))
                    .spawn(move || {
                        let start = crate::obs::otel::now_unix_nanos();
                        let (output, is_error, error) = match client.call_tool_called(
                            &tool,
                            Some(args),
                            Some(meta),
                            Some(timeout),
                            tasks,
                        ) {
                            Ok(::mcp::wire::Called::Result(res)) => {
                                let v = super::worker::tool_result_value(&res);
                                if res.is_error() {
                                    (v.clone(), true, Some(res.text()))
                                } else {
                                    (v, false, None)
                                }
                            }
                            Ok(::mcp::wire::Called::Task(task)) => {
                                crate::obs::otel::mcp_span(
                                    call.as_ref(),
                                    "tools/call",
                                    &tool,
                                    &server,
                                    true,
                                    start,
                                );
                                let _ = tx.send(super::events::Event::McpTask(Box::new(
                                    super::mcp_tasks::Note::Created {
                                        run: r,
                                        step: s,
                                        server,
                                        tool,
                                        task,
                                        trace: call,
                                        registry: false,
                                        calling,
                                    },
                                )));
                                return;
                            }
                            Err(f) => (
                                Value::Null,
                                true,
                                Some(format!("transport error: {}", f.error)),
                            ),
                        };
                        crate::obs::otel::mcp_span(
                            call.as_ref(),
                            "tools/call",
                            &tool,
                            &server,
                            !is_error,
                            start,
                        );
                        let _ = tx.send(super::events::Event::StepDone {
                            run: r,
                            step: s,
                            output,
                            is_error,
                            error,
                            tokens: 0,
                        });
                    })
                    .ok();
            }
            "agent" | "think" => {
                let role = node_role(&step.kind);
                self.step_turn(
                    run_id,
                    step_id,
                    &step,
                    &spec,
                    &data,
                    role,
                    StepWork::default(),
                )
            }
            "foreach" | "batch" | "iterate" | "parallel" | "race" | "subgraph" => {
                self.nested_start(run_id, step_id, &step, &spec)
            }
            "wait" | "join" | "workflow" | "message" | "workflow.signal" | "workflow.wait"
            | "workflow.cancel" | "subagent" | "human" | "mcp.resource" | "a2a.delegate"
            | "a2a.send" | "a2a.wait" | "classify" | "extract" | "summarize" | "judge"
            | "route" | "decide" => {
                self.execute_orchestration_step(run_id, step_id, &step, &spec, &data, &step_caller)
            }
            "switch" => {
                let on = spec.get("on").cloned().unwrap_or(Value::Null);
                let key = match &on {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                let cases = step
                    .field("cases")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let target = cases
                    .get(&key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| step.field_str("default").map(str::to_string));
                match target {
                    Some(t) => {
                        // The chosen case runs even without its deps being terminal
                        // (an explicit routing edge); the other cases are skipped.
                        let scope_prefix = step_id
                            .rsplit_once('.')
                            .map(|(p, _)| format!("{p}."))
                            .unwrap_or_default();
                        let mut skipped = Vec::new();
                        // Every other target (cases + default) still pending is skipped;
                        // the chosen one is forced (runs even without its deps).
                        let mut others: Vec<String> = cases
                            .values()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        if let Some(d) = step.field_str("default") {
                            others.push(d.to_string());
                        }
                        if let Some(run) = self.runs.get_mut(run_id) {
                            for tid in others {
                                if tid == t {
                                    continue;
                                }
                                let sid = format!("{scope_prefix}{tid}");
                                if let Some(st) = run.steps.get_mut(&sid)
                                    && st.status == StepStatus::Pending
                                {
                                    // Pruned, not skipped: the case was not
                                    // chosen, so its whole tail is dead.
                                    st.status = StepStatus::Pruned;
                                    skipped.push(sid);
                                }
                            }
                            let sid = format!("{scope_prefix}{t}");
                            if let Some(st) = run.steps.get_mut(&sid) {
                                st.status = StepStatus::Pending;
                                st.forced = true;
                            }
                        }
                        self.finish_step(
                            run_id,
                            step_id,
                            StepStatus::Done,
                            Some(json!({"case": key, "goto": t, "skipped": skipped})),
                            None,
                            0,
                        );
                    }
                    // No case, no default: `on_no_match: skip` prunes every
                    // branch and completes — for a switch whose "else" is
                    // honestly "do nothing". The default stays fail-closed.
                    None if step.field_str("on_no_match") == Some("skip") => {
                        let scope_prefix = step_id
                            .rsplit_once('.')
                            .map(|(p, _)| format!("{p}."))
                            .unwrap_or_default();
                        let mut skipped = Vec::new();
                        let others: Vec<String> = cases
                            .values()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        if let Some(run) = self.runs.get_mut(run_id) {
                            for tid in others {
                                let sid = format!("{scope_prefix}{tid}");
                                if let Some(st) = run.steps.get_mut(&sid)
                                    && st.status == StepStatus::Pending
                                {
                                    st.status = StepStatus::Pruned;
                                    skipped.push(sid);
                                }
                            }
                        }
                        self.finish_step(
                            run_id,
                            step_id,
                            StepStatus::Done,
                            Some(json!({"case": key, "matched": false, "skipped": skipped})),
                            None,
                            0,
                        );
                    }
                    None => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(json!({"case": key})),
                        Some(format!("switch: no case for {key:?} and no default")),
                        0,
                    ),
                }
            }
            "map" | "filter" | "reduce" | "sort" | "dedupe" | "chunk" | "parse" => {
                let out = match step.kind.as_str() {
                    "map" => crate::engine::data::map(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        step.field_str("as").unwrap_or("item"),
                        &data,
                    ),
                    "filter" => crate::engine::data::filter(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        step.field_str("as").unwrap_or("item"),
                        &data,
                    ),
                    "reduce" => crate::engine::data::reduce(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        spec.get("initial").cloned().unwrap_or(Value::Null),
                        step.field_str("as").unwrap_or("item"),
                        step.field_str("acc").unwrap_or("acc"),
                        &data,
                    ),
                    "sort" => crate::engine::data::sort(
                        spec.get("over").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                        spec.get("order").and_then(Value::as_str),
                    ),
                    "dedupe" => crate::engine::data::dedupe(
                        spec.get("over").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                    ),
                    "chunk" => crate::engine::data::chunk(
                        spec.get("value").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                        spec.get("size").and_then(Value::as_u64).unwrap_or(0) as usize,
                        spec.get("overlap").and_then(Value::as_u64).unwrap_or(0) as usize,
                    ),
                    _ => crate::engine::data::parse(
                        spec.get("text").and_then(Value::as_str).unwrap_or(""),
                        spec.get("format").and_then(Value::as_str),
                    ),
                };
                match out {
                    Ok(v) => self.finish_step(run_id, step_id, StepStatus::Done, Some(v), None, 0),
                    Err(e) => {
                        self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0)
                    }
                }
            }
            other => self.finish_step(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!(
                    "step kind {other:?} is not executable in this build"
                )),
                0,
            ),
        }
    }

    /// A step's internal tool call (in-loop or deferred/executor).
    pub(crate) fn step_tool_call(
        &mut self,
        run_id: &str,
        step_id: &str,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) {
        let outcome = self.execute_tool(caller, name, args);
        self.step_tool_outcome(run_id, step_id, name, outcome);
    }

    /// Deliver a step's tool outcome: finish the step, suspend it on the wait
    /// the tool parked on, or leave it to the executor thread.
    pub(crate) fn step_tool_outcome(
        &mut self,
        run_id: &str,
        step_id: &str,
        name: &str,
        outcome: ToolOutcome,
    ) {
        match outcome {
            ToolOutcome::Ready(v, is_error) => {
                let err = is_error.then(|| match &v {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                });
                self.finish_step(
                    run_id,
                    step_id,
                    if is_error {
                        StepStatus::Failed
                    } else {
                        StepStatus::Done
                    },
                    Some(v),
                    err,
                    0,
                );
            }
            ToolOutcome::Deferred(kind) => {
                let wait = match &kind {
                    PendingKind::Timer { id } => json!({"kind": "timer", "timer": id}),
                    PendingKind::Subagent { handle } => {
                        json!({"kind": "subagent", "handle": handle})
                    }
                    PendingKind::Think { .. } => json!({"kind": "think"}),
                    // A sign-in does not outlive this process: a restart that
                    // finds the step waiting on one ends it `interrupted`.
                    PendingKind::CompanySignIn { id, company } => {
                        json!({"kind": "company_sign_in", "sign_in": id, "company": company})
                    }
                    // Nor does an operation's approval, or the User's
                    // answer about a Direct Conversation.
                    PendingKind::CompanyOperation { id, company } => {
                        json!({"kind": "company_operation", "operation": id, "company": company})
                    }
                    PendingKind::CompanyDirect { id, company } => {
                        json!({"kind": "company_direct", "direct": id, "company": company})
                    }
                    // The person's consent gate travels whole, so a restart
                    // asks again with the same binding and deadline.
                    #[cfg(feature = "xaa")]
                    PendingKind::Consent { id } => self.consent_wait(id),
                    #[cfg(not(feature = "xaa"))]
                    PendingKind::Consent { id } => json!({"kind": "consent", "gate": id}),
                    PendingKind::Run { run, .. } => json!({"kind": "run", "run": run}),
                    PendingKind::Await {
                        condition,
                        deadline_ms,
                    } => {
                        json!({"kind": "await", "condition": condition, "deadline_ms": deadline_ms})
                    }
                    // A gate's ENFORCEMENT travels in the durable record, as
                    // the `human` node's does: a restart rebuilds the pending
                    // ask from it, so what is left out here is dropped then —
                    // a policy gate would lose its addressee, its answer
                    // shape, and the call an approval must run.
                    PendingKind::Human {
                        task,
                        deadline_ms,
                        schema,
                        addressee,
                        policy,
                        ..
                    } => {
                        let mut w =
                            json!({"kind": "human", "task": task, "deadline_ms": deadline_ms});
                        if let Some(schema) = schema {
                            w["schema"] = schema.clone();
                        }
                        if let Some(to) = addressee {
                            w["to"] = to.to_json();
                        }
                        if let Some(call) = policy {
                            w["policy"] = super::human::policy_call_json(call);
                        }
                        w
                    }
                };
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .suspend_step(step_id, wait);
                if !matches!(kind, PendingKind::Timer { .. }) {
                    self.push_pending(super::reactor::PendingTool {
                        target: Target::Step(run_id.to_string(), step_id.to_string()),
                        name: name.to_string(),
                        kind,
                        started_ms: now_ms(),
                    });
                }
                self.checkpoint(false);
            }
            ToolOutcome::Executing => {
                self.executing
                    .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
            }
        }
    }

    /// A step suspended on a policy gate whose call was just approved: put it
    /// back to running — the call now runs as the step's own work, and a step
    /// left suspended on an answered gate would be re-armed, or stranded, by
    /// a restart — and return the caller the call runs as. `None` when the
    /// run or step is gone.
    pub(crate) fn resume_gated_step(&mut self, run_id: &str, step_id: &str) -> Option<ToolCaller> {
        let run = self.runs.get_mut(run_id)?;
        let attempt = {
            let st = run.steps.get_mut(step_id)?;
            st.status = StepStatus::Running;
            st.wait = None;
            st.attempt
        };
        if !run.status.is_terminal() {
            run.status = RunStatus::Running;
        }
        run.touch();
        Some(ToolCaller {
            run: Some(run_id.to_string()),
            step: Some(step_id.to_string()),
            req: attempt as u64,
            principal: run.principal.clone(),
            ctx: run.conversation.clone(),
            msg_depth: run.msg_depth,
            session: run.session.clone(),
            ..Default::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn step_turn_pub(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &template::Data,
        role: ModelRole,
        work: StepWork,
    ) {
        self.step_turn(run_id, step_id, step, spec, data, role, work)
    }

    /// An `agent`/`think` step → a turn worker (budget-admitted), on the
    /// node's `model:` or else `role`'s. With a decision in `work`, the worker
    /// answers it instead (RFC 0046 §5.5).
    #[allow(clippy::too_many_arguments)]
    fn step_turn(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &template::Data,
        role: ModelRole,
        work: StepWork,
    ) {
        let deciding = work.decision.is_some();
        let is_think = step.kind == "think" || deciding;
        let prompt = if is_think {
            spec.get("prompt").and_then(Value::as_str).unwrap_or("")
        } else {
            spec.get("instruction")
                .and_then(Value::as_str)
                .unwrap_or("")
        }
        .to_string();
        let output_schema = spec.get("output_schema").cloned();
        let mut messages = Vec::new();
        // `reads`: fold named run data into the prompt.
        if let Some(reads) = spec.get("reads").and_then(Value::as_array) {
            for path in reads.iter().filter_map(Value::as_str) {
                if let Some(v) = template::lookup(path, data) {
                    messages.push(Msg::system(format!("{path} = {v}")));
                }
            }
        }
        // `context`: seed messages — a bare array, or the object form
        // `{template: <name>, seed: [...]}`, where `template` names one of the
        // config's `context.templates` to render THIS step's system prompt
        // (node-level context control; `context.template` is the default).
        let step_template: Option<String> = spec
            .get("context")
            .and_then(Value::as_object)
            .and_then(|o| o.get("template"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let seed_list = spec.get("context").and_then(Value::as_array).or_else(|| {
            spec.get("context")
                .and_then(Value::as_object)
                .and_then(|o| o.get("seed"))
                .and_then(Value::as_array)
        });
        if let Some(seed) = seed_list {
            for m in seed {
                match (m["role"].as_str(), m["content"].as_str()) {
                    (Some("system"), Some(c)) => messages.push(Msg::system(c)),
                    (Some("assistant"), Some(c)) => {
                        messages.push(Msg::assistant(Some(c.to_string()), vec![]))
                    }
                    (_, Some(c)) => messages.push(Msg::user(c, None)),
                    _ => {}
                }
            }
        }
        let mut user = prompt.clone();
        if let Some(c) = spec.get("output_contract").and_then(Value::as_str) {
            user.push_str(&format!("\n\nOutput contract:\n{c}"));
        }
        if let Some(s) = &output_schema {
            user.push_str(&format!(
                "\n\nReply with ONLY one JSON object matching this JSON Schema:\n{s}"
            ));
        }
        messages.push(Msg::user(user, None));
        // Skills for the step: each loaded, or said not to be (RFC 0051 D3).
        // A model kind takes `skills` as its own field, rendered; one taken
        // as the field every step accepts has only the definition's. Each
        // reference counts once.
        let mut refs: Vec<String> = match spec.get("skills").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            None => step.skills.clone(),
        };
        let mut seen = std::collections::BTreeSet::new();
        refs.retain(|r| seen.insert(r.clone()));
        let extra = self.step_skills(run_id, step_id, &refs);
        let mut system = match spec.get("system").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None if is_think => format!(
                "You are the reasoning module of {}. Reply with {}. No tools are available.",
                self.instance,
                if output_schema.is_some() {
                    "ONLY one JSON object matching the schema"
                } else {
                    "your conclusion"
                }
            ),
            None => self.system_prompt_named(None, extra.as_deref(), step_template.as_deref()),
        };
        // The template places the skills itself; a step's own `system`, and
        // a tool-free step's, carry them after it — a `think` step, and each
        // preset that runs as one, loaded them and then left them out.
        if (spec.get("system").and_then(Value::as_str).is_some() || is_think)
            && let Some(x) = &extra
        {
            system.push_str("\n\n");
            system.push_str(x);
        }
        let listed: Option<Vec<String>> = spec.get("servers").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
        let (tools, internal, routes) = if is_think {
            (Vec::new(), Vec::new(), BTreeMap::new())
        } else {
            let allow: Option<Vec<String>> = spec.get("tools").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
            let owner = self.runs.get(run_id).and_then(|r| r.principal.clone());
            let session = self.runs.get(run_id).and_then(|r| r.session.clone());
            self.tool_plan(
                &Caller::Workflow,
                allow.as_deref(),
                listed.as_deref(),
                owner.as_deref(),
                session.as_deref(),
            )
        };
        let servers: Vec<String> = match listed {
            Some(listed) => listed,
            None => routes
                .values()
                .map(|(s, _)| s.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        // Budget admission: the estimate is charged against every scope this
        // run belongs to as well as the instance, so the tightest one binds.
        let est: u64 = messages.iter().map(Msg::est_tokens).sum::<u64>()
            + crate::context::tokens::estimate(&system)
            + 4096;
        let scopes = self.run_scopes(run_id);
        let reservation = match self.governor.admit(est, &scopes, now_ms()) {
            Admission::Ok { reservation, model } => {
                if let Some(m) = model {
                    self.run_log(run_id).info(
                        "budget.degraded",
                        json!({"run": run_id, "step": step_id, "model": m}),
                    );
                }
                Some(reservation)
            }
            Admission::Wait { until_ms, reason } => {
                self.run_log(run_id).info(
                    "budget.wait",
                    json!({"run": run_id, "step": step_id, "until_ms": until_ms, "reason": reason}),
                );
                crate::state::kill_point("budget.waiting");
                match self.timers.arm(
                    &self.durable,
                    until_ms,
                    json!({"kind": "step_budget", "run": run_id, "step": step_id}),
                    Value::Null,
                ) {
                    Ok(id) => {
                        self.runs.get_mut(run_id).expect("present").suspend_step(step_id, json!({"kind": "waiting_budget", "timer": id, "until_ms": until_ms, "reason": reason}));
                        self.checkpoint(false);
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("budget wait: {e}")),
                        0,
                    ),
                }
                return;
            }
            Admission::Exhausted { reason } => {
                self.apply_lifetime_exhausted(&reason);
                crate::obs::metrics::record_refusal("budget");
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(reason), 0);
                return;
            }
            Admission::Refuse { reason } | Admission::Fail { reason } => {
                crate::obs::metrics::record_refusal("budget");
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(reason), 0);
                return;
            }
        };
        // `model:` on an `agent`/`think` node: cost tiering inside one
        // workflow, without forking a subagent process just to change a model
        // string. Read here, because `spec` is shadowed by the `TurnSpec`
        // below.
        let node_model = spec
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        // `effort:` on a `think`/`agent` node, for a reasoning model.
        let node_effort = spec
            .get("effort")
            .and_then(Value::as_str)
            .and_then(crate::wire::intel::Effort::parse);
        let limits = spec.get("limits").cloned().unwrap_or(json!({}));
        let max_steps = limits
            .get("steps")
            .and_then(Value::as_u64)
            .map(|s| s as u32)
            .unwrap_or(self.settings.limits.run.steps());
        let max_tokens = step
            .budget
            .or_else(|| limits.get("tokens").and_then(Value::as_u64))
            .unwrap_or(self.settings.limits.run.tokens());
        // The step's own timeout, cut to what is left of the run's deadline:
        // the turn ends when the run must, model call in flight included.
        let step_ms = step.timeout_ms.unwrap_or(
            self.settings
                .limits
                .step_timeout
                .map(|d| d.0.as_millis() as u64)
                .unwrap_or(600_000),
        );
        let deadline_ms = match self.runs.get(run_id).and_then(|r| r.deadline_ms) {
            Some(at) => step_ms.min(at.saturating_sub(crate::state::now_ms()).max(1)),
            None => step_ms,
        };
        let spec = TurnSpec {
            kind: if deciding {
                TurnKind::Decide
            } else if is_think {
                TurnKind::Think
            } else {
                TurnKind::Agent
            },
            system,
            messages,
            tools,
            internal,
            mcp_routes: routes,
            output_schema,
            max_rounds: if is_think { 3 } else { 0 },
            budget_admission: self.governor.is_active(),
            idempotency_prefix: format!("{}/{run_id}/{step_id}", self.instance),
            tool_meta: Some({
                let mut m = json!({"agent/run": run_id, "agent/step": step_id, "agent/instance": self.instance});
                let principal = self.runs.get(run_id).and_then(|r| r.principal.clone());
                self.attribute(&mut m, principal.as_deref());
                crate::mcp::scope(&mut m, principal.as_deref());
                m
            }),
            temperature: None,
            max_tokens_per_call: 0,
            turn_id: format!(
                "{run_id}/{step_id}#{}",
                self.runs
                    .get(run_id)
                    .and_then(|r| r.step(step_id))
                    .map(|s| s.attempt)
                    .unwrap_or(1)
            ),
            decision: work.decision,
            sure: work.sure,
        };
        // The span of the step's attempt, under the run's: the worker it
        // spawns is a span of its own beneath it — its `invoke_agent` — and a
        // replay after a crash is another attempt, in the same trace.
        let step_trace = self.step_trace(run_id, step_id);
        let launch = super::turns::TurnLaunch {
            spec,
            kind: ChildKind::StepTurn {
                run: run_id.to_string(),
                step: step_id.to_string(),
                reservation,
            },
            servers,
            max_steps,
            max_tokens,
            deadline_ms,
            agent_path: format!("run/{run_id}/{step_id}"),
            model: node_model,
            role,
            effort: node_effort,
            trace: step_trace.clone(),
        };
        match self.spawn_turn(launch) {
            Ok(node) => {
                if let Some(st) = self
                    .runs
                    .get_mut(run_id)
                    .and_then(|r| r.steps.get_mut(step_id))
                {
                    st.worker = Some(node.0.to_string());
                }
                self.log.in_trace(step_trace.as_ref()).info(
                    "step.turn.spawn",
                    json!({"run": run_id, "step": step_id, "node": node.0}),
                );
            }
            Err(e) => {
                if let Some(r) = reservation {
                    self.governor.release(r);
                }
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some(format!("spawn: {e}")),
                    0,
                );
            }
        }
    }

    /// The governor scopes a run's turns are charged to (workflow budget → run scope).
    fn run_scopes(&mut self, run_id: &str) -> Vec<String> {
        // A run's model spend is charged to the principal it is being done for
        // as well as to the run itself, so a per-person ceiling covers the
        // work someone STARTED, not only the turns they typed.
        let mut scopes = self.principal_scopes(
            self.runs
                .get(run_id)
                .and_then(|r| r.principal.clone())
                .as_deref(),
        );
        let Some(wf) = self.definition_for_run(run_id) else {
            return scopes;
        };
        if let Some(b) =
            wf.limits.budget.as_ref().and_then(|b| {
                serde_json::from_value::<crate::config::settings::Budget>(b.clone()).ok()
            })
        {
            let key = format!("run:{run_id}");
            self.governor.ensure_scope(&key, &b);
            scopes.push(key);
        }
        scopes
    }

    // ---- outcomes --------------------------------------------------------------

    /// An executor / deferred tool finished a step.
    pub(crate) fn on_step_done(
        &mut self,
        run_id: &str,
        step_id: &str,
        output: Value,
        is_error: bool,
        error: Option<String>,
        tokens: u64,
    ) {
        self.executing.remove(&format!("{run_id}/{step_id}"));
        self.finish_step(
            run_id,
            step_id,
            if is_error {
                StepStatus::Failed
            } else {
                StepStatus::Done
            },
            Some(output),
            error,
            tokens,
        );
    }

    /// A step's turn worker finished.
    pub(crate) fn on_step_turn_done(&mut self, run_id: &str, step_id: &str, turn: TurnResult) {
        let tokens = turn.usage.total();
        if turn.status == "completed" {
            let output = turn
                .value
                .clone()
                .or_else(|| turn.finish.as_ref().and_then(|f| f.get("output").cloned()))
                .or_else(|| turn.text.clone().map(Value::String))
                .unwrap_or(Value::Null);
            // A `finish {status: failed|refused}` from an agent step fails the step.
            let failed = turn
                .finish
                .as_ref()
                .and_then(|f| f.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s != "completed");
            if failed {
                let reason = turn
                    .finish
                    .as_ref()
                    .and_then(|f| f.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("agent finished with a non-completed status")
                    .to_string();
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    Some(output),
                    Some(reason),
                    tokens,
                );
            } else {
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    Some(output),
                    None,
                    tokens,
                );
            }
        } else {
            let status = if turn.status == "deadline" {
                StepStatus::Timeout
            } else {
                StepStatus::Failed
            };
            self.finish_step(
                run_id,
                step_id,
                status,
                turn.value
                    .clone()
                    .or_else(|| turn.text.clone().map(Value::String)),
                Some(format!(
                    "turn {}{}",
                    turn.status,
                    turn.error
                        .as_deref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                )),
                tokens,
            );
        }
    }

    /// A step timer fired (`sleep` done, or a budget window opened).
    pub(crate) fn on_step_timer(
        &mut self,
        run_id: &str,
        step_id: &str,
        budget: bool,
        payload: &Value,
    ) {
        if budget {
            if let Some(st) = self
                .runs
                .get_mut(run_id)
                .and_then(|r| r.steps.get_mut(step_id))
            {
                st.status = StepStatus::Pending;
                st.wait = None;
            }
            if let Some(r) = self.runs.get_mut(run_id) {
                r.touch();
            }
            return;
        }
        self.finish_step(
            run_id,
            step_id,
            StepStatus::Done,
            Some(payload.clone()),
            None,
            0,
        );
    }

    /// Record a step's terminal outcome; retry / route failures; checkpoint.
    pub(crate) fn finish_step_pub(
        &mut self,
        run_id: &str,
        step_id: &str,
        status: StepStatus,
        output: Option<Value>,
        error: Option<String>,
        tokens: u64,
    ) {
        self.finish_step(run_id, step_id, status, output, error, tokens)
    }

    fn finish_step(
        &mut self,
        run_id: &str,
        step_id: &str,
        status: StepStatus,
        output: Option<Value>,
        error: Option<String>,
        tokens: u64,
    ) {
        // A terminal step may make dependents ready this very iteration — tell
        // the loop to re-run scheduling before it parks (the inline fixpoint).
        self.resched = true;
        // A wait that subscribed lets go as it leaves the wait, so its
        // grant's slot is free for the next one at once (RFC 0045 §5.15).
        #[cfg(feature = "mcp-events")]
        if let Some(key) = self
            .runs
            .get(run_id)
            .and_then(|r| r.steps.get(step_id))
            .and_then(|st| st.wait.as_ref())
            .and_then(|w| w["subscription"].as_str())
            .map(str::to_string)
        {
            let owner = super::mcp_events::Owner::Wait {
                run: run_id.to_string(),
                step: step_id.to_string(),
            };
            self.events_release(&key, &owner, "wait_left");
        }
        let Some(wf) = self
            .runs
            .get(run_id)
            .and_then(|_| self.definition_for_run(run_id))
        else {
            return;
        };
        let Some((step, scope)) = self
            .runs
            .get(run_id)
            .and_then(|r| self.resolve_step(&wf, r, step_id))
        else {
            return;
        };
        {
            let run = self.runs.get_mut(run_id).expect("present");
            if run.status.is_terminal() {
                return; // a late result for a finished run
            }
            run.tokens += tokens;
        }
        crate::obs::metrics::record_step(match status {
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
            _ => "other",
        });
        // An output past `limits.inline_max_bytes` is stored as an artifact and
        // replaced by a reference, keeping the run record (and every checkpoint
        // that serializes it) bounded. A failed artifact write keeps the inline
        // value rather than losing the output.
        let output = match output {
            Some(v) if !v.is_null() => {
                let cap = self.settings.limits.inline_max_bytes.unwrap_or(65_536) as usize;
                if v.to_string().len() > cap {
                    let carries = self.run_carries(run_id);
                    match self.artifacts.create(
                        &self.durable,
                        super::artifacts::NewArtifact {
                            name: &format!("{run_id}/{step_id}/output.json"),
                            mime: Some("application/json"),
                            content: v.clone(),
                            created_by: Some("engine"),
                            sensitive: false,
                            owner: Some(run_id),
                            carries: &carries,
                        },
                    ) {
                        Ok(meta) => {
                            self.run_log(run_id).info("step.output.artifact", json!({"run": run_id, "step": step_id, "artifact": meta["id"], "size": meta["size"]}));
                            Some(json!({"$artifact": meta["id"], "size": meta["size"]}))
                        }
                        Err(e) => {
                            self.run_log(run_id).warn(
                                "step.output.artifact_fail",
                                json!({"run": run_id, "step": step_id, "err": e}),
                            );
                            Some(v)
                        }
                    }
                } else {
                    Some(v)
                }
            }
            other => other,
        };
        // A declared `output_schema` is enforced here: a step that completed
        // but produced a shape its consumers cannot parse fails instead.
        let (status, error) = match (&status, &step.output_schema, &output) {
            (StepStatus::Done, Some(schema), Some(out)) => {
                match crate::jsonschema::validate(schema, out) {
                    Ok(()) => (status, error),
                    Err(e) => (
                        StepStatus::Failed,
                        Some(format!(
                            "output does not match output_schema: {}",
                            crate::jsonschema::explain(&e)
                        )),
                    ),
                }
            }
            _ => (status, error),
        };
        let attempt = self
            .runs
            .get(run_id)
            .and_then(|r| r.step(step_id))
            .map(|s| s.attempt)
            .unwrap_or(1);
        // The attempt is over: its lines are in its span, which is exported
        // now — a retry is an attempt of its own.
        let slog = self.step_log(run_id, step_id);
        slog.info("step.done", json!({"run": run_id, "step": step_id, "status": status, "attempt": attempt, "tokens": tokens, "err": error}));
        // Outside text the step read — an http response, a peer's answer, an
        // untrusted_input tool's result — taints its run before any step
        // after it reads it (`runtime::taint`).
        if matches!(status, StepStatus::Done | StepStatus::Failed) {
            self.taint_step(run_id, &step.kind, &step.spec);
        }
        self.end_step_span(
            run_id,
            step_id,
            &step.kind,
            crate::runtime::nested::StatusLabel::as_label(&status),
        );
        // Feed the circuit breaker, when this step keeps one. Every ATTEMPT
        // counts (a breaker measures calls, not runs) — except our own
        // fast-fails: refusing to dial is not evidence about the remote.
        if matches!(
            step.kind.as_str(),
            "http" | "mcp.tool" | "a2a.send" | "a2a.delegate"
        ) && let Some(cfg) = self.effective_breaker(&step)
            && !matches!(status, StepStatus::Failed | StepStatus::Timeout if error
                .as_deref()
                .is_some_and(|e| e.starts_with(super::breaker::OPEN_ERR)))
            && matches!(
                status,
                StepStatus::Done | StepStatus::Failed | StepStatus::Timeout
            )
        {
            let workflow = self
                .runs
                .get(run_id)
                .map(|r| r.workflow.clone())
                .unwrap_or_default();
            let key = super::breaker::key(&workflow, step_id);
            let mut st = self
                .durable
                .manifest()
                .breakers
                .get(&key)
                .cloned()
                .unwrap_or_else(|| json!({}));
            let t = super::breaker::record(&mut st, cfg, status == StepStatus::Done, now_ms());
            self.durable.manifest_update(|m| {
                m.breakers.insert(key.clone(), st);
            });
            match t {
                super::breaker::Transition::Opened { fails } => self.log.warn(
                    "breaker.open",
                    json!({"breaker": key, "consecutive_failures": fails,
                           "cooldown": format!("{}ms", cfg.cooldown_ms)}),
                ),
                super::breaker::Transition::Reopened => self.log.warn(
                    "breaker.reopen",
                    json!({"breaker": key, "probe_failed": true}),
                ),
                super::breaker::Transition::Closed => {
                    self.log.info("breaker.closed", json!({"breaker": key}))
                }
                super::breaker::Transition::None => {}
            }
        }
        #[cfg(feature = "a2a")]
        self.feed_push(
            "step",
            crate::runtime::a2a_server::FeedVis::Operator,
            json!({"run": run_id, "step": step_id, "phase": "done",
                   "status": crate::runtime::nested::StatusLabel::as_label(&status), "attempt": attempt, "tokens": tokens,
                   "trace_id": self.run_trace(run_id).map(|t| t.trace_id),
                   // Same 2 KiB cap the run drill-down applies: a step output can
                   // be an entire document, and a feed is not the place for it.
                   "err": error.as_deref().map(|e| {
                       e.chars().take(2048).collect::<String>()
                   })}),
        );
        if matches!(status, StepStatus::Failed | StepStatus::Timeout) {
            // Retry? Never a Company call that may have been performed: it
            // is not sent twice blind (RFC 0047 §5.9).
            let unknown = error
                .as_deref()
                .is_some_and(super::companies::is_outcome_unknown);
            if unknown && step.retry.is_some() {
                slog.info(
                    "step.retry.skipped",
                    json!({"run": run_id, "step": step_id, "attempt": attempt,
                           "reason": "outcome unknown"}),
                );
            }
            if let Some(retry) = &step.retry
                && attempt <= retry.max
                && !unknown
            {
                // Exponential, with jitter. Without it every step that failed
                // in the same wave — the usual case, since they usually failed
                // for the same upstream reason — retries in lockstep and
                // rebuilds the thundering herd the backoff exists to break up.
                // Deterministic per (run, step, attempt): no RNG, so a replay
                // reproduces the same schedule.
                let base = retry
                    .backoff_ms
                    .saturating_mul(1u64 << (attempt.saturating_sub(1)).min(10));
                let backoff = if base == 0 {
                    0
                } else {
                    let mut h: u64 = 1469598103934665603;
                    for b in run_id.bytes().chain(step_id.bytes()).chain([attempt as u8]) {
                        h ^= b as u64;
                        h = h.wrapping_mul(1099511628211);
                    }
                    // ±20% around the base.
                    let spread = (base / 5).max(1);
                    base.saturating_sub(spread) + (h % (spread * 2 + 1))
                };
                slog.info("step.retry", json!({"run": run_id, "step": step_id, "attempt": attempt, "backoff_ms": backoff}));
                if backoff == 0 {
                    if let Some(st) = self
                        .runs
                        .get_mut(run_id)
                        .and_then(|r| r.steps.get_mut(step_id))
                    {
                        st.status = StepStatus::Pending;
                        st.error = error;
                    }
                } else {
                    match self.timers.arm(
                        &self.durable,
                        now_ms() + backoff,
                        json!({"kind": "step_budget", "run": run_id, "step": step_id}),
                        Value::Null,
                    ) {
                        Ok(id) => {
                            self.runs.get_mut(run_id).expect("present").suspend_step(
                                step_id,
                                json!({"kind": "retry_backoff", "timer": id, "error": error}),
                            );
                        }
                        Err(_) => {
                            if let Some(st) = self
                                .runs
                                .get_mut(run_id)
                                .and_then(|r| r.steps.get_mut(step_id))
                            {
                                st.status = StepStatus::Pending;
                            }
                        }
                    }
                }
                self.checkpoint(false);
                return;
            }
            let err_text = error.clone().unwrap_or_else(|| "failed".into());
            self.runs
                .get_mut(run_id)
                .expect("present")
                .end_step(step_id, status, output, error);
            // `on_timeout`: a deadline expiring on a wait is usually an
            // EXPECTED branch (nobody replied, the alert never cleared), not
            // a failure — route to the named step, forced, and keep the run
            // alive. Only a real Timeout takes this edge; other errors still
            // answer to `on_error`. The step itself stays `Timeout`, which
            // does NOT satisfy dependents — so the success path and the
            // timeout path are mutually exclusive by construction.
            if status == StepStatus::Timeout
                && let Some(t) = step.field_str("on_timeout")
            {
                let target = match &scope {
                    Some(sc) => super::nested::scoped_id(&sc.parent, t),
                    None => t.to_string(),
                };
                if let Some(st) = self
                    .runs
                    .get_mut(run_id)
                    .and_then(|r| r.steps.get_mut(&target))
                {
                    st.status = StepStatus::Pending;
                    st.forced = true;
                }
                self.run_log(run_id).info(
                    "step.timeout_routed",
                    json!({"run": run_id, "step": step_id, "to": t}),
                );
                crate::state::kill_point("step.before_done");
                self.checkpoint(false);
                if let Some(sc) = &scope {
                    self.on_scoped_step_done(run_id, sc);
                }
                return;
            }
            if let Some(sc) = &scope {
                // Inside a body: `continue` marks done-with-error, `goto` re-arms
                // a sibling, `fail` leaves the step failed for the parent to judge.
                match &step.on_error {
                    OnError::Continue => {
                        if let Some(st) = self
                            .runs
                            .get_mut(run_id)
                            .and_then(|r| r.steps.get_mut(step_id))
                        {
                            st.status = StepStatus::Done;
                            st.error = Some(err_text.clone());
                            if st.output.is_none() {
                                st.output = Some(json!({"error": err_text}));
                            }
                        }
                    }
                    OnError::Goto(t) => {
                        let sid = super::nested::scoped_id(&sc.parent, t);
                        if let Some(st) = self
                            .runs
                            .get_mut(run_id)
                            .and_then(|r| r.steps.get_mut(&sid))
                        {
                            st.status = StepStatus::Pending;
                            st.forced = true;
                        }
                    }
                    OnError::Fail => {}
                }
                crate::state::kill_point("step.before_done");
                if !crate::engine::model::pure_data_kind(&step.kind) {
                    self.checkpoint(false);
                }
                self.on_scoped_step_done(run_id, sc);
                return;
            }
            let routed = run::route_failure(
                &wf,
                self.runs.get_mut(run_id).expect("present"),
                &step,
                &err_text,
            );
            match routed {
                Ok(next) => {
                    if !next.is_empty() {
                        self.run_log(run_id).info(
                            "step.goto",
                            json!({"run": run_id, "from": step_id, "to": next}),
                        );
                    }
                }
                Err(reason) => {
                    self.cancel_children_of_run(run_id, "run failed");
                    self.runs.get_mut(run_id).expect("present").finish(
                        RunStatus::Failed,
                        None,
                        Some(reason),
                    );
                    self.on_run_terminal(run_id);
                    return;
                }
            }
            if let OnError::Continue = step.on_error {
                // Already marked done-with-error by route_failure.
            }
        } else {
            // Memoize (`cache`).
            if step.cache.is_some()
                && status == StepStatus::Done
                && let Some(key) = self
                    .runs
                    .get(run_id)
                    .and_then(|r| r.steps.get(step_id))
                    .and_then(|st| st.cache_key.clone())
                && let Some(out) = &output
            {
                self.cache_store(run_id, step_id, &key, out);
            }
            self.runs
                .get_mut(run_id)
                .expect("present")
                .end_step(step_id, status, output, error);
        }
        crate::state::kill_point("step.before_done");
        // Pure steps ride the tick's checkpoint — unless this completion made
        // the RUN terminal (a routed failure), which must land durably now.
        let terminal_now = self
            .runs
            .get(run_id)
            .is_some_and(|r| r.status.is_terminal());
        if terminal_now || !crate::engine::model::pure_data_kind(&step.kind) {
            self.checkpoint(false);
        }
        if let Some(sc) = &scope {
            self.on_scoped_step_done(run_id, sc);
        }
    }

    /// The run reached a terminal state: report, wake, plan bindings, counters.
    /// Evict terminal runs beyond `store.retention.runs`.
    ///
    /// Without this a long-lived instance keeps one durable record per run
    /// forever — on a laptop, the difference between an agent that runs for a
    /// month and one that fills a disk. Only TERMINAL runs are candidates:
    /// nothing in flight is ever dropped, whatever the policy says. Default is
    /// unbounded, so an operator who has not thought about it keeps today's
    /// behaviour.
    fn evict_terminal_runs(&mut self) {
        let policy = &self.settings.store.retention.runs;
        let keep_last = policy.keep_last;
        let ttl_ms = policy.ttl.as_ref().map(|d| d.0.as_millis() as u64);
        if keep_last.is_none() && ttl_ms.is_none() {
            return;
        }
        let now = now_ms();
        // Newest first, so "keep the last N" is a prefix.
        let mut terminal: Vec<(String, u64)> = self
            .runs
            .values()
            .filter(|r| r.status.is_terminal())
            .map(|r| (r.id.clone(), r.finished.unwrap_or(0)))
            .collect();
        terminal.sort_by_key(|(_, finished)| std::cmp::Reverse(*finished));

        let mut drop: Vec<String> = Vec::new();
        for (i, (id, finished)) in terminal.iter().enumerate() {
            let over_count = keep_last.is_some_and(|k| i >= k as usize);
            let over_age = ttl_ms.is_some_and(|t| now.saturating_sub(*finished) > t);
            if over_count || over_age {
                drop.push(id.clone());
            }
        }
        for id in drop {
            // A non-durable run has nothing in the store to evict.
            let was_durable = self.runs.get(&id).is_none_or(|r| r.durable);
            // Taken before the record goes: the lines are about the run.
            let rlog = self.run_log(&id);
            self.runs.remove(&id);
            if was_durable && let Err(e) = self.durable.delete(crate::state::Kind::Run, &id) {
                rlog.warn("run.evict.fail", json!({"run": id, "err": e.to_string()}));
                continue;
            }
            rlog.info("run.evicted", json!({"run": id}));
        }
    }

    pub(crate) fn on_run_terminal(&mut self, run_id: &str) {
        let Some(run) = self.runs.get(run_id) else {
            return;
        };
        let (status, output, error, workflow) = (
            run.status,
            run.output.clone(),
            run.error.clone(),
            run.workflow.clone(),
        );
        #[cfg(feature = "a2a")]
        let a2a_task = run.task.clone();
        // Eviction runs here because this is the only moment the candidate set
        // grows. Deferred to the end of the function so the run's own
        // completion handling (webhook reply, A2A task, feed) happens first —
        // evicting a record before its result was delivered would be a fine way
        // to lose an answer.
        let evict_after = true;
        // A `respond: sync` webhook awaiting this run gets its result now.
        #[cfg(feature = "a2a")]
        self.webhook_sync_reply(run_id);
        // A queued child run (`child_run` wait) resolves its parent step.
        if let Some(parent) = self.runs.get(run_id).and_then(|r| r.parent.clone())
            && let (Some(pr), Some(ps)) = (
                parent["run"].as_str().map(str::to_string),
                parent["step"].as_str().map(str::to_string),
            )
            && self
                .runs
                .get(&pr)
                .and_then(|r| r.steps.get(&ps))
                .is_some_and(|st| {
                    st.status == StepStatus::Suspended
                        && st.wait.as_ref().is_some_and(|w| w["kind"] == "child_run")
                })
        {
            // The parent's `workflow` step reads its child back: withheld
            // when the parent holds both other legs and the child carries
            // outside input the parent was not handed (`withhold`).
            let mut out =
                json!({"run": run_id, "status": status, "output": output, "error": error});
            let withheld = self.withhold_record(
                &super::withhold::Reader::Run(pr.clone()),
                super::withhold::Source::Run(run_id),
                &mut out,
                "workflow step",
            );
            self.finish_step_pub(
                &pr,
                &ps,
                if status == RunStatus::Completed {
                    StepStatus::Done
                } else {
                    StepStatus::Failed
                },
                Some(out),
                (status != RunStatus::Completed).then(|| {
                    withheld.clone().unwrap_or_else(|| {
                        error
                            .clone()
                            .unwrap_or_else(|| format!("child run {}", status.as_str()))
                    })
                }),
                0,
            );
        }
        self.counters.runs_finished += 1;
        crate::obs::metrics::record_run(status.as_str());
        self.run_log(run_id).info("run.done", json!({"run": run_id, "workflow": workflow, "status": status, "err": error, "output": if self.log.content_capture() { output.clone().unwrap_or(Value::Null) } else { Value::Null }}));
        self.end_run_span(run_id);
        self.governor.drop_scope(&format!("run:{run_id}"));
        // What the table says the run carries now goes on its record before
        // the sweep below can release the definition it ran under: the
        // result outlives the definition, and the waiters below, the plan
        // and every later read judge it by the record (`withhold`).
        let carries = self.run_carries(run_id);
        self.hand_run(run_id, &carries);
        self.retire_sweep();
        // Durable-pin GC for the ordinary path: when the LAST run of a
        // definition version lands, its stored pin has no reader left. (A
        // still-armed workflow re-pins on its next run's first start.)
        if let Some(hash) = self.runs.get(run_id).map(|r| r.workflow_hash.clone())
            && !self
                .runs
                .values()
                .any(|r| !r.status.is_terminal() && r.workflow_hash == hash)
        {
            let _ = self.durable.delete(
                crate::state::Kind::Memory,
                &format!("{}{hash}", super::retire::PIN_PREFIX),
            );
            self.pin_written.remove(&hash);
        }
        // Answer waiters (workflow.wait / run sync).
        let waiting: Vec<(Target, String)> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Run { run, .. } if run == run_id))
            .map(|p| (p.target.clone(), p.name.clone()))
            .collect();
        self.pending
            .retain(|p| !matches!(&p.kind, PendingKind::Run { run, .. } if run == run_id));
        for (t, name) in waiting {
            self.reply_read_back(
                &t,
                json!({"run": run_id, "status": status, "output": output, "error": error}),
                &name,
            );
        }
        // Plan bindings + the root note (wake policy).
        let ok = status == RunStatus::Completed;
        let note = error
            .clone()
            .or_else(|| output.as_ref().map(|o| o.to_string()))
            .unwrap_or_default();
        self.settle_plan_bindings(
            &crate::context::plan::Binding::Run {
                id: run_id.to_string(),
            },
            super::withhold::Source::Run(run_id),
            ok,
            &note,
        );
        let wake = self.settings.agent.wake_on();
        let notify = match self.settings.agent.on_workflow_finished {
            crate::config::settings::OnWorkflowFinished::Ignore => false,
            _ => {
                ok && wake.contains(&crate::config::settings::WakeEvent::WorkflowFinished)
                    || !ok && wake.contains(&crate::config::settings::WakeEvent::WorkflowFailed)
            }
        };
        if notify && !self.job_shape {
            // The note lands in the root transcript, which is a read-back
            // like any other (`withhold`).
            let note = self.withhold_text(
                &super::withhold::Reader::Conversation(Some(crate::context::ROOT.to_string())),
                super::withhold::Source::Run(run_id),
                note,
                "agent.on_workflow_finished",
            );
            let short = if note.chars().count() > 400 {
                format!("{}…", note.chars().take(400).collect::<String>())
            } else {
                note.clone()
            };
            let line = format!(
                "workflow {workflow} run {run_id} {}: {short}",
                status.as_str()
            );
            match self.settings.agent.on_workflow_finished {
                // `note` appends to the root transcript and waits for whatever
                // happens next to read it. `think` delivers, which starts a
                // turn: the difference between leaving a message and making
                // the call. The hop depth continues this run's chain, so a
                // workflow the agent started cannot wake it without bound.
                crate::config::settings::OnWorkflowFinished::Think => {
                    let depth = self.runs.get(run_id).map(|r| r.msg_depth).unwrap_or(0) + 1;
                    let cap = self.settings.limits.message_depth();
                    if depth > cap {
                        self.run_log(run_id).warn(
                            "message.too_deep",
                            json!({"run": run_id, "reason": "on_workflow_finished",
                                   "depth": depth, "max": cap}),
                        );
                        self.note_root(line);
                    } else {
                        let principal = self.runs.get(run_id).and_then(|r| r.principal.clone());
                        let session = self.runs.get(run_id).and_then(|r| r.session.clone());
                        let trace = self.run_trace(run_id);
                        if let Err(e) = self.accept_event_in(
                            kinds::A2A_MESSAGE,
                            principal,
                            session,
                            json!({"text": line.clone(), "context_id": crate::context::ROOT,
                                   "msg_depth": depth}),
                            trace.as_ref(),
                        ) {
                            self.run_log(run_id)
                                .warn("workflow.think.fail", json!({"run": run_id, "err": e}));
                            self.note_root(line);
                        }
                    }
                }
                _ => self.note_root(line),
            }
        }
        // A `loop` start re-arms the next iteration; `event` start nodes fire on
        // workflow.finished/failed.
        if let Some((wf, node, spec, kind)) = self.run_start_spec(run_id)
            && kind == "loop"
        {
            self.on_loop_run_finished(
                &wf,
                &node,
                &spec,
                ok,
                &output.clone().unwrap_or(Value::Null),
            );
        }
        if let Some(ev) = super::starts::run_event(status) {
            self.fire_event_starts(
                ev,
                &json!({"run": run_id, "workflow": workflow, "status": status.as_str()}),
            );
        }
        // A run started over A2A drives its task to the run's outcome.
        #[cfg(feature = "a2a")]
        if let Some(tid) = &a2a_task {
            self.a2a_task_for_run(tid, status.as_str(), output.as_ref(), error.as_deref());
        }
        self.checkpoint(false);
        if evict_after {
            self.evict_terminal_runs();
        }
    }

    /// Cancel a run: cancel its children, fail suspended waits, mark cancelled.
    pub(crate) fn cancel_run(&mut self, run_id: &str, reason: &str) {
        // Cascade to child runs started with `cascade: true` — a cancelled
        // parent must not leave its children running unattended.
        let kids: Vec<String> = self
            .runs
            .values()
            .filter(|r| {
                !r.status.is_terminal()
                    && r.parent.as_ref().is_some_and(|p| {
                        p["run"].as_str() == Some(run_id) && p["cascade"].as_bool().unwrap_or(true)
                    })
            })
            .map(|r| r.id.clone())
            .collect();
        for k in kids {
            self.cancel_run(&k, "parent run cancelled");
        }
        self.cancel_children_of_run(run_id, reason);
        let timers = self.timers.owned_by(|o| o["run"].as_str() == Some(run_id));
        for t in timers {
            let _ = self.timers.disarm(&self.durable, &t);
        }
        self.pending
            .retain(|p| !matches!(&p.target, Target::Step(r, _) if r == run_id));
        if let Some(r) = self.runs.get_mut(run_id)
            && !r.status.is_terminal()
        {
            r.finish(RunStatus::Cancelled, None, Some(reason.to_string()));
            self.on_run_terminal(run_id);
        }
    }

    fn cancel_children_of_run(&mut self, run_id: &str, reason: &str) {
        let nodes: Vec<_> = self
            .children
            .iter()
            .filter(|(_, c)| matches!(&c.kind, ChildKind::StepTurn { run, .. } if run == run_id))
            .map(|(n, _)| *n)
            .collect();
        for n in nodes {
            self.children.cancel(n, reason);
        }
    }

    // ---- workflow.* tools ------------------------------------------------------

    pub(crate) fn workflow_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        // The model acts for whoever drives its turn, and no further: a tool
        // that names a run acts on it only for the run's owner, exactly as the
        // A2A op of the same name would for that caller. Not-yours reads as
        // unknown. `None` is the runtime acting for itself, as before.
        let acting = self.acting_principal(caller.principal.as_deref());
        if matches!(
            name,
            "workflow.status"
                | "workflow.cancel"
                | "workflow.wait"
                | "workflow.pause"
                | "workflow.resume"
                | "workflow.signal"
        ) && let Some(p) = &acting
            && let Some(id) = args.get("run").and_then(Value::as_str)
            && self.owned_run(p, id).is_none()
        {
            return err(format!("no such run {id:?}"));
        }
        let visible = |r: &RunState| {
            acting
                .as_ref()
                .is_none_or(|p| super::reactor::may_act_on(p, r.principal.as_deref()))
        };
        match name {
            "workflow.run" => {
                let wname = args["name"].as_str().unwrap_or("").to_string();
                // The one predicate the A2A op and the card ask: the grants
                // allow this workflow and its default start admits the role.
                // Without it a principal narrowed to `workflow.run:triage`
                // could run anything by asking the model to. A caller who may
                // not run a workflow cannot tell it from one that does not
                // exist, as over A2A.
                if let Some(p) = &acting
                    && !self
                        .workflows
                        .get(&wname)
                        .is_some_and(|w| Runtime::may_run(p, w))
                {
                    return err(format!(
                        "workflow.run refused: workflow {wname:?} is not runnable by {}",
                        p.id
                    ));
                }
                let Some(w) = self.workflows.get(&wname) else {
                    return err(format!("no such workflow {wname:?}"));
                };
                if let Some(cause) = self
                    .pressure
                    .refusal(w.priority == crate::engine::model::Priority::Low)
                {
                    return err(format!(
                        "workflow.run refused: {cause}; retry when it clears"
                    ));
                }
                // Anyone but the operator runs a workflow from its DEFAULT
                // start, as the A2A op does: `may_run` judged that start's
                // `roles:`, and a sibling — an operator-only `a2a` command,
                // a webhook's — was never part of what it admitted.
                let named = args.get("start").and_then(Value::as_str);
                if let (Some(p), Some(s)) = (&acting, named)
                    && !p.is_operator()
                    && default_start(w).as_deref() != Some(s)
                {
                    return err(format!(
                        "workflow.run refused: {} may run {wname:?} only from its default start, not {s:?}",
                        p.id
                    ));
                }
                let start = match named {
                    Some(s) => match w.step(s) {
                        Some(st) if st.is_start() => s.to_string(),
                        _ => return err(format!("workflow {wname:?} has no start node {s:?}")),
                    },
                    None => match default_start(w) {
                        Some(s) => s,
                        None => return err(format!("workflow {wname:?} has no start node")),
                    },
                };
                let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
                let timeout_ms = args
                    .get("timeout")
                    .and_then(Value::as_str)
                    .and_then(|t| crate::config::parse_duration(t).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(3_600_000);
                let request = match (caller.node, &caller.run, &caller.step) {
                    (Some(n), _, _) => {
                        json!({"node": n.0, "req": caller.req, "wait": wait, "timeout_ms": timeout_ms})
                    }
                    (None, Some(r), Some(s)) => {
                        json!({"run": r, "step": s, "wait": wait, "timeout_ms": timeout_ms})
                    }
                    _ => Value::Null,
                };
                // The run is handed its caller's text, and so carries what the
                // caller does (`withhold`) — a workflow tool's run included.
                let mut payload = json!({"workflow": wname, "node": start, "payload": {"requested_by": caller.label_pub()}, "inputs": args.get("inputs").cloned().unwrap_or(json!({})), "request": request, "conversation": caller.ctx, "msg_depth": caller.msg_depth, "carries": self.caller_carries(caller)});
                // A subagent's call, or a run one started, starts a run no
                // delegated call is made for (RFC 0044 P1).
                if let Some(h) = self.subagent_lineage(caller) {
                    payload["subagent"] = json!(h);
                }
                // A model's call starts a run whose inputs are the model's.
                if self.model_asked(caller) {
                    payload["by_model"] = json!(true);
                }
                // The run continues its caller's trace.
                let trace = self.caller_trace(caller);
                match self.accept_event_in(
                    kinds::WORKFLOW_RUN,
                    caller.principal.clone(),
                    caller.session.clone(),
                    payload,
                    trace.as_ref(),
                ) {
                    Ok(_) => {
                        // Process it right away so the caller learns the run id.
                        if let Some(ev) = self.inbox_queue.pop_back() {
                            let done = self.on_start_event(&ev);
                            if done {
                                self.inbox_done(&ev.id);
                            }
                        }
                        // The reply is (or will be) delivered through the request
                        // target: immediately with the run id, or when the run
                        // finishes for `wait: true` (registered by `on_start_event`).
                        ToolOutcome::Executing
                    }
                    Err(e) => err(e),
                }
            }
            "workflow.list" => ToolOutcome::Ready(
                json!({"workflows": self.workflows.values().map(|w| json!({
                    "name": w.name, "description": w.description, "armed": w.armed, "hash": w.hash,
                    "starts": w.start_steps().iter().map(|s| json!({"node": s.id, "kind": s.kind})).collect::<Vec<_>>(),
                    "runs": self.runs.values().filter(|r| r.workflow == w.name && visible(r)).map(|r| json!({"id": r.id, "status": r.status})).collect::<Vec<_>>(),
                })).collect::<Vec<_>>()}),
                false,
            ),
            "workflow.status" => {
                let runs: Vec<Value> = match (
                    args.get("run").and_then(Value::as_str),
                    args.get("name").and_then(Value::as_str),
                ) {
                    // One run in full, with what the taint engine knows of
                    // it — why a step of it met a consent gate or a policy
                    // fact (RFC 0044 §5.13).
                    (Some(id), _) => self
                        .runs
                        .get(id)
                        .map(|r| {
                            let mut d = run_detail(r);
                            d["taint"] = self.run_facts(id).to_value();
                            vec![d]
                        })
                        .unwrap_or_default(),
                    (None, Some(n)) => self
                        .runs
                        .values()
                        .filter(|r| r.workflow == n && visible(r))
                        .map(RunState::summary)
                        .collect(),
                    _ => self
                        .runs
                        .values()
                        .filter(|r| visible(r))
                        .map(RunState::summary)
                        .collect(),
                };
                ToolOutcome::Ready(json!({"runs": runs}), false)
            }
            "workflow.cancel" => {
                let id = args["run"].as_str().unwrap_or("").to_string();
                if !self.runs.contains_key(&id) {
                    return err(format!("no such run {id:?}"));
                }
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("cancelled by request")
                    .to_string();
                self.cancel_run(&id, &reason);
                ToolOutcome::Ready(
                    json!({"ok": true, "status": self.runs.get(&id).map(|r| r.status.as_str()).unwrap_or("cancelled")}),
                    false,
                )
            }
            "workflow.wait" => {
                let id = args["run"].as_str().unwrap_or("").to_string();
                let timeout_ms = args
                    .get("timeout")
                    .and_then(Value::as_str)
                    .and_then(|t| crate::config::parse_duration(t).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(3_600_000);
                match self.runs.get(&id) {
                    None => err(format!("no such run {id:?}")),
                    Some(r) if r.status.is_terminal() => ToolOutcome::Ready(
                        json!({"run": id, "status": r.status, "output": r.output, "error": r.error}),
                        false,
                    ),
                    Some(_) => ToolOutcome::Deferred(PendingKind::Run {
                        run: id,
                        deadline_ms: now_ms() + timeout_ms,
                    }),
                }
            }
            "workflow.pause" | "workflow.resume" => {
                let pause = name == "workflow.pause";
                // `before_step`: pause the run the moment a named step is about
                // to start, rather than immediately. This is a breakpoint —
                // "stop when you reach `notify`" — which is what you actually
                // want when debugging a graph, and it needs no new surface
                // because pause already exists and already survives a restart.
                if pause
                    && let Some(id) = args.get("run").and_then(Value::as_str)
                    && let Some(step) = args.get("before_step").and_then(Value::as_str)
                {
                    let known = self
                        .definition_for_run(id)
                        .is_some_and(|wf| wf.steps.contains_key(step));
                    if !known {
                        return err(format!(
                            "before_step {step:?} is not a step of this run's workflow"
                        ));
                    }
                    match self.runs.get_mut(id) {
                        None => return err(format!("no such run {id:?}")),
                        Some(r) => {
                            r.break_before = Some(step.to_string());
                            r.dirty = true;
                        }
                    }
                    self.run_log(id)
                        .info("run.breakpoint", json!({"run": id, "before_step": step}));
                    return ToolOutcome::Ready(json!({"run": id, "break_before": step}), false);
                }
                if let Some(id) = args.get("run").and_then(Value::as_str) {
                    match self.runs.get_mut(id) {
                        None => return err(format!("no such run {id:?}")),
                        Some(r) if r.status.is_terminal() => {
                            return err(format!("run {id:?} is already {}", r.status.as_str()));
                        }
                        Some(r) => {
                            r.status = if pause {
                                RunStatus::Paused
                            } else {
                                RunStatus::Running
                            };
                            r.touch();
                        }
                    }
                    return ToolOutcome::Ready(json!({"ok": true}), false);
                }
                if let Some(n) = args.get("name").and_then(Value::as_str) {
                    match self.workflows.get_mut(n) {
                        None => return err(format!("no such workflow {n:?}")),
                        // The definitions are shared (`Arc`) on the hot path;
                        // arming is the one mutation, and it is rare —
                        // copy-on-write is the honest cost here.
                        Some(w) => std::sync::Arc::make_mut(w).armed = !pause,
                    }
                    if !pause {
                        self.arm_workflows();
                    }
                    return ToolOutcome::Ready(json!({"ok": true}), false);
                }
                err(format!("{name}: give run or name"))
            }
            "workflow.create" | "workflow.update" => {
                // Workflows are STANDING instructions — what the agent does
                // when a schedule fires or a webhook lands, unattended. An
                // agent that can rewrite them changes what happens next time,
                // and the change outlives the conversation that caused it.
                if let Some(e) = self.workflows_locked(name) {
                    return e;
                }
                // While the instruction is frozen, what a model writes is the
                // document's, and a definition outlives the freeze: a person
                // who later runs it would act with their grant on it (RFC
                // 0044 §5.12 I).
                #[cfg(feature = "xaa")]
                if self.freshness_frozen && self.delegation.is_some() && self.model_asked(caller) {
                    return err(format!(
                        "{name} refused: the instruction document could not be confirmed current (agent.instruction.trust freshness), and until it is no model writes a workflow a person's grant could act on"
                    ));
                }
                let def = args["definition"].clone();
                // The `name` argument names the workflow the call is aimed
                // at, so an update of a configured workflow is refused as
                // that, not as a missing name, whatever the definition says.
                if let Some(n) = args.get("name").and_then(Value::as_str)
                    && let Some(e) = self.workflow_configured(name, n.trim())
                {
                    return e;
                }
                let parsed = parse_workflow(&def);
                // …and the name the definition would be stored and installed
                // under is the one parsing settled (trimmed), not the string
                // as sent: `" tick "` is `tick`. Judged before anything else
                // about the definition, so `create` never answers "exists
                // (use workflow.update)" for a configured name.
                if let Ok(w) = &parsed
                    && let Some(e) = self.workflow_configured(name, &w.name)
                {
                    return e;
                }
                match parsed {
                    Err(e) => err(format!("{name}: {}", e.join("; "))),
                    Ok(w) if w.tool.is_some() => err(format!(
                        "{name}: a `tool:` block may only be declared in the startup config.                          The tool registry is built once and validated fail-closed; minting                          or shadowing a tool name at runtime would put no operator in the                          loop. (workflow {:?})",
                        w.name
                    )),
                    Ok(mut w) => {
                        self.fill_durable_default(&mut w);
                        if name == "workflow.create" && self.workflows.contains_key(&w.name) {
                            return err(format!(
                                "workflow {:?} exists (use workflow.update)",
                                w.name
                            ));
                        }
                        if name == "workflow.update" && !self.workflows.contains_key(&w.name) {
                            return err(format!(
                                "workflow {:?} does not exist (use workflow.create)",
                                w.name
                            ));
                        }
                        // A route on the operator's webhook listener answers
                        // the network with the auth the definition names, so
                        // a definition the agent writes opens one only under
                        // the grant a served document needs. Judged here, in
                        // the one handler every caller of the tool reaches.
                        let mut problems = crate::config::settings::stored_route_refusals(
                            &def,
                            &self.settings.agent.document_capabilities,
                        );
                        // Checked now against what is running, as a start
                        // checks a configured one: a definition naming a
                        // tool, server or stream this agent does not have is
                        // refused here rather than stored to fail when it
                        // runs.
                        if problems.is_empty() {
                            problems = self.reference_errors(&w, &self.registry, &self.skills);
                        }
                        // The taint check, over the running set with this
                        // definition in place of any it replaces — the same
                        // line a configured definition is held to at load.
                        if problems.is_empty() {
                            problems = crate::config::taint::refusals(
                                &self.settings,
                                &self
                                    .workflows
                                    .values()
                                    .map(AsRef::as_ref)
                                    .filter(|o: &&Workflow| o.name != w.name)
                                    .chain(std::iter::once(&w))
                                    .collect::<Vec<_>>(),
                            );
                        }
                        if !problems.is_empty() {
                            return err(format!("{name}: {}", problems.join("; ")));
                        }
                        let (wname, hash) = (w.name.clone(), w.hash.clone());
                        // Durable definition (memory/_workflows/<name>).
                        let rec = crate::context::memory::Record {
                            value: def,
                            ts: now_ms(),
                            ttl_ms: None,
                            by: Some(caller.label_pub()),
                            carries: Default::default(),
                        };
                        if let Err(e) = self.durable.put(
                            Kind::Memory,
                            &format!("{WORKFLOW_DEF_PREFIX}{wname}"),
                            serde_json::to_value(&rec).unwrap_or(Value::Null),
                            None,
                        ) {
                            return err(format!("{name}: store: {e}"));
                        }
                        let arm = args.get("arm").and_then(Value::as_bool).unwrap_or(true);
                        let mut w = w;
                        w.armed = arm;
                        self.workflows.insert(wname.clone(), std::sync::Arc::new(w));
                        self.log.info(
                            "workflow.defined",
                            json!({"name": wname, "hash": &hash[..12], "op": name}),
                        );
                        self.retag_workflow_tools();
                        if arm {
                            self.arm_workflows();
                        }
                        ToolOutcome::Ready(
                            json!({"name": wname, "hash": hash, "armed": arm}),
                            false,
                        )
                    }
                }
            }
            "workflow.delete" => {
                if let Some(e) = self.workflows_locked(name) {
                    return e;
                }
                let wname = args["name"].as_str().unwrap_or("").to_string();
                if let Some(e) = self.workflow_configured(name, &wname) {
                    return e;
                }
                let Some(wf) = self.workflows.remove(&wname) else {
                    return err(format!("no such workflow {wname:?}"));
                };
                let _ = self
                    .durable
                    .delete(Kind::Memory, &format!("{WORKFLOW_DEF_PREFIX}{wname}"));
                // Retire rather than drop: retirement pins the definition so
                // live runs keep resolving `definition_for_run` mid-flight, and
                // applies the workflow's `unload:` policy to them. Delete means
                // "stop being a workflow", not "strand whatever is in flight".
                // It also re-derives the read-back contracts' tags.
                self.retire_workflow(&wf, "deleted");
                self.log.info("workflow.deleted", json!({"name": wname}));
                ToolOutcome::Ready(json!({"ok": true}), false)
            }
            "workflow.signal" => {
                let sname = args["name"].as_str().unwrap_or("").to_string();
                // A run the signal starts continues the sender's trace.
                let trace = self.caller_trace(caller);
                let mut signal = json!({"name": sname, "payload": args.get("payload").cloned().unwrap_or(Value::Null), "run": args.get("run"), "from": caller.label_pub(), "carries": self.caller_carries(caller)});
                // A run a subagent's signal starts is no delegated call's.
                if let Some(h) = self.subagent_lineage(caller) {
                    signal["subagent"] = json!(h);
                }
                // A run a model's signal starts or wakes has the model's
                // payload.
                if self.model_asked(caller) {
                    signal["by_model"] = json!(true);
                }
                let _ = self.accept_event_in(
                    kinds::SIGNAL,
                    caller.principal.clone(),
                    caller.session.clone(),
                    signal,
                    trace.as_ref(),
                );
                // The signal goes on the durable inbox; waits and `signal`
                // start nodes are woken when the loop drains it, so no
                // delivery count is available at this point.
                ToolOutcome::Ready(
                    json!({"delivered": 0, "note": "signal recorded; waits and signal start nodes are woken when the loop drains the inbox"}),
                    false,
                )
            }
            _ => err(format!("unknown workflow tool {name}")),
        }
    }
}

impl Runtime {
    /// The subagent behind `caller`'s work: itself, or the subagent whose call
    /// started the run it works in, directly or through the runs that one
    /// started (RFC 0044 P1: a subagent never acts with a person's grant).
    pub(crate) fn subagent_lineage(&self, caller: &ToolCaller) -> Option<String> {
        caller.subagent.clone().or_else(|| {
            caller
                .run
                .as_ref()
                .and_then(|r| self.runs.get(r))
                .and_then(|r| r.subagent.clone())
        })
    }

    /// Whether a model asked for what `caller` asks for: a turn's or a
    /// subagent's call, or a step of a run a model asked for
    /// ([`RunState::by_model`]).
    pub(crate) fn model_asked(&self, caller: &ToolCaller) -> bool {
        caller.node.is_some()
            || self.subagent_lineage(caller).is_some()
            || caller
                .run
                .as_ref()
                .and_then(|r| self.runs.get(r))
                .is_some_and(|r| r.by_model)
    }
}

impl ToolCaller {
    pub(crate) fn label_pub(&self) -> String {
        if let Some(s) = &self.subagent {
            return format!("subagent:{s}");
        }
        if let (Some(r), Some(s)) = (&self.run, &self.step) {
            return format!("step:{r}/{s}");
        }
        format!(
            "ctx:{}",
            self.ctx.as_deref().unwrap_or(crate::context::ROOT)
        )
    }
}

/// The start node `workflow.run` uses by default: `manual`, else the first.
fn default_start(w: &Workflow) -> Option<String> {
    w.default_start().map(|s| s.id.clone())
}

fn node_kind<'a>(w: &'a Workflow, node: &str) -> Option<&'a str> {
    w.step(node).map(|s| s.kind.as_str())
}

fn run_detail(r: &RunState) -> Value {
    let mut v = r.summary();
    v["step_states"] = json!(r.steps);
    v["vars"] = Value::Object(r.vars.clone());
    v
}

/// The `memory.<key>` roots a value's templates reference.
fn collect_memory_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find("memory.") {
                let after = &rest[i + "memory.".len()..];
                let key: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | ':'))
                    .collect();
                if !key.is_empty() && !out.contains(&key) {
                    out.push(key.clone());
                }
                rest = &after[key.len().min(after.len())..];
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_memory_keys(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_memory_keys(x, out)),
        _ => {}
    }
}

/// The configured workflow entries as a flat list of documents, each with
/// the source a refusal names it by. Reads folders; installs nothing — and
/// needs nothing but the settings, so `--validate-config` expands the same
/// folders the loader does.
pub(crate) fn workflow_documents(
    settings: &crate::config::settings::Settings,
    errs: &mut Vec<String>,
) -> Vec<WorkflowDocument> {
    // A `{dir}` entry expands into one entry per matching file BEFORE
    // resolution, so everything downstream — parsing, naming, the duplicate
    // check — sees a plain list of documents and needs no directory case.
    // Each document travels with where it came from, so two that resolve
    // to one name can be refused naming both.
    let mut docs: Vec<WorkflowDocument> = Vec::new();
    for (i, doc) in settings.workflows.clone().into_iter().enumerate() {
        let from_document = settings.agent.document_workflows.contains(&i);
        // The ENTRY fold runs here, BEFORE the dir expansion, because a
        // `dir:` is consumed by that expansion and would never reach the
        // per-document fold below — `{{config.wf_dir}}` went to the
        // filesystem verbatim and failed as "not a directory". `file:` and
        // `url:` are folded again below (idempotent: a folded string has no
        // tokens left), which keeps the inline case reporting an unresolved
        // reference exactly once.
        let mut doc = doc;
        if doc.get("steps").is_none() {
            substitute_config_vars(&mut doc, &settings.vars, "workflow entry", errs);
        }
        // `dir:` is the folder source shared with instructions: a path,
        // or `{path, glob, order}`. `glob` and `order` live INSIDE it —
        // one home for the settings that qualify a folder, here as
        // everywhere else.
        let entry_dir = match doc.get("dir") {
            None => None,
            Some(v) => match serde_json::from_value::<Dir>(v.clone()) {
                Ok(dir) => {
                    if doc.get("glob").is_some() {
                        errs.push(
                            "workflow entry: `glob` beside `dir` — it belongs inside \
                             (`dir: {path: …, glob: …}`), which is where `order` lives too"
                                .to_string(),
                        );
                    }
                    Some(dir)
                }
                Err(e) => {
                    errs.push(format!("workflow entry dir: {e}"));
                    None
                }
            },
        };
        match entry_dir {
            None => docs.push(WorkflowDocument {
                entry: doc,
                source: settings.workflow_entry_source(i),
                from_document,
            }),
            Some(dir) => {
                let pattern = dir.glob().unwrap_or("*.yaml,*.yml,*.json");
                match expand_dir_ordered(dir.path(), pattern, dir.order()) {
                    Ok(paths) if paths.is_empty() => {
                        // Silence here would mean a schedule that never
                        // fires and no way to tell why.
                        errs.push(format!(
                            "workflow dir {}: no file matched {pattern:?}",
                            dir.path()
                        ));
                    }
                    Ok(paths) => {
                        for path in paths {
                            let mut d = json!({"file": path});
                            if let Some(a) = doc.get("armed") {
                                d["armed"] = a.clone();
                            }
                            let source =
                                format!("file {path} (from {})", settings.workflow_entry_source(i));
                            docs.push(WorkflowDocument {
                                entry: d,
                                source,
                                from_document,
                            });
                        }
                    }
                    Err(e) => errs.push(format!("workflow dir {}: {e}", dir.path())),
                }
            }
        }
    }
    docs
}

/// A `file:` entry's document, read from the local filesystem as a
/// definition. Shared with `--validate-config`, which reads the same files.
/// A file on disk is held to the config file's rule for credentials: a
/// webhook verifier's secret is a reference.
pub(crate) fn read_workflow_file(path: &str, entry: &Value) -> Result<Value, String> {
    let def = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| {
            crate::config::file::parse_document(
                &t,
                crate::config::file::Format::detect(Some(std::path::Path::new(path)), &t),
            )
        })
        .and_then(|d| as_definition(d, entry))?;
    let mut secrets = Vec::new();
    crate::config::settings::webhook_secrets(&def, "", &mut secrets);
    if let Some((at, _)) = secrets
        .iter()
        .find(|(_, v)| !crate::sec::secret::has_secret_ref(v))
    {
        return Err(format!(
            "{at} carries an inline credential; use {{{{secret:NAME}}}} / {{{{secret-file:PATH}}}}"
        ));
    }
    Ok(def)
}

/// A resolved document as a definition: a mapping, which takes its entry's
/// `name` when it has none of its own. Anything else — a resource that
/// serves plain text, say — is refused here, naming the source: filling the
/// name in by index panicked on it, and on a reload that took the daemon
/// down.
fn as_definition(mut resolved: Value, entry: &Value) -> Result<Value, String> {
    let Some(obj) = resolved.as_object_mut() else {
        return Err("the document is not a workflow definition (a mapping)".into());
    };
    if !obj.contains_key("name")
        && let Some(n) = entry.get("name")
    {
        obj.insert("name".into(), n.clone());
    }
    Ok(resolved)
}

/// Read a resource URI (`mcp://<server>/<uri>`, or a URI one of `mcp`'s
/// servers serves) through the connection set given — the running one, or
/// the one a reload has staged — and never through a server `company` says
/// is a Company's.
pub(crate) fn read_resource_any(
    mcp: &BTreeMap<String, std::sync::Arc<crate::mcp::client::McpClient>>,
    uri: &str,
    company: &dyn Fn(&str) -> bool,
) -> Result<String, String> {
    if let Some(rest) = uri.strip_prefix("mcp://") {
        let (server, res) = rest
            .split_once('/')
            .ok_or("mcp:// uri needs <server>/<resource-uri>")?;
        if company(server) {
            return Err(format!(
                "mcp server {server:?} is a Company's, and a definition is never read from one"
            ));
        }
        let c = mcp
            .get(server)
            .ok_or_else(|| format!("mcp server {server:?} is not connected"))?;
        return c
            .read_resource(res, None)
            .map(|r| r.text())
            .map_err(|e| e.to_string());
    }
    let mut last = String::from("no connected server serves it");
    for (_, c) in mcp.iter().filter(|(n, _)| !company(n)) {
        match c.read_resource(uri, None) {
            Ok(r) => return Ok(r.text()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// `text` with every part of `url` that `url_locator` drops taken out — the
/// userinfo, the query, the fragment — and the URL itself named by its
/// locator. An error about a fetch can echo the URL whole (one that does not
/// parse) or its host with the userinfo still on it (the resolver's), and the
/// error goes where the URL may not: the log and a reload's refusal.
fn scrub_url(text: &str, url: &str) -> String {
    let mut out = text.replace(url, &crate::config::settings::url_locator(url));
    // The userinfo runs to the last `@` of the authority, found before any
    // `?` or `#` is looked for: a raw one inside a password does not end it.
    let mut after_userinfo = 0;
    if let Some(start) = url.find("://").map(|i| i + 3) {
        let rest = &url[start..];
        let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
        if let Some(at) = authority.rfind('@') {
            out = out.replace(&format!("{}@", &authority[..at]), "");
            after_userinfo = start + at + 1;
        }
    }
    // The query and the fragment each on its own, as well as together: an
    // error echoing the request line carries the query without the fragment.
    let tail = &url[after_userinfo..];
    if let Some(q) = tail.find(['?', '#']) {
        let tail = &tail[q..];
        out = out.replace(tail, "");
        let (query, fragment) = tail.split_at(tail.find('#').unwrap_or(tail.len()));
        for part in [query, fragment] {
            if part.len() > 1 {
                out = out.replace(part, "");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A definition is never read from a Company's server: named, it is
    /// refused as one; unnamed, that server is not asked.
    #[test]
    fn a_definition_is_never_read_from_a_company() {
        let shop = std::sync::Arc::new(
            crate::mcp::client::McpClient::connect(
                "shop",
                "https://127.0.0.1:9/mcp",
                Vec::new(),
                std::time::Duration::from_millis(200),
            )
            .unwrap(),
        );
        let mcp = BTreeMap::from([("shop".to_string(), shop)]);
        let company = |n: &str| n == "shop";
        let e = read_resource_any(&mcp, "mcp://shop/flow.yaml", &company).unwrap_err();
        assert!(e.contains("is a Company's"), "{e}");
        let e = read_resource_any(&mcp, "file:///flow.yaml", &company).unwrap_err();
        assert_eq!(
            e, "no connected server serves it",
            "the Company's server was asked"
        );
        // Another server is asked as before.
        let e = read_resource_any(&mcp, "file:///flow.yaml", &|_| false).unwrap_err();
        assert_ne!(e, "no connected server serves it");
    }

    /// Every part of a URL that `url_locator` drops is taken out of an error
    /// about it: echoed whole (a URL that does not parse), or as a host with
    /// the userinfo still on it (the resolver's).
    #[test]
    fn a_fetch_error_loses_the_urls_userinfo_and_query() {
        let url = "http://ops:pw@host.example:8080/defs.yaml?token=s3cr3t#frag";
        let echoed = format!("bad port in {url}");
        assert_eq!(
            scrub_url(&echoed, url),
            "bad port in http://host.example:8080/defs.yaml"
        );
        let resolver = "host `ops:pw@host.example` rejected: resolve failed";
        assert_eq!(
            scrub_url(resolver, url),
            "host `host.example` rejected: resolve failed"
        );
        let query_alone = "GET /defs.yaml?token=s3cr3t#frag: HTTP 404";
        assert_eq!(scrub_url(query_alone, url), "GET /defs.yaml: HTTP 404");
        // The request line an HTTP client sends carries the query without
        // the fragment.
        let request_line = "GET /defs.yaml?token=s3cr3t: HTTP 404";
        assert_eq!(scrub_url(request_line, url), "GET /defs.yaml: HTTP 404");
        // A raw `?` or `#` inside the password does not end the userinfo.
        let raw = "http://ops:p?w#d@host.example/defs.yaml";
        assert_eq!(
            scrub_url("host `ops:p?w#d@host.example` rejected", raw),
            "host `host.example` rejected"
        );
        assert_eq!(
            crate::config::settings::url_locator(raw),
            "http://host.example/defs.yaml"
        );
        // A URL with nothing to hide leaves the text as it was.
        assert_eq!(
            scrub_url("HTTP 404", "https://defs.example/a.yaml"),
            "HTTP 404"
        );
    }

    /// A resolved document that is not a mapping is refused, not indexed
    /// into — `d["name"] = …` on a string panicked, and on a reload that took
    /// the daemon down. A mapping takes its entry's name when it has none.
    #[test]
    fn only_a_mapping_is_a_workflow_definition() {
        let entry = json!({"name": "from-entry", "uri": "mcp://s/x"});
        assert!(as_definition(json!("plain text"), &entry).is_err());
        assert!(as_definition(json!([1, 2]), &entry).is_err());
        assert_eq!(
            as_definition(json!({"steps": {}}), &entry).unwrap()["name"],
            "from-entry"
        );
        assert_eq!(
            as_definition(json!({"name": "own", "steps": {}}), &entry).unwrap()["name"],
            "own"
        );
    }
}
