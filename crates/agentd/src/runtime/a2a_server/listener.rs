// SPDX-License-Identifier: AGPL-3.0-only
//! Binding the listener: the TLS it is spawned with, the URL it is reached at,
//! and the sessions it issues. Its identity posture is not here — it lives in
//! the bridge's resolver, so a reload that changes the rules changes the
//! posture with them.

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
    /// The sessions this listener has issued. On every TCP listener, device
    /// grant or not — `auth.sessions` lists them and the resolver routes
    /// every `agentd_at_` bearer to them — and held here, outside the
    /// authority, for that reason. `None` on a unix socket, which issues none.
    pub sessions: Option<Arc<crate::a2a::oauth::Sessions>>,
    /// The authorization server, when `a2a.device_grant.enabled` or a
    /// launcher installed its slot.
    pub authority: Option<Arc<crate::a2a::oauth::Authority>>,
    /// The launcher's slot, when `agentd tui` or `agentd ui` started this
    /// daemon: kept so every rebuild of [`A2aServing::origins`] — a reload
    /// that edits `a2a.cors.origins` — admits the UI it launched.
    pub launch: Option<Arc<crate::a2a::oauth::LaunchSlot>>,
}

/// The URL a caller reaches this listener at, once it is bound.
///
/// `a2a.url` when set — behind a proxy or a load balancer that is the only
/// address a caller can use. Otherwise the bind itself, with the port the
/// kernel actually gave (`:0` asks for one): a concrete host as written, and
/// `unix://<path>` for a socket. A wildcard bind names no host a caller could
/// dial, which is why validation requires `a2a.url` for one; should one reach
/// here without it, the bound socket address is the least wrong answer.
pub(crate) fn advertised_url(a2a: &crate::config::settings::A2a, bound: &str) -> String {
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
///
/// `launch` is the slot a launcher installed in this process
/// ([`crate::runtime::RunOpts`]). It adds the launch grant to the token
/// endpoint and, for `agentd ui`, the launched UI's origin to the CORS list —
/// and nothing else: the posture, the cards and the manifest are what they
/// would be without it, because it is not a mechanism any caller but the
/// launcher's own client can use.
///
/// Before anything binds, every rule id is recorded in the identity registry
/// ([`crate::runtime::identities`]), and a `user`-role id that an approved
/// device already owns refuses the start: the two would be one principal, and
/// the rule would inherit the device's history.
pub(crate) fn spawn_a2a_listener(
    a2a: &crate::config::settings::A2a,
    events_tx: Sender<Event>,
    resolver: Resolver,
    durable: &crate::state::Durable,
    _write_timeout: Duration,
    log: Logger,
    launch: Option<Arc<crate::a2a::oauth::LaunchSlot>>,
) -> Result<A2aServing, String> {
    use std::path::Path;
    let listen = a2a.listen.as_deref().ok_or("a2a.listen is not set")?;
    if let Err(refused) = crate::runtime::identities::register_rules(durable, a2a) {
        if let Some(line) = refused.collision_line() {
            log.error("identity.collision", line);
        }
        return Err(refused.to_string());
    }
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
    // A unix socket has no browsers and issues no sessions: a slot there
    // (which the launcher refuses to install) would have nothing to serve.
    let launch = launch.filter(|_| !unix_listener);
    // Shared with the listener so a reload can revise the CORS allowlist —
    // which, like the reload, admits the UI a launcher started.
    let origins: crate::a2a::serve::OriginList = Arc::new(std::sync::RwLock::new(
        crate::a2a::oauth::admitted_origins(&a2a.cors.origins, launch.as_deref()),
    ));
    let bridge = A2aBridge::with_feed(events_tx, resolver, feed.clone());
    // Sessions on every TCP listener; the authority that issues into them
    // only with a grant — the device grant, or a launcher's slot. A unix
    // socket's peers are the kernel's to name.
    let sessions = (!unix_listener).then(|| {
        Arc::new(crate::a2a::oauth::Sessions::new(
            crate::a2a::oauth::system_clock(),
        ))
    });
    let authority = sessions
        .as_ref()
        .filter(|_| a2a.device_grant.enabled || launch.is_some())
        .map(|s| {
            Arc::new(crate::a2a::oauth::Authority::new(
                a2a.device_grant.enabled.then(|| {
                    crate::a2a::oauth::DeviceGrant::new(
                        &a2a.device_grant,
                        crate::a2a::oauth::system_clock(),
                        crate::a2a::oauth::os_mint(),
                    )
                }),
                launch.clone(),
                Arc::clone(s),
            ))
        });
    if let Some(slot) = &launch {
        slot.attach_log(log.clone());
    }

    let listener = crate::a2a::serve::spawn(
        if unix_listener {
            crate::a2a::serve::Bind::Unix(bind.clone())
        } else {
            crate::a2a::serve::Bind::Tcp(bind.clone())
        },
        crate::a2a::serve::Opts {
            auth: crate::a2a::serve::Auth {
                sessions: sessions.clone(),
                authority: authority.clone(),
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

    let bound = listener.bound.clone();
    let advertised = advertised_url(a2a, &bound);
    // The issuer is the advertised ORIGIN — known only now that a `:0` port
    // has been given one — and every endpoint URL is built from it, so the
    // metadata, the card and the verification URI can never disagree.
    if let Some(a) = &authority
        && let Some(origin) = crate::runtime::surface::auth::origin_of(&advertised)
    {
        a.set_issuer(&origin);
    }
    let serving = A2aServing {
        feed,
        advertised_url: advertised,
        bound,
        listener,
        bridge,
        origins,
        sessions,
        authority,
        launch,
    };
    log.info(
        "a2a.listen",
        json!({"authority": listen, "bound": serving.bound, "url": serving.advertised_url, "tls": tls_scheme, "mtls": posture.mtls, "required": posture.required, "implicit_operator": posture.implicit_operator, "device_grant": a2a.device_grant.enabled, "events": a2a.events.enabled, "introspection": a2a.introspection.enabled}),
    );
    Ok(serving)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Logger {
        Logger::new(
            crate::obs::log::LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: crate::obs::log::Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
            },
            crate::obs::log::Level::Error,
        )
    }

    /// Spawn a listener for `a2a` on a free loopback port (a listen URL
    /// names a fixed one), with `launch` installed or not.
    fn serve(
        a2a: &crate::config::settings::A2a,
        launch: Option<Arc<crate::a2a::oauth::LaunchSlot>>,
    ) -> A2aServing {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        let mut a2a = a2a.clone();
        a2a.listen = Some(format!("http://127.0.0.1:{port}"));
        let a2a = &a2a;
        let durable = crate::state::Durable::new(
            Arc::new(crate::store::memory::MemoryStore::new()),
            "agentd",
            "i",
            crate::state::Policy::default(),
            None,
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        spawn_a2a_listener(
            a2a,
            tx,
            Resolver::build(a2a, &|_| None).unwrap(),
            &durable,
            Duration::from_secs(1),
            quiet(),
            launch,
        )
        .unwrap()
    }

    /// A launcher's slot is not a mechanism any caller but its own client can
    /// use, so it changes no posture: what the resolver enforces, the public
    /// and the extended card byte for byte — built as the runtime builds
    /// them, from everything it reads — and the manifest's a2a section are
    /// the same with and without one, for every shape of listener a launcher
    /// can start. Only the grant's own routes and the launched UI's origin
    /// differ.
    #[test]
    fn a_launch_changes_no_posture() {
        let settings = |a2a: serde_json::Value| crate::config::settings::Settings {
            a2a: serde_json::from_value(a2a).unwrap(),
            ..Default::default()
        };
        for doc in [
            json!({"listen": "http://127.0.0.1:8420", "url": "http://127.0.0.1:8420"}),
            json!({"listen": "http://127.0.0.1:8420", "url": "http://127.0.0.1:8420",
                   "cors": {"origins": ["https://ui.example"]},
                   "device_grant": {"enabled": true}, "events": {"enabled": true}}),
        ] {
            let s = settings(doc);
            let plain = serve(&s.a2a, None);
            let slot = Arc::new(
                crate::a2a::oauth::LaunchSlot::new(Some("http://127.0.0.1:4555")).unwrap(),
            );
            let launched = serve(&s.a2a, Some(Arc::clone(&slot)));
            let posture = plain.bridge.resolver().posture();
            assert_eq!(launched.bridge.resolver().posture(), posture, "{s:?}");
            assert_eq!(
                posture,
                crate::runtime::surface::auth::listener_auth_of(&s.a2a)
            );
            assert_eq!(launched.advertised_url, plain.advertised_url);
            let workflows = std::collections::BTreeMap::new();
            let operator = crate::a2a::Principal {
                role: crate::config::settings::Role::Operator,
                id: "operator".into(),
                ..crate::a2a::Principal::anonymous()
            };
            for view in [
                super::super::card::CardView::Public,
                super::super::card::CardView::Extended(&operator),
            ] {
                let card = |serving| {
                    serde_json::to_string(&super::super::card::card_served(
                        &s, serving, &workflows, view,
                    ))
                    .unwrap()
                };
                assert_eq!(card(Some(&launched)), card(Some(&plain)), "{s:?}");
            }
            let manifest = crate::runtime::surface::manifest::a2a_section(&s);
            assert_eq!(
                manifest["cors_origins"],
                s.a2a.cors.origins.len(),
                "the launched origin is not configuration"
            );
            // The slot is what it adds: the launch grant, and the origin.
            assert!(plain.launch.is_none() && launched.launch.is_some());
            assert!(launched.authority.as_ref().unwrap().launch().is_some());
            let admitted = launched.origins.read().unwrap().clone();
            assert_eq!(
                admitted.last().map(String::as_str),
                Some("http://127.0.0.1:4555")
            );
            assert_eq!(*plain.origins.read().unwrap(), s.a2a.cors.origins);
        }
    }

    fn a2a(doc: serde_json::Value) -> crate::config::settings::A2a {
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
