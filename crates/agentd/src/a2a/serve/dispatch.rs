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

use super::cors::{self, allow_origin};
use super::feed::feed_stream;
use super::identity::{
    Peer, PeerId, challenge, evidence_of, forbidden, is_session_token, json_with, too_many,
};
use super::{App, LivenessCheck};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::ports;
use crate::a2a::principals::{Resolution, Via};
use crate::runtime::surface::{
    A2A_PROTOCOL_VERSION, Active, Command, Ext, Reply, Route, SpecMethod, accepts_version,
    check_command, negotiate, owner_of, parse_extension_header, route_of,
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
    allow_origin(
        dispatch(app, peer_id, peer, headers, body).await,
        allowed.as_deref(),
    )
}

/// The extension handshake's answer: `resp` with the `A2A-Extensions` echo
/// naming what the request activated — set on the response head, so an SSE
/// answer carries it before its body streams.
///
/// Only an answer the handler produced is echoed, and only one sent `200`:
/// the spec's echo lists what was "successfully activated for that request",
/// and a refusal — whether the pipeline's or a runtime 401/403 — activated
/// nothing. An empty set is no header rather than an empty one.
fn echoed(mut resp: Response, active: Active) -> Response {
    if resp.status() == StatusCode::OK
        && let Some(v) = active
            .echo()
            .and_then(|e| axum::http::HeaderValue::from_str(&e).ok())
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
/// 9. the extensions: what the `A2A-Extensions` header activates, and the
///    refusal of an extension method whose extension it does not;
/// 10. the method's authorization (and the extended card's credential gate);
/// 11. the checks that need the params: a send's request shape, the task it
///     names, its command envelope and the caller's reach to the op, a
///     subscribe's task;
/// 12. the answer — here for the few calls answered locally, else a2a-rs's,
///     filtered back to the runtime's own words on the way out, and echoing
///     the extensions it was given under.
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
    if let Err(refused) = cors::gate(&headers, &app.origins()) {
        return refused.into_response();
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
    let sessions = app
        .auth
        .sessions
        .as_deref()
        .map(|s| s as &dyn crate::a2a::principals::SessionVerifier);
    // A source past its failure limit presents no more bearers: they are
    // refused before they are checked. Checking them — throttling only the
    // ones that turned out wrong — would leave a guesser's rate untouched
    // and hand it an oracle besides: a right guess answered 200 at full
    // speed, a wrong one 429. A bearer is the only thing that can be
    // guessed; a certificate is proven in the handshake, and a request that
    // presents nothing guesses nothing, so neither is refused here — the
    // implicit operator and an `any` rule are never locked out by a flood.
    //
    // A live session token is the exception. The launched console and every
    // signed-in device send one from this host, so refusing it unchecked
    // would let any local process, or a page on an admitted origin, keep
    // the operator's console at 429 with a trickle of junk bearers. It is
    // 256 bits agentd minted, not a guessable secret, so answering "valid,
    // or 429" tells a guesser nothing it could use.
    if let Some(ip) = source
        && let Some(token) = ev.bearer.as_deref()
        && let Some(retry) = app.failures.over(ip)
        && !live_session(token, sessions)
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
            Some(&principal),
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

    // The route table, matched exactly, for every caller: an unknown name is
    // the same answer for an operator and a stranger, and it comes before the
    // method gate so authorization can never be what tells them apart.
    let Some(route) = route_of(&method) else {
        return err(
            id,
            errors::METHOD_NOT_FOUND,
            &format!("method not found: {method}"),
        );
    };
    let name = route.name();

    // The extensions, negotiated against what this listener serves: the
    // header is the only way one is activated, and an extension method is
    // part of its extension — calling it without declaring and activating
    // the extension is refused like any unknown method, before authorization.
    let requested = parse_extension_header(
        headers
            .get_all("a2a-extensions")
            .iter()
            .map(|v| v.as_bytes()),
    );
    let active = match negotiate(&requested, &app.bridge.declared(), route) {
        Ok(active) => active,
        Err(e) => return json_response(json!({"jsonrpc": "2.0", "id": id, "error": e})),
    };

    // The method gate. A refusal here is NOT counted against the source: the
    // caller already proved who it is, so it guessed nothing, and a source
    // over the failure limit has every bearer refused unchecked — counting
    // an authenticated principal's refusals would let any narrowed caller
    // lock out every bearer sharing its address (loopback, a NAT, a proxy),
    // the operator's included.
    let op = params
        .get("message")
        .and_then(crate::runtime::a2a_server::command_op);
    if !principal.may(name, None) {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal),
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

    // The spec makes the extended card an authenticated read (§13.3): on a
    // listener that declares a scheme, a caller the scheme did not name — an
    // `any` rule, the implicit operator — is asked for a credential rather
    // than handed it. The card itself is a2a-rs's to serve.
    if route == Route::Spec(SpecMethod::GetExtendedAgentCard)
        && posture.declares_any()
        && !matches!(via, Via::Bearer | Via::Session | Via::Cert)
    {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal),
            rule,
            Some(name),
            None,
            "unauthenticated",
            401,
        );
        return challenge(false, false, false);
    }

    let send = matches!(
        route,
        Route::Spec(SpecMethod::SendMessage | SpecMethod::SendStreamingMessage)
    );

    // A send's params are the spec's `SendMessageRequest`, read with its own
    // type before anything is done on the caller's behalf. What the type does
    // not know is ignored, as ProtoJSON says it must be — a newer client's
    // field is not a malformed request — and the task the message names, if
    // any, is what the caller meant: continue that task. Only this layer sees
    // the message as the caller sent it (a2a-rs writes a generated id into an
    // empty `taskId` before any port runs), so it says so to the ports. A
    // blank id names nothing, by a2a-rs's own rule (`supplied`, which trims):
    // were the two layers to disagree, a whitespace id would be looked up as
    // the task a2a-rs generated in its place.
    let named_task = if send {
        match sent_message(&params) {
            Ok(message) => Some(message.task_id).filter(|t| !t.trim().is_empty()),
            Err(why) => return err(id, errors::INVALID_PARAMS, &why),
        }
    } else {
        None
    };
    // The command the send carries, held to the command extension before anything is
    // done with it: activated by the header and marked on the message, one
    // envelope, no task of its own choosing, an answer the caller accepts,
    // an op something serves and arguments that match its published schema.
    // A DataPart under `agentd` sent without the extension is refused rather
    // than read as data — the runtime would find the envelope and run it all
    // the same. The runtime asks the same function again, so a path around
    // this one is refused alike.
    let command = if send {
        match check_command(
            &params,
            named_task.as_deref(),
            active.contains(Ext::Command),
        ) {
            Ok(command) => command,
            Err(e) => {
                return error_response(
                    json!({"jsonrpc": "2.0", "id": id, "error": e}),
                    bearer_used,
                );
            }
        }
    } else {
        None
    };

    // The command-op gate: the op's floor and the caller's grants, checked
    // before anything is created on the caller's behalf. Which workflow a
    // `workflow.run` may start is the runtime's to judge — it alone holds the
    // workflows and their start roles — and its refusal comes back as the same
    // 403, recorded by the audit mirror.
    if let Some(op) = command.as_ref().map(|c| c.op.as_str())
        && !principal.may_command(op)
    {
        let rule = resolver.rule_of(&principal);
        denied(
            &app,
            unvouched,
            Some(&principal),
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
        let v = tokio::task::spawn_blocking(move || {
            bridge.call("GetTask", read, who, Active::NONE)
        })
        .await
        .unwrap_or_else(
            |e| json!({"_error": {"code": errors::INTERNAL_ERROR, "message": e.to_string()}}),
        );
        if let Some(e) = ports::error_of(&v) {
            return error_response(json!({"jsonrpc": "2.0", "id": id, "error": e}), bearer_used);
        }
    }

    // Every check has passed: the handler runs, and its answer — only its
    // answer — echoes what the request activated.
    let resp = answer(
        &app,
        route,
        Admitted {
            id,
            params,
            body,
            headers,
            principal,
            via,
            named_task,
            command,
            bearer_used,
            active,
        },
    )
    .await;
    echoed(resp, active)
}

/// A request every check has passed, as the answer needs it.
struct Admitted {
    id: Value,
    params: Value,
    body: Bytes,
    headers: HeaderMap,
    principal: Principal,
    via: Via,
    /// The task the send's message named (see `ports::RequestScope`).
    named_task: Option<String>,
    /// The command a send carries, held to the command extension.
    command: Option<Command>,
    bearer_used: bool,
    active: Active,
}

/// Pipeline step 12: the answer to `route`.
async fn answer(app: &Arc<App>, route: Route, req: Admitted) -> Response {
    let Admitted {
        id,
        params,
        body,
        headers,
        principal,
        via,
        named_task,
        command,
        bearer_used,
        active,
    } = req;
    let send = matches!(
        route,
        Route::Spec(SpecMethod::SendMessage | SpecMethod::SendStreamingMessage)
    );
    match route {
        // Answered here because a2a-rs 0.10 cannot: its JSON-RPC adapter
        // hands the port the task id alone, dropping `pageSize` and
        // `pageToken`, and its response has no `nextPageToken`. Passed
        // down, a caller asking for two configs would get all of them and
        // a page size of 500 would be accepted. The caller's request is
        // read with the spec's own type and crosses whole to the runtime,
        // which pages it and refuses what it cannot honour — with the same
        // codes the runtime gives every caller.
        Route::Spec(SpecMethod::ListTaskPushNotificationConfigs) => {
            let req = match serde_json::from_value::<
                a2a_rs::domain::generated::ListTaskPushNotificationConfigsRequest,
            >(params)
            {
                Ok(req) => req,
                Err(e) => {
                    return err(id, errors::INVALID_PARAMS, &format!("invalid params: {e}"));
                }
            };
            let Ok(params) = serde_json::to_value(&req) else {
                return err(
                    id,
                    errors::INTERNAL_ERROR,
                    "could not re-encode the listing request",
                );
            };
            return unary(app, id, "PushConfigList", params, principal, bearer_used).await;
        }
        // agentd's own method, which a2a-rs correctly does not know.
        // Negotiation has already refused it wherever the listener holds no
        // feed to serve it from. Served only as the events extension's own:
        // a method some other extension declares is not the feed, and until
        // it has a handler of its own it is a method nobody answers.
        Route::Extension { ext_method } if owner_of(ext_method) == Some(Ext::Events) => {
            let Some(feed) = app.bridge.feed() else {
                return err(
                    id,
                    errors::METHOD_NOT_FOUND,
                    &format!("method not found: {ext_method}"),
                );
            };
            let alive = app.liveness.as_ref().and_then(|l| l(&principal));
            return feed_stream(
                feed,
                id,
                params,
                principal,
                app.stream_deadline,
                alive,
                active,
            );
        }
        Route::Extension { ext_method } => {
            return err(
                id,
                errors::METHOD_NOT_FOUND,
                &format!("method not found: {ext_method}"),
            );
        }
        _ => {}
    }

    // A read op answers with a Message, not a Task, so there is nothing
    // for the protocol layer to track or frame — forcing it through a
    // port that must return a `Task` would mean inventing one. The
    // runtime answers it here, as one JSON body or, to a caller that
    // asked for a stream, exactly one frame. Every command that does work
    // is a task like any other message, and goes to a2a-rs below.
    if send
        && command
            .as_ref()
            .and_then(|c| c.spec)
            .is_some_and(|spec| spec.reply == Reply::Message)
    {
        let streamed = route == Route::Spec(SpecMethod::SendStreamingMessage);
        return message_reply(app, id, params, principal, streamed, bearer_used, active).await;
    }

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
    let mut scope = ports::RequestScope::new(principal, via, active);
    if send {
        scope = scope.send(named_task);
    }
    let kept = Arc::clone(&scope.error);
    let protocol = app.protocol.clone();
    let answered = ports::with_request(scope, async move {
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

/// Whether `token` is a session the listener issued and still holds. Anything
/// else — a configured bearer, junk, an expired or revoked session — is not.
fn live_session(
    token: &str,
    sessions: Option<&dyn crate::a2a::principals::SessionVerifier>,
) -> bool {
    token.starts_with(crate::a2a::principals::SESSION_TOKEN_PREFIX)
        && sessions.is_some_and(|s| {
            matches!(
                s.verify(token),
                crate::a2a::principals::SessionCheck::Valid(_)
            )
        })
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

/// The audit line for a refusal the listener made: who (when anybody), the
/// rule that named them, the session they signed in with (when they did),
/// what they asked for, why, and the status sent.
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
    principal: Option<&Principal>,
    rule: Option<&str>,
    method: Option<&str>,
    op: Option<&str>,
    reason: &'static str,
    status: u16,
) {
    let mut line = json!({"principal": principal.map(|p| &p.id), "rule": rule, "method": method, "op": op, "reason": reason, "status": status});
    // Several sessions share a principal id by design; the sid says which
    // of them was refused.
    if let Some(sid) = principal.and_then(|p| p.session.as_deref()) {
        line["sid"] = json!(sid);
    }
    if let Some(ip) = source {
        match app.denials.admit(ip, reason) {
            None => return,
            Some(0) => {}
            Some(n) => line["suppressed"] = json!(n),
        }
    }
    app.log.warn("a2a.denied", line);
}

/// A send's message, read as the spec's `SendMessageRequest`, or why it is
/// not one. The message is a user's: `ROLE_AGENT` is the agent's own voice,
/// and a caller does not get to speak in it.
fn sent_message(params: &Value) -> Result<a2a_rs::domain::Message, String> {
    let req =
        serde_json::from_value::<a2a_rs::domain::generated::SendMessageRequest>(params.clone())
            .map_err(|e| format!("invalid params: {e}"))?;
    let Some(message) = req.message.into_option() else {
        return Err("invalid params: a send carries a message".to_string());
    };
    if message.role != a2a_rs::domain::Role::ROLE_USER {
        return Err(format!(
            "invalid params: message.role must be ROLE_USER, not {}",
            message.role
        ));
    }
    Ok(message)
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
    // No extension applies to what is answered this way.
    let envelope = round_trip(app, id, verb, params, principal, Active::NONE).await;
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
    active: Active,
) -> Value {
    let bridge = Arc::clone(&app.bridge);
    let verb = verb.to_string();
    let v = tokio::task::spawn_blocking(move || bridge.call(&verb, params, principal, active))
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
    active: Active,
) -> Response {
    let envelope = round_trip(app, id, "SendMessage", params, principal, active).await;
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
    /// a2a-rs writes ProtoJSON, which leaves out an empty string, an empty
    /// list and a zero — so the last page came back with no `nextPageToken`
    /// and an empty one with no `tasks` and no `totalSize`, and a client
    /// reading "no token" as "more to come" (or any of the four as the
    /// REQUIRED fields A2A marks them) broke on exactly the page that ends
    /// the listing. Returns whether anything was added.
    fn complete_page(&self, envelope: &mut Value) -> bool {
        let Some(result) = envelope
            .get_mut("result")
            .and_then(Value::as_object_mut)
            .filter(|_| self.list_tasks)
        else {
            return false;
        };
        let mut changed = false;
        for (field, empty) in [
            ("nextPageToken", json!("")),
            ("tasks", json!([])),
            ("totalSize", json!(0)),
            ("pageSize", json!(0)),
        ] {
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
