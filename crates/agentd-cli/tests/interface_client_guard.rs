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

/// The source and constant naming the OAuth `client_id` the client `agentd
/// <sub>` starts presents when it redeems its launch code.
fn client_id_const(sub: &str) -> (&'static str, &'static str) {
    match sub {
        "tui" => ("src/tui/args.ts", "TUI_CLIENT_ID"),
        "ui" => ("src/ui/app.tsx", "UI_CLIENT_ID"),
        other => panic!("no known client_id constant for the {other} client: name it here"),
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

/// The client uses an extension only when the card declares that exact URI,
/// so a client constant that drifted from the daemon's drops the TUI and the
/// web UI to core mode without a word — and every interface test would still
/// pass, because they read the client's own constants. Each is held to the
/// daemon's here, and every op the client sends to one the daemon has.
#[test]
fn the_clients_extension_vocabulary_is_the_daemons() {
    use agentd::runtime::surface as daemon;
    let ext = interface_src("src/client/ext.ts");
    for (name, want) in [
        ("COMMAND_EXTENSION", daemon::COMMAND_EXTENSION),
        ("EVENTS_EXTENSION", daemon::EVENTS_EXTENSION),
        (
            "TASK_ANNOTATIONS_EXTENSION",
            daemon::TASK_ANNOTATIONS_EXTENSION,
        ),
        ("UNIX_BINDING", daemon::UNIX_BINDING),
        ("EVENTS_METHOD", daemon::EVENTS_METHOD),
    ] {
        assert_eq!(
            ts_const(&ext, name).as_deref(),
            Some(want),
            "interface/src/client/ext.ts {name} must equal runtime/surface/ext.rs {name}"
        );
    }
    let ops = ts_ops(&ext);
    assert!(ops.len() > 10, "OPS parsed as {ops:?}");
    for op in &ops {
        assert!(
            daemon::op_spec(op).is_some(),
            "interface/src/client/ext.ts OPS sends `{op}`, which is no op the daemon serves"
        );
    }
}

/// The values of ext.ts's `OPS` table (`key: 'op',` lines).
fn ts_ops(src: &str) -> Vec<String> {
    let start = src
        .find("export const OPS = Object.freeze({")
        .expect("ext.ts declares OPS");
    let body = &src[start..];
    // The table closes on the first line that starts with `}`.
    let body = &body[..body.find("\n}").expect("OPS closes")];
    body.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':').map(|(_, v)| v.trim()))
        .map(|v| {
            v.trim_end_matches(',')
                .trim_matches(['\'', '"'])
                .to_string()
        })
        .collect()
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
/// daemon's token endpoint accepts, the `client_id` it presents is the one
/// its code is issued to (a code is refused to any other, so a drifted id
/// fails every launch with `invalid_grant`), and every flag the launcher
/// passes a client is one that client's parser reads — so neither side can
/// rename its half of the contract alone.
#[test]
fn the_launch_contract_matches_the_clients() {
    use agentd::runtime::surface::launch::{LAUNCH_CONTRACT, LAUNCH_GRANT_TYPE};
    assert_eq!(
        ts_const(&interface_src("src/client/auth.ts"), "LAUNCH_GRANT_TYPE").as_deref(),
        Some(LAUNCH_GRANT_TYPE),
        "interface/src/client/auth.ts LAUNCH_GRANT_TYPE must be the grant the daemon redeems"
    );
    for client in LAUNCH_CONTRACT {
        let (at, name) = client_id_const(client.sub);
        assert_eq!(
            ts_const(&interface_src(at), name).as_deref(),
            Some(client.client_id),
            "interface/{at} {name} must be the client_id `agentd {}` issues its code to",
            client.sub
        );
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
