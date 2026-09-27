// SPDX-License-Identifier: AGPL-3.0-only
//! Binding the listener: the identity posture and TLS it is spawned with.

use super::{A2aBridge, PairingState, SharedFeed};
use crate::a2a::Resolver;
use crate::obs::log::Logger;
use crate::runtime::events::Event;
use serde_json::json;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// What [`spawn_a2a_listener`] hands the runtime: the interface feed (when `interface.enabled`), the pairing state (when
/// `interface.pairing.enabled`), and the live listener — which must be kept,
/// because dropping it stops serving.
pub(crate) struct A2aServing {
    pub feed: Option<Arc<SharedFeed>>,
    pub pairing: Option<Arc<PairingState>>,
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
    interface: &crate::config::v2::Interface,
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
    // Pairing-code login is armed with the interface. On a
    // NON-loopback listener it also counts as "client auth exists" — an
    // uncredentialed caller then gets through as anonymous (able to call
    // exactly `Pair` + the public card) instead of 401.
    let pairing = if interface.enabled && interface.pairing.enabled {
        let role = interface
            .pairing
            .role
            .unwrap_or(crate::config::v2::Role::Operator);
        let ttl = interface
            .pairing
            .ttl
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(12 * 3600));
        Some(Arc::new(
            PairingState::new(role, ttl).map_err(|e| format!("interface.pairing: {e}"))?,
        ))
    } else {
        None
    };
    let loopback_listener =
        unix_listener || crate::net::http::is_loopback_host(crate::config::serve_host_of(&bind));
    let require_auth = a2a.tls.client_ca.is_some()
        || server_bearer.is_some()
        || (pairing.is_some() && !loopback_listener);

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

    // The interface feed exists only while `interface.enabled`.
    let feed = interface
        .enabled
        .then(|| Arc::new(SharedFeed::new(interface.debug)));
    // Shared with the listener so a reload can revise the CORS allowlist.
    let origins: crate::a2a::serve::OriginList =
        Arc::new(std::sync::RwLock::new(interface.origins.clone()));
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
                pairing: pairing.clone(),
            },
            extra_origins: Arc::clone(&origins),
            tls,
            request_timeout: bridge.request_timeout,
            stream_deadline: bridge.stream_deadline,
        },
        Arc::clone(&bridge),
        feed.clone(),
        log.clone(),
    )?;

    log.info("a2a.listen", json!({"authority": listen, "bound": listener.bound, "tls": tls_scheme, "mtls": a2a.tls.client_ca.is_some(), "require_auth": require_auth, "interface": interface.enabled, "interface_debug": interface.enabled && interface.debug, "pairing": pairing.is_some()}));
    Ok(A2aServing {
        feed,
        pairing,
        listener,
        bridge,
        origins,
    })
}
