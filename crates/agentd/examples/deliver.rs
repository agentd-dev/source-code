// SPDX-License-Identifier: AGPL-3.0-only
//! Print the delivered text of an instruction document (§3.5 pipeline).
//!
//! `cargo run -p agentd-core --example deliver -- [--model <id>] <doc.md> [name=value ...]`
//!
//! Grants every family so the whole document folds; `name=value` arguments
//! supply `${}` parameter values. `--model` names the model the variants are
//! decided for, as `intelligence.model` would.

use std::collections::BTreeMap;

use agentd::config::idoc;

fn main() {
    const USAGE: &str = "usage: deliver [--model <id>] <doc.md> [name=value ...]";
    let mut args = std::env::args().skip(1).peekable();
    let model = if args.peek().is_some_and(|a| a == "--model") {
        args.next();
        Some(args.next().expect(USAGE))
    } else {
        None
    };
    let path = args.next().expect(USAGE);
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
            eprintln!("refused:");
            for e in errs {
                eprintln!("  {e}");
            }
            std::process::exit(2);
        }
    };

    // Resolve `::include{id=…}` from sibling documents: a file in the same
    // directory whose front-matter `id` ends with the requested id.
    let dir = std::path::Path::new(&path)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let resolve = move |id: &str| -> Option<String> {
        let want = format!("ins_{}", id.trim_start_matches("ins_"));
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "md")
                && let Ok(t) = std::fs::read_to_string(&p)
                && idoc::parse(&t)
                    .ok()
                    .and_then(|d| {
                        d.front
                            .get("id")
                            .and_then(|v| v.as_str().map(str::to_string))
                    })
                    .is_some_and(|fid| fid.ends_with(&want))
            {
                return Some(t);
            }
        }
        None
    };

    // The facts the daemon supplies (§5.2) — the library assumes none.
    let facts = idoc::instruction_facts(model.as_deref());
    match idoc::fold_full(&doc, &idoc::all_families(), &overrides, &facts, &resolve) {
        Ok(ex) => print!("{}", ex.cleaned),
        Err(errs) => {
            eprintln!("refused:");
            for e in errs {
                eprintln!("  {e}");
            }
            std::process::exit(2);
        }
    }
}
