//! The conformance runner: run every check against a freshly-built agentd and
//! render a PASS/FAIL report. `--json` emits the machine-readable record. Exits
//! non-zero if any check fails (so it doubles as a CI gate).

use agentd_conformance::report::Record;
use agentd_conformance::{all_checks, run_check, Harness, Report};

fn main() {
    let json_mode = std::env::args().any(|a| a == "--json");

    eprintln!("building + locating agentd…");
    let h = Harness::new();

    let records: Vec<Record> = all_checks()
        .into_iter()
        .map(|check| {
            let outcome = run_check(&h, &check);
            if !json_mode {
                let mark = if outcome.passed { "ok  " } else { "FAIL" };
                eprintln!("  [{mark}] {}", check.id);
            }
            Record { id: check.id, category: check.category, desc: check.desc, outcome }
        })
        .collect();

    let report = Report::new(records);
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&report.to_json()).expect("json"));
    } else {
        print!("{}", report.render_text());
    }

    if !report.all_passed() {
        std::process::exit(1);
    }
}
