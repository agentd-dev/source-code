//! `agentd-conformance` — run the conformance corpus and report.
//!
//! Usage:
//!   agentd-conformance [CORPUS_DIR]   (default: ./corpus)
//!
//! Exits non-zero if any scenario fails, so it slots into CI as a gate.

use std::path::PathBuf;
use std::process::ExitCode;

use agentd_conformance::{discover_scenarios, run_scenario_file};

fn main() -> ExitCode {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("corpus"));

    let files = match discover_scenarios(&dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("agentd-conformance: no scenarios under {}", dir.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("agentd-conformance: read {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };

    let mut passed = 0usize;
    let mut failed = 0usize;
    for path in &files {
        let report = run_scenario_file(path);
        if report.passed() {
            passed += 1;
            println!(
                "  ok   {:<32} pass^{} = 1.0  ({} llm calls, {} tokens)",
                report.name, report.trials, report.cost.llm_calls, report.cost.llm_tokens
            );
        } else {
            failed += 1;
            println!("  FAIL {:<32} pass^{} = 0.0", report.name, report.trials);
            if let Some(e) = &report.load_error {
                println!("         load error: {e}");
            }
            for f in report.failures.iter().take(8) {
                println!("         {f}");
            }
        }
    }

    println!(
        "\n{} scenario(s): {passed} passed, {failed} failed",
        files.len()
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
