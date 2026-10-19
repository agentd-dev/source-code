// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --login idp:<name>`, `--delegation-list`, `--delegation-approve
//! <gate>`, `--delegation-deny <gate>` and `--delegation-revoke <target>`
//! (RFC 0044 P1): thin clients of the running daemon. Each sends an operator
//! op over the daemon's unix A2A listener — whose peer the kernel holds to
//! the daemon's own uid, or root, and which names the per-person operator by
//! that uid — prints what the daemon says, and, for a sign-in, follows its
//! task to the end. The sign-in itself happens in the daemon, where every
//! secret it involves stays: this process never holds a device code, a
//! verifier or a token.
//!
//! An approval is bound to the gate it answers: the client reads the gate's
//! `nonce` and `args_hash` from `delegation.list` and sends them back, and
//! the daemon refuses an answer whose binding is not the gate's — so an
//! answer meant for one call can never approve another.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::A2aEndpoint;
use crate::config::settings::Settings;
use crate::config::settings::delegation as cfg;
use crate::mcp::a2a_client::{self, PeerAuth};

/// Why a command did not end as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// The configuration does not let the client reach the daemon for it.
    Usage(String),
    /// The daemon refused, or the sign-in ended otherwise.
    Ended(String),
}

/// How long a sign-in is followed.
const FOLLOW_FOR: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_millis(300);

/// The daemon's unix A2A listener.
fn daemon(settings: &Settings, what: &str) -> Result<A2aEndpoint, String> {
    if cfg::person(settings).is_none() {
        return Err(format!(
            "{what} acts for the per-person operator, and no a2a.principals rule with match.uid and idp_subject declares one"
        ));
    }
    let listen = settings.a2a.listen.as_deref().unwrap_or_default();
    let Some(path) = crate::config::unix_socket_path(listen) else {
        return Err(format!(
            "{what} reaches the running daemon over its unix A2A listener, and a2a.listen is {}",
            if listen.is_empty() { "not set" } else { listen }
        ));
    };
    Ok(A2aEndpoint::Https(format!("unix://{path}")))
}

/// Send `op` with `args`: the daemon's answer.
fn send(endpoint: &A2aEndpoint, op: &str, args: Value, deadline: Instant) -> Result<Value, String> {
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
        &format!("delegation-{id}"),
        None,
        true,
    );
    a2a_client::call(
        endpoint,
        PeerAuth::default(),
        "SendMessage",
        params,
        deadline,
    )
    .map_err(|e| format!("no daemon answered on its A2A listener ({e})"))
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

/// Follow the task `sent` answered with to its end, writing each thing it
/// says.
fn follow(endpoint: &A2aEndpoint, sent: Value, out: &mut dyn Write) -> Result<String, String> {
    let deadline = Instant::now() + FOLLOW_FOR;
    if let Some(err) = sent.get("_error").or_else(|| sent.get("error")) {
        return Err(err.to_string());
    }
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
            return Err(format!("did not end within {}s", FOLLOW_FOR.as_secs()));
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

/// The document a Message-reply op answered with.
fn doc_of(sent: &Value) -> Result<Value, String> {
    sent["message"]["parts"][0]["data"]
        .as_object()
        .map(|d| Value::Object(d.clone()))
        .ok_or_else(|| format!("the daemon answered no document: {sent}"))
}

/// `--login idp:<name>[/<grant>]`: the daemon signs the person in at the
/// IdP — by `device` or `authorization_code` after a `/`, else the
/// provider's first — and this prints where and how, then how it ended.
pub fn login(settings: &Settings, target: &str, out: &mut dyn Write) -> Result<String, Failed> {
    let (idp, grant) = match target.split_once('/') {
        Some((i, g)) => (i, Some(g)),
        None => (target, None),
    };
    if !settings.identity.providers.contains_key(idp) {
        return Err(Failed::Usage(format!(
            "{idp:?} is not an identity.providers entry"
        )));
    }
    if let Some(g) = grant
        && !matches!(g, "device" | "authorization_code")
    {
        return Err(Failed::Usage(format!(
            "idp:<name>/<grant> names device or authorization_code, not {g:?}"
        )));
    }
    let endpoint = daemon(settings, "--login idp:").map_err(Failed::Usage)?;
    let mut args = json!({"idp": idp});
    if let Some(g) = grant {
        args["grant"] = json!(g);
    }
    let sent = send(
        &endpoint,
        "delegation.enrol",
        args,
        Instant::now() + Duration::from_secs(30),
    )
    .map_err(Failed::Ended)?;
    follow(&endpoint, sent, out).map_err(Failed::Ended)
}

/// `--delegation-list`: the grants and the open consent gates.
pub fn list(settings: &Settings, out: &mut dyn Write) -> Result<String, Failed> {
    let endpoint = daemon(settings, "--delegation-list").map_err(Failed::Usage)?;
    let sent = send(
        &endpoint,
        "delegation.list",
        json!({}),
        Instant::now() + Duration::from_secs(30),
    )
    .map_err(Failed::Ended)?;
    let doc = doc_of(&sent).map_err(Failed::Ended)?;
    let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
    let _ = writeln!(out, "{text}");
    Ok(text)
}

/// `--delegation-approve <gate>` and `--delegation-deny <gate>`: the
/// person's answer to one consent gate, bound to its nonce and arguments.
pub fn answer(
    settings: &Settings,
    gate: &str,
    approve: bool,
    out: &mut dyn Write,
) -> Result<String, Failed> {
    let what = if approve {
        "--delegation-approve"
    } else {
        "--delegation-deny"
    };
    let endpoint = daemon(settings, what).map_err(Failed::Usage)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let listed = send(&endpoint, "delegation.list", json!({}), deadline).map_err(Failed::Ended)?;
    let doc = doc_of(&listed).map_err(Failed::Ended)?;
    let Some(g) = doc["gates"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| g["gate"].as_str() == Some(gate))
        .cloned()
    else {
        return Err(Failed::Ended(format!("no open consent gate is {gate:?}")));
    };
    let _ = writeln!(
        out,
        "{} {} on {} — arguments {}",
        if approve { "approving" } else { "denying" },
        g["rendered"]["tool"].as_str().unwrap_or_default(),
        g["rendered"]["server"].as_str().unwrap_or_default(),
        g["rendered"]["args"]
    );
    let op = if approve {
        "delegation.approve"
    } else {
        "delegation.deny"
    };
    let sent = send(
        &endpoint,
        op,
        json!({"gate": gate, "nonce": g["nonce"], "args_hash": g["args_hash"]}),
        deadline,
    )
    .map_err(Failed::Ended)?;
    follow(&endpoint, sent, out).map_err(Failed::Ended)
}

/// `--delegation-revoke <principal | idp:<name>#<sub>>`.
pub fn revoke(settings: &Settings, target: &str, out: &mut dyn Write) -> Result<String, Failed> {
    let endpoint = daemon(settings, "--delegation-revoke").map_err(Failed::Usage)?;
    let sent = send(
        &endpoint,
        "delegation.revoke",
        json!({"target": target}),
        Instant::now() + Duration::from_secs(60),
    )
    .map_err(Failed::Ended)?;
    follow(&endpoint, sent, out).map_err(Failed::Ended)
}
