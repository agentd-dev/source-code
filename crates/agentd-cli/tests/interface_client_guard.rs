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
//! **Monorepo-only.** It reads `interface/src/client/wire.ts` beside this
//! crate, so it guards the client only while the two live in one repository.
//! A client built elsewhere is held to the same contract by the contract job
//! that runs its own suite against a daemon, not by this file.

use std::path::Path;

/// The client transport, which every JSON-RPC request goes through.
fn wire_ts() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interface/src/client/wire.ts");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — this guard is monorepo-only and expects the interface client beside the crates",
            path.display()
        )
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
