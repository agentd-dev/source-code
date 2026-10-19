// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --login company:<name>` and `--logout company:<name>` (RFC 0047
//! §5.3): thin clients of the running daemon. Each sends the operator op
//! `company.sign_in` or `company.sign_out` over the daemon's unix A2A
//! listener — whose peer the kernel holds to the daemon's own uid, or root —
//! prints what the daemon says, and waits for the end. The sign-in itself
//! happens in the daemon, where every secret it involves stays: this
//! process never holds a device code, a verifier or a token.
//!
//! The unix-socket peer is the operator, so both are refused unless
//! `security.companies.user` is `operator`. Any other User who is an
//! operator principal sends the `company.sign_in` op from their own A2A
//! client as that principal, and any User can ask the agent, whose
//! `company.sign_in` tool opens the same gate.
//!
//! The User's account at the Company is named after a `/`:
//! `company:<name>/<label>` (RFC 0047 §9, P5b), and without one the op's
//! own default holds — the only account held, else `default`. The label
//! rides in the one target argument `--login` already takes, so no flag is
//! added that every other invocation would have to refuse; and `/` is in
//! neither an entry's name nor a label, so the target splits one way only.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::A2aEndpoint;
use crate::config::settings::{ServiceKind, Settings, companies::OPERATOR};
use crate::mcp::a2a_client::{self, PeerAuth};

/// Why a login or logout did not end as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// The configuration does not let the client reach the daemon for it.
    Usage(String),
    /// The daemon did not answer, or the sign-in or sign-out ended
    /// otherwise: what it said last.
    Ended(String),
}

/// How long the client follows a sign-in: longer than any device code lives.
const FOLLOW_FOR: Duration = Duration::from_secs(30 * 60);
/// How often it reads the task.
const POLL: Duration = Duration::from_millis(300);

/// The daemon's unix A2A listener, when `settings` let `what` reach the
/// Company `name` through it.
fn daemon(settings: &Settings, name: &str, what: &str) -> Result<A2aEndpoint, String> {
    let user = settings.security.companies.user();
    if user != OPERATOR {
        return Err(format!(
            "{what} company:{name} signs in the operator, and security.companies.user is {user}: they sign in by asking the agent (its company.sign_in tool), or, when they are an operator principal, by sending the company.sign_in op from their own A2A client"
        ));
    }
    if settings
        .services
        .get(name)
        .is_none_or(|s| s.kind != ServiceKind::Company)
    {
        return Err(format!(
            "{name:?} is not a company entry (services.<name>.kind: company)"
        ));
    }
    let listen = settings.a2a.listen.as_deref().unwrap_or_default();
    let Some(path) = crate::config::unix_socket_path(listen) else {
        return Err(format!(
            "{what} company:{name} reaches the running daemon over its unix A2A listener, and a2a.listen is {}: ask the agent to sign in (its company.sign_in tool), or send the company.sign_in op from the User's own A2A client",
            if listen.is_empty() { "not set" } else { listen }
        ));
    };
    Ok(A2aEndpoint::Https(format!("unix://{path}")))
}

/// Send the op `op` with `args`, and follow its task to its end, writing each
/// thing it says to `out`: the last thing it said, or why it failed.
fn follow(
    endpoint: &A2aEndpoint,
    op: &str,
    args: Value,
    out: &mut dyn Write,
) -> Result<String, String> {
    let deadline = Instant::now() + FOLLOW_FOR;
    let mut command = json!({"op": op});
    if let (Value::Object(c), Value::Object(a)) = (&mut command, args) {
        c.extend(a);
    }
    let id = crate::sec::random::b64url_token(16)
        .map_err(|e| format!("no OS randomness for a message id: {e}"))?;
    let params = crate::a2a::peer::send_message_params_cmd(
        "",
        Some(&command),
        None,
        &format!("login-{id}"),
        None,
        true,
    );
    let sent = a2a_client::call(
        endpoint,
        PeerAuth::default(),
        "SendMessage",
        params,
        deadline,
    )
    .map_err(|e| format!("no daemon answered on its A2A listener ({e})"))?;
    let mut task = sent["task"].clone();
    let id = task["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return Err(format!("the daemon answered no task: {sent}"));
    }
    let mut said = String::new();
    loop {
        let state = task["status"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let text = status_text(&task);
        if !text.is_empty() && text != said {
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
            said = text;
        }
        match state.as_str() {
            "TASK_STATE_COMPLETED" => return Ok(said),
            "TASK_STATE_FAILED" | "TASK_STATE_CANCELED" | "TASK_STATE_REJECTED" => {
                return Err(said);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(format!("{op} did not end within {}s", FOLLOW_FOR.as_secs()));
        }
        std::thread::sleep(POLL);
        task = a2a_client::call(
            endpoint,
            PeerAuth::default(),
            "GetTask",
            crate::a2a::peer::get_task_params(&id, None),
            deadline,
        )?;
    }
}

/// What a task's status message says.
fn status_text(task: &Value) -> String {
    task["status"]["message"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The company entry and the label of the User's account there that
/// `target` — what follows `company:` — names: `<name>` or
/// `<name>/<label>`, a label refused unless agentd would keep one by it.
pub fn target_of(target: &str) -> Result<(&str, Option<&str>), String> {
    match target.split_once('/') {
        None => Ok((target, None)),
        Some((name, label)) if super::account::is_label(label) => Ok((name, Some(label))),
        Some((_, label)) => Err(format!(
            "company:<name>/<label> names an account by a label of 1 to 32 of a-z, 0-9, _ and -, and {label:?} is not one"
        )),
    }
}

/// The op `op`'s arguments for `target`.
fn args_of(settings: &Settings, target: &str, what: &str) -> Result<(A2aEndpoint, Value), String> {
    let (name, label) = target_of(target)?;
    let endpoint = daemon(settings, name, what)?;
    let mut args = json!({"company": name});
    if let Some(l) = label {
        args["account"] = json!(l);
    }
    Ok((endpoint, args))
}

/// `--login company:<name>[/<label>]`: the daemon asks the User — the
/// operator — to sign in at `name`, to the account `label` names, and this
/// prints where and how, then how it ended.
pub fn login(settings: &Settings, target: &str, out: &mut dyn Write) -> Result<String, Failed> {
    let (endpoint, args) = args_of(settings, target, "--login").map_err(Failed::Usage)?;
    follow(&endpoint, "company.sign_in", args, out).map_err(Failed::Ended)
}

/// `--logout company:<name>[/<label>]`: the daemon revokes and forgets the
/// Account Token of the account `label` names at `name` — or the only one
/// held.
pub fn logout(settings: &Settings, target: &str, out: &mut dyn Write) -> Result<String, Failed> {
    let (endpoint, args) = args_of(settings, target, "--logout").map_err(Failed::Usage)?;
    follow(&endpoint, "company.sign_out", args, out).map_err(Failed::Ended)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(yaml: &str) -> Settings {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seal"), "ab".repeat(32)).unwrap();
        let path = dir.path().join("c.yaml");
        let yaml = yaml.replace("__DIR__", &dir.path().display().to_string());
        std::fs::write(&path, yaml).unwrap();
        crate::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .unwrap_or_else(|e| panic!("{e}"))
            .0
            .settings
    }

    const BASE: &str = "security:\n  companies:\n    client: {id: \"https://agent.example/c.json\", name: A, key_file: \"__DIR__/k\"}\n    seal_key: \"{{secret-file:__DIR__/seal}}\"\n__USER__store: {kind: file, file: {path: \"__DIR__/s\"}}\nservices:\n  shop: {kind: company, endpoint: \"https://example.com\", sign_in: [device], scopes: [\"poppy:read\"]}\n";

    /// The client reaches the daemon only over its unix listener, only for
    /// a declared Company, and only when the operator is the User.
    #[test]
    fn the_client_needs_a_unix_listener_and_the_operator_as_the_user() {
        let unix = settings(&format!(
            "{}a2a: {{listen: \"unix:///run/agentd/a2a.sock\"}}\n",
            BASE.replace("__USER__", "")
        ));
        let e = daemon(&unix, "shop", "--login").unwrap();
        assert_eq!(e, A2aEndpoint::Https("unix:///run/agentd/a2a.sock".into()));
        let e = daemon(&unix, "travel", "--login").unwrap_err();
        assert!(e.contains("is not a company entry"), "{e}");
        let tcp = settings(&format!(
            "{}a2a: {{listen: \"http://127.0.0.1:9\"}}\n",
            BASE.replace("__USER__", "")
        ));
        let e = daemon(&tcp, "shop", "--login").unwrap_err();
        assert!(e.contains("over its unix A2A listener"), "{e}");
        let dana = settings(&format!(
            "{}a2a: {{listen: \"unix:///run/agentd/a2a.sock\"}}\n",
            BASE.replace("__USER__", "    user: \"user:dana\"\n")
        ));
        let e = daemon(&dana, "shop", "--logout").unwrap_err();
        assert!(e.contains("security.companies.user is user:dana"), "{e}");
    }

    /// The target names the entry, and after a `/` the label of the User's
    /// account there, which rides to the daemon as the op's `account`.
    #[test]
    fn the_target_names_the_account_after_a_slash() {
        assert_eq!(target_of("shop"), Ok(("shop", None)));
        assert_eq!(target_of("shop/work"), Ok(("shop", Some("work"))));
        let e = target_of("shop/Work Account").unwrap_err();
        assert!(e.contains("\"Work Account\" is not one"), "{e}");
        assert!(target_of("shop/").is_err());
        let unix = settings(&format!(
            "{}a2a: {{listen: \"unix:///run/agentd/a2a.sock\"}}\n",
            BASE.replace("__USER__", "")
        ));
        let (_, args) = args_of(&unix, "shop/work", "--login").unwrap();
        assert_eq!(args, json!({"company": "shop", "account": "work"}));
        let (_, args) = args_of(&unix, "shop", "--logout").unwrap();
        assert_eq!(args, json!({"company": "shop"}));
        let e = args_of(&unix, "travel/work", "--login").unwrap_err();
        assert!(e.contains("\"travel\" is not a company entry"), "{e}");
    }
}
