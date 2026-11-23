// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC endpoint: the host's gates in front of the SDK's router.
//!
//! The protocol is the SDK's: `a2a-server-lf`'s router parses every request,
//! checks its envelope and its `A2A-Version`, decodes its params, dispatches
//! it to agentd's handler ([`ports::RuntimePorts`]) and writes the answer,
//! errors included. What runs here is the host's: who the caller is, how
//! fast it may ask, what it may ask for — and agentd's own extension
//! methods (the feed, the operator methods), which the SDK does not know.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::cors::{self, allow_origin};
use super::feed::feed_stream;
#[cfg(feature = "oauth")]
use super::identity::insufficient_scope;
use super::identity::{
    Peer, PeerId, challenge, challenge_at, evidence_of, forbidden, is_session_token, json_with,
    too_many,
};
use super::{App, LivenessCheck};
use crate::a2a::Principal;
use crate::a2a::errors;
use crate::a2a::ports;
use crate::a2a::principals::{Resolution, Via};
use crate::a2a::sdk_gaps;
use crate::obs::trace::TraceContext;
use crate::runtime::surface::{
    self, Active, Ext, Route, SpecMethod, accepts_version, negotiate, owner_of,
    parse_extension_header, route_of,
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
/// Only an answer the handler produced is echoed — a JSON-RPC error the SDK
/// answered with included, which is an answer the handler gave — and only
/// one sent `200`: the spec's echo lists what was "successfully activated
/// for that request", and a gate's refusal activated nothing. An empty set
/// is no header rather than an empty one.
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

/// One POST, through the listener's gates and then the SDK.
///
/// The gates run in a fixed order, and each refusal is final and plain JSON
/// (no SSE response is ever written for a refusal):
///
/// 1. the browser origin; the source's failure limit (for a bearer);
/// 2. the content type — before a byte of the body means anything;
/// 3. who is calling, from the headers and the connection alone;
/// 4. the caller's rate;
/// 5. the extensions: what the `A2A-Extensions` header activates for the
///    method, and the refusal of an extension method whose extension it does
///    not;
/// 6. the method's authorization, the extended card's credential, and — of a
///    send or an operator method — the reach of the caller (an instance
///    child's channel, an inbound token's scopes, the op's floor and grants)
///    to the op its command DataPart or its method names;
/// 7. the answer: the SDK's router over agentd's handler — or, for agentd's
///    own extension methods, the feed or the operator method's op —
///    echoing the extensions it was given under.
///
/// The order is the point. Nothing that costs the runtime anything runs before
/// the caller is known, and a method that is no route is the SDK's `-32601`
/// for an operator and a stranger alike, before any authorization could make
/// the two answers differ. The gates read the request's `id`, `method` and a
/// send's command DataPart ([`Peek`]) and nothing else of it: a malformed
/// envelope, a wrong `A2A-Version` or bad params are the SDK's to refuse.
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
    #[cfg_attr(not(feature = "oauth"), allow(unused_mut))]
    let mut ev = evidence_of(&headers, &peer_id, &peer);
    let rm = app.resource_metadata.as_ref().map(|r| r.url.as_str());
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

    // An inbound access token (RFC 0044 §5.9): a bearer that is no session
    // token and reads as a JWT is checked by the verifier — reading the
    // issuer's keys again, off this thread, when they are not loaded or lack
    // the token's — and the resolver reads what it found after every static
    // secret. Nothing of the token is kept but what it names.
    #[cfg(feature = "oauth")]
    let verified = match (&app.auth.inbound, ev.bearer.as_deref()) {
        (Some(inbound), Some(token))
            if posture.tokens && !is_session_token(&ev) && token.matches('.').count() == 2 =>
        {
            let checked = match inbound.verify(token, crate::a2a::inbound::For::Listener) {
                Err(crate::a2a::inbound::Refused::Keys { .. }) => {
                    let (i, t) = (Arc::clone(inbound), token.to_string());
                    tokio::task::spawn_blocking(move || {
                        i.verify_fetching(&t, crate::a2a::inbound::For::Listener)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        Err(crate::a2a::inbound::Refused::NotOurs(format!(
                            "the check did not finish: {e}"
                        )))
                    })
                }
                r => r,
            };
            match checked {
                Ok(v) => {
                    ev.token = Some(Ok(crate::a2a::principals::TokenId {
                        issuer: v.issuer.clone(),
                        subject: v.subject.clone(),
                        client: v.client.clone(),
                        user: v.user,
                        principal: v.principal_id(),
                        typed: v.typed(),
                        sid: v.sid(),
                    }));
                    Some(v)
                }
                // Not a token of any issuer here: the bearer is just a bearer.
                Err(crate::a2a::inbound::Refused::NotOurs(_)) => None,
                Err(e) => {
                    denied_token(&app, source, e.issuer(), e.why());
                    ev.token = Some(Err(e.why().to_string()));
                    None
                }
            }
        }
        _ => None,
    };

    let (principal, via) = match resolver.resolve(&ev, peer.is_unix(), sessions) {
        Resolution::Named(p, via) => (p, via),
        // Nothing presented: a challenge, never counted. An uncredentialed
        // browser lands here, and so does every sign-in's first request.
        Resolution::Unauthenticated { presented: false } => {
            // Refused only for being a browser: the same request without
            // `Origin` would have been the implicit operator.
            let browser = ev.origin && ev.local && posture.implicit_operator;
            denied(&app, source, None, None, None, None, "unauthenticated", 401);
            return challenge_at(false, false, browser, rm);
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
                challenge_at(true, is_session_token(&ev), false, rm)
            };
        }
    };
    // The caller an inbound token named is kept as an inbound session for as
    // long as the token is good: the handle the work it starts acts through.
    #[cfg(feature = "oauth")]
    if via == Via::Token
        && let (Some(v), Some(sessions), Some(inbound)) =
            (&verified, &app.auth.sessions, &app.auth.inbound)
    {
        sessions.inbound_upsert(crate::a2a::oauth::InboundSession {
            sid: v.sid(),
            principal: principal.id.clone(),
            typed: v.typed(),
            role: principal.role,
            issuer: v.issuer.clone(),
            client: v.client.clone(),
            scopes: v.scopes.clone(),
            act: v.act.clone(),
            labels: principal.labels.clone(),
            expires_ms: v.exp_ms,
            token: inbound
                .keeps(&v.issuer)
                .then(|| ev.bearer.clone())
                .flatten(),
        });
    }
    // The response carries this when the credential was a bearer, so a
    // refused caller learns its token lacks the scope rather than that it is
    // bad.
    let bearer_used = matches!(via, Via::Bearer | Via::Session | Via::Token);
    // Whose refusals are free: a principal no credential named — an `any`
    // rule's — can be anyone, so its refusals are logged like a stranger's.
    let unvouched =
        source.filter(|_| !matches!(via, Via::Bearer | Via::Session | Via::Cert | Via::Token));

    // What the host reads of the request before the SDK does — its `id`,
    // its `method` and, of a send, the command it carries — and nothing else.
    // The SDK parses the request: a body that is not JSON-RPC, or a method
    // that is no route, is its to answer, for every caller alike.
    let peeked = Peek::of(&body);
    let id = peeked.id.clone();

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
            peeked.method.as_deref(),
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

    // The route table, matched exactly. A name that is no route is the SDK's
    // to refuse (`-32601`), before any gate below could make an operator's
    // answer differ from a stranger's.
    let route = peeked.method.as_deref().and_then(route_of);

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
    let active = match route.map(|r| negotiate(&requested, &app.bridge.declared(), r)) {
        Some(Ok(active)) => active,
        Some(Err(e)) => return json_response(errors::response(id, &e)),
        None => Active::NONE,
    };

    let send = matches!(
        route,
        Some(Route::Spec(
            SpecMethod::SendMessage | SpecMethod::SendStreamingMessage
        ))
    );
    // The operator op an operator method calls: the method IS the op, so
    // the gates below hold it to everything a command naming the op was
    // held to — the op's floor and the caller's grants, an inbound token's
    // scopes.
    let operator = match route {
        Some(Route::Extension { ext_method }) => surface::operator_op_of(ext_method),
        _ => None,
    };
    // The op a send's command DataPart names, or an operator method's —
    // what the command gates below authorize. A command's envelope itself
    // (its extension, its marking, its arguments) is the runtime's to hold
    // to the command extension; an operator method's params, to the op's
    // argument schema.
    let op = match operator {
        Some(spec) => Some(spec.name.to_string()),
        None => send.then(|| peeked.op()).flatten(),
    };
    // An op the table names but nothing serves as a command — held in
    // reserve, or an operator op sent as a DataPart — is unknown to every
    // caller alike, which the runtime says (`UNKNOWN_OP`): no grant could
    // reach it, so the op gate does not answer "not permitted" for it
    // first. An operator method's op is served, by its method, and the op
    // gate holds it to the op's floor and the caller's grants.
    let unserved = operator.is_none()
        && op.as_deref().is_some_and(|op| {
            surface::is_builtin_op(op) && surface::command_envelope(op).is_none()
        });

    if let Some(route) = route {
        let name = route.name();
        // The method gate. A refusal here is NOT counted against the source:
        // the caller already proved who it is, so it guessed nothing, and a
        // source over the failure limit has every bearer refused unchecked —
        // counting an authenticated principal's refusals would let any
        // narrowed caller lock out every bearer sharing its address
        // (loopback, a NAT, a proxy), the operator's included.
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

        // The spec makes the extended card an authenticated read (§13.3): on
        // a listener that declares a scheme, a caller the scheme did not name
        // — an `any` rule, the implicit operator — is asked for a credential
        // rather than handed it. The card itself is the handler's to serve.
        if route == Route::Spec(SpecMethod::GetExtendedAgentCard)
            && posture.declares_any()
            && !matches!(via, Via::Bearer | Via::Session | Via::Cert | Via::Token)
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
            return challenge_at(false, false, false, rm);
        }

        // An instance child's channel carries its reports and nothing else:
        // a request that is not one is refused, whatever it says — an op
        // that is no report included, whether a command serves it or not.
        let report = op
            .as_deref()
            .is_some_and(|o| surface::INSTANCE_OPS.iter().any(|m| m.name == o));
        if principal.instance_handle().is_some() && !report {
            denied(
                &app,
                unvouched,
                Some(&principal),
                None,
                Some(name),
                op.as_deref(),
                "not_permitted",
                403,
            );
            return forbidden(
                id,
                &format!(
                    "{} sends its reports (_instance.result, _instance.emit) and nothing else",
                    principal.id
                ),
                bearer_used,
            );
        }

        // What an inbound token's scopes let its holder use (RFC 0044 §5.9):
        // a conversation, a workflow, a command — each a skill a scope maps
        // to. A send no scope of the token reaches is a 403 naming the scopes
        // that would. A plain message naming a task is the runtime's to
        // judge: an answer to a gate of the task goes to work its scope
        // already admitted, and one that becomes a turn needs `conversation`
        // there.
        #[cfg(feature = "oauth")]
        if (operator.is_some() || (send && (op.is_some() || !peeked.names_a_task())))
            && let (Some(v), Some(inbound)) = (&verified, &app.auth.inbound)
        {
            let skill = skill_of(op.as_deref(), &peeked.message);
            if !inbound.covers(v, &skill) {
                let scopes = inbound.scopes_for(&v.issuer, &skill);
                denied(
                    &app,
                    None,
                    Some(&principal),
                    None,
                    Some(name),
                    op.as_deref(),
                    "insufficient_scope",
                    403,
                );
                return insufficient_scope(
                    id,
                    &format!(
                        "the token's scopes do not reach {skill}: {}",
                        if scopes.is_empty() {
                            "no scope of its issuer does".to_string()
                        } else {
                            format!("one of {} does", scopes.join(", "))
                        }
                    ),
                    &scopes,
                    rm,
                );
            }
        }

        // The command-op gate: the op's floor and the caller's grants,
        // checked before anything is created on the caller's behalf. Which
        // workflow a `workflow.run` may start is the runtime's to judge — it
        // alone holds the workflows and their start roles.
        if let Some(op) = op.as_deref()
            && !unserved
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
    }

    // The span the caller's W3C headers name: the work this request starts
    // continues it. Read here, once, for TCP and unix alike, and only ever
    // carried as correlation — every decision above was made without it.
    let trace = upstream_of(&headers);

    // Every check has passed: the answer runs, and its answer — only its
    // answer — echoes what the request activated.
    let resp = match route {
        Some(Route::Extension { ext_method }) => {
            extension_method(&app, ext_method, &headers, &body, principal, active, trace).await
        }
        _ => {
            let scope = ports::RequestScope::new(principal, via, active).in_trace(trace);
            let identity = Identity {
                token: via == Via::Token,
                resource_metadata: rm.map(str::to_string),
            };
            sdk(&app, headers, body, peeked.method, scope, identity).await
        }
    };
    echoed(resp, active)
}

/// What the host reads of a request before handing it to the SDK.
///
/// a2a-server-lf 0.5.3 has no hook that runs between parsing a request and
/// dispatching it: its `InterceptedHandler`/`CallInterceptor` is not wired
/// into the JSON-RPC router (`InterceptedHandler` does not implement
/// `RequestHandler`). So the gates that must refuse a request before any of
/// it is acted on — the method's authorization, the extended card's
/// credential, the extensions it activates and the op a command names —
/// run here, ahead of the router, and read only what they decide on. When
/// the SDK runs interceptors before dispatch, these move into one.
struct Peek {
    /// The request's `id`, for a refusal to echo: a string or an integer,
    /// else `null`.
    id: Value,
    /// The JSON-RPC `method`.
    method: Option<String>,
    /// A send's `params.message`, read only for its command DataPart (and
    /// whether it names a task); `null` otherwise.
    message: Value,
}

impl Peek {
    fn of(body: &[u8]) -> Peek {
        let v = serde_json::from_slice::<Value>(body).unwrap_or_default();
        let id = match v.get("id") {
            Some(id @ Value::String(_)) => id.clone(),
            Some(id @ Value::Number(n)) if n.is_i64() => id.clone(),
            _ => Value::Null,
        };
        Peek {
            id,
            method: v.get("method").and_then(Value::as_str).map(str::to_string),
            message: v.pointer("/params/message").cloned().unwrap_or_default(),
        }
    }

    /// The op the message's command DataPart names, if it carries one.
    fn op(&self) -> Option<String> {
        crate::runtime::a2a_server::command_op(&self.message)
    }

    /// Whether the message names a task to continue. A blank id names
    /// nothing — the handler reads it by the same rule.
    #[cfg(feature = "oauth")]
    fn names_a_task(&self) -> bool {
        self.message["taskId"]
            .as_str()
            .is_some_and(|t| !t.trim().is_empty())
    }
}

/// agentd's own extension methods, which the SDK correctly does not know:
/// the feed, and the operator methods. Negotiation has already refused each
/// wherever its extension is not declared or not activated, and the gates
/// have held an operator method to its op's floor and the caller's grants.
///
/// The request is read with the SDK's own `JsonRpcRequest`, and the
/// `A2A-Version` it states held to the rule the SDK's router holds every
/// other method to: the major version this listener speaks, a missing
/// header read as 0.3.
async fn extension_method(
    app: &Arc<App>,
    ext_method: &'static str,
    headers: &HeaderMap,
    body: &[u8],
    principal: Principal,
    active: Active,
    trace: Option<TraceContext>,
) -> Response {
    let req = match serde_json::from_slice::<a2a_types::JsonRpcRequest>(body) {
        Ok(req) if req.jsonrpc == "2.0" => req,
        Ok(req) => {
            return refusal(
                req.id,
                &a2a_types::A2AError::invalid_request("invalid jsonrpc version"),
            );
        }
        Err(e) => {
            return refusal(
                a2a_types::JsonRpcId::Null,
                &a2a_types::A2AError::invalid_request(format!("invalid request: {e}")),
            );
        }
    };
    let version = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .map_or("0.3", str::trim);
    if !accepts_version(version) {
        return refusal(req.id, &a2a_types::A2AError::version_not_supported(version));
    }
    if owner_of(ext_method) == Some(Ext::Operator) {
        let id = serde_json::to_value(&req.id).unwrap_or_default();
        let params = req.params.unwrap_or(Value::Null);
        return operator_method(app, ext_method, id, params, principal, active, trace).await;
    }
    let feed = app
        .bridge
        .feed()
        .filter(|_| owner_of(ext_method) == Some(Ext::Events));
    let Some(feed) = feed else {
        return refusal(req.id, &a2a_types::A2AError::method_not_found(ext_method));
    };
    let id = serde_json::to_value(&req.id).unwrap_or_default();
    let alive = app.liveness.as_ref().and_then(|l| l(&principal));
    feed_stream(
        feed,
        id,
        req.params.unwrap_or_else(|| json!({})),
        principal,
        app.stream_deadline,
        alive,
        active,
    )
}

/// An operator method, answered by the runtime on its loop: the op's
/// params held to its schema, the caller to the op's reach, then the op's
/// handler. A read's result is its document; work's is the Task it created,
/// as `GetTask` would answer — never waited on: the caller follows it with
/// `GetTask` or `SubscribeToTask`. A refusal is the runtime's error object,
/// a `-31401`/`-31403` at its HTTP status, as every other refusal is.
async fn operator_method(
    app: &Arc<App>,
    method: &'static str,
    id: Value,
    params: Value,
    principal: Principal,
    active: Active,
    trace: Option<TraceContext>,
) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let out =
        tokio::task::spawn_blocking(move || bridge.call(method, params, principal, active, trace))
            .await
            .unwrap_or_else(|e| {
                json!({"_error": {"code": errors::INTERNAL_ERROR,
                                  "message": format!("the runtime call did not complete: {e}")}})
            });
    if let Some(e) = out.get("_error") {
        let status = e["code"].as_i64().map_or(200, errors::http_status_of);
        return json_with(
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            &errors::response(id, e),
        );
    }
    json_response(json!({"jsonrpc": "2.0", "id": id, "result": out}))
}

/// The SDK's answer to a request every gate has passed: its router parses
/// it, dispatches it to agentd's handler with the caller in scope, and
/// frames the answer, error or not. The host adds only what is its own —
/// the end of an answer whose caller's session died, and the `traceparent`
/// of the span the work was done in — and [`sdk_gaps`] bridges what the
/// released SDK gets wrong.
/// Who the caller proved to be, for the HTTP status an identity refusal of
/// the runtime's answers with.
struct Identity {
    /// Authenticated by an access token: a `-31403` is its scopes' refusal.
    token: bool,
    resource_metadata: Option<String>,
}

async fn sdk(
    app: &Arc<App>,
    headers: HeaderMap,
    body: Bytes,
    method: Option<String>,
    scope: ports::RequestScope,
    identity: Identity,
) -> Response {
    let body = sdk_gaps::request(&body).map_or(body, Bytes::from);
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/")
        .body(Body::from(body))
        .expect("build request");
    *request.headers_mut() = headers;
    let alive = app.liveness.as_ref().and_then(|l| l(&scope.caller));
    let span = Arc::clone(&scope.span);
    let protocol = app.protocol.clone();
    let answered = ports::with_request(scope, async move {
        match protocol.oneshot(request).await {
            Ok(resp) => resp,
            Err(never) => match never {},
        }
    });
    let resp = match &alive {
        Some(check) => match while_alive(answered, check).await {
            Some(resp) => resp,
            None => return revoked(),
        },
        None => answered.await,
    };
    let resp = match alive {
        _ if !is_stream(&resp) => completed(resp, method.as_deref(), &identity).await,
        Some(check) => until_revoked(streamed(resp), check),
        None => streamed(resp),
    };
    let span = span.lock().ok().and_then(|s| s.clone());
    traced(resp, span)
}

/// `resp`, a unary answer to `method`, with [`sdk_gaps::answer`] applied,
/// and agentd's two identity codes at their HTTP status: they are an
/// authentication answer, not a protocol one, so a `-31401` is a 401 and a
/// `-31403` a 403 — with RFC 6750's `insufficient_scope` challenge when the
/// caller presented an access token (§3.1). The body stays the SDK's.
async fn completed(resp: Response, method: Option<&str>, identity: &Identity) -> Response {
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    if let Some(code) = parsed
        .as_ref()
        .and_then(|e| e.pointer("/error/code"))
        .and_then(Value::as_i64)
    {
        let status = errors::http_status_of(code);
        if status != 200 {
            parts.status = StatusCode::from_u16(status).unwrap_or(parts.status);
        }
        if code == errors::PERMISSION_DENIED && identity.token {
            let mut www = String::from("Bearer realm=\"agentd\", error=\"insufficient_scope\"");
            if let Some(url) = &identity.resource_metadata {
                www.push_str(&format!(", resource_metadata=\"{url}\""));
            }
            if let Ok(v) = HeaderValue::from_str(&www) {
                parts.headers.insert(header::WWW_AUTHENTICATE, v);
            }
        }
    }
    if let Some(mut envelope) = parsed
        && sdk_gaps::answer(method, &mut envelope)
    {
        return json_with(parts.status, &envelope);
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// `resp`, an SSE answer, with [`sdk_gaps::Frames`] applied.
fn streamed(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    Response::from_parts(
        parts,
        Body::from_stream(sdk_gaps::Frames::new(body.into_data_stream())),
    )
}

/// Whether `resp` is an SSE stream.
fn is_stream(resp: &Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/event-stream"))
}

/// The JSON-RPC response refusing request `id` with `e`, as the SDK writes it.
fn refusal(id: a2a_types::JsonRpcId, e: &a2a_types::A2AError) -> Response {
    json_response(
        serde_json::to_value(a2a_types::JsonRpcResponse::error(id, e.to_jsonrpc_error()))
            .unwrap_or_default(),
    )
}

/// The skill a send asks for, as `a2a.inbound_tokens[].scopes_to_skills`
/// names it: a conversation, the workflow `workflow.run` starts (named in
/// the command DataPart of `message`), or the command.
#[cfg(feature = "oauth")]
fn skill_of(op: Option<&str>, message: &Value) -> String {
    match op {
        None => "conversation".to_string(),
        Some("workflow.run") => format!(
            "workflow:{}",
            crate::runtime::a2a_server::command_data(message)
                .and_then(|c| c["workflow"].as_str().map(str::to_string))
                .unwrap_or_default()
        ),
        Some(op) => format!("command:{op}"),
    }
}

/// An inbound token the verifier refused: logged as every refusal is,
/// bounded per source, with the issuer and why — never the token.
#[cfg(feature = "oauth")]
fn denied_token(app: &App, source: Option<std::net::IpAddr>, issuer: Option<&str>, why: &str) {
    let mut line = json!({"reason": "invalid_token", "status": 401, "issuer": issuer, "why": why});
    if let Some(ip) = source {
        match app.denials.admit(ip, "invalid_token") {
            None => return,
            Some(0) => {}
            Some(n) => line["suppressed"] = json!(n),
        }
    }
    app.log.warn("a2a.denied", line);
}

/// The span a request's `traceparent` and `tracestate` headers name, if they
/// name one ([`crate::obs::trace::inbound`]).
fn upstream_of(headers: &HeaderMap) -> Option<TraceContext> {
    let all = |name: &'static str| {
        headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap_or_default())
    };
    crate::obs::trace::inbound(all("traceparent"), all("tracestate"))
}

/// `resp` with a `traceparent` response header naming the span agentd did
/// the request's work in — the task's, when the answer is a task — so a
/// caller can find its work in agentd's lines and traces.
fn traced(mut resp: Response, span: Option<String>) -> Response {
    if let Some(v) = span.and_then(|s| axum::http::HeaderValue::from_str(&s).ok()) {
        resp.headers_mut().insert("traceparent", v);
    }
    resp
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

/// How long a revoked session's in-flight request may outlive it.
const LIVENESS_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// `resp`, an SSE stream, ended within a tick of the caller's session dying.
/// Every byte the SDK writes goes out as it is.
fn until_revoked(resp: Response, check: LivenessCheck) -> Response {
    let (parts, body) = resp.into_parts();
    let frames = UntilRevoked {
        inner: Box::pin(body.into_data_stream()),
        check,
        tick: tokio::time::interval(LIVENESS_TICK),
        done: false,
    };
    Response::from_parts(parts, Body::from_stream(frames))
}

/// The SDK's stream, until the caller's session dies.
struct UntilRevoked {
    inner: std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    check: LivenessCheck,
    tick: tokio::time::Interval,
    done: bool,
}

impl futures_util::Stream for UntilRevoked {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        // The tick is polled first and on every wake, so a quiet stream —
        // the common case for a revoked watcher — still ends on time.
        while this.tick.poll_tick(cx).is_ready() {
            if !(this.check)() {
                this.done = true;
                return Poll::Ready(None);
            }
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            other => other,
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
