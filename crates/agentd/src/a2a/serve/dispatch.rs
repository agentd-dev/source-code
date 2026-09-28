// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC endpoint: agentd's own vocabulary answered here, everything
//! else handed to a2a-rs with the authenticated principal attached.

use std::net::SocketAddr;
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
use super::identity::{Peer, PeerId, resolve};
use crate::a2a::Principal;
use crate::a2a::ports;
use crate::runtime::a2a_server::A2aBridge;

pub(super) async fn rpc(
    State(app): State<Arc<App>>,
    axum::Extension(peer_id): axum::Extension<PeerId>,
    axum::Extension(Peer(peer)): axum::Extension<Peer>,
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
    peer: SocketAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // A browser page on an unexpected origin must not be able to drive this
    // endpoint through a victim's browser (DNS rebinding). Checked before the
    // body is even parsed, so an unauthorised origin reaches no dispatch logic.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(o) = &origin
        && !origin_allowed(o, &app.origins())
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

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

    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
        })
        .map(str::to_string);

    let Some(principal) = resolve(&app, &peer_id, peer, bearer.as_deref()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "",
        )
            .into_response();
    };

    // agentd's own vocabulary, which the protocol layer does not know.
    let bare = method.strip_prefix("a2a.").unwrap_or(&method).to_string();
    match bare.as_str() {
        // Discovery. The spec's JSON-RPC binding has no method for the *public*
        // card — it is fetched from `.well-known` — but every agentd client asks
        // for it here, so both ways work and both are unauthenticated.
        "GetAgentCard" | "agent/card" => {
            return unary(&app, id, "GetAgentCard", json!({}), principal).await;
        }
        // Served here rather than passed down for the same reason as the public
        // card: a round trip through the SDK's typed `AgentCard` drops any field
        // it has no place for. Unlike the public card this one is SCOPED — the
        // skills are the ones this caller may actually run, and it sets
        // `supportsAuthenticatedExtendedCard` — so the two are deliberately not
        // the same document.
        "GetExtendedAgentCard" | "agent/getAuthenticatedExtendedCard" => {
            if principal.is_anonymous() {
                return err(
                    id,
                    -32007,
                    "the extended card requires an authenticated caller",
                );
            }
            return unary(&app, id, "GetExtendedAgentCard", json!({}), principal).await;
        }
        "SubscribeToEvents" => {
            return match &app.bridge.feed() {
                Some(feed) => {
                    if !principal.may("SubscribeToEvents", None) {
                        return err(id, -32003, "not authorized");
                    }
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
    // Authorization for the spec's methods: natural language is open to any
    // non-anonymous role; a command DataPart is checked against the role's
    // command grants.
    let op = params
        .get("message")
        .and_then(crate::runtime::a2a_server::command_op);
    if !principal.may(&bare, op.as_deref()) {
        app.log.warn(
            "a2a.denied",
            json!({"principal": principal.id, "method": bare, "op": op}),
        );
        return err(id, -32003, "not authorized");
    }

    // A command DataPart is agentd's own vocabulary riding the spec's data
    // part, not an A2A concept: some commands are plain reads that answer
    // without creating a task at all. Forcing those through a port that must
    // return a `Task` would mean inventing one. So they go straight to the
    // runtime, and its answer — task or not — is returned as it stands.
    if matches!(bare.as_str(), "SendMessage" | "SendStreamingMessage")
        && crate::runtime::a2a_server::command_op(&params["message"]).is_some()
    {
        let streaming = bare == "SendStreamingMessage";
        return unary_maybe_streamed(&app, id, "SendMessage", params, principal, streaming).await;
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
    unary_maybe_streamed(app, id, method, params, principal, false).await
}

/// [`unary`], but able to answer as a one-frame SSE stream.
///
/// `SendStreamingMessage` promises a stream, and that promise does not depend on
/// what the message turned out to contain. A command DataPart is answered by the
/// runtime in one step, so there is exactly one frame to send — but a caller
/// that asked for a stream and received a JSON body would fail to parse it,
/// which is a worse answer than a short stream.
async fn unary_maybe_streamed(
    app: &Arc<App>,
    id: Value,
    method: &str,
    params: Value,
    principal: Principal,
    streamed: bool,
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
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
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
