// SPDX-License-Identifier: AGPL-3.0-only
//! The launcher's contract with the display clients it starts.
//!
//! `agentd tui` and `agentd ui` are a thin launcher: they run the daemon
//! exactly as `agentd <args>` would, and start one display client beside it
//! with nothing but its endpoint. Everything that promise depends on — which
//! binary, which flags, which grant the client signs in with and for how long —
//! is written down here, once, so the launcher, the daemon's launch grant, the
//! docs guard and the TypeScript clients' guard all read one source instead of
//! each carrying a copy that could drift.
//!
//! Always compiled, and every item is `pub`, so a feature-matrix row that
//! builds no launcher still compiles the contract without a dead-code warning.

/// Where a missing or unstartable client sends the operator.
pub const LAUNCHER_DOCS: &str = "https://agentd.dev/docs/interface#launcher";

/// One display client the launcher can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchClient {
    /// The subcommand that starts it (`agentd <sub>`).
    pub sub: &'static str,
    /// The binary looked up on `PATH` when the override is unset.
    pub bin: &'static str,
    /// The environment variable that overrides the binary.
    pub bin_env: &'static str,
    /// The flags the client is given, in order, each followed by its value.
    /// Exactly these: the launcher passes no other flag, and never a
    /// credential.
    pub argv: &'static [&'static str],
}

/// The flag naming the daemon's endpoint.
pub const ENDPOINT_FLAG: &str = "--endpoint";
/// The flag naming the listening socket `agentd ui` hands its server.
pub const LISTEN_FD_FLAG: &str = "--listen-fd";
/// The descriptor number an inherited socket or pipe arrives on in the client.
pub const LAUNCH_FD: i32 = 3;

/// Every client the launcher starts, and exactly what it is given.
///
/// `ui` gets a TCP listener the launcher bound on loopback itself, so no other
/// local process can hold the UI's port and receive whatever the browser opens
/// there.
pub const LAUNCH_CONTRACT: &[LaunchClient] = &[
    LaunchClient {
        sub: "tui",
        bin: "agentd-tui",
        bin_env: "AGENTD_TUI_BIN",
        argv: &[ENDPOINT_FLAG],
    },
    LaunchClient {
        sub: "ui",
        bin: "agentd-ui",
        bin_env: "AGENTD_UI_BIN",
        argv: &[ENDPOINT_FLAG, LISTEN_FD_FLAG],
    },
];

/// The launch contract for `sub`, when it names a launcher subcommand.
pub fn launch_client(sub: &str) -> Option<&'static LaunchClient> {
    LAUNCH_CONTRACT.iter().find(|c| c.sub == sub)
}

/// The port `agentd ui` serves the web UI on when `--port` is not given.
pub const DEFAULT_UI_PORT: u16 = 4173;

/// The OAuth extension grant (RFC 6749 §4.5) a launched client redeems its
/// single-use launch code with.
pub const LAUNCH_GRANT_TYPE: &str = "https://agentd.dev/oauth/grant-type/launch";

/// How long a launch code stays redeemable: long enough for a client to start
/// and exchange it, short enough that a copy found later is worthless.
pub const LAUNCH_CODE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a web UI's launch session lasts. A terminal UI's has no expiry of
/// its own — its token lives only in that process's memory and ends with the
/// launcher.
pub const LAUNCH_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(8 * 3600);

/// Why the launcher cannot hand a client the daemon's endpoint, or `Ok` with
/// the endpoint it can.
///
/// The display clients present no certificate and are signed in only from a
/// loopback peer, so the listener must be one they can reach and be admitted
/// to: http(s) on a fixed port, without `client_ca`, at a loopback host. The
/// endpoint is `a2a.url` when set — what the card advertises, so an https
/// certificate matches the name the client dials — else the concrete bind (a
/// wildcard bind requires `a2a.url` anyway).
pub fn launch_endpoint(s: &crate::config::settings::Settings) -> Result<String, String> {
    let listen = s
        .a2a
        .listen
        .as_deref()
        .ok_or("the display client needs an A2A listener: a2a.listen is not set")?;
    let crate::config::ServeTarget::Http { bind, tls } =
        crate::config::ServeTarget::parse(listen).map_err(|e| format!("a2a.listen: {e}"))?
    else {
        return Err(
            "a2a.listen is a unix socket, which the display clients cannot dial; add a loopback http(s) listener".into(),
        );
    };
    let host = crate::config::serve_host_of(&bind);
    let port = bind.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
    if port.is_empty() || port == "0" {
        return Err(
            "a2a.listen has no fixed port (:0); the launcher needs a fixed port to name the endpoint".into(),
        );
    }
    if s.a2a.tls.client_ca.is_some() {
        return Err(
            "the listener requires client certificates (a2a.tls.client_ca) and the display clients present none".into(),
        );
    }
    let endpoint = match &s.a2a.url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => {
            let scheme = if tls { "https" } else { "http" };
            if host.contains(':') {
                format!("{scheme}://[{host}]:{port}")
            } else {
                format!("{scheme}://{host}:{port}")
            }
        }
    };
    let endpoint_host = endpoint
        .split_once("://")
        .map(|(_, rest)| crate::config::serve_host_of(rest))
        .unwrap_or("");
    if !is_loopback_name(endpoint_host) {
        return Err(format!(
            "the endpoint {endpoint} is not a loopback host; the launcher signs its client in only on loopback"
        ));
    }
    Ok(endpoint)
}

/// A loopback name: `localhost`, `::1`, or any address in 127.0.0.0/8.
fn is_loopback_name(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(a2a: serde_json::Value) -> crate::config::settings::Settings {
        crate::config::settings::Settings {
            a2a: serde_json::from_value(a2a).unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn the_endpoint_is_a_loopback_origin_or_a_named_refusal() {
        let ok = |a2a| launch_endpoint(&settings(a2a)).unwrap();
        let no = |a2a| launch_endpoint(&settings(a2a)).unwrap_err();
        assert_eq!(
            ok(serde_json::json!({"listen": "http://127.0.0.1:8420"})),
            "http://127.0.0.1:8420"
        );
        assert_eq!(
            ok(serde_json::json!({"listen": "http://[::1]:8420"})),
            "http://[::1]:8420"
        );
        // A wildcard bind is named by its a2a.url, which must be loopback.
        assert_eq!(
            ok(
                serde_json::json!({"listen": "https://0.0.0.0:9443", "url": "https://localhost:9443"})
            ),
            "https://localhost:9443"
        );
        assert!(
            no(serde_json::json!({"listen": "https://0.0.0.0:9443", "url": "https://agent.example:9443"}))
                .contains("loopback"),
        );
        assert!(no(serde_json::json!({"listen": "unix:/run/a.sock"})).contains("unix"));
        assert!(
            no(serde_json::json!({"listen": "https://127.0.0.1:9443", "tls": {"client_ca": "/ca.pem"}}))
                .contains("client_ca")
        );
        assert!(no(serde_json::json!({})).contains("a2a.listen"));
    }

    #[test]
    fn every_client_is_named_once_with_the_endpoint_first() {
        for (i, c) in LAUNCH_CONTRACT.iter().enumerate() {
            assert_eq!(launch_client(c.sub), Some(c));
            assert!(LAUNCH_CONTRACT[..i].iter().all(|o| o.sub != c.sub));
            assert_eq!(
                c.argv[0], ENDPOINT_FLAG,
                "{}: the endpoint comes first",
                c.sub
            );
        }
    }
}
