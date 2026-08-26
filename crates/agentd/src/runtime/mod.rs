// SPDX-License-Identifier: AGPL-3.0-only
//! The **agentd runtime**: the supervisor's event loop over durable state, the
//! turn workers it spawns, and the lifecycle policy.
//!
//! Startup is strictly ordered, because each step depends on the last:
//! parse+validate config → connect MCP servers (a failed server is contained,
//! not fatal) → connect the store or refuse to start → restore → build the
//! registry, validating overrides against the servers that actually answered →
//! discover skills → resolve the instruction → load workflows → arm start
//! nodes (`once` fires unless a live run was restored, so a restart does not
//! re-fire it) → re-spawn pending subagents → announce `proc.ready` → enter
//! the loop. Nothing accepts outside work before `proc.ready`.

#[cfg(feature = "a2a")]
pub mod a2a_server;
pub mod activity;
pub mod artifacts;
pub mod audit;
pub mod breaker;
pub mod children;
pub mod env; // system-prompt data + the default template
pub mod events;
#[cfg(feature = "exec")]
pub mod exec; // guarded local command runner behind the `exec` tool (default-OFF)
pub mod goal;
pub mod http_node;
pub mod human; // human-in-the-loop: ask_human gates + fallbacks
pub(crate) mod instances; // instance-tier template children (a full daemon each)
pub mod nested;
pub mod pressure; // disk/memory pressure: shed new work, drain what is in flight
pub mod reactor;
pub mod reload;
pub(crate) mod retire;
pub mod starts;
pub mod steps;
pub(crate) mod streams;
pub mod subagents;
pub mod timers;
pub mod tools;
pub mod turns;
pub mod waits;
#[cfg(feature = "a2a")]
pub mod webhooks;
pub mod worker;

pub use reactor::Runtime;

use crate::config::v2::{Loaded, StoreKind};
use crate::context::memory::Memory;
use crate::context::{Contexts, skills, tokens};
use crate::engine::run::StepStatus;
use crate::governor::Governor;
use crate::mcp::client::McpClient;
use crate::obs::log::{Comp, Level, LogCtx, Logger};
use crate::registry::{Registry, ServerTools};
use crate::state::{Durable, Kind, Policy, now_ms};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Check every secret reference in `doc`; prompt for the promptable ones when
/// `--prompt-missing` was given and a controlling terminal exists; report
/// whatever is still missing — all of it, together — and return the exit code
/// if startup cannot proceed.
///
/// Only `{{secret:NAME}}` is promptable: a missing `{{secret-file:…}}` is a
/// path that does not exist (typing its CONTENT at a prompt would not make the
/// file appear), and an undefined `{{config.…}}` is an authoring error whose
/// fix belongs in the file, not in a terminal that will forget it.
fn reference_preflight(
    doc: &Value,
    settings: &crate::config::v2::Settings,
    at: &str,
    log: &Logger,
) -> Option<i32> {
    let mut missing = crate::config::v2::missing_references(doc, at, &settings.vars);
    if missing.is_empty() {
        return None;
    }
    if crate::config::prompt::prompt_missing_requested() {
        let mut found = Vec::new();
        crate::config::v2::scan_references(doc, at, &mut found);
        let mut names: Vec<String> = found
            .into_iter()
            .filter(|r| r.kind == "secret" && !crate::sec::secret::secret_available(&r.name))
            .map(|r| r.name)
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            match crate::config::prompt::read_secret_from_tty(&format!("{name} (secret)")) {
                Ok(v) => crate::sec::secret::set_prompted(&name, v),
                Err(e) => {
                    log.error("prompt.failed", json!({"secret": name, "err": e}));
                    break;
                }
            }
        }
        missing = crate::config::v2::missing_references(doc, at, &settings.vars);
        if missing.is_empty() {
            return None;
        }
    }
    for m in &missing {
        log.error("config.invalid", json!({"error": m}));
    }
    log.error(
        "proc.exit",
        json!({"code": crate::exit::USAGE, "err": format!("{} unresolved reference(s)", missing.len())}),
    );
    Some(crate::exit::USAGE)
}

/// Start the runtime for a loaded configuration and block until it stops.
/// Returns the process exit code: startup failures report before the loop is
/// entered, so a non-zero return here is always a refusal to run rather than a
/// partially started daemon.
pub fn run(loaded: &Loaded, args: &[String], env: &[(String, String)]) -> i32 {
    let settings = loaded.settings.clone();
    let instance = settings.instance_name();
    let run_id = match settings.lifecycle.run_id.clone() {
        Some(r) => r,
        None => crate::state::ulid::new(),
    };
    let trace = crate::obs::trace::resolve(&run_id, settings.observability.traceparent.as_deref());
    let level = settings
        .observability
        .log_level
        .as_deref()
        .and_then(Level::parse)
        .unwrap_or(Level::Info);
    let log = Logger::new(
        LogCtx {
            run_id: run_id.clone(),
            agent_id: "sup".into(),
            agent_path: "0".into(),
            comp: Comp::Supervisor,
            pid: std::process::id(),
            trace_id: Some(trace.trace_id.clone()),
        },
        level,
    )
    .with_content(settings.observability.log_content);
    log.info("proc.start", json!({"version": crate::VERSION, "runtime": "1", "instance": instance, "config_files": loaded.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()}));
    for w in &loaded.warnings {
        log.warn("config.warning", json!({"warning": w}));
    }
    crate::signals::install();
    crate::supervisor::reap::set_child_subreaper();

    // The reference preflight, phase 1: every `{{secret:…}}` / `{{secret-file:…}}`
    // visible in the assembled document, checked BEFORE anything dials out —
    // reported together, optionally filled in interactively. Phase 2 runs after
    // workflow loading, for definitions that arrive from files and URLs.
    if let Some(code) = reference_preflight(&loaded.doc, &settings, "config", &log) {
        return code;
    }
    let envmap = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    // Outbound trust anchor.
    #[cfg(feature = "tls")]
    if let Some(path) = settings.security.tls_ca.as_deref()
        && let Err(e) = std::fs::read(path).and_then(|pem| crate::net::tls::install_extra_ca(&pem))
    {
        log.error(
            "proc.exit",
            json!({"code": crate::exit::USAGE, "err": format!("security.tls_ca {path}: {e}")}),
        );
        return crate::exit::USAGE;
    }
    // AAuth identity — signs outbound MCP requests tree-wide. Set up before
    // any server is dialed, so no request can leave unsigned.
    #[cfg(feature = "aauth")]
    if let Some(a) = &settings.security.aauth {
        let v1 = crate::config::AAuthSettings {
            provider: a.provider.clone(),
            key_file: a
                .key_file
                .clone()
                .unwrap_or_else(|| "/var/lib/agentd/aauth-key".into()),
            enrollment_token: a.enroll_token.as_ref().map(|s| s.0.clone()),
            enroll_assertion_file: a.enroll_assertion_file.clone(),
            person_server: a.person_server.clone(),
        };
        if let Err(e) = crate::aauth::setup(&v1, Duration::from_secs(30)) {
            log.error(
                "proc.exit",
                json!({"code": crate::exit::USAGE, "err": format!("aauth: {e}")}),
            );
            return crate::exit::USAGE;
        }
    }

    // Resource containment: arm the process-tree cgroup so
    // each spawned child (turn workers + subagents) is placed in its own leaf
    // with the configured `memory.max`/`pids.max`, and gets `cgroup.kill` atomic
    // teardown. A no-op unless `security.cgroup.spec` is set.
    #[cfg(unix)]
    if settings.security.cgroup.spec.is_some() {
        let c = &settings.security.cgroup;
        if let Some(configured) = crate::supervisor::cgroup::configure(
            c.spec.as_deref(),
            c.memory_max.as_deref(),
            c.pids_max.as_deref(),
        ) {
            log.info("cgroup.armed", json!({"parent": configured.parent.display().to_string(), "limits_unavailable": configured.limits_unavailable}));
        }
    }

    // Intelligence.
    let intel_uri = settings.intelligence.endpoint_list().unwrap_or_default();
    let intel_token = match resolve_intel_token(&settings, &envmap) {
        Ok(t) => t,
        Err(e) => {
            log.error("proc.exit", json!({"code": crate::exit::USAGE, "err": e}));
            return crate::exit::USAGE;
        }
    };
    let model = settings.intelligence.model.clone().unwrap_or_default();
    // Resolve `intelligence.headers` once here — they are applied per dial —
    // plus an optional OAuth credential provider that refreshes its bearer.
    // A header whose secret cannot be resolved is dropped rather than sent
    // with an unresolved placeholder in it.
    let intel_headers: Vec<(String, String)> = settings
        .intelligence
        .headers
        .iter()
        .filter_map(|(k, v)| {
            crate::sec::secret::resolve(v, &envmap)
                .ok()
                .map(|r| (k.clone(), r))
        })
        .collect();
    let intel_bearer = intel_bearer_provider(&settings);

    // MCP servers (contained failures: a down server is logged; tools that need
    // it are unavailable; the store server must be up).
    let mut mcp: BTreeMap<String, Arc<McpClient>> = BTreeMap::new();
    let mut mcp_specs = BTreeMap::new();
    let mut server_tools: Vec<ServerTools> = Vec::new();
    let mcp_timeout = settings
        .mcp
        .default_timeout
        .map(|d| d.0)
        .unwrap_or(Duration::from_secs(60));
    for s in &settings.mcp.servers {
        // The dial-time backstop behind boot validation: under `closed`
        // egress an endpoint with no service-catalog entry must never reach
        // the socket, whichever path assembled it.
        if let Err(e) = crate::config::v2::egress_allows(
            &settings.services,
            settings.security.egress,
            crate::config::v2::ServiceKind::Mcp,
            &s.endpoint,
        ) {
            log.error("proc.exit", json!({"code": crate::exit::USAGE, "err": e}));
            return crate::exit::USAGE;
        }
        let spec = match s.to_spec() {
            Ok(sp) => sp,
            Err(e) => {
                log.error("proc.exit", json!({"code": crate::exit::USAGE, "err": e}));
                return crate::exit::USAGE;
            }
        };
        let per_timeout = s.timeout.map(|d| d.0).unwrap_or(mcp_timeout);
        match crate::mcp::from_spec(&spec, per_timeout).and_then(|mut c| c.initialize().map(|()| c))
        {
            Ok(mut c) => {
                let mut meta = json!({"agent/run_id": run_id, "agent/instance": instance});
                meta["traceparent"] =
                    crate::obs::trace::outbound_traceparent(&trace.trace_id).into();
                c.set_tool_meta(meta);
                let tools = c.list_tools().unwrap_or_default();
                log.info(
                    "mcp.connect",
                    json!({"server": s.name, "tools": tools.len()}),
                );
                server_tools.push(ServerTools {
                    name: s.name.clone(),
                    ns: s.ns.clone(),
                    tags: spec.tags.clone(),
                    tools,
                });
                mcp.insert(s.name.clone(), Arc::new(c));
            }
            Err(e) => {
                log.warn(
                    "mcp.connect.fail",
                    json!({"server": s.name, "err": e.to_string()}),
                );
                crate::obs::metrics::record_mcp_connect_failure(&s.name);
            }
        }
        mcp_specs.insert(s.name.clone(), spec);
    }

    // The store. `none` means an in-process store, which is only ever the
    // right answer for a job-shaped instance: a long-lived one either
    // defaults to `file` or asks for `none` in writing, and validation
    // refuses that combination before startup gets here.
    let store = match settings.store.kind {
        StoreKind::None => {
            log.warn(
                "store.none",
                json!({"note": "no durable store: state lives in this process only (job shape)"}),
            );
            Arc::new(crate::store::memory::MemoryStore::new()) as crate::store::SharedStore
        }
        _ => {
            let mcp_ref = mcp.clone();
            match crate::store::open(&settings.store, &|name: &str| {
                mcp_ref
                    .get(name)
                    .map(|c| c.clone() as Arc<dyn crate::store::mcp::McpCall>)
            }) {
                Ok(Some(s)) => s,
                Ok(None) => Arc::new(crate::store::memory::MemoryStore::new()),
                Err(e) => {
                    log.error("proc.exit", json!({"code": crate::exit::MCP_REQUIRED_DOWN, "err": format!("store: {e}")}));
                    return crate::exit::MCP_REQUIRED_DOWN;
                }
            }
        }
    };
    let durable = Durable::new(
        store,
        settings.store.prefix(),
        &instance,
        Policy::from_settings(&settings.store),
        Some(log.clone()),
    );

    // Restore. A store that cannot be read is fatal: starting with an empty
    // view of state a previous life already wrote would silently re-run
    // finished work.
    let restored = match durable.restore() {
        Ok(r) => r,
        Err(e) => {
            log.error(
                "proc.exit",
                json!({"code": crate::exit::MCP_REQUIRED_DOWN, "err": format!("restore: {e}")}),
            );
            return crate::exit::MCP_REQUIRED_DOWN;
        }
    };
    // The file store, named out loud. Durability is a property of the
    // DIRECTORY, not of agentd: on a mounted volume this survives anything, on
    // a container's writable layer it survives a restart of this process and
    // not a reschedule. A store that implies more durability than it delivers
    // is the dangerous case, so the path, the life we are in and whether it was
    // chosen or defaulted all go on one line. Logged after `restore` because
    // that is where the manifest's `generation` becomes known — a fresh
    // instance has no manifest and is generation 1.
    if settings.store.kind == StoreKind::File {
        let root = crate::config::v2::file_store_root(&settings.store);
        log.info(
            "store.file",
            json!({
                "path": root.display().to_string(),
                "generation": restored.manifest.as_ref().map(|m| m.generation).unwrap_or(1),
                // `store.kind` absent from the effective document (files ← env ←
                // flags) is exactly what `load` defaulted to `file`.
                "defaulted": loaded.doc.pointer("/store/kind").is_none(),
                "msg": "durable state is on the local filesystem; it survives a restart of this process but not a move to another host — use store.kind mcp|http for a fleet",
            }),
        );
    }

    // The tool registry. Overrides are validated against the servers that
    // actually connected, so an override naming a tool nothing offers is a
    // startup error rather than a silent no-op.
    let registry = match Registry::build(&settings, &server_tools) {
        Ok(r) => r,
        Err(errs) => {
            for e in &errs {
                log.error("config.invalid", json!({"error": e}));
            }
            log.error(
                "proc.exit",
                json!({"code": crate::exit::USAGE, "err": "tool registry"}),
            );
            return crate::exit::USAGE;
        }
    };
    for w in &registry.warnings {
        log.warn("registry.warning", json!({"warning": w}));
    }

    // The skills catalogue, discovered from the connected MCP servers.
    let mut catalogue = skills::Catalogue::new(
        settings
            .skills
            .reference_prefix
            .as_deref()
            .unwrap_or(skills::DEFAULT_PREFIX),
        settings.skills.max_bytes.unwrap_or(32_768) as usize,
    );
    for src in &settings.skills.sources {
        match mcp.get(&src.server) {
            Some(c) => {
                let mode = match src.discover {
                    crate::config::v2::Discover::Prompts => skills::Discover::Prompts,
                    crate::config::v2::Discover::Resources => skills::Discover::Resources,
                    crate::config::v2::Discover::Auto => skills::Discover::Auto,
                };
                let found = catalogue.discover(&**c, mode, src.filter.as_deref());
                log.info(
                    "skills.discovered",
                    json!({"server": src.server, "count": found.len(), "skills": found}),
                );
            }
            None => log.warn("skills.source.unavailable", json!({"server": src.server})),
        }
    }
    if !settings.agent.inline_skills.is_empty() {
        let names = catalogue.add_inline(&settings.agent.inline_skills);
        log.info(
            "skills.discovered",
            json!({"server": "instruction", "count": names.len(), "skills": names}),
        );
    }

    // Channels.
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    // Child frames ride the SAME channel the loop parks on: a frame arriving
    // while the reactor is in `recv_timeout` must WAKE it rather than wait for
    // the next tick, or a subagent's 5 ms answer costs a full tick of latency.
    //
    // The readers send DIRECTLY into this channel — no forwarder thread — so
    // that joining a child's reader is a real ordering guarantee: everything
    // the child wrote is IN the queue when join returns, and a reap requeued
    // after it necessarily lands behind those frames. An intermediate hop
    // would break that, letting the requeued reap overtake frames still
    // sitting in the hop's own queue and settle a child before its last
    // words were read.
    let child_tx: crate::supervisor::spawn::FrameSink = {
        let events_tx = events_tx.clone();
        std::sync::Arc::new(move |node, msg| {
            events_tx.send(events::Event::Child(node, msg)).is_ok()
        })
    };
    let (reap_tx, reap_rx) = std::sync::mpsc::channel();
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("agentd"));

    let model_window = settings.context.model_window.unwrap_or_else(|| {
        if model.is_empty() {
            tokens::DEFAULT_MODEL_WINDOW
        } else {
            tokens::window_for_model(&model)
        }
    });
    // Pressure watches the FILE store's filesystem (a memory/mcp/http store's
    // durability does not live on this disk). `min_free` defaults to 256MB:
    // a checkpoint failure at ENOSPC halts the daemon, so at that point
    // shedding new work while draining is strictly better than dying mid-write.
    let pressure = {
        use crate::config::v2::StoreKind;
        let (path, shed) = if settings.store.kind == StoreKind::File {
            let root = crate::config::v2::file_store_root(&settings.store);
            let min = settings
                .store
                .file
                .as_ref()
                .and_then(|f| f.min_free.as_deref())
                .map(super::runtime::pressure::parse_bytes)
                .transpose()
                .unwrap_or_else(|e| {
                    log.warn("config.warning", json!({"warning": format!("store.file.min_free: {e}; using the 256MB default")}));
                    None
                })
                .unwrap_or(256 << 20);
            (Some(root), min)
        } else {
            (None, 0)
        };
        std::sync::Arc::new(pressure::Pressure::new(path, shed))
    };

    let mut rt = Runtime {
        instance: instance.clone(),
        run_id: run_id.clone(),
        durable,
        mcp,
        mcp_specs,
        registry,
        contexts: Contexts::new(model_window),
        memory: Memory::new(
            settings.memory.max_value_bytes.unwrap_or(65_536) as usize,
            settings.memory.list_default_limit.unwrap_or(100) as usize,
        ),
        artifacts: artifacts::Artifacts::new(),
        skills: catalogue,
        governor: Governor::new(&settings.intelligence.budget),
        workflows: BTreeMap::new(),
        runs: BTreeMap::new(),
        children: children::Children::new(exe, child_tx, reap_tx),
        timers: timers::Timers::new(),
        events_rx,
        events_tx,
        reap_rx,
        pending: Vec::new(),
        turn_queue: Default::default(),
        staged_turns: BTreeMap::new(),
        inbox_queue: Default::default(),
        subagents: BTreeMap::new(),
        instruction: reactor::Instruction {
            text: String::new(),
            source: "static",
            uri: None,
            server: None,
            version: 1,
        },
        job_shape: false,
        exit: None,
        draining: false,
        paused: false,
        drain_started: None,
        drain_reason: String::new(),
        idle_since: None,
        intel_uri,
        intel_token,
        intel_headers,
        intel_bearer,
        model,
        trace_id: Some(trace.trace_id.clone()),
        started: Instant::now(),
        seq: 0,
        counters: Default::default(),
        job_runs: Vec::new(),
        executing: BTreeMap::new(),
        last_manifest_flush: Instant::now(),
        goal_judge_at: None,
        #[cfg(feature = "a2a")]
        tasks: BTreeMap::new(),
        #[cfg(feature = "a2a")]
        event_to_task: BTreeMap::new(),
        #[cfg(feature = "a2a")]
        #[cfg(feature = "a2a")]
        a2a_feed: None,
        #[cfg(feature = "a2a")]
        a2a_pairing: None,
        #[cfg(feature = "a2a")]
        reserved_task_id: None,
        #[cfg(feature = "a2a")]
        a2a_sink: None,
        #[cfg(feature = "a2a")]
        a2a_listener: None,
        activity: BTreeMap::new(),
        last_root_reply: None,
        #[cfg(feature = "a2a")]
        feed_marks: BTreeMap::new(),
        #[cfg(feature = "a2a")]
        feed_last: Instant::now(),
        #[cfg(feature = "a2a")]
        webhook_callbacks: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        #[cfg(feature = "a2a")]
        webhook_sync: std::collections::HashMap::new(),
        pressure: pressure.clone(),
        pressure_seen: pressure::Level::Ok,
        resched: false,
        reap_deferred: Default::default(),
        step_rates: Default::default(),
        settings_doc: loaded.doc.clone(),
        args: args.to_vec(),
        env: env.to_vec(),
        pinned: BTreeMap::new(),
        retiring: BTreeMap::new(),
        pin_written: Default::default(),
        recent_signals: BTreeMap::new(),
        memory_keys: std::collections::HashMap::new(),
        stream_dirty: false,
        settings,
        log: log.clone(),
    };

    // Adopt the restored state.
    let lost_ctx = rt.contexts.restore(restored.of(Kind::Context));
    if !lost_ctx.is_empty() {
        log.warn("restore.context.lost", json!({"ids": lost_ctx}));
    }
    rt.timers.restore(restored.timers());
    rt.artifacts.restore(restored.of(Kind::Artifact));
    let mut replayed: Vec<(String, String)> = Vec::new();
    for env in restored.of(Kind::Run) {
        match serde_json::from_value::<crate::engine::RunState>(env.state.clone()) {
            Ok(mut r) => {
                r.dirty = false;
                if !r.status.is_terminal() {
                    // Replay policy: a step left `running` by the crash is
                    // re-executed under the SAME idempotency key, so a remote
                    // that already saw the first attempt can deduplicate it;
                    // a suspended step keeps the wait it was parked on.
                    for (id, st) in r.steps.iter_mut() {
                        if st.status == StepStatus::Running {
                            log.info(
                                "restore.step.replay",
                                json!({"run": r.id, "step": id, "attempt": st.attempt}),
                            );
                            st.status = StepStatus::Pending;
                            st.worker = None;
                            // The step's `on_replay` policy is applied in a
                            // second pass: the definitions are not loaded yet
                            // here, and the policy lives in the definition.
                            replayed.push((r.id.clone(), id.clone()));
                        }
                    }
                    r.status = crate::engine::RunStatus::Running;
                    r.dirty = true;
                }
                rt.runs.insert(r.id.clone(), r);
            }
            Err(e) => log.warn(
                "restore.run.corrupt",
                json!({"id": env.id, "err": e.to_string()}),
            ),
        }
    }
    for env in restored.of(Kind::Subagent) {
        match serde_json::from_value::<reactor::SubagentRecord>(env.state.clone()) {
            Ok(s) => {
                rt.subagents.insert(s.handle.clone(), s);
            }
            Err(e) => log.warn(
                "restore.subagent.corrupt",
                json!({"id": env.id, "err": e.to_string()}),
            ),
        }
    }
    #[cfg(feature = "a2a")]
    rt.restore_a2a_tasks(restored.of(Kind::Task));
    if let Some(m) = &restored.manifest {
        rt.governor.restore(&m.budget, now_ms());
    }
    for ev in restored.inbox_pending() {
        rt.inbox_queue.push_back(ev);
    }
    if restored.manifest.is_some() {
        log.info("restore.adopted", json!({"runs": rt.runs.len(), "contexts": rt.contexts.len(), "subagents": rt.subagents.len(), "timers": rt.timers.len(), "artifacts": rt.artifacts.len(), "inbox_pending": rt.inbox_queue.len(), "lost": restored.lost.len()}));
        rt.restore_pins();
        // Audit the restore: adopting a previous life's durable state is a
        // trust event, and any entity that could not be read back is recorded
        // as `lost` so the gap is visible rather than inferred from silence.
        rt.audit(audit::AuditEvent {
            action: "restore",
            target: json!({"runs": rt.runs.len(), "subagents": rt.subagents.len(), "inbox_pending": rt.inbox_queue.len(), "lost": restored.lost.len()}),
            outcome: if restored.lost.is_empty() { "restored" } else { "restored_with_loss" },
            principal: Some("system"),
            role: Some("system"),
            request_id: None,
        });
    }

    // The instruction: either static text, or a resource URI that is read
    // now and subscribed to so later updates reach the agent.
    if let Some(text) = rt.settings.agent.instruction.clone() {
        if crate::config::v2::looks_like_resource_uri(&text) {
            match rt.subscribe_instruction(&text) {
                Ok(()) => {}
                Err(e) => {
                    log.error("proc.exit", json!({"code": crate::exit::MCP_REQUIRED_DOWN, "err": format!("agent.instruction {text}: {e}")}));
                    return crate::exit::MCP_REQUIRED_DOWN;
                }
            }
        } else {
            rt.instruction.text = text;
        }
    }

    if let Err(errs) = rt.load_workflows() {
        for e in &errs {
            log.error("config.invalid", json!({"error": e}));
        }
        log.error(
            "proc.exit",
            json!({"code": crate::exit::USAGE, "err": "workflow definitions"}),
        );
        return crate::exit::USAGE;
    }
    // Workflows — a definition that fails to load is a config error, not a
    // warning: a daemon must not run with a workflow it silently dropped.
    // Phase 2 of the reference preflight: the workflows are loaded now, so the
    // ones that arrived from files, URLs and directories are visible. A secret
    // that only a fetched definition mentions is found HERE, before any start
    // node arms — not at 03:00 when the schedule first fires the step that
    // needed it.
    {
        let mut all = serde_json::Map::new();
        for (name, wf) in &rt.workflows {
            let mut steps = serde_json::Map::new();
            for (sid, step) in &wf.steps {
                steps.insert(
                    sid.clone(),
                    Value::Object(step.spec.clone().into_iter().collect()),
                );
            }
            all.insert(name.clone(), Value::Object(steps));
        }
        if let Some(code) =
            reference_preflight(&Value::Object(all), &rt.settings, "workflows", &rt.log)
        {
            return code;
        }
    }

    // `on_replay` was published in the JSON Schema, documented, and read by
    // nothing: every in-flight step was re-executed on restore regardless. Now
    // the declared policy decides. `retry` (the default) keeps the old
    // behaviour, so this only changes runs that asked for something else.
    if !replayed.is_empty() {
        let policies: Vec<(String, String, crate::engine::model::OnReplay)> = replayed
            .iter()
            .filter_map(|(rid, sid)| {
                let wf_name = rt.runs.get(rid)?.workflow.clone();
                let step = rt.workflows.get(&wf_name)?.steps.get(sid)?;
                Some((rid.clone(), sid.clone(), step.on_replay))
            })
            .collect();
        for (rid, sid, policy) in policies {
            match policy {
                crate::engine::model::OnReplay::Retry => {}
                crate::engine::model::OnReplay::Skip => {
                    if let Some(r) = rt.runs.get_mut(&rid) {
                        r.end_step(&sid, StepStatus::Skipped, None, None);
                    }
                    rt.log.info(
                        "restore.step.skipped",
                        json!({"run": rid, "step": sid, "on_replay": "skip"}),
                    );
                }
                crate::engine::model::OnReplay::Fail => {
                    if let Some(r) = rt.runs.get_mut(&rid) {
                        r.end_step(
                            &sid,
                            StepStatus::Failed,
                            None,
                            Some(
                                "step was in flight when the process died and its \
                                 on_replay policy is `fail`"
                                    .into(),
                            ),
                        );
                    }
                    rt.log.warn(
                        "restore.step.failed",
                        json!({"run": rid, "step": sid, "on_replay": "fail"}),
                    );
                }
            }
        }
    }
    // Arm the runtime-events tap before the first tick, so the events of
    // starting up are themselves observable. `audit.sink: [stream]` needs the
    // tap too — it queues through the same drain — so arm it for that alone
    // even when no families were selected.
    {
        let re = rt.settings.observability.runtime_events.clone();
        let audit_stream = rt.settings.observability.audit.stream.clone();
        let audit_wants_stream = rt
            .settings
            .observability
            .audit
            .sink
            .as_ref()
            .is_some_and(|s| {
                s.iter()
                    .any(|x| matches!(x, crate::config::v2::AuditSink::Stream))
            });
        if re.is_some() || (audit_wants_stream && audit_stream.is_some()) {
            let (stream, include, sampled, cap) = match &re {
                Some(r) => (
                    r.stream.clone().unwrap_or_default(),
                    r.include.clone(),
                    r.sampled.clone(),
                    r.queue_cap(),
                ),
                None => (
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                    crate::config::v2::DEFAULT_TAP_QUEUE as usize,
                ),
            };
            crate::obs::log::install_runtime_tap(&stream, include.clone(), sampled.clone(), cap);
            rt.log.info(
                "stream.tap",
                json!({"stream": stream, "include": include, "sampled": sampled,
                       "queue": cap, "audit_stream": audit_stream}),
            );
        }
    }
    // `lifecycle.run_until` decides whether this process is a job or a daemon:
    // `idle` is the job shape, `drained` is a daemon, and `auto` infers the job
    // shape when nothing can bring in outside work — no A2A listener and no
    // long-lived start node.
    rt.job_shape = match rt.settings.lifecycle.run_until {
        crate::config::v2::RunUntil::Drained => false,
        crate::config::v2::RunUntil::Idle => true,
        crate::config::v2::RunUntil::Auto => {
            rt.settings.a2a.listen.is_none() && !rt.workflows.values().any(|w| w.is_long_lived())
        }
    };
    // Restored `once` runs of a job count toward its exit code.
    for r in rt.runs.values() {
        if rt.job_shape
            && rt
                .workflows
                .get(&r.workflow)
                .and_then(|w| w.step(&r.start.node))
                .is_some_and(|s| s.kind == "once")
        {
            rt.job_runs.push(r.id.clone());
        }
    }
    // Skill references in the instruction preload into the root context.
    let refs = rt.skills.references(&rt.instruction.text.clone());
    if !refs.is_empty() {
        let unknown = rt.preload_skills(crate::context::ROOT, &refs, None);
        for u in unknown {
            rt.note_root(format!(
                "skill.unknown: {u:?} referenced by the instruction is not in the catalogue"
            ));
        }
    }
    // `lifecycle.watch_config`: a file change reloads exactly like SIGHUP,
    // through the same validate-then-apply path.
    #[cfg(all(unix, feature = "config-watch"))]
    if rt.settings.lifecycle.watch_config {
        for (path, _) in &loaded.files {
            crate::config::watch::spawn_config_watcher(std::path::Path::new(path), &log);
        }
    }
    rt.arm_workflows();
    rt.arm_long_lived_starts();
    rt.arm_goal();
    rt.respawn_restored_subagents();
    rt.respawn_restored_instances();
    // The A2A transport: the HTTPS listener for conversations,
    // command DataParts, and durable tasks. A bind/TLS/principals failure at
    // startup is fatal — the daemon cannot serve its only external channel.
    #[cfg(feature = "a2a")]
    if rt.settings.a2a.listen.is_some() {
        let resolver = match crate::a2a::Resolver::build(&rt.settings.a2a, &envmap) {
            Ok(r) => r,
            Err(e) => {
                log.error(
                    "proc.exit",
                    json!({"code": crate::exit::USAGE, "err": format!("a2a principals: {e}")}),
                );
                return crate::exit::USAGE;
            }
        };
        let write_timeout = rt.settings.lifecycle.drain_timeout();
        match a2a_server::spawn_a2a_listener(
            &rt.settings.a2a,
            &rt.settings.interface,
            rt.events_tx.clone(),
            resolver,
            &envmap,
            write_timeout,
            log.clone(),
        ) {
            Ok(serving) => {
                rt.a2a_feed = serving.feed;
                rt.a2a_pairing = serving.pairing;
                rt.a2a_sink = Some(std::sync::Arc::clone(&serving.listener.sink));
                // The listener stops the moment it is dropped, so the runtime
                // holds it for as long as it is serving.
                rt.a2a_listener = Some(serving.listener);
                // The interface debug reads tail the live log ring. Install
                // the ring only when debug is on, so the ordinary build keeps
                // its zero-cost logging hot path.
                if rt.settings.interface.enabled && rt.settings.interface.debug {
                    let cap = rt
                        .settings
                        .observability
                        .events_ring
                        .map(|n| n as usize)
                        .unwrap_or(crate::obs::log::EVENTS_RING_DEFAULT);
                    crate::obs::log::install_event_ring(cap);
                    log.info("interface.debug", json!({"events_ring": cap}));
                }
                // Publish restored tasks now that the shared view exists.
                for id in rt.tasks.keys().cloned().collect::<Vec<_>>() {
                    rt.task_sync(&id);
                }
            }
            Err(e) => {
                log.error(
                    "proc.exit",
                    json!({"code": crate::exit::USAGE, "err": format!("a2a listen: {e}")}),
                );
                return crate::exit::USAGE;
            }
        }
    }
    // The inbound webhook surface: a dedicated HTTP listener that turns
    // signed requests into workflow runs. A bind/TLS failure at startup is fatal —
    // a daemon that can't serve its declared webhooks is misconfigured.
    #[cfg(feature = "a2a")]
    if rt.settings.webhooks.listen.is_some() {
        let nodes: Vec<(
            String,
            String,
            serde_json::Map<String, serde_json::Value>,
            bool,
        )> = rt
            .workflows
            .values()
            .flat_map(|wf| {
                let low = wf.priority == crate::engine::model::Priority::Low;
                wf.steps
                    .values()
                    .filter(|s| s.kind == "webhook")
                    .map(|s| (wf.name.clone(), s.id.clone(), s.spec.clone(), low))
                    .collect::<Vec<_>>()
            })
            .collect();
        let write_timeout = rt.settings.lifecycle.drain_timeout();
        if let Err(e) = webhooks::spawn_webhook_listener(
            &rt.settings.webhooks,
            nodes,
            rt.webhook_callbacks.clone(),
            rt.events_tx.clone(),
            &envmap,
            write_timeout,
            rt.pressure.clone(),
            log.clone(),
        ) {
            log.error(
                "proc.exit",
                json!({"code": crate::exit::USAGE, "err": format!("webhooks listen: {e}")}),
            );
            return crate::exit::USAGE;
        }
    }
    // Observability serving: the Prometheus `/metrics` surface and the
    // health-file heartbeat a fleet supervisor watches, when configured.
    #[cfg(feature = "metrics")]
    if let Some(addr) = rt.settings.observability.metrics_addr.clone()
        && let Err(e) = crate::obs::serve::spawn(&addr, log.clone())
    {
        log.warn(
            "metrics.serve.fail",
            json!({"addr": addr, "err": e.to_string()}),
        );
    }
    if let Some(path) = rt.settings.observability.health_file.clone() {
        crate::obs::health::spawn_writer(
            std::path::PathBuf::from(path),
            run_id.clone(),
            "1".into(),
            std::time::Duration::from_secs(10),
        );
    }
    // OTLP logs export (optional): mirror the JSON-lines log surface
    // to `<endpoint>/v1/logs` when `observability.otel.logs` is on.
    #[cfg(feature = "otel")]
    if rt.settings.observability.otel.logs == Some(true)
        && let Some(ep) = rt
            .settings
            .observability
            .otel
            .endpoint
            .clone()
            .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
    {
        crate::obs::otel::arm_logs(&ep, "agentd", crate::VERSION);
        log.info("otel.logs.armed", json!({"endpoint": ep}));
    }
    // `--prompt`: the task, delivered as a MESSAGE into the agent's root
    // context — the same path an A2A message takes. Root scope is the point:
    // the agent answers with its full tool surface, so a prompt may set the
    // instance up (`workflow.create` a loop/schedule/subscribe) instead of
    // only answering once. Whether the process then exits is the ordinary
    // lifecycle question: `auto` stays up iff something long-lived is armed.
    if let Some(prompt) = rt.settings.agent.prompt.clone()
        && !prompt.trim().is_empty()
        && let Err(err) = rt.accept_event(
            events::kinds::A2A_MESSAGE,
            Some("operator".into()),
            json!({"text": prompt, "context_id": crate::context::ROOT}),
        )
    {
        log.warn("prompt.reject", json!({"err": err}));
    }
    // A debug-only seam (`AGENTD_TEST_INBOX_FILE`): inject inbox events from a
    // JSON file, so the e2e suite can drive the runtime without standing up an
    // A2A listener. Compiled out of a release build without `internal-mocks`.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if let Ok(path) = std::env::var("AGENTD_TEST_INBOX_FILE") {
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
            Ok(Value::Array(events)) => {
                for e in events {
                    let kind = e["kind"].as_str().unwrap_or(events::kinds::A2A_MESSAGE).to_string();
                    let principal = e["principal"].as_str().map(str::to_string);
                    let payload = e.get("payload").cloned().unwrap_or(Value::Null);
                    if let Err(err) = rt.accept_event(&kind, principal, payload) {
                        log.warn("test.inbox.reject", json!({"err": err}));
                    }
                }
                let _ = std::fs::remove_file(&path);
            }
            other => log.warn("test.inbox.bad_file", json!({"path": path, "err": format!("{other:?}").chars().take(200).collect::<String>()})),
        }
    }
    rt.checkpoint(true);
    let code = rt.run_loop();
    let _ = &rt.last_manifest_flush;
    // A job-shaped run prints its result on stdout, so it composes with a
    // shell pipeline the way any other one-shot command does.
    if rt.job_shape
        && let Some(out) = rt.job_output()
    {
        match out {
            Value::String(s) => println!("{s}"),
            Value::Null => {}
            other => println!(
                "{}",
                serde_json::to_string_pretty(&other).unwrap_or_default()
            ),
        }
    }
    code
}

/// A static **capability document** for `--capabilities`: describes the
/// configured surface with **no side effects** — it does not connect to MCP
/// servers, read secrets, or start the loop, so it is safe to run against a
/// production configuration. It reflects the configuration (what the agent is
/// set up to do), not live state.
pub fn capabilities(loaded: &Loaded) -> Value {
    const START_KINDS: &[&str] = &[
        "once",
        "manual",
        "loop",
        "schedule",
        "subscribe",
        "signal",
        "event",
        "a2a",
    ];
    let s = &loaded.settings;
    let workflows: Vec<Value> = s
        .workflows
        .iter()
        .map(|w| {
            let starts: Vec<String> = w["steps"]
                .as_object()
                .map(|steps| steps.values().filter_map(|st| st["kind"].as_str()).filter(|k| START_KINDS.contains(k)).map(str::to_string).collect())
                .unwrap_or_default();
            json!({"name": w["name"].as_str().unwrap_or(""), "description": w.get("description").and_then(Value::as_str), "start_kinds": starts, "inputs_schema": w.get("inputs").is_some()})
        })
        .collect();
    let a2a = s.a2a.listen.as_ref().map(|listen| {
        let principals: Vec<Value> = s
            .a2a
            .principals
            .iter()
            .map(|p| json!({"role": format!("{:?}", p.role).to_lowercase(), "match": principal_match_desc(&p.matcher), "grants": p.grants}))
            .collect();
        let mut methods = vec![
            "SendMessage",
            "SendStreamingMessage",
            "GetTask",
            "CancelTask",
            "ListTasks",
            "SubscribeToTask",
            "GetAgentCard",
        ];
        let mut command_ops = vec![
            "status",
            "config",
            "workflow.run",
            "workflow.status",
            "workflow.cancel",
            "workflow.signal",
            "subagent.send",
            "subagent.kill",
            "subagent.status",
            "plan.get",
        ];
        if s.interface.enabled {
            methods.push("SubscribeToEvents");
            command_ops.push("interface.info");
            if s.interface.debug {
                command_ops.extend(["conversation.get", "run.get", "debug.events"]);
            }
        }
        json!({
            "listen": listen,
            "tls": s.a2a.tls.cert.is_some(),
            "mtls": s.a2a.tls.client_ca.is_some(),
            "bearer": s.a2a.bearer.is_some(),
            "methods": methods,
            "admin": ["a2a.drain", "a2a.lameduck", "a2a.cancel", "a2a.pause", "a2a.resume"],
            "command_ops": command_ops,
            "principals": principals,
            "loopback_operator": s.a2a.principals.is_empty(),
        })
    });
    json!({
        "runtime": "1",
        "version": crate::VERSION,
        "agent": {"name": s.instance_name(), "instruction": s.agent.instruction.is_some(), "preflight": format!("{:?}", s.agent.preflight).to_lowercase()},
        "intelligence": {"model": s.intelligence.model, "endpoints": s.intelligence.endpoints.len()},
        "mcp_servers": s.mcp.servers.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        "internal_tools": crate::registry::internal::names(),
        "tools": {"overrides": s.tools.overrides.keys().cloned().collect::<Vec<_>>(), "disabled": s.tools.disabled},
        "workflows": workflows,
        "knowledge": {"server": s.knowledge.server},
        "search": {"server": s.search.server},
        "skills": {"sources": s.skills.sources.len()},
        "a2a": a2a,
        "interface": {"enabled": s.interface.enabled, "debug": s.interface.debug, "origins": s.interface.origins.len(), "pairing": s.interface.pairing.enabled, "display": {"top": s.interface.display.top, "bottom": s.interface.display.bottom}},
        "store": format!("{:?}", s.store.kind).to_lowercase(),
        // For the file adapter the kind alone under-reports: what an operator
        // actually gets depends on the directory it lands in, and on whether
        // they chose it or the long-lived default did. Additive, and `null`
        // for every other adapter, so the `store` string above stays the
        // stable answer to "which adapter".
        "store_file": (s.store.kind == StoreKind::File).then(|| json!({
            "path": crate::config::v2::file_store_root(&s.store).display().to_string(),
            "defaulted": loaded.doc.pointer("/store/kind").is_none(),
        })),
        "lifecycle": {"run_until": format!("{:?}", s.lifecycle.run_until).to_lowercase(), "daemon": s.a2a.listen.is_some() || s.workflows.iter().any(|w| w["steps"].as_object().is_some_and(|st| st.values().any(|n| n["kind"].as_str().is_some_and(|k| matches!(k, "loop" | "schedule" | "subscribe" | "signal" | "event")))))},
    })
}

/// A redacted description of a principal matcher (secrets never leak here).
fn principal_match_desc(m: &crate::config::v2::PrincipalMatch) -> Value {
    if m.any {
        json!({"any": true})
    } else if let Some(s) = &m.san {
        json!({"san": s})
    } else if let Some(s) = &m.sub {
        json!({"sub": s})
    } else if m.bearer_ref.is_some() {
        json!({"bearer_ref": "***"})
    } else if let Some(a) = &m.aauth_agent {
        json!({"aauth_agent": a})
    } else {
        json!({})
    }
}

/// Build the intelligence credential provider: a closure returning the current
/// bearer, refreshed from the `agentd login intelligence` device-login cache.
///
/// Returns `None` when no bearer-style `intelligence.auth` is configured (and
/// always without `--features oauth`), which leaves the static
/// `intelligence.token` path untouched.
fn intel_bearer_provider(
    settings: &crate::config::v2::Settings,
) -> Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>> {
    #[cfg(feature = "oauth")]
    {
        let auth = settings.intelligence.auth.as_ref()?;
        let spec = auth.to_spec();
        // SigV4 (`kind: aws`) signs each request over its own method, path
        // and body, so there is no reusable bearer to hand back. That case is
        // carried separately as an `AuthSpec` (see `Runtime::intel_aws_auth`)
        // and turned into a per-dial signer at the call site.
        if spec.kind == "aws" {
            return None;
        }
        // Build the provider's signer once (preserving the oauth2 in-memory
        // refresh) and extract the bearer per LLM dial. Covers static / oauth2
        // device-login / spiffe jwt — all bearer-style for intelligence.
        let signer = crate::auth::device::signer_for(
            &spec,
            "intelligence",
            std::time::Duration::from_secs(30),
        )
        .ok()??;
        Some(std::sync::Arc::new(move || {
            signer
                .sign("POST", "", "", &[])
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.strip_prefix("Bearer ").unwrap_or(&v).to_string())
        }))
    }
    #[cfg(not(feature = "oauth"))]
    {
        let _ = settings;
        None
    }
}

impl Runtime {
    /// The current intelligence bearer: the credential provider's refreshing
    /// token when an `intelligence.auth` oauth2 block is configured, else the
    /// static `intelligence.token`. Resolved fresh at each subagent spawn so a
    /// child rides a live token without carrying refresh machinery of its own,
    /// and so a child spawned late in a long life does not inherit an expired
    /// one.
    pub(crate) fn current_intel_bearer(&self) -> Option<String> {
        self.intel_bearer
            .as_ref()
            .and_then(|f| f())
            .or_else(|| self.intel_token.clone())
    }

    /// The AWS SigV4 intelligence-auth spec, when `intelligence.auth` selects
    /// `kind: aws`. Threaded to subagents (which build the signer themselves)
    /// and used by the goal judge to sign its own LLM dial, so every path that
    /// dials intelligence carries the same credential.
    pub(crate) fn intel_aws_auth(&self) -> Option<crate::config::AuthSpec> {
        let a = self.settings.intelligence.auth.as_ref()?;
        (a.kind == crate::config::v2::AuthKind::Aws).then(|| a.to_spec())
    }

    /// The configured `intelligence.dialect`, threaded into a child's spawn
    /// payload so the child selects the same wire adapter as its parent.
    /// `None` means the OpenAI-compatible dialect.
    pub(crate) fn intel_dialect(&self) -> Option<String> {
        self.settings.intelligence.dialect.clone()
    }
}

/// Resolve `intelligence.token` / `token_file` (secret refs, files).
fn resolve_intel_token(
    settings: &crate::config::v2::Settings,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    if let Some(t) = &settings.intelligence.token {
        let resolved = crate::sec::secret::resolve(&t.0, env)
            .map_err(|e| format!("intelligence.token: {e}"))?;
        return Ok(Some(resolved));
    }
    if let Some(p) = &settings.intelligence.token_file {
        return crate::sec::secret::read_token_file(p)
            .map(Some)
            .map_err(|e| format!("intelligence.token_file: {e}"));
    }
    // No token in the configuration: the intel client falls back to its own
    // environment conventions (`AGENT_INTELLIGENCE_TOKEN`…).
    Ok(None)
}

impl Runtime {
    /// Read the instruction resource and subscribe to it, so an update at the
    /// server reaches this agent without a reload.
    pub(crate) fn subscribe_instruction(&mut self, uri: &str) -> Result<(), String> {
        let (server, res) = match uri.strip_prefix("mcp://").and_then(|r| r.split_once('/')) {
            Some((s, r)) => (Some(s.to_string()), r.to_string()),
            None => (None, uri.to_string()),
        };
        // Find the serving client.
        let candidates: Vec<(String, Arc<McpClient>)> = match &server {
            Some(s) => self
                .mcp
                .get(s)
                .map(|c| vec![(s.clone(), c.clone())])
                .unwrap_or_default(),
            None => self
                .mcp
                .iter()
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect(),
        };
        let mut last_err = String::from("no connected MCP server serves it");
        for (name, c) in candidates {
            match c.read_resource(&res) {
                Ok(r) => {
                    let text = r.text();
                    if c.capabilities().supports_resources()
                        && let Err(e) = c.subscribe(&res)
                    {
                        self.log.warn(
                            "instruction.subscribe.fail",
                            json!({"server": name, "uri": res, "err": e.to_string()}),
                        );
                    }
                    let changed = self.instruction.text != text;
                    self.instruction = reactor::Instruction {
                        text,
                        source: "resource",
                        uri: Some(res.clone()),
                        server: Some(name.clone()),
                        version: self.instruction.version + u64::from(changed),
                    };
                    self.log.info("instruction.loaded", json!({"server": name, "uri": res, "bytes": self.instruction.text.len(), "version": self.instruction.version}));
                    return Ok(());
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err)
    }

    /// Drain MCP notifications. An updated instruction resource is re-read and
    /// wakes the root (`instruction_updated`). A `tools/list_changed` is only
    /// recorded: the tool catalogue is rebuilt from a fresh `tools/list` at the
    /// next config reload, so a server cannot change what this agent may call
    /// without an operator-initiated reload.
    pub(crate) fn poll_mcp_notifications(&mut self) {
        let mut updated_instruction = false;
        let mut tools_changed = Vec::new();
        let mut resource_updates: Vec<(String, String)> = Vec::new();
        for (name, c) in &self.mcp {
            for n in c.drain_notifications() {
                match n.method.as_str() {
                    ::mcp::wire::method::NOTIFY_RESOURCES_UPDATED => {
                        let uri = n
                            .params
                            .as_ref()
                            .and_then(|p| p.get("uri"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if self.instruction.uri.as_deref() == Some(uri)
                            && self.instruction.server.as_deref() == Some(name.as_str())
                        {
                            updated_instruction = true;
                        }
                        resource_updates.push((name.clone(), uri.to_string()));
                    }
                    ::mcp::wire::method::NOTIFY_TOOLS_LIST_CHANGED => {
                        tools_changed.push(name.clone())
                    }
                    _ => {}
                }
            }
        }
        if updated_instruction && let Some(uri) = self.instruction.uri.clone() {
            let full = match &self.instruction.server {
                Some(s) => format!("mcp://{s}/{uri}"),
                None => uri,
            };
            let before = self.instruction.version;
            if self.subscribe_instruction(&full).is_ok() && self.instruction.version != before {
                self.log.info(
                    "instruction.updated",
                    json!({"version": self.instruction.version}),
                );
                if self
                    .settings
                    .agent
                    .wake_on()
                    .contains(&crate::config::v2::WakeEvent::InstructionUpdated)
                {
                    self.note_root("instruction.updated: the instruction resource changed; re-read it with instruction.read".into());
                }
            }
        }
        for (server, uri) in resource_updates {
            self.on_resource_updated(&server, &uri); // `wait` steps
            self.on_subscribe_resource(&server, &uri); // `subscribe` start nodes
        }
        for s in tools_changed {
            self.log.info("mcp.tools_changed", json!({"server": s, "note": "recorded only; the tool catalogue is rebuilt at the next config reload"}));
        }
    }
}
