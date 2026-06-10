//! `agentd-conformance` — run the conformance corpus and report.
//!
//! Usage:
//!   agentd-conformance [run] [CORPUS_DIR] [--json]
//!
//! `CORPUS_DIR` defaults to `./corpus`. With `--json`, prints the
//! machine-readable suite report instead of the text summary. Exits
//! non-zero if any scenario fails, so it slots into CI as a gate.

use std::path::PathBuf;
use std::process::ExitCode;

use agentd_conformance::run_corpus;

fn main() -> ExitCode {
    let mut dir: Option<PathBuf> = None;
    let mut json = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "run" => {}
            "--json" => json = true,
            "-h" | "--help" => {
                eprintln!("usage: agentd-conformance [run] [CORPUS_DIR] [--json]");
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

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
