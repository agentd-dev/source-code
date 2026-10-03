// SPDX-License-Identifier: AGPL-3.0-only
//! The fixture dumper (E1-06): print one artifact of a document, as the
//! shared conformance corpus pins it — the §9.1 block tree, the delivered
//! text, the §7.4 resolution manifest (pretty, and in its signed canonical
//! form), or the refusals.
//!
//! ```console
//! $ cargo run -p agentd-instruction --example dump -- text      corpus/variants > delivered.txt
//! $ cargo run -p agentd-instruction --example dump -- manifest  corpus/variants > manifest.json
//! $ cargo run -p agentd-instruction --example dump -- canonical corpus/variants > manifest.canonical.json
//! $ cargo run -p agentd-instruction --example dump -- refusals  refusals/bare-machinery-kind > refusals.json
//! $ cargo run -p agentd-instruction --example dump -- tree      path/to/doc.md path/to/context.json
//! ```
//!
//! The input is a case directory (its `doc.md`, and its `context.json` when
//! there is one) or a `doc.md` with an optional `context.json`, shaped
//! `{"params": {…}, "facts": {…}}`. Every family is granted, and includes
//! resolve by front-matter `id` among the documents of the case directories
//! beside the document's own — the conformance runner's context and
//! resolver, read by the same code (`tests/support/corpus_case.rs`), so the
//! text, manifest, canonical and refusals modes print a corpus case's
//! fixture bytes. `tree` prints the same tree as `tree.json` holds, its
//! members in `serde_json`'s key order rather than the fixture's.
//!
//! `refusals` prints the `refusals.json` shape, `[{line, code, message}]`,
//! with the line as data and never inside the message.

use std::path::{Path, PathBuf};

use instruction_core::{Context, deliver, parse, tree_json, validate};

#[path = "../tests/support/corpus_case.rs"]
mod corpus_case;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| usage());
    let input = PathBuf::from(args.next().unwrap_or_else(|| usage()));
    let (doc_path, ctx_path) = if input.is_dir() {
        (input.join("doc.md"), Some(input.join("context.json")))
    } else {
        (input.clone(), args.next().map(PathBuf::from))
    };
    let text = std::fs::read_to_string(&doc_path).expect("read the document");

    let (params, facts) = match &ctx_path {
        Some(p) => corpus_case::context_of(p),
        None => Default::default(),
    };
    // The case directory's parent holds the documents an include resolves
    // among, as the corpus does.
    let corpus = doc_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let by_id = corpus_case::documents_by_id(&corpus);
    let resolver = |id: &str| by_id.get(id).cloned();
    let ctx = Context {
        grants: instruction_core::doc::all_families(),
        params,
        facts,
        resolve_include: Some(&resolver),
    };

    let refused = |errs: Vec<instruction_core::Refusal>| -> ! {
        for e in errs {
            eprintln!("{e}");
        }
        std::process::exit(2)
    };
    let out = match (mode.as_str(), parse(&text)) {
        ("refusals", Err(errs)) => pretty(&errs),
        ("refusals", Ok(d)) => pretty(&validate(&d, &ctx)),
        (_, Err(errs)) => refused(errs),
        ("tree", Ok(d)) => pretty(&tree_json(&d)),
        ("text", Ok(d)) => deliver(&d, &ctx).unwrap_or_else(|e| refused(e)).text,
        ("manifest", Ok(d)) => pretty(&deliver(&d, &ctx).unwrap_or_else(|e| refused(e)).manifest),
        // The signed form's exact bytes: no newline after it.
        ("canonical", Ok(d)) => deliver(&d, &ctx)
            .unwrap_or_else(|e| refused(e))
            .manifest
            .canonical(),
        (other, Ok(_)) => {
            eprintln!("unknown mode {other:?}");
            usage();
        }
    };
    print!("{out}");
}

/// An artifact as the fixtures are exported: pretty JSON and a newline.
fn pretty<T: serde::Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string_pretty(v).expect("an artifact is JSON") + "\n"
}

fn usage() -> ! {
    eprintln!(
        "usage: dump <tree|text|manifest|canonical|refusals> <case-dir | doc.md [context.json]>"
    );
    std::process::exit(64)
}
