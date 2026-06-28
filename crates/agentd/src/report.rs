// SPDX-License-Identifier: Apache-2.0
//! Run-outcome reports — the machine-readable terminal-outcome object for a
//! `once`/`loop`/`schedule`-bounded run (RFC 0016 §6). This is the structured
//! backend for `kubectl agents results`: a `Job`'s pod is gone seconds after it
//! exits, so the *full* outcome (terminal status, usage, duration, a pointer to
//! the distilled result, the trace id) must be captured to a durable place at
//! exit, not inferred from a vanished pod.
//!
//! This RFC OWNS the report schema; it REUSES the primitives it points at:
//!   * `status` is the **RFC 0007 §3.4** terminal-status string (the authority),
//!     not a synonym — see [`crate::agentloop::stop::TerminalStatus`];
//!   * `exit_code` is the **RFC 0011 §5** coarse projection of `status` — see
//!     [`crate::exit`]; both are present so a reader sees the precise status and
//!     can still author exit-code policy (§6.2);
//!   * `distillate_ref` POINTS at the result body (`agentd://subagent/0/result`,
//!     RFC 0005 §3.3); it never embeds it — the report stays small and bounded;
//!   * `trace_id` is the RFC 0010 §3.6 trace, for cross-pod stitching.
//!
//! Two delivery surfaces, both optional, both off for a bare CLI run (§6.3):
//! `--report-file PATH` (atomic write at the terminal transition) and the
//! `agentd://run/{run_id}` resource (the served self-MCP, frozen to this schema —
//! wired in [`crate::mcp::server`]). **Reactive daemons emit no report** (§6.4):
//! they have no single terminal outcome.
//!
//! Telemetry never crashes the run (§8.4): a failed `--report-file` write logs
//! `report.write.fail` (warn) and the run STILL exits with the correct exit code
//! (the exit code is the floor contract, RFC 0011 §5; it never depends on the
//! report landing).

use crate::agentloop::stop::TerminalStatus;
use crate::obs::log::{Logger, rfc3339_millis};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::SystemTime;

/// The report schema version (RFC 0016 §6.2/§8.1), surfaced in the manifest at
/// `surfaces.report_schema`. Additive field changes keep this; a removed/renamed
/// field bumps the major (§8.2). agentctl branches on this.
pub const REPORT_SCHEMA: &str = "1.0";

/// Per-run usage roll-up (§6.2 `usage`). Token honesty (RFC 0010 §3.9 / §4.3):
/// absence is `0`, NEVER an estimate — a gateway that omits `usage` leaves these
/// at `0` so a cost reader stays trustworthy. The supervisor fills what it knows
/// at the terminal transition.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Loop steps across the tree.
    pub steps: u64,
    /// Total subagents spawned in the run.
    pub subagents: u64,
}

/// Per-run refusal roll-up (§6.2 `refusals`) over the §4.3 closed reason set.
/// The metric counters are fleet-cumulative; the report is THIS run, so
/// `kubectl agents results` shows "this run hit 1 depth refusal" with no metrics
/// query. Unknown-to-this-run reasons stay `0`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Refusals {
    pub trifecta: u64,
    pub rate: u64,
    pub budget: u64,
    pub depth: u64,
    pub mcp: u64,
}

/// The frozen run-outcome report object (RFC 0016 §6.2). Built once, at the
/// terminal transition, from what the supervisor knows; serialized to
/// `--report-file` and served as `agentd://run/{run_id}` once terminal. Secrets
/// never appear (RFC 0010 §3.4 allowlist applies) — this carries counts/refs,
/// never raw content.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// RFC 0010 `run_id` — the unit of work. Stable across a retried Job when the
    /// operator sets `AGENTD_RUN_ID` (RFC 0011 §6), so retried reports collapse.
    pub run_id: String,
    /// Downward-API identity when present (RFC 0014 §5); `None` omits the field.
    pub instance: Option<String>,
    /// `once` | `loop` | `schedule` — never `reactive` (§6.4).
    pub mode: String,
    /// The RFC 0007 §3.4 terminal-status string (the authority).
    pub status: String,
    /// The RFC 0011 §5 code (the coarse projection of `status`).
    pub exit_code: i32,
    /// Whether the run produced usable partial output (drives the 3-vs-7 split,
    /// RFC 0011 §5.2).
    pub has_usable_partial: bool,
    pub usage: Usage,
    pub duration_ms: u64,
    /// RFC 3339 UTC start/end (RFC 0010 line-schema timestamps).
    pub started_at: String,
    pub ended_at: String,
    /// RFC 0005 §3.3 handle where the result body lives (it is NOT embedded).
    pub distillate_ref: String,
    /// RFC 0010 §3.6 trace id — stitch to the trace. `None` omits the field.
    pub trace_id: Option<String>,
    pub refusals: Refusals,
}

impl RunReport {
    /// Assemble a report from a finished run's terminal facts. `started`/`ended`
    /// are `SystemTime`s (rendered RFC 3339); `duration_ms` is computed from them
    /// (clamped to 0 on a non-monotonic clock — never negative). `status` is the
    /// RFC 0007 §3.4 string; `exit_code` the RFC 0011 §5 projection.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: String,
        instance: Option<String>,
        mode: String,
        status: TerminalStatus,
        exit_code: i32,
        has_usable_partial: bool,
        usage: Usage,
        refusals: Refusals,
        trace_id: Option<String>,
        started: SystemTime,
        ended: SystemTime,
    ) -> RunReport {
        let duration_ms = ended
            .duration_since(started)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        RunReport {
            run_id,
            instance,
            mode,
            status: status.as_str().to_string(),
            exit_code,
            has_usable_partial,
            usage,
            duration_ms,
            started_at: rfc3339_millis(started),
            ended_at: rfc3339_millis(ended),
            // The distilled return of the root (depth-0) subagent (RFC 0007 §3.9
            // / RFC 0005 §3.3). The report points; the body is read on demand.
            distillate_ref: crate::agentd_uri::subagent_uri("0/result"),
            trace_id,
            refusals,
        }
    }

    /// The frozen §6.2 JSON object. `trace_id`/`instance` are omitted when absent
    /// (a clean schema, never `null` noise). The result body is NOT embedded —
    /// only the `distillate_ref` handle.
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "report_schema": REPORT_SCHEMA,
            "run_id": self.run_id,
            "mode": self.mode,
            "status": self.status,
            "exit_code": self.exit_code,
            "has_usable_partial": self.has_usable_partial,
            "usage": {
                "tokens_in": self.usage.tokens_in,
                "tokens_out": self.usage.tokens_out,
                "steps": self.usage.steps,
                "subagents": self.usage.subagents,
            },
            "duration_ms": self.duration_ms,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "distillate_ref": self.distillate_ref,
            "refusals": {
                "trifecta": self.refusals.trifecta,
                "rate": self.refusals.rate,
                "budget": self.refusals.budget,
                "depth": self.refusals.depth,
                "mcp": self.refusals.mcp,
            },
        });
        if let Value::Object(m) = &mut obj {
            if let Some(instance) = &self.instance {
                m.insert("instance".into(), Value::String(instance.clone()));
            }
            if let Some(trace_id) = &self.trace_id {
                m.insert("trace_id".into(), Value::String(trace_id.clone()));
            }
        }
        obj
    }

    /// Write the report to `path` via an atomic write (temp + `rename`, the same
    /// primitive RFC 0010 §3.7 uses for the health file) so a reader never sees a
    /// torn file (§6.3). Best-effort-but-loud (§8.4): on failure the supervisor
    /// logs `report.write.fail` (warn) and the caller STILL exits with the
    /// correct code — the report write never gates the exit. Written **once**, at
    /// the terminal transition, before the `proc.exit` log line.
    pub fn write_to_file(&self, path: &str, log: &Logger) {
        let body = self.to_json().to_string();
        match write_atomic(Path::new(path), body.as_bytes()) {
            Ok(()) => log.info(
                "report.written",
                json!({"path": path, "status": self.status, "exit_code": self.exit_code}),
            ),
            Err(e) => log.warn(
                "report.write.fail",
                json!({"path": path, "err": e.to_string()}),
            ),
        }
    }
}

/// Write `bytes` to `path` atomically (temp + rename) — mirrors the health-file
/// primitive (RFC 0010 §3.7) so a reader never sees a partial report.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(status: TerminalStatus, exit_code: i32, partial: bool) -> RunReport {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let ended = started + Duration::from_millis(84_213);
        RunReport::new(
            "01J8Z3K2Qn7".into(),
            Some("pod-abc".into()),
            "once".into(),
            status,
            exit_code,
            partial,
            Usage {
                tokens_in: 9_412_233,
                tokens_out: 412_044,
                steps: 37,
                subagents: 4,
            },
            Refusals {
                depth: 1,
                ..Refusals::default()
            },
            Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            started,
            ended,
        )
    }

    #[test]
    fn report_object_has_the_frozen_shape() {
        let r = sample(TerminalStatus::Completed, 0, false);
        let v = r.to_json();
        assert_eq!(v["report_schema"], REPORT_SCHEMA);
        assert_eq!(v["run_id"], "01J8Z3K2Qn7");
        assert_eq!(v["instance"], "pod-abc");
        assert_eq!(v["mode"], "once");
        // status is the RFC 0007 §3.4 string, exit_code the RFC 0011 §5 projection.
        assert_eq!(v["status"], "completed");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["has_usable_partial"], false);
        assert_eq!(v["usage"]["tokens_in"], 9_412_233u64);
        assert_eq!(v["usage"]["tokens_out"], 412_044u64);
        assert_eq!(v["usage"]["steps"], 37);
        assert_eq!(v["usage"]["subagents"], 4);
        assert_eq!(v["duration_ms"], 84_213u64);
        assert_eq!(v["started_at"], "2023-11-14T22:13:20.000Z");
        // distillate_ref points; it does not embed.
        assert_eq!(v["distillate_ref"], "agentd://subagent/0/result");
        assert_eq!(v["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(v["refusals"]["depth"], 1);
        assert_eq!(v["refusals"]["trifecta"], 0);
    }

    #[test]
    fn status_and_exit_code_can_diverge() {
        // A budget-bounded run: status is the precise RFC 0007 string, exit_code
        // the coarse RFC 0011 projection — both present (§6.2).
        let r = sample(TerminalStatus::ExhaustedSteps, crate::exit::BUDGET, false);
        let v = r.to_json();
        assert_eq!(v["status"], "exhausted_steps");
        assert_eq!(v["exit_code"], 7);
    }

    #[test]
    fn absent_instance_and_trace_id_are_omitted() {
        let started = SystemTime::UNIX_EPOCH;
        let r = RunReport::new(
            "r".into(),
            None,
            "loop".into(),
            TerminalStatus::Refused,
            crate::exit::REFUSED,
            false,
            Usage::default(),
            Refusals::default(),
            None,
            started,
            started,
        );
        let v = r.to_json();
        assert!(v.get("instance").is_none(), "instance omitted when absent");
        assert!(v.get("trace_id").is_none(), "trace_id omitted when absent");
        // Honest absence: usage is 0, never an estimate (RFC 0010 §3.9).
        assert_eq!(v["usage"]["tokens_in"], 0);
        assert_eq!(v["duration_ms"], 0);
    }

    #[test]
    fn write_to_file_is_atomic_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let path_str = path.to_string_lossy().into_owned();
        let r = sample(TerminalStatus::Completed, 0, false);
        r.write_to_file(&path_str, &test_logger());
        let read = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["report_schema"], REPORT_SCHEMA);
        // No leftover temp file (the rename completed).
        assert!(!path.with_file_name("report.json.tmp").exists());
    }

    #[test]
    fn write_to_file_failure_is_swallowed_not_fatal() {
        // A path under a non-existent directory cannot be written; the call must
        // return normally (telemetry never crashes the run, §8.4).
        let r = sample(TerminalStatus::Completed, 0, false);
        r.write_to_file("/nonexistent-dir-xyz/report.json", &test_logger());
        // Reaching here (no panic) is the assertion.
    }

    fn test_logger() -> Logger {
        use crate::obs::log::{Comp, Level, LogCtx};
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }
}
