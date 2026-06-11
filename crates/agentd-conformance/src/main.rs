//! `agentd-conformance` — run the conformance corpus and report.
//!
//! Usage:
//!   agentd-conformance [run] [CORPUS_DIR] [--json] [--min-pass-rate T]
//!
//! `CORPUS_DIR` defaults to `./corpus`. With `--json`, prints the
//! machine-readable suite report instead of the text summary.
//! `--min-pass-rate T` enforces a suite-wide reliability floor (in
//! addition to any per-scenario `min_pass_rate`) — the deploy gate for
//! reliability-gated autonomy. Exits non-zero if any scenario fails or
//! falls below its reliability bar, so it slots into CI as a gate.

use std::path::PathBuf;
use std::process::ExitCode;

use agentd_conformance::run_corpus;

fn main() -> ExitCode {
    let mut dir: Option<PathBuf> = None;
    let mut json = false;
    let mut min_pass_rate: Option<f64> = None;

    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "run" => {}
            "--json" => json = true,
            "--min-pass-rate" => match args.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) if (0.0..=1.0).contains(&v) => min_pass_rate = Some(v),
                _ => {
                    eprintln!("agentd-conformance: --min-pass-rate expects a number in [0,1]");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                eprintln!(
                    "usage: agentd-conformance [run] [CORPUS_DIR] [--json] [--min-pass-rate T]"
                );
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("agentd-conformance: unknown flag `{other}`");
                return ExitCode::from(2);
            }
            other => dir = Some(PathBuf::from(other)),
        }
    }
    let dir = dir.unwrap_or_else(|| PathBuf::from("corpus"));

    let report = match run_corpus(&dir) {
        Ok(r) if !r.scenarios.is_empty() => r,
        Ok(_) => {
            eprintln!("agentd-conformance: no scenarios under {}", dir.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("agentd-conformance: read {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap()
        );
    } else {
        print!("{}", report.render_text());
    }

    // Reliability gate: per-scenario declared bars + the optional
    // suite-wide floor. A violation fails the run independent of the
    // pass/fail tally above.
    let violations = report.reliability_violations(min_pass_rate);
    if !violations.is_empty() {
        eprintln!("\nreliability gate FAILED:");
        for v in &violations {
            eprintln!(
                "  {} — pass_rate {:.2} < required {:.2}",
                v.name, v.pass_rate, v.required
            );
        }
        return ExitCode::FAILURE;
    }

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
