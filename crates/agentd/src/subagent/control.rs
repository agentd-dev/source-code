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

use crate::agentloop::runner::{LoopAbort, LoopInput, Session, run_loop};
use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::intel::client::IntelClient;
use crate::json::frame;
use crate::mcp::client::McpClient;
use crate::obs::log::{Comp, Level, LogCtx, Logger};
use crate::subagent::orchestrator::Orchestrator;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::budget::Budget;
use std::io::{self, BufReader, Stdin, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
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
    // Tree-wide turn-boundary suspension (RFC 0005 §4.3 / RFC 0015 §4.3): the
    // control thread sets this on `Pause`, clears it on `Resume`; the loop waits
    // between turns while it is set. `cancel` always wins (see `pause_wait`).
    let paused = Arc::new(AtomicBool::new(false));

    // For a warm continue-session, the control thread forwards each `Inject`
    // event to the loop over this channel; a one-shot run never reads it.
    let (inject_tx, inject_rx) = std::sync::mpsc::channel::<String>();

    // The control reader runs on its own thread and owns stdin from here on,
    // so Ping/Pong keeps flowing while the loop is busy — and so Resume/Cancel
    // still arrive while the loop is suspended at a turn boundary.
    spawn_control_thread(
        stdin,
        Arc::clone(&up),
        Arc::clone(&cancel),
        Arc::clone(&paused),
        inject_tx,
        log.ctx().clone(),
    );

    send_up(&up, &AgentMsg::Ready);
    log.info(
        "loop.start",
        serde_json::json!({"depth": payload.depth, "warm": payload.warm}),
    );

    let mut intel = match IntelClient::from_parts(
        &payload.intelligence.uri,
        payload.intelligence.token.clone(),
    ) {
        Ok(mut c) => {
            // Outbound LLM calls join the run's distributed trace (RFC 0010).
            c.set_trace_id(payload.telemetry.trace_id.clone());
            c
        }
        Err(e) => {
            return fail(
                &up,
                &log,
                format!("intel: {e}"),
                crate::exit::INTEL_UNAVAILABLE,
            );
        }
    };

    let mut servers = Vec::new();
    for spec in &payload.mcp_servers {
        let connected = McpClient::spawn(&spec.name, &spec.command, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c));
        match connected {
            Ok(mut c) => {
                log.info("mcp.connect", serde_json::json!({"server": spec.name}));
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
                return fail(
                    &up,
                    &log,
                    format!("mcp '{}': {e}", spec.name),
                    crate::exit::MCP_REQUIRED_DOWN,
                );
            }
        }
    }

    let input = LoopInput {
        instruction: payload.instruction.clone(),
        output_contract: payload.output_contract.clone(),
        seed: payload
            .context_seed
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect(),
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

    // A warm continue-session lives across many events; a one-shot runs once.
    // A warm session is the long-lived loop/reactive shape (RFC 0008), so it gets
    // the all-down backoff (RFC 0018 §6): a transient host-model roll recovers
    // without crashing the daemon, rather than exiting 4 like a `once` job.
    if payload.warm {
        intel.enable_alldown_backoff(crate::intel::client::AllDownPolicy::default());
        return run_warm(
            &intel, &servers, &input, &payload, &mut orch, &cancel, &paused, &inject_rx, &up, &log,
        );
    }

    // One-shot: a single turn. Suspend at the turn boundary (before the turn
    // starts) if paused; a turn already in progress is never interrupted.
    pause_wait(&paused, &cancel, &log);
    match run_loop(&intel, &servers, &input, &mut orch, &log) {
        Ok(outcome) => {
            let code = crate::exit::once_exit(outcome.status, outcome.partial);
            send_up(&up, &AgentMsg::Result { outcome });
            code
        }
        Err(LoopAbort::Intel(m)) => fail(
            &up,
            &log,
            format!("intel: {m}"),
            crate::exit::INTEL_UNAVAILABLE,
        ),
        Err(LoopAbort::Mcp(m)) => fail(
            &up,
            &log,
            format!("mcp: {m}"),
            crate::exit::MCP_REQUIRED_DOWN,
        ),
    }
}

/// Drive a **warm continue-session** (RFC 0008 §spawn-vs-continue): prepare the
/// session once, then run one turn per delivered event over the *same*
/// transcript, emitting [`AgentMsg::Turn`] after each. The process and its
/// conversation stay warm between events until the supervisor cancels it or
/// closes the control channel, at which point a terminal [`AgentMsg::Result`]
/// marks closure. Each turn gets a fresh per-event budget (steps/tokens/deadline)
/// so one reaction can't starve the session.
#[allow(clippy::too_many_arguments)]
fn run_warm(
    intel: &IntelClient,
    servers: &[McpClient],
    input: &LoopInput,
    payload: &SpawnPayload,
    orch: &mut Orchestrator,
    cancel: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    inject_rx: &Receiver<String>,
    up: &Up,
    log: &Logger,
) -> i32 {
    let mut session = match Session::prepare(servers, input, orch) {
        Ok(s) => s,
        Err(LoopAbort::Intel(m)) => {
            return fail(
                up,
                log,
                format!("intel: {m}"),
                crate::exit::INTEL_UNAVAILABLE,
            );
        }
        Err(LoopAbort::Mcp(m)) => {
            return fail(up, log, format!("mcp: {m}"), crate::exit::MCP_REQUIRED_DOWN);
        }
    };
    let limits = &payload.limits;
    loop {
        // Turn boundary: if paused (RFC 0005 §4.3 / RFC 0015 §4.3), suspend HERE,
        // before starting the next turn — never mid-turn. A `Cancel` during pause
        // wins and proceeds to wind-down (the loop falls through to the cancel
        // check below). The control thread keeps running, so Resume/Cancel arrive
        // while we wait. The supervisor reactor and its liveness heartbeat are not
        // affected — only this child loop suspends.
        pause_wait(paused, cancel, log);
        // One turn over the persistent transcript, bounded by a fresh per-event
        // budget (a new deadline each turn, so the session isn't globally capped).
        let deadline = Instant::now() + Duration::from_millis(limits.deadline_ms.max(1));
        let mut budget = Budget::new(limits.max_steps, limits.max_tokens, deadline);
        let outcome = match session.run_turn(intel, orch, log, &mut budget, Some(cancel)) {
            Ok(o) => o,
            Err(LoopAbort::Intel(m)) => {
                return fail(
                    up,
                    log,
                    format!("intel: {m}"),
                    crate::exit::INTEL_UNAVAILABLE,
                );
            }
            Err(LoopAbort::Mcp(m)) => {
                return fail(up, log, format!("mcp: {m}"), crate::exit::MCP_REQUIRED_DOWN);
            }
        };
        // Cancellation during a turn ends the session (terminal Result below);
        // any other terminal is just this reaction's turn — the session lives on.
        if outcome.status == TerminalStatus::Cancelled {
            break;
        }
        send_up(up, &AgentMsg::Turn { outcome });
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Block for the next event (single-consumer, in-order FIFO).
        match wait_for_inject(inject_rx, cancel) {
            Some(message) => {
                log.info(
                    "subagent.inject",
                    serde_json::json!({"bytes": message.len()}),
                );
                session.deliver(&message);
            }
            None => break, // cancelled, or the supervisor closed the control channel
        }
    }
    // Session closed: a single terminal Result so the supervisor sees closure.
    let status = if cancel.load(Ordering::Relaxed) {
        TerminalStatus::Cancelled
    } else {
        TerminalStatus::Completed
    };
    let code = crate::exit::once_exit(status, false);
    send_up(
        up,
        &AgentMsg::Result {
            outcome: Outcome {
                status,
                partial: false,
                result: serde_json::Value::Null,
                scheduled: Vec::new(),
                subscriptions: Vec::new(),
            },
        },
    );
    code
}

/// Block until the next event is injected, the supervisor closes the control
/// channel (its `Inject` sender drops → `Disconnected`), or a cancel is
/// requested — polled so cancellation between events stays prompt.
fn wait_for_inject(rx: &Receiver<String>, cancel: &AtomicBool) -> Option<String> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(message) => return Some(message),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Suspend the loop at a turn boundary while `paused` is set (RFC 0005 §4.3 /
/// RFC 0015 §4.3). Polls at the same cadence as `wait_for_inject` so a `Resume`
/// (or `Cancel`) lands promptly. `cancel` always wins: a cancel during a pause
/// returns immediately so the loop proceeds to wind-down. Logs once on enter and
/// once on leave (debounced — never per poll). The supervisor reactor is NOT
/// gated by this; only the child's agentic loop suspends, so the liveness
/// heartbeat keeps ticking (RFC 0015 §4.3).
fn pause_wait(paused: &AtomicBool, cancel: &AtomicBool, log: &Logger) {
    if !paused.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
        return; // fast path: not paused (or cancel wins) → no log, no wait
    }
    log.info("loop.paused", serde_json::json!({}));
    while paused.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    log.info("loop.resumed", serde_json::json!({}));
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
    .with_content(t.log_content)
}

fn send_up(up: &Up, msg: &AgentMsg) {
    if let Ok(mut out) = up.lock() {
        // Best-effort: a dead parent means our writes fail; we don't crash.
        let _ = frame::write_frame(&mut *out, msg);
    }
}

/// The control reader thread. Owns stdin, answers `Ping` with `Pong`, flips the
/// cancel flag on `Cancel`, toggles the `paused` flag on `Pause`/`Resume`, and
/// forwards each `Inject` event to a warm session's loop over `inject_tx`. It
/// keeps running while the loop is suspended (so `Resume`/`Cancel`/`Ping` still
/// arrive — the whole point of a separate thread). Exits on EOF (the supervisor
/// closed the channel) or a read error — which drops `inject_tx`, unblocking a
/// warm session's wait.
fn spawn_control_thread(
    mut stdin: BufReader<Stdin>,
    up: Up,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    inject_tx: Sender<String>,
    ctx: LogCtx,
) {
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
                    // Turn-boundary suspension (RFC 0005 §4.3 / RFC 0015 §4.3): set
                    // the flag here; the loop suspends at its next boundary. The
                    // loop's `pause_wait` does the enter/leave logging (debounced).
                    Ok(ControlMsg::Pause) => paused.store(true, Ordering::Relaxed),
                    Ok(ControlMsg::Resume) => paused.store(false, Ordering::Relaxed),
                    // Deliver into the warm session; a one-shot run never reads
                    // the receiver, so the send is simply dropped there.
                    Ok(ControlMsg::Inject { message }) => {
                        let _ = inject_tx.send(message);
                    }
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
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn install_pdeathsig() {
    // PDEATHSIG is Linux-only; on other Unix the supervisor's kill ladder is
    // the fallback. (agentd targets Linux for production.)
}

// The full control path is exercised end to end by the `subagent_spawn`
// integration test (a real subagent process). The flag-driven turn-boundary
// suspend logic is unit-tested here directly.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx, Logger};

    fn test_log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Info,
        )
    }

    #[test]
    fn pause_wait_returns_immediately_when_not_paused() {
        let paused = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        // No sleep on the fast path.
        assert!(t.elapsed() < Duration::from_millis(40));
    }

    #[test]
    fn pause_wait_cancel_wins_over_pause() {
        // Paused AND cancelled → cancel wins: return at once (the loop then winds
        // down at its cancel check). Never blocks.
        let paused = AtomicBool::new(true);
        let cancel = AtomicBool::new(true);
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        assert!(t.elapsed() < Duration::from_millis(40));
    }

    #[test]
    fn pause_wait_suspends_until_resume() {
        // Paused → block; another thread clears `paused` (a Resume), and the wait
        // returns. The flag is the whole mechanism — this proves the seam.
        let paused = Arc::new(AtomicBool::new(true));
        let cancel = Arc::new(AtomicBool::new(false));
        let p2 = Arc::clone(&paused);
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            p2.store(false, Ordering::Relaxed); // Resume
        });
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        // It actually waited for the resume (≥ ~one poll interval), then returned.
        assert!(t.elapsed() >= Duration::from_millis(80));
        assert!(!paused.load(Ordering::Relaxed));
        unblock.join().unwrap();
    }

    #[test]
    fn pause_wait_breaks_out_on_cancel_during_pause() {
        // A cancel that lands WHILE suspended unblocks the wait (cancel always
        // wins), so a drain during a pause proceeds (RFC 0015 §4.3).
        let paused = Arc::new(AtomicBool::new(true));
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            c2.store(true, Ordering::Relaxed); // Cancel during pause
        });
        pause_wait(&paused, &cancel, &test_log());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(paused.load(Ordering::Relaxed)); // still paused, but cancel broke us out
        canceller.join().unwrap();
    }
}
