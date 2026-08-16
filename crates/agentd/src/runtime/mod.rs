// SPDX-License-Identifier: AGPL-3.0-only
//! The **agentd 2.0 runtime** (RFC 0026): the supervisor's event loop over
//! durable state, the turn workers it spawns, and the lifecycle policy. Built
//! beside the 1.x mode drivers and selected by a v2 configuration document;
//! the 1.x drivers are removed at the P5 cut-over.
//!
//! Startup (RFC 0026 §8): parse+validate config → connect MCP servers
//! (contained failures) → connect the store or refuse → restore → build the
//! registry (validate overrides) → discover skills → resolve the instruction
//! → load workflows → arm start nodes (`once` fires unless a live run was
//! restored) → re-spawn pending subagents → `proc.ready` → the loop.

#[cfg(feature = "a2a")]
pub mod a2a_server;
pub mod artifacts;
pub mod audit;
pub mod children;
pub mod events;
#[cfg(feature = "exec")]
pub mod exec; // guarded local command runner behind the `exec` tool (RFC 0028; default-OFF)
pub mod goal;
pub mod http_node;
pub mod human; // human-in-the-loop: ask_human gates + fallbacks (RFC 0032 §16)
pub mod nested;
pub mod reactor;
pub mod reload;
pub mod starts;
pub mod steps;
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

/// Run the 2.0 runtime for a loaded v2 configuration. Returns the exit code.
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
    log.info("proc.start", json!({"version": crate::VERSION, "runtime": "2.0", "instance": instance, "config_files": loaded.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()}));
    for w in &loaded.warnings {
        log.warn("config.warning", json!({"warning": w}));
    }
    crate::signals::install();
    crate::supervisor::reap::set_child_subreaper();
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
    // AAuth identity (RFC 0023) — signs outbound MCP requests tree-wide.
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

    // Resource containment (RFC 0009 §cgroup): arm the process-tree cgroup so
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
    // RFC 0031: resolved `intelligence.headers` (per-dial) + an optional OAuth
    // credential provider (device-login bearer, refreshing).
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

    // The store (RFC 0025). `none` ⇒ an in-process store for a job-shaped
    // instance (validation already refused long-lived instances without one).
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

    // Restore (RFC 0025 §6).
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

    // Registry (RFC 0028): overrides validated against the connected servers.
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

    // Skills (RFC 0028 §7).
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

    // Channels.
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let (child_tx, child_rx) = std::sync::mpsc::channel();
    let (reap_tx, reap_rx) = std::sync::mpsc::channel();
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("agentd"));

    let model_window = settings.context.model_window.unwrap_or_else(|| {
        if model.is_empty() {
            tokens::DEFAULT_MODEL_WINDOW
        } else {
            tokens::window_for_model(&model)
        }
    });
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
        child_rx,
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
        a2a_shared: None,
        #[cfg(feature = "a2a")]
        a2a_feed: None,
        #[cfg(feature = "a2a")]
        a2a_pairing: None,
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
        settings_doc: loaded.doc.clone(),
        args: args.to_vec(),
        env: env.to_vec(),
        pinned: BTreeMap::new(),
        recent_signals: BTreeMap::new(),
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
    for env in restored.of(Kind::Run) {
        match serde_json::from_value::<crate::engine::RunState>(env.state.clone()) {
            Ok(mut r) => {
                r.dirty = false;
                if !r.status.is_terminal() {
                    // Replay policy (RFC 0027 §7): a `running` step is re-executed
                    // (same idempotency key); a suspended step keeps its wait.
                    for (id, st) in r.steps.iter_mut() {
                        if st.status == StepStatus::Running {
                            log.info(
                                "restore.step.replay",
                                json!({"run": r.id, "step": id, "attempt": st.attempt}),
                            );
                            st.status = StepStatus::Pending;
                            st.worker = None;
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
        // Audit the restore — a durable-state generation adoption (plan §3.11:
        // restore is audited; `lost` entities are recorded).
        rt.audit(audit::AuditEvent {
            action: "restore",
            target: json!({"runs": rt.runs.len(), "subagents": rt.subagents.len(), "inbox_pending": rt.inbox_queue.len(), "lost": restored.lost.len()}),
            outcome: if restored.lost.is_empty() { "restored" } else { "restored_with_loss" },
            principal: Some("system"),
            role: Some("system"),
            request_id: None,
        });
    }

    // The instruction (RFC 0028 §3): static text or a resource (read + subscribe).
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

    // Workflows (RFC 0027) — refused definitions are a config error.
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
    // RFC 0026 §8: `auto` ⇒ the job shape when there is no A2A listener and no
    // long-lived start node; `idle` ⇒ job shape; `drained` ⇒ a daemon.
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
    // `lifecycle.watch_config`: a file change reloads like SIGHUP (RFC 0017 §5.2).
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
    // The A2A v2 transport (RFC 0029): the HTTPS listener for conversations,
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
            Ok((shared, feed, pairing)) => {
                rt.a2a_shared = Some(shared);
                rt.a2a_feed = feed;
                rt.a2a_pairing = pairing;
                // The interface debug reads tail the live log ring (RFC 0016
                // §7.2 / RFC 0032 §5) — install it only when debug is on, so
                // the default build keeps its zero-cost logging hot path.
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
    // The inbound webhook surface (RFC 0027): a dedicated HTTP listener that turns
    // signed requests into workflow runs. A bind/TLS failure at startup is fatal —
    // a daemon that can't serve its declared webhooks is misconfigured.
    #[cfg(feature = "a2a")]
    if rt.settings.webhooks.listen.is_some() {
        let nodes: Vec<(String, String, serde_json::Map<String, serde_json::Value>)> = rt
            .workflows
            .values()
            .flat_map(|wf| {
                wf.steps
                    .values()
                    .filter(|s| s.kind == "webhook")
                    .map(|s| (wf.name.clone(), s.id.clone(), s.spec.clone()))
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
            log.clone(),
        ) {
            log.error(
                "proc.exit",
                json!({"code": crate::exit::USAGE, "err": format!("webhooks listen: {e}")}),
            );
            return crate::exit::USAGE;
        }
    }
    // Observability serving (plan §3.11): the Prometheus `/metrics` surface and
    // the health-file heartbeat (RFC 0016 §10 fleet liveness), when configured.
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
            "2.0".into(),
            std::time::Duration::from_secs(10),
        );
    }
    // OTLP logs export (plan §3.11, optional): mirror the JSON-lines log surface
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
    // A debug-only seam (`AGENTD_TEST_INBOX_FILE`): inject inbox events from a
    // JSON file — the e2e suite's stand-in for the A2A server until P5.
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
    // A job-shaped run prints its result on stdout (the 1.x `once` contract).
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

/// A static **capability document** for `--capabilities` (RFC 0015 §5.2):
/// describes the configured 2.0 surface with **no side effects** — it does not
/// connect to MCP servers, read secrets, or start the loop. It reflects the
/// configuration (what the agent is set up to do), not live state.
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
        "runtime": "2.0",
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

/// Build the intelligence OAuth credential provider (RFC 0031 §7): a closure
/// returning the current bearer (refreshing from the `agentd login intelligence`
/// device-login cache). `None` when no oauth2 `intelligence.auth` is set (or
/// without `--features oauth`), so the static `intelligence.token` path is
/// byte-identical.
fn intel_bearer_provider(
    settings: &crate::config::v2::Settings,
) -> Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>> {
    #[cfg(feature = "oauth")]
    {
        let auth = settings.intelligence.auth.as_ref()?;
        let spec = auth.to_spec();
        // SigV4 (`kind: aws`) is a per-request signature, not a bearer — the intel
        // path has no generic signer hook yet, so it is a follow-up for LLM auth.
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
    /// The current intelligence bearer (RFC 0031): the OAuth provider's token
    /// (refreshing) when an `intelligence.auth` oauth2 block is configured, else
    /// the static `intelligence.token`. Resolved fresh at each subagent spawn so
    /// a child rides a live token without its own refresh machinery.
    pub(crate) fn current_intel_bearer(&self) -> Option<String> {
        self.intel_bearer
            .as_ref()
            .and_then(|f| f())
            .or_else(|| self.intel_token.clone())
    }

    /// The AWS SigV4 intelligence-auth spec (RFC 0031), when `intelligence.auth`
    /// selects `kind: aws`. Threaded to subagents (which build the signer) and
    /// used by the goal judge to SigV4-sign the LLM dial.
    pub(crate) fn intel_aws_auth(&self) -> Option<crate::config::AuthSpec> {
        let a = self.settings.intelligence.auth.as_ref()?;
        (a.kind == crate::config::v2::AuthKind::Aws).then(|| a.to_spec())
    }

    /// The configured `intelligence.dialect` (RFC 0031 §8), threaded into a
    /// child's spawn payload so it selects the same wire adapter. `None` ⇒
    /// OpenAI-compatible.
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
    // The v1 env conventions still apply inside the intel client (AGENT_INTELLIGENCE_TOKEN…).
    Ok(None)
}

impl Runtime {
    /// Read + subscribe the instruction resource (RFC 0028 §3).
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

    /// Drain MCP notifications: an updated instruction resource re-reads it
    /// and wakes the root (`instruction_updated`); `tools/list_changed` is
    /// noted (registry rebuild lands with the P5 reload choreography).
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
            self.log.info("mcp.tools_changed", json!({"server": s, "note": "registry rebuild lands with the P5 reload choreography"}));
        }
    }
}
