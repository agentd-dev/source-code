// SPDX-License-Identifier: AGPL-3.0-only
//! The agent card, read from the runtime: the well-known route and the
//! provider a2a-rs asks for the extended card.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::{App, cors};
use crate::a2a::Principal;
use crate::a2a::ports;
use crate::runtime::a2a_server::A2aBridge;

/// The agent card, read from the runtime so its skills reflect the workflows
/// that are actually loaded rather than a snapshot taken at boot.
pub(super) struct CardFromRuntime(pub(super) Arc<A2aBridge>);

#[async_trait::async_trait]
impl a2a_rs::services::AgentInfoProvider for CardFromRuntime {
    /// The public card. No wire method reads it — the spec publishes it at
    /// `.well-known` — but the port is a2a-rs's to call, so it answers the
    /// same document the route does.
    async fn get_agent_card(&self) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("PublicCard", Principal::anonymous()).await
    }

    /// The authenticated card, scoped to whoever is asking. The caller travels
    /// on the request's scope, because the port takes none.
    async fn get_authenticated_extended_card(
        &self,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetExtendedAgentCard", ports::caller()).await
    }
}

impl CardFromRuntime {
    async fn card(
        &self,
        verb: &'static str,
        who: Principal,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        let bridge = Arc::clone(&self.0);
        let v = tokio::task::spawn_blocking(move || bridge.call(verb, json!({}), who))
            .await
            .map_err(|e| a2a_rs::domain::A2AError::Internal(e.to_string()))?;
        // The runtime's refusal — no extended card for an anonymous caller,
        // say — is recorded whole, like every port's, so the listener answers
        // with the runtime's code and words rather than a2a-rs's rendering.
        if let Some(e) = ports::error_of(&v) {
            ports::record_error(e);
            return Err(ports::from_error_object(e));
        }
        serde_json::from_value(v).map_err(a2a_rs::domain::A2AError::JsonParse)
    }
}

/// How long the card route waits on the runtime. Discovery is the first thing
/// a client does and a busy reactor should not hold it for the two minutes a
/// turn may take: a client told 503 comes back, one left hanging gives up.
const CARD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client may reuse a card without asking again. Short, because
/// the card follows the configuration a reload changes; the ETag makes the
/// asking again cheap.
const CARD_CACHE_CONTROL: &str = "public, max-age=60";

/// The request headers a cross-origin card GET may carry. The card takes no
/// credential, so `authorization` is not among them; these are what a spec
/// client may attach to any request, plus the revalidation a cache sends.
const CARD_REQUEST_HEADERS: &[&str] = &["if-none-match", "a2a-version", "a2a-extensions"];

/// GET (and HEAD) on the well-known path: discovery is public, by design —
/// every caller is handed the same document, read from the runtime as nobody.
///
/// The body is exactly what the runtime built, which is already the SDK's
/// canonical form — re-serialising the typed card instead would put its
/// `securitySchemes` map through a `HashMap` and give one card many ETags.
pub(super) async fn card(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let reply = tokio::time::timeout(
        CARD_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            bridge.call("PublicCard", json!({}), Principal::anonymous())
        }),
    )
    .await;
    card_response(reply.ok().and_then(Result::ok), &headers)
}

/// The response to a card GET, given what the runtime answered (`None` when
/// it did not answer in time, or the call itself failed).
///
/// Only an `AgentCard` is ever a 200 — the well-known URI's registration says
/// the resource MUST be one. Anything else is the runtime's passing state
/// (shutting down, stalled) and is answered 503 with a time to come back and
/// nothing a cache may keep, rather than `200 null` or `200 {"_error": …}`.
fn card_response(reply: Option<Value>, headers: &HeaderMap) -> Response {
    let Some(body) = reply
        .filter(is_agent_card)
        .and_then(|v| serde_json::to_vec(&v).ok())
    else {
        return public(unavailable());
    };
    // Of the bytes served, not of the card's `version`: that is the build's,
    // and the card changes under it whenever a reload changes what the card
    // says. 128 bits is plenty to tell two cards of one agent apart.
    let etag = format!("\"{}\"", &crate::sha::sha256_hex(&body)[..32]);
    let fresh = if_none_match(headers, &etag);
    let mut resp = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CARD_CACHE_CONTROL),
    );
    public(resp)
}

/// Whether the runtime's reply is a card: not an error envelope, and an
/// `AgentCard` with the name and the interface a client needs to use it.
/// The SDK's type defaults every field, so parsing alone would take `{}` —
/// or the runtime's `{"_error": …}` — for a card.
fn is_agent_card(v: &Value) -> bool {
    ports::error_of(v).is_none()
        && serde_json::from_value::<a2a_rs::domain::AgentCard>(v.clone())
            .is_ok_and(|c| !c.name.is_empty() && !c.supported_interfaces.is_empty())
}

/// `If-None-Match` against the card's ETag, by the weak comparison RFC 9110
/// §13.1.2 prescribes for it: `*`, or any listed tag whose opaque part is
/// ours whether or not it carries `W/` (a proxy that recompresses the body
/// may weaken the tag it forwards).
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag)
}

/// 503: the card cannot be built right now.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "application/problem+json"),
            (header::RETRY_AFTER, "5"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        json!({
            "type": "about:blank",
            "title": "Service Unavailable",
            "status": 503,
            "detail": "the agent card cannot be read from the runtime right now; retry shortly",
        })
        .to_string(),
    )
        .into_response()
}

/// The card's CORS: any origin may read it. It is the same public document for
/// every caller and no credential is ever part of reading it, so there is
/// nothing an origin allowlist would protect — while a hosted UI, or any A2A
/// client in a page, needs to read it before it can do anything else.
/// `retry-after` is exposed so a page told 503 knows when to come back.
fn public(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("retry-after"),
    );
    resp
}

/// The browser preflight for the card route — which a plain GET never needs;
/// it is for a client that attaches a header of its own.
///
/// Every origin is answered, as the GET is. Private Network Access is the
/// exception: a public page reaching a loopback daemon is exactly what Chrome
/// gates, and the card is enough to fingerprint a local agent, so that grant
/// is kept for the origins the operator listed in `a2a.cors.origins`.
pub(super) async fn card_preflight(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    card_preflight_for(&app.origins(), &headers)
}

/// [`card_preflight`] against an allowlist in hand.
fn card_preflight_for(origins: &[String], headers: &HeaderMap) -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    // Exposure is read from the GET itself, so a preflight exposes nothing.
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    if let Ok(v) = HeaderValue::from_str(&CARD_REQUEST_HEADERS.join(", ")) {
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
    }
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    // The PNA grant is the one part that depends on who asks.
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    if cors::wants_private_network(headers)
        && cors::origin_of(headers).is_some_and(|o| cors::origin_allowed(o, origins))
    {
        h.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::time::Instant;
    use tower::ServiceExt;

    fn a_card(description: &str) -> Value {
        json!({
            "name": "card-test",
            "description": description,
            "version": "1.0.0",
            "supportedInterfaces": [{
                "url": "http://127.0.0.1:4000",
                "protocolBinding": "JSONRPC",
                "protocolVersion": "1.0"
            }],
            "capabilities": {"streaming": true},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": []
        })
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn header_of<'r>(resp: &'r Response, name: &str) -> Option<&'r str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    async fn body_of(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    /// Only a card is ever a 200, and it carries what a cache needs: an ETag
    /// of the bytes served (stable for one card, different for another),
    /// `Cache-Control`, and a 304 for a matching `If-None-Match` — by weak
    /// comparison, `*` or in a list. Whatever else the runtime answers is a
    /// 503 a cache may not keep.
    #[tokio::test]
    async fn card_response() {
        let card = a_card("one");
        let resp = super::card_response(Some(card.clone()), &HeaderMap::new());
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header_of(&resp, "content-type"), Some("application/json"));
        assert_eq!(
            header_of(&resp, "cache-control"),
            Some("public, max-age=60")
        );
        let etag = header_of(&resp, "etag").unwrap().to_string();
        let hex = etag.trim_matches('"');
        assert!(
            etag.starts_with('"')
                && etag.ends_with('"')
                && hex.len() == 32
                && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "{etag}"
        );
        let body = body_of(resp).await;
        assert_eq!(
            hex,
            &crate::sha::sha256_hex(&body)[..32],
            "the tag is the bytes'"
        );
        let served: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(served, card);
        serde_json::from_slice::<a2a_rs::domain::AgentCard>(&body).expect("an AgentCard");

        // One card, one tag; another card, another.
        let again = super::card_response(Some(card.clone()), &HeaderMap::new());
        assert_eq!(header_of(&again, "etag"), Some(etag.as_str()));
        let other = super::card_response(Some(a_card("two")), &HeaderMap::new());
        assert_ne!(header_of(&other, "etag"), Some(etag.as_str()));

        for inm in [
            etag.clone(),
            format!("W/{etag}"),
            "*".to_string(),
            format!("\"0000\", {etag}"),
        ] {
            let resp =
                super::card_response(Some(card.clone()), &headers(&[("if-none-match", &inm)]));
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "{inm}");
            assert_eq!(header_of(&resp, "etag"), Some(etag.as_str()), "{inm}");
            assert_eq!(
                header_of(&resp, "cache-control"),
                Some("public, max-age=60"),
                "{inm}"
            );
            assert!(body_of(resp).await.is_empty(), "{inm}");
        }
        for inm in ["\"0000\"", &etag[..etag.len() - 2], "W/\"x\", \"y\""] {
            let resp =
                super::card_response(Some(card.clone()), &headers(&[("if-none-match", inm)]));
            assert_eq!(resp.status(), StatusCode::OK, "{inm}");
        }

        // Not a card: the runtime's error (even beside card-shaped fields),
        // nothing at all, a default-shaped object the SDK's type would take,
        // and a card with no interface.
        let mut errored = a_card("one");
        errored["_error"] = json!({"code": -32603, "message": "x"});
        for reply in [
            Some(errored),
            None,
            Some(Value::Null),
            Some(json!({"_error": {"code": -32603, "message": "the runtime is shutting down"}})),
            Some(json!({})),
            Some(json!("card")),
            Some(json!({"name": "card-test"})),
        ] {
            let resp = super::card_response(reply.clone(), &headers(&[("if-none-match", "*")]));
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
            assert_eq!(header_of(&resp, "retry-after"), Some("5"), "{reply:?}");
            assert_eq!(
                header_of(&resp, "cache-control"),
                Some("no-store"),
                "{reply:?}"
            );
            assert_eq!(
                header_of(&resp, "content-type"),
                Some("application/problem+json"),
                "{reply:?}"
            );
            assert_eq!(header_of(&resp, "etag"), None, "{reply:?}");
            assert_eq!(header_of(&resp, "access-control-allow-origin"), Some("*"));
            let problem: Value = serde_json::from_slice(&body_of(resp).await).unwrap();
            assert_eq!(problem["status"], 503, "{problem}");
        }
    }

    /// The card is public to every origin and takes no credential: any
    /// `Origin` reads it under `*` with no credentials grant, and the
    /// preflight answers every origin too — but Private Network Access, the
    /// grant that lets a public page reach a local daemon, only for an origin
    /// the operator listed.
    #[tokio::test]
    async fn card_cors_is_public() {
        let evil = headers(&[("origin", "https://evil.example")]);
        for resp in [
            super::card_response(Some(a_card("one")), &evil),
            super::card_response(None, &evil),
        ] {
            assert_eq!(header_of(&resp, "access-control-allow-origin"), Some("*"));
            assert_eq!(header_of(&resp, "access-control-allow-credentials"), None);
            assert_eq!(
                header_of(&resp, "access-control-expose-headers"),
                Some("retry-after")
            );
        }

        let listed = vec!["https://ui.example".to_string()];
        let ask = |origin: &str, pna: bool| {
            let mut h = vec![("origin", origin), ("access-control-request-method", "GET")];
            if pna {
                h.push(("access-control-request-private-network", "true"));
            }
            card_preflight_for(&listed, &headers(&h))
        };
        let resp = ask("https://evil.example", true);
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(header_of(&resp, "access-control-allow-origin"), Some("*"));
        assert_eq!(
            header_of(&resp, "access-control-allow-methods"),
            Some("GET, HEAD, OPTIONS")
        );
        assert_eq!(header_of(&resp, "access-control-allow-credentials"), None);
        assert_eq!(
            header_of(&resp, "access-control-allow-private-network"),
            None,
            "an unlisted origin is not granted PNA"
        );
        assert_eq!(header_of(&resp, "vary"), Some("Origin"));
        let resp = ask("https://ui.example", true);
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header_of(&resp, "access-control-allow-private-network"),
            Some("true")
        );
        let resp = ask("https://ui.example", false);
        assert_eq!(
            header_of(&resp, "access-control-allow-private-network"),
            None,
            "not volunteered"
        );
    }

    /// A runtime that does not answer is a 503 within the card's own
    /// deadline, not after the two minutes a runtime call may otherwise take.
    #[tokio::test]
    async fn a_stalled_runtime_is_a_503_in_seconds() {
        let resolver = crate::a2a::Resolver::build(
            &serde_json::from_value(json!({"listen": "http://127.0.0.1:0"})).unwrap(),
            &|_| None,
        )
        .unwrap();
        // A reactor that takes the request and answers nothing until the
        // test is done with it (then drops it, so the blocked call returns
        // and the runtime can shut down).
        let (tx, rx) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let held = rx.recv();
            let _ = finished.recv();
            drop(held);
        });
        let log = crate::obs::log::Logger::new(
            crate::obs::log::LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: crate::obs::log::Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
            },
            crate::obs::log::Level::Error,
        );
        let opts = super::super::Opts {
            auth: super::super::Auth::default(),
            cors_origins: Arc::default(),
            tls: None,
            request_timeout: Duration::from_secs(120),
            stream_deadline: Duration::from_secs(5),
        };
        let app = App::new(
            A2aBridge::new(tx, resolver),
            opts,
            Arc::new(a2a_rs::adapter::InMemoryStreamingHandler::new()),
            log,
        );
        let router = super::super::router(Arc::new(app));
        let started = Instant::now();
        let resp = router
            .oneshot(
                Request::get("/.well-known/agent-card.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "answered after {:?}",
            started.elapsed()
        );
        let _ = done.send(());
    }
}
