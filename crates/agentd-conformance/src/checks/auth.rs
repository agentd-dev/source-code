// SPDX-License-Identifier: AGPL-3.0-only
//! Authentication honesty: the card's security fields are a promise about the
//! listener, and every check here holds the listener to it from outside.
//!
//! A peer decides how to authenticate by reading `securitySchemes` and
//! `securityRequirements`, so a card that declares a scheme the listener does
//! not enforce, or that omits one it does, sends every such peer to the wrong
//! door. The checks read the card the way that peer would, derive what the
//! listener must do from it alone, and then knock: under each posture an
//! operator can configure — no credential on loopback, a bearer, a bearer
//! beside an `any` rule. The extended card is an authenticated read, so it is
//! declared only where a scheme is and served only to a caller who presented
//! one. And a browser reaches the agent only from an origin the operator
//! listed, and is never the implicit operator.

use serde_json::{Value, json};

use crate::checks::util::{
    Headers, exchange, free_port, get_card, header, mock_llm, rpc_as, rpc_body, wait_ready,
    write_file,
};
use crate::harness::{Daemon, MockLlm, TempDir};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "auth/card-declares-what-the-listener-enforces",
            category: Category::Auth,
            desc: "under no credential, a bearer, and a bearer beside an `any` rule, an uncredentialed call is served exactly when the card's securityRequirements admit it (none, or an empty alternative) and is otherwise a 401 challenging a declared scheme; the declared bearer is served and a wrong one is 401",
            run: card_matches_listener,
        },
        Check {
            id: "auth/extended-card-needs-a-declared-credential",
            category: Category::Auth,
            desc: "on an `any`-rule daemon an uncredentialed GetExtendedAgentCard is 401 and the declared bearer reads it; on a no-auth loopback daemon the card's extendedAgentCard is not true and the method is -32004",
            run: extended_card,
        },
        Check {
            id: "auth/cors-exact-origins",
            category: Category::Auth,
            desc: "only a listed origin is granted (a2a-version preflighted, the challenge readable); its uncredentialed POST is a 401, never the implicit operator; an unlisted one — loopback too — is 403 with no grant; the card is public (ACAO *)",
            run: cors_exact_origins,
        },
    ]
}

/// The bearer the checks configure, handed to the daemon through the
/// environment because agentd refuses a credential written inline.
const TOKEN_ENV: &str = "AGENTD_CONF_BEARER";
const TOKEN: &str = "conformance-bearer-0123456789abcdef";

/// The `a2a:` lines of each posture a check boots.
const BEARER: &str = "  bearer: \"{{secret:AGENTD_CONF_BEARER}}\"\n";
const BEARER_AND_ANY: &str = "  bearer: \"{{secret:AGENTD_CONF_BEARER}}\"\n  principals:\n    - id: pub\n      match: { any: true }\n      role: user\n";

fn config(llm: &str, port: u16, a2a: &str) -> String {
    format!(
        "\
         agent:\n  name: auth-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{a2a}\
         lifecycle:\n  run_until: drained\n"
    )
}

/// A booted daemon and what it runs on; everything stops on drop, the daemon
/// first.
struct Booted {
    _daemon: Daemon,
    _llm: MockLlm,
    _tmp: TempDir,
    addr: String,
}

/// Boot a daemon whose `a2a:` section adds `a2a`.
fn boot(h: &Harness, a2a: &str) -> Booted {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, a2a));
    let daemon = h.spawn_env(&["--config", &cfg], &[(TOKEN_ENV, TOKEN)]);
    wait_ready(&addr);
    Booted {
        _daemon: daemon,
        _llm: llm,
        _tmp: tmp,
        addr,
    }
}

/// Whether the card lets a caller in with no credential: it requires nothing,
/// or one of its alternatives names no scheme (the proto3-JSON `{}`).
fn admits_anonymous(card: &Value) -> bool {
    match card["securityRequirements"].as_array() {
        None => true,
        Some(alts) if alts.is_empty() => true,
        Some(alts) => alts.iter().any(|alt| {
            alt["schemes"]
                .as_object()
                .is_none_or(serde_json::Map::is_empty)
        }),
    }
}

/// The auth-scheme tokens (`Bearer`, …) of the card's HTTP schemes.
fn http_schemes(card: &Value) -> Vec<String> {
    card["securitySchemes"]
        .as_object()
        .map(|schemes| {
            schemes
                .values()
                .filter_map(|s| s["httpAuthSecurityScheme"]["scheme"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A 401 whose challenge names a scheme the card declares.
fn challenged(status: u16, headers: &Headers, v: &Value, card: &Value) -> Result<(), String> {
    let challenge = header(headers, "www-authenticate").unwrap_or("");
    let names_declared = http_schemes(card).iter().any(|s| {
        challenge
            .get(..s.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(s))
    });
    if status == 401 && v["error"]["code"] == -31401 && names_declared {
        Ok(())
    } else {
        Err(format!(
            "expected a 401 challenging a declared scheme ({:?}): {status} {challenge:?} {v}",
            http_schemes(card)
        ))
    }
}

/// Hold one posture's listener to its card.
fn holds_to_card(what: &str, addr: &str) -> Result<(), String> {
    let card = get_card(addr);
    let bearer_declared = http_schemes(&card).iter().any(|s| s == "Bearer");
    // Every requirement names only schemes the card defines: a requirement
    // on an undefined scheme is a door with no description.
    if let Some(alts) = card["securityRequirements"].as_array() {
        for alt in alts {
            for name in alt["schemes"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.keys())
            {
                if card["securitySchemes"].get(name).is_none() {
                    return Err(format!(
                        "{what}: a requirement names an undeclared scheme {name}: {card}"
                    ));
                }
            }
        }
    }

    let (status, headers, v) = rpc_as(addr, "", 1, "ListTasks", json!({}));
    if admits_anonymous(&card) {
        if status != 200 || v.get("error").is_some() {
            return Err(format!(
                "{what}: the card admits a caller with no credential, but the listener refused one: {status} {v}"
            ));
        }
    } else {
        challenged(status, &headers, &v, &card).map_err(|e| format!("{what}: {e}"))?;
    }

    if bearer_declared {
        let (status, _, v) = rpc_as(addr, TOKEN, 2, "ListTasks", json!({}));
        if status != 200 || v.get("error").is_some() {
            return Err(format!(
                "{what}: the declared bearer should be served: {status} {v}"
            ));
        }
        // A scheme the card declares is checked, not decoration: a bearer
        // that matches nothing is a failed credential — never read as no
        // credential, which an `any` rule would admit.
        let (status, headers, v) = rpc_as(addr, "not-the-bearer", 3, "ListTasks", json!({}));
        challenged(status, &headers, &v, &card)
            .map_err(|e| format!("{what}: a wrong bearer: {e}"))?;
    }
    Ok(())
}

fn card_matches_listener(h: &Harness) -> Outcome {
    for (what, a2a, secured) in [
        ("no credential", "", false),
        ("a bearer", BEARER, true),
        ("a bearer and an any rule", BEARER_AND_ANY, true),
    ] {
        let booted = boot(h, a2a);
        let addr = &booted.addr;
        let card = get_card(addr);
        // The posture shows on the card at all: a card that declared no
        // scheme for a secured listener would pass the knock above vacuously.
        if secured != card.get("securitySchemes").is_some() {
            return Outcome::fail(format!(
                "{what}: securitySchemes should be {} on this listener: {card}",
                if secured { "declared" } else { "absent" }
            ));
        }
        if let Err(e) = holds_to_card(what, addr) {
            return Outcome::fail(e);
        }
    }
    Outcome::pass()
}

fn extended_card(h: &Harness) -> Outcome {
    {
        let booted = boot(h, BEARER_AND_ANY);
        let addr = &booted.addr;
        let card = get_card(addr);
        if card["capabilities"]["extendedAgentCard"] != true {
            return Outcome::fail(format!(
                "a listener declaring a bearer should announce the extended card: {card}"
            ));
        }
        // The `any` rule makes the uncredentialed caller somebody, which is
        // enough to talk to the agent — not enough for an authenticated read.
        let (status, headers, v) = rpc_as(addr, "", 1, "GetExtendedAgentCard", json!({}));
        if let Err(e) = challenged(status, &headers, &v, &card) {
            return Outcome::fail(format!("an uncredentialed GetExtendedAgentCard: {e}"));
        }
        let (status, _, v) = rpc_as(addr, TOKEN, 2, "GetExtendedAgentCard", json!({}));
        if status != 200 || !v["result"]["capabilities"].is_object() {
            return Outcome::fail(format!(
                "the declared bearer should read the extended card: {status} {v}"
            ));
        }
    }

    // No scheme declared: nobody can present a declared credential, so there
    // is no extended card to announce or to serve — not even to the implicit
    // operator the loopback bind admits.
    let booted = boot(h, "");
    let addr = &booted.addr;
    let card = get_card(addr);
    if card["capabilities"]["extendedAgentCard"] == true {
        return Outcome::fail(format!(
            "a listener declaring no scheme must not announce an extended card: {card}"
        ));
    }
    let (status, _, v) = rpc_as(addr, "", 3, "GetExtendedAgentCard", json!({}));
    Outcome::require(
        status == 200 && v["error"]["code"] == -32004,
        format!("GetExtendedAgentCard with no scheme declared should be -32004: {status} {v}"),
    )
}

/// The browser path under the strict policy, on a no-auth loopback daemon that
/// lists one UI origin: the listed origin is granted and preflighted with
/// `a2a-version`; its uncredentialed POST is challenged (a browser is never
/// the implicit operator) with the grant and the challenge exposed; another
/// loopback port is refused like any foreign site; and the card is readable
/// from anywhere.
fn cors_exact_origins(h: &Harness) -> Outcome {
    const UI: &str = "http://127.0.0.1:4173";
    let booted = boot(h, &format!("  cors:\n    origins: [\"{UI}\"]\n"));
    let addr = booted.addr.as_str();

    let preflight = |origin: &str| {
        exchange(
            addr,
            "OPTIONS",
            "/",
            &[
                ("Origin", origin),
                ("Access-Control-Request-Method", "POST"),
                (
                    "Access-Control-Request-Headers",
                    "content-type, a2a-version",
                ),
            ],
            "",
        )
    };
    let body = rpc_body(1, "ListTasks", json!({}));
    let post = |origin: &str| {
        exchange(
            addr,
            "POST",
            "/",
            &[
                ("Origin", origin),
                ("Content-Type", "application/json"),
                ("A2A-Version", "1.0"),
            ],
            &body,
        )
    };

    let (code, hs, _) = preflight(UI);
    Outcome::require(
        code == 204
            && header(&hs, "access-control-allow-origin") == Some(UI)
            && header(&hs, "access-control-allow-headers")
                .is_some_and(|v| v.split(',').any(|h| h.trim() == "a2a-version")),
        format!("the listed origin's preflight should be granted with a2a-version: {code} {hs:?}"),
    )
    .and(|| {
        let (code, hs, reply) = post(UI);
        let exposed = header(&hs, "access-control-expose-headers").unwrap_or("");
        Outcome::require(
            code == 401
                && reply.contains("-31401")
                && header(&hs, "access-control-allow-origin") == Some(UI)
                && exposed.split(',').any(|h| h.trim() == "www-authenticate"),
            format!("a listed origin without a credential should get a readable 401 challenge: {code} {hs:?} {reply}"),
        )
    })
    .and(|| {
        for origin in ["http://127.0.0.1:9999", "https://evil.example", "null"] {
            for (what, (code, hs, _)) in [("preflight", preflight(origin)), ("POST", post(origin))] {
                if code != 403 || header(&hs, "access-control-allow-origin").is_some() {
                    return Outcome::fail(format!(
                        "an unlisted origin's {what} ({origin}) should be 403 with no grant: {code} {hs:?}"
                    ));
                }
            }
        }
        Outcome::pass()
    })
    .and(|| {
        let (code, hs, _) = exchange(
            addr,
            "GET",
            "/.well-known/agent-card.json",
            &[("Origin", "https://evil.example")],
            "",
        );
        Outcome::require(
            code == 200
                && header(&hs, "access-control-allow-origin") == Some("*")
                && header(&hs, "access-control-allow-credentials").is_none(),
            format!("the public card should be readable from any origin, without credentials: {code} {hs:?}"),
        )
    })
}
