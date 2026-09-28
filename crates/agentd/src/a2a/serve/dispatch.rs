// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC endpoint: the listener's pipeline, the few calls answered
//! here, and the filter that keeps what a2a-rs answers in the runtime's words.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::cors::{allow_origin, origin_allowed};
use super::feed::feed_stream;
use super::identity::{
    Peer, PeerId, challenge, evidence_of, forbidden, is_session_token, json_with, too_many,
};
use super::{App, LivenessCheck};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::ports;
use crate::a2a::principals::{Resolution, Via};
use crate::runtime::a2a_server::A2aBridge;
use crate::runtime::surface::{
    A2A_PROTOCOL_VERSION, EXTENSION_METHODS, INTERFACE_EXTENSION, Route, SpecMethod,
    accepts_version, route_of,
};

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

/// One POST, through the listener's pipeline.
///
/// The steps run in a fixed order, and each refusal is final and plain JSON
/// (no SSE response is ever written for a refusal):
///
/// 1. the browser origin; the source's failure limit (for a bearer);
/// 2. the content type — before a byte of the body means anything;
/// 3. who is calling, from the headers and the connection alone;
/// 4. the body parses as JSON;
/// 5. the JSON-RPC envelope, which must carry an `id`;
/// 6. the `A2A-Version`;
/// 7. the caller's rate;
/// 8. the method, from the route table, for every caller alike;
/// 9. the method's authorization (and the extended card's credential gate);
/// 10. the checks that need the params: a send's command op, a subscribe's
///     task;
/// 11. the answer — here for the few calls answered locally, else a2a-rs's,
///     filtered back to the runtime's own words on the way out.
///
/// The order is the point. Nothing that costs the runtime anything runs before
/// the caller is known and has asked in the protocol this listener speaks, and
/// an unknown method is the same `-32601` for an operator and a stranger,
/// before any authorization could make the two answers differ.
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

    // The binding is JSON-RPC over `application/json`. Refused with an empty
    // 415 before anything is read as JSON: a `text/plain` POST is what a
    // browser may send cross-origin without a preflight, so a body that only
    // happens to parse is not a request this endpoint accepts.
    if !is_json(&headers) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
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
        return err(Value::Null, errors::PARSE_ERROR, "invalid JSON");
    };
    let Envelope { id, method, params } = match envelope(&req) {
        Ok(e) => e,
        Err(refusal) => return json_response(refusal),
    };

    // The protocol version the caller speaks, before anything is done on its
    // behalf: a 0.3 client reads the same method names differently, so
    // answering it with 1.0 semantics would be answering a question it did
    // not ask.
    if let Err(refusal) = version_gate(&headers, &id) {
        return json_response(refusal);
    }

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

    // The route table, matched exactly, for every caller: an unknown name —
    // the card read as a method, an `a2a.` prefix, a 0.3 spelling — is the
    // same answer for an operator and a stranger, and it comes before the
    // method gate so authorization can never be what tells them apart.
    let Some(route) = route_of(&method) else {
        return err(
            id,
            errors::METHOD_NOT_FOUND,
            &format!("method not found: {method}"),
        );
    };
    let name = route.name();

    // The method gate. A refusal here counts against the source, which only
    // matters to later requests from it that ALSO fail to authenticate.
    let op = params
        .get("message")
        .and_then(crate::runtime::a2a_server::command_op);
    if !principal.may(name, None) {
        if let Some(ip) = source {
            app.failures.failed(ip);
        }
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal.id),
            rule,
            Some(name),
            op.as_deref(),
            "not_permitted",
            403,
        );
        return forbidden(
            id,
            &format!("{name} is not permitted for {}", principal.id),
            bearer_used,
        );
    }

    match route {
        // The spec makes the extended card an authenticated read (§13.3): on
        // a listener that declares a scheme, a caller the scheme did not name
        // — an `any` rule, the implicit operator — is asked for a credential
        // rather than handed it. The card itself is a2a-rs's to serve.
        Route::Spec(SpecMethod::GetExtendedAgentCard)
            if posture.declares_any() && !matches!(via, Via::Bearer | Via::Session | Via::Cert) =>
        {
            let rule = resolver.rule_of(&principal);
            denied(
                &app,
                unvouched,
                Some(&principal.id),
                rule,
                Some(name),
                None,
                "unauthenticated",
                401,
            );
            return challenge(false, false, false);
        }
        // Answered here because a2a-rs 0.10 cannot: its JSON-RPC adapter hands
        // the port the task id alone, dropping `pageSize` and `pageToken`, and
        // its response has no `nextPageToken`. Passed down, a caller asking
        // for two configs would get all of them and a page size of 500 would
        // be accepted. The caller's request is read with the spec's own type
        // and crosses whole to the runtime, which pages it and refuses what it
        // cannot honour — with the same codes the runtime gives every caller.
        Route::Spec(SpecMethod::ListTaskPushNotificationConfigs) => {
            let req = match serde_json::from_value::<
                a2a_rs::domain::generated::ListTaskPushNotificationConfigsRequest,
            >(params)
            {
                Ok(req) => req,
                Err(e) => return err(id, errors::INVALID_PARAMS, &format!("invalid params: {e}")),
            };
            let Ok(params) = serde_json::to_value(&req) else {
                return err(
                    id,
                    errors::INTERNAL_ERROR,
                    "could not re-encode the listing request",
                );
            };
            return unary(&app, id, "PushConfigList", params, principal, bearer_used).await;
        }
        // agentd's own method, which a2a-rs correctly does not know.
        Route::Extension { ext_method }
            if extension_of(ext_method) == Some(INTERFACE_EXTENSION) =>
        {
            return match &app.bridge.feed() {
                Some(feed) => {
                    feed_stream(Arc::clone(feed), id, params, principal, app.stream_deadline)
                }
                None => err(
                    id,
                    errors::UNSUPPORTED_OPERATION,
                    "the observation feed is disabled (set a2a.events.enabled: true)",
                ),
            };
        }
        // An extension method this build declares but serves no handler for
        // is not a method it answers.
        Route::Extension { ext_method } => {
            return err(
                id,
                errors::METHOD_NOT_FOUND,
                &format!("method not found: {ext_method}"),
            );
        }
        _ => {}
    }

    let send = matches!(
        route,
        Route::Spec(SpecMethod::SendMessage | SpecMethod::SendStreamingMessage)
    );

    // The command-op gate: the op's floor and the caller's grants, checked
    // before anything is created on the caller's behalf. Which workflow a
    // `workflow.run` may start is the runtime's to judge — it alone holds the
    // workflows and their start roles — and its refusal comes back as the same
    // 403, recorded by the audit mirror.
    if send
        && let Some(op) = &op
        && !principal.may_command(op)
    {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal.id),
            rule,
            Some(name),
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

    // A read op answers with a Message, not a Task, so there is nothing for
    // the protocol layer to track or frame — forcing it through a port that
    // must return a `Task` would mean inventing one. The runtime answers it
    // here, as one JSON body or, to a caller that asked for a stream, exactly
    // one frame. Every command that does work is a task like any other
    // message, and goes to a2a-rs below.
    if send
        && op
            .as_deref()
            .is_some_and(crate::runtime::surface::is_read_op)
    {
        let streamed = route == Route::Spec(SpecMethod::SendStreamingMessage);
        return message_reply(&app, id, params, principal, streamed, bearer_used).await;
    }

    // a2a-rs 0.10's subscribe reads the task first and, finding nothing,
    // opens a stream anyway — which a caller cannot tell from a task that has
    // yet to speak, and which for someone else's task would say nothing
    // either way. So the listener reads it first, as the caller: an unknown
    // id and a task the caller may not see get the runtime's "not found", as
    // JSON, and no stream is opened.
    if route == Route::Spec(SpecMethod::SubscribeToTask)
        && let Some(task) = params.get("id").and_then(Value::as_str)
    {
        let bridge = Arc::clone(&app.bridge);
        let who = principal.clone();
        let read = json!({"id": task, "historyLength": 0});
        let v = tokio::task::spawn_blocking(move || bridge.call("GetTask", read, who))
            .await
            .unwrap_or_else(
                |e| json!({"_error": {"code": errors::INTERNAL_ERROR, "message": e.to_string()}}),
            );
        if let Some(e) = ports::error_of(&v) {
            return error_response(json!({"jsonrpc": "2.0", "id": id, "error": e}), bearer_used);
        }
    }

    // A send with no task id yet gets one now. The protocol layer subscribes to
    // a task's updates *before* it processes the message — so that a fast
    // transition cannot be missed — and it can only do that if the id exists
    // first. Without this, a blocking send would never see the task settle and
    // a streaming send would be refused outright for want of an id.
    let body = if send {
        match normalize_send(&app.bridge, &req, &params).await {
            Some(rewritten) => Bytes::from(rewritten),
            None => body,
        }
    } else {
        body
    };

    // Everything else is the specification's, and a2a-rs answers it.
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
    let alive = app.liveness.as_ref().and_then(|l| l(&principal));
    let scope = ports::RequestScope::new(principal, via);
    let kept = Arc::clone(&scope.error);
    let protocol = app.protocol.clone();
    let answered = ports::with_request(scope, send, async move {
        protocol
            .oneshot(request)
            .await
            .unwrap_or_else(|_| err(Value::Null, errors::INTERNAL_ERROR, "dispatch failed"))
    });
    let resp = match &alive {
        Some(check) => match while_alive(answered, check).await {
            Some(resp) => resp,
            None => return revoked(),
        },
        None => answered.await,
    };
    let fidelity = Fidelity {
        kept: kept.lock().map(|k| k.clone()).unwrap_or_default(),
        bearer_used,
        list_tasks: route == Route::Spec(SpecMethod::ListTasks),
    };
    faithful(resp, fidelity, app.request_timeout, alive).await
}

/// Whether the request says its body is JSON: `application/json`, with any
/// parameters (`; charset=utf-8`), in any case.
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// A request that passed the envelope checks.
struct Envelope {
    /// A string or an integer — never null, never absent.
    id: Value,
    method: String,
    /// An object; `{}` when the request carried none.
    params: Value,
}

/// The JSON-RPC 2.0 envelope, or the whole error response refusing it.
///
/// An `id` is REQUIRED. JSON-RPC calls a request without one a notification,
/// which the server executes and never answers — for an A2A send that is a
/// task started for a caller who can never learn its id, so it is refused
/// instead, and so is `id: null`, which JSON-RPC reserves for answers to
/// requests whose id could not be read. The refusal's own id is echoed only
/// when the request carried a usable one.
fn envelope(req: &Value) -> Result<Envelope, Value> {
    let refuse = |id: &Value, code: i64, message: &str| {
        Err(errors::rpc_error(id.clone(), code, message, vec![]))
    };
    let Some(obj) = req.as_object() else {
        let message = if req.is_array() {
            "batch requests are not supported"
        } else {
            "a JSON-RPC request is an object"
        };
        return refuse(&Value::Null, errors::INVALID_REQUEST, message);
    };
    let id = match obj.get("id") {
        Some(id @ Value::String(_)) => id.clone(),
        Some(id @ Value::Number(n)) if n.is_i64() => id.clone(),
        _ => Value::Null,
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return refuse(&id, errors::INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }
    let method = match obj.get("method").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            return refuse(
                &id,
                errors::INVALID_REQUEST,
                "method must be a non-empty string",
            );
        }
    };
    if id.is_null() {
        return refuse(
            &Value::Null,
            errors::INVALID_REQUEST,
            "A2A requests must carry an id",
        );
    }
    let params = match obj.get("params") {
        None => json!({}),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => return refuse(&id, errors::INVALID_PARAMS, "params must be an object"),
    };
    Ok(Envelope { id, method, params })
}

/// The `A2A-Version` gate: `-32009` unless the header names a version this
/// listener speaks. Read from the header alone — never the query string, so a
/// URL a browser can be sent to cannot choose the protocol.
///
/// A missing header is refused too. The spec reads it as 0.3, and a 0.3 client
/// that happened to use a 1.0 method name would otherwise be answered in 1.0.
fn version_gate(headers: &HeaderMap, id: &Value) -> Result<(), Value> {
    let asked = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    if asked.is_some_and(accepts_version) {
        return Ok(());
    }
    let message = match asked {
        Some(v) if !v.is_empty() => {
            format!("A2A-Version {v} is not supported; this agent speaks {A2A_PROTOCOL_VERSION}")
        }
        _ => {
            format!("the A2A-Version header is required; this agent speaks {A2A_PROTOCOL_VERSION}")
        }
    };
    Err(errors::rpc_error(
        id.clone(),
        errors::VERSION_NOT_SUPPORTED,
        &message,
        vec![errors::error_info(
            errors::domain_of(reason::VERSION_NOT_SUPPORTED),
            reason::VERSION_NOT_SUPPORTED,
            &[("supportedVersions", A2A_PROTOCOL_VERSION)],
        )],
    ))
}

/// The extension that declares `method`, if any does.
fn extension_of(method: &str) -> Option<&'static str> {
    EXTENSION_METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, uri)| *uri)
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
    verb: &str,
    params: Value,
    principal: Principal,
    bearer_used: bool,
) -> Response {
    let envelope = round_trip(app, id, verb, params, principal).await;
    if envelope.get("error").is_some() {
        return error_response(envelope, bearer_used);
    }
    json_response(envelope)
}

/// The runtime's answer to `verb`, as a JSON-RPC envelope for `id`.
async fn round_trip(
    app: &Arc<App>,
    id: Value,
    verb: &str,
    params: Value,
    principal: Principal,
) -> Value {
    let bridge = Arc::clone(&app.bridge);
    let verb = verb.to_string();
    let v = tokio::task::spawn_blocking(move || bridge.call(&verb, params, principal))
        .await
        .unwrap_or_else(
            |e| json!({"_error": {"code": errors::INTERNAL_ERROR, "message": e.to_string()}}),
        );
    match ports::error_of(&v) {
        Some(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        None => json!({"jsonrpc": "2.0", "id": id, "result": v}),
    }
}

/// A read op's answer: its Message, as one JSON body — or, to a caller that
/// asked for a stream, as exactly one frame.
///
/// `SendStreamingMessage` promises a stream, and that promise does not depend
/// on what the message turned out to contain; a caller that asked for one and
/// received a JSON body would fail to parse it. The frame is the spec's
/// `StreamResponse{message}` under the request's own id, with no SSE `id:` —
/// there is no later event a reconnect could resume from — and the stream
/// closes after it.
///
/// A refusal is never a frame. A proxy, a browser and a plain HTTP client all
/// read a JSON error with its status; none of them reads an error inside an
/// SSE body the same way.
async fn message_reply(
    app: &Arc<App>,
    id: Value,
    params: Value,
    principal: Principal,
    streamed: bool,
    bearer_used: bool,
) -> Response {
    let envelope = round_trip(app, id, "SendMessage", params, principal).await;
    if envelope.get("error").is_some() {
        return error_response(envelope, bearer_used);
    }
    if !streamed {
        return json_response(envelope);
    }
    let frame = axum::response::sse::Event::default()
        .data(serde_json::to_string(&envelope).unwrap_or_default());
    axum::response::Sse::new(futures_util::stream::once(async move {
        Ok::<_, std::convert::Infallible>(frame)
    }))
    .into_response()
}

/// An error envelope as the response it travels in: HTTP 200 unless the code
/// is one the listener gives a status ([`errors::http_status_of`]), with the
/// challenge a 401 owes and, for a bearer caller refused a call, the RFC 6750
/// `insufficient_scope` that tells it the token is good but not enough.
fn error_response(envelope: Value, bearer_used: bool) -> Response {
    let code = envelope["error"]["code"].as_i64().unwrap_or_default();
    let status = StatusCode::from_u16(errors::http_status_of(code)).unwrap_or(StatusCode::OK);
    let mut resp = json_with(status, &envelope);
    let www = match code {
        errors::UNAUTHENTICATED => Some("Bearer realm=\"agentd\""),
        errors::PERMISSION_DENIED if bearer_used => {
            Some("Bearer realm=\"agentd\", error=\"insufficient_scope\"")
        }
        _ => None,
    };
    if let Some(www) = www {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static(www),
        );
    }
    resp
}

// ---- what a2a-rs answered, in the runtime's words ---------------------------

/// How long a revoked session's in-flight request may outlive it.
const LIVENESS_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// The most a stream's first event may take before the listener gives up
/// reading it. a2a-rs opens every stream with a task snapshot, so this is a
/// bound on a pathological task, not a limit a real one approaches.
const FIRST_EVENT_CAP: usize = 8 * 1024 * 1024;

/// What the filter needs to know about the request whose answer it filters.
pub(super) struct Fidelity {
    /// The runtime's own error object, when a port recorded one
    /// ([`ports::RequestScope::error`]).
    pub(super) kept: Option<Value>,
    pub(super) bearer_used: bool,
    /// The answer is a `ListTasks` result, whose empty fields a2a-rs drops.
    pub(super) list_tasks: bool,
}

impl Fidelity {
    /// Put the runtime's error in place of a2a-rs's rendering of it.
    ///
    /// When a port recorded the runtime's refusal, the error a2a-rs answered
    /// with IS that refusal — a port that fails returns at once — reworded:
    /// its message prefixed, its `data` replaced by a2a-rs's own `ErrorInfo`.
    /// The runtime's object goes back in whole. The codes must agree, which
    /// they always do for the error a port returned (every code crosses the
    /// typed error unchanged); an error a2a-rs raised on its own after a
    /// refusal it swallowed keeps its own answer rather than borrowing the
    /// swallowed one's.
    ///
    /// Otherwise the error is a2a-rs's own, and only the codes it invented
    /// outside the spec are folded ([`errors::normalize_native`]).
    fn restore(&self, error: &mut Value) {
        match &self.kept {
            Some(kept) if kept.get("code") == error.get("code") => *error = kept.clone(),
            _ => errors::normalize_native(error),
        }
    }

    /// A `ListTasks` page with the fields the spec's response always carries.
    /// a2a-rs writes ProtoJSON, which leaves out an empty string and an empty
    /// list — so the last page came back with no `nextPageToken` and an empty
    /// one with no `tasks`, and a client reading "no token" as "more to
    /// come" (or `tasks` as required) broke on exactly the page that ends the
    /// listing. Returns whether anything was added.
    fn complete_page(&self, envelope: &mut Value) -> bool {
        let Some(result) = envelope
            .get_mut("result")
            .and_then(Value::as_object_mut)
            .filter(|_| self.list_tasks)
        else {
            return false;
        };
        let mut changed = false;
        for (field, empty) in [("nextPageToken", json!("")), ("tasks", json!([]))] {
            if !result.contains_key(field) {
                result.insert(field.to_string(), empty);
                changed = true;
            }
        }
        changed
    }
}

/// a2a-rs's response, with every error in it the runtime's own.
///
/// A unary answer is read whole and its `error`, if any, restored — with the
/// HTTP status the restored code travels with. A stream's first event is read
/// before anything is sent: an error there becomes the plain JSON answer the
/// same refusal gets on a unary call (never an error frame on a 200), and
/// anything else is sent on unchanged, `id:` and all, followed by the rest of
/// the stream — whose error frames are restored in place.
pub(super) async fn faithful(
    resp: Response,
    fidelity: Fidelity,
    first_event_within: std::time::Duration,
    alive: Option<LivenessCheck>,
) -> Response {
    let streamed = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/event-stream"));
    let (parts, body) = resp.into_parts();
    if streamed {
        return faithful_stream(parts, body, fidelity, first_event_within, alive).await;
    }
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return err(
            Value::Null,
            errors::INTERNAL_ERROR,
            "the answer could not be read",
        );
    };
    let Ok(mut envelope) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some(error) = envelope.get_mut("error") {
        fidelity.restore(error);
        return error_response(envelope, fidelity.bearer_used);
    }
    if fidelity.complete_page(&mut envelope) {
        return json_with(parts.status, &envelope);
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// [`faithful`] for an SSE answer.
async fn faithful_stream(
    parts: axum::http::response::Parts,
    body: Body,
    fidelity: Fidelity,
    first_event_within: std::time::Duration,
    alive: Option<LivenessCheck>,
) -> Response {
    let mut data = body.into_data_stream();
    let mut read = Vec::new();
    // The first event that carries data. A keep-alive comment is not an
    // answer, so it is read past (and sent on) rather than taken for one.
    let first = tokio::time::timeout(first_event_within, async {
        loop {
            if let Some(event) = sse_events(&read).find(|e| !sse_data(e).is_empty()) {
                return Ok(Some(event.to_vec()));
            }
            if read.len() > FIRST_EVENT_CAP {
                return Err("the stream's first event is too large");
            }
            match data.next().await {
                Some(Ok(chunk)) => read.extend_from_slice(&chunk),
                // A stream that ended without a complete event: what arrived
                // is the last, unterminated one, if anything did.
                Some(Err(_)) | None => {
                    return Ok((!read.is_empty()).then(|| read.clone()));
                }
            }
        }
    })
    .await;
    let first = match first {
        Ok(Ok(first)) => first,
        Ok(Err(why)) => return err(Value::Null, errors::INTERNAL_ERROR, why),
        Err(_) => {
            return err(
                Value::Null,
                errors::INTERNAL_ERROR,
                "the stream did not start in time",
            );
        }
    };
    if let Some(first) = &first
        && let Ok(mut envelope) = serde_json::from_slice::<Value>(&sse_data(first))
        && let Some(error) = envelope.get_mut("error")
    {
        fidelity.restore(error);
        return error_response(envelope, fidelity.bearer_used);
    }
    let frames = Frames {
        inner: Box::pin(data),
        buf: read,
        fidelity,
        done: false,
        alive: alive.map(|check| (check, tokio::time::interval(LIVENESS_TICK))),
    };
    Response::from_parts(parts, Body::from_stream(frames))
}

/// The complete events at the front of an SSE buffer, each with its closing
/// blank line.
fn sse_events(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        let end = rest.windows(2).position(|w| w == b"\n\n")? + 2;
        let (event, tail) = rest.split_at(end);
        rest = tail;
        Some(event)
    })
}

/// An event's data: its `data:` lines, joined as the SSE rules join them.
fn sse_data(event: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for line in event.split(|b| *b == b'\n') {
        if let Some(value) = line.strip_prefix(b"data:") {
            if !out.is_empty() {
                out.push(b'\n');
            }
            out.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
        }
    }
    out
}

/// `event` with its error restored, or `None` when it is not an error frame
/// and goes out exactly as it came. Every line but the data — the `id:` a
/// reconnect resumes from, above all — is kept as it was.
fn restore_event(event: &[u8], fidelity: &Fidelity) -> Option<Vec<u8>> {
    let mut envelope = serde_json::from_slice::<Value>(&sse_data(event)).ok()?;
    fidelity.restore(envelope.get_mut("error")?);
    let data = serde_json::to_vec(&envelope).ok()?;
    let mut out = Vec::with_capacity(event.len());
    let mut wrote_data = false;
    for line in event.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if line.starts_with(b"data:") {
            if !wrote_data {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(&data);
                out.push(b'\n');
                wrote_data = true;
            }
        } else {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out.push(b'\n');
    Some(out)
}

/// The rest of a2a-rs's stream, event by event: error frames restored, every
/// other byte passed through, and the whole ended within a tick of the
/// caller's session dying.
struct Frames {
    inner: std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    /// Bytes read and not yet sent: the events already read past, then a
    /// partial one.
    buf: Vec<u8>,
    fidelity: Fidelity,
    done: bool,
    alive: Option<(LivenessCheck, tokio::time::Interval)>,
}

impl futures_util::Stream for Frames {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            // The tick is polled first and on every wake, so a quiet stream
            // — the common case for a revoked watcher — still ends on time.
            if let Some((check, tick)) = &mut this.alive {
                while tick.poll_tick(cx).is_ready() {
                    if !check() {
                        this.done = true;
                        return Poll::Ready(None);
                    }
                }
            }
            let next = sse_events(&this.buf).next().map(|event| {
                let out = restore_event(event, &this.fidelity).unwrap_or_else(|| event.to_vec());
                (event.len(), out)
            });
            if let Some((len, out)) = next {
                this.buf.drain(..len);
                return Poll::Ready(Some(Ok(Bytes::from(out))));
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => this.buf.extend_from_slice(&chunk),
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    this.done = true;
                    if !this.buf.is_empty() {
                        let rest = std::mem::take(&mut this.buf);
                        return Poll::Ready(Some(Ok(Bytes::from(rest))));
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// `answer`, unless the caller's session dies first — then `None`.
async fn while_alive<F>(answer: F, check: &LivenessCheck) -> Option<Response>
where
    F: std::future::Future<Output = Response>,
{
    tokio::pin!(answer);
    let mut tick = tokio::time::interval(LIVENESS_TICK);
    loop {
        tokio::select! {
            biased;
            resp = &mut answer => return Some(resp),
            _ = tick.tick() => {
                if !check() {
                    return None;
                }
            }
        }
    }
}

/// The answer to a request whose session was revoked while it waited: the
/// same 401 the revoked token now gets on any new request.
fn revoked() -> Response {
    challenge(true, true, false)
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
            let req = json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": params});
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
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {
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
