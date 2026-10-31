// SPDX-License-Identifier: AGPL-3.0-only
//! Print the agent text agentd delivers for an instruction/1 document, and
//! what it maps onto configuration.
//!
//! `cargo run -p agentd-core --example deliver -- <doc.md>`
//!
//! Grants everything, so every connected object maps. The agent text goes
//! to stdout, byte for byte; the diagnostics and the mapping go to stderr.

use agentd::config::idoc;

fn main() {
    let path = std::env::args().nth(1).expect("usage: deliver <doc.md>");
    let text = std::fs::read_to_string(&path).expect("read the document");
    match idoc::extract(&text, &idoc::all_families()) {
        Ok(ex) => {
            print!("{}", ex.cleaned);
            for w in &ex.warnings {
                eprintln!("warning: {w}");
            }
            eprintln!(
                "maps: {}",
                serde_json::json!({
                    "config": ex.config,
                    "workflows": ex.workflows.iter().filter_map(|w| w["name"].as_str()).collect::<Vec<_>>(),
                    "skills": ex.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                    "people": ex.people,
                    "declarations": ex.declarations.keys().collect::<Vec<_>>(),
                })
            );
        }
        Err(errs) => {
            eprintln!("refused:");
            for e in errs {
                eprintln!("  {e} ({})", e.code);
            }
            std::process::exit(2);
        }
    }
}
