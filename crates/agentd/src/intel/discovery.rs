//! Optional model discovery (capability-negotiated). RFC 0018 §5.4.
//!
//! agentd may learn what an endpoint serves via a tiny handshake — **off unless
//! an endpoint looks discovery-capable, silent on failure, never fatal, never on
//! the hot path, never at startup before a side effect** (RFC 0018 decision 5).
//!
//! The probe is one hand-rolled HTTP `GET /v1/models` over the EXISTING intel
//! transport ([`super::endpoints::Endpoint::discover_models`]) — no new client,
//! no streaming, zero new deps. For an OpenAI-compatible endpoint it parses
//! `{ "data": [ { "id": "…" } ] }`; the `anthropic` dialect has no list endpoint
//! → it contributes nothing (the configured `model` is dialed regardless).
//!
//! The SURFACE that consumes this — `agentd://intelligence` + the capabilities
//! manifest `intelligence.models` — is supervisor-side (RFC 0018 §4.4 / RFC 0015
//! §capabilities). The architecture adaptation of the RFC's "lazy on first
//! `complete_once`" (a child-side hook) is to probe **supervisor-side, lazily +
//! cached, on read of the served surface**: those reads are infrequent and
//! operator-driven, so a cached probe there is the right seam, and it keeps the
//! additive discovery field off the control protocol. The cache + TTL live in
//! [`crate::mcp::server::ServeCtx`]; this module is the pure probe.

use std::time::Duration;

use super::endpoints::EndpointList;

/// The default per-probe timeout (RFC 0018 §5.4 — "a SHORT timeout, it's
/// best-effort"). Discovery is off the hot path; a slow/wedged endpoint must not
/// stall the operator-driven read that triggered the probe.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// The discovery outcome for the served surface (RFC 0018 §5.4):
/// - `discovery`: at least one endpoint answered `/v1/models`.
/// - `models`: the union of discovered ids across endpoints **+** the configured
///   `model`, de-duplicated, order-stable (`[]` if none discovered AND no model).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryResult {
    pub discovery: bool,
    pub models: Vec<String>,
}

/// Probe every OpenAI-compatible endpoint in `list` for its served models and
/// fold the result into a [`DiscoveryResult`] (RFC 0018 §5.4). `model` is the
/// configured model id, always unioned in (it is usable regardless of whether
/// any endpoint answered discovery). Best-effort + silent: a per-endpoint probe
/// that fails (404 / connection / non-JSON) simply contributes no models and does
/// not flip `discovery` — it is NEVER fatal, NEVER a failover-class error (§5.4).
///
/// Not called on the hot path or at startup — only when the served
/// `agentd://intelligence` / live `agentd://capabilities` surface is actually
/// read (the supervisor caches it; [`crate::mcp::server::ServeCtx`]).
pub fn discover(list: &EndpointList, model: Option<&str>, timeout: Duration) -> DiscoveryResult {
    let mut models: Vec<String> = Vec::new();
    let mut any = false;

    for ep in list.iter() {
        let discovered = ep.discover_models(timeout);
        if !discovered.is_empty() {
            any = true;
            for m in discovered {
                if !models.contains(&m) {
                    models.push(m);
                }
            }
        }
    }

    // §5.4: union of discovered + the configured model. The configured model is
    // always usable, so it joins the set — but its presence alone does NOT set
    // `discovery` (which means "an endpoint answered the probe").
    if let Some(m) = model
        && !m.is_empty()
        && !models.contains(&m.to_string())
    {
        models.push(m.to_string());
    }

    DiscoveryResult {
        discovery: any,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_of(uri: &str) -> EndpointList {
        EndpointList::parse_with_env(uri, None, &|_| None).unwrap()
    }

    // A tiny single-shot HTTP server answering a fixed status + body to one GET,
    // so the probe dials a REAL endpoint over the real transport.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf); // drain the request line + headers
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn dead_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn discovers_models_and_unions_configured() {
        let uri = serve_once(
            200,
            r#"{"data":[{"id":"claude-opus-4"},{"id":"claude-haiku-4"}]}"#,
        );
        let list = list_of(&uri);
        let r = discover(&list, Some("claude-opus-4"), Duration::from_secs(2));
        assert!(r.discovery, "an endpoint answered /v1/models");
        // union of discovered + configured, de-duplicated (opus appears once).
        assert_eq!(
            r.models,
            vec!["claude-opus-4".to_string(), "claude-haiku-4".to_string()]
        );
    }

    #[test]
    fn configured_model_is_added_when_not_already_discovered() {
        let uri = serve_once(200, r#"{"data":[{"id":"served-model"}]}"#);
        let list = list_of(&uri);
        let r = discover(&list, Some("configured-model"), Duration::from_secs(2));
        assert!(r.discovery);
        assert_eq!(
            r.models,
            vec!["served-model".to_string(), "configured-model".to_string()]
        );
    }

    #[test]
    fn http_404_degrades_silently_to_no_discovery() {
        // 404 → discovery unsupported for the endpoint: discovery=false, but the
        // configured model is still in `models` (it is usable regardless).
        let uri = serve_once(404, r#"{"error":"not found"}"#);
        let list = list_of(&uri);
        let r = discover(&list, Some("only-configured"), Duration::from_secs(2));
        assert!(!r.discovery, "a 404 is not an answer");
        assert_eq!(r.models, vec!["only-configured".to_string()]);
    }

    #[test]
    fn connection_failure_degrades_silently() {
        let list = list_of(&dead_endpoint());
        let r = discover(&list, Some("m"), Duration::from_secs(1));
        assert!(!r.discovery);
        assert_eq!(r.models, vec!["m".to_string()]);
    }

    #[test]
    fn non_json_body_degrades_silently() {
        let uri = serve_once(200, "<html>not json</html>");
        let list = list_of(&uri);
        let r = discover(&list, Some("m"), Duration::from_secs(2));
        assert!(!r.discovery, "a non-JSON 200 yields no models");
        assert_eq!(r.models, vec!["m".to_string()]);
    }

    #[test]
    fn no_configured_and_no_discovery_is_empty() {
        let list = list_of(&dead_endpoint());
        let r = discover(&list, None, Duration::from_secs(1));
        assert!(!r.discovery);
        assert!(r.models.is_empty(), "[] if none discovered + no model");
    }
}
