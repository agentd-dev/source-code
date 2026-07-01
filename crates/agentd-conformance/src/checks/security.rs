// SPDX-License-Identifier: Apache-2.0
//! Security posture (RFC 0012): the Rule-of-Two lethal-trifecta refusal and its
//! explicit override, plus secret redaction in telemetry.

use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "security/trifecta-refused",
            category: Category::Security,
            desc: "granting one agent all three lethal-trifecta legs is refused at startup → exit 2",
            run: trifecta_refused,
        },
        Check {
            id: "security/trifecta-allow-override",
            category: Category::Security,
            desc: "--allow-trifecta downgrades the refusal to a logged grant",
            run: trifecta_allow_override,
        },
        Check {
            id: "security/secret-not-in-telemetry",
            category: Category::Security,
            desc: "the intelligence token never appears in the JSON-lines telemetry",
            run: secret_not_in_telemetry,
        },
    ]
}

/// The three-leg grant: one MCP server tagged with all of untrusted_input,
/// sensitive, egress.
const TRIFECTA_ARGS: &[&str] = &[
    "--instruction",
    "x",
    "--intelligence",
    "unix:/nonexistent/agentd-conf.sock",
    "--mcp",
    "fs=unix:/nonexistent/fs.sock",
    "--mcp-tags",
    "fs=untrusted_input,sensitive,egress",
];

fn trifecta_refused(h: &Harness) -> Outcome {
    let mut args = TRIFECTA_ARGS.to_vec();
    args.extend(["--log-level", "error"]);
    let r = h.run(&args);
    // The refusal is a startup config-validation rejection (the agent never runs)
    // → exit 2 (config/usage). Exit 5 is reserved for a *runtime* refusal after
    // the agent ran.
    //
    // The trifecta gate now lives in `Config::validate()` — the single validation
    // authority (RFC 0017 §7), so `--validate-config` and startup agree. That means
    // the refusal happens during config load, BEFORE the logger is constructed, so
    // it surfaces as the usage-refusal MESSAGE on stderr (exactly like every other
    // `validate()` failure) rather than a structured `scope.trifecta_refused` log
    // event. We assert that human-readable refusal text instead of the event.
    Outcome::require(
        r.code == Some(2),
        format!(
            "want exit 2 (config refusal), got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            r.stderr.contains("lethal-trifecta"),
            format!("no lethal-trifecta refusal on stderr:\n{}", r.stderr),
        )
    })
}

fn trifecta_allow_override(h: &Harness) -> Outcome {
    let mut args = TRIFECTA_ARGS.to_vec();
    args.extend(["--allow-trifecta", "--log-level", "warn"]);
    let r = h.run(&args);
    // The override must NOT refuse (exit 5); it proceeds (then fails for another
    // reason, e.g. intel down → 4). The grant is logged as allowed.
    Outcome::require(
        r.code != Some(5),
        format!("override still refused (exit 5); stderr:\n{}", r.stderr),
    )
    .and(|| {
        Outcome::require(
            r.saw_event("scope.trifecta_grant"),
            "no scope.trifecta_grant event".to_string(),
        )
    })
}

fn secret_not_in_telemetry(h: &Harness) -> Outcome {
    // A recognizable token; the run fails on the unreachable endpoint, but the
    // token must never surface in any log line (Config redacts it to ***).
    const TOKEN: &str = "conf-SECRET-3f9a2b-do-not-log";
    let r = h.run(&[
        "--instruction",
        "x",
        "--intelligence",
        "unix:/nonexistent/agentd-conf.sock",
        "--intelligence-token",
        TOKEN,
        "--model",
        "m",
        "--log-level",
        "debug",
    ]);
    Outcome::require(
        !r.stderr.contains(TOKEN),
        "the intelligence token leaked into telemetry".to_string(),
    )
}
