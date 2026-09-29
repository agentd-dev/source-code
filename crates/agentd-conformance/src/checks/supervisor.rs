// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor contract: the documented exit-code table (a `once` job maps
//! its outcome to an exit code).
//!
//! Every check here drives the real binary and judges it by its exit code, so
//! the contract is proven the way an operator's supervisor observes it — not
//! through agentd's own types.

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
