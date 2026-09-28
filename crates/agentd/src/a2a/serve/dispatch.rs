// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC endpoint: agentd's own vocabulary answered here, everything
//! else handed to a2a-rs with the authenticated principal attached.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::App;
use super::cors::{allow_origin, origin_allowed};
use super::feed::feed_stream;
use super::identity::{
    Peer, PeerId, challenge, evidence_of, forbidden, is_session_token, json_with, too_many,
};
use crate::a2a::Principal;
use crate::a2a::errors;
use crate::a2a::ports;
use crate::a2a::principals::{Resolution, Via};
use crate::runtime::a2a_server::A2aBridge;

pub(super) async fn rpc(
    State(app): State<Arc<App>>,
    axum::Extension(peer_id): axum::Extension<PeerId>,
    axum::Extension(peer): axum::Extension<Peer>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let allowed = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // The extension handshake: a client lists the extensions it means to
    // activate, and the response says which of them actually were.
    let activated = activated_extensions(&headers);
    let resp = allow_origin(
        dispatch(app, peer_id, peer, headers, body).await,
        allowed.as_deref(),
    );
    with_activated_extensions(resp, &activated)
}

/// The `A2A-Extensions` request header, intersected with what this build can
/// activate. Unknown URIs are ignored rather than refused: the spec's rule is
/// that a client asks and the response reports what was granted, and none of
/// agentd's extensions is `required`, so a request naming only unknown ones is
/// still a perfectly good request.
fn activated_extensions(headers: &HeaderMap) -> Vec<String> {
    headers
        .get("a2a-extensions")
        .and_then(|v| v.to_str().ok())
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|u| crate::runtime::a2a_server::EXTENSIONS.contains(u))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Echo the activated set, as the spec asks a server to.
fn with_activated_extensions(mut resp: Response, activated: &[String]) -> Response {
    if !activated.is_empty()
        && let Ok(v) = axum::http::HeaderValue::from_str(&activated.join(", "))
    {
        resp.headers_mut().insert("a2a-extensions", v);
    }
    resp
}

async fn dispatch(
    app: Arc<App>,
    peer_id: PeerId,
    peer: Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // A browser page on an unexpected origin must not be able to drive this
    // endpoint through a victim's browser (DNS rebinding). Checked before the
    // body is even parsed, so an unauthorised origin reaches no dispatch logic
    // — and never counted as a failure: it guesses nothing, and counting it
    // would let any web page lock the operator's own console out.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(o) = &origin
        && !origin_allowed(o, &app.origins())
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    // Authentication, from the headers and the connection alone: a caller
    // who is nobody gets no byte of its body parsed. ONE resolver snapshot
    // answers both who this is and what the listener's posture is, so a
    // reload landing mid-request can never pair new rules with an old
    // posture.
    let resolver = app.bridge.resolver();
    let posture = resolver.posture();
    let ev = evidence_of(&headers, &peer_id, &peer);
    let source = peer.source();
    // A source past its failure limit presents no more bearers: they are
    // refused before they are checked. Checking them — throttling only the
    // ones that turned out wrong — would leave a guesser's rate untouched
    // and hand it an oracle besides: a right guess answered 200 at full
    // speed, a wrong one 429. A bearer is the only thing that can be
    // guessed; a certificate is proven in the handshake, and a request that
    // presents nothing guesses nothing, so neither is refused here — the
    // implicit operator and an `any` rule are never locked out by a flood.
    if let Some(ip) = source
        && ev.bearer.is_some()
        && let Some(retry) = app.failures.over(ip)
    {
        denied(
            &app,
            source,
            None,
            None,
            None,
            None,
            "auth_failures_limited",
            429,
        );
        return too_many(
            Value::Null,
            &format!("too many failed authentications from this source: retry in about {retry}s"),
            retry,
        );
    }
    let sessions = app
        .auth
        .sessions
        .as_deref()
        .map(|s| s as &dyn crate::a2a::principals::SessionVerifier);
    let (principal, via) = match resolver.resolve(&ev, peer.is_unix(), sessions) {
        Resolution::Named(p, via) => (p, via),
        // Nothing presented: a challenge, never counted. An uncredentialed
        // browser lands here, and so does every sign-in's first request.
        Resolution::Unauthenticated { presented: false } => {
            // Refused only for being a browser: the same request without
            // `Origin` would have been the implicit operator.
            let browser = ev.origin && ev.local && posture.implicit_operator;
            denied(&app, source, None, None, None, None, "unauthenticated", 401);
            return challenge(false, false, browser);
        }
        // A credential that failed: counted against its source, whose next
        // bearer, once it is over the limit, is refused unchecked (above).
        refused @ (Resolution::Unauthenticated { presented: true } | Resolution::NoRole) => {
            if let Some(ip) = source {
                app.failures.failed(ip);
            }
            return if refused == Resolution::NoRole {
                denied(&app, source, None, None, None, None, "no_role", 403);
                forbidden(Value::Null, "no role for this identity", false)
            } else {
                denied(
                    &app,
                    source,
                    None,
                    None,
                    None,
                    None,
                    "invalid_credential",
                    401,
                );
                challenge(true, is_session_token(&ev), false)
            };
        }
    };
    // The response carries this when the credential was a bearer, so a
    // refused caller learns its token lacks the scope rather than that it is
    // bad.
    let bearer_used = matches!(via, Via::Bearer | Via::Session);
    // Whose refusals are free: a principal no credential named — an `any`
    // rule's — can be anyone, so its refusals are logged like a stranger's.
    let unvouched = source.filter(|_| !matches!(via, Via::Bearer | Via::Session | Via::Cert));

    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return err(Value::Null, -32700, "invalid JSON");
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Admission: one token per request from a principal whose rule declares a
    // rate. A refusal here is not a result — nothing reaches the runtime, so
    // no task is created and nothing is charged.
    if let Err(retry) = app.rates.admit(&principal) {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            source,
            Some(&principal.id),
            rule,
            Some(&method),
            None,
            "rate_limited",
            429,
        );
        return too_many(
            id,
            &format!("rate limit for {}: retry in about {retry}s", principal.id),
            retry,
        );
    }

    // agentd's own vocabulary, which the protocol layer does not know.
    let bare = method.strip_prefix("a2a.").unwrap_or(&method).to_string();
    // Discovery. The spec's JSON-RPC binding has no method for the *public*
    // card — it is fetched from `.well-known` — but every agentd client asks
    // for it here, so both ways work.
    if matches!(bare.as_str(), "GetAgentCard" | "agent/card") {
        return unary(&app, id, "GetAgentCard", json!({}), principal).await;
    }

    // The method gate. A refusal here counts against the source, which only
    // matters to later requests from it that ALSO fail to authenticate.
    let op = params
        .get("message")
        .and_then(crate::runtime::a2a_server::command_op);
    if !principal.may(&bare, None) {
        if let Some(ip) = source {
            app.failures.failed(ip);
        }
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal.id),
            rule,
            Some(&bare),
            op.as_deref(),
            "not_permitted",
            403,
        );
        return forbidden(
            id,
            &format!("{bare} is not permitted for {}", principal.id),
            bearer_used,
        );
    }

    match bare.as_str() {
        // Served here rather than passed down for the same reason as the public
        // card: a round trip through the SDK's typed `AgentCard` drops any field
        // it has no place for. Unlike the public card this one is SCOPED — the
        // skills are the ones this caller may actually run — so the two are
        // deliberately not the same document. The spec makes it an
        // authenticated read (§13.3): on a listener that declares a scheme, a
        // caller the scheme did not name — an `any` rule, the implicit
        // operator — is asked for a credential rather than handed it.
        "GetExtendedAgentCard" | "agent/getAuthenticatedExtendedCard" => {
            if posture.declares_any() && !matches!(via, Via::Bearer | Via::Session | Via::Cert) {
                let rule = resolver.rule_of(&principal);
                denied(
                    &app,
                    unvouched,
                    Some(&principal.id),
                    rule,
                    Some(&bare),
                    None,
                    "unauthenticated",
                    401,
                );
                return challenge(false, false, false);
            }
            return unary(&app, id, "GetExtendedAgentCard", json!({}), principal).await;
        }
        // Answered here because a2a-rs 0.10 cannot: its JSON-RPC adapter hands
        // the port the task id alone, dropping `pageSize` and `pageToken`, and
        // its response has no `nextPageToken`. Passed down, a caller asking
        // for two configs would get all of them and a page size of 500 would
        // be accepted. The caller's request is read with the spec's own type
        // and crosses whole to the runtime, which pages it and refuses what it
        // cannot honour — with the same codes the runtime gives every caller.
        "ListTaskPushNotificationConfigs" => {
            let req = match serde_json::from_value::<
                a2a_rs::domain::generated::ListTaskPushNotificationConfigsRequest,
            >(params)
            {
                Ok(req) => req,
                Err(e) => return err(id, -32602, &format!("invalid params: {e}")),
            };
            let Ok(params) = serde_json::to_value(&req) else {
                return err(id, -32603, "could not re-encode the listing request");
            };
            return unary(&app, id, "PushConfigList", params, principal).await;
        }
        "SubscribeToEvents" => {
            return match &app.bridge.feed() {
                Some(feed) => {
                    feed_stream(Arc::clone(feed), id, params, principal, app.stream_deadline)
                }
                None => err(
                    id,
                    -32004,
                    "the observation feed is disabled (set a2a.events.enabled: true)",
                ),
            };
        }
        _ => {}
    }

    // The command-op gate: the op's floor and the caller's grants, checked
    // before anything is created on the caller's behalf. Which workflow a
    // `workflow.run` may start is the runtime's to judge — it alone holds the
    // workflows and their start roles — and its refusal comes back as the same
    // 403, recorded by the audit mirror.
    if matches!(bare.as_str(), "SendMessage" | "SendStreamingMessage")
        && let Some(op) = &op
        && !principal.may_command(op)
    {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal.id),
            rule,
            Some(&bare),
            Some(op),
            "not_permitted",
            403,
        );
        return forbidden(
            id,
            &format!("{op} is not permitted for {}", principal.id),
            bearer_used,
        );
    }

    // A command DataPart is agentd's own vocabulary riding the spec's data
    // part, not an A2A concept: some commands are plain reads that answer
    // without creating a task at all. Forcing those through a port that must
    // return a `Task` would mean inventing one. So they go straight to the
    // runtime, and its answer — task or not — is returned as it stands.
    if matches!(bare.as_str(), "SendMessage" | "SendStreamingMessage") && op.is_some() {
        let streaming = bare == "SendStreamingMessage";
        return unary_maybe_streamed(
            &app,
            id,
            "SendMessage",
            params,
            principal,
            streaming,
            bearer_used,
        )
        .await;
    }

    // A send with no task id yet gets one now. The protocol layer subscribes to
    // a task's updates *before* it processes the message — so that a fast
    // transition cannot be missed — and it can only do that if the id exists
    // first. Without this, a blocking send would never see the task settle and
    // a streaming send would be refused outright for want of an id.
    let body = match bare.as_str() {
        "SendMessage" | "SendStreamingMessage" => {
            match normalize_send(&app.bridge, &req, &params).await {
                Some(rewritten) => Bytes::from(rewritten),
                None => body,
            }
        }
        _ => body,
    };

    // Everything else is the specification's, and a2a-rs answers it — including
    // the methods it implements and agentd does not, which is why an
    // unsupported one comes back with the spec's code rather than ours.
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from(body))
        .expect("build request");
    *request.headers_mut() = headers;
    request
        .extensions_mut()
        .insert(a2a_rs::port::AuthPrincipal::new(
            principal.id.clone(),
            "agentd".to_string(),
        ));
    let protocol = app.protocol.clone();
    let in_send = matches!(bare.as_str(), "SendMessage" | "SendStreamingMessage");
    ports::with_caller(principal, in_send, async move {
        protocol
            .oneshot(request)
            .await
            .unwrap_or_else(|_| err(Value::Null, -32603, "dispatch failed"))
    })
    .await
}

/// The audit line for a refusal the listener made: who (when anybody), the
/// rule that named them, what they asked for, why, and the status sent.
///
/// `source` is given for the refusals any caller can provoke for free — no
/// credential, a bad one, a source over its limit, a rate already spent, a
/// gate refusing a principal only an `any` rule named — and those go through
/// the [`super::limits::DenialLog`]: one line per source and reason per window, which
/// says how many it stands for. A refusal of a principal a credential named
/// is written every time (`source` is `None`): it is attributable, and its
/// requests are that principal's to account for.
#[allow(clippy::too_many_arguments)]
fn denied(
    app: &App,
    source: Option<std::net::IpAddr>,
    principal: Option<&str>,
    rule: Option<&str>,
    method: Option<&str>,
    op: Option<&str>,
    reason: &'static str,
    status: u16,
) {
    let mut line = json!({"principal": principal, "rule": rule, "method": method, "op": op, "reason": reason, "status": status});
    if let Some(ip) = source {
        match app.denials.admit(ip, reason) {
            None => return,
            Some(0) => {}
            Some(n) => line["suppressed"] = json!(n),
        }
    }
    app.log.warn("a2a.denied", line);
}

/// Prepare a send for the protocol layer, returning a rewritten request body —
/// or `None` when nothing needed changing.
///
/// Two adjustments:
///
/// * **The task id.** A send that names no task is given one here, because the
///   protocol layer subscribes to a task's updates *before* it processes the
///   message — so a fast transition cannot be missed — and it can only do that
///   if the id exists first.
/// * **`blocking` → `returnImmediately`.** agentd's own clients ask not to wait
///   with `configuration.blocking: false`; the spec spells the same thing
///   `returnImmediately: true`. Translating here lets those clients keep their
///   spelling against a server that speaks only the specification's field.
///
/// Both rewrites write *into* `params`, which is whatever a remote caller put on
/// the wire. Neither is attempted unless the params carry the shape the spec
/// requires — an object with an object `message` — because the only way to write
/// into a `Value` is through a path of objects, and serde_json's `IndexMut`
/// *panics* rather than declining when the value under the path is a string, a
/// number or an array (`params: []`, `params: {"message": "hi"}`). The release
/// profile is `panic = "abort"`, so one malformed request would take the whole
/// daemon down. A shape that cannot be rewritten is passed through untouched
/// instead, and a2a-rs refuses it with the spec's -32602.
async fn normalize_send(bridge: &Arc<A2aBridge>, req: &Value, params: &Value) -> Option<Vec<u8>> {
    if !params.is_object() || !params.get("message").is_some_and(Value::is_object) {
        return None;
    }

    let mut req = req.clone();
    let mut changed = false;

    if params["message"]["taskId"]
        .as_str()
        .unwrap_or("")
        .is_empty()
    {
        let bridge = Arc::clone(bridge);
        if let Ok(v) = tokio::task::spawn_blocking(move || {
            bridge.call("NewTaskId", json!({}), Principal::anonymous())
        })
        .await
            && let Some(id) = v.get("id").and_then(Value::as_str)
            && let Some(message) = param_object(&mut req, "message")
        {
            message.insert("taskId".to_string(), json!(id));
            changed = true;
        }
    }

    if let Some(blocking) = params["configuration"]["blocking"].as_bool()
        && params["configuration"]["returnImmediately"].is_null()
        && let Some(config) = param_object(&mut req, "configuration")
    {
        config.insert("returnImmediately".to_string(), json!(!blocking));
        changed = true;
    }

    changed.then(|| serde_json::to_vec(&req).ok()).flatten()
}

/// `req.params.<field>` as a map to write into, or `None` when anything along
/// that path is not an object. Every rewrite goes through here rather than
/// through `IndexMut`, whose failure mode on a caller-controlled shape is a
/// panic in the listener rather than a request that gets refused.
fn param_object<'a>(
    req: &'a mut Value,
    field: &str,
) -> Option<&'a mut serde_json::Map<String, Value>> {
    req.as_object_mut()?
        .get_mut("params")?
        .as_object_mut()?
        .get_mut(field)?
        .as_object_mut()
}

/// One reactor round trip, answered as a JSON-RPC envelope.
async fn unary(
    app: &Arc<App>,
    id: Value,
    method: &str,
    params: Value,
    principal: Principal,
) -> Response {
    unary_maybe_streamed(app, id, method, params, principal, false, false).await
}

/// [`unary`], but able to answer as a one-frame SSE stream.
///
/// `SendStreamingMessage` promises a stream, and that promise does not depend on
/// what the message turned out to contain. A command DataPart is answered by the
/// runtime in one step, so there is exactly one frame to send — but a caller
/// that asked for a stream and received a JSON body would fail to parse it,
/// which is a worse answer than a short stream.
///
/// The exception is a refusal of the caller itself — the runtime's second lock
/// on a command (-31403), or an unauthenticated answer (-31401). Those travel
/// with their HTTP status as plain JSON, never as a frame: a proxy, a browser
/// and a plain HTTP client all read a 403 as a 403, and none of them reads the
/// status of an SSE frame.
async fn unary_maybe_streamed(
    app: &Arc<App>,
    id: Value,
    method: &str,
    params: Value,
    principal: Principal,
    streamed: bool,
    bearer_used: bool,
) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let method = method.to_string();
    let v = tokio::task::spawn_blocking(move || bridge.call(&method, params, principal))
        .await
        .unwrap_or_else(|e| json!({"_error": {"code": -32603, "message": e.to_string()}}));
    let envelope = match v.get("_error") {
        Some(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        None => json!({"jsonrpc": "2.0", "id": id, "result": v}),
    };
    let code = envelope["error"]["code"].as_i64();
    if let Some(code) = code
        && let Ok(status) = StatusCode::from_u16(errors::http_status_of(code))
        && status != StatusCode::OK
    {
        let mut resp = json_with(status, &envelope);
        if code == errors::PERMISSION_DENIED && bearer_used {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static(
                    "Bearer realm=\"agentd\", error=\"insufficient_scope\"",
                ),
            );
        }
        return resp;
    }
    if !streamed {
        return json_response(envelope);
    }
    let frame = axum::response::sse::Event::default()
        .id("1")
        .data(serde_json::to_string(&envelope).unwrap_or_default());
    axum::response::Sse::new(futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(frame)
    }))
    .into_response()
}

fn json_response(v: Value) -> Response {
    json_with(StatusCode::OK, &v)
}

fn err(id: Value, code: i64, message: &str) -> Response {
    json_response(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge with a stand-in for the reactor: it answers `NewTaskId` with an
    /// id, because a bridge whose loop is missing fails fast and would leave
    /// the rewrite unreached — making these tests pass without ever exercising
    /// the code path they exist to guard.
    fn stub_bridge() -> Arc<A2aBridge> {
        let resolver =
            crate::a2a::Resolver::build(&crate::config::v2::A2a::default(), &|_| None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(crate::runtime::events::Event::A2a(req)) = rx.recv() {
                let _ = req.reply.send(json!({"id": "task-stub"}));
            }
        });
        A2aBridge::new(tx, resolver)
    }

    /// Params are remote input, and a send whose params are not the shape the
    /// spec requires must never reach serde_json's `IndexMut`, which panics the
    /// listener — under the release profile's `panic = "abort"` that is a dead
    /// daemon from one curl. Every one of these must come back "nothing to
    /// rewrite" so the body travels on and a2a-rs answers it with -32602.
    #[tokio::test]
    async fn malformed_send_params_are_left_alone_rather_than_panicking() {
        let bridge = stub_bridge();
        for params in [
            json!([]),
            json!({"message": "hi"}),
            json!({"message": 3}),
            json!({"message": []}),
            Value::Null,
            json!("send"),
            json!({}),
        ] {
            let req =
                json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": params});
            // Exactly how `dispatch` derives the params it passes in.
            let p = req.get("params").cloned().unwrap_or_else(|| json!({}));
            assert_eq!(
                normalize_send(&bridge, &req, &p).await,
                None,
                "params {p} must not be rewritten"
            );
        }
    }

    /// The other half of the guard: a well-formed send must still be normalised
    /// — both rewrites — because refusing every shape would "fix" the panic by
    /// breaking the send path the protocol layer depends on.
    #[tokio::test]
    async fn a_well_formed_send_is_still_normalised() {
        let bridge = stub_bridge();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": {
            "message": {"messageId": "m1", "role": "user", "parts": [{"kind": "text", "text": "hi"}]},
            "configuration": {"blocking": false},
        }});
        let params = req["params"].clone();
        let out = normalize_send(&bridge, &req, &params)
            .await
            .expect("a well-formed send is rewritten");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["params"]["message"]["taskId"], json!("task-stub"));
        assert_eq!(
            v["params"]["configuration"]["returnImmediately"],
            json!(true)
        );
    }
}
