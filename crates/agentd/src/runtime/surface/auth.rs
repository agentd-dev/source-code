// SPDX-License-Identifier: AGPL-3.0-only
//! Who the listener admits, derived from the bind and the configured
//! credentials — the listener's **posture**, computed in one place.
//!
//! Every input lives under `a2a`, so the posture is a function of that section
//! alone: [`listener_auth_of`]. The resolver stores the value it builds from
//! (so the rules and the posture are one value a reload swaps together), the
//! card derives its security fields from it, and the `--capabilities`
//! manifest reports it. Nothing keeps a copy taken at spawn: a reload that
//! adds a principal rule to a no-auth loopback daemon ends the implicit
//! operator on the next request, because there is no second posture left
//! believing otherwise.

use crate::config::v2::{A2a, Role, Settings};

/// What the listener requires of a caller, and what it grants without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListenerAuth {
    /// A bearer can authenticate: `a2a.bearer`, or any `bearer_ref` rule.
    pub bearer: bool,
    /// A client certificate is verified: `https` with `a2a.tls.client_ca`.
    pub mtls: bool,
    /// The device authorization grant issues session tokens.
    pub device: bool,
    /// The listener is a unix socket: the kernel names the peer's uid.
    pub unix: bool,
    /// The TCP bind host is loopback (`127.0.0.0/8`, `::1`, `localhost`).
    pub loopback_bind: bool,
    /// A non-browser caller that presents nothing is the operator.
    ///
    /// True for a unix socket, and for a loopback-bound TCP listener with no
    /// mechanism at all. It is a statement about the BIND, never about the
    /// peer: on any other bind a loopback peer is somebody a same-host proxy
    /// relayed, not the person who started the daemon. And it never applies
    /// to a request that carries `Origin` (the resolver's step 4): a browser
    /// tab is not the operator merely because it runs on the same machine.
    pub implicit_operator: bool,
    /// Some named rule matches every caller (`match: {any: true}`).
    pub any_rule: bool,
    /// A caller must present something to be anybody.
    pub required: bool,
}

impl ListenerAuth {
    /// Whether the listener declares any authentication scheme a caller could
    /// present. A unix socket declares none: its authenticator is the kernel.
    pub fn declares_any(&self) -> bool {
        !self.unix && (self.bearer || self.mtls || self.device)
    }
}

/// The listener's posture, from `a2a` alone.
pub fn listener_auth_of(a2a: &A2a) -> ListenerAuth {
    let listen = a2a.listen.as_deref().map(Listen::of);
    let unix = matches!(listen, Some(Listen::Unix));
    let (https, loopback_bind) = match &listen {
        Some(Listen::Tcp { https, host, .. }) => (*https, crate::net::http::is_loopback_host(host)),
        _ => (false, false),
    };
    let bearer = a2a.bearer.is_some()
        || a2a
            .principals
            .iter()
            .any(|p| p.matcher.bearer_ref.is_some());
    let mtls = https && a2a.tls.client_ca.is_some();
    let device = a2a.device_grant.enabled;
    let implicit_operator = unix
        || (loopback_bind
            && a2a.bearer.is_none()
            && a2a.principals.is_empty()
            && a2a.tls.client_ca.is_none()
            && !device);
    // The FIRST `any` rule is the one the resolver applies (step 3 stops at
    // it), so it alone decides: an anonymous one admits nobody — the
    // resolver counts it as unauthenticated — whatever `any` rules follow it.
    // Reading "some rule" here instead would have the card say no credential
    // is needed while every uncredentialed request got a 401.
    let any_rule = a2a
        .principals
        .iter()
        .find(|p| p.matcher.any)
        .is_some_and(|p| p.role != Role::Anonymous);
    ListenerAuth {
        bearer,
        mtls,
        device,
        unix,
        loopback_bind,
        implicit_operator,
        any_rule,
        required: !(implicit_operator || any_rule),
    }
}

/// `a2a.listen`, split into what the posture and the URLs need. Parsed
/// leniently on purpose: validation already refused what cannot bind, and a
/// `:0` port — which a test or a launcher asks for — is still a loopback bind.
enum Listen<'a> {
    Tcp {
        https: bool,
        host: &'a str,
        port: Option<u16>,
    },
    Unix,
}

impl<'a> Listen<'a> {
    fn of(listen: &'a str) -> Listen<'a> {
        if listen.starts_with("unix:") {
            return Listen::Unix;
        }
        let (https, rest) = match listen.strip_prefix("https://") {
            Some(rest) => (true, rest),
            None => (false, listen.strip_prefix("http://").unwrap_or(listen)),
        };
        let authority = rest.split('/').next().unwrap_or(rest);
        let host = crate::config::serve_host_of(authority);
        let port = authority
            .rsplit_once(':')
            .filter(|(h, _)| !h.is_empty() && !authority.ends_with(']'))
            .and_then(|(_, p)| p.parse().ok());
        Listen::Tcp { https, host, port }
    }
}

/// Whether `host` binds every interface rather than naming one.
fn wildcard(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "")
}

/// The URL this listener is reached at, as far as the settings alone can say:
/// `a2a.url`; else `scheme://host:port` from a concrete bind; else `None` — a
/// `:0` port is not known until the bind, and a unix socket has no URL a
/// remote caller could use.
pub fn configured_url(s: &Settings) -> Option<String> {
    if let Some(url) = &s.a2a.url {
        return Some(url.clone());
    }
    match Listen::of(s.a2a.listen.as_deref()?) {
        Listen::Tcp {
            https,
            host,
            port: Some(port),
        } if port != 0 && !wildcard(host) => Some(format!(
            "{}://{}:{port}/",
            if https { "https" } else { "http" },
            bracket(host)
        )),
        _ => None,
    }
}

/// An IPv6 literal wrapped for an authority; anything else unchanged.
pub(crate) fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The origin (`scheme://authority`) of an absolute `http(s)` URL, or `None`
/// for anything else — a unix URL has no origin a browser or an OAuth client
/// could name.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    Some(format!("{}://{authority}", scheme.to_ascii_lowercase()))
}

/// `origin` joined with an absolute `path`, never producing `//` between them
/// whichever side carries the slash.
pub fn join(origin: &str, path: &str) -> String {
    format!(
        "{}/{}",
        origin.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::Resolver;
    use crate::a2a::principals::{Evidence, Resolution, Via};
    use serde_json::{Value, json};

    fn a2a(doc: Value) -> A2a {
        serde_json::from_value(doc).unwrap()
    }

    fn env(k: &str) -> Option<String> {
        match k {
            "B" => Some("server-bearer".into()),
            "R" => Some("rule-bearer".into()),
            _ => None,
        }
    }

    /// The postures the table covers: a name, the settings, and whether a
    /// caller must present something — written out here rather than computed,
    /// so the formula has something to be wrong against.
    fn postures() -> Vec<(&'static str, A2a, bool)> {
        vec![
            (
                "loopback, nothing configured",
                a2a(json!({"listen": "http://127.0.0.1:8080"})),
                false,
            ),
            (
                "loopback, a2a.bearer",
                a2a(json!({"listen": "http://127.0.0.1:8080", "bearer": "{{secret:B}}"})),
                true,
            ),
            (
                "bearer_ref only",
                a2a(json!({"listen": "http://127.0.0.1:8080", "principals": [
                    {"id": "ci", "match": {"bearer_ref": "{{secret:R}}"}, "role": "user"}
                ]})),
                true,
            ),
            (
                "any rule",
                a2a(json!({"listen": "http://127.0.0.1:8080", "principals": [
                    {"id": "pub", "match": {"any": true}, "role": "user"}
                ]})),
                false,
            ),
            (
                "an anonymous any rule before a user one",
                a2a(json!({"listen": "http://127.0.0.1:8080", "principals": [
                    {"id": "nobody", "match": {"any": true}, "role": "anonymous"},
                    {"id": "pub", "match": {"any": true}, "role": "user"}
                ]})),
                true,
            ),
            (
                "a user any rule before an anonymous one",
                a2a(json!({"listen": "http://127.0.0.1:8080", "principals": [
                    {"id": "pub", "match": {"any": true}, "role": "user"},
                    {"id": "nobody", "match": {"any": true}, "role": "anonymous"}
                ]})),
                false,
            ),
            (
                "https + client_ca",
                a2a(
                    json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                    "tls": {"cert": "c", "key": "k", "client_ca": "ca"}}),
                ),
                true,
            ),
            (
                "loopback https + client_ca",
                a2a(json!({"listen": "https://127.0.0.1:8443",
                    "tls": {"cert": "c", "key": "k", "client_ca": "ca"}})),
                true,
            ),
            (
                "client_ca + bearer",
                a2a(
                    json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                    "tls": {"cert": "c", "key": "k", "client_ca": "ca"}, "bearer": "{{secret:B}}"}),
                ),
                true,
            ),
            (
                "wildcard device_grant + bearer",
                a2a(
                    json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                    "tls": {"cert": "c", "key": "k"}, "bearer": "{{secret:B}}",
                    "device_grant": {"enabled": true}}),
                ),
                true,
            ),
            (
                "loopback device_grant + bearer",
                a2a(
                    json!({"listen": "http://127.0.0.1:8080", "bearer": "{{secret:B}}",
                    "device_grant": {"enabled": true}}),
                ),
                true,
            ),
            // Not a posture validation lets through (a wildcard bind needs a
            // mechanism), and here for exactly that reason: with none at all,
            // only the bind separates "the operator's own machine" from
            // "anybody who can route to it".
            (
                "wildcard, nothing configured",
                a2a(
                    json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                    "tls": {"cert": "c", "key": "k"}}),
                ),
                true,
            ),
            // Validation requires an operator credential beside the grant;
            // the posture does not lean on that.
            (
                "loopback device_grant alone",
                a2a(json!({"listen": "http://127.0.0.1:8080", "device_grant": {"enabled": true}})),
                true,
            ),
            (
                "unix",
                a2a(json!({"listen": "unix:///run/agentd.sock"})),
                false,
            ),
        ]
    }

    fn is_unauthenticated(r: &Resolution) -> bool {
        matches!(r, Resolution::Unauthenticated { .. })
    }

    /// The posture is ONE formula, and the resolver obeys it.
    ///
    /// For every posture and every locality a caller can arrive from, the
    /// posture's `required` says exactly whether an empty request — nothing
    /// presented, no `Origin` — is refused by the resolver built from the same
    /// settings. And on a bind that is not loopback, arriving from loopback
    /// buys nothing: a same-host proxy relays requests from 127.0.0.1, and
    /// those are not the operator's. A request that carries `Origin` is a
    /// browser, which is never the implicit operator: it is refused in every
    /// posture unless an `any` rule names it, and then it is that rule's
    /// principal — never the operator. (Unix is left out of the browser row:
    /// no browser reaches a unix socket, and the peer's uid names it operator
    /// before any header is read.)
    #[test]
    fn listener_auth_is_the_single_posture_source() {
        for (name, a, required) in postures() {
            let posture = listener_auth_of(&a);
            assert_eq!(posture.required, required, "{name}: {posture:?}");
            let r = Resolver::build(&a, &env).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                r.posture(),
                posture,
                "{name}: the resolver stores the posture"
            );
            let localities: &[bool] = if posture.loopback_bind || posture.unix {
                &[true]
            } else {
                &[true, false]
            };
            let mut seen = Vec::new();
            for &local in localities {
                let ev = Evidence {
                    local,
                    ..Default::default()
                };
                let got = r.resolve(&ev, posture.unix, None);
                assert_eq!(
                    posture.required,
                    is_unauthenticated(&got),
                    "{name} (local={local}): required={} but empty evidence resolved to {got:?}",
                    posture.required
                );
                seen.push(got);

                if posture.unix {
                    continue;
                }
                let browser = Evidence {
                    local,
                    origin: true,
                    ..Default::default()
                };
                let got = r.resolve(&browser, false, None);
                if posture.any_rule {
                    match &got {
                        Resolution::Named(p, Via::AnyRule) => assert!(
                            !p.is_operator(),
                            "{name}: an any rule carried the operator role: {p:?}"
                        ),
                        other => panic!("{name}: a browser under an any rule got {other:?}"),
                    }
                } else {
                    assert!(
                        is_unauthenticated(&got),
                        "{name} (local={local}): a browser request resolved to {got:?}"
                    );
                }
            }
            if !posture.loopback_bind && !posture.unix {
                assert_eq!(
                    seen[0], seen[1],
                    "{name}: a loopback peer on a non-loopback bind resolved differently"
                );
            }
        }
    }

    #[test]
    fn declares_any_and_the_flags() {
        let p = |doc| listener_auth_of(&a2a(doc));
        let lo = p(json!({"listen": "http://127.0.0.1:0"}));
        assert!(lo.loopback_bind && lo.implicit_operator && !lo.required && !lo.declares_any());
        let unix = p(json!({"listen": "unix:/run/a.sock", "bearer": "{{secret:B}}"}));
        assert!(unix.unix && unix.implicit_operator && !unix.declares_any());
        let wild = p(json!({"listen": "http://[::1]:8080", "bearer": "{{secret:B}}"}));
        assert!(wild.loopback_bind && !wild.implicit_operator && wild.required);
        assert!(wild.bearer && wild.declares_any());
        let mtls = p(json!({"listen": "https://0.0.0.0:1", "tls": {"client_ca": "ca"}}));
        assert!(mtls.mtls && !mtls.loopback_bind && mtls.required);
        // A client CA on a plaintext listener verifies nothing.
        assert!(!p(json!({"listen": "http://127.0.0.1:1", "tls": {"client_ca": "ca"}})).mtls);
        let anon_any = p(json!({"listen": "http://127.0.0.1:1", "principals": [
            {"id": "x", "match": {"any": true}, "role": "anonymous"}
        ]}));
        assert!(!anon_any.any_rule && anon_any.required);
    }

    #[test]
    fn urls_join_and_origins() {
        let s = |doc: Value| Settings {
            a2a: a2a(doc),
            ..Default::default()
        };
        assert_eq!(
            configured_url(&s(json!({"listen": "http://127.0.0.1:8080"}))).as_deref(),
            Some("http://127.0.0.1:8080/")
        );
        assert_eq!(
            configured_url(&s(json!({"listen": "https://[::1]:8443"}))).as_deref(),
            Some("https://[::1]:8443/")
        );
        assert_eq!(
            configured_url(&s(
                json!({"listen": "https://0.0.0.0:8443", "url": "https://a.example"})
            ))
            .as_deref(),
            Some("https://a.example")
        );
        for unknown in [
            json!({"listen": "http://127.0.0.1:0"}),
            json!({"listen": "https://0.0.0.0:8443"}),
            json!({"listen": "unix:///run/a.sock"}),
            json!({}),
        ] {
            assert_eq!(configured_url(&s(unknown.clone())), None, "{unknown}");
        }
        assert_eq!(
            origin_of("HTTPS://a.example:8443/x?y#z").as_deref(),
            Some("https://a.example:8443")
        );
        assert_eq!(origin_of("unix:///run/a.sock"), None);
        assert_eq!(origin_of("https://"), None);
        for (o, p) in [
            ("https://a", "/oauth2/token"),
            ("https://a/", "/oauth2/token"),
            ("https://a/", "oauth2/token"),
        ] {
            assert_eq!(join(o, p), "https://a/oauth2/token");
        }
    }
}
