// SPDX-License-Identifier: AGPL-3.0-only
//! The inbound **webhook** listener's auth gate, end to end.
//!
//! `webhooks.listen` and `a2a.listen` are the same kind of thing: an inbound
//! HTTP surface that TRIGGERS work. A2A refuses a non-loopback bind with no
//! client auth (exit 2), and the webhook surface must refuse it too: were a
//! `webhooks.listen: https://0.0.0.0:8099` with no auth allowed to start,
//! anyone who could reach the port could fire the agent's workflows. There is
//! no principled reason one listener refuses and the other shrugs; these tests
//! hold the line at the same place for both.
//!
//! The gate is deliberately about the ROUTE, not the listener: the runtime
//! resolves a webhook's verifier per node (`webhooks::build_verify` — the node's
//! own `auth`, else the listener `default_auth`), so a bind whose every node
//! signs is safe and must keep validating. The refusal fires exactly when a
//! reachable route would end up unverified.

mod common;

use std::path::Path;
use std::process::Command;

/// A fresh, empty directory per case — the loader also DISCOVERS `.agentd.yml`
/// from the working directory, and no case here may inherit the repository's own
/// dotfile.
fn workdir(tag: &str) -> String {
    let dir = common::unique_path(tag, "dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    // `https://` demands `webhooks.tls.{cert,key}`; validation only checks that
    // they are SET (dialing happens later), so empty files are enough to get the
    // auth check under test.
    for f in ["cert.pem", "key.pem"] {
        std::fs::write(Path::new(&dir).join(f), b"").expect("write tls stub");
    }
    dir
}

/// Load + validate one config and report `(exit code, stderr)`. `AGENTD_*` /
/// `AGENT_*` are scrubbed so the verdict comes from the file alone.
fn validate(dir: &str, yaml: &str) -> (i32, String) {
    let path = Path::new(dir).join("agentd.yaml");
    std::fs::write(&path, yaml).expect("write config");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.current_dir(dir)
        .args(["--config", &path.to_string_lossy(), "--validate-config"]);
    for (k, _) in std::env::vars() {
        if k.starts_with("AGENTD_") || k.starts_with("AGENT_") {
            cmd.env_remove(k);
        }
    }
    // The `{{secret:…}}` refs below must resolve, or the config fails for THAT
    // reason and the case proves nothing.
    cmd.env("HOOK_SECRET", "topsecret");
    let out = cmd.output().expect("spawn agentd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A webhook-serving config: `listen` verbatim, and one `webhook` start node
/// whose `auth` is spliced in verbatim (empty = declares none).
fn config(listen: &str, listener_auth: &str, node_auth: &str) -> String {
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: hook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: [\"https://intel.invalid/v1\"]\n  model: mock\n\
         store: {{kind: memory}}\n\
         webhooks:\n  listen: \"{listen}\"\n  tls: {{cert: cert.pem, key: key.pem}}\n{listener_auth}\
         workflows:\n  - name: on-hook\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/deploy, methods: [POST]{node_auth}}}\n\
         \x20     f: {{kind: finish, depends_on: [h]}}\n\
         lifecycle: {{run_until: drained}}\n"
    )
}

const HMAC: &str = ", auth: {hmac: {secret: \"{{secret:HOOK_SECRET}}\"}}";

/// (a) THE HOLE: a public bind with no auth anywhere — not on the listener, not
/// on the node — is exit 2, and the message names what to set. Anything softer
/// — a `config.warning` followed by `config.valid` and exit 0 — would start an
/// internet-reachable, unauthenticated workflow trigger.
#[test]
fn a_non_loopback_listener_with_no_auth_anywhere_is_refused_naming_what_to_set() {
    let dir = workdir("wh-gate-open");
    let (code, err) = validate(&dir, &config("https://0.0.0.0:8099", "", ""));
    assert_eq!(
        code, 2,
        "an open public webhook listener must exit 2: {err}"
    );
    assert!(err.contains("config.invalid"), "{err}");
    // Symmetric with the a2a refusal: name the surface, and name the knobs.
    assert!(
        err.contains("webhooks.listen on a non-loopback address needs auth"),
        "{err}"
    );
    assert!(err.contains("webhooks.default_auth"), "{err}");
    assert!(err.contains("`auth`"), "{err}");
    // And name the route that is actually open, so the operator can go fix it.
    assert!(err.contains("on-hook/h"), "{err}");
}

/// (b) A listener-wide `webhooks.default_auth` covers every node that declares
/// none — that is exactly what the runtime falls back to, so it satisfies the
/// gate.
#[test]
fn a_listener_default_auth_satisfies_the_gate() {
    let dir = workdir("wh-gate-default");
    let listener_auth = "  default_auth: {hmac: {secret: \"{{secret:HOOK_SECRET}}\"}}\n";
    let (code, err) = validate(&dir, &config("https://0.0.0.0:8099", listener_auth, ""));
    assert_eq!(code, 0, "webhooks.default_auth must validate: {err}");
    assert!(err.contains("config.valid"), "{err}");
}

/// (c) Per-node `auth` is the documented shape — auth is declared per node —
/// and it must be enough on its own: the check is about routes, not about
/// whether a listener-wide default happens to exist.
#[test]
fn every_node_carrying_its_own_auth_satisfies_the_gate() {
    let dir = workdir("wh-gate-node");
    let (code, err) = validate(&dir, &config("https://0.0.0.0:8099", "", HMAC));
    assert_eq!(code, 0, "a node's own auth must validate: {err}");
    assert!(err.contains("config.valid"), "{err}");
}

/// (d) THE CASE THAT MUST NOT REGRESS: loopback dev keeps working. An
/// unauthenticated `http://127.0.0.1` webhook is the documented dev shape (it is
/// what `webhook_e2e` runs), and nothing off-box can reach it.
#[test]
fn a_loopback_listener_with_no_auth_still_validates() {
    let dir = workdir("wh-gate-loopback");
    let (code, err) = validate(&dir, &config("http://127.0.0.1:8099", "", ""));
    assert_eq!(code, 0, "loopback dev must keep working: {err}");
    assert!(err.contains("config.valid"), "{err}");
}

/// `auth: {none: true}` is the *loopback-only* dev opt-out (that is what its
/// field documents, and what `build_verify` does with it: `Verify::None`). It is
/// the absence of authentication, so it cannot buy an open public bind — the
/// schema offers no way to declare one, and the gate does not invent one.
#[test]
fn an_explicit_none_auth_does_not_open_a_public_bind() {
    let dir = workdir("wh-gate-none");
    let (code, err) = validate(
        &dir,
        &config("https://0.0.0.0:8099", "", ", auth: {none: true}"),
    );
    assert_eq!(code, 2, "auth:{{none:true}} is not authentication: {err}");
    assert!(err.contains("on-hook/h"), "{err}");

    // Same for the listener-wide spelling — `default_auth` being *present* is not
    // the question; whether it verifies anybody is.
    let dir = workdir("wh-gate-none-default");
    let (code, err) = validate(
        &dir,
        &config("https://0.0.0.0:8099", "  default_auth: {none: true}\n", ""),
    );
    assert_eq!(code, 2, "default_auth:{{none:true}} is not auth: {err}");
    assert!(err.contains("on-hook/h"), "{err}");
}

/// A `wait: {on: webhook}` callback is the second inbound route shape: it arms a
/// path on the same listener, and its `auth` lives under `webhook.auth`. An
/// unauthenticated callback on a public bind is the same hole — resuming a
/// suspended run on a forged callback — so it is refused too.
#[test]
fn a_wait_on_webhook_callback_is_covered_by_the_gate() {
    let dir = workdir("wh-gate-wait");
    let yaml = "config_version: \"1\"\n\
         agent:\n  name: hook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: [\"https://intel.invalid/v1\"]\n  model: mock\n\
         store: {kind: memory}\n\
         webhooks:\n  listen: \"https://0.0.0.0:8099\"\n  tls: {cert: cert.pem, key: key.pem}\n\
         workflows:\n  - name: on-cb\n    steps:\n\
         \x20     s: {kind: once}\n\
         \x20     w: {kind: wait, on: webhook, webhook: {path: /hooks/cb}, depends_on: [s], timeout: 30s}\n\
         \x20     f: {kind: finish, depends_on: [w]}\n\
         lifecycle: {run_until: drained}\n";
    let (code, err) = validate(&dir, yaml);
    assert_eq!(code, 2, "an open callback route must exit 2: {err}");
    assert!(err.contains("on-cb/w"), "{err}");

    // …and signing the callback clears it.
    let signed = yaml.replace(
        "webhook: {path: /hooks/cb}",
        "webhook: {path: /hooks/cb, auth: {hmac: {secret: \"{{secret:HOOK_SECRET}}\"}}}",
    );
    let dir = workdir("wh-gate-wait-signed");
    let (code, err) = validate(&dir, &signed);
    assert_eq!(code, 0, "a signed callback must validate: {err}");
    assert!(err.contains("config.valid"), "{err}");
}
