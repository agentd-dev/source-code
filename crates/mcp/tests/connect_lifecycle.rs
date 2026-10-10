// SPDX-License-Identifier: AGPL-3.0-only
//! How a connection reaches a server, at either revision.
//!
//! A connection that starts with `server/discover` reaches a 2026-07-28 server
//! with that alone — no `initialize`, no session — and falls back to the
//! 2025-11-25 handshake for a server that predates it, whatever shape the
//! server's refusal takes. What a fallback cannot fix is refused instead: a
//! modern server rejecting agentd's own request, an auth failure that carries
//! the challenge, a server that was never reached. Each test asserts what
//! reached the wire, request by request.

mod common;

use common::*;
use mcp::client::{FirstRequest, McpClient};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn client(ep: &str, timeout: Duration, first: FirstRequest) -> McpClient {
    McpClient::connect("t", ep, vec![], timeout)
        .unwrap()
        .start_with(first)
}

fn connect(ep: &str, first: FirstRequest) -> Result<McpClient, String> {
    let mut c = client(ep, Duration::from_secs(5), first);
    c.initialize().map_err(|e| e.to_string())?;
    Ok(c)
}

/// Every POST after the handshake carries the revision header and the
/// session `initialize` issued, and no 2026-07-28 routing header.
fn assert_legacy_wire(seen: &Seen, session: &str) {
    let reqs = seen.lock().unwrap().clone();
    let after: Vec<_> = reqs
        .iter()
        .filter(|r| !r.is_get())
        .skip_while(|r| r.method() != "initialize")
        .skip(1)
        .collect();
    assert!(!after.is_empty(), "nothing after the handshake");
    for r in after {
        assert_eq!(
            r.header("mcp-protocol-version"),
            Some("2025-11-25"),
            "{}",
            r.method()
        );
        assert_eq!(r.header("mcp-session-id"), Some(session), "{}", r.method());
    }
}

#[test]
fn a_server_without_server_discover_is_reached_through_the_2025_11_25_handshake() {
    for shape in LEGACY_SHAPES {
        let (ep, seen) = legacy_server(LegacyOpts {
            discover: shape,
            ..LegacyOpts::default()
        });
        let c = connect(&ep, FirstRequest::Discover).unwrap_or_else(|e| panic!("{shape:?}: {e}"));
        c.list_tools().unwrap_or_else(|e| panic!("{shape:?}: {e}"));

        let m = methods(&seen);
        assert_eq!(
            m[..3],
            ["server/discover", "initialize", "notifications/initialized"],
            "{shape:?}: {m:?}"
        );
        let discover = &requests(&seen, "server/discover")[0];
        assert_eq!(discover.count("mcp-protocol-version"), 1, "{shape:?}");
        assert_eq!(
            discover.header("mcp-protocol-version"),
            Some("2026-07-28"),
            "{shape:?}"
        );
        assert_eq!(
            discover.header("mcp-method"),
            Some("server/discover"),
            "{shape:?}"
        );
        assert_eq!(discover.header("mcp-session-id"), None, "{shape:?}");
        assert_eq!(
            discover.meta("protocolVersion"),
            Some(&json!("2026-07-28")),
            "{shape:?}"
        );
        assert_eq!(
            discover.meta("clientInfo").unwrap()["name"],
            "agentd",
            "{shape:?}"
        );
        assert!(discover.meta("clientCapabilities").is_some(), "{shape:?}");

        let init = &requests(&seen, "initialize")[0];
        assert_eq!(
            init.body["params"]["protocolVersion"], "2025-11-25",
            "{shape:?}"
        );
        assert_eq!(init.header("mcp-method"), None, "{shape:?}");
        assert_eq!(init.header("mcp-session-id"), None, "{shape:?}");

        assert_legacy_wire(&seen, "s-1");
        assert_eq!(
            c.protocol_version().as_deref(),
            Some("2025-11-25"),
            "{shape:?}"
        );
        assert_eq!(
            c.first_request(),
            Some(FirstRequest::Initialize),
            "{shape:?}"
        );
    }
}

#[test]
fn a_2026_07_28_server_is_reached_through_server_discover_alone() {
    let (ep, seen) = stateless_server(StatelessOpts::default());
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    c.list_tools().unwrap();
    c.call_tool("echo", Some(json!({}))).unwrap();
    c.read_resource("demo://x").unwrap();

    assert_eq!(
        methods(&seen),
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "resources/read"
        ]
    );
    for r in seen.lock().unwrap().iter() {
        assert!(!r.is_get(), "no GET stream at 2026-07-28");
        assert_eq!(r.count("mcp-protocol-version"), 1, "{}", r.method());
        assert_eq!(
            r.header("mcp-protocol-version"),
            Some("2026-07-28"),
            "{}",
            r.method()
        );
        assert_eq!(r.header("mcp-method"), Some(r.method()), "{}", r.method());
        assert_eq!(r.header("mcp-session-id"), None, "{}", r.method());
        assert_eq!(
            r.meta("protocolVersion"),
            Some(&json!("2026-07-28")),
            "{}",
            r.method()
        );
        assert!(r.meta("clientInfo").is_some() && r.meta("clientCapabilities").is_some());
    }
    assert_eq!(
        requests(&seen, "tools/call")[0].header("mcp-name"),
        Some("echo")
    );
    assert_eq!(
        requests(&seen, "resources/read")[0].header("mcp-name"),
        Some("demo://x")
    );
    assert_eq!(c.protocol_version().as_deref(), Some("2026-07-28"));
    assert_eq!(c.first_request(), Some(FirstRequest::Discover));
}

#[test]
fn an_unreachable_server_is_not_taken_for_an_older_server() {
    let ep = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/mcp", l.local_addr().unwrap())
    };
    let err = connect(&ep, FirstRequest::Discover)
        .err()
        .expect("nothing listens");
    assert!(err.contains("server/discover"), "{err}");
    assert!(
        !err.contains("initialize"),
        "no fallback for a dead port: {err}"
    );
}

#[test]
fn a_modern_rejection_of_discover_is_not_taken_for_an_older_server() {
    for code in [-32020, -32021] {
        let (ep, seen) =
            spawn(move |_, w| respond(w, 400, &[], &error(&Value::Null, code, "refused")));
        let err = connect(&ep, FirstRequest::Discover).err().expect("refused");
        assert!(err.contains(&code.to_string()), "{code}: {err}");
        assert_eq!(methods(&seen), ["server/discover"], "{code}");
    }
}

#[test]
fn an_auth_failure_on_discover_is_not_taken_for_an_older_server() {
    let (ep, seen) =
        spawn(|_, w| respond_raw(w, 401, &[("WWW-Authenticate", "Bearer realm=\"x\"")], ""));
    let err = connect(&ep, FirstRequest::Discover).err().expect("401");
    assert!(err.contains("401"), "{err}");
    assert_eq!(methods(&seen), ["server/discover"]);
}

#[test]
fn a_forbidden_discover_falls_back_to_the_handshake() {
    // A proxy that allow-lists methods may answer an unknown one 403 and still
    // pass `initialize`: such a server works today, so it must keep working.
    let (ep, seen) = spawn(|req, w| match req.method() {
        "server/discover" => respond_raw(w, 403, &[], ""),
        "initialize" => respond(
            w,
            200,
            &[("Mcp-Session-Id", "s-1")],
            &initialize_answer(&req.id(), "2025-11-25", &json!({"tools": {}})),
        ),
        _ => respond_raw(w, 202, &[], ""),
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    assert_eq!(c.protocol_version().as_deref(), Some("2025-11-25"));
    assert_eq!(
        methods(&seen)[..3],
        ["server/discover", "initialize", "notifications/initialized"]
    );
}

#[test]
fn a_version_the_server_does_not_take_is_retried_from_its_supported_list() {
    // The first discover asks for a revision the server says it does not
    // speak, listing one agentd does: rmcp asks again at that one.
    let mut refusal = error(&Value::Null, -32022, "Unsupported protocol version");
    refusal["error"]["data"] = json!({"supported": ["2026-07-28"], "requested": "2026-07-28"});
    let (ep, seen) = stateless_server(StatelessOpts {
        first_discover: Some((400, refusal)),
        ..StatelessOpts::default()
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    assert_eq!(c.protocol_version().as_deref(), Some("2026-07-28"));
    assert_eq!(count(&seen, "server/discover"), 2);
    assert_eq!(count(&seen, "initialize"), 0);
}

#[test]
fn a_server_pinned_to_an_older_revision_is_reached_at_its_newest() {
    let cases: [(Value, Option<&str>); 3] = [
        // rmcp's server pinned to 2025-11-25 lists every revision up to it.
        (
            json!(["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]),
            Some("2025-11-25"),
        ),
        (json!(["2025-06-18"]), Some("2025-06-18")),
        (json!(["2027-01-01"]), None),
    ];
    for (supported, want) in cases {
        let s = supported.clone();
        let (ep, seen) = spawn(move |req, w| match req.method() {
            "server/discover" => {
                let mut e = error(&req.id(), -32022, "Unsupported protocol version");
                e["error"]["data"] = json!({"supported": s, "requested": "2026-07-28"});
                respond(w, 400, &[], &e)
            }
            "initialize" => respond(
                w,
                200,
                &[("Mcp-Session-Id", "s-1")],
                &initialize_answer(
                    &req.id(),
                    req.body["params"]["protocolVersion"].as_str().unwrap(),
                    &json!({}),
                ),
            ),
            _ => respond_raw(w, 202, &[], ""),
        });
        let out = connect(&ep, FirstRequest::Discover);
        match want {
            Some(v) => {
                let c = out.unwrap_or_else(|e| panic!("{supported}: {e}"));
                assert_eq!(
                    requests(&seen, "initialize")[0].body["params"]["protocolVersion"],
                    v,
                    "{supported}"
                );
                assert_eq!(c.protocol_version().as_deref(), Some(v));
            }
            None => {
                let e = out.err().expect("nothing in common");
                assert_eq!(count(&seen, "initialize"), 0, "{supported}: {e}");
            }
        }
    }
    // The same, from a DiscoverResult that lists only older revisions.
    let (ep, seen) = spawn(|req, w| match req.method() {
        "server/discover" => respond(
            w,
            200,
            &[],
            &discover_answer(
                &req.id(),
                &StatelessOpts {
                    supported: Some(vec!["2025-06-18".into(), "2025-11-25".into()]),
                    ..StatelessOpts::default()
                },
            ),
        ),
        "initialize" => respond(
            w,
            200,
            &[("Mcp-Session-Id", "s-1")],
            &initialize_answer(&req.id(), "2025-11-25", &json!({})),
        ),
        _ => respond_raw(w, 202, &[], ""),
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    assert_eq!(
        requests(&seen, "initialize")[0].body["params"]["protocolVersion"],
        "2025-11-25"
    );
    assert_eq!(c.protocol_version().as_deref(), Some("2025-11-25"));
}

#[test]
fn a_slow_server_still_connects() {
    // rmcp's own fallback gives `server/discover` 10 s; agentd's gives it the
    // connection's timeout, so a server answering in 11 s still connects.
    let modern = std::thread::spawn(|| {
        let (ep, _) = stateless_server(StatelessOpts {
            delay_discover_ms: 11_000,
            ..StatelessOpts::default()
        });
        let mut c = client(&ep, Duration::from_secs(20), FirstRequest::Discover);
        c.initialize().map(|_| c.protocol_version())
    });
    let legacy = std::thread::spawn(|| {
        let (ep, _) = legacy_server(LegacyOpts {
            delay_discover_ms: 11_000,
            ..LegacyOpts::default()
        });
        let mut c = client(&ep, Duration::from_secs(20), FirstRequest::Discover);
        c.initialize().map(|_| c.protocol_version())
    });
    // A server that never answers discover but answers `initialize`: the
    // connection's timeout ends the wait, and the fallback reaches it.
    let silent = std::thread::spawn(|| {
        let (ep, _) = legacy_server(LegacyOpts {
            silent_discover: true,
            ..LegacyOpts::default()
        });
        let start = Instant::now();
        let mut c = client(&ep, Duration::from_secs(2), FirstRequest::Discover);
        c.initialize()
            .map(|_| (c.protocol_version(), start.elapsed()))
    });
    assert_eq!(
        modern.join().unwrap().unwrap().as_deref(),
        Some("2026-07-28")
    );
    assert_eq!(
        legacy.join().unwrap().unwrap().as_deref(),
        Some("2025-11-25")
    );
    let (v, took) = silent.join().unwrap().unwrap();
    assert_eq!(v.as_deref(), Some("2025-11-25"));
    assert!(took < Duration::from_secs(4), "{took:?}");
}

#[test]
fn a_discover_answer_without_its_cache_fields_still_connects() {
    let (ep, _) = stateless_server(StatelessOpts {
        omit_cache_fields: true,
        ..StatelessOpts::default()
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    assert_eq!(c.protocol_version().as_deref(), Some("2026-07-28"));
}

#[test]
fn a_session_stamped_by_a_2026_07_28_server_is_never_echoed() {
    let (ep, seen) = stateless_server(StatelessOpts {
        stamp_session: Some("nope".into()),
        ..StatelessOpts::default()
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    c.list_tools().unwrap();
    c.call_tool("echo", Some(json!({}))).unwrap();
    // rmcp's server answers an unknown method `404` + -32601: on a request
    // that carried no session, that is not a lost session.
    assert!(c.list_resource_templates().is_err());
    for r in seen.lock().unwrap().iter() {
        assert_eq!(r.header("mcp-session-id"), None, "{}", r.method());
    }
    assert!(!c.session_lost());
}

#[test]
fn a_session_stamped_on_a_refused_handshake_does_not_ride_the_re_probe() {
    // A 2026-07-28 server that refuses `initialize` with a 200 and stamps a
    // session on it: the re-probe and everything after it carry no session.
    let (ep, seen) = spawn(|req, w| {
        let id = req.id();
        match req.method() {
            "initialize" => respond(
                w,
                200,
                &[("Mcp-Session-Id", "stale")],
                &initialize_refusal(&id, "2025-11-25", &[MODERN.to_string()]),
            ),
            "server/discover" => respond(
                w,
                200,
                &[],
                &discover_answer(&id, &StatelessOpts::default()),
            ),
            "tools/list" => respond(w, 200, &[], &result(&id, json!({"tools": []}))),
            _ => respond(w, 404, &[], &error(&id, -32601, "Method not found")),
        }
    });
    let c = connect(&ep, FirstRequest::Initialize).unwrap();
    c.list_tools().unwrap();
    assert!(c.list_resource_templates().is_err());
    let reqs = seen.lock().unwrap().clone();
    for r in reqs.iter().filter(|r| r.method() != "initialize") {
        assert_eq!(r.header("mcp-session-id"), None, "{}", r.method());
    }
    assert!(!c.session_lost(), "a 404 on a sessionless request");
}

#[test]
fn an_initialize_first_connection_re_probes_past_a_forbidden_handshake() {
    let (ep, seen) = spawn(|req, w| {
        let id = req.id();
        match req.method() {
            "initialize" => respond_raw(w, 403, &[], ""),
            "server/discover" => respond(
                w,
                200,
                &[],
                &discover_answer(&id, &StatelessOpts::default()),
            ),
            _ => respond(w, 200, &[], &result(&id, json!({"tools": []}))),
        }
    });
    let c = connect(&ep, FirstRequest::Initialize).unwrap();
    assert_eq!(methods(&seen)[..2], ["initialize", "server/discover"]);
    assert_eq!(c.protocol_version().as_deref(), Some("2026-07-28"));
}

#[test]
fn a_session_stamped_on_a_discover_refusal_does_not_ride_the_handshake() {
    let (ep, seen) = legacy_server(LegacyOpts {
        stamp_on_refusal: Some("pre".into()),
        session: "real".into(),
        ..LegacyOpts::default()
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    c.list_tools().unwrap();
    assert_eq!(
        requests(&seen, "initialize")[0].header("mcp-session-id"),
        None
    );
    assert_legacy_wire(&seen, "real");
}

#[test]
fn a_server_that_refuses_discover_and_the_handshake_names_both() {
    let (ep, _) = legacy_server(LegacyOpts {
        refuse_initialize: Some((401, 0)),
        ..LegacyOpts::default()
    });
    let err = connect(&ep, FirstRequest::Discover).err().expect("refused");
    assert!(err.contains("server/discover"), "{err}");
    assert!(err.contains("initialize"), "{err}");
    assert!(err.contains("401"), "{err}");
}

#[test]
fn the_raw_capabilities_keep_members_rmcp_does_not_know() {
    let caps = json!({"tools": {}, "events": {"x": 1}});
    let (modern, _) = stateless_server(StatelessOpts {
        caps: caps.clone(),
        ..StatelessOpts::default()
    });
    let (legacy, _) = legacy_server(LegacyOpts {
        caps,
        ..LegacyOpts::default()
    });
    for ep in [modern, legacy] {
        let c = connect(&ep, FirstRequest::Discover).unwrap();
        assert_eq!(c.capability("events"), Some(json!({"x": 1})), "{ep}");
        assert_eq!(c.capability("tools"), Some(json!({})), "{ep}");
        assert_eq!(c.capability("nope"), None, "{ep}");
    }
}

#[test]
fn an_initialize_first_connection_re_probes_when_the_handshake_is_refused() {
    let (ep, seen) = stateless_server(StatelessOpts::default());
    let c = connect(&ep, FirstRequest::Initialize).unwrap();
    assert_eq!(methods(&seen)[..2], ["initialize", "server/discover"]);
    assert_eq!(c.protocol_version().as_deref(), Some("2026-07-28"));
    assert_eq!(c.first_request(), Some(FirstRequest::Discover));
}

#[test]
fn an_initialize_first_connection_whose_re_probe_is_refused_too_does_not_loop() {
    let (ep, seen) = legacy_server(LegacyOpts {
        discover: LegacyDiscover::MethodNotFound200,
        refuse_initialize: Some((400, -32600)),
        ..LegacyOpts::default()
    });
    let err = connect(&ep, FirstRequest::Initialize)
        .err()
        .expect("refused");
    assert!(
        err.contains("initialize") && err.contains("server/discover"),
        "{err}"
    );
    assert!(
        err.contains("initialize refused") && err.contains("Method not found"),
        "{err}"
    );
    assert_eq!(count(&seen, "initialize"), 1, "{err}");
    assert_eq!(count(&seen, "server/discover"), 1, "{err}");
}

#[test]
fn a_discover_first_connection_whose_fallback_is_refused_does_not_re_probe() {
    let (ep, seen) = legacy_server(LegacyOpts {
        discover: LegacyDiscover::MethodNotFound200,
        refuse_initialize: Some((400, -32600)),
        ..LegacyOpts::default()
    });
    let err = connect(&ep, FirstRequest::Discover).err().expect("refused");
    assert!(
        err.contains("initialize") && err.contains("server/discover"),
        "{err}"
    );
    assert_eq!(count(&seen, "initialize"), 1, "{err}");
    assert_eq!(count(&seen, "server/discover"), 1, "{err}");
}

#[test]
fn a_declared_capability_reaches_the_discover_meta() {
    struct Never;
    impl mcp::inbound::Handler for Never {
        fn handle(&self, _: mcp::inbound::Inbound) -> Option<mcp::inbound::Answer> {
            None
        }
    }
    let (ep, seen) = stateless_server(StatelessOpts::default());
    let mut c = client(&ep, Duration::from_secs(5), FirstRequest::Discover)
        .with_elicitation(std::sync::Arc::new(Never));
    c.initialize().unwrap();
    c.list_tools().unwrap();
    for r in seen.lock().unwrap().iter() {
        assert!(
            r.meta("clientCapabilities")
                .unwrap()
                .get("elicitation")
                .is_some(),
            "{}: {:?}",
            r.method(),
            r.meta("clientCapabilities")
        );
    }
}

#[test]
fn a_redial_starts_with_the_handshake_that_reached_the_server() {
    // A 2025-11-25 server reached by fallback forgets its session. The
    // re-dial starts with `initialize`, as the lost connection was reached —
    // not with another `server/discover` — so it stays two POSTs, inside the
    // short bound a host re-dials under.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let generation = std::sync::Arc::new(AtomicU64::new(0));
    let forget = std::sync::Arc::new(AtomicBool::new(false));
    let (g, f) = (generation.clone(), forget.clone());
    let (ep, seen) = spawn(move |req, w| {
        let id = req.id();
        match req.method() {
            "server/discover" => refuse_discover(w, LegacyDiscover::MethodNotFound200, &id, &[]),
            "initialize" => {
                let n = g.fetch_add(1, Ordering::SeqCst) + 1;
                f.store(false, Ordering::SeqCst);
                respond(
                    w,
                    200,
                    &[("Mcp-Session-Id", &format!("s-{n}"))],
                    &initialize_answer(&id, "2025-11-25", &json!({"tools": {}})),
                )
            }
            _ if f.load(Ordering::SeqCst) => respond_raw(w, 404, &[], ""),
            m if m.starts_with("notifications/") => respond_raw(w, 202, &[], ""),
            _ => respond(w, 200, &[], &result(&id, json!({"tools": []}))),
        }
    });
    let c = connect(&ep, FirstRequest::Discover).unwrap();
    assert_eq!(c.first_request(), Some(FirstRequest::Initialize));
    forget.store(true, Ordering::SeqCst);
    assert!(c.list_tools().is_err());
    assert!(c.session_lost());
    let before = methods(&seen).len();
    c.redial_within(Duration::from_secs(2)).expect("re-dialed");
    let m = methods(&seen);
    assert_eq!(m[before], "initialize", "{m:?}");
    assert_eq!(count(&seen, "server/discover"), 1, "{m:?}");
    c.list_tools().unwrap();
    assert_eq!(generation.load(Ordering::SeqCst), 2);
}
