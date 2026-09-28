// SPDX-License-Identifier: AGPL-3.0-only
//! **Everything agentd speaks beyond core A2A is reachable the way the spec
//! provides for**, proven against the real listener.
//!
//! Four claims, in the order a peer meets them:
//!
//! 1. the PUBLIC card declares each extension, with a versioned URI and the
//!    vocabulary agentd can answer — and nothing that depends on what this
//!    instance has loaded or switched on, or on who is asking; what a caller
//!    may actually run is on the extended card, narrowed to that caller;
//! 2. the card declares exactly the ways in the listener accepts, follows a
//!    reload of the principal rules, and offers no extended card where no
//!    scheme is declared;
//! 3. the `A2A-Extensions` handshake works — a client lists what it means to
//!    activate and the response echoes what actually was, which is the rule the
//!    spec states for the header;
//! 4. the operator admin family is reachable as an ordinary `SendMessage` with
//!    a command DataPart, so a client that has never heard of agentd can drain
//!    this instance — and a non-operator still cannot, whatever its grants.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use agentd::runtime::surface::{COMMAND_EXTENSION, static_vocabulary};
use common::{SendMessage, a2a_post, get_card, rpc_as, rpc_body};

const OPERATOR: &str = "ext-e2e-operator-token";
const USER: &str = "ext-e2e-user-token";
const CI: &str = "ext-e2e-ci-token";
const POD: &str = "ext-e2e-pod-7f9c";
const HOST: &str = "ext-e2e-host.internal";

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Daemon {
    child: Child,
    cfg: String,
    stderr_path: String,
}
impl Daemon {
    // Only the reload test reads the log; gated as it is, or the `a2a` row
    // without `hot-reload` fails -D warnings on a dead method.
    #[cfg(feature = "hot-reload")]
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cfg);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            std::thread::sleep(Duration::from_millis(200));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("listener never came up on {addr}");
}

/// The two named callers every card test reads as: an operator, and a user
/// holding every grant — which still makes it no operator.
const PRINCIPALS: &str = "\x20 principals:\n\
                          \x20   - id: ext-operator\n\
                          \x20     match: { bearer_ref: \"{{secret:AGENTD_EXT_OPERATOR}}\" }\n\
                          \x20     role: operator\n\
                          \x20   - id: ext-user\n\
                          \x20     match: { bearer_ref: \"{{secret:AGENTD_EXT_USER}}\" }\n\
                          \x20     role: user\n\
                          \x20     grants: [\"*\"]\n";

/// Three workflows: one anybody may run, one whose command only an operator
/// may fire, and one declaring a typed command.
const WORKFLOWS: &str = "workflows:\n\
     \x20 - name: greet\n\
     \x20   steps:\n\
     \x20     s: {kind: manual}\n\
     \x20     f: {kind: finish, depends_on: [s], output: \"done\"}\n\
     \x20 - name: ops-only\n\
     \x20   steps:\n\
     \x20     s: {kind: a2a, command: ops.go, roles: [operator]}\n\
     \x20     f: {kind: finish, depends_on: [s], output: \"done\"}\n\
     \x20 - name: report\n\
     \x20   steps:\n\
     \x20     s: {kind: a2a, command: report.make, schema: {type: object}}\n\
     \x20     f: {kind: finish, depends_on: [s], output: \"done\"}\n";

fn config(port: u16, a2a_extra: &str, top_extra: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: a2a-ext\n  instruction: You are a test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{a2a_extra}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n{top_extra}"
    )
}

/// A daemon on a free loopback port, `a2a_extra` under `a2a:` and
/// `top_extra` at the top level; returns it and the authority it serves.
fn boot_with(a2a_extra: &str, top_extra: &str) -> (Daemon, String) {
    spawn(|port| config(port, a2a_extra, top_extra))
}

/// A daemon on a free loopback port, configured by `text(port)`. It runs with
/// a pod name and a host name in its environment — what the instance name
/// falls back to, and what an unauthenticated card must never carry.
fn spawn(text: impl Fn(u16) -> String) -> (Daemon, String) {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-ext", "yaml");
    std::fs::write(&cfg, text(port)).unwrap();
    let stderr_path = common::unique_path("a2a-ext-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_EXT_OPERATOR", OPERATOR)
        .env("AGENTD_EXT_USER", USER)
        .env("AGENTD_EXT_CI", CI)
        .env("AGENT_POD_NAME", POD)
        .env("HOSTNAME", HOST)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn");
    wait_ready(&addr);
    (
        Daemon {
            child,
            cfg,
            stderr_path,
        },
        addr,
    )
}

fn boot() -> (Daemon, String) {
    boot_with(PRINCIPALS, WORKFLOWS)
}

/// The declaration of `uri` on `card`.
fn extension<'c>(card: &'c Value, uri: &str) -> &'c Value {
    card["capabilities"]["extensions"]
        .as_array()
        .and_then(|a| a.iter().find(|e| e["uri"] == uri))
        .unwrap_or_else(|| panic!("{uri} is not declared: {card}"))
}

fn ops(card: &Value) -> Vec<String> {
    extension(card, COMMAND_EXTENSION)["params"]["ops"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| o["op"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn skill_ids(card: &Value) -> Vec<String> {
    card["skills"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `GetExtendedAgentCard` as `bearer`'s principal; the card.
fn extended_as(addr: &str, bearer: &str) -> Value {
    let v = rpc_as(addr, bearer, 5, "GetExtendedAgentCard", json!({}));
    assert!(v.get("error").is_none(), "the extended card: {v}");
    v["result"].clone()
}

/// The public card is what anybody may know; the extended card is what THIS
/// caller may use.
///
/// The public card used to list every loaded workflow and every op this
/// instance served, as skills and as the extension's params — so an
/// anonymous reader learned the workflow inventory, whether introspection
/// was on and what the operator could do, while an authenticated user's
/// extended card told it LESS. Now the public card carries one conversation
/// skill and the static vocabulary, and each caller's extended card adds
/// what that caller may run.
#[test]
fn the_public_card_reveals_no_posture_and_the_extended_card_adds_per_caller() {
    let (_d, addr) = boot();
    let public = get_card(&addr);

    // Every declaration carries what the spec asks of an AgentExtension, and
    // none is `required` — a proto3 `bool`, so absent means false.
    for e in public["capabilities"]["extensions"].as_array().unwrap() {
        assert!(e["uri"].is_string(), "an extension needs a uri: {e}");
        assert!(
            e["description"].as_str().is_some_and(|d| !d.is_empty()),
            "an extension needs a description: {e}"
        );
        assert_ne!(
            e["required"],
            json!(true),
            "no extension may be required: {e}"
        );
    }
    let params = &extension(&public, COMMAND_EXTENSION)["params"];
    assert_eq!(params["dataPartKey"], "agentd", "{params}");
    assert_eq!(ops(&public), static_vocabulary(), "the static vocabulary");
    assert!(params.get("commands").is_none() && params.get("settable").is_none());

    assert_eq!(skill_ids(&public), vec!["conversation"], "{public}");
    let text = public.to_string();
    for loaded in ["greet", "ops-only", "report.make", "ops.go"] {
        assert!(
            !text.contains(loaded),
            "the public card names {loaded}: {public}"
        );
    }
    // The bearers this listener accepts are declared, and so is the card that
    // needs one.
    assert!(public["securitySchemes"]["bearer"].is_object(), "{public}");
    assert_eq!(
        public["capabilities"]["extendedAgentCard"], true,
        "{public}"
    );

    // The operator: every workflow, the admin family, every declared command,
    // and the settable paths.
    let op = extended_as(&addr, OPERATOR);
    assert_eq!(
        skill_ids(&op),
        vec![
            "conversation",
            "workflow:greet",
            "workflow:ops-only",
            "workflow:report"
        ]
    );
    let op_ops = ops(&op);
    for want in [
        "status",
        "workflow.run",
        "admin.drain",
        "admin.set",
        "config",
    ] {
        assert!(op_ops.iter().any(|o| o == want), "{want}: {op_ops:?}");
    }
    let op_params = &extension(&op, COMMAND_EXTENSION)["params"];
    let fired: Vec<&str> = op_params["commands"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["op"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(fired, vec!["ops.go", "report.make"], "{op_params}");
    assert!(op_params["settable"].is_array(), "{op_params}");

    // The user — every grant, still no operator: no operator-floor op, no
    // command only an operator may fire, no workflow whose start admits only
    // operators, and nothing to set.
    let user = extended_as(&addr, USER);
    assert_eq!(
        skill_ids(&user),
        vec!["conversation", "workflow:greet", "workflow:report"]
    );
    let user_ops = ops(&user);
    assert!(user_ops.iter().any(|o| o == "workflow.run"), "{user_ops:?}");
    for never in ["admin.drain", "admin.set", "config", "debug.events"] {
        assert!(
            !user_ops.iter().any(|o| o == never),
            "{never} offered to a user"
        );
    }
    let user_params = &extension(&user, COMMAND_EXTENSION)["params"];
    assert_eq!(
        user_params["commands"],
        json!([{"op": "report.make", "workflow": "report", "schema": {"type": "object"}}])
    );
    assert!(user_params.get("settable").is_none(), "{user_params}");

    // A command op is never a skill, on any card.
    for c in [&public, &op, &user] {
        for id in skill_ids(c) {
            assert!(
                !static_vocabulary().contains(&id.as_str()),
                "the op {id} is a skill"
            );
        }
    }
}

/// A listener that declares no scheme has no extended card: the public card
/// says so, and asking anyway is "unsupported operation" (A2A 1.0 §3.3.4) —
/// not the extended card handed to whoever the bind admits.
#[test]
fn no_scheme_means_no_extended_card() {
    // No `agent.name` either: the card then names the product, never the
    // instance — whose fallbacks, the pod and the host, are in this
    // daemon's environment.
    let (_d, addr) = spawn(|port| config(port, "", "").replace("  name: a2a-ext\n", ""));
    let card = get_card(&addr);
    assert_eq!(card["name"], "agentd", "{card}");
    for private in [POD, HOST] {
        assert!(!card.to_string().contains(private), "{private}: {card}");
    }
    assert_ne!(card["capabilities"]["extendedAgentCard"], true, "{card}");
    assert!(card.get("securitySchemes").is_none(), "{card}");
    assert!(card.get("securityRequirements").is_none(), "{card}");
    // The extended card is announced through the capability the A2A message
    // carries; the older card's flat `supportsAuthenticatedExtendedCard` is
    // in neither `AgentCard` nor `AgentCapabilities`.
    assert!(
        card.get("supportsAuthenticatedExtendedCard").is_none(),
        "{card}"
    );

    // The caller here is the implicit operator — admitted, and still told no.
    let reply = a2a_post(&addr, &rpc_body(1, "GetExtendedAgentCard", json!({})), &[]);
    assert_eq!(reply.status, 200, "{reply:?}");
    let v = reply.json();
    assert_eq!(v["error"]["code"], -32004, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("declares no authentication scheme")),
        "{v}"
    );
}

/// The card follows a reload of the principal rules.
///
/// It is built from the settings and the posture in force on every request,
/// so a SIGHUP that gives a no-auth loopback daemon its first bearer rule
/// makes the very next card declare the bearer and the extended card — and a
/// caller holding that bearer can read it. A card built from a posture taken
/// at spawn would keep saying "no scheme" while the listener asked for one.
#[test]
#[cfg(feature = "hot-reload")]
fn the_card_follows_a_principals_reload() {
    let (d, addr) = boot_with("", "");
    let well_known = "/.well-known/agent-card.json";
    let before = common::http_get(&addr, well_known);
    let card = before.json();
    assert!(card.get("securitySchemes").is_none(), "{card}");
    assert_ne!(card["capabilities"]["extendedAgentCard"], true, "{card}");

    let port = addr.rsplit_once(':').unwrap().1.parse().unwrap();
    let rule = "\x20 principals:\n\
                \x20   - id: ci\n\
                \x20     match: { bearer_ref: \"{{secret:AGENTD_EXT_CI}}\" }\n\
                \x20     role: user\n";
    std::fs::write(&d.cfg, config(port, rule, "")).unwrap();
    unsafe { libc::kill(d.child.id() as i32, libc::SIGHUP) };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = d.stderr();
        if log
            .lines()
            .any(|l| l.contains("\"event\":\"config.reloaded\"") && l.contains("a2a.principals"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never reloaded:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let after = common::http_get(&addr, well_known);
    let card = after.json();
    assert!(
        card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"] == "Bearer",
        "the reloaded rule's bearer is declared: {card}"
    );
    assert_eq!(card["capabilities"]["extendedAgentCard"], true, "{card}");
    assert_ne!(before.body, after.body, "the card did not change");
    // The well-known route's validator changes with the body it names.
    if let Some(etag) = before.header("etag") {
        assert_ne!(after.header("etag"), Some(etag), "the ETag did not change");
    }

    let ext = extended_as(&addr, CI);
    assert_eq!(ext["capabilities"]["extendedAgentCard"], true, "{ext}");
}

/// The `A2A-Extensions` handshake: the client asks, the response reports what
/// was actually activated. An unknown URI is simply not echoed.
#[test]
fn the_extension_header_is_echoed_with_what_was_activated() {
    let (_d, addr) = boot();
    // A plain read, so the only extension header on the request is the one
    // this test writes.
    let body = rpc_body(1, "ListTasks", json!({}));
    let auth = format!("Bearer {OPERATOR}");
    let reply = a2a_post(
        &addr,
        &body,
        &[
            ("Authorization", &auth),
            (
                "A2A-Extensions",
                "https://agentd.dev/a2a/ext/command/v1, https://example.invalid/nope/v1",
            ),
        ],
    );
    assert_eq!(
        reply.header("a2a-extensions"),
        Some("https://agentd.dev/a2a/ext/command/v1"),
        "the activated extension is echoed, and only it: {reply:?}"
    );

    // A request that activates nothing gets no header — silence, not an empty one.
    let reply = a2a_post(&addr, &body, &[("Authorization", &auth)]);
    assert_eq!(reply.header("a2a-extensions"), None, "{reply:?}");
}

/// The operator family, reached with a stock `SendMessage` — and refused for a
/// non-operator even with `grants: ["*"]`.
#[test]
fn admin_is_a_command_datapart_and_stays_operator_only() {
    let (_d, addr) = boot();

    // A `user` with a wildcard grant is still not an operator.
    let refused = SendMessage::command("admin.pause", json!({}))
        .bearer(USER)
        .post(&addr);
    assert!(
        refused["error"].is_object() || refused["result"]["status"]["state"] == "TASK_STATE_FAILED",
        "a non-operator must not pause the instance: {refused}"
    );

    // The operator can, through the same ordinary method.
    let paused = SendMessage::command("admin.pause", json!({"reason": "e2e"}))
        .bearer(OPERATOR)
        .post(&addr);
    let body = serde_json::to_string(&paused).unwrap();
    assert!(
        body.contains("paused"),
        "the admin op answers as a task: {body}"
    );

    let resumed = SendMessage::command("admin.resume", json!({}))
        .bearer(OPERATOR)
        .post(&addr);
    assert!(
        serde_json::to_string(&resumed).unwrap().contains("running"),
        "and resume brings it back: {resumed}"
    );
}
