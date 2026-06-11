//! `agentd-conformance` — run the conformance corpus and report.
//!
//! Usage:
//!   agentd-conformance [run] [CORPUS_DIR] [flags]
//!
//! Flags:
//!   --json                    machine-readable suite report
//!   --min-pass-rate T         suite-wide reliability floor (deploy gate)
//!   --forecast-runs-per-day N project spend at this trigger rate
//!   --price-per-mtok P        $ per 1M tokens, for the forecast
//!   --save-baseline PATH      write this run as a baseline for drift
//!   --baseline PATH           compare against a baseline; fail on a
//!                             pass_rate regression (e.g. a model update)
//!
//! `CORPUS_DIR` defaults to `./corpus`. Exits non-zero if any scenario
//! fails, falls below its reliability bar, or regresses against a
//! baseline — so it slots into CI as a gate.

use std::path::PathBuf;
use std::process::ExitCode;

use agentd_conformance::run_corpus;

fn main() -> ExitCode {
    let mut dir: Option<PathBuf> = None;
    let mut json = false;
    let mut min_pass_rate: Option<f64> = None;
    let mut forecast_runs: Option<f64> = None;
    let mut price_per_mtok: Option<f64> = None;
    let mut baseline: Option<PathBuf> = None;
    let mut save_baseline: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let num =
            |a: &mut dyn Iterator<Item = String>| a.next().and_then(|v| v.parse::<f64>().ok());
        match arg.as_str() {
            "run" => {}
            "--json" => json = true,
            "--min-pass-rate" => match num(&mut args) {
                Some(v) if (0.0..=1.0).contains(&v) => min_pass_rate = Some(v),
                _ => return usage_err("--min-pass-rate expects a number in [0,1]"),
            },
            "--forecast-runs-per-day" => match num(&mut args) {
                Some(v) if v >= 0.0 => forecast_runs = Some(v),
                _ => return usage_err("--forecast-runs-per-day expects a non-negative number"),
            },
            "--price-per-mtok" => match num(&mut args) {
                Some(v) if v >= 0.0 => price_per_mtok = Some(v),
                _ => return usage_err("--price-per-mtok expects a non-negative number"),
            },
            "--save-baseline" => match args.next() {
                Some(p) => save_baseline = Some(PathBuf::from(p)),
                None => return usage_err("--save-baseline expects a path"),
            },
            "--baseline" => match args.next() {
                Some(p) => baseline = Some(PathBuf::from(p)),
                None => return usage_err("--baseline expects a path"),
            },
            "-h" | "--help" => {
                eprintln!(
                    "usage: agentd-conformance [run] [CORPUS_DIR] [--json] [--min-pass-rate T]"
                );
                eprintln!("       [--forecast-runs-per-day N] [--price-per-mtok P]");
                eprintln!("       [--save-baseline PATH] [--baseline PATH]");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                return usage_err(&format!("unknown flag `{other}`"));
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

    let mut failed = !report.all_passed();

    // Reliability gate: declared bars + the optional suite-wide floor.
    let violations = report.reliability_violations(min_pass_rate);
    if !violations.is_empty() {
        eprintln!("\nreliability gate FAILED:");
        for v in &violations {
            eprintln!(
                "  {} — pass_rate {:.2} < required {:.2}",
                v.name, v.pass_rate, v.required
            );
        }
        failed = true;
    }

    // Cost forecast.
    if let Some(rpd) = forecast_runs {
        match report.forecast(rpd, price_per_mtok) {
            Some(f) => {
                eprintln!(
                    "\nforecast @ {:.0} runs/day: {:.0} tokens/success → {:.0} tokens/day, \
                     {:.0} tokens/month{}",
                    f.runs_per_day,
                    f.cost_per_success_tokens,
                    f.tokens_per_day,
                    f.tokens_per_month,
                    f.usd_per_month
                        .map(|u| format!(" (~${u:.2}/month)"))
                        .unwrap_or_default(),
                );
            }
            None => eprintln!("\nforecast: no successful runs to base a projection on"),
        }
    }

    // Drift vs a saved baseline.
    if let Some(path) = &baseline {
        match std::fs::read_to_string(path).and_then(|s| {
            serde_json::from_str::<serde_json::Value>(&s)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) {
            Ok(base) => {
                let drift = report.drift_vs(&base);
                let regressions: Vec<_> = drift.iter().filter(|d| d.regressed).collect();
                if regressions.is_empty() {
                    eprintln!("\ndrift vs baseline: no pass_rate regressions");
                } else {
                    eprintln!("\ndrift vs baseline — REGRESSIONS:");
                    for d in regressions {
                        eprintln!(
                            "  {} — pass_rate {:.2} → {:.2}  (tokens {} → {})",
                            d.name, d.old_pass_rate, d.new_pass_rate, d.old_tokens, d.new_tokens
                        );
                    }
                    failed = true;
                }
            }
            Err(e) => {
                eprintln!("agentd-conformance: read baseline {}: {e}", path.display());
                failed = true;
            }
        }
    }

    if let Some(path) = &save_baseline {
        let body = serde_json::to_string_pretty(&report.to_json()).unwrap();
        if let Err(e) = std::fs::write(path, body) {
            eprintln!("agentd-conformance: write baseline {}: {e}", path.display());
            failed = true;
        } else {
            eprintln!("baseline written to {}", path.display());
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn usage_err(msg: &str) -> ExitCode {
    eprintln!("agentd-conformance: {msg}");
    ExitCode::from(2)
}
