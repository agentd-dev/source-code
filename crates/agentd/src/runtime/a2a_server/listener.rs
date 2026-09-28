// SPDX-License-Identifier: AGPL-3.0-only
//! Binding the listener: the TLS it is spawned with, and the URL it is reached
//! at. Its identity posture is not here — it lives in the bridge's resolver, so
//! a reload that changes the rules changes the posture with them.

use super::{A2aBridge, SharedFeed};
use crate::a2a::Resolver;
use crate::obs::log::Logger;
use crate::runtime::events::Event;
use serde_json::json;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// What [`spawn_a2a_listener`] hands the runtime: the observation feed (when
/// `a2a.events.enabled`) and the live listener — which must be kept, because
/// dropping it stops serving.
///
/// It keeps no posture. Whether a caller is the implicit operator, whether a
/// bearer is required, whether an `any` rule opens the door — all of it is read
/// per request from the bridge's resolver, which a reload replaces. A copy here
/// would be the one a SIGHUP could not reach.
pub(crate) struct A2aServing {
    pub feed: Option<Arc<SharedFeed>>,
    pub listener: crate::a2a::serve::Listener,
    /// Kept so a reload can swap the principal rules into the live listener.
    pub bridge: Arc<A2aBridge>,
    /// The live CORS allowlist, revisable by a reload.
    pub origins: crate::a2a::serve::OriginList,
    /// The authority actually bound (`unix:<path>` for a socket).
    pub bound: String,
    /// The URL this listener is published at: [`advertised_url`].
    pub advertised_url: String,
}

/// The URL a caller reaches this listener at, once it is bound.
///
/// `a2a.url` when set — behind a proxy or a load balancer that is the only
/// address a caller can use. Otherwise the bind itself, with the port the
/// kernel actually gave (`:0` asks for one): a concrete host as written, and
/// `unix://<path>` for a socket. A wildcard bind names no host a caller could
/// dial, which is why validation requires `a2a.url` for one; should one reach
/// here without it, the bound socket address is the least wrong answer.
pub(crate) fn advertised_url(a2a: &crate::config::v2::A2a, bound: &str) -> String {
    if let Some(url) = &a2a.url {
        return url.clone();
    }
    if let Some(path) = bound.strip_prefix("unix:") {
        return format!("unix://{path}");
    }
    let listen = a2a.listen.as_deref().unwrap_or_default();
    let scheme = if listen.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let host = listen
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .map(crate::config::serve_host_of)
        .unwrap_or_default();
    let port = bound.parse::<std::net::SocketAddr>().map(|a| a.port()).ok();
    match port {
        Some(port) if !matches!(host, "" | "0.0.0.0" | "::") => {
            format!(
                "{scheme}://{}:{port}/",
                crate::runtime::surface::auth::bracket(host)
            )
        }
        _ => format!("{scheme}://{bound}/"),
    }
}

/// Bind and start the A2A listener.
///
/// Everything protocol-shaped below this line belongs to `a2a-rs`; what is
/// assembled here is the TLS identity that makes a client certificate readable
/// in the first place, and the bridge that carries the resolver — the rules and
/// the posture — every request is resolved under.
pub(crate) fn spawn_a2a_listener(
    a2a: &crate::config::v2::A2a,
    events_tx: Sender<Event>,
    resolver: Resolver,
    _write_timeout: Duration,
    log: Logger,
) -> Result<A2aServing, String> {
    use std::path::Path;
    let listen = a2a.listen.as_deref().ok_or("a2a.listen is not set")?;
    let target =
        crate::config::ServeTarget::parse(listen).map_err(|e| format!("a2a.listen: {e}"))?;
    let (bind, tls_scheme) = match &target {
        crate::config::ServeTarget::Http { bind, tls } => (bind.clone(), *tls),
        // A unix socket is bound by path; loopback-equivalent trust (stronger:
        // the kernel gates by uid where loopback TCP admits every local user).
        crate::config::ServeTarget::Unix { path } => (path.clone(), false),
    };
    let unix_listener = matches!(&target, crate::config::ServeTarget::Unix { .. });
    // For the startup line only: the listener reads the posture per request
    // from the resolver, never from here.
    let posture = resolver.posture();

    let tls = if tls_scheme {
        let cert = a2a
            .tls
            .cert
            .as_deref()
            .ok_or("a2a.tls.cert is required for https")?;
        let key = a2a
            .tls
            .key
            .as_deref()
            .ok_or("a2a.tls.key is required for https")?;
        let acceptor = crate::net::tls::TlsAcceptor::from_paths(
            Path::new(cert),
            Path::new(key),
            a2a.tls.client_ca.as_deref().map(Path::new),
        )
        .map_err(|e| format!("a2a tls: {e}"))?;
        // The ACCESSOR, not its current value: `server_config()` re-stats the
        // mounted identity on a throttle, so handing the listener a closure
        // means a rotated certificate is picked up by the next handshake.
        // Calling it once here — which is what this did — captured the boot
        // identity and made every cert-manager renewal a restart.
        let acceptor = std::sync::Arc::new(acceptor);
        Some(std::sync::Arc::new(move || acceptor.server_config())
            as crate::a2a::serve::TlsConfigProvider)
    } else {
        None
    };

    // The observation feed exists only while `a2a.events.enabled`; whether it
    // carries the introspection kinds (audit, logs) is the separate,
    // reloadable `a2a.introspection.enabled`.
    let feed = a2a
        .events
        .enabled
        .then(|| Arc::new(SharedFeed::new(a2a.introspection.enabled)));
    // Shared with the listener so a reload can revise the CORS allowlist.
    let origins: crate::a2a::serve::OriginList =
        Arc::new(std::sync::RwLock::new(a2a.cors.origins.clone()));
    let bridge = A2aBridge::with_feed(events_tx, resolver, feed.clone());

    let listener = crate::a2a::serve::spawn(
        if unix_listener {
            crate::a2a::serve::Bind::Unix(bind.clone())
        } else {
            crate::a2a::serve::Bind::Tcp(bind.clone())
        },
        crate::a2a::serve::Opts {
            // No session store is installed yet, so every session token is
            // refused as unknown.
            auth: crate::a2a::serve::Auth::default(),
            cors_origins: Arc::clone(&origins),
            tls,
            request_timeout: bridge.request_timeout,
            stream_deadline: bridge.stream_deadline,
        },
        Arc::clone(&bridge),
        feed.clone(),
        log.clone(),
    )?;

    let bound = listener.bound.clone();
    let serving = A2aServing {
        feed,
        advertised_url: advertised_url(a2a, &bound),
        bound,
        listener,
        bridge,
        origins,
    };
    log.info(
        "a2a.listen",
        json!({"authority": listen, "bound": serving.bound, "url": serving.advertised_url, "tls": tls_scheme, "mtls": posture.mtls, "required": posture.required, "implicit_operator": posture.implicit_operator, "events": a2a.events.enabled, "introspection": a2a.introspection.enabled}),
    );
    Ok(serving)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a2a(doc: serde_json::Value) -> crate::config::v2::A2a {
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn the_advertised_url_prefers_a2a_url_then_the_bound_port() {
        let url = |doc, bound: &str| advertised_url(&a2a(doc), bound);
        assert_eq!(
            url(
                json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example"}),
                "0.0.0.0:8443"
            ),
            "https://agent.example"
        );
        // `:0` is resolved to the port the kernel gave.
        assert_eq!(
            url(json!({"listen": "http://127.0.0.1:0"}), "127.0.0.1:41234"),
            "http://127.0.0.1:41234/"
        );
        assert_eq!(
            url(json!({"listen": "https://[::1]:0"}), "[::1]:5000"),
            "https://[::1]:5000/"
        );
        assert_eq!(
            url(json!({"listen": "http://localhost:8080"}), "127.0.0.1:8080"),
            "http://localhost:8080/"
        );
        assert_eq!(
            url(json!({"listen": "unix:///run/a.sock"}), "unix:/run/a.sock"),
            "unix:///run/a.sock"
        );
    }
}
