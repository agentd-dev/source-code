//! The subagent side of the control channel. RFC 0005, RFC 0003, RFC 0009.
//!
//! Entered when `main` sees `AGENTD_SUBAGENT` set. The child:
//! 1. installs `PR_SET_PDEATHSIG` so a supervisor death collapses it (must be
//!    here — `pre_exec`'s setting is cleared by `execve`);
//! 2. reads its [`SpawnPayload`] (first control frame) from stdin;
//! 3. starts a **control reader thread** (separate from the agentic loop) that
//!    answers `Ping` with `Pong` and flips a cancel flag on `Cancel` — so
//!    liveness survives a long in-flight tool/model call (Detector C);
//! 4. emits `Ready`, connects intelligence + its scoped MCP servers, runs
//!    `agentloop::run_loop`, and sends `Result`/`Failed` back up.
//!
//! Wire: stdout carries length-framed [`AgentMsg`] up; stderr carries the
//! child's JSON telemetry (inherited to the parent). stdin carries
//! [`ControlMsg`] down.

use crate::agentloop::runner::{run_loop, LoopAbort, LoopInput};
use crate::intel::client::IntelClient;
use crate::json::frame;
use crate::mcp::client::McpClient;
use crate::obs::log::{Comp, Level, LogCtx, Logger};
use crate::subagent::orchestrator::Orchestrator;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use std::io::{self, BufReader, Stdin, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Up = Arc<Mutex<Stdout>>;

/// The subagent entry point. Returns the process exit code.
pub fn run() -> i32 {
    install_pdeathsig();
    // If the supervisor already died in the fork/exec window, bail (we'd be
    // reparented to init / the subreaper).
    #[cfg(unix)]
    if unsafe { libc::getppid() } == 1 {
        return crate::exit::GENERIC;
    }

    let mut stdin = BufReader::new(io::stdin());
    let payload = match read_spawn(&mut stdin) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentd subagent: bad spawn payload: {e}");
            return crate::exit::USAGE;
        }
    };

    let up: Up = Arc::new(Mutex::new(io::stdout()));
    let log = build_logger(&payload);
    let cancel = Arc::new(AtomicBool::new(false));

    // The control reader runs on its own thread and owns stdin from here on,
    // so Ping/Pong keeps flowing while the loop is busy.
    spawn_control_thread(stdin, Arc::clone(&up), Arc::clone(&cancel), log.ctx().clone());

    send_up(&up, &AgentMsg::Ready);
    log.info("loop.start", serde_json::json!({"depth": payload.depth}));

    let intel = match IntelClient::from_parts(&payload.intelligence.uri, payload.intelligence.token.clone()) {
        Ok(c) => c,
        Err(e) => return fail(&up, &log, format!("intel: {e}"), crate::exit::INTEL_UNAVAILABLE),
    };

    let mut servers = Vec::new();
    for spec in &payload.mcp_servers {
        let connected = McpClient::spawn(&spec.name, &spec.command, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c));
        match connected {
            Ok(mut c) => {
                // Stamp the run id (retry dedup, RFC 0011) + a W3C traceparent
                // (distributed tracing, RFC 0010) on every tool call.
                let mut meta = serde_json::json!({"agentd/run_id": payload.telemetry.run_id});
                if let Some(tid) = &payload.telemetry.trace_id {
                    meta["traceparent"] = crate::obs::trace::outbound_traceparent(tid).into();
                }
                c.set_tool_meta(meta);
                servers.push(c);
            }
            Err(e) => {
                return fail(&up, &log, format!("mcp '{}': {e}", spec.name), crate::exit::MCP_REQUIRED_DOWN)
            }
        }
    }

    let input = LoopInput {
        instruction: payload.instruction.clone(),
        output_contract: payload.output_contract.clone(),
        seed: payload.context_seed.iter().map(|m| (m.role.clone(), m.content.clone())).collect(),
        model: payload.intelligence.model.clone().unwrap_or_default(),
        max_steps: payload.limits.max_steps,
        max_tokens: payload.limits.max_tokens,
        deadline: Instant::now() + Duration::from_millis(payload.limits.deadline_ms.max(1)),
        cancel: Some(Arc::clone(&cancel)),
    };

    // Self-orchestration: the model can delegate subtasks via subagent.spawn,
    // which spawns + supervises a child agent (depth + 1, scoped) from here.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agentd"));
    let mut orch = Orchestrator::from_payload(exe, &payload, Duration::from_secs(25), log.clone());

    match run_loop(&intel, &servers, &input, &mut orch, &log) {
        Ok(outcome) => {
            let code = crate::exit::once_exit(outcome.status, outcome.partial);
            send_up(&up, &AgentMsg::Result { outcome });
            code
        }
        Err(LoopAbort::Intel(m)) => fail(&up, &log, format!("intel: {m}"), crate::exit::INTEL_UNAVAILABLE),
        Err(LoopAbort::Mcp(m)) => fail(&up, &log, format!("mcp: {m}"), crate::exit::MCP_REQUIRED_DOWN),
    }
}

fn fail(up: &Up, log: &Logger, error: String, code: i32) -> i32 {
    log.error("loop.error", serde_json::json!({"err": error}));
    send_up(up, &AgentMsg::Failed { error });
    code
}

fn read_spawn(reader: &mut BufReader<Stdin>) -> Result<SpawnPayload, String> {
    let bytes = frame::read_frame(reader)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "stdin closed before spawn payload".to_string())?;
    match serde_json::from_slice::<ControlMsg>(&bytes).map_err(|e| e.to_string())? {
        ControlMsg::Spawn(p) => Ok(*p),
        other => Err(format!("first frame was not Spawn: {other:?}")),
    }
}

fn build_logger(payload: &SpawnPayload) -> Logger {
    let t = &payload.telemetry;
    let level = Level::parse(&t.log_level).unwrap_or(Level::Info);
    Logger::new(
        LogCtx {
            run_id: t.run_id.clone(),
            agent_id: t.agent_id.clone(),
            agent_path: t.agent_path.clone(),
            comp: Comp::Agent,
            pid: std::process::id(),
            trace_id: t.trace_id.clone(),
        },
        level,
    )
}

fn send_up(up: &Up, msg: &AgentMsg) {
    if let Ok(mut out) = up.lock() {
        // Best-effort: a dead parent means our writes fail; we don't crash.
        let _ = frame::write_frame(&mut *out, msg);
    }
}

/// The control reader thread. Owns stdin, answers `Ping` with `Pong`, flips
/// the cancel flag on `Cancel`. Exits on EOF (the supervisor closed the
/// channel) or a read error.
fn spawn_control_thread(mut stdin: BufReader<Stdin>, up: Up, cancel: Arc<AtomicBool>, ctx: LogCtx) {
    let log = Logger::new(ctx, Level::Debug);
    std::thread::Builder::new()
        .name("subagent-control".into())
        .spawn(move || {
            // Exits on Ok(None)/Err — the supervisor closed the channel.
            while let Ok(Some(bytes)) = frame::read_frame(&mut stdin) {
                match serde_json::from_slice::<ControlMsg>(&bytes) {
                    Ok(ControlMsg::Ping { seq }) => send_up(&up, &AgentMsg::Pong { seq }),
                    Ok(ControlMsg::Cancel { reason }) => {
                        log.info("subagent.cancel", serde_json::json!({"reason": reason}));
                        cancel.store(true, Ordering::Relaxed);
                    }
                    Ok(ControlMsg::Inject { .. }) => { /* M3: deliver into the session */ }
                    Ok(ControlMsg::Spawn(_)) | Err(_) => { /* unexpected/garbage — ignore */ }
                }
            }
        })
        .ok();
}

/// `PR_SET_PDEATHSIG(SIGKILL)`: when the supervisor (our parent) dies, the
/// kernel sends us SIGKILL — the leaf-up tree collapse (RFC 0003). Must be set
/// after `execve` (it is cleared across exec), i.e. here in the child's `main`.
#[cfg(target_os = "linux")]
fn install_pdeathsig() {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
    }
}

#[cfg(not(target_os = "linux"))]
fn install_pdeathsig() {
    // PDEATHSIG is Linux-only; on other Unix the supervisor's kill ladder is
    // the fallback. (agentd targets Linux for production.)
}

// (No unit tests here — the control path is exercised end to end by the
// `subagent_spawn` integration test, which launches a real subagent process.)
