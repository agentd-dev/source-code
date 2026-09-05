// SPDX-License-Identifier: AGPL-3.0-only
//! The fixture dumper (E1-06): parse a document and print, per the flag, the
//! §9.1 block tree, the delivered text, the resolution manifest, or the
//! refusals — the four artifacts the shared corpus compares implementations
//! by.
//!
//! ```console
//! $ cargo run -p instruction-core --example dump -- tree  doc.md            > tree.json
//! $ cargo run -p instruction-core --example dump -- text  doc.md ctx.json   > delivered.txt
//! $ cargo run -p instruction-core --example dump -- manifest doc.md ctx.json > manifest.json
//! $ cargo run -p instruction-core --example dump -- refusals doc.md         > refusals.json
//! ```
//!
//! `ctx.json` (optional): `{"grants": ["compute", …], "params": {"env": "prod"},
//! "includes": {"<id>": "<inline document text>"}}` — includes are given
//! inline so a fixture is one directory with no resolver of its own.

use std::collections::{BTreeMap, BTreeSet};

use instruction_core::{Context, deliver, parse, tree_json, validate};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| usage());
    let path = args.next().unwrap_or_else(|| usage());
    let text = std::fs::read_to_string(&path).expect("read the document");

    // Context (grants/params/includes) from an optional JSON file.
    let ctx_v: serde_json::Value = args
        .next()
        .map(|p| {
            serde_json::from_str(&std::fs::read_to_string(&p).expect("read ctx"))
                .expect("ctx is JSON")
        })
        .unwrap_or_else(|| serde_json::json!({}));
    let grants: BTreeSet<String> = ctx_v["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let params: BTreeMap<String, String> = ctx_v["params"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    let facts: BTreeMap<String, String> = ctx_v["facts"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    let includes: BTreeMap<String, String> = ctx_v["includes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    let resolver = move |id: &str| includes.get(id).cloned();
    let ctx = Context {
        grants,
        params,
        facts,
        resolve_include: Some(&resolver),
    };

    match (mode.as_str(), parse(&text)) {
        ("refusals", Err(errs)) => {
            println!("{}", serde_json::to_string_pretty(&errs).unwrap());
        }
        ("refusals", Ok(d)) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&validate(&d, &ctx)).unwrap()
            );
        }
        (_, Err(errs)) => {
            eprintln!("refused:");
            for e in errs {
                eprintln!("  {e}");
            }
            std::process::exit(2);
        }
        ("tree", Ok(d)) => {
            println!("{}", serde_json::to_string_pretty(&tree_json(&d)).unwrap());
        }
        ("text", Ok(d)) => match deliver(&d, &ctx) {
            Ok(out) => print!("{}", out.text),
            Err(errs) => {
                for e in errs {
                    eprintln!("{e}");
                }
                std::process::exit(2);
            }
        },
        ("manifest", Ok(d)) => match deliver(&d, &ctx) {
            Ok(out) => println!("{}", serde_json::to_string_pretty(&out.manifest).unwrap()),
            Err(errs) => {
                for e in errs {
                    eprintln!("{e}");
                }
                std::process::exit(2);
            }
        },
        (other, Ok(_)) => {
            eprintln!("unknown mode {other:?}");
            usage();
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: dump <tree|text|manifest|refusals> <doc.md> [ctx.json]");
    std::process::exit(64)
}
