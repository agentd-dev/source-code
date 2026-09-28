// SPDX-License-Identifier: AGPL-3.0-only
//! **The display clients speak the protocol version the listener serves.**
//!
//! The listener refuses every JSON-RPC request whose `A2A-Version` is not the
//! one it speaks (`-32009`), so a TypeScript client whose constant drifted from
//! the daemon's — or whose transport stopped sending the header — would fail
//! every call it makes, and nothing on the Rust side would notice until a
//! person opened the TUI. This reads the client's source and holds it to the
//! daemon's constant.
//!
//! The same holds for the launcher's contract with the clients it starts: the
//! launch grant type the client redeems its code with, and every flag the
//! launcher passes, must be what the client's source actually reads.
//!
//! **Monorepo-only.** It reads the interface sources beside this crate
//! (`interface/src/client/*.ts`, the clients' argument parsers), so it guards
//! the clients only while they live in one repository.
//! A client built elsewhere is held to the same contract by the contract job
//! that runs its own suite against a daemon, not by this file.

use std::path::Path;

/// A source file of the interface, by its path under `interface/`.
fn interface_src(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interface")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — this guard is monorepo-only and expects the interface client beside the crates",
            path.display()
        )
    })
}

/// The client transport, which every JSON-RPC request goes through.
fn wire_ts() -> String {
    interface_src("src/client/wire.ts")
}

/// The source that parses the argv of the client `agentd <sub>` starts.
fn argv_parser(sub: &str) -> &'static str {
    match sub {
        "tui" => "src/tui/args.ts",
        "ui" => "bin/serve.mjs",
        other => panic!("no known argument parser for the {other} client: name it here"),
    }
}

/// Whether `src` reads `flag`: as the literal flag (`'--endpoint'`), or by
/// the name a `--${name}` helper builds it from (`opt('endpoint'`).
fn reads_flag(src: &str, flag: &str) -> bool {
    let name = flag.trim_start_matches('-');
    ['\'', '"'].iter().any(|q| {
        src.contains(&format!("{q}{flag}{q}")) || src.contains(&format!("opt({q}{name}{q}"))
    })
}

/// The value of `export const <name> = '<value>';`, or `None`.
fn ts_const(src: &str, name: &str) -> Option<String> {
    let decl = format!("export const {name} = ");
    let rest = &src[src.find(&decl)? + decl.len()..];
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let value = &rest[1..];
    Some(value[..value.find(quote)?].to_string())
}

#[test]
fn interface_client_speaks_the_served_protocol_version() {
    let src = wire_ts();
    assert_eq!(
        ts_const(&src, "A2A_VERSION").as_deref(),
        Some(agentd::runtime::surface::A2A_PROTOCOL_VERSION),
        "interface/src/client/wire.ts A2A_VERSION must be the version the listener serves"
    );
    // …and the transport puts it on the request. The header is written in one
    // place, `headers()`, which every call and stream goes through; a constant
    // nobody sends would pass the check above and fail every request.
    let headers = src
        .find("function headers(")
        .map(|at| &src[at..])
        .and_then(|f| f.find("\n}").map(|end| &f[..end]))
        .expect("wire.ts has a headers() function");
    assert!(
        headers.contains("h['a2a-version'] = A2A_VERSION"),
        "headers() must send `a2a-version: A2A_VERSION` on every request:\n{headers}"
    );
}

#[test]
fn the_const_reader_reads_what_it_is_given() {
    assert_eq!(
        ts_const("export const A2A_VERSION = '1.0';", "A2A_VERSION").as_deref(),
        Some("1.0")
    );
    assert_eq!(
        ts_const("export const A2A_VERSION = \"2.1\";", "A2A_VERSION").as_deref(),
        Some("2.1")
    );
    assert_eq!(ts_const("const A2A_VERSION = '1.0';", "A2A_VERSION"), None);
}

/// The launch grant the client redeems its launch code with is the one the
/// daemon's token endpoint accepts, and every flag the launcher passes a
/// client is one that client's parser reads — so neither side can rename its
/// half of the contract alone.
#[test]
fn the_launch_contract_matches_the_clients() {
    use agentd::runtime::surface::launch::{LAUNCH_CONTRACT, LAUNCH_GRANT_TYPE};
    assert_eq!(
        ts_const(&interface_src("src/client/auth.ts"), "LAUNCH_GRANT_TYPE").as_deref(),
        Some(LAUNCH_GRANT_TYPE),
        "interface/src/client/auth.ts LAUNCH_GRANT_TYPE must be the grant the daemon redeems"
    );
    for client in LAUNCH_CONTRACT {
        let parser = argv_parser(client.sub);
        let src = interface_src(parser);
        for flag in client.argv {
            assert!(
                reads_flag(&src, flag),
                "`agentd {}` passes {flag}, which interface/{parser} does not read",
                client.sub
            );
        }
    }
}

#[test]
fn the_flag_reader_reads_what_it_is_given() {
    assert!(reads_flag("valued(argv, i, '--launch-fd')", "--launch-fd"));
    assert!(reads_flag("const e = opt('endpoint', x);", "--endpoint"));
    assert!(!reads_flag(
        "valued(argv, i, '--launch-fd-x')",
        "--launch-fd"
    ));
    assert!(!reads_flag("// --endpoint in a comment", "--endpoint"));
}
