//! The supervisor contract: the documented exit-code table (RFC 0011 §5) and the
//! SIGTERM graceful-drain choreography. Driven by running the real binary and
//! observing the exit code.

use crate::{Category, Check, Harness, Outcome};
use std::time::Duration;

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "supervisor/exit-0-on-success",
            category: Category::Supervisor,
            desc: "a completed once-mode run exits 0",
            run: exit_success,
        },
        Check {
            id: "supervisor/exit-2-on-bad-flag",
            category: Category::Supervisor,
            desc: "an unknown flag is a usage error → exit 2",
            run: exit_bad_flag,
        },
        Check {
            id: "supervisor/exit-2-on-validation",
            category: Category::Supervisor,
            desc: "reactive without a subscription fails validation → exit 2",
            run: exit_validation,
        },
        Check {
            id: "supervisor/exit-4-on-intel-down",
            category: Category::Supervisor,
            desc: "an unreachable intelligence endpoint → exit 4",
            run: exit_intel_down,
        },
        Check {
            id: "supervisor/exit-6-on-required-mcp-down",
            category: Category::Supervisor,
            desc: "a required MCP server that won't start → exit 6",
            run: exit_mcp_down,
        },
        Check {
            id: "supervisor/drain-0-on-sigterm",
            category: Category::Supervisor,
            desc: "SIGTERM drains a daemon gracefully → exit 0 (not 143)",
            run: drain_on_sigterm,
        },
    ]
}

fn exit_success(h: &Harness) -> Outcome {
    let llm = h.mock_llm("final");
    let r = h.run(&[
        "--instruction",
        "do a thing",
        "--intelligence",
        &llm.uri,
        "--model",
        "m",
        "--log-level",
        "error",
    ]);
    Outcome::require(
        r.code == Some(0),
        format!("want exit 0, got {:?}; stderr:\n{}", r.code, r.stderr),
    )
}

fn exit_bad_flag(h: &Harness) -> Outcome {
    let r = h.run(&["--no-such-flag"]);
    Outcome::require(r.code == Some(2), format!("want exit 2, got {:?}", r.code))
}

fn exit_validation(h: &Harness) -> Outcome {
    // reactive mode needs a subscription / continue — without one, validation fails.
    let r = h.run(&[
        "--mode",
        "reactive",
        "--instruction",
        "hi",
        "--intelligence",
        "unix:/x",
    ]);
    Outcome::require(
        r.code == Some(2),
        format!("want exit 2, got {:?}; stderr:\n{}", r.code, r.stderr),
    )
}

fn exit_intel_down(h: &Harness) -> Outcome {
    let r = h.run(&[
        "--instruction",
        "do a thing",
        "--intelligence",
        "unix:/nonexistent/agentd-conf-intel.sock",
        "--model",
        "m",
        "--log-level",
        "error",
    ]);
    Outcome::require(
        r.code == Some(4),
        format!(
            "want exit 4 (intel unavailable), got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
}

fn exit_mcp_down(h: &Harness) -> Outcome {
    let r = h.run(&[
        "--mode",
        "reactive",
        "--instruction",
        "react",
        "--intelligence",
        "unix:/x",
        "--subscribe",
        "file:///in.json",
        "--mcp",
        "bad=/nonexistent/agentd-conf-mcp-server",
        "--log-level",
        "error",
    ]);
    Outcome::require(
        r.code == Some(6),
        format!(
            "want exit 6 (required MCP down), got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
}

fn drain_on_sigterm(h: &Harness) -> Outcome {
    // An idle reactive daemon: SIGTERM should drain it and exit 0 (we self-exit 0
    // on a graceful drain; 143 would mean the kernel killed us).
    let daemon = h.spawn(&[
        "--mode",
        "reactive",
        "--subscribe",
        "file:///noop",
        "--instruction",
        "stand by",
        "--intelligence",
        "unix:/nonexistent/agentd-conf.sock",
        "--log-level",
        "warn",
    ]);
    // Give it a moment to reach its idle loop, then SIGTERM and observe the exit.
    std::thread::sleep(Duration::from_millis(300));
    daemon.sigterm();
    match daemon.wait(Duration::from_secs(5)) {
        Some(0) => Outcome::pass(),
        other => Outcome::fail(format!("want graceful exit 0 on SIGTERM, got {other:?}")),
    }
}
