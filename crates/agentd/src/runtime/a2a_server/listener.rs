// SPDX-License-Identifier: AGPL-3.0-only
//! Binding the listener: the identity posture and TLS it is spawned with.

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
pub(crate) struct A2aServing {
    pub feed: Option<Arc<SharedFeed>>,
    pub listener: crate::a2a::serve::Listener,
    /// Kept so a reload can swap the principal rules into the live listener.
    pub bridge: Arc<A2aBridge>,
    /// The live CORS allowlist, revisable by a reload.
    pub origins: crate::a2a::serve::OriginList,
}

/// Bind and start the A2A listener.
///
/// Everything protocol-shaped below this line belongs to `a2a-rs`; what is
/// assembled here is the identity posture — whether a credential is required at
/// all, which one counts, and what a bare loopback connection means — plus the
/// TLS identity that makes a client certificate readable in the first place.
pub(crate) fn spawn_a2a_listener(
    a2a: &crate::config::v2::A2a,
    events_tx: Sender<Event>,
    resolver: Resolver,
    env: &dyn Fn(&str) -> Option<String>,
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
    let server_bearer = match &a2a.bearer {
        Some(b) => {
            Some(crate::sec::secret::resolve(&b.0, env).map_err(|e| format!("a2a.bearer: {e}"))?)
        }
        None => None,
    };
    let require_auth = a2a.tls.client_ca.is_some() || server_bearer.is_some();

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
            auth: crate::a2a::serve::Auth {
                require_auth,
                server_bearer,
            },
            cors_origins: Arc::clone(&origins),
            tls,
            request_timeout: bridge.request_timeout,
            stream_deadline: bridge.stream_deadline,
        },
        Arc::clone(&bridge),
        feed.clone(),
        log.clone(),
    )?;

    log.info("a2a.listen", json!({"authority": listen, "bound": listener.bound, "tls": tls_scheme, "mtls": a2a.tls.client_ca.is_some(), "require_auth": require_auth, "events": a2a.events.enabled, "introspection": a2a.introspection.enabled}));
    Ok(A2aServing {
        feed,
        listener,
        bridge,
        origins,
    })
}
