// SPDX-License-Identifier: AGPL-3.0-only
//! Print the delivered text of an instruction document (§3.5 pipeline).
//!
//! `cargo run -p agentd-core --example deliver -- <doc.md> [name=value ...]`
//!
//! Grants every family so the whole document folds; `name=value` arguments
//! supply `${}` parameter values.

use std::collections::BTreeMap;

use agentd::config::idoc;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: deliver <doc.md> [name=value ...]");
    let text = std::fs::read_to_string(&path).expect("read the document");

    let mut overrides = BTreeMap::new();
    for a in args {
        if let Some((k, v)) = a.split_once('=') {
            overrides.insert(k.to_string(), v.to_string());
        }
    }

    let doc = match idoc::parse(&text) {
        Ok(d) => d,
        Err(errs) => {
            eprintln!("refused:\n  {}", errs.join("\n  "));
            std::process::exit(2);
        }
    };
    match idoc::fold_with_params(&doc, &idoc::all_families(), &overrides) {
        Ok(ex) => print!("{}", ex.cleaned),
        Err(errs) => {
            eprintln!("refused:\n  {}", errs.join("\n  "));
            std::process::exit(2);
        }
    }
}
