// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor contract (agentd 2.0): the documented exit-code table (the
//! `once` job maps its outcome to an exit code) and the v1→v2 migration gate.
//! Driven by running the real binary and observing the exit code.
//!
//! (The graceful-drain, required-MCP-down, and spawn-rate checks move to the P7
//! v2 conformance rebuild; SIGTERM drain is exercised by the v2 reload/a2a e2e.)

use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "supervisor/exit-0-on-success",
            category: Category::Supervisor,
            desc: "a completed once job exits 0",
            run: exit_success,
        },
        Check {
            id: "supervisor/exit-2-on-bad-flag",
            category: Category::Supervisor,
            desc: "an unknown flag is a usage error → exit 2",
            run: exit_bad_flag,
        },
        Check {
            id: "supervisor/exit-2-on-retired-v1-flag",
            category: Category::Supervisor,
            desc: "a retired 1.x flag (--mode) is rejected with a migration hint → exit 2",
            run: exit_retired_flag,
        },
        Check {
            id: "supervisor/exit-4-on-intel-down",
            category: Category::Supervisor,
            desc: "an unreachable intelligence endpoint → exit 4",
            run: exit_intel_down,
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

fn exit_retired_flag(h: &Harness) -> Outcome {
    // agentd 2.0 removed the mode drivers; `--mode` is a retired flag.
    let r = h.run(&[
        "--mode",
        "reactive",
        "--instruction",
        "hi",
        "--intelligence",
        "http://127.0.0.1:9",
    ]);
    Outcome::require(
        r.code == Some(2),
        format!("want exit 2, got {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            r.stderr.contains("--mode"),
            format!("stderr should name the retired flag:\n{}", r.stderr),
        )
    })
}

fn exit_intel_down(h: &Harness) -> Outcome {
    let r = h.run(&[
        "--instruction",
        "do a thing",
        "--intelligence",
        "http://127.0.0.1:9",
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
